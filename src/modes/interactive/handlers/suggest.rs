//! 输入框候选/补全（/ 命令、子命令、@ 文件、Tab 路径补全）
//!
//! `/history` 的候选（历史实时过滤 / `@clear` 控制词）在 [`super::history`]。

use super::commands::{slash_commands, subcommands_for};
use crate::{
    core::extensions::suggestion_candidates,
    modes::interactive::{
        app::{App, FileScanResult, SuggestionItem, SuggestionKind},
        run_in_event_loop,
    },
    utils::fuzzy::fuzzy_filter,
};
use ignore::WalkState;
use regex::Regex;
use std::{
    cmp::Ordering,
    collections::BinaryHeap,
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
    },
};

/// 单次 @ 扫描的遍历上限：`~` 下有 `Code -> /data/Code` 这类指向巨树的符号链接，
const SCAN_MAX_VISITED: usize = 50_000;

/// 候选面板保留条数
const SCAN_TOP_N: usize = 20;

/// 遍历线程数：固定小值，避免每次按键扫描都拉起 num_cpus 个线程
const SCAN_THREADS: usize = 4;

/// 路径 token 前可能出现的开括号及其配对闭括号。
/// `(~/Dev` / `` `src/ma `` 这类散文包装符不是分隔符，但补全前必须剥掉才能当合法路径用。
const PATH_WRAPPERS: [(char, char); 5] =
    [('(', ')'), ('[', ']'), ('{', '}'), ('<', '>'), ('`', '`')];

impl App {
    /// 更新补全弹窗内容。
    ///
    /// 类型与查询串均未变时保留现有列表与选中项（仅修正越界选中）；否则重置选中到首项。
    /// `items` 为空且 `allow_empty` 为 false 时关闭弹窗；`start` 记录触发位置。
    pub(super) fn show_suggestions(
        &mut self,
        kind: SuggestionKind,
        start: usize,
        query: String,
        items: Vec<SuggestionItem>,
        allow_empty: bool,
    ) {
        if items.is_empty() && !allow_empty {
            if self.suggestion.active
                && self.suggestion.kind == kind
                && self.suggestion.query == query
            {
                return; // 内容未变仅元数据变化，保留
            }

            self.suggestion.deselect();
            return;
        }

        // 内容未变且类型相同：保留选中项与列表
        if self.suggestion.active
            && self.suggestion.kind == kind
            && self.suggestion.query == query
            && self.suggestion.items.len() == items.len()
        {
            // 仍需要保证 selected 不越界
            self.suggestion.selected = self.suggestion.selected.min(items.len().saturating_sub(1));
            return;
        }

        self.suggestion.active = true;
        self.suggestion.kind = kind;
        self.suggestion.items = items;
        self.suggestion.selected = 0;
        self.suggestion.start = start;
        self.suggestion.query = query;
    }
}

/// 取路径最后一段（已归一化为 / 分隔）
fn basename(path: &str) -> &str {
    path.rfind('/').map(|i| &path[i + 1..]).unwrap_or(path)
}

/// 评分：文件名完全>开头>子串，完整路径子串最低，目录 +10
fn score_entry(file_path: &str, query: &str, is_dir: bool) -> i32 {
    let lower_file = basename(file_path).to_lowercase();
    let lower_query = query.to_lowercase();
    let mut score = 0;
    if lower_file == lower_query {
        score = 100;
    } else if lower_file.starts_with(&lower_query) {
        score = 80;
    } else if lower_file.contains(&lower_query) {
        score = 50;
    } else if file_path.to_lowercase().contains(&lower_query) {
        score = 30;
    }
    if is_dir && score > 0 {
        score += 10;
    }
    score
}

/// 候选排序键：分数高优先，其次浅层、短路径、字典序。
/// `Ord` 把「更优」排在前面，故 `BinaryHeap::into_sorted_vec()` 直接得到排名顺序；
/// 路径各不相同，排序是全序，故边遍历边保留 top-N 与「全量收集后再排」结果一致，且结果与遍历顺序（并行调度）无关。
#[derive(PartialEq, Eq)]
struct RankedCandidate {
    /// 匹配得分，越高越靠前。
    score: i32,
    /// 相对扫描根的目录深度，越浅越靠前。
    depth: usize,
    /// 候选文件或目录的路径。
    path: String,
    /// true = 该项是目录（同分时略有加成）。
    is_dir: bool,
}

impl Ord for RankedCandidate {
    /// 按「更优在前」排序：分数降序，再按目录深度浅、路径短、字典序。
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .score
            .cmp(&self.score)
            .then_with(|| self.depth.cmp(&other.depth))
            .then_with(|| self.path.len().cmp(&other.path.len()))
            .then_with(|| self.path.cmp(&other.path))
    }
}

impl PartialOrd for RankedCandidate {
    /// 委托给 [`Ord::cmp`]，与全序保持一致。
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// @ 文件候选：`ignore` 库递归扫描目录树
///
/// 同步阻塞实现：调用方必须放进 `spawn_blocking`；`cancel` 置位后尽快退出。
fn file_suggestions_walk(tail: &str, cwd: &str, cancel: &AtomicBool) -> Vec<SuggestionItem> {
    let normalized = tail.replace('\\', "/");

    // scoped 查询拆分：@ 后含 / 时，最后一段是文件名查询，前面是目录；目录不存在返回空
    let Some((base_dir, query, display_base)) = split_base_and_query(&normalized, cwd) else {
        return Vec::new();
    };

    if base_dir.is_empty() {
        return Vec::new();
    }

    let mut builder = ignore::WalkBuilder::new(&base_dir);
    builder
        .hidden(false)
        .follow_links(true)
        .parents(true)
        .add_custom_ignore_filename(".fdignore")
        .threads(SCAN_THREADS)
        .filter_entry(|e| e.file_name() != ".git");

    let visited = AtomicUsize::new(0);
    let top = Mutex::new(BinaryHeap::<RankedCandidate>::new());
    builder.build_parallel().run(|| {
        Box::new(|entry| {
            if cancel.load(AtomicOrdering::Relaxed) {
                return WalkState::Quit;
            }
            if visited.fetch_add(1, AtomicOrdering::Relaxed) >= SCAN_MAX_VISITED {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if entry.depth() == 0 {
                return WalkState::Continue; // 根自身不参与匹配
            }
            let Ok(rel) = entry.path().strip_prefix(&base_dir) else {
                return WalkState::Continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if rel.is_empty() {
                return WalkState::Continue;
            }

            let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
            let score = if query.is_empty() {
                1
            } else {
                score_entry(&rel, &query, is_dir)
            };
            if score <= 0 {
                return WalkState::Continue;
            }

            let mut top = top.lock().unwrap();
            top.push(RankedCandidate {
                score,
                depth: entry.depth(),
                path: rel,
                is_dir,
            });
            if top.len() > SCAN_TOP_N {
                top.pop(); // 堆顶是当前最差候选
            }
            WalkState::Continue
        })
    });

    // into_sorted_vec：按 Ord 升序 = 排名从优到劣
    top.into_inner()
        .unwrap()
        .into_sorted_vec()
        .into_iter()
        .map(|c| candidate_item(&c.path, c.is_dir, &display_base))
        .collect()
}

/// 拆分 @ scoped 查询：含 / 时最后一段为文件名查询、前面为目录，返回 (base_dir, query, display_base)
fn split_base_and_query(normalized: &str, cwd: &str) -> Option<(String, String, String)> {
    match normalized.rfind('/') {
        Some(slash) => {
            let display_base = normalized[..=slash].to_string();
            let query = normalized[slash + 1..].to_string();
            let base_dir = if display_base.starts_with("~/") {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
                format!(
                    "{}/{}",
                    home,
                    display_base.trim_start_matches("~/").trim_end_matches('/')
                )
            } else if display_base.starts_with('/') {
                // `@/x`：display_base 就是 "/"，根目录不能 trim 成空串
                let trimmed = display_base.trim_end_matches('/');
                if trimmed.is_empty() {
                    "/".to_string()
                } else {
                    trimmed.to_string()
                }
            } else {
                format!("{}/{}", cwd, display_base.trim_end_matches('/'))
            };

            if !Path::new(&base_dir).is_dir() {
                return None;
            }
            Some((base_dir, query, display_base))
        }
        None => Some((cwd.to_string(), normalized.to_string(), String::new())),
    }
}

/// 由相对路径构造候选条目（目录名/补全带尾随 `/`；含空白/CJK 标点的路径加引号，
/// 否则补全后 token 被截断）
fn candidate_item(rel: &str, is_dir: bool, display_base: &str) -> SuggestionItem {
    let display_path = if display_base.is_empty() {
        rel.to_string()
    } else if display_base == "/" {
        format!("/{rel}")
    } else {
        format!("{display_base}{rel}")
    };

    let entry_name = basename(rel);
    let completion = if is_dir {
        format!("{display_path}/")
    } else {
        display_path.clone()
    };
    let completion = if completion.chars().any(is_autocomplete_separator) {
        format!("\"{completion}\"")
    } else {
        completion
    };

    SuggestionItem {
        name: if is_dir {
            format!("{entry_name}/")
        } else {
            entry_name.to_string()
        },
        description: display_path,
        insert: completion,
    }
}

impl App {
    /// 根据输入框光标所在行的前缀刷新候选列表
    pub(super) fn refresh_suggestions(&mut self) {
        // 只有内容变化（输入/删除等）才重新计算候选列表。
        // 历史浏览（↑/↓ 载入历史，history_index 非 None）：关闭候选，避免 `/model`
        // 这类以 / 开头的历史载入后抢走 Up/Down 焦点。
        if self.editor.history_index.is_some() {
            self.suggestion.deselect();
            return;
        }

        // 纯光标移动/翻页（内容版本未变）：不触发新建议，也不主动关闭已激活的建议
        // （否则 Ctrl+J 导航、异步 @ 扫描结果会被误杀）。保持现状即可。
        if self.last_suggestion_version == self.editor.content_version {
            return;
        }
        self.last_suggestion_version = self.editor.content_version;

        let line = self
            .editor
            .lines
            .get(self.editor.cursor_line)
            .cloned()
            .unwrap_or_default();
        let col = self.editor.cursor_col.min(line.chars().count());
        let prefix: String = line.chars().take(col).collect();

        // /history：精确 `/history` 或 `/history <关键词>` 时实时过滤历史。
        // 必须排在命令候选之前：`/history` 不含空白，否则会被命令候选分支吃掉，
        // 留下一个「按 Enter 没反应」的死胡同。`/hist` 等未完成的写法仍走命令候选提示。
        if self.refresh_history_suggestions(&prefix) {
            self.cancel_file_scan(); // 面板已换成历史：作废进行中的 @ 扫描
            return;
        }

        // / 命令：行以 / 开头且其后无空白
        if self.refresh_command_suggestions(&prefix) {
            self.cancel_file_scan();
            return;
        }

        // 子命令：`/命令 ` 后（第一个词）→ 二级候选面板
        if self.refresh_subcommand_suggestions(&prefix) {
            self.cancel_file_scan();
            return;
        }

        // @ 文件：光标前最后一个 @ 之后无空白；后台 ignore 遍历，回传主循环应用
        if self.refresh_file_suggestions(&prefix) {
            return;
        }

        self.cancel_file_scan();
        self.suggestion.deselect();
    }

    /// 刷新 / 命令候选（内置命令 + 已启用扩展命令 + 技能）；命中返回 true。
    /// 允许输入行以空白开头（pi #10218）：忽略前导空白，但替换锚点要指回原位。
    fn refresh_command_suggestions(&mut self, prefix: &str) -> bool {
        let command_text = prefix.trim_start();
        let offset = prefix.chars().count() - command_text.chars().count();

        // strip_prefix 后 query 非空（'/' 开头至少剩一个字符），无需再校验
        if let Some(query) = command_text.strip_prefix('/')
            && !query.contains(char::is_whitespace)
        {
            let q = query.to_lowercase();

            // 查询字符顺序出现即可命中，按匹配质量（词边界/连续/位置）升序，位置靠前的排前面。
            // 候选 = 内置命令 + 已启用扩展命令（禁用扩展的命令不出现）。
            let mut candidates: Vec<(String, String)> = slash_commands();

            // 技能注册为 /skill:name 命令候选（默认开启）
            for skill in &self.skills {
                candidates.push((format!("skill:{}", skill.name), skill.description.clone()));
            }

            // 技能候选按裸名（去掉 `skill:` 前缀）参与模糊匹配与排序；但显式 `skill:` 查询仍按完整名匹配。
            let explicit_skill = q.starts_with("skill:");
            let items: Vec<SuggestionItem> = fuzzy_filter(&candidates, &q, move |c| {
                if !explicit_skill && c.0.starts_with("skill:") {
                    &c.0["skill:".len()..]
                } else {
                    c.0.as_str()
                }
            })
            .into_iter()
            .map(|(name, desc)| SuggestionItem {
                name: name.clone(),
                description: desc.clone(),
                insert: name.clone(),
            })
            .collect();

            self.show_suggestions(SuggestionKind::Commands, offset, q, items, false);
            return true;
        }
        false
    }

    /// 刷新子命令候选（`/命令 ` 后的第一个词）；命中返回 true。
    ///
    /// 规则（一级面板：Tab 只补全、Enter 补全并执行）：
    /// - `/<命令> ` —— 命令名后有空白才进入子命令模式，空查询列出全部子命令；
    /// - 查询词 = 命令名之后的**第一个词**，出现第二个空白（`/goal pause `、
    ///   `/goal 一个目标`）即关闭面板，避免干扰自由文本参数（如同长度目标描述）；
    /// - 命令无子命令（`/model x`）→ 不开面板。
    fn refresh_subcommand_suggestions(&mut self, prefix: &str) -> bool {
        let command_text = prefix.trim_start();
        let offset = prefix.chars().count() - command_text.chars().count();
        let Some(rest) = command_text.strip_prefix('/') else {
            return false;
        };
        // 命令名 + 至少一个空白；命令名本身不含空白
        let Some((name, after)) = rest.split_once(char::is_whitespace) else {
            return false;
        };
        if name.is_empty() {
            return false;
        }
        // 去掉命令名后的连续空白：`/goal  p` 也算子命令查询（锚点固定在命令名之后的第一个空白）
        let query = after.trim_start();
        if query.contains(char::is_whitespace) {
            return false; // 第二个词已开始：用户在写参数，不再提示
        }

        // 命令名与分发一致地大小写不敏感（dispatch 会对整行 to_lowercase）
        let candidates = subcommands_for(&name.to_lowercase());
        if candidates.is_empty() {
            return false;
        }

        // 锚点 = 命令名后的第一个空白字符：应用时整段（空白 + 查询词）被替换，
        // 多敲的空格会被归一化（`/goal  p` → Tab → `/goal pause `）。
        let start = offset + 1 + name.chars().count(); // 前导空白 + '/' + 命令名
        let q = query.to_lowercase();

        let items: Vec<SuggestionItem> = fuzzy_filter(&candidates, &q, |c| c.name)
            .into_iter()
            .map(|sub| SuggestionItem {
                name: sub.name.to_string(),
                description: sub.description.to_string(),
                insert: sub.name.to_string(),
            })
            .collect();

        // 无匹配：返回 false 让后续分支（@ 文件）接管，并在无其他命中时关闭面板。
        // 否则 `/goal @src` 这类会被空列表抢走，@ 文件候选不再弹出。
        if items.is_empty() {
            return false;
        }

        self.show_suggestions(SuggestionKind::Subcommands, start, q, items, false);
        true
    }

    /// 刷新 @ 文件候选：后台 ignore 遍历（spawn_blocking），回传主循环应用；已派发返回 true
    fn refresh_file_suggestions(&mut self, prefix: &str) -> bool {
        if let Some(at) = prefix.rfind('@') {
            // `@` 必须在 token 起始（行首/分隔符后，允许 `(` `[` `{` `<` 反引号包装）：避免 `user@example.com` 误触发
            if at > 0 && !is_at_token_start(prefix, at) {
                return false;
            }

            let at_char = prefix[..at].chars().count();
            let rest: String = prefix.chars().skip(at_char + 1).collect();

            if !rest.contains(is_autocomplete_separator) {
                let cwd = self.cwd.clone();
                let query = rest.clone();
                let start = at_char;

                // 本次刷新接管 @：作废上一次扫描（用户继续输入时旧 walk 尽快退出）
                self.cancel_file_scan();

                // 扩展名册候选（子代理 `@handle` / 命令实参）：同步、立即显示（不等扫描）
                let (mentions, exclusive) = mention_candidates(&query, prefix);
                if exclusive {
                    // 命令实参补全（如 `/agents stop @`、`/agents enable @`）：
                    // 只展示这些候选，不扫描/并入文件（id / 类型名与文件名无关）。
                    if mentions.is_empty() {
                        self.suggestion.deselect();
                    } else {
                        self.show_suggestions(
                            SuggestionKind::Files,
                            start,
                            query.clone(),
                            mentions,
                            false,
                        );
                    }
                    return true;
                }

                if !mentions.is_empty() {
                    self.show_suggestions(
                        SuggestionKind::Files,
                        start,
                        query.clone(),
                        mentions,
                        false,
                    );
                }

                // 目录遍历是同步阻塞 IO/CPU：必须走 blocking 线程池，否则会占住
                // runtime worker（多核下尚可，小机器上几个并发扫描就能饿死 UI 事件）
                let cancel = Arc::new(AtomicBool::new(false));
                self.file_scan_cancel = Some(Arc::clone(&cancel));
                tokio::task::spawn_blocking(move || {
                    let items = file_suggestions_walk(&query, &cwd, &cancel);
                    if cancel.load(AtomicOrdering::Relaxed) {
                        return; // 已被更新的扫描取代：不投递过期结果
                    }

                    _ = run_in_event_loop(move |ui| {
                        ui.apply_file_scan_result(FileScanResult {
                            kind: SuggestionKind::Files,
                            start,
                            query,
                            items,
                        })
                    });
                });

                return true;
            }
        }
        false
    }

    /// 作废进行中的 @ 扫描（不阻塞：walk 在下一个条目处退出并丢弃结果）
    fn cancel_file_scan(&mut self) {
        if let Some(cancel) = self.file_scan_cancel.take() {
            cancel.store(true, AtomicOrdering::Relaxed);
        }
    }
}

/// 扩展提供的 `@` 候选（如子代理的 `@handle`、`/agents stop @` 的代理 id）：
/// 映射到 TUI 候选条目。`line_prefix` 让扩展能按光标前的命令上下文区分候选；
/// 返回的 bool = 是否独占该 `@`（true 时 TUI 不再并入文件候选）。
fn mention_candidates(query: &str, line_prefix: &str) -> (Vec<SuggestionItem>, bool) {
    let cands = suggestion_candidates(query, line_prefix);
    let items = cands
        .items
        .into_iter()
        .map(|s| SuggestionItem {
            name: s.name,
            description: s.description,
            insert: s.insert,
        })
        .collect();
    (items, cands.exclusive)
}

impl App {
    /// 主循环收到异步 @ 扫描结果后应用；当前 @ 查询已变化则丢弃过期结果
    pub(crate) fn apply_file_scan_result(&mut self, result: FileScanResult) {
        let line = self
            .editor
            .lines
            .get(self.editor.cursor_line)
            .cloned()
            .unwrap_or_default();

        let col = self.editor.cursor_col.min(line.chars().count());
        let prefix: String = line.chars().take(col).collect();

        // 当前光标前必须还存在触发符 @，且其位置与派发扫描时一致；
        // 否则是过期结果（例如用户已清空该行、或把 @ 删掉了）：
        // 旧逻辑用 unwrap_or_default() 把「无 @」也判为 query 相同，
        // 于是会在空行上激活候选，随后 Tab/Enter 应用时索引越界 panic。
        let Some(at) = prefix.rfind('@') else {
            return;
        };
        let at_char = prefix[..at].chars().count();
        let rest: String = prefix.chars().skip(at_char + 1).collect();
        let current = if rest.contains(is_autocomplete_separator) {
            String::new()
        } else {
            rest
        };

        if at_char != result.start || current != result.query {
            return;
        }

        // 文件候选前面并上扩展名册候选（`@handle`）。
        let (mentions, exclusive) = mention_candidates(&result.query, &prefix);
        if exclusive {
            // 命令实参上下文：忽略（可能已过期的）文件扫描结果
            if mentions.is_empty() {
                self.suggestion.deselect();
            } else {
                self.show_suggestions(result.kind, result.start, result.query, mentions, false);
            }
            return;
        }

        let mut items = mentions;
        items.extend(result.items);
        self.show_suggestions(result.kind, result.start, result.query, items, false);
    }

    /// Ctrl+J：下一条（到底部回绕到顶部）
    pub(super) fn suggestion_next(&mut self) {
        let n = self.suggestion.items.len();
        if n > 0 {
            self.suggestion.selected = (self.suggestion.selected + 1) % n;
        }
    }

    /// Ctrl+K：上一条（到顶部回绕到底部）
    pub(super) fn suggestion_prev(&mut self) {
        let n = self.suggestion.items.len();
        if n > 0 {
            // selected 可能因列表缩短而越界，先归一化再回绕
            let cur = self.suggestion.selected.min(n - 1);
            self.suggestion.selected = (cur + n - 1) % n;
        }
    }

    /// Tab/Enter：应用当前选中项（替换触发符后的文本，保留触发符；
    /// / 命令名后加空格；@ 文件路径后加空格，目录不加（便于继续补全））
    pub(super) fn apply_suggestion(&mut self) {
        if !self.suggestion.active {
            return;
        }

        // 控制词候选（`@clear` / `@clear-all`）：Tab 只把控制词填进输入框
        // （`/history @clear`），不执行删除；执行仍由 Enter 负责（见 `handle_input_submit_key`）。
        // 这里必须整行替换：控制词候选的 `start` 恒为 0，而通用补全路径要求
        // `start` 处是触发符 `@`——`start=0` 处是 `/`，会直接放弃应用。
        if self.suggestion.kind == SuggestionKind::HistoryAction {
            if let Some(item) = self.suggestion.items.get(self.suggestion.selected) {
                let text = format!("/history {}", item.insert);
                self.editor.replace_all(&text);
            }
            self.suggestion.deselect();
            return;
        }

        let Some(item) = self.suggestion.items.get(self.suggestion.selected).cloned() else {
            // 无候选（如查询无匹配、列表被清空）：面板不能留在原地。
            // 否则 Enter 提交后候选会继续渲染在输入区（/agents 等后续覆盖层上方）
            self.suggestion.deselect();
            return;
        };

        // 历史候选是整段替换语义，单独一条路（见 super::history::apply_history_suggestion）
        if self.suggestion.kind == SuggestionKind::History {
            self.apply_history_suggestion(&item.insert);
            return;
        }

        let replacement = item.insert;
        let suffix = match self.suggestion.kind {
            // 命令与子命令都补一个尾随空格，便于继续输入（子命令补完后即退出面板）
            SuggestionKind::Commands | SuggestionKind::Subcommands => " ".to_string(),
            SuggestionKind::Files if !replacement.ends_with('/') => " ".to_string(),
            // History 已在上面提前返回；HistoryAction 不走到这里（调用方已提前 return）
            SuggestionKind::Files | SuggestionKind::History | SuggestionKind::HistoryAction => {
                String::new()
            }
        };
        let expected_trigger = match self.suggestion.kind {
            SuggestionKind::Commands => '/',
            // 子命令锚点是命令名后的第一个空白（含查询词整段替换）
            SuggestionKind::Subcommands => ' ',
            // History/HistoryAction 已在上面提前返回
            SuggestionKind::Files | SuggestionKind::History | SuggestionKind::HistoryAction => '@',
        };

        let line_chars: Vec<char> = self
            .editor
            .lines
            .get(self.editor.cursor_line)
            .map(|line| line.chars().collect())
            .unwrap_or_default();
        let col = self.editor.cursor_col.min(line_chars.len());
        let start = self.suggestion.start.min(col.saturating_sub(1));

        // 候选激活后编辑器内容可能已被改动（清空整行、删除触发符、异步结果回填等），
        // 此时 start 可能越界或指向别处：直接放弃应用，避免索引越界 panic。
        if col == 0
            || start >= line_chars.len()
            || start >= col
            || line_chars[start] != expected_trigger
        {
            self.suggestion.deselect();
            return;
        }

        let trigger = line_chars[start];
        let editor = &mut self.editor;
        let new_chars: Vec<char> = line_chars[..start]
            .iter()
            .copied()
            .chain(std::iter::once(trigger))
            .chain(replacement.chars())
            .chain(suffix.chars())
            .chain(line_chars[col..].iter().copied())
            .collect();

        editor.lines[editor.cursor_line] = new_chars.into_iter().collect();
        editor.cursor_col = start + 1 + replacement.chars().count() + suffix.chars().count();
        // 补全也是一次内容变化：递增版本让随后的 refresh 重算候选
        // （Tab 补全 `/goal ` 后立即弹出子命令面板依赖这一点）
        editor.content_changed();
        self.suggestion.deselect();
    }
}

/// CJK 标点：仅标点分隔词与补全（CJK 文字仍是词/路径的一部分）。CJK 脚本内的标点 + 显式清单。
fn is_cjk_punctuation(c: char) -> bool {
    /// CJK 标点判定用的正则缓存：匹配 Unicode 标点类别。
    static PUNCT: OnceLock<Regex> = OnceLock::new();
    /// CJK 文字脚本判定用的正则缓存：Han / 假名 / 谚文 / 注音等。
    static CJK_SCRIPT: OnceLock<Regex> = OnceLock::new();

    // CJK 脚本内的标点（regex crate 无 lookahead，故交集分两次判）
    let punct = PUNCT.get_or_init(|| Regex::new(r"\p{Punctuation}").expect("punct"));
    let script = CJK_SCRIPT.get_or_init(|| {
        regex::Regex::new(
            r"[\p{Script_Extensions=Han}\p{Script_Extensions=Hiragana}\p{Script_Extensions=Katakana}\p{Script_Extensions=Hangul}\p{Script_Extensions=Bopomofo}]",
        )
        .expect("cjk script")
    });

    let s = c.to_string();
    (script.is_match(&s) && punct.is_match(&s))
        || matches!(
            c,
            '，' | '．'
                | '：'
                | '；'
                | '！'
                | '？'
                | '（'
                | '）'
                | '［'
                | '］'
                | '｛'
                | '｝'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '…'
                | '—'
        )
}

/// 补全分隔符：空白或 CJK 标点。
fn is_autocomplete_separator(c: char) -> bool {
    c.is_whitespace() || is_cjk_punctuation(c)
}

/// 返回与开包装符 `c` 配对的闭括号；`c` 不是已知开括号时返回 `None`。
fn wrapper_closer(c: char) -> Option<char> {
    PATH_WRAPPERS
        .iter()
        .find(|(open, _)| *open == c)
        .map(|(_, close)| *close)
}

/// 剥掉 token 开头的连续开括号：`(~/Dev` → `~/Dev`。
/// token 自身含对应闭括号（`app/[slug]/pa`、`(group)/pa`）时保留，避免误伤真实路径。
fn strip_leading_wrappers(token: &str) -> &str {
    let mut rest = token;
    while let Some(first) = rest.chars().next() {
        let Some(closer) = wrapper_closer(first) else {
            break;
        };

        let tail = &rest[first.len_utf8()..];

        if tail.contains(closer) {
            break;
        }
        rest = tail;
    }
    rest
}

/// `index`（字节偏移）是否位于 token 起始：跳过紧邻的开括号后，前一字符是分隔符或行首。
/// 用于 `(@src` / `` `@src `` 这类被包装符包住的 `@`。
fn is_at_token_start(text: &str, index: usize) -> bool {
    let mut start = index;
    while start > 0 {
        let Some(prev) = text[..start].chars().next_back() else {
            break;
        };
        if wrapper_closer(prev).is_none() {
            break;
        }
        start -= prev.len_utf8();
    }

    start == 0
        || text[..start]
            .chars()
            .next_back()
            .is_some_and(is_autocomplete_separator)
}

/// Tab 路径补全（handlers.rs 中 tui.input.tab 直接调用）
/// 取编辑器文本最后一个词（用于 Tab 补全），返回 (前缀, 起始字节索引)。
/// 分隔符 = 空白或 CJK 标点；起始索引已跳过开括号包装符（`(~/Dev`）。
pub(super) fn last_word(text: &str) -> Option<(String, usize)> {
    let start = match text.rfind(is_autocomplete_separator) {
        Some(i) => i + text[i..].chars().next().map(char::len_utf8).unwrap_or(1),
        None => 0,
    };
    let word = strip_leading_wrappers(&text[start..]);
    if word.is_empty() {
        None
    } else {
        Some((word.to_string(), text.len() - word.len()))
    }
}

/// 当前目录下按前缀补全路径
pub(super) fn complete_path(prefix: &str, cwd: &str) -> Option<String> {
    let (dir_part, name_part) = match prefix.rfind('/') {
        Some(i) => {
            let dir = if prefix.starts_with('/') {
                prefix[..=i].to_string()
            } else {
                format!("{}/{}", cwd, &prefix[..=i])
            };
            (dir, prefix[i + 1..].to_string())
        }
        None => (format!("{}/", cwd), prefix.to_string()),
    };

    let entries: Vec<String> = std::fs::read_dir(&dir_part)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    let mut matches: Vec<String> = entries
        .into_iter()
        .filter(|n| n.starts_with(&name_part))
        .collect();

    matches.sort();
    if matches.is_empty() {
        return None;
    }

    if matches.len() == 1 {
        let is_dir = Path::new(&format!("{}{}", dir_part, matches[0])).is_dir();
        return Some(if is_dir {
            format!("{}/", matches[0])
        } else {
            format!("{} ", matches[0])
        });
    }

    let common = common_prefix(&matches);
    if common.len() > name_part.len() {
        Some(common)
    } else {
        None
    }
}

/// 求一组字符串的公共前缀（按字节比较）；输入为空返回空串。
fn common_prefix(v: &[String]) -> String {
    if v.is_empty() {
        return String::new();
    }

    let bytes = v[0].as_bytes();
    let mut len = bytes.len();
    for s in &v[1..] {
        let b = s.as_bytes();
        let mut i = 0;
        while i < len && i < b.len() && bytes[i] == b[i] {
            i += 1;
        }
        len = i;
    }
    v[0][..len].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::SubcommandDef;
    use crate::modes::interactive::app::App;

    #[test]
    fn apply_suggestion_replaces_trigger_text() {
        let mut a = App::new();
        a.editor.insert_text("/mo");
        a.suggestion.active = true;
        a.suggestion.kind = SuggestionKind::Commands;
        a.suggestion.start = 0;
        a.suggestion.items = vec![SuggestionItem {
            name: "model".to_string(),
            description: String::new(),
            insert: "model".to_string(),
        }];
        a.suggestion.selected = 0;
        a.apply_suggestion();
        assert_eq!(a.editor.text(), "/model ", "命令补全后应加空格（对齐 pi）");
        assert!(!a.suggestion.active);
    }

    #[test]
    fn score_ranks_filenames_before_paths() {
        // 文件名完全匹配=100 > 开头=80 > 子串=50 > 仅完整路径子串=30；目录 +10
        assert_eq!(score_entry("main.rs", "main.rs", false), 100);
        assert_eq!(score_entry("main.rs", "main", false), 80);
        assert_eq!(score_entry("xmain.rs", "main", false), 50);
        assert_eq!(score_entry("src/main", "src", false), 30);
        assert!(score_entry("main.rs", "main", false) > score_entry("xmain.rs", "main", false));
        assert!(score_entry("xmain.rs", "main", false) > score_entry("src/main", "src", false));
        assert_eq!(score_entry("adir", "ad", true), 90);
    }

    #[test]
    fn cjk_punctuation_is_autocomplete_separator() {
        assert!(is_autocomplete_separator('，'));
        assert!(is_autocomplete_separator('。'));
        assert!(is_autocomplete_separator(' '));
        // CJK 文字不是分隔符
        assert!(!is_autocomplete_separator('文'));
        assert!(!is_autocomplete_separator('a'));
    }

    #[test]
    fn last_word_splits_on_cjk_punctuation() {
        assert_eq!(last_word("查看，文档/说").unwrap().0, "文档/说");
        assert_eq!(last_word("hello world").unwrap().0, "world");
        assert_eq!(last_word("文档").unwrap().0, "文档");
    }

    /// pi d5629e204：`(~/Dev<Tab>` 这类散文包装符后的路径也要能补全，
    /// 起始索引指向包装符之后（包装符本身保留在行里）。
    #[test]
    fn last_word_strips_leading_wrappers() {
        for (text, word, start) in [
            ("(~/Dev", "~/Dev", 1),
            ("`src/ma", "src/ma", 1),
            ("<[a]b", "[a]b", 1),
            ("见 (./a", "./a", 5),
        ] {
            let (w, s) = last_word(text).unwrap();
            assert_eq!((w.as_str(), s), (word, start), "input: {text}");
        }
        // token 内已含配对闭括号：是真实路径，不剥
        assert_eq!(last_word("app/[slug]/pa").unwrap().0, "app/[slug]/pa");
        // 只剩包装符：无词可补
        assert!(last_word("(").is_none());
    }

    #[test]
    fn at_after_wrapper_is_token_start() {
        // 包装符后：视为 token 起始（`(@src`、`` `@src ``）
        assert!(is_at_token_start("(@src", 1));
        assert!(is_at_token_start("`@src", 1));
        assert!(is_at_token_start("见 (@src", 5));
        // 行首 `@`
        assert!(is_at_token_start("@src", 0));
        // 邮箱/词中 `@`：不是 token 起始
        assert!(!is_at_token_start("user@example.com", 4));
        assert!(!is_at_token_start("app/(a)@x", 7));
    }

    #[tokio::test]
    async fn at_file_suggestions_trigger_after_wrapper() {
        let mut a = App::new();
        a.editor.insert_text("见 (@src");
        a.refresh_suggestions();
        assert!(
            a.file_scan_cancel.is_some(),
            "包装符后的 @ 应被当作 token 起始并派发文件扫描"
        );
        a.cancel_file_scan();

        // 邮箱不触发
        let mut b = App::new();
        b.editor.insert_text("user@example.com");
        b.refresh_suggestions();
        assert!(!b.suggestion.active);
        assert!(b.file_scan_cancel.is_none());
        b.cancel_file_scan();
    }

    #[test]
    fn candidate_item_quotes_paths_with_separators() {
        // 含分隔符（空白/CJK 标点）的路径需加引号，否则补全后 token 被截断
        let f = candidate_item("my dir/file.txt", false, "");
        assert_eq!(f.name, "file.txt");
        assert_eq!(f.description, "my dir/file.txt");
        assert_eq!(f.insert, "\"my dir/file.txt\"");

        let plain = candidate_item("plain.txt", false, "");
        assert_eq!(plain.insert, "plain.txt");

        // 目录：名称与补全都带尾随 /（便于继续补全）
        let dir = candidate_item("src", true, "");
        assert_eq!(dir.name, "src/");
        assert_eq!(dir.insert, "src/");

        // scoped 显示前缀（`@sub/ma`）
        let scoped = candidate_item("main.rs", false, "sub/");
        assert_eq!(scoped.description, "sub/main.rs");
        assert_eq!(scoped.insert, "sub/main.rs");
    }

    #[test]
    fn ranked_candidates_order_by_score_then_depth_length_name() {
        // 对齐 pi：分数降序 → 深度升序 → 路径长度升序 → 字典序
        let mk = |score, depth, path: &str| RankedCandidate {
            score,
            depth,
            path: path.to_string(),
            is_dir: false,
        };
        let mut heap = BinaryHeap::from(vec![
            mk(80, 2, "a/bbbb.rs"),
            mk(30, 1, "a.rs"),
            mk(80, 1, "longer.rs"),
            mk(80, 1, "a.rs"),
        ]);
        let got: Vec<i32> = std::iter::from_fn(|| heap.pop().map(|c| c.score)).collect();
        // 堆顶是「最差」，逐个 pop 得到从劣到优
        assert_eq!(got, vec![30, 80, 80, 80]);
        let sorted: Vec<String> = BinaryHeap::from(vec![
            mk(80, 2, "a/bbbb.rs"),
            mk(80, 1, "longer.rs"),
            mk(80, 1, "a.rs"),
        ])
        .into_sorted_vec()
        .into_iter()
        .map(|c| c.path)
        .collect();
        assert_eq!(sorted, vec!["a.rs", "longer.rs", "a/bbbb.rs"]);
    }

    /// 建临时目录：`.git` 目录存在即被 ignore 视为 git 仓（require_git 语义）
    fn write_file(root: &Path, rel: &str, body: &str) {
        let path = root.join(rel);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(path, body).unwrap();
    }

    #[test]
    fn slash_lo_ranks_login_before_clone() {
        // 用户验收场景：/lo 时 login（匹配位置 0）必须排在 clone（位置 2）前，
        // 即使 clone 在命令列表中出现得更早
        let candidates: Vec<(&str, &str)> =
            vec![("clone", "Fork session"), ("login", "Store an API key")];
        let got: Vec<&str> = fuzzy_filter(&candidates, "lo", |c| c.0)
            .into_iter()
            .map(|c| c.0)
            .collect();
        assert_eq!(got, vec!["login", "clone"]);
    }

    #[test]
    fn skill_commands_rank_by_bare_name() {
        // 回归 #9120：未写 `skill:` 前缀时，技能候选按裸名参与模糊匹配，
        // `/idea` 不得因 `skill:` 中的 `i` 误命中 `skill:deep-research`。
        use crate::core::skills::Skill;
        let mk = |name: &str, desc: &str| Skill {
            name: name.to_string(),
            description: desc.to_string(),
            path: std::path::PathBuf::new(),
            base_dir: std::path::PathBuf::new(),
            disable_model_invocation: false,
            source: "test".to_string(),
        };
        let mut a = App::new();
        a.skills = vec![
            mk("deep-research", "Multi-agent deep research"),
            mk("research-idea", "Refine a raw idea"),
        ];
        a.editor.insert_text("/idea");
        a.refresh_suggestions();
        let names: Vec<&str> = a.suggestion.items.iter().map(|i| i.name.as_str()).collect();
        assert!(
            names.contains(&"skill:research-idea"),
            "应命中 skill:research-idea: {names:?}"
        );
        assert!(
            !names.contains(&"skill:deep-research"),
            "skill:deep-research 不应因 skill: 前缀的 i 误命中: {names:?}"
        );
    }

    #[test]
    fn cursor_movement_does_not_trigger_suggestions() {
        // 回归：移动光标不得触发建议面板（对齐 pi：只有输入才触发）。
        // `/login hfei` 把光标移到 `login` 末尾（光标前无空白），旧逻辑会误激活命令候选。
        let mut a = App::new();
        a.editor.insert_text("/login hfei");
        a.refresh_suggestions(); // 内容变化后首次刷新，同步 last_suggestion_version（/ 后含空白，不激活）
        let version = a.editor.content_version;
        for _ in 0..5 {
            a.editor.move_left();
        }
        assert_eq!(
            a.editor.cursor_col, 6,
            "光标应停在 login 后（/login 为 6 字符）"
        );
        assert_eq!(
            a.editor.content_version, version,
            "移动光标不应改变内容版本"
        );
        a.refresh_suggestions();
        assert!(!a.suggestion.active, "纯光标移动不得触发建议面板");
    }

    #[test]
    fn history_browsing_does_not_activate_suggestions() {
        // 回归：↑ 载入以 / 开头的历史后不得弹出候选列表抢占 Up/Down 焦点（对齐 pi）。
        //
        // 直接播种而不用 `submit()`：以 `/` 开头的行现在会正常进历史，↑ 载入后的行为（不激活候选列表）必须正确。
        let mut a = App::new();
        a.editor.history.push("/model".to_string());
        a.editor.clear();
        a.editor.history_prev();
        assert_eq!(a.editor.text(), "/model");
        assert!(a.editor.history_index.is_some());
        a.refresh_suggestions();
        assert!(!a.suggestion.active, "历史浏览期间不应激活 / 候选");

        // 开始编辑 → 退出历史浏览（history_index 清空）→ 后续刷新恢复正常触发
        a.editor.insert_char('x');
        assert_eq!(a.editor.history_index, None, "编辑后应退出历史浏览");
        a.editor.clear();
        a.editor.insert_text("/mod");
        a.refresh_suggestions();
        assert!(a.suggestion.active, "编辑后 / 前缀应恢复激活候选");
    }

    #[test]
    fn file_scan_result_applies_when_query_matches() {
        let mut a = App::new();
        a.editor.insert_text("@ma");
        a.apply_file_scan_result(FileScanResult {
            kind: SuggestionKind::Files,
            start: 0,
            query: "ma".to_string(),
            items: vec![SuggestionItem {
                name: "main.rs".to_string(),
                description: "main.rs".to_string(),
                insert: "main.rs".to_string(),
            }],
        });
        assert!(a.suggestion.active);
        assert_eq!(a.suggestion.query, "ma");
        assert_eq!(a.suggestion.items.len(), 1);
    }

    #[test]
    fn stale_file_scan_result_is_dropped() {
        let mut a = App::new();
        a.editor.insert_text("@ma");
        a.apply_file_scan_result(FileScanResult {
            kind: SuggestionKind::Files,
            start: 0,
            query: "mab".to_string(), // 与当前输入 @ma 不一致：过期结果
            items: vec![SuggestionItem {
                name: "mab.rs".to_string(),
                description: "mab.rs".to_string(),
                insert: "mab.rs".to_string(),
            }],
        });
        assert!(!a.suggestion.active);
    }

    #[test]
    fn stale_empty_query_scan_on_cleared_line_is_dropped() {
        // 回归（线上 panic：suggest.rs index out of bounds, len 0, index 0）：
        // 输入 "@" 派发异步扫描后清空整行，旧结果（start=0, query=""）回填时
        // 因「无 @ 也视作 query 相同」而在空行上激活候选，Tab/Enter 应用即越界。
        let mut a = App::new();
        a.editor.insert_text("@");
        a.editor.clear();
        a.apply_file_scan_result(FileScanResult {
            kind: SuggestionKind::Files,
            start: 0,
            query: String::new(),
            items: vec![SuggestionItem {
                name: "main.rs".to_string(),
                description: "main.rs".to_string(),
                insert: "main.rs".to_string(),
            }],
        });
        assert!(!a.suggestion.active, "行内已无 @：应丢弃过期扫描结果");
    }

    #[test]
    fn stale_scan_with_shifted_at_is_dropped() {
        // @ 被移到别的位置（如行首插入文本）后，start 不再匹配：结果作废
        let mut a = App::new();
        a.editor.insert_text("@ma");
        a.editor.home();
        a.editor.insert_text("see ");
        a.apply_file_scan_result(FileScanResult {
            kind: SuggestionKind::Files,
            start: 0,
            query: "ma".to_string(),
            items: vec![SuggestionItem {
                name: "main.rs".to_string(),
                description: "main.rs".to_string(),
                insert: "main.rs".to_string(),
            }],
        });
        assert!(!a.suggestion.active, "@ 位置已变化：应丢弃过期扫描结果");
    }

    // ---- 子命令候选（`/命令 ` 后的二级面板）----

    /// 带子命令的测试扩展：`/subcmd-demo pause|resume|clear`
    struct SubcmdDemoExt;

    impl crate::core::extensions::Extension for SubcmdDemoExt {
        fn name(&self) -> &str {
            "subcmd-demo-ext"
        }
        fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
            Vec::new()
        }
        fn commands(&self) -> Vec<crate::core::extensions::ExtensionCommand> {
            vec![crate::core::extensions::ExtensionCommand {
                name: "subcmd-demo".to_string(),
                description: "demo subcommands".to_string(),
                busy_safe: true,
                subcommands: vec![
                    SubcommandDef {
                        name: "pause",
                        description: "Pause it",
                    },
                    SubcommandDef {
                        name: "resume",
                        description: "Resume it",
                    },
                    SubcommandDef {
                        name: "clear",
                        description: "Clear it",
                    },
                ],
            }]
        }
    }

    /// 注册（仅一次）并启用测试扩展；调用方需持 AUTH_TEST_LOCK + AgentDirGuard
    fn ensure_subcmd_ext() {
        static REG: std::sync::Once = std::sync::Once::new();
        REG.call_once(|| crate::core::extensions::register_extension(SubcmdDemoExt));
        crate::core::extensions::set_extension_enabled("subcmd-demo-ext", true);
    }

    /// 子命令面板测试的公共前置：注册表/设置文件隔离
    fn subcmd_env() -> (
        std::sync::MutexGuard<'static, ()>,
        crate::test_support::AgentDirGuard,
    ) {
        let lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let ad = crate::test_support::AgentDirGuard::temp();
        ensure_subcmd_ext();
        (lock, ad)
    }

    fn item_names(a: &App) -> Vec<&str> {
        a.suggestion.items.iter().map(|i| i.name.as_str()).collect()
    }

    #[test]
    fn subcommand_panel_appears_after_command_space() {
        let _env = subcmd_env();

        // 一级：`/subcmd-demo` 仍是命令候选
        let mut a = App::new();
        a.editor.insert_text("/subcmd-demo");
        a.refresh_suggestions();
        assert!(a.suggestion.active);
        assert_eq!(a.suggestion.kind, SuggestionKind::Commands);

        // 末尾空格：切到子命令面板，顺序 = 声明顺序（空查询的默认选中项是第一个）
        a.editor.insert_char(' ');
        a.refresh_suggestions();
        assert!(a.suggestion.active, "`/subcmd-demo ` 应弹出子命令面板");
        assert_eq!(a.suggestion.kind, SuggestionKind::Subcommands);
        assert_eq!(item_names(&a), vec!["pause", "resume", "clear"]);
        assert_eq!(a.suggestion.items[0].description, "Pause it");
        assert_eq!(a.suggestion.selected, 0);

        // Tab 补全一级命令后（`/subcmd-demo `）立即进入子命令面板
        let mut a = App::new();
        a.editor.insert_text("/subcmd-demo");
        a.refresh_suggestions();
        a.apply_suggestion();
        assert_eq!(a.editor.text(), "/subcmd-demo ");
        a.refresh_suggestions();
        assert_eq!(a.suggestion.kind, SuggestionKind::Subcommands);
    }

    #[test]
    fn subcommand_query_filters_and_second_space_closes() {
        let _env = subcmd_env();
        let mut a = App::new();
        a.editor.insert_text("/subcmd-demo p");
        a.refresh_suggestions();
        assert_eq!(a.suggestion.kind, SuggestionKind::Subcommands);
        assert_eq!(item_names(&a), vec!["pause"], "p 应只匹配 pause");

        // 第二个空格 = 已在写参数（如 /goal <目标>）：关闭面板
        a.editor.insert_char(' ');
        a.refresh_suggestions();
        assert!(!a.suggestion.active, "子命令后第二个空格应关闭面板");
    }

    #[test]
    fn subcommand_apply_completes_with_trailing_space() {
        let _env = subcmd_env();
        let mut a = App::new();
        a.editor.insert_text("/subcmd-demo cl");
        a.refresh_suggestions();
        assert!(a.suggestion.active);
        a.apply_suggestion();
        assert_eq!(a.editor.text(), "/subcmd-demo clear ");
        assert!(!a.suggestion.active, "补全后尾随空格导致面板关闭");

        // 多敲的空格被归一化（锚点为命令名后的第一个空白）
        let mut a = App::new();
        a.editor.insert_text("/subcmd-demo  cl");
        a.refresh_suggestions();
        a.apply_suggestion();
        assert_eq!(a.editor.text(), "/subcmd-demo clear ");
    }

    #[test]
    fn slash_suggestions_allow_leading_whitespace() {
        // pi #10218：输入以空白开头时也要触发命令/子命令候选
        let _env = subcmd_env();

        // 命令候选：前导空白不影响，且替换锚点指回 '/'
        let mut a = App::new();
        a.editor.insert_text("   /subcmd-de");
        a.refresh_suggestions();
        assert!(a.suggestion.active, "前导空白后仍应弹出命令面板");
        assert_eq!(a.suggestion.kind, SuggestionKind::Commands);
        assert_eq!(a.suggestion.start, 3, "锚点应指向 '/' 而不是 0");
        a.apply_suggestion();
        assert_eq!(a.editor.text(), "   /subcmd-demo ");

        // 子命令候选：同样支持前导空白
        let mut a = App::new();
        a.editor.insert_text("  /subcmd-demo cl");
        a.refresh_suggestions();
        assert_eq!(a.suggestion.kind, SuggestionKind::Subcommands);
        a.apply_suggestion();
        assert_eq!(a.editor.text(), "  /subcmd-demo clear ");
    }

    #[test]
    fn no_subcommand_panel_without_declared_subcommands() {
        let _env = subcmd_env();

        // 内置命令无子命令：`/model ` 不弹面板
        let mut a = App::new();
        a.editor.insert_text("/model ");
        a.refresh_suggestions();
        assert!(!a.suggestion.active, "无子命令的 /model 不应弹面板");

        // 未知命令
        let mut a = App::new();
        a.editor.insert_text("/nope ");
        a.refresh_suggestions();
        assert!(!a.suggestion.active);

        // 查询无匹配：面板关闭（不残留空列表）
        let mut a = App::new();
        a.editor.insert_text("/subcmd-demo zz");
        a.refresh_suggestions();
        assert!(!a.suggestion.active, "无匹配子命令应关闭面板");
    }

    #[test]
    fn apply_suggestion_on_empty_line_is_safe() {
        // 防御：候选仍标记 active 但行内容已空时，应用不得 panic，且应关闭候选
        let mut a = App::new();
        a.editor.insert_text("@");
        a.suggestion.active = true;
        a.suggestion.kind = SuggestionKind::Files;
        a.suggestion.start = 0;
        a.suggestion.items = vec![SuggestionItem {
            name: "main.rs".to_string(),
            description: "main.rs".to_string(),
            insert: "main.rs".to_string(),
        }];
        a.suggestion.selected = 0;
        a.editor.clear();
        a.apply_suggestion();
        assert_eq!(a.editor.text(), "");
        assert!(!a.suggestion.active);
    }

    #[test]
    fn apply_suggestion_with_no_items_deactivates_panel() {
        // 无候选（查询无匹配）时按 Enter：不得把“No matching commands”面板留在原地，
        // 否则它会停在随后的覆盖层上方。
        let mut a = App::new();
        a.editor.insert_text("/agents workflowz");
        a.suggestion.active = true;
        a.suggestion.kind = SuggestionKind::Subcommands;
        a.suggestion.start = 7;
        a.suggestion.items.clear();
        a.suggestion.selected = 0;
        a.apply_suggestion();
        assert!(!a.suggestion.active, "无候选时也应关闭候选面板");
        assert!(a.suggestion.items.is_empty());
    }

    #[test]
    fn apply_suggestion_with_mismatched_trigger_is_skipped() {
        // 防御：start 处不是本次触发符（如陈旧 Files 候选指向普通字符）时不应用
        let mut a = App::new();
        a.editor.insert_text("hello");
        a.suggestion.active = true;
        a.suggestion.kind = SuggestionKind::Files;
        a.suggestion.start = 3;
        a.suggestion.items = vec![SuggestionItem {
            name: "main.rs".to_string(),
            description: "main.rs".to_string(),
            insert: "main.rs".to_string(),
        }];
        a.suggestion.selected = 0;
        a.apply_suggestion();
        assert_eq!(a.editor.text(), "hello", "触发符不匹配时不得改写输入");
        assert!(!a.suggestion.active);
    }

    #[test]
    fn walk_scan_and_score_without_external_fd() {
        // 不再依赖外部 fd：任何环境都能跑（旧实现无 fd 时静默跳过）
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write_file(root, "sub/main.rs", "x");
        write_file(root, "src/clone.txt", "x");
        write_file(root, "login.txt", "x");
        let cwd = root.to_str().unwrap();
        let never = AtomicBool::new(false);
        let inserts = |items: &[SuggestionItem]| -> Vec<String> {
            items.iter().map(|i| i.insert.clone()).collect()
        };

        // @lo：文件名匹配优先（login 开头命中 > clone 子串命中）
        let items = file_suggestions_walk("lo", cwd, &never);
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names[0], "login.txt", "got: {names:?}");
        assert!(names.contains(&"clone.txt"), "got: {names:?}");

        // 结果与遍历顺序（并行调度）无关：重复扫描得到同一序列
        assert_eq!(
            inserts(&file_suggestions_walk("lo", cwd, &never)),
            inserts(&items)
        );

        // @sub/ma：scoped 到 sub 目录，插入相对路径
        let items = file_suggestions_walk("sub/ma", cwd, &never);
        let got = inserts(&items);
        assert!(got.contains(&"sub/main.rs".to_string()), "got: {got:?}");
    }

    #[test]
    fn walk_honors_gitignore_fdignore_hidden_and_skips_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".git")).unwrap(); // 仅需 .git 存在
        std::fs::write(root.join(".gitignore"), "secret.txt\n").unwrap();
        std::fs::write(root.join(".fdignore"), "fdignored.txt\n").unwrap();
        write_file(root, "secret.txt", "x");
        write_file(root, "fdignored.txt", "x");
        write_file(root, "kept.txt", "x");
        write_file(root, ".hidden.txt", "x");
        write_file(root, ".git/config", "x");

        let items = file_suggestions_walk("", root.to_str().unwrap(), &AtomicBool::new(false));
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"kept.txt"), "got: {names:?}");
        assert!(
            names.contains(&".hidden.txt"),
            "--hidden：隐藏文件可见: {names:?}"
        );
        assert!(!names.contains(&"secret.txt"), ".gitignore 生效: {names:?}");
        assert!(
            !names.contains(&"fdignored.txt"),
            ".fdignore 生效: {names:?}"
        );
        assert!(!names.contains(&".git/"), ".git 目录不参与候选: {names:?}");
    }

    #[test]
    fn walk_ignores_gitignore_outside_git_repo() {
        // 对齐 fd 10 默认（require_git）：非 git 目录不应用 .gitignore，但 .ignore 仍生效
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".gitignore"), "secret.txt\n").unwrap();
        std::fs::write(root.join(".ignore"), "plainignored.txt\n").unwrap();
        write_file(root, "secret.txt", "x");
        write_file(root, "plainignored.txt", "x");

        let items = file_suggestions_walk("", root.to_str().unwrap(), &AtomicBool::new(false));
        let names: Vec<&str> = items.iter().map(|i| i.name.as_str()).collect();
        assert!(
            names.contains(&"secret.txt"),
            "非 git 仓：.gitignore 不生效: {names:?}"
        );
        assert!(
            !names.contains(&"plainignored.txt"),
            ".ignore 生效: {names:?}"
        );
    }

    #[test]
    fn walk_cancel_stops_scan() {
        let dir = tempfile::tempdir().unwrap();
        write_file(dir.path(), "main.rs", "x");
        let cancel = AtomicBool::new(true); // 一进去就作废
        let items = file_suggestions_walk("", dir.path().to_str().unwrap(), &cancel);
        assert!(items.is_empty(), "取消后不应产出候选: {items:?}");
    }

    #[test]
    fn split_base_and_query_handles_root_and_scoped() {
        // `@/x`：display_base 为 "/"，base_dir 必须是根而非空串（旧实现 trim 成空 → 永远无候选）
        let (base, query, display) = split_base_and_query("/etc", "/tmp").unwrap();
        assert_eq!(base, "/");
        assert_eq!(query, "etc");
        assert_eq!(display, "/");

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_str().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();

        let (base, query, display) = split_base_and_query("sub/ma", root).unwrap();
        assert_eq!(base, format!("{root}/sub"));
        assert_eq!(query, "ma");
        assert_eq!(display, "sub/");

        // 目录不存在 → 无候选
        assert!(split_base_and_query("nope/x", root).is_none());
    }

    #[test]
    fn at_file_candidate_appends_to_cursor_like_tab() {
        // Enter 与 Tab 共用 apply_suggestion：@ 前缀候选把文件位置替换到光标处
        let mut a = App::new();
        a.editor.insert_text("see @fi");
        a.suggestion.active = true;
        a.suggestion.kind = SuggestionKind::Files;
        a.suggestion.start = 4; // '@' 的字符位置
        a.suggestion.items = vec![SuggestionItem {
            name: "file.txt".to_string(),
            description: "file".to_string(),
            insert: "file.txt".to_string(),
        }];
        a.suggestion.selected = 0;
        a.apply_suggestion();
        assert_eq!(
            a.editor.text(),
            "see @file.txt ",
            "文件补全后应加空格（对齐 pi）"
        );
        assert!(!a.suggestion.active);
    }

    #[tokio::test]
    async fn at_input_full_chain_activates_suggestion() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("login.txt"), "x").unwrap();
        std::fs::write(dir.path().join("clone.txt"), "x").unwrap();

        let (ui_tx, mut ui_rx) =
            tokio::sync::mpsc::unbounded_channel::<crate::modes::interactive::UiTask>();
        crate::modes::interactive::set_event_loop_tx(ui_tx);
        let mut st = App::new();
        st.cwd = dir.path().to_str().unwrap().to_string();
        st.editor.insert_text("@");

        // 模拟 handle_key 尾部的刷新：派生 blocking 任务做后台遍历
        st.refresh_suggestions();
        assert!(st.file_scan_cancel.is_some(), "应记录进行中的扫描中止开关");

        // 模拟主循环 select 分支：接收闭包并应用到 App。
        // 事件循环队列是进程级全局（其他用例可能在向它投递），必须过滤掉
        // 与本次 @ 查询无关的闭包，直到候选被真正激活。
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let task = tokio::time::timeout_at(deadline, ui_rx.recv())
                .await
                .expect("timeout waiting for file scan")
                .expect("channel closed");
            task(&mut st);
            if st.suggestion.active {
                break;
            }
        }
        crate::modes::interactive::clear_event_loop_tx();

        assert!(st.suggestion.active, "@ 候选应激活");
        assert_eq!(st.suggestion.kind, SuggestionKind::Files);
        let names: Vec<&str> = st
            .suggestion
            .items
            .iter()
            .map(|i| i.name.as_str())
            .collect();
        assert!(names.contains(&"login.txt"), "got: {:?}", names);
    }

    #[tokio::test]
    async fn new_scan_cancels_previous_one() {
        // 连续输入会在后台堆扫描：新扫描接管后旧扫描应被置位作废。
        // 本用例不碰全局事件循环队列（那是进程级状态，并行用例间会互相干扰）：
        // run_in_event_loop 失败/投递到别的队列都不影响中止开关的断言。
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("login.txt"), "x").unwrap();

        let mut st = App::new();
        st.cwd = dir.path().to_str().unwrap().to_string();

        st.editor.insert_text("@lo");
        st.refresh_suggestions();
        let first = st.file_scan_cancel.clone().expect("第一次扫描");

        st.editor.insert_text("g"); // @log
        st.refresh_suggestions();
        assert!(
            first.load(AtomicOrdering::Relaxed),
            "新扫描派发时旧扫描应被作废"
        );
        let second = st.file_scan_cancel.clone().expect("第二次扫描");
        assert!(
            !second.load(AtomicOrdering::Relaxed),
            "新扫描自身不应被作废"
        );

        // 删掉 @ 后候选关闭：进行中的扫描也应作废
        st.editor.clear();
        st.refresh_suggestions();
        assert!(st.file_scan_cancel.is_none(), "取消后不再持有中止开关");
        assert!(second.load(AtomicOrdering::Relaxed), "关闭候选应作废扫描");
    }
}

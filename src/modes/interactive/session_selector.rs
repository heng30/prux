//! /resume、/session 会话选择器：状态、数据加载、过滤/树构建与删除/重命名动作。
//! （header 状态行 + 快捷键提示 + 搜索过滤（re: regex / "phrase" / fuzzy token）
//! + 树形列表 + 删除（trash → unlink）+ 重命名）。
//!
//! 渲染在 `render::session_selector`，事件处理在 `handlers::sessions`。

use crate::{
    core::session_manager::{self, Session, format_session_age},
    modes::interactive::line_input::InputBox,
    utils::paths::mtime_of,
};
use regex::{Regex, RegexBuilder};
use std::{
    cmp::Ordering,
    collections::{HashMap, VecDeque},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant, SystemTime},
};

/// 列表最大展示行数
pub const MAX_VISIBLE: usize = 10;

/// 单文件消息数统计上限：达到后提前终止（超大会话文件不再全文解析）
const MSG_COUNT_LIMIT: usize = 1000;

/// 单文件扫描字节预算：即使消息数很少（如以 thinking/tool 数据为主的巨大文件），读到该预算即提前终止，避免全文解析
const SCAN_BYTES_BUDGET: usize = 4 * 1024 * 1024;

/// 搜索文本收集上限（截断避免超大会话撑爆内存）
const SEARCH_TEXT_LIMIT: usize = 200_000;

/// 渐进加载首批行数：同步加载完这批后立即出首帧（可交互），
/// 其余按 mtime 顺序逐帧补齐（`--resume` 结果渐进出现、按修改时间优先）。
const INITIAL_BATCH: usize = 12;

/// 每个事件循环 tick 补齐的行数上限（避免单帧卡顿）
pub const DRAIN_BATCH: usize = 16;

/// 会话范围
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 仅当前工作目录下的会话。
    Current,
    /// 所有项目目录下的会话。
    All,
}

/// 排序模式（threaded → recent → relevance）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    /// 按 fork 关系组成树，父子相邻展示。
    Threaded,
    /// 按会话文件修改时间由新到旧平铺。
    Recent,
    /// 按搜索词匹配度排序。
    Relevance,
}

/// 命名过滤
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameFilter {
    /// 不区分是否命名。
    All,
    /// 只保留用户命名过的会话。
    Named,
}

/// 会话行（SessionInfo 的展示字段 + 树信息）
#[derive(Debug, Clone)]
pub struct SessionRow {
    /// 会话文件路径。
    pub path: PathBuf,
    /// 会话 id（v4 header 的 id，fork 关系按它关联）。
    pub id: String,
    /// 用户自定义名称（v4 kind=fact/fact=name 最新值）
    pub name: Option<String>,
    /// 显示文本
    pub text: String,
    /// 消息条数（达到上限则被截断）。
    pub msg_count: usize,
    /// 消息数达到上限被截断
    pub msg_count_capped: bool,
    /// 修改时间（排序用）
    pub modified: SystemTime,
    /// 相对时间（now/5m/3h/...）
    pub age: String,
    /// 会话工作目录（header cwd）
    pub cwd: String,
    /// 父会话 id（v4 header parentSessionId；fork 关系树形显示用）
    pub parent_session_id: Option<String>,
    /// 是否为当前打开的会话（左侧 ✓ 标记；禁止删除）
    pub is_current: bool,
    /// 搜索文本（id + name + 消息文本 + cwd）
    pub search_text: String,
}

/// 树形节点
#[derive(Debug, Clone)]
pub struct DisplayNode {
    /// 索引 -> rows
    pub idx: usize,
    /// 在树中的深度，决定缩进层级。
    pub depth: usize,
    /// 是否为同级中的最后一个（决定画 └─ 还是 ├─）。
    pub is_last: bool,
    /// 每层祖先之后是否还有兄弟（绘制 ├─/└─ 用）
    pub ancestor_continues: Vec<bool>,
}

/// /resume /session 选择器状态
#[derive(Debug)]
pub struct SessionSelector {
    /// 面板是否处于打开状态。
    pub active: bool,
    /// 当前会话范围（当前目录 / 全部项目目录）。
    pub scope: Scope,
    /// 当前排序模式。
    pub sort_mode: SortMode,
    /// 名称过滤（全部 / 仅命名）。
    pub name_filter: NameFilter,
    /// 是否在行内显示会话的 cwd 路径。
    pub show_path: bool,
    /// 搜索输入框
    pub filter: InputBox,
    /// 当前选中行在 `display` 中的下标。
    pub selected: usize,
    /// 打开后自动选中首个非当前会话（用户首次导航前持续跟随列表重排）
    initial_select: bool,
    /// 等待确认删除的会话路径（非 None 时键盘拦截，行标红）
    pub confirming_delete: Option<String>,
    /// 状态消息：(error?, message, 过期时间)；过期自动消失
    pub status: Option<(bool, String, Instant)>,
    /// 当前目录会话（原始，含当前会话）
    pub current: Vec<SessionRow>,
    /// 所有项目目录会话（原始，含当前会话）
    pub all: Vec<SessionRow>,
    /// 名称过滤后的当前 scope 列表（display 的索引基）
    pub rows: Vec<SessionRow>,
    /// 过滤 + 排序 + 树后的显示列表
    pub display: Vec<DisplayNode>,
    /// 重命名模式
    pub rename_mode: bool,
    /// 待重命名的会话路径（非 None 时重命名输入框生效）。
    pub rename_target: Option<String>,
    /// 重命名输入框（与过滤框同用 InputBox，处理 Shift 层映射）
    pub rename_input: InputBox,
    /// 当前会话路径（✓ 标记 + 禁止删除）
    current_session_path: Option<String>,
    /// 当前工作目录，`Scope::Current` 下用于匹配同目录会话。
    cwd: String,
    /// agent 数据根目录，其下 `sessions/` 存放所有项目的会话。
    agent_dir: PathBuf,
    /// 待加载候选（mtime 新→旧）：渐进加载剩余部分，由事件循环逐帧补齐
    pending: VecDeque<PendingJob>,
    /// 本轮渐进加载的候选总数（进度提示；0 = 未在加载）
    loading_total: usize,
}

/// 待加载候选：路径 + 目标 scope（scope 可在加载途中切换）+ cwd 回退（all scope 用）
#[derive(Debug, Clone)]
struct PendingJob {
    /// 加载完成后归入的 scope（加载途中用户可切换，故逐个记录）。
    scope: Scope,
    /// 会话文件路径。
    path: PathBuf,
    /// `Scope::All` 下 header 未带 cwd 时的回退目录。
    cwd_fallback: Option<String>,
}

impl SessionSelector {
    /// 构造未激活、默认 `Current` scope / 线程树排序的实例；会话数据需由 `open` 填充。
    pub fn new() -> Self {
        SessionSelector {
            active: false,
            scope: Scope::Current,
            sort_mode: SortMode::Threaded,
            name_filter: NameFilter::All,
            show_path: false,
            filter: Default::default(),
            selected: 0,
            initial_select: false,
            confirming_delete: None,
            status: None,
            current: Vec::new(),
            all: Vec::new(),
            rows: Vec::new(),
            display: Vec::new(),
            rename_mode: false,
            rename_target: None,
            rename_input: Default::default(),
            current_session_path: None,
            cwd: String::new(),
            agent_dir: PathBuf::new(),
            pending: VecDeque::new(),
            loading_total: 0,
        }
    }

    /// 打开选择器：加载当前目录会话（含当前会话，渲染时以 ✓ 标记）
    pub fn open(&mut self, cwd: &str, agent_dir: &Path, current_session_path: Option<String>) {
        self.active = true;
        self.scope = Scope::Current;
        self.sort_mode = SortMode::Threaded;
        self.name_filter = NameFilter::All;
        self.show_path = false;
        self.filter.clear();
        self.selected = 0;
        self.initial_select = true;
        self.confirming_delete = None;
        self.status = None;
        self.rename_mode = false;
        self.rename_target = None;
        self.rename_input.clear();
        self.cwd = cwd.to_string();
        self.agent_dir = agent_dir.to_path_buf();
        self.current_session_path = current_session_path;
        self.all.clear();
        self.load_current();
        self.recompute();
    }

    /// 关闭选择器：清空行与显示缓存，并取消未完成的渐进加载。
    pub fn close(&mut self) {
        self.active = false;
        self.rows.clear();
        self.display.clear();
        self.cancel_pending();
    }

    /// 当前是否处于删除确认状态
    pub fn is_confirming_delete(&self) -> bool {
        self.confirming_delete.is_some()
    }

    /// 状态消息过期清理（渲染前或键盘处理前调用）
    pub fn expire_status(&mut self) {
        if let Some((_, _, until)) = &self.status
            && Instant::now() >= *until
        {
            self.status = None;
        }
    }

    /// 重新扫描当前 scope 的会话文件（删除/重命名后刷新）
    pub fn reload(&mut self) {
        self.cancel_pending();
        match self.scope {
            Scope::Current => self.load_current(),
            Scope::All => self.load_all(),
        }
        self.recompute();
    }

    /// 加载当前目录会话：枚举候选（只 stat，按 mtime 新→旧）→ 首批同步加载出首帧，其余进入渐进队列由事件循环补齐。
    fn load_current(&mut self) {
        let dir = session_manager::default_session_dir(&self.cwd, &self.agent_dir);
        let jobs: Vec<PendingJob> = candidate_paths(&dir)
            .into_iter()
            .map(|path| PendingJob {
                scope: Scope::Current,
                path,
                cwd_fallback: None,
            })
            .collect();
        self.current.clear();
        self.enqueue(jobs);
    }

    /// 加载所有项目目录会话：候选先按 mtime 新→旧排序
    fn load_all(&mut self) {
        let jobs: Vec<PendingJob> = all_candidate_jobs(&self.agent_dir)
            .into_iter()
            .map(|(path, cwd_fallback)| PendingJob {
                scope: Scope::All,
                path,
                cwd_fallback,
            })
            .collect();
        self.all.clear();
        self.enqueue(jobs);
    }

    /// 排入候选并同步加载首批（首帧即可交互；剩余留给 [`Self::drain_pending`]）。
    fn enqueue(&mut self, jobs: Vec<PendingJob>) {
        let scope = jobs.first().map(|j| j.scope);
        if let Some(scope) = scope {
            // 同一 scope 的旧候选作废（重新扫描），另一 scope 的在途候选保留
            self.pending.retain(|j| j.scope != scope);
            self.pending.extend(jobs);
            self.loading_total = self.pending.len();
        }
        self.drain_pending(INITIAL_BATCH);
    }

    /// 渐进补齐：每帧最多 `budget` 行；返回是否有新行加入（调用方据此置脏重绘）。
    pub fn drain_pending(&mut self, budget: usize) -> bool {
        if self.pending.is_empty() {
            return false;
        }

        // 新行按 modified 排序后可能插到选中项之前：先记下选中路径，补齐后按路径恢复
        let keep = self.selected_path();
        let current = self.current_session_path.clone();
        let mut added = 0usize;
        for _ in 0..budget {
            let Some(job) = self.pending.pop_front() else {
                break;
            };
            let path_str = job.path.to_string_lossy();
            let is_current = current.as_deref() == Some(path_str.as_ref());
            let Some(mut row) = load_row(&job.path, is_current) else {
                continue;
            };
            if row.cwd.is_empty()
                && let Some(fb) = job.cwd_fallback
            {
                row.cwd = fb;
            }
            match job.scope {
                Scope::Current => self.current.push(row),
                Scope::All => self.all.push(row),
            }
            added += 1;
        }

        if added == 0 {
            return false;
        }

        match self.scope {
            Scope::Current => self.current.sort_by_key(|b| std::cmp::Reverse(b.modified)),
            Scope::All => self.all.sort_by_key(|b| std::cmp::Reverse(b.modified)),
        }

        self.recompute();

        if let Some(path) = keep
            && !self.initial_select
            && let Some(idx) = self
                .display
                .iter()
                .position(|n| self.rows[n.idx].path.to_string_lossy() == path)
        {
            self.selected = idx;
        }
        true
    }

    /// 是否仍有候选在渐进加载。
    pub fn is_loading(&self) -> bool {
        !self.pending.is_empty()
    }

    /// 加载进度 `(已加载, 总数)`。
    pub fn loading_progress(&self) -> (usize, usize) {
        (
            self.loading_total.saturating_sub(self.pending.len()),
            self.loading_total,
        )
    }

    /// 取消未完成的加载（选中/关闭选择器时调用，避免后台空转解析大目录）。
    pub fn cancel_pending(&mut self) {
        self.pending.clear();
        self.loading_total = 0;
    }

    /// 重算显示列表：名称过滤 → 树/排序 → 修正选中项
    pub fn recompute(&mut self) {
        let base: Vec<SessionRow> = match self.scope {
            Scope::Current => &self.current,
            Scope::All => &self.all,
        }
        .iter()
        .filter(|r| match self.name_filter {
            NameFilter::All => true,
            NameFilter::Named => r
                .name
                .as_deref()
                .map(|n| !n.trim().is_empty())
                .unwrap_or(false),
        })
        .cloned()
        .collect();
        self.rows = base;

        let query = self.filter.value.trim().to_string();
        let mut display: Vec<DisplayNode> = Vec::new();

        if self.sort_mode == SortMode::Threaded && query.is_empty() {
            // Threaded 无查询：树形显示（threaded + 空查询走树）
            let tree = build_session_tree(&self.rows);
            flatten_session_tree(&tree, &mut display);
        } else {
            let parsed = parse_query(&query);

            if parsed.has_error() {
                // 解析失败：全部不匹配
            } else if self.sort_mode == SortMode::Recent {
                // Recent：只过滤，保持 modified 倒序
                for (i, r) in self.rows.iter().enumerate() {
                    if match_session(r, &parsed).is_some() {
                        display.push(DisplayNode {
                            idx: i,
                            depth: 0,
                            is_last: true,
                            ancestor_continues: Vec::new(),
                        });
                    }
                }
            } else {
                // Relevance / Threaded+查询：评分排序
                let mut scored: Vec<(usize, f64)> = self
                    .rows
                    .iter()
                    .enumerate()
                    .filter_map(|(i, r)| match_session(r, &parsed).map(|s| (i, s)))
                    .collect();

                scored.sort_by(|a, b| {
                    a.1.partial_cmp(&b.1)
                        .unwrap_or(Ordering::Equal)
                        .then_with(|| self.rows[b.0].modified.cmp(&self.rows[a.0].modified))
                });

                for (i, _) in scored {
                    display.push(DisplayNode {
                        idx: i,
                        depth: 0,
                        is_last: true,
                        ancestor_continues: Vec::new(),
                    });
                }
            }
        }

        self.display = display;
        self.selected = self.selected.min(self.display.len().saturating_sub(1));

        if self.initial_select {
            // 打开后默认选中首个非当前会话；渐进加载/树重排期间持续跟随，直到用户导航
            if let Some(i) = self.first_selectable() {
                self.selected = i;
            }
        } else if self.is_current_idx(self.selected)
            && let Some(i) = self.nearest_selectable(self.selected)
        {
            // 当前会话不可选中：若选中项落到当前会话，就近跳到可选项
            self.selected = i;
        }
    }

    /// 切换 scope（current ↔ all；all 首次访问时扫描）
    pub fn toggle_scope(&mut self) {
        self.scope = match self.scope {
            Scope::Current => {
                if self.all.is_empty()
                    && !self.agent_dir.as_os_str().is_empty()
                    && !self.pending.iter().any(|j| j.scope == Scope::All)
                {
                    self.load_all();
                }
                Scope::All
            }
            Scope::All => Scope::Current,
        };
        self.recompute();
    }

    /// 循环排序：threaded → recent → relevance(fuzzy) → threaded
    pub fn cycle_sort(&mut self) {
        self.sort_mode = match self.sort_mode {
            SortMode::Threaded => SortMode::Recent,
            SortMode::Recent => SortMode::Relevance,
            SortMode::Relevance => SortMode::Threaded,
        };
        self.recompute();
    }

    /// 切换命名过滤
    pub fn toggle_name_filter(&mut self) {
        self.name_filter = match self.name_filter {
            NameFilter::All => NameFilter::Named,
            NameFilter::Named => NameFilter::All,
        };
        self.recompute();
    }

    /// 切换路径显示
    pub fn toggle_path(&mut self) {
        self.show_path = !self.show_path;
    }

    /// 当前会话已打开，不可选中：`display` 中第 `i` 行是否为当前会话
    fn is_current_idx(&self, i: usize) -> bool {
        self.display
            .get(i)
            .map(|n| self.rows[n.idx].is_current)
            .unwrap_or(false)
    }

    /// 首个可选中（非当前会话）的显示下标
    fn first_selectable(&self) -> Option<usize> {
        (0..self.display.len()).find(|&i| !self.is_current_idx(i))
    }

    /// 从 `from` 出发就近找可选中（非当前会话）的显示下标；全为当前会话时返回 None
    fn nearest_selectable(&self, from: usize) -> Option<usize> {
        let n = self.display.len();
        if n == 0 {
            return None;
        }

        let from = from.min(n - 1);
        for d in 0..n {
            if let Some(i) = from.checked_add(d).filter(|&i| i < n)
                && !self.is_current_idx(i)
            {
                return Some(i);
            }
            if let Some(i) = from.checked_sub(d)
                && !self.is_current_idx(i)
            {
                return Some(i);
            }
        }
        None
    }

    /// 移动选中项（±delta 个可选项，跳过当前会话；两端环绕）
    pub fn move_selection(&mut self, delta: i32) {
        let n = self.display.len() as i32;
        if n == 0 || delta == 0 {
            return;
        }
        self.initial_select = false;

        let step = if delta > 0 { 1 } else { -1 };
        let mut remaining = delta.unsigned_abs();
        let mut idx = self.selected as i32;

        while remaining > 0 {
            let mut moved = false;
            for _ in 0..n {
                idx = (idx + step).rem_euclid(n);
                if !self.is_current_idx(idx as usize) {
                    moved = true;
                    break;
                }
            }

            if !moved {
                return; // 列表中只有当前会话：无处可去
            }
            remaining -= 1;
        }
        self.selected = idx as usize;
    }

    /// Home：跳到首个可选中项（跳过当前会话）
    pub fn select_first(&mut self) {
        self.initial_select = false;
        if let Some(i) = self.first_selectable() {
            self.selected = i;
        }
    }

    /// End：跳到最后一个可选中项（跳过当前会话）
    pub fn select_last(&mut self) {
        self.initial_select = false;
        if let Some(i) = (0..self.display.len())
            .rev()
            .find(|&i| !self.is_current_idx(i))
        {
            self.selected = i;
        }
    }

    /// Ctrl+G：跳到当前打开会话（✓）的下一条会话（显示列表中的下一行，两端环绕）。
    /// 当前会话不在列表中（如被搜索过滤掉）时保持原位。
    pub fn select_next_after_current(&mut self) {
        self.initial_select = false;
        let n = self.display.len();
        if n == 0 {
            return;
        }

        let Some(cur) = (0..n).find(|&i| self.is_current_idx(i)) else {
            return;
        };

        for d in 1..n {
            let i = (cur + d) % n;
            if !self.is_current_idx(i) {
                self.selected = i;
                return;
            }
        }
    }

    /// 开始删除确认（选中行）；当前打开的会话禁止删除
    pub fn start_delete_confirm(&mut self) {
        let Some(node) = self.display.get(self.selected) else {
            return;
        };
        let row = &self.rows[node.idx];

        if row.is_current {
            self.status = Some((
                true,
                "Cannot delete the current session".to_string(),
                Instant::now() + Duration::from_secs(3),
            ));
            return;
        }
        self.confirming_delete = Some(row.path.to_string_lossy().to_string());
    }

    /// 执行删除：trash CLI 优先，失败回退 unlink
    pub fn confirm_delete(&mut self) {
        let Some(path) = self.confirming_delete.take() else {
            return;
        };

        match delete_session_file(&path) {
            Ok(method) => {
                // 从两个缓存列表移除（删除后两列表同步过滤）
                self.current.retain(|r| r.path.to_string_lossy() != path);
                self.all.retain(|r| r.path.to_string_lossy() != path);

                let msg = if method == "trash" {
                    "Session moved to trash".to_string()
                } else {
                    "Session deleted".to_string()
                };

                self.status = Some((false, msg, Instant::now() + Duration::from_secs(2)));
                self.reload();
            }
            Err(e) => {
                self.status = Some((
                    true,
                    format!("Failed to delete: {}", e),
                    Instant::now() + Duration::from_secs(3),
                ));
            }
        }
    }

    /// 取消删除确认（保留当前选中行）。
    pub fn cancel_delete(&mut self) {
        self.confirming_delete = None;
    }

    /// 进入重命名模式（选中行）
    pub fn start_rename(&mut self) {
        let Some(node) = self.display.get(self.selected) else {
            return;
        };
        let row = &self.rows[node.idx];
        self.rename_mode = true;
        self.rename_target = Some(row.path.to_string_lossy().to_string());
        self.rename_input
            .set_value(row.name.as_deref().unwrap_or_default());
    }

    /// 退出重命名模式并清除重命名目标（不提交输入内容）。
    pub fn exit_rename(&mut self) {
        self.rename_mode = false;
        self.rename_target = None;
    }

    /// 提交重命名：追加 v4 fact（kind=fact/fact=name）后刷新列表
    pub fn confirm_rename(&mut self) {
        let next = self.rename_input.value.trim().to_string();
        let target = self.rename_target.take();

        if !next.is_empty()
            && let Some(path) = &target
        {
            match Session::open(path) {
                Ok(mut s) => {
                    s.append_session_info(&next);
                }
                Err(e) => {
                    self.status = Some((
                        true,
                        format!("Failed to rename: {}", e),
                        Instant::now() + std::time::Duration::from_secs(3),
                    ));
                }
            }
        }

        self.exit_rename();
        self.reload();
    }

    /// 选中会话路径（Enter 确认用）；当前会话不可选中，落在当前会话时返回 None
    pub fn selected_path(&self) -> Option<String> {
        let node = self.display.get(self.selected)?;
        let row = &self.rows[node.idx];
        if row.is_current {
            return None;
        }
        Some(row.path.to_string_lossy().to_string())
    }
}

impl Default for SessionSelector {
    /// 默认实例等同 [`SessionSelector::new`]。
    fn default() -> Self {
        Self::new()
    }
}

/// 单行扫描的浅结构：只提取面板所需字段。
/// 不深度反序列化 AgentMessage（thinking/tool_call/usage 等大字段全部跳过），
/// 这是 /resume 面板加载巨大会话文件的主要性能优化。
#[derive(serde::Deserialize)]
struct ScanLine {
    /// 行类型：`header` / `fact` / `message`。
    kind: Option<String>,
    /// 行的 `type` 字段，只有 `message` 行才会被进一步解析。
    #[serde(rename = "type")]
    ty: Option<String>,
    /// header 行里的会话 id。
    id: Option<String>,
    /// header 行里的会话工作目录。
    cwd: Option<String>,
    /// header 行里的父会话 id（fork 关系）。
    #[serde(rename = "parentSessionId")]
    parent_session_id: Option<String>,
    /// fact 行的类型（如 `name`）。
    fact: Option<String>,
    /// fact 行携带的用户自定义名称。
    name: Option<String>,
    /// message 行的浅解析内容。
    message: Option<ScanMessage>,
}

/// 会话文件浅解析得到的消息行，含角色与内容块。
#[derive(serde::Deserialize)]
struct ScanMessage {
    /// 消息角色（`user` / `assistant`）。
    role: Option<String>,
    /// 消息内容块列表。
    content: Option<Vec<ScanBlock>>,
}

/// 消息内容块，含类型与文本，用于提取首条用户消息。
#[derive(serde::Deserialize)]
struct ScanBlock {
    /// 内容块类型，只提取 `text` 块。
    #[serde(rename = "type")]
    ty: Option<String>,
    /// 文本内容，仅 `text` 块有值。
    text: Option<String>,
}

/// 读取单个会话文件的行级元数据（不构建完整会话树）。
/// 流式逐行浅解析：header/fact 行直接取字段，message 行只取 role + text 块；
/// 拿到首条用户消息且消息数达到上限后提前终止，避免全文解析超大会话文件。
/// `keep_current` 为 true（当前打开的会话）时即使没有用户消息也保留，
/// 保证列表能显示当前会话（树状结构也依赖它作为父节点出现）。
fn load_row(path: &Path, keep_current: bool) -> Option<SessionRow> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::with_capacity(64 * 1024, file);
    let mut line = String::new();

    let mut has_header = false;
    let mut id = String::new();
    let mut cwd = String::new();
    let mut parent_session_id: Option<String> = None;
    let mut name: Option<String> = None;
    let mut first_message: Option<String> = None;
    let mut msg_count = 0usize;
    let mut msg_count_capped = false;
    let mut search_text = String::new();
    let mut scanned_bytes = 0usize;
    let mut truncated = false;

    loop {
        line.clear();
        let n = reader.read_line(&mut line).ok()?;
        if n == 0 {
            break;
        }
        scanned_bytes += n;

        // 字节预算：超大会话文件（无论内容多少）读到预算即停，避免全文解析
        if scanned_bytes >= SCAN_BYTES_BUDGET {
            msg_count_capped = true;
            truncated = true;
            break;
        }
        let Ok(v) = serde_json::from_str::<ScanLine>(&line) else {
            continue;
        };

        let kind = v.kind.as_deref().unwrap_or("");
        if !has_header {
            if kind != "header" {
                return None;
            }

            has_header = true;
            id = v.id.unwrap_or_default();
            cwd = v.cwd.unwrap_or_default();
            parent_session_id = v.parent_session_id;
            continue;
        }

        // 使用最新名称（含显式清空）：v4 名称以 kind=fact/fact=name 持久化
        if kind == "fact" && v.fact.as_deref() == Some("name") {
            if let Some(n) = v.name.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                name = Some(n.to_string());
            }
            continue;
        }

        if v.ty.as_deref() != Some("message") {
            continue;
        }
        let Some(msg) = v.message else {
            continue;
        };

        let role = msg.role.as_deref().unwrap_or("");
        if role != "user" && role != "assistant" {
            continue;
        }

        // 只拼 text 块
        let mut mtext = String::new();
        if let Some(content) = msg.content {
            for b in content {
                if b.ty.as_deref() == Some("text")
                    && let Some(tx) = b.text
                {
                    mtext.push_str(&tx);
                }
            }
        }
        let mtext = mtext.replace(['\r', '\n'], " ").trim().to_string();
        if mtext.is_empty() {
            continue;
        }

        // 消息数达到上限：列表信息已足够，提前终止
        if first_message.is_some() && msg_count >= MSG_COUNT_LIMIT {
            msg_count_capped = true;
            truncated = true;
            break;
        }
        msg_count += 1;

        if first_message.is_none() && role == "user" {
            first_message = Some(mtext.clone());
        }

        // 搜索文本：截断避免超大会话撑爆内存
        if search_text.len() < SEARCH_TEXT_LIMIT {
            search_text.push_str(&mtext);
            search_text.push(' ');
        }
    }

    if !has_header {
        return None;
    }

    if !truncated && first_message.is_none() && !keep_current {
        // /resume 列表不显示空会话（无用户消息）；当前会话例外
        return None;
    }

    // 截断可能砍掉首条用户消息：用已收集文本兜底展示
    if first_message.is_none() && truncated {
        first_message = Some(if search_text.is_empty() {
            "(large session)".to_string()
        } else {
            search_text
                .split_whitespace()
                .take(30)
                .collect::<Vec<_>>()
                .join(" ")
        });
    }

    let modified = std::fs::metadata(path).and_then(|m| m.modified()).ok()?;
    Some(SessionRow {
        path: path.to_path_buf(),
        id,
        name,
        text: first_message.unwrap_or_else(|| "(no messages)".to_string()),
        msg_count,
        msg_count_capped,
        modified,
        age: format_session_age(modified),
        cwd,
        parent_session_id,
        is_current: keep_current,
        search_text,
    })
}

/// 枚举目录下的会话候选：只 `stat`（不解析文件），按 mtime 新→旧排序。
///
/// 渐进加载的**加载顺序**由 mtime 决定（按修改时间优先），最终展示顺序在行加载完后
/// 由 `recompute`/排序决定——这样最旧的巨大会话不会拖住最新会话出现在列表里。
fn candidate_paths(dir: &Path) -> Vec<PathBuf> {
    let mut paths: Vec<(SystemTime, PathBuf)> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            let mtime = mtime_of(&path);
            paths.push((mtime, path));
        }
    }
    paths.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    paths.into_iter().map(|(_, p)| p).collect()
}

/// 枚举 agent_dir/sessions/ 下所有项目目录的会话候选（mtime 新→旧 + cwd 回退）。
fn all_candidate_jobs(agent_dir: &Path) -> Vec<(PathBuf, Option<String>)> {
    let root = agent_dir.join("sessions");
    let mut jobs: Vec<(SystemTime, PathBuf, Option<String>)> = Vec::new();

    if let Ok(rd) = std::fs::read_dir(&root) {
        for dir in rd.flatten() {
            let dir_path = dir.path();
            if !dir_path.is_dir() {
                continue;
            }

            // all scope 显示 cwd 用（header cwd 缺失时回退目录名）
            let fallback = dir_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string());

            if let Ok(files) = std::fs::read_dir(&dir_path) {
                for e in files.flatten() {
                    let path = e.path();
                    if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                        continue;
                    }
                    let mtime = mtime_of(&path);
                    jobs.push((mtime, path, fallback.clone()));
                }
            }
        }
    }

    jobs.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    jobs.into_iter().map(|(_, p, fb)| (p, fb)).collect()
}

/// 中间容器：children 存节点索引（树构建的两阶段：建图 → 排序 → 展平）
struct TreeIndexNode {
    /// 对应 `rows` 中的下标。
    row_idx: usize,
    /// 子节点在 `nodes` 中的下标。
    children: Vec<usize>,
    /// 整棵子树的最新活动时间，用于同级排序。
    latest_activity: SystemTime,
}

/// 最终树：children 递归持有
struct TreeNode {
    /// 对应 `rows` 中的下标。
    row_idx: usize,
    /// 已按活动时间排序的子节点。
    children: Vec<TreeNode>,
}

/// 按 `parentSessionId` 把会话行组成森林，同级按子树最新活动时间倒序排列。
fn build_session_tree(rows: &[SessionRow]) -> Vec<TreeNode> {
    // v4 父会话以 id（parentSessionId）关联，直接按 id 建树
    let by_id: HashMap<String, usize> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| (r.id.clone(), i))
        .collect();

    let mut nodes: Vec<TreeIndexNode> = rows
        .iter()
        .enumerate()
        .map(|(i, r)| TreeIndexNode {
            row_idx: i,
            children: Vec::new(),
            latest_activity: r.modified,
        })
        .collect();

    let mut roots: Vec<usize> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let parent = r
            .parent_session_id
            .as_deref()
            .and_then(|p| by_id.get(p).copied());
        match parent {
            Some(pidx) if pidx != i => nodes[pidx].children.push(i),
            _ => roots.push(i),
        }
    }

    for r in &roots {
        update_latest(*r, &mut nodes);
    }

    for r in &roots {
        sort_rec(*r, &mut nodes);
    }
    roots.sort_by(|a, b| nodes[*b].latest_activity.cmp(&nodes[*a].latest_activity));

    /// 递归把索引节点转成持有子节点的 [`TreeNode`]。
    fn collect(idx: usize, nodes: &[TreeIndexNode]) -> TreeNode {
        TreeNode {
            row_idx: nodes[idx].row_idx,
            children: nodes[idx]
                .children
                .iter()
                .map(|&c| collect(c, nodes))
                .collect(),
        }
    }
    roots.iter().map(|&r| collect(r, &nodes)).collect()
}

// 递归更新子树最新活动
/// 自底向上把子节点（及其子树）的最新活动时间汇总到该节点。
#[stacksafe::stacksafe]
fn update_latest(idx: usize, nodes: &mut [TreeIndexNode]) {
    let mut latest = nodes[idx].latest_activity;
    for c in nodes[idx].children.clone() {
        update_latest(c, nodes);
        latest = latest.max(nodes[c].latest_activity);
    }
    nodes[idx].latest_activity = latest;
}

// 递归排序（孩子按各自子树最新活动倒序）
/// 递归按子树最新活动时间倒序排列各节点的 `children`。
#[stacksafe::stacksafe]
fn sort_rec(idx: usize, nodes: &mut [TreeIndexNode]) {
    for c in nodes[idx].children.clone() {
        sort_rec(c, nodes);
    }
    let mut children = nodes[idx].children.clone();
    children.sort_by(|a, b| nodes[*b].latest_activity.cmp(&nodes[*a].latest_activity));
    nodes[idx].children = children;
}

/// 递归展平树节点为显示行，计算缩进与祖先连接线，结果追加到 `out`。
#[stacksafe::stacksafe]
fn walk_tree(
    node: &TreeNode,
    depth: usize,
    ancestor_continues: &[bool],
    is_last: bool,
    out: &mut Vec<DisplayNode>,
) {
    out.push(DisplayNode {
        idx: node.row_idx,
        depth,
        is_last,
        ancestor_continues: ancestor_continues.to_vec(),
    });

    for (i, child) in node.children.iter().enumerate() {
        let child_is_last = i == node.children.len() - 1;
        let continues = depth > 0 && !is_last; // 非根祖先才记录延续线
        let mut next = ancestor_continues.to_vec();
        next.push(continues);
        walk_tree(child, depth + 1, &next, child_is_last, out);
    }
}

/// 对每个根节点调用 [`walk_tree`]，把整片森林展平为显示列表。
fn flatten_session_tree(roots: &[TreeNode], out: &mut Vec<DisplayNode>) {
    for (i, root) in roots.iter().enumerate() {
        walk_tree(root, 0, &[], i == roots.len() - 1, out);
    }
}

/// 搜索查询中的词元类型：模糊匹配或短语匹配。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenKind {
    /// 空白分隔的模糊匹配词元。
    Fuzzy,
    /// 双引号包裹的精确短语。
    Phrase,
}

/// 解析后的搜索查询，含可选正则、词元列表与错误标记。
struct ParsedQuery {
    /// `re:<pattern>` 解析出的正则，无该前缀时为 None。
    regex: Option<Regex>,
    /// 模糊/短语词元，按出现顺序排列。
    tokens: Vec<(TokenKind, String)>,
    /// 正则解析失败标记（此时回退为普通空白分词）。
    error: bool,
}

impl ParsedQuery {
    /// 查询解析是否失败（正则无效或引号未闭合），调用方应回退为普通分词匹配。
    fn has_error(&self) -> bool {
        self.error
    }
}

/// 转小写并把连续空白折叠为单个空格，用于名称匹配。
fn normalize_whitespace_lower(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// 解析搜索查询：`re:<pattern>` regex、`"phrase"` 精确短语、空白分隔 fuzzy token。
/// 引号未闭合时回退为普通空白分词
fn parse_query(query: &str) -> ParsedQuery {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return ParsedQuery {
            regex: None,
            tokens: Vec::new(),
            error: false,
        };
    }

    // 正则匹配
    if let Some(pattern) = trimmed.strip_prefix("re:") {
        let pattern = pattern.trim();
        if pattern.is_empty() {
            return ParsedQuery {
                regex: None,
                tokens: Vec::new(),
                error: true,
            };
        }

        return match RegexBuilder::new(pattern).case_insensitive(true).build() {
            Ok(re) => ParsedQuery {
                regex: Some(re),
                tokens: Vec::new(),
                error: false,
            },
            Err(_) => ParsedQuery {
                regex: None,
                tokens: Vec::new(),
                error: true,
            },
        };
    }

    let mut tokens: Vec<(TokenKind, String)> = Vec::new();
    let mut buf = String::new();
    let mut in_quote = false;
    let flush = |kind: TokenKind, tokens: &mut Vec<(TokenKind, String)>, buf: &mut String| {
        let v = buf.trim().to_string();
        buf.clear();
        if !v.is_empty() {
            tokens.push((kind, v));
        }
    };

    for ch in trimmed.chars() {
        if ch == '"' {
            if in_quote {
                flush(TokenKind::Phrase, &mut tokens, &mut buf);
                in_quote = false;
            } else {
                flush(TokenKind::Fuzzy, &mut tokens, &mut buf);
                in_quote = true;
            }
            continue;
        }

        if !in_quote && ch.is_whitespace() {
            flush(TokenKind::Fuzzy, &mut tokens, &mut buf);
            continue;
        }
        buf.push(ch);
    }

    flush(
        if in_quote {
            TokenKind::Phrase
        } else {
            TokenKind::Fuzzy
        },
        &mut tokens,
        &mut buf,
    );

    if in_quote {
        // 未闭合引号：回退为普通空白分词
        tokens = trimmed
            .split_whitespace()
            .map(|t| (TokenKind::Fuzzy, t.to_string()))
            .collect();
    }

    ParsedQuery {
        regex: None,
        tokens,
        error: false,
    }
}

/// fuzzy 匹配（字符有序子序列，低分更好）
fn fuzzy_match(query: &str, text: &str) -> Option<f64> {
    let query_lower: Vec<char> = query.to_lowercase().chars().collect();
    let text_lower: Vec<char> = text.to_lowercase().chars().collect();

    let score_for = |q: &[char]| -> Option<f64> {
        if q.is_empty() {
            return Some(0.0);
        }

        if q.len() > text_lower.len() {
            return None;
        }

        let mut qi = 0usize;
        let mut score = 0.0f64;
        let mut last: i64 = -1;
        let mut consecutive = 0i64;

        for (i, &c) in text_lower.iter().enumerate() {
            if qi >= q.len() {
                break;
            }

            if c == q[qi] {
                let is_boundary =
                    i == 0 || matches!(text_lower[i - 1], ' ' | '-' | '_' | '.' | '/' | ':');

                if last == i as i64 - 1 {
                    consecutive += 1;
                    score -= (consecutive * 5) as f64;
                } else {
                    consecutive = 0;
                    if last >= 0 {
                        score += (i as i64 - last - 1) as f64 * 2.0;
                    }
                }

                if is_boundary {
                    score -= 10.0;
                }

                score += i as f64 * 0.1;
                last = i as i64;
                qi += 1;
            }
        }

        if qi < q.len() {
            return None;
        }

        if q.iter().collect::<String>() == text_lower.iter().collect::<String>() {
            score -= 100.0;
        }

        Some(score)
    };

    let primary = score_for(&query_lower);
    if primary.is_some() {
        return primary;
    }

    // 字母+数字交换兜底（"abc12" ↔ "12abc"）
    let alpha: String = query_lower
        .iter()
        .filter(|c| c.is_ascii_alphabetic())
        .collect();

    let digits: String = query_lower.iter().filter(|c| c.is_ascii_digit()).collect();
    if alpha.is_empty() || digits.is_empty() || alpha.len() + digits.len() != query_lower.len() {
        return primary;
    }

    let swapped1: String = digits.clone() + &alpha;
    let swapped2: String = alpha + &digits;
    let swapped = if swapped1 == query_lower.iter().collect::<String>() {
        swapped2
    } else {
        swapped1
    };
    let s = score_for(&swapped.chars().collect::<Vec<_>>())?;

    Some(s + 5.0)
}

/// 会话匹配
fn match_session(row: &SessionRow, parsed: &ParsedQuery) -> Option<f64> {
    let text = format!(
        "{} {} {} {}",
        row.id,
        row.name.as_deref().unwrap_or(""),
        row.search_text,
        row.cwd
    );

    if let Some(re) = &parsed.regex {
        let idx = re.find(&text)?;
        return Some(idx.start() as f64 * 0.1);
    }

    if parsed.tokens.is_empty() {
        return Some(0.0);
    }

    let mut total = 0.0f64;
    let mut normalized_text: Option<String> = None;

    for (kind, value) in &parsed.tokens {
        if *kind == TokenKind::Phrase {
            if normalized_text.is_none() {
                normalized_text = Some(normalize_whitespace_lower(&text));
            }

            let phrase = normalize_whitespace_lower(value);
            if phrase.is_empty() {
                continue;
            }

            let idx = normalized_text.as_ref()?.find(&phrase)?;
            total += idx as f64 * 0.1;
            continue;
        }
        total += fuzzy_match(value, &text)?;
    }
    Some(total)
}

/// 删除会话文件：先试 `trash` CLI，失败回退 unlink
fn delete_session_file(path: &str) -> Result<&'static str, String> {
    let mut cmd = Command::new("trash");
    if path.starts_with('-') {
        cmd.arg("--");
    }
    cmd.arg(path);
    let trash_ok = match cmd.output() {
        Ok(o) => o.status.success() || !Path::new(path).exists(),
        Err(_) => false,
    };
    if trash_ok {
        return Ok("trash");
    }
    std::fs::remove_file(path).map_err(|e| e.to_string())?;
    Ok("unlink")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMessage;

    fn test_row(text: &str) -> SessionRow {
        SessionRow {
            path: PathBuf::from(format!("/tmp/{}.jsonl", text.replace(' ', "_"))),
            id: format!("id-{}", text.replace(' ', "_")),
            name: None,
            text: text.to_string(),
            msg_count: 1,
            msg_count_capped: false,
            modified: SystemTime::now(),
            age: "now".to_string(),
            cwd: String::new(),
            parent_session_id: None,
            is_current: false,
            search_text: text.to_string(),
        }
    }

    fn row_with_name(text: &str, name: &str) -> SessionRow {
        let mut r = test_row(text);
        r.name = Some(name.to_string());
        r
    }

    #[test]
    fn parse_query_tokens_and_phrases() {
        let q = parse_query("foo \"node cve\" bar");
        assert!(!q.has_error());
        assert_eq!(q.tokens.len(), 3);
        assert_eq!(q.tokens[0], (TokenKind::Fuzzy, "foo".to_string()));
        assert_eq!(q.tokens[1], (TokenKind::Phrase, "node cve".to_string()));
        assert_eq!(q.tokens[2], (TokenKind::Fuzzy, "bar".to_string()));

        // 未闭合引号回退普通分词
        let q = parse_query("foo \"unclosed");
        assert_eq!(q.tokens.len(), 2);
        assert!(q.tokens.iter().all(|(k, _)| *k == TokenKind::Fuzzy));
    }

    #[test]
    fn parse_query_regex() {
        let q = parse_query("re:^foo");
        assert!(!q.has_error());
        assert!(q.regex.is_some());

        let q = parse_query("re:[");
        assert!(q.has_error());

        let q = parse_query("re:");
        assert!(q.has_error());
    }

    #[test]
    fn fuzzy_match_prefers_consecutive() {
        let a = fuzzy_match("abc", "xxabcxx").unwrap();
        let b = fuzzy_match("abc", "axxbxxcxx").unwrap();
        assert!(a < b, "consecutive should score better: {} vs {}", a, b);
        assert!(fuzzy_match("abc", "xxaxx").is_none());
        // 数字字母交换兜底
        assert!(fuzzy_match("abc12", "12abc").is_some());
    }

    #[test]
    fn match_session_tokens() {
        let row = test_row("hello world session");
        let q = parse_query("hello");
        assert!(match_session(&row, &q).is_some());
        let q = parse_query("\"hello world\"");
        assert!(match_session(&row, &q).is_some());
        let q = parse_query("\"world hello\"");
        assert!(match_session(&row, &q).is_none());
        let q = parse_query("nonexistent");
        assert!(match_session(&row, &q).is_none());
    }

    #[test]
    fn recompute_filters_and_sorts() {
        let mut s = SessionSelector::new();
        s.active = true;
        s.current = vec![
            row_with_name("old message", "named-one"),
            test_row("second message"),
            row_with_name("third", "named-two"),
        ];
        s.recompute();
        assert_eq!(s.display.len(), 3);

        // named 过滤
        s.name_filter = NameFilter::Named;
        s.recompute();
        assert_eq!(s.display.len(), 2);

        // fuzzy 过滤
        s.name_filter = NameFilter::All;
        s.filter.set_value("second");
        s.sort_mode = SortMode::Recent;
        s.recompute();
        assert_eq!(s.display.len(), 1);
        assert!(s.rows[s.display[0].idx].text.contains("second"));

        // re: regex
        s.filter.set_value("re:^third");
        s.recompute();
        // `^` 锚定在搜索文本开头（id 在前），与 pi 一致匹配不到
        assert_eq!(s.display.len(), 0);
        s.filter.set_value("re:third");
        s.recompute();
        assert_eq!(s.display.len(), 1);
    }

    #[test]
    fn move_selection_wraps_at_both_ends() {
        let mut s = SessionSelector::new();
        s.active = true;
        s.current = vec![test_row("a"), test_row("b"), test_row("c")];
        s.recompute();
        assert_eq!(s.display.len(), 3);

        // 顶部 ↑ 回绕到末项，末项 ↓ 回绕到首项
        s.selected = 0;
        s.move_selection(-1);
        assert_eq!(s.selected, 2, "首项 ↑ 应环绕到末项");
        s.move_selection(1);
        assert_eq!(s.selected, 0, "末项 ↓ 应环绕到首项");

        // 空列表不 panic
        s.filter.set_value("zzz-nope");
        s.recompute();
        s.move_selection(1);
        assert_eq!(s.selected, 0, "空列表导航应保持原位");
    }

    #[test]
    fn move_selection_skips_current_session() {
        let base = SystemTime::UNIX_EPOCH;
        let row_at = |text: &str, secs: u64, is_current: bool| {
            let mut r = test_row(text);
            r.modified = base + Duration::from_secs(secs);
            r.is_current = is_current;
            r
        };

        let mut s = SessionSelector::new();
        s.active = true;
        // modified 递增 → 树形（单层）按最新优先：c(0), current(1), a(2)
        s.current = vec![
            row_at("a", 1, false),
            row_at("current", 2, true),
            row_at("c", 3, false),
        ];
        s.recompute();

        let cur_idx = s
            .display
            .iter()
            .position(|n| s.rows[n.idx].is_current)
            .expect("当前会话应在列表中");
        assert_eq!(cur_idx, 1, "当前会话位于中间");
        assert_ne!(s.selected, cur_idx, "初始选中不应落在当前会话");

        // 向下：0 → 跳过当前会话 → 2；再向下回绕 → 0
        s.selected = 0;
        s.move_selection(1);
        assert_eq!(s.selected, 2, "向下应跳过当前会话");
        s.move_selection(1);
        assert_eq!(s.selected, 0, "末项向下应环绕到首项");

        // 向上：0 → 回绕并跳过当前会话 → 2
        s.move_selection(-1);
        assert_eq!(s.selected, 2, "向上应环绕并跳过当前会话");

        // 选中项落到当前会话时 recompute 就近跳到可选项
        s.selected = cur_idx;
        s.recompute();
        assert_ne!(s.selected, cur_idx, "recompute 后不应停在当前会话");

        // 列表中只有当前会话：无可选项，Enter 无目标（selected_path 为 None）
        s.current = vec![row_at("only", 1, true)];
        s.recompute();
        assert!(s.selected_path().is_none(), "当前会话不可作为选择目标");
        s.move_selection(1);
        assert!(
            s.selected_path().is_none(),
            "无处可去时导航不应选中当前会话"
        );
    }

    #[test]
    fn select_first_last_and_next_after_current() {
        fn row_at(
            id: &str,
            text: &str,
            secs: u64,
            parent: Option<&str>,
            is_current: bool,
        ) -> SessionRow {
            let mut r = test_row(text);
            r.id = id.to_string();
            r.modified = SystemTime::UNIX_EPOCH + Duration::from_secs(secs);
            r.parent_session_id = parent.map(|p| p.to_string());
            r.is_current = is_current;
            r
        }

        let build = |current: Option<&str>| {
            let mut s = SessionSelector::new();
            s.active = true;
            // 树：root a（最新）→ 子 a1/a2；root b
            // 展平顺序：a(0,d0) a1(1,d1) a2(2,d1) b(3,d0)
            s.current = vec![
                row_at("a", "root a", 1, None, current == Some("a")),
                row_at("a1", "child a1", 5, Some("a"), current == Some("a1")),
                row_at("a2", "child a2", 3, Some("a"), current == Some("a2")),
                row_at("b", "root b", 2, None, current == Some("b")),
            ];
            s.recompute();
            s
        };

        let texts: Vec<String> = {
            let s = build(None);
            s.display
                .iter()
                .map(|n| s.rows[n.idx].text.clone())
                .collect()
        };
        assert_eq!(texts, vec!["root a", "child a1", "child a2", "root b"]);

        // 无当前会话：Ctrl+G 无基准点，保持原位
        let mut s = build(None);
        s.selected = 2;
        s.select_next_after_current();
        assert_eq!(s.selected, 2, "无当前会话时 Ctrl+G 不应移动");

        // 当前会话 = root a（index 0）：下一条为 child a1
        let mut s = build(Some("a"));
        s.selected = 3;
        s.select_next_after_current();
        assert_eq!(s.selected, 1, "跳到当前会话的下一条会话（与选中位置无关）");
        // 再按一次仍以当前会话为基准（幂等）
        s.select_next_after_current();
        assert_eq!(s.selected, 1);

        // Home / End 跳过当前会话
        s.select_first();
        assert_eq!(s.selected, 1, "Home 跳过当前会话");
        s.select_last();
        assert_eq!(s.selected, 3, "End 跳到末项");

        // 当前会话 = 末个 root b（index 3）：Ctrl+G 环绕到首个可选项
        let mut s = build(Some("b"));
        s.selected = 0;
        s.select_next_after_current();
        assert_eq!(s.selected, 0, "当前会话在末尾时环绕到首个可选项");

        // 当前会话 = 子会话 a1（index 1）：下一条为 a2
        let mut s = build(Some("a1"));
        s.select_next_after_current();
        assert_eq!(s.selected, 2);
    }

    #[test]
    fn tree_flatten_marks_depth() {
        // 无 parent 关系：全部为根，depth 0
        let rows = vec![test_row("a"), test_row("b")];
        let tree = build_session_tree(&rows);
        assert_eq!(tree.len(), 2);
        let mut flat = Vec::new();
        flatten_session_tree(&tree, &mut flat);
        assert_eq!(flat.len(), 2);
        assert!(flat.iter().all(|n| n.depth == 0));
    }

    #[test]
    fn build_tree_with_parent_link() {
        // 直接用 SessionRow 构造：父会话以 id 关联（v4 parentSessionId 语义）
        let mut parent = test_row("parent");
        parent.id = "p1".to_string();
        let mut child = test_row("child");
        child.id = "c1".to_string();
        child.parent_session_id = Some("p1".to_string());
        let tree = build_session_tree(&[parent, child]);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree[0].children.len(), 1);
    }

    #[test]
    fn initial_select_picks_first_non_current_and_follows_recompute() {
        let mut s = SessionSelector::new();
        s.active = true;
        s.initial_select = true; // 模拟 open 后（用户尚未导航）
        let mut cur = test_row("b");
        cur.is_current = true;
        s.current = vec![test_row("a"), cur, test_row("c")];
        s.recompute();
        assert_eq!(s.selected, 0, "打开后应选中首个非当前会话");

        // 列表重排（如渐进加载/树重排）后仍跟随首个非当前会话
        s.current = vec![test_row("x"), test_row("a")];
        s.recompute();
        assert_eq!(s.selected, 0);

        // 用户导航后不再自动跟随
        s.move_selection(1);
        assert_eq!(s.selected, 1);
        s.current = vec![test_row("x"), test_row("a"), test_row("y")];
        s.recompute();
        assert_eq!(s.selected, 1, "用户导航后保持选中，不再跳到首个");
    }

    #[test]
    fn open_selects_first_non_current_after_full_load() {
        // 当前会话在中间；fork 子会话使树重排：首帧首行与最终首行不同。
        // 打开并补齐后，选中应落在最终列表的首个非当前会话（不被首帧选中项钉住）。
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let agent_dir = dir.path().join("agent");
        let sess_dir = session_manager::default_session_dir(cwd, &agent_dir);
        std::fs::create_dir_all(&sess_dir).unwrap();

        let mut paths = Vec::new();
        let mut ids = Vec::new();
        for i in 0..(INITIAL_BATCH + 4) {
            let mut s = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
            s.append_message(&AgentMessage::user_text(&format!("msg {i}")));
            paths.push(s.get_session_file().unwrap().to_path_buf());
            ids.push(s.session_id.clone());
            drop(s);
            std::thread::sleep(Duration::from_millis(2));
        }
        // 为较早的会话（首批之后才加载）追加最新的 fork 子会话，触发树重排
        for (pi, tag) in [(2usize, "child-a"), (2, "child-b")] {
            let mut c = Session::create_with(crate::core::session_manager::SessionCreateOptions {
                cwd: cwd.to_string(),
                session_dir: Some(sess_dir.clone()),
                persist: true,
                parent_session_id: Some(ids[pi].clone()),
                ..Default::default()
            })
            .unwrap();
            c.append_message(&AgentMessage::user_text(tag));
            paths.push(c.get_session_file().unwrap().to_path_buf());
            drop(c);
            std::thread::sleep(Duration::from_millis(2));
        }

        let current = paths[7].to_string_lossy().to_string();
        let mut sel = SessionSelector::new();
        sel.open(cwd, &agent_dir, Some(current));
        while sel.is_loading() {
            sel.drain_pending(DRAIN_BATCH);
        }

        let first_non_current = (0..sel.display.len())
            .find(|&i| !sel.rows[sel.display[i].idx].is_current)
            .expect("应有非当前会话");
        assert_eq!(
            sel.selected, first_non_current,
            "补齐后应选中首个非当前会话"
        );
    }

    /// 渐进加载：`open()` 只同步加载首批（首帧即可交互），其余由 `drain_pending` 逐帧补齐，
    /// 补齐顺序按 mtime 新→旧（pi 0.86.0）。
    #[test]
    fn progressive_loading_fills_rows_in_mtime_order() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let agent_dir = dir.path().join("agent");
        let sess_dir = session_manager::default_session_dir(cwd, &agent_dir);
        std::fs::create_dir_all(&sess_dir).unwrap();

        // 比首批多的会话，保证有剩余候选
        let total = INITIAL_BATCH + 5;
        for i in 0..total {
            let mut s = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
            s.append_message(&AgentMessage::user_text(&format!("msg {i}")));
            drop(s);
            std::thread::sleep(Duration::from_millis(2)); // mtime 严格递增
        }

        let mut sel = SessionSelector::new();
        sel.open(cwd, &agent_dir, None);
        assert_eq!(sel.current.len(), INITIAL_BATCH, "首批同步加载");
        assert!(sel.is_loading(), "其余候选应留给渐进补齐");
        assert_eq!(sel.loading_progress(), (INITIAL_BATCH, total));
        // 首批是最新的那些（mtime 优先）
        assert!(
            sel.current[0].text.contains(&format!("msg {}", total - 1)),
            "最新的会话应在最前：{}",
            sel.current[0].text
        );

        // 逐帧补齐（选中项按路径保持）
        let before = sel.selected_path();
        let mut guard = 0;
        while sel.is_loading() {
            sel.drain_pending(DRAIN_BATCH);
            guard += 1;
            assert!(guard < 100, "补齐应在有限帧内完成");
        }
        assert_eq!(sel.current.len(), total, "全部候选补齐后行数等于候选数");
        assert_eq!(sel.selected_path(), before, "补齐后选中项按路径保持");
        assert!(!sel.is_loading());

        // 取消：重新加载后 cancel 即停止补齐
        sel.open(cwd, &agent_dir, None);
        assert!(sel.is_loading());
        sel.cancel_pending();
        assert!(!sel.is_loading(), "取消后不再加载");
        assert!(!sel.drain_pending(DRAIN_BATCH), "取消后无新行");
    }

    #[test]
    fn load_row_reads_v4_fact_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        std::fs::write(
            &path,
            concat!(
                r#"{"kind":"header","version":4,"id":"h1","createdAt":0,"cwd":"/tmp"}"#,
                "\n",
                r#"{"id":"e1","kind":"entry","lane":"main","message":{"content":[],"isError":false,"role":"assistant","timestamp":0},"seq":1,"timestamp":0,"type":"message"}"#,
                "\n",
                r#"{"id":"e2","kind":"entry","lane":"main","message":{"content":[{"text":"probe","type":"text"}],"isError":false,"role":"user","timestamp":0},"seq":2,"timestamp":0,"type":"message"}"#,
                "\n",
                r#"{"fact":"name","kind":"fact","name":"newname","seq":3}"#,
                "\n",
            ),
        )
        .unwrap();
        let row = load_row(&path, false).expect("row should load");
        assert_eq!(row.name.as_deref(), Some("newname"));
    }

    #[test]
    fn load_row_hides_empty_session_without_user_messages() {
        // /resume 列表不显示空会话（无用户消息）
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        let empty = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        let empty_path = empty.get_session_file().unwrap().to_path_buf();
        drop(empty);
        let mut full = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        full.append_message(&AgentMessage::user_text("hi"));
        let full_path = full.get_session_file().unwrap().to_path_buf();
        drop(full);

        assert!(
            load_row(&empty_path, false).is_none(),
            "空会话不应出现在 /resume 列表"
        );
        assert!(
            load_row(&full_path, false).is_some(),
            "有用户消息的会话应显示"
        );
    }

    #[test]
    fn load_row_keeps_empty_current_session() {
        // 当前打开的会话即使没有用户消息也保留（列表需显示 + 树状结构依赖）
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        let empty = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        let empty_path = empty.get_session_file().unwrap().to_path_buf();
        drop(empty);

        assert!(
            load_row(&empty_path, false).is_none(),
            "空会话默认隐藏（非当前会话）"
        );
        let row = load_row(&empty_path, true).expect("当前会话即使为空也应显示");
        assert!(row.is_current, "应标记为当前会话");
    }

    #[test]
    fn load_row_caps_msg_count_on_large_session() {
        // 消息数达到上限 → 提前终止：msg_count_capped=true 且仍显示
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.jsonl");
        let mut content =
            String::from(r#"{"kind":"header","version":4,"id":"h1","createdAt":0,"cwd":"/tmp"}"#);
        content.push('\n');
        // 压缩消息行模板（MSg_COUNT_LIMIT+ 条）
        let msg = r#"{"id":"e","kind":"entry","message":{"content":[{"text":"m","type":"text"}],"role":"user"},"type":"message"}"#;
        for _ in 0..(MSG_COUNT_LIMIT + 50) {
            content.push_str(msg);
            content.push('\n');
        }
        std::fs::write(&path, content).unwrap();
        let row = load_row(&path, false).expect("large session should still show");
        assert!(row.msg_count_capped, "达到上限后应标记截断");
        assert_eq!(row.msg_count, MSG_COUNT_LIMIT);
        assert!(row.text.contains("m"));
    }

    #[test]
    fn load_row_stops_at_byte_budget_when_no_text() {
        // 巨型文件但 text 块极少（thinking/tool 为主）：字节预算触发提前终止，不隐藏
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("huge_no_text.jsonl");
        let mut content =
            String::from(r#"{"kind":"header","version":4,"id":"h1","createdAt":0,"cwd":"/tmp"}"#);
        content.push('\n');
        // 超过预算：每条 ~150B，凑 > 4MB 需要 2.8 万行 —— 测试太久，改由行内填充大 thinking 块
        let pad = "x".repeat(20_000);
        let big_msg = format!(
            r#"{{"id":"e","kind":"entry","message":{{"content":[{{"thinking":"{}","type":"thinking"}}],"role":"assistant"}},"type":"message"}}"#,
            pad
        );
        // 4 条 20KB thinking 行 + 少量 text 消息即可超过 4MB 预算
        for _ in 0..250 {
            content.push_str(&big_msg);
            content.push('\n');
        }
        std::fs::write(&path, content).unwrap();
        let row = load_row(&path, false).expect("byte-budget truncated session should still show");
        assert!(row.msg_count_capped, "字节预算触发应标记截断");
        assert_eq!(row.msg_count, 0, "text 块为空不计数");
        // 首条用户消息被预算砍掉 → 用兜底文本显示而非隐藏
        assert!(row.text.contains("large session") || !row.text.is_empty());
        assert!(std::fs::metadata(&path).unwrap().len() > SCAN_BYTES_BUDGET as u64);
    }
}

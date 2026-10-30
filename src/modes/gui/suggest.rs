//! 输入框补全提示：`/` 命令、`@` 文件路径。
//!
//! 只做「识别触发词 + 给候选」，插入/执行由视图层负责（见 [`super::app`]）。
//! 匹配规则与 TUI 一致：`/` 按字符顺序模糊匹配（[`crate::utils::fuzzy`]），
//! `@` 按路径打分（[`crate::utils::fuzzy::path_score`]），都不是简单前缀。

use crate::utils::{
    fuzzy::{fuzzy_filter, path_score},
    mime::is_image_path,
};
use std::path::Path;

/// 候选面板最多显示几条。
pub const MAX_ITEMS: usize = 40;

/// 有独立 GUI 面板入口的扩展命令：`/` 面板与 Ctrl-M 命令面板里都列（同一个动作两个入口）。
pub const PANEL_COMMANDS: &[&str] = &["skills", "extension", "settings"];

/// GUI 支持的斜杠命令：名字（不含 `/`）+ 说明。
/// 与 TUI 的命令集对齐（`dock` 没有桌面等价物，除了它全部列出）。
pub const COMMANDS: &[(&str, &str)] = &[
    ("new", "New session"),
    ("resume", "Switch session"),
    ("model", "Select model"),
    ("thinking", "Set thinking level"),
    ("compact", "Compact context"),
    ("copy", "Copy last reply"),
    ("rewind", "Rewind last input"),
    ("session", "Show current session info"),
    ("name", "Rename current session"),
    ("clone", "Clone current branch"),
    ("fork", "Fork current session"),
    ("label", "Label current branch"),
    ("tree", "Session tree"),
    ("import", "Import session file"),
    ("export", "Export session (HTML)"),
    ("share", "Share session (gist)"),
    ("history", "Search prompt history"),
    ("giff", "Review uncommitted changes"),
    ("trust", "Trust the current project"),
    ("reload", "Reload skills, extensions and context"),
    ("theme", "Show/switch theme"),
    ("settings", "Open settings"),
    ("extension", "Enable/disable extensions"),
    ("skills", "Enable/disable skills"),
    ("scoped-models", "Models for Ctrl-P cycling"),
    ("login", "Store an API key for a provider"),
    ("logout", "Remove stored API key for a provider"),
    ("changelog", "Show changelog entries"),
    ("hotkeys", "Show keybindings"),
    ("bug", "Report a bug (collects diagnostics, exports a zip)"),
    ("usage", "Usage & cost"),
    ("debug", "Debug info"),
    ("dock", "Toggle the dock panel"),
    ("sidebar", "Toggle session list"),
    ("quit", "Quit"),
];

/// 触发词类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trigger {
    /// `/` 开头的 GUI 命令
    Command,
    /// `@` 开头的文件路径
    File,
    /// `/<命令> ` 之后的子命令（扩展命令声明的二级候选）
    Subcommand,
}

impl Trigger {
    /// 采纳候选后是否立即提交：命令与子命令补全即执行，`@` 文件只补全。
    pub fn submits_after_apply(self) -> bool {
        matches!(self, Trigger::Command | Trigger::Subcommand)
    }
}

/// 一条候选。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// 插入/展示的主文本（命令名或路径）
    pub label: String,
    /// 右侧灰色说明
    pub detail: String,
}

/// 当前补全状态。
#[derive(Clone, Debug)]
pub struct Suggest {
    /// 触发词类型
    pub trigger: Trigger,
    /// 替换区间起点（全文字节）：命令/文件是触发符位置，子命令是**整行命令的 `/`**
    /// （补全时整段重写，重复采纳也不会把命令名和子命令粘在一起）
    pub start: usize,
    /// 子命令面板所属的命令名（不含 `/`）；其它触发词为空串
    pub command: String,
    /// 触发符之后已输入的串（过滤用）
    pub query: String,
    /// 候选列表
    pub items: Vec<Item>,
    /// 当前选中项下标
    pub selected: usize,
}

/// 光标前最后一个 token 的起点。
fn token_start(text: &str) -> usize {
    text.rfind(['\n', ' ']).map_or(0, |index| index + 1)
}

/// 识别光标前的触发词，返回类型、起点（全文字节）与已输入前缀。
///
/// `/` 只在行首生效（与命令行习惯一致），`@` 可以在行内任意位置。
/// `!` / `!!` 是 shell 命令，没有候选，不触发面板。
pub fn detect(text_before_cursor: &str) -> Option<(Trigger, usize, String)> {
    let start = token_start(text_before_cursor);
    let token = &text_before_cursor[start..];
    let at_line_start = start == 0 || text_before_cursor[..start].ends_with('\n');

    if at_line_start && let Some(rest) = token.strip_prefix('/') {
        return Some((Trigger::Command, start, rest.to_string()));
    }

    if let Some(rest) = token.strip_prefix('@') {
        return Some((Trigger::File, start, rest.to_string()));
    }
    None
}

/// 命令候选：内置命令 + `extra`（技能、扩展命令）一起做字符顺序匹配并按匹配质量排序。
///
/// `extra` 是 `(名字, 说明)`，名字不含前导 `/`。技能名以 `skill:` 开头时按裸名参与匹配
/// （`/idea` 不得因 `skill:` 里的字符误命中），显式查 `skill:` 前缀时仍按全名匹配。
fn command_candidates(extra: &[(String, String)], query: &str) -> Vec<Item> {
    let mut all: Vec<(&str, &str)> = COMMANDS.to_vec();
    all.extend(
        extra
            .iter()
            .map(|(name, detail)| (name.as_str(), detail.as_str())),
    );
    let explicit_skill = query.trim().to_lowercase().starts_with("skill:");
    fuzzy_filter(&all, query, move |(name, _): &(&str, &str)| {
        if explicit_skill {
            name
        } else {
            name.strip_prefix("skill:").unwrap_or(name)
        }
    })
    .into_iter()
    .take(MAX_ITEMS)
    .map(|(name, detail)| Item {
        label: format!("/{name}"),
        detail: (*detail).to_string(),
    })
    .collect()
}

/// 命令候选（内置 + 技能 + 扩展命令），排序与查询不分来源。
pub fn commands_with(extra: &[(String, String)], query: &str) -> Vec<Item> {
    command_candidates(extra, query)
}

/// 识别 `/命令 <子命令查询>`，返回（整行命令起点、命令名、查询词）。
///
/// 规则同 TUI：命令名后必须有空白；查询只取第一个词，出现第二个空白
/// （`/goal pause ` 已在写自由参数）就返回 `None`。
/// 起点取行首 `/` 而不是命令名后的空白：采纳候选时整段 `/命令 查询词` 一起重写，
/// 因此命令名与子命令永远不会被拼接成 `/agentssettings`。
pub fn detect_subcommand(text_before_cursor: &str) -> Option<(usize, String, String)> {
    let line_start = text_before_cursor.rfind('\n').map_or(0, |index| index + 1);
    let rest = text_before_cursor[line_start..].strip_prefix('/')?;
    let (name, after) = rest.split_once(char::is_whitespace)?;
    if name.is_empty() {
        return None;
    }

    let query = after.trim_start();
    if query.contains(char::is_whitespace) {
        return None;
    }

    Some((line_start, name.to_string(), query.to_string()))
}

/// 子命令候选：`defs` 是命令声明的 `(名字, 说明)`，按声明顺序做模糊匹配。
///
/// 候选只写子命令名（插入位置在命令名之后），说明进右侧灰字。
pub fn subcommands(defs: &[(String, String)], query: &str) -> Vec<Item> {
    fuzzy_filter(
        defs,
        &query.to_lowercase(),
        |(name, _): &(String, String)| name.as_str(),
    )
    .into_iter()
    .take(MAX_ITEMS)
    .map(|(name, detail)| Item {
        label: name.clone(),
        detail: detail.clone(),
    })
    .collect()
}

/// 文件候选：路径打分（文件名完全>开头>子串，其次仅路径子串），同分按层级浅、路径短、字典序。
///
/// 查询是子串而非字符顺序匹配（`@sgt` 不命中 `suggest.rs`）。
/// `images_only` 为真时只列图片（附件按钮选「图片」后）。
pub fn files(paths: &[String], query: &str, images_only: bool) -> Vec<Item> {
    let query = query.trim();
    let mut scored: Vec<(i32, &String)> = paths
        .iter()
        .filter(|path| !images_only || is_image_path(Path::new(path)))
        .map(|path| {
            let score = if query.is_empty() {
                1
            } else {
                path_score(path, query, false)
            };
            (score, path)
        })
        .filter(|(score, _)| *score > 0)
        .collect();
    scored.sort_by(|(a_score, a_path), (b_score, b_path)| {
        b_score
            .cmp(a_score)
            .then_with(|| {
                a_path
                    .matches('/')
                    .count()
                    .cmp(&b_path.matches('/').count())
            })
            .then_with(|| a_path.len().cmp(&b_path.len()))
            .then_with(|| a_path.cmp(b_path))
    });
    scored
        .into_iter()
        .take(MAX_ITEMS)
        .map(|(_, path)| Item {
            label: format!("@{path}"),
            detail: String::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行首的 `/` 才算触发；行中的 `/` 与 shell 的 `!` `!!` 都不触发。
    #[test]
    fn detects_line_start_triggers() {
        assert_eq!(detect("/mo"), Some((Trigger::Command, 0, "mo".to_string())));
        assert_eq!(detect("看下 /usr 里的东西"), None);
        assert_eq!(detect("!ls"), None);
        assert_eq!(detect("!!ls"), None);
        assert_eq!(detect("echo !x"), None);
    }

    /// `@` 行内也触发，起点按 token 算。
    #[test]
    fn detects_at_anywhere_in_line() {
        assert_eq!(
            detect("改一下 @src/ma"),
            // 「改一下 」是 3×3 字节 + 空格 = 10
            Some((Trigger::File, 10, "src/ma".to_string()))
        );
        assert_eq!(
            detect("第一行\n@lib"),
            // 「第一行」9 字节 + 换行
            Some((Trigger::File, 10, "lib".to_string()))
        );
    }

    /// 子命令只在行首 `/命令 ` 后第一个词内生效：锚点固定在命令名后的第一个空白（多敲的空格会被归一化）。
    #[test]
    fn detects_subcommand_after_command_name() {
        // 起点是整行命令的 `/`：采纳候选时整段 `/命令 查询词` 一起重写
        assert_eq!(
            detect_subcommand("/goal p"),
            Some((0, "goal".to_string(), "p".to_string()))
        );
        // 空查询（刚敲完空格）也弹面板；多敲的空格不影响起点
        assert_eq!(
            detect_subcommand("/goal "),
            Some((0, "goal".into(), String::new()))
        );
        assert_eq!(
            detect_subcommand("/goal  p"),
            Some((0, "goal".to_string(), "p".to_string()))
        );
        // 第二行上的命令：起点跟着行走（「第一行\n」是 3×3 字节 + 换行）
        assert_eq!(
            detect_subcommand("第一行\n/agents se"),
            Some((10, "agents".to_string(), "se".to_string()))
        );
        // 第二个词已开始（在写自由参数）/ 行中 / 不是斜杠命令：不提示
        assert_eq!(detect_subcommand("/goal pause "), None);
        assert_eq!(detect_subcommand("看下 /goal p"), None);
        assert_eq!(detect_subcommand("@goal p"), None);
    }

    /// 子命令补全只重写起点之后的命令前缀：重复采纳不会把命令名和子命令粘在一起。
    #[test]
    fn subcommand_apply_is_idempotent() {
        let mut editor = crate::modes::gui::text_edit::Editor::new();
        for text in ["/agents ", "/agents se", "/agentssettings "] {
            editor = crate::modes::gui::text_edit::Editor::new();
            editor.insert(text);
            let (start, command, _) = detect_subcommand(&editor.text_before_cursor()).unwrap_or((
                0,
                "agents".to_string(),
                String::new(),
            ));
            editor.replace_byte_range(start..editor.cursor_byte(), "/agents settings ");
            assert_eq!(editor.text(), "/agents settings ", "起点: {start}");
            let _ = command;
        }
    }

    /// 子命令候选按声明顺序模糊匹配，主文本只有子命令名（插入位置在命令名之后）。
    #[test]
    fn subcommand_candidates_keep_declared_order() {
        let defs = vec![
            ("pause".to_string(), "Pause the goal".to_string()),
            ("resume".to_string(), "Resume it".to_string()),
        ];
        let all = subcommands(&defs, "");
        assert_eq!(all[0].label, "pause");
        assert_eq!(all[0].detail, "Pause the goal");
        assert_eq!(all.len(), 2);
        assert_eq!(subcommands(&defs, "res")[0].label, "resume");
        assert!(subcommands(&defs, "zzz").is_empty());
    }

    /// 命令与子命令采纳后立即提交，`@` 文件只补全。
    #[test]
    fn only_command_like_triggers_submit() {
        assert!(Trigger::Command.submits_after_apply());
        assert!(Trigger::Subcommand.submits_after_apply());
        assert!(!Trigger::File.submits_after_apply());
    }

    /// 命令候选：字符顺序命中即可，按匹配质量排序，带 `/` 前缀展示。
    #[test]
    fn commands_match_in_order_by_quality() {
        let all = commands_with(&[], "");
        assert_eq!(all.len(), COMMANDS.len().min(MAX_ITEMS));
        assert_eq!(all[0].label, "/new");
        // 模糊命中：`mdl` 的项目符号顺序命中 `/model`
        assert_eq!(commands_with(&[], "mdl")[0].label, "/model");
        // 同分/近分时词边界命中的排前面
        let re: Vec<String> = commands_with(&[], "re")
            .into_iter()
            .map(|item| item.label)
            .collect();
        assert_eq!(re[0], "/resume");
        assert!(re.contains(&"/tree".to_string()));
        // 乱序不算命中
        assert!(commands_with(&[], "zzz").is_empty());
        assert!(commands_with(&[], "dle").is_empty());
        // 配置类命令也在 `/` 面板里（TUI 对齐）；Ctrl-M 面板只额外保留三个独立面板入口
        assert_eq!(commands_with(&[], "settings")[0].label, "/settings");
        assert!(COMMANDS.iter().any(|(name, _)| *name == "trust"));
    }

    /// 技能候选按裸名参与模糊匹配，不因 `skill:` 前缀误命中。
    #[test]
    fn skills_match_by_bare_name() {
        let extra = vec![
            (
                "skill:deep-research".to_string(),
                "Multi-agent deep research".to_string(),
            ),
            ("skill:research-idea".to_string(), "Refine idea".to_string()),
        ];
        let labels: Vec<String> = commands_with(&extra, "idea")
            .into_iter()
            .map(|item| item.label)
            .collect();
        assert!(labels.contains(&"/skill:research-idea".to_string()));
        assert!(!labels.contains(&"/skill:deep-research".to_string()));
        // 显式写 skill: 时按完整名称匹配
        let explicit: Vec<String> = commands_with(&extra, "skill:deep")
            .into_iter()
            .map(|item| item.label)
            .collect();
        assert_eq!(explicit, vec!["/skill:deep-research"]);
    }

    /// 触发词识别：`/` 只在行首，`@` 行内任意位置。
    #[test]
    fn detect_requires_line_start_for_commands() {
        assert!(detect("abc /new").is_none());
        // 参数段不触发命令面板（`/session 2` 的 token 是 `2`）
        assert!(detect("/session 2").is_none());
        assert_eq!(detect("/ne").map(|d| d.0), Some(Trigger::Command));
        assert!(detect("第一行\n!/ls").is_none());
        assert_eq!(detect("第一行\n/x").map(|d| d.0), Some(Trigger::Command));
    }

    /// 文件候选：文件名命中先于仅路径命中，同分按层级浅、路径短。
    #[test]
    fn files_prefer_name_hits() {
        let paths = vec![
            "src/modes/gui/suggest.rs".to_string(),
            "docs/suggest.md".to_string(),
            "src/app.rs".to_string(),
        ];
        // 两者文件名都是开头命中（80）：路径更短、层级更浅的 docs 在前
        let items = files(&paths, "suggest", false);
        assert_eq!(items[0].label, "@docs/suggest.md");
        assert!(items.iter().all(|item| item.label.starts_with('@')));
        // 文件名子串命中（50）排在仅路径命中（30）前
        let mixed = vec!["a/app.rs".to_string(), "app/x.rs".to_string()];
        let got: Vec<String> = files(&mixed, "app", false)
            .into_iter()
            .map(|item| item.label)
            .collect();
        assert_eq!(got, vec!["@a/app.rs", "@app/x.rs"]);
        // 子串语义：乱序字符不命中（与 `/` 的模糊匹配不同）
        assert!(files(&paths, "sgt", false).is_empty());
    }

    /// 附件选「图片」时只列图片。
    #[test]
    fn files_filter_images_only() {
        let mixed = vec!["a/b.png".to_string(), "a/c.rs".to_string()];
        let images = files(&mixed, "", true);
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].label, "@a/b.png");
    }
}

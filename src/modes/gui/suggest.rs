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
pub const MAX_ITEMS: usize = 30;

/// 有独立 GUI 面板入口的扩展命令：`/` 面板不再列它们，避免同一个动作两个入口
/// （技能面板 Ctrl-I，扩展/设置/循环名单在 Ctrl-M 命令面板里）。
pub const PANEL_COMMANDS: &[&str] = &["skills", "extension", "settings", "scoped-models"];

/// GUI 支持的斜杠命令：名字（不含 `/`）+ 说明。
/// `/` 面板里列出的命令：只放对话相关动作，配置类留给 Ctrl-M 命令面板。
pub const COMMANDS: &[(&str, &str)] = &[
    ("new", "New session"),
    ("resume", "Switch session"),
    ("model", "Select model"),
    ("thinking", "Set thinking level"),
    ("compact", "Compact context"),
    ("name", "Rename current session"),
    ("clone", "Clone current branch"),
    ("fork", "Fork current session"),
    ("label", "Label current branch"),
    ("tree", "Session tree"),
    ("import", "Import session file"),
    ("history", "Search prompt history"),
    ("share", "Share session (gist)"),
    ("export", "Export session (HTML)"),
    ("copy", "Copy last reply"),
    ("rewind", "Rewind last input"),
    ("giff", "Review uncommitted changes"),
    ("sidebar", "Toggle session sidebar"),
    ("quit", "Quit"),
];

/// 触发词类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trigger {
    /// `/` 开头的 GUI 命令
    Command,
    /// `@` 开头的文件路径
    File,
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
    /// 触发词在全文中的字节起点（含触发符）
    pub start: usize,
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
        // 配置类命令不在 `/` 里（走 Ctrl-M 命令面板）
        assert!(commands_with(&[], "settings").is_empty());
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

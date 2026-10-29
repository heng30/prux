//! 输入框补全提示：`/` 命令、`@` 文件路径、`!` `!!` shell 命令。
//!
//! 只做「识别触发词 + 给候选」，插入/执行由视图层负责（见 [`super::app`]）。

/// 候选面板最多显示几条。
pub const MAX_ITEMS: usize = 30;

/// 触发词类型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trigger {
    /// `/` 开头的 GUI 命令
    Command,
    /// `@` 开头的文件路径
    File,
    /// `!` / `!!` 开头的 shell 命令
    Shell,
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
    ("giff", "Review uncommitted changes"),
    ("import", "Import session file"),
    ("history", "Search prompt history"),
    ("share", "Share session (gist)"),
    ("export", "Export session (HTML)"),
    ("copy", "Copy last reply"),
    ("rewind", "Rewind last input"),
    ("quit", "Quit"),
];

/// 光标前最后一个 token 的起点。
fn token_start(text: &str) -> usize {
    text.rfind(['\n', ' ']).map_or(0, |index| index + 1)
}

/// 识别光标前的触发词，返回类型、起点（全文字节）与已输入前缀。
///
/// `/` 与 `!` `!!` 只在行首生效（与命令行习惯一致），`@` 可以在行内任意位置。
pub fn detect(text_before_cursor: &str) -> Option<(Trigger, usize, String)> {
    let start = token_start(text_before_cursor);
    let token = &text_before_cursor[start..];
    let at_line_start = start == 0 || text_before_cursor[..start].ends_with('\n');

    if at_line_start {
        if let Some(rest) = token.strip_prefix("!!") {
            return Some((Trigger::Shell, start, rest.to_string()));
        }
        if let Some(rest) = token.strip_prefix('!') {
            return Some((Trigger::Shell, start, rest.to_string()));
        }
        if let Some(rest) = token.strip_prefix('/') {
            return Some((Trigger::Command, start, rest.to_string()));
        }
    }
    if let Some(rest) = token.strip_prefix('@') {
        return Some((Trigger::File, start, rest.to_string()));
    }
    None
}

/// 命令候选：名字前缀匹配。
pub fn commands(query: &str) -> Vec<Item> {
    let query = query.trim().to_lowercase();
    COMMANDS
        .iter()
        .filter(|(name, _)| name.starts_with(&query))
        .take(MAX_ITEMS)
        .map(|(name, detail)| Item {
            label: format!("/{name}"),
            detail: (*detail).to_string(),
        })
        .collect()
}

/// 文件候选：路径子串匹配（`@src/mo` 这种逐段输入最好用），文件名命中的排前面。
///
/// `images_only` 为真时只列图片（附件按钮选「图片」后）。
pub fn files(paths: &[String], query: &str, images_only: bool) -> Vec<Item> {
    let query = query.trim().to_lowercase();
    let mut matched: Vec<&String> = paths
        .iter()
        .filter(|path| {
            !images_only || crate::utils::mime::is_image_path(std::path::Path::new(path))
        })
        .filter(|path| query.is_empty() || path.to_lowercase().contains(&query))
        .collect();
    matched.sort_by_key(|path| {
        let lower = path.to_lowercase();
        let name_hit = lower
            .rsplit('/')
            .next()
            .is_some_and(|name| name.starts_with(&query));
        let prefix_hit = lower.starts_with(&query);
        (!name_hit, !prefix_hit, path.len())
    });
    matched
        .into_iter()
        .take(MAX_ITEMS)
        .map(|path| Item {
            label: format!("@{path}"),
            detail: String::new(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 行首的 `/` `!` `!!` 才算触发；行中的 `!` 不算。
    #[test]
    fn detects_line_start_triggers() {
        assert_eq!(detect("/mo"), Some((Trigger::Command, 0, "mo".to_string())));
        assert_eq!(detect("!ls"), Some((Trigger::Shell, 0, "ls".to_string())));
        assert_eq!(detect("!!ls"), Some((Trigger::Shell, 0, "ls".to_string())));
        assert_eq!(detect("看下 /usr 里的东西"), None);
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

    /// 命令候选按前缀过滤，带 `/` 前缀展示。
    #[test]
    fn commands_filter_by_prefix() {
        let all = commands("");
        assert_eq!(all.len(), COMMANDS.len().min(MAX_ITEMS));
        assert_eq!(all[0].label, "/new");

        let some = commands("re");
        assert!(some.iter().all(|item| item.label.starts_with("/re")));
        assert!(some.iter().any(|item| item.label == "/resume"));
        assert!(commands("zzz").is_empty());
        // 配置类命令不在 `/` 里（走 Ctrl-M 命令面板）
        assert!(commands("settings").is_empty());
    }

    /// 触发词识别：`/` `!` 只在行首，`@` 行内任意位置。
    #[test]
    fn detect_requires_line_start_for_commands() {
        assert!(detect("abc /new").is_none());
        assert_eq!(detect("/ne").map(|d| d.0), Some(Trigger::Command));
        assert_eq!(detect("第一行\n!/ls").map(|d| d.0), Some(Trigger::Shell));
    }

    /// 文件候选命中文件名前缀的排前面。
    #[test]
    fn files_prefer_name_hits() {
        let paths = vec![
            "src/modes/gui/suggest.rs".to_string(),
            "docs/suggest.md".to_string(),
            "src/app.rs".to_string(),
        ];
        let items = files(&paths, "suggest", false);
        assert_eq!(items[0].label, "@docs/suggest.md");
        assert!(items.iter().all(|item| item.label.starts_with('@')));
        assert!(
            files(&paths, "app", false)
                .iter()
                .any(|i| i.label.ends_with("app.rs"))
        );
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

//! `/history` 候选（prompt 历史实时过滤 + `@clear` / `@clear-all` 控制词）
//!
//! 与 [`super::suggest`] 的 `/`、`@` 候选不同，历史候选没有「触发符」可供锚定：
//! 编辑器里那一整行（`/history <关键词>`）就是被替换的对象，所以 `start` 恒为 0，
//! 应用时是**整行替换**而不是从触发符往后替换。

use crate::{
    modes::interactive::app::{App, SuggestionItem, SuggestionKind},
    utils::display::flatten_line,
};

/// `/history` 的控制词表：(词, 描述, 是否影响所有项目)。
///
/// 加 `@` 前缀是为了与自由文本搜索词**分层**：否则 `clear` 这类很可能会出现在
/// prompt 里的词（“clear the cache”）会从「搜索词」变成「删除动作」——这是最坏的误伤方向。
const HISTORY_ACTIONS: [(&str, &str, bool); 2] = [
    ("@clear", "Remove prompt history for this project", false),
    (
        "@clear-all",
        "Remove ALL prompt history files (every project)",
        true,
    ),
];

/// 从历史构建候选：**空格分词 + 子串 AND**（大小写不敏感），顺序为**最近优先**。空查询 = 全部。
///
/// 不用 `fuzzy_filter`：模糊匹配是给短标识符（命令名/文件名）设计的，
/// 用在几百字的长 prompt 上会退化成「字符按顺序出现即命中」，等于全量返回。
fn history_items(history: &[String], query: &str) -> Vec<SuggestionItem> {
    let tokens: Vec<String> = query.split_whitespace().map(|t| t.to_lowercase()).collect();
    history
        .iter()
        .rev() // 最近优先
        .filter(|text| {
            let lower = text.to_lowercase();
            tokens.iter().all(|t| lower.contains(t.as_str()))
        })
        .map(|text| SuggestionItem {
            // name 仅用于渲染（单行、压平、截断）；insert 是原文，Tab/Enter 写回编辑器
            name: flatten_line(text),
            description: String::new(),
            insert: text.clone(),
        })
        .collect()
}

/// 控制词候选：查询以 `@` 开头且是某个控制词的**前缀**时命中。
///
/// 无匹配返回 `None`（而不是空列表）：那样 `/history @foo` 会退化成搜历史里的
/// `@foo`，而不是把用户卡在一个空面板上。
fn history_action_items(query: &str) -> Option<Vec<SuggestionItem>> {
    let q = query.trim().to_lowercase();
    if !q.starts_with('@') {
        return None;
    }

    let items: Vec<SuggestionItem> = HISTORY_ACTIONS
        .iter()
        .filter(|(name, _, _)| name.starts_with(&q))
        .map(|(name, desc, _)| SuggestionItem {
            name: name.to_string(),
            description: desc.to_string(),
            insert: name.to_string(),
        })
        .collect();
    (!items.is_empty()).then_some(items)
}

impl App {
    /// 刷新 prompt 历史候选（`/history` 或 `/history <关键词>`）；命中返回 true。
    ///
    /// 与其它候选不同，这里是**实时过滤**：光标前行前缀只要还是 `/history …`，
    /// 面板就随每次按键重建（`handle_editor_key` 尾部统一调 `refresh_suggestions`）。
    /// 因此 Enter 在任何时刻都有意义（发送选中项），不会出现「按下没反应」的死胡同。
    ///
    /// 只认精确的 `/history` 或其后紧跟空白：`/historyfoo` 仍未完成，交回命令候选/
    /// 未知命令处理。点开 `/history` 后 `start` 恒为 0——History 的 `apply_suggestion`
    /// 是**整行替换**，不是从触发符往后替换。
    pub(super) fn refresh_history_suggestions(&mut self, prefix: &str) -> bool {
        let Some(rest) = prefix.strip_prefix("/history") else {
            return false;
        };
        if !rest.is_empty() && !rest.starts_with(char::is_whitespace) {
            return false;
        }
        let query = rest.trim_start().to_string();

        // 控制词（`@clear` / `@clear-all`）：Enter 执行，Tab 不生效（不是可填入的文本）
        if let Some(items) = history_action_items(&query) {
            self.show_suggestions(SuggestionKind::HistoryAction, 0, query, items, true);
            return true;
        }

        let items = history_items(&self.editor.history, &query);
        // allow_empty：零匹配/空历史时面板留在原地显示「No matching history」，
        // 而不是静默关掉（搜索框的心智模型）
        self.show_suggestions(SuggestionKind::History, 0, query, items, true);
        true
    }

    /// 历史候选：把**整段**替换为条目原文（多行条目会真正插入换行）。
    ///
    /// 与 `/`、`@` 候选不同，历史没有「触发符」可供锚定——编辑器里那一整行
    /// （`/history <关键词>`）就是被替换的对象，所以 `start` 恒为 0、不做触发符校验。
    pub(super) fn apply_history_suggestion(&mut self, text: &str) {
        self.editor.replace_all(text);
        self.suggestion.deselect();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item_names(a: &App) -> Vec<&str> {
        a.suggestion.items.iter().map(|i| i.name.as_str()).collect()
    }

    /// 播种历史（最旧在前，对齐 `Editor::history`）并输入一行文字后刷新候选
    fn history_app(entries: &[&str], line: &str) -> App {
        let mut a = App::new();
        a.editor.history = entries.iter().map(|s| s.to_string()).collect();
        a.editor.clear();
        a.editor.insert_text(line);
        a.refresh_suggestions();
        a
    }

    #[test]
    fn history_panel_opens_at_exact_command_and_lists_recent_first() {
        // `/hist` 还走命令候选（提示别打错）
        let a = history_app(&["one", "two"], "/hist");
        assert!(a.suggestion.active);
        assert_eq!(a.suggestion.kind, SuggestionKind::Commands);

        // 打完 `/history` → 历史面板接管，最近优先
        let a = history_app(&["one", "two"], "/history");
        assert_eq!(a.suggestion.kind, SuggestionKind::History);
        assert_eq!(item_names(&a), vec!["two", "one"], "最近优先");

        // `/history `（带空格）同样是历史面板，空查询 = 全部
        let a = history_app(&["one", "two"], "/history ");
        assert_eq!(a.suggestion.kind, SuggestionKind::History);
        assert_eq!(item_names(&a), vec!["two", "one"]);

        // `/historyfoo` 不是本命令，不被征用
        let a = history_app(&["one"], "/historyfoo");
        assert_ne!(
            a.suggestion.kind,
            SuggestionKind::History,
            "未完成的写法不征用面板"
        );
    }

    #[test]
    fn history_query_uses_token_and_substring() {
        let entries = ["fix the bug", "add tests", "Fix BUG in parser"];
        // 多词 AND + 大小写不敏感：`fix bug` 能命中两个
        let a = history_app(&entries, "/history fix bug");
        assert_eq!(item_names(&a), vec!["Fix BUG in parser", "fix the bug"]);

        // 单词子串
        let a = history_app(&entries, "/history tests");
        assert_eq!(item_names(&a), vec!["add tests"]);

        // 词序无关
        let a = history_app(&entries, "/history bug fix");
        assert_eq!(item_names(&a), vec!["Fix BUG in parser", "fix the bug"]);
    }

    #[test]
    fn history_zero_match_keeps_panel_open_and_empty() {
        // 零匹配：面板留在原地（空列表 → 渲染为 No matching history），不静默关闭
        let a = history_app(&["one"], "/history zzz");
        assert!(a.suggestion.active, "零匹配应保持面板打开");
        assert_eq!(a.suggestion.kind, SuggestionKind::History);
        assert!(a.suggestion.items.is_empty());

        // 历史为空同样处于该状态
        let a = history_app(&[], "/history");
        assert!(a.suggestion.active);
        assert!(a.suggestion.items.is_empty());
    }

    #[test]
    fn history_apply_replaces_whole_line() {
        let mut a = history_app(&["fix the bug"], "/history fix");
        assert_eq!(a.suggestion.kind, SuggestionKind::History);
        a.apply_suggestion();
        assert_eq!(a.editor.text(), "fix the bug", "整行替换掉 /history …");
        assert!(!a.suggestion.active, "应用后关闭面板");
    }

    #[test]
    fn history_apply_inserts_multiline_entry_as_lines() {
        let entry = "first line\nsecond line";
        let mut a = history_app(&[entry], "/history first");
        a.apply_suggestion();
        assert_eq!(a.editor.line_count(), 2, "多行条目真正插入换行");
        assert_eq!(a.editor.text(), entry);
    }

    #[test]
    fn history_apply_survives_cursor_not_at_line_end() {
        // 即使光标不在行尾，整行替换也不依赖触发符位置
        let mut a = history_app(&["target"], "/history");
        a.editor.home();
        a.refresh_suggestions();
        let selected = a.suggestion.selected;
        assert!(a.suggestion.active);
        a.suggestion.selected = selected;
        a.apply_suggestion();
        assert_eq!(a.editor.text(), "target");
    }

    // ---- /history 控制词（@clear / @clear-all）----

    #[test]
    fn history_control_words_appear_for_at_prefix() {
        let a = history_app(&["one"], "/history @");
        assert_eq!(a.suggestion.kind, SuggestionKind::HistoryAction);
        assert_eq!(item_names(&a), vec!["@clear", "@clear-all"]);

        // 前缀也命中（可发现性）
        let a = history_app(&["one"], "/history @cl");
        assert_eq!(a.suggestion.kind, SuggestionKind::HistoryAction);

        // 精确的长词只剩自己
        let a = history_app(&["one"], "/history @clear-all");
        assert_eq!(item_names(&a), vec!["@clear-all"]);
    }

    #[test]
    fn history_control_words_do_not_hijack_plain_search() {
        // 不带 @ 的 clear 仍是普通搜索词——避免把「clear the cache」变成删除动作
        let a = history_app(&["clear the cache"], "/history clear");
        assert_eq!(a.suggestion.kind, SuggestionKind::History);
        assert_eq!(item_names(&a), vec!["clear the cache"]);

        // @ 开头但不成任何控制词前缀 → 退回搜索
        let a = history_app(&["@foo bar"], "/history @foo");
        assert_eq!(a.suggestion.kind, SuggestionKind::History);
        assert_eq!(item_names(&a), vec!["@foo bar"]);
    }

    #[test]
    fn history_control_word_tab_fills_history_command_without_executing() {
        // Tab：把控制词补进输入框（保留 `/history ` 前缀），但不执行删除、面板关闭
        let mut a = history_app(&["one"], "/history @clear");
        assert_eq!(a.suggestion.kind, SuggestionKind::HistoryAction);
        a.apply_suggestion();
        assert_eq!(
            a.editor.text(),
            "/history @clear",
            "Tab 应把控制词填入输入框"
        );
        assert!(!a.suggestion.active, "补齐后面板关闭");

        // 前缀查询（`/history @cl`）同样补成完整控制词
        let mut a = history_app(&["one"], "/history @cl");
        assert_eq!(a.suggestion.items[0].insert, "@clear");
        a.apply_suggestion();
        assert_eq!(a.editor.text(), "/history @clear");
    }

    #[test]
    fn history_names_are_flattened_to_one_line() {
        // 换行/制表/连续空白都压平（否则渲染行数与面板高度会算错）
        let a = history_app(&["a\n\nb\tc  d"], "/history");
        assert_eq!(a.suggestion.items[0].name, "a b c d");
        // insert 保留原文：Tab/Enter 写回编辑器的是用户当初输入的内容
        assert_eq!(a.suggestion.items[0].insert, "a\n\nb\tc  d");
    }
}

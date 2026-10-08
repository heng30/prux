//! 杂项模块集成测试：core::extensions、modes::interactive::editor、core::export_html。

use prux::core::provider::AgentMessage;
use prux::modes::interactive::editor::Editor;
use prux::test_support::HomeGuard;

// ---------- core::extensions ----------

#[test]
fn extension_trait_defines_tools_and_dispatch() {
    use prux::core::extensions::{Extension, ExtensionTool};
    use prux::core::tools::{ToolError, ToolResult};

    struct Demo;
    impl Extension for Demo {
        fn name(&self) -> &str {
            "demo"
        }
        fn tools(&self) -> Vec<ExtensionTool> {
            vec![ExtensionTool::simple(
                "ping",
                "reply with pong",
                serde_json::json!({ "type": "object" }),
                "ping",
            )]
        }
        fn execute_tool(
            &self,
            name: &str,
            _args: &serde_json::Value,
        ) -> std::result::Result<ToolResult, ToolError> {
            if name == "ping" {
                Ok(ToolResult::text("pong"))
            } else {
                Err(ToolError("unknown".to_string()))
            }
        }
    }

    let ext = Demo;
    assert_eq!(ext.name(), "demo");
    assert_eq!(ext.tools()[0].name, "ping");
    // 未注册到全局表，直接验证 trait 分发逻辑
    assert!(matches!(
        ext.execute_tool("ping", &serde_json::json!({})),
        Ok(ToolResult { text: t, .. }) if t == "pong"
    ));
    assert!(ext.execute_tool("nope", &serde_json::json!({})).is_err());
}

// ---------- modes::interactive::editor ----------

#[test]
fn editor_basic_editing() {
    let mut e = Editor::new();
    e.insert_text("hello");
    assert_eq!(e.text(), "hello");
    e.newline();
    e.insert_text("world");
    assert_eq!(e.text(), "hello\nworld");

    // undo 是逐操作快照：一次 undo 撤销一次输入/换行
    e.undo();
    assert_eq!(e.text(), "hello\n");
    e.undo();
    assert_eq!(e.text(), "hello");
    e.undo();
    assert_eq!(e.text(), "");
    // 无可撤销操作时不变化
    e.undo();
    assert_eq!(e.text(), "");
}

#[test]
fn editor_submit_and_history() {
    let mut e = Editor::new();
    e.insert_text("first");
    let submitted = e.submit();
    assert_eq!(submitted.as_deref(), Some("first"));
    assert!(e.is_empty());
    e.history_prev();
    assert_eq!(e.text(), "first");
    e.history_next();
    assert!(e.text().is_empty());
}

#[test]
fn editor_movement() {
    let mut e = Editor::new();
    e.insert_text("abcd");
    e.move_left();
    e.move_left();
    e.insert_char('X');
    assert_eq!(e.text(), "abXcd");
    e.home();
    e.delete();
    assert_eq!(e.text(), "bXcd");
    e.end();
    e.backspace();
    assert_eq!(e.text(), "bXc");
    e.clear_line();
    assert!(e.text().is_empty());
}

// ---------- core::export_html ----------

#[test]
fn export_session_writes_html() {
    // 依赖 pi 包的模板资源；不可用时跳过
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let mut s = prux::core::session_manager::Session::create(cwd, Some(dir.path().join("s")), true)
        .unwrap();
    s.append_message(&AgentMessage::user_text("hello"));
    let out = dir.path().join("out.html");
    let written =
        prux::core::export_html::export_session(&s, Some(out.to_str().unwrap()), Some("dark"))
            .unwrap();
    assert_eq!(written, out.to_str().unwrap());
    let html = std::fs::read_to_string(&out).unwrap();
    assert!(html.contains("<!DOCTYPE html>") || html.contains("<html"));
    assert!(
        html.len() > 1000,
        "HTML should include template and vendor assets"
    );
}

#[test]
fn export_html_ships_hidden_custom_message_toggle() {
    // pi #10020：display:false 的自定义消息默认隐藏，导出页 H 键/头部按钮可切换显示。
    // 模板资源必须自带该开关的样式与脚本（污染成只渲染 display 为真的条目会让它们彻底消失）
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let mut s = prux::core::session_manager::Session::create(cwd, Some(dir.path().join("s")), true)
        .unwrap();
    s.append_custom_entry(
        "hidden-card",
        Some(serde_json::json!({"content": "secret note", "display": false})),
    );
    let out = dir.path().join("out.html");
    prux::core::export_html::export_session(&s, Some(out.to_str().unwrap()), Some("dark")).unwrap();
    let html = std::fs::read_to_string(&out).unwrap();

    assert!(html.contains("hook-message-hidden"), "缺少隐藏样式类");
    assert!(
        html.contains("body:not(.show-hidden-messages) .hook-message-hidden"),
        "缺少默认隐藏的 CSS 规则"
    );
    assert!(
        html.contains("data-action=\"toggle-hidden-messages\""),
        "缺少头部切换按钮"
    );
    assert!(html.contains("setHiddenMessagesVisible"), "缺少切换函数");

    // 会话数据（base64）里必须带上 custom 条目（否则导出页无从切换）
    use base64::{Engine, engine::general_purpose::STANDARD};
    let marker = "<script id=\"session-data\" type=\"application/json\">";
    let start = html.find(marker).expect("模板应内嵌 session-data") + marker.len();
    let end = start
        + html[start..]
            .find('<')
            .expect("session-data 应以 </script> 结束");
    let decoded = String::from_utf8(STANDARD.decode(&html[start..end]).unwrap()).unwrap();
    assert!(decoded.contains("hidden-card"), "会话数据应含 custom 条目");
    assert!(decoded.contains("secret note"));
}

#[test]
fn export_session_expands_tilde_and_defaults_theme_path() {
    // /export ~/out.html 无法导出的回归：~ 前缀必须展开到 $HOME
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let home = tempfile::tempdir().unwrap();
    // 隔离 HOME（线程本地 override），避免污染真实家目录、无 set_var 竞态
    let _guard = HomeGuard::set(home.path().to_path_buf());
    let mut s = prux::core::session_manager::Session::create(cwd, Some(dir.path().join("s")), true)
        .unwrap();
    s.append_message(&AgentMessage::user_text("hello"));
    // 触发落盘（对齐新语义：首条 assistant 消息才建文件；export 默认名依赖 session 文件名）
    s.append_message(&AgentMessage {
        role: "assistant".into(),
        content: vec![],
        ..AgentMessage::user_text("")
    });

    // ~/out.html → $HOME/out.html（对齐 pi normalizePath expandTilde）
    let written =
        prux::core::export_html::export_session(&s, Some("~/out.html"), Some("dark")).unwrap();
    let expected = home.path().join("out.html");
    assert_eq!(written, expected.to_str().unwrap());
    assert!(expected.exists(), "导出文件应写入展开后的 $HOME 路径");

    // 无输出路径时默认 <app>-session-<stem>.html，且落在会话 cwd 下（对齐 pi exportSessionToHtml）
    let stem = s
        .get_session_file()
        .and_then(|f| f.file_stem())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap();
    let written = prux::core::export_html::export_session(&s, None, None).unwrap();
    assert_eq!(written, format!("{}/prux-session-{}.html", cwd, stem));
    assert!(std::path::Path::new(&written).exists());
}

#[test]
fn export_theme_vars_resolve_refs_and_are_valid_css() {
    // 回归：导出 HTML 主题变量必须是解析后的真实颜色。
    // 旧实现把 vars+colors 原样输出，`--text: text;` 这类未解析引用覆写了正确值，
    // 非法颜色被浏览器 fallback 成黑字，导出页面黑底黑字看不清。
    for theme in ["dark", "light"] {
        let (vars, page_bg, card_bg, info_bg) = prux::core::export_html::load_theme_vars(theme);
        // 应为十六进制颜色，而非 vars 引用键名
        for key in [
            "--text:",
            "--dim:",
            "--muted:",
            "--accent:",
            "--userMessageText:",
            "--toolOutput:",
            "--selectedBg:",
            "--mdHeading:",
            "--syntaxKeyword:",
        ] {
            let line = vars
                .lines()
                .find(|l| l.trim_start().starts_with(key))
                .unwrap_or_else(|| panic!("{} missing in {} theme", key, theme));
            let value = line
                .split(':')
                .nth(1)
                .unwrap_or("")
                .trim()
                .trim_end_matches(';');
            assert!(
                value.starts_with('#') || value.starts_with("rgb("),
                "{theme}: {key} 应为真实颜色，得到 {value:?}"
            );
        }
        assert!(
            page_bg.starts_with('#') || page_bg.starts_with("rgb("),
            "pageBg: {page_bg}"
        );
        assert!(
            card_bg.starts_with('#') || card_bg.starts_with("rgb("),
            "cardBg: {card_bg}"
        );
        assert!(
            info_bg.starts_with('#') || info_bg.starts_with("rgb("),
            "infoBg: {info_bg}"
        );
        // dark 应明确给出浅色文字而非黑字
        let text_line = vars
            .lines()
            .find(|l| l.trim_start().starts_with("--text:"))
            .unwrap();
        let text_color = text_line
            .split(':')
            .nth(1)
            .unwrap()
            .trim()
            .trim_end_matches(';');
        assert_ne!(text_color, "text", "dark: --text 不得是未解析引用");
    }
}

#[test]
fn export_html_contains_resolved_text_color() {
    // 端到端：导出的 HTML 内嵌 CSS 里 --text 为真实 hex 值
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().to_str().unwrap();
    let mut s = prux::core::session_manager::Session::create(cwd, Some(dir.path().join("s")), true)
        .unwrap();
    s.append_message(&AgentMessage::user_text("hello"));
    let out = dir.path().join("out.html");
    prux::core::export_html::export_session(&s, Some(out.to_str().unwrap()), Some("dark")).unwrap();
    let html = std::fs::read_to_string(&out).unwrap();
    // 只断言形态与明暗（不硬编码调色板）：dark 主题的正文色必须是具体浅色 hex
    let text_var = html
        .split("--text: ")
        .nth(1)
        .and_then(|rest| rest.split(';').next())
        .unwrap_or("")
        .to_string();
    let hex = text_var.strip_prefix('#').unwrap_or("");
    assert_eq!(hex.len(), 6, "dark 主题 --text 应为 hex，得到 {text_var:?}");
    let channel = |i: usize| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap();
    let luminance =
        0.2126 * channel(0) as f64 + 0.7152 * channel(1) as f64 + 0.0722 * channel(2) as f64;
    assert!(
        luminance > 128.0,
        "dark 主题 --text 应为浅色，得到 {text_var:?}"
    );
    assert!(
        !html.contains("--text: text;"),
        "不得出现未解析引用 --text: text"
    );
}

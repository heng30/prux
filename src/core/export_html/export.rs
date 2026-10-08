//! 会话导出主逻辑：使用内嵌模板与 vendor JS，注入会话数据与主题。

use crate::{
    core::{export_html::theme::load_theme_vars, session_manager::Session},
    embedded,
    error::{Error, Result},
    modes::interactive::theme::DEFAULT_THEME_NAME,
    utils::paths::resolve_path,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::path::PathBuf;

/// 导出页面的 HTML 骨架模板，运行时注入 CSS、数据与脚本。
const TEMPLATE_HTML: &str = embedded!("export-html/template.html");
/// 导出页面的样式模板，含主题变量占位符待注入。
const TEMPLATE_CSS: &str = embedded!("export-html/template.css");
/// 导出页面的交互脚本，负责渲染会话条目与滚动等行为。
const TEMPLATE_JS: &str = embedded!("export-html/template.js");
/// 内嵌的 marked 库，用于在导出页把 markdown 渲染为 HTML。
const MARKED_JS: &str = embedded!("export-html/vendor/marked.min.js");
/// 内嵌的 highlight.js 库，用于导出页代码块语法高亮。
const HIGHLIGHT_JS: &str = embedded!("export-html/vendor/highlight.min.js");

/// 导出会话为 HTML
pub fn export_session(
    session: &Session,
    output_path: Option<&str>,
    theme_name: Option<&str>,
) -> Result<String> {
    export_session_extra(session, output_path, theme_name, None)
}

/// 带额外会话数据（systemPrompt/tools）的导出
pub fn export_session_extra(
    session: &Session,
    output_path: Option<&str>,
    theme_name: Option<&str>,
    extra: Option<Value>,
) -> Result<String> {
    let theme = theme_name.unwrap_or(DEFAULT_THEME_NAME);
    let (theme_vars, body_bg, container_bg, info_bg) = load_theme_vars(theme);

    let mut session_data = json!({
        "header": session.get_header().unwrap_or(Value::Null),
        "entries": session.get_entries(),
        "leafId": session.get_leaf_id(),
    });
    if let Some(extra) = extra
        && let Some(obj) = session_data.as_object_mut()
    {
        for (k, v) in extra.as_object().unwrap_or(&serde_json::Map::new()) {
            obj.insert(k.clone(), v.clone());
        }
    }
    let session_data_b64 = base64_encode(&serde_json::to_string(&session_data)?);

    let css = TEMPLATE_CSS
        .replace("{{THEME_VARS}}", &theme_vars)
        .replace("{{BODY_BG}}", &body_bg)
        .replace("{{CONTAINER_BG}}", &container_bg)
        .replace("{{INFO_BG}}", &info_bg);

    let html = TEMPLATE_HTML
        .replace("{{CSS}}", &css)
        .replace("{{JS}}", TEMPLATE_JS)
        .replace("{{SESSION_DATA}}", &session_data_b64)
        .replace("{{MARKED_JS}}", MARKED_JS)
        .replace("{{HIGHLIGHT_JS}}", HIGHLIGHT_JS);

    // 展开 ~、相对路径基于会话 cwd；未指定路径时默认
    // `<app>-session-<basename>.html`（沿用会话文件名 stem）。
    let out_path = resolve_export_path(session, output_path);
    if let Some(parent) = out_path.parent() {
        _ = std::fs::create_dir_all(parent);
    }

    std::fs::write(&out_path, &html).map_err(|source| Error::Io {
        context: format!("Failed to write export file: {}", out_path.display()),
        source,
    })?;

    Ok(out_path.to_string_lossy().to_string())
}

/// 解析导出输出路径：`~`/`~/` 前缀、相对路径基于会话 cwd；
/// 未指定时生成 `<app>-session-<basename>.html` 到会话 cwd。
fn resolve_export_path(session: &Session, output_path: Option<&str>) -> PathBuf {
    let Some(raw) = output_path else {
        let basename = session
            .get_session_file()
            .and_then(|f| f.file_stem())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| session.session_id.clone());
        return resolve_path(
            &format!("{}-session-{}.html", crate::APP_NAME, basename),
            &session.cwd,
        );
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return resolve_export_path(session, None);
    }
    resolve_path(raw, &session.cwd)
}

/// 把字符串按 UTF-8 字节做 base64（STANDARD 字母表，带 padding）。
#[inline(always)]
fn base64_encode(data: &str) -> String {
    STANDARD.encode(data.as_bytes())
}

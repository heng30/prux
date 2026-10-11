//! `browser` 工具的动作实现。
//!
//! 每个动作 = 一次或几次 CDP 命令 + 一段面向模型的文本。动作都在一条**已连接**的
//! [`Cdp`] 上执行（连接与生命周期由扩展层负责），所以本模块可以脱离全局状态单测。
//!
//! 两条贯穿全部动作的约定：
//! - 等页面就绪用轮询 `document.readyState`，不等 `Page.loadEventFired`（省掉事件路由）；
//! - 元素定位优先用 `snapshot` 分配的 `ref`（`e1`…），其次才是 CSS `selector`。

use super::{cdp::Cdp, config::Config, snapshot};
use crate::{
    core::tools::{ToolError, ToolResult, ToolResultAttachment, resolve_tool_path},
    extensions::util::{
        opt_str, opt_string_array, req_str, truncate_chars_with_postfix, value_text,
    },
    utils::{
        image::{ImageResizeLimits, prepare_image},
        output_files,
        truncate::format_size,
    },
};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

/// `browser` 工具的全部动作名（也用于错误文案）。
pub(crate) const ACTIONS: [&str; 16] = [
    "status",
    "list_tabs",
    "new_tab",
    "select_tab",
    "open",
    "snapshot",
    "get_content",
    "click",
    "type",
    "select",
    "press",
    "scroll",
    "wait",
    "eval",
    "screenshot",
    "upload",
];

/// 轮询类动作（等页面就绪、`wait`）的间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// 快照/正文截断的尾部标注。
const TRUNCATED_SUFFIX: &str = "\n… [truncated]";

/// 校验动作名。
///
/// 在建立连接**之前**调用：动作名拼错时不该为了报错去连浏览器。
/// 返回规范化后的动作名（原样，仅去首尾空白）。
pub(crate) fn validate_action(args: &Value) -> Result<String, ToolError> {
    let action = req_str(args, "action")?;
    if ACTIONS.contains(&action.as_str()) {
        return Ok(action);
    }
    Err(unknown_action(&action))
}

/// 按动作分发（`action` 已由 [`validate_action`] 校验过）；
/// `cwd` 用于解析 `upload` 的相对路径，`cfg` 提供超时与输出上限。
pub(crate) async fn execute(
    cdp: &Cdp,
    action: &str,
    args: &Value,
    cwd: &str,
    cfg: &Config,
) -> Result<ToolResult, ToolError> {
    match action {
        "status" => status(cdp).await,
        "list_tabs" => list_tabs(cdp).await,
        "new_tab" => new_tab(cdp, args, cfg).await,
        "select_tab" => select_tab(cdp, args).await,
        "open" => open(cdp, args, cfg).await,
        "snapshot" => take_snapshot(cdp, args, cfg).await,
        "get_content" => get_content(cdp, args, cfg).await,
        "click" => click(cdp, args).await,
        "type" => type_text(cdp, args).await,
        "select" => select_option(cdp, args).await,
        "press" => press(cdp, args).await,
        "scroll" => scroll(cdp, args).await,
        "wait" => wait(cdp, args, cfg).await,
        "eval" => eval(cdp, args).await,
        "screenshot" => screenshot(cdp, args).await,
        "upload" => upload(cdp, args, cwd).await,
        other => Err(unknown_action(other)),
    }
}

/// 未知动作的报错（列出全部合法动作）。
fn unknown_action(action: &str) -> ToolError {
    ToolError(format!(
        "browser: unknown action `{action}`; expected one of {}",
        ACTIONS.join(", ")
    ))
}

/// `status`：连接信息与活动标签页（不发任何改变页面状态的命令）。
async fn status(cdp: &Cdp) -> Result<ToolResult, ToolError> {
    let pages = cdp.pages().await.map_err(ToolError)?;
    let mut lines = vec![
        format!("browser: {}", cdp.browser()),
        format!("endpoint: {}", cdp.ws_url()),
        format!("tabs: {}", pages.len()),
    ];
    match cdp.active() {
        Some(target) => lines.push(format!(
            "active tab: {} <{}>",
            one_line(&target.title, 80),
            one_line(&target.url, 120)
        )),
        None => lines
            .push("active tab: none (the next page action attaches to the first tab)".to_string()),
    }
    Ok(ToolResult::text(lines.join("\n")))
}

/// `list_tabs`：列出全部标签页，活动标签页前面标 `*`。
async fn list_tabs(cdp: &Cdp) -> Result<ToolResult, ToolError> {
    let pages = cdp.pages().await.map_err(ToolError)?;
    let active = cdp.active().map(|target| target.target_id);
    let mut lines = vec![format!("{} tab(s):", pages.len())];
    if pages.is_empty() {
        lines.push("  (none)".to_string());
    }
    for (index, target) in pages.iter().enumerate() {
        let marker = if active.as_deref() == Some(target.target_id.as_str()) {
            "*"
        } else {
            " "
        };
        lines.push(format!(
            "{marker} [{index}] {} <{}>  id={}",
            one_line(&target.title, 80),
            one_line(&target.url, 120),
            target.target_id
        ));
    }
    Ok(ToolResult::text(lines.join("\n")))
}

/// `new_tab`：新建标签页并切过去；给了 URL 就等它加载。
async fn new_tab(cdp: &Cdp, args: &Value, cfg: &Config) -> Result<ToolResult, ToolError> {
    let url = match opt_str(args, "url") {
        Some(url) => normalize_url(&url)?,
        None => "about:blank".to_string(),
    };
    let target = cdp.create_target(&url).await.map_err(ToolError)?;
    let session = cdp.attach(&target, true).await.map_err(ToolError)?;
    let state = if url == "about:blank" {
        String::new()
    } else {
        wait_ready(cdp, &session, cfg.timeout).await
    };
    let summary = page_summary(cdp, &session).await;
    Ok(ToolResult::text(format!(
        "new tab: {url}{}\n{summary}",
        ready_suffix(&state)
    )))
}

/// `select_tab`：按 `index`（来自 `list_tabs`）或 `targetId` 切换活动标签页。
async fn select_tab(cdp: &Cdp, args: &Value) -> Result<ToolResult, ToolError> {
    let pages = cdp.pages().await.map_err(ToolError)?;
    let target = match (
        args.get("index").and_then(Value::as_u64),
        opt_str(args, "targetId"),
    ) {
        (Some(index), _) => pages.get(index as usize).cloned().ok_or_else(|| {
            ToolError(format!(
                "browser: no tab at index {index}; {} tab(s) open (use list_tabs)",
                pages.len()
            ))
        })?,
        (None, Some(target_id)) => pages
            .iter()
            .find(|target| target.target_id == target_id)
            .cloned()
            .ok_or_else(|| {
                ToolError(format!(
                    "browser: no tab with id `{target_id}`; use list_tabs to see the open tabs"
                ))
            })?,
        (None, None) => {
            return Err(ToolError(
                "browser: select_tab needs `index` (from list_tabs) or `targetId`".to_string(),
            ));
        }
    };

    let session = cdp.attach(&target, true).await.map_err(ToolError)?;
    let summary = page_summary(cdp, &session).await;
    Ok(ToolResult::text(format!("active tab: {summary}")))
}

/// `open`：在当前标签页导航。
async fn open(cdp: &Cdp, args: &Value, cfg: &Config) -> Result<ToolResult, ToolError> {
    let url = normalize_url(&req_str(args, "url")?)?;
    let session = ensure_session(cdp).await?;
    let result = cdp
        .send_session(&session, "Page.navigate", json!({ "url": url }))
        .await
        .map_err(ToolError)?;
    if let Some(error) = result.get("errorText").and_then(Value::as_str) {
        return Err(ToolError(format!(
            "browser: navigation to {url} failed: {error}"
        )));
    }

    let state = wait_ready(cdp, &session, cfg.timeout).await;
    let summary = page_summary(cdp, &session).await;
    Ok(ToolResult::text(format!(
        "opened {url}{}\n{summary}",
        ready_suffix(&state)
    )))
}

/// `snapshot`：页面结构 + 可交互元素引用（`e1`…）。
async fn take_snapshot(cdp: &Cdp, args: &Value, cfg: &Config) -> Result<ToolResult, ToolError> {
    let session = ensure_session(cdp).await?;
    let max_elements = optional_usize(args, "maxElements")?
        .map(|value| value.clamp(1, 2000))
        .unwrap_or(cfg.max_elements);
    let max_chars = optional_usize(args, "maxChars")?
        .map(|value| value.clamp(500, 1_000_000))
        .unwrap_or(cfg.max_snapshot_chars);

    let remote = cdp
        .evaluate(&session, &snapshot::snapshot_expression(max_elements), true)
        .await
        .map_err(ToolError)?;
    let data =
        snapshot::parse(&remote.get("value").cloned().unwrap_or(Value::Null)).map_err(ToolError)?;
    Ok(ToolResult::text(snapshot::format(&data, max_chars)))
}

/// `get_content`：页面可见文本（`text`）或整页 HTML（`html`）。
async fn get_content(cdp: &Cdp, args: &Value, cfg: &Config) -> Result<ToolResult, ToolError> {
    let format = opt_str(args, "format").unwrap_or_else(|| "text".to_string());
    let expression = match format.as_str() {
        "text" => snapshot::TEXT_JS,
        "html" => snapshot::HTML_JS,
        other => {
            return Err(ToolError(format!(
                "browser: `format` must be `text` or `html`, got `{other}`"
            )));
        }
    };

    let session = ensure_session(cdp).await?;
    let body = eval_string(cdp, &session, expression).await?;
    let summary = page_summary(cdp, &session).await;
    let body = truncate_chars_with_postfix(&body, cfg.max_content_chars, TRUNCATED_SUFFIX);
    Ok(ToolResult::text(format!(
        "{summary}\nformat: {format}\n\n{body}"
    )))
}

/// `click`：把元素滚到视口中央后派发真实鼠标事件（`el.click()` 覆盖不到依赖坐标的场景）。
async fn click(cdp: &Cdp, args: &Value) -> Result<ToolResult, ToolError> {
    let session = ensure_session(cdp).await?;
    let element = element_arg(args)?;
    let rect = element_rect(cdp, &session, &element).await?;
    if !(rect.width > 0.0 && rect.height > 0.0) {
        return Err(ToolError(
            "browser: the element has no clickable area (is it visible?)".to_string(),
        ));
    }
    let (x, y) = (rect.x + rect.width / 2.0, rect.y + rect.height / 2.0);

    for params in [
        json!({ "type": "mouseMoved", "x": x, "y": y }),
        json!({ "type": "mousePressed", "x": x, "y": y, "button": "left", "buttons": 1, "clickCount": 1 }),
        json!({ "type": "mouseReleased", "x": x, "y": y, "button": "left", "buttons": 0, "clickCount": 1 }),
    ] {
        cdp.send_session(&session, "Input.dispatchMouseEvent", params)
            .await
            .map_err(ToolError)?;
    }

    Ok(ToolResult::text(format!(
        "clicked ({x:.0}, {y:.0}) on {}",
        target_description(args)
    )))
}

/// `type`：聚焦目标元素（可选先清空）后插入文本，可选回车提交。
///
/// 没有 `ref` / `selector` 时输入到当前焦点元素。
async fn type_text(cdp: &Cdp, args: &Value) -> Result<ToolResult, ToolError> {
    let session = ensure_session(cdp).await?;
    let text = args
        .get("text")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError("browser: `text` is required".to_string()))?
        .to_string();
    let clear = optional_bool(args, "clear")?.unwrap_or(false);
    let submit = optional_bool(args, "submit")?.unwrap_or(false);

    if has_element_arg(args) {
        let element = element_arg(args)?;
        eval_object(cdp, &session, &snapshot::focus_expression(&element, clear)).await?;
    } else if clear {
        eval_object(cdp, &session, snapshot::CLEAR_FOCUSED_JS).await?;
    }

    if !text.is_empty() {
        cdp.send_session(&session, "Input.insertText", json!({ "text": text }))
            .await
            .map_err(ToolError)?;
    }
    if submit {
        dispatch_key(cdp, &session, "Enter").await?;
    }

    let typed = truncate_chars_with_postfix(&text, 60, "…");
    Ok(ToolResult::text(format!(
        "typed {typed:?} into {}{}",
        target_description(args),
        if submit { " and pressed Enter" } else { "" }
    )))
}

/// `select`：设置 `<select>` 的取值并派发 `input` / `change`。
async fn select_option(cdp: &Cdp, args: &Value) -> Result<ToolResult, ToolError> {
    let session = ensure_session(cdp).await?;
    let element = element_arg(args)?;
    let value = req_str(args, "value")?;
    let result = eval_object(
        cdp,
        &session,
        &snapshot::select_expression(&element, &value),
    )
    .await?;
    let selected = result
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or(&value)
        .to_string();
    Ok(ToolResult::text(format!(
        "selected {selected:?} on {}",
        target_description(args)
    )))
}

/// `press`：聚焦目标元素（若给了）后按一个键。
async fn press(cdp: &Cdp, args: &Value) -> Result<ToolResult, ToolError> {
    let session = ensure_session(cdp).await?;
    let key = req_str(args, "key")?;
    if has_element_arg(args) {
        let element = element_arg(args)?;
        eval_object(cdp, &session, &snapshot::focus_expression(&element, false)).await?;
    }
    dispatch_key(cdp, &session, &key).await?;
    Ok(ToolResult::text(format!("pressed {key}")))
}

/// `scroll`：滚动到元素 / 页顶页底，或按增量滚动。
async fn scroll(cdp: &Cdp, args: &Value) -> Result<ToolResult, ToolError> {
    let session = ensure_session(cdp).await?;
    let element = if has_element_arg(args) {
        Some(element_arg(args)?)
    } else {
        None
    };
    let to = match opt_str(args, "to").as_deref() {
        Some(value @ ("top" | "bottom")) => Some(value.to_string()),
        Some(other) => {
            return Err(ToolError(format!(
                "browser: `to` must be `top` or `bottom`, got `{other}`"
            )));
        }
        None => None,
    };
    let dx = optional_f64(args, "deltaX")?.unwrap_or(0.0);
    let dy = optional_f64(args, "deltaY")?.unwrap_or(if element.is_none() && to.is_none() {
        500.0
    } else {
        0.0
    });

    let result = eval_object(
        cdp,
        &session,
        &snapshot::scroll_expression(element.as_deref(), to.as_deref(), dx, dy),
    )
    .await?;
    let y = result.get("y").and_then(Value::as_f64).unwrap_or(0.0);
    Ok(ToolResult::text(format!("scrolled to y={y:.0}")))
}

/// `wait`：按毫秒、选择器或页面文本等待。
async fn wait(cdp: &Cdp, args: &Value, cfg: &Config) -> Result<ToolResult, ToolError> {
    if let Some(ms) = optional_usize(args, "ms")? {
        let ms = ms.min(600_000);
        tokio::time::sleep(Duration::from_millis(ms as u64)).await;
        return Ok(ToolResult::text(format!("waited {ms}ms")));
    }

    let timeout = optional_usize(args, "timeout")?
        .map(|value| value.clamp(1, 600) as u64)
        .unwrap_or(cfg.timeout.as_secs());
    let condition = if let Some(selector) = opt_str(args, "selector") {
        format!(
            "!!document.querySelector({})",
            snapshot::json_literal(&selector)
        )
    } else if let Some(text) = opt_str(args, "text") {
        format!(
            "!!(document.body && (document.body.innerText || '').includes({}))",
            snapshot::json_literal(&text)
        )
    } else {
        return Err(ToolError(
            "browser: wait needs `ms`, `selector`, or `text`".to_string(),
        ));
    };

    let session = ensure_session(cdp).await?;
    let started = Instant::now();
    let deadline = started + Duration::from_secs(timeout);
    loop {
        let remote = cdp
            .evaluate(&session, &condition, true)
            .await
            .map_err(ToolError)?;
        if remote.get("value").and_then(Value::as_bool) == Some(true) {
            return Ok(ToolResult::text(format!(
                "condition met after {}ms",
                started.elapsed().as_millis()
            )));
        }
        if Instant::now() >= deadline {
            return Err(ToolError(format!(
                "browser: wait timed out after {timeout}s"
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// `eval`：在页面里执行一段表达式，返回它的值。
async fn eval(cdp: &Cdp, args: &Value) -> Result<ToolResult, ToolError> {
    let session = ensure_session(cdp).await?;
    let expression = req_str(args, "expression")?;
    let await_promise = optional_bool(args, "awaitPromise")?.unwrap_or(false);
    let remote = cdp
        .evaluate_with(&session, &expression, true, await_promise)
        .await
        .map_err(ToolError)?;

    let text = match remote.get("value") {
        Some(value) => value_text(value),
        None => remote
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("undefined")
            .to_string(),
    };
    Ok(ToolResult::text(text))
}

/// `screenshot`：截当前视口 / 整页 / 某个元素，落盘并作为图片附件回给模型。
async fn screenshot(cdp: &Cdp, args: &Value) -> Result<ToolResult, ToolError> {
    let session = ensure_session(cdp).await?;
    let full_page = optional_bool(args, "fullPage")?.unwrap_or(false);
    let element = if has_element_arg(args) {
        Some(element_arg(args)?)
    } else {
        None
    };

    let mut params = json!({ "format": "png", "fromSurface": true });
    if let Some(element) = element {
        let rect = element_rect(cdp, &session, &element).await?;
        if !(rect.width > 0.0 && rect.height > 0.0) {
            return Err(ToolError(
                "browser: the element has no visible area to capture".to_string(),
            ));
        }
        params["clip"] = json!({
            "x": rect.x,
            "y": rect.y,
            "width": rect.width,
            "height": rect.height,
            "scale": 1,
        });
    } else if full_page {
        let metrics = cdp
            .send_session(&session, "Page.getLayoutMetrics", json!({}))
            .await
            .map_err(ToolError)?;
        let size = metrics
            .get("cssContentSize")
            .or_else(|| metrics.get("contentSize"))
            .cloned()
            .unwrap_or(Value::Null);
        let width = size.get("width").and_then(Value::as_f64).unwrap_or(0.0);
        let height = size.get("height").and_then(Value::as_f64).unwrap_or(0.0);
        if !(width > 0.0 && height > 0.0) {
            return Err(ToolError(
                "browser: the page reports no layout size to capture".to_string(),
            ));
        }
        params["captureBeyondViewport"] = json!(true);
        params["clip"] = json!({ "x": 0, "y": 0, "width": width, "height": height, "scale": 1 });
    }

    let result = cdp
        .send_session(&session, "Page.captureScreenshot", params)
        .await
        .map_err(ToolError)?;
    let data = result
        .get("data")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError("browser: the browser returned no screenshot data".to_string()))?;
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data)
        .map_err(|e| ToolError(format!("browser: invalid screenshot data: {e}")))?;

    let summary = page_summary(cdp, &session).await;
    let mut out = ToolResult::text(format!("{summary}\n{}", save_screenshot(&bytes)));
    if let Some(prepared) = prepare_image(&bytes, "image/png", ImageResizeLimits::default(), true) {
        out = out.with_attachment(ToolResultAttachment {
            data_base64: prepared.data_base64,
            mime_type: prepared.mime_type,
            original_size: prepared.original_size,
            converted_from: prepared.converted_from,
        });
    }
    Ok(out)
}

/// `upload`：把本地文件塞进 `<input type="file">`（`DOM.setFileInputFiles`）。
async fn upload(cdp: &Cdp, args: &Value, cwd: &str) -> Result<ToolResult, ToolError> {
    let session = ensure_session(cdp).await?;
    let element = element_arg(args)?;
    let paths = opt_string_array(args, "paths")
        .filter(|paths| !paths.is_empty())
        .ok_or_else(|| {
            ToolError("browser: `paths` must be a non-empty array of file paths".to_string())
        })?;

    let files: Vec<String> = paths
        .iter()
        .map(|path| resolve_tool_path(path, cwd).to_string_lossy().to_string())
        .collect();
    for file in &files {
        if !std::path::Path::new(file).is_file() {
            return Err(ToolError(format!("browser: file not found: {file}")));
        }
    }

    let remote = cdp
        .evaluate(
            &session,
            &format!("(() => {{ const el = {element}; return el || null; }})()"),
            false,
        )
        .await
        .map_err(ToolError)?;
    let object_id = remote
        .get("objectId")
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError(element_missing_hint()))?;

    cdp.send_session(
        &session,
        "DOM.setFileInputFiles",
        json!({ "files": files, "objectId": object_id }),
    )
    .await
    .map_err(ToolError)?;

    Ok(ToolResult::text(format!(
        "uploaded {} file(s) to {}",
        files.len(),
        target_description(args)
    )))
}

/// 取当前活动页面的会话；没有就 attach 第一个标签页，再没有就新建 `about:blank`。
///
/// 新 attach 的标签页会被切到前台——输入事件只对活动标签页生效。
async fn ensure_session(cdp: &Cdp) -> Result<String, ToolError> {
    if let Some(session) = cdp.active_session() {
        return Ok(session);
    }
    match cdp.pages().await.map_err(ToolError)?.into_iter().next() {
        Some(target) => cdp.attach(&target, true).await.map_err(ToolError),
        None => {
            let target = cdp.create_target("about:blank").await.map_err(ToolError)?;
            cdp.attach(&target, true).await.map_err(ToolError)
        }
    }
}

/// 轮询 `document.readyState` 直到 `complete`；超时或页面取不到状态时返回最后的取值（不报错）。
async fn wait_ready(cdp: &Cdp, session: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut last = String::new();
    loop {
        match eval_string(cdp, session, snapshot::READY_STATE_JS).await {
            Ok(state) => {
                last = state;
                if last == "complete" {
                    return last;
                }
            }
            Err(_) => return last,
        }
        if Instant::now() >= deadline {
            return last;
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// 就绪状态的后缀（空状态 = 没等，不加后缀）。
fn ready_suffix(state: &str) -> String {
    if state.is_empty() {
        String::new()
    } else {
        format!("\nreadyState: {state}")
    }
}

/// 当前页面的 `title <url>` 摘要（取不到时返回空串）。
async fn page_summary(cdp: &Cdp, session: &str) -> String {
    let expression = "(() => ({ title: document.title, url: location.href }))()";
    match eval_object(cdp, session, expression).await {
        Ok(value) => format!(
            "{} <{}>",
            one_line(value.get("title").and_then(Value::as_str).unwrap_or(""), 80),
            one_line(value.get("url").and_then(Value::as_str).unwrap_or(""), 120)
        ),
        Err(_) => String::new(),
    }
}

/// 执行一段返回对象的 JS 并取出对象；页面返回 `{ error: ... }` 时转成 `ToolError`。
async fn eval_object(cdp: &Cdp, session: &str, expression: &str) -> Result<Value, ToolError> {
    let remote = cdp
        .evaluate(session, expression, true)
        .await
        .map_err(ToolError)?;
    let value = remote.get("value").cloned().unwrap_or(Value::Null);
    if let Some(error) = snapshot::error_of(&value) {
        return Err(ToolError(format!("browser: {error}")));
    }
    Ok(value)
}

/// 执行一段返回字符串的 JS（页面取不到值时返回空串）。
async fn eval_string(cdp: &Cdp, session: &str, expression: &str) -> Result<String, ToolError> {
    let remote = cdp
        .evaluate(session, expression, true)
        .await
        .map_err(ToolError)?;
    Ok(remote
        .get("value")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string())
}

/// 元素的视口矩形（CSS 像素，`x` / `y` 是左上角）。
struct Rect {
    /// 左边界（视口坐标，可为负）。
    x: f64,
    /// 上边界（视口坐标，可为负）。
    y: f64,
    /// 宽度（像素）；0 表示元素不可见。
    width: f64,
    /// 高度（像素）；0 表示元素不可见。
    height: f64,
}

/// 取元素矩形（先滚到视口中央）。
async fn element_rect(cdp: &Cdp, session: &str, element: &str) -> Result<Rect, ToolError> {
    let value = eval_object(cdp, session, &snapshot::element_rect_expression(element)).await?;
    let number = |key: &str| value.get(key).and_then(Value::as_f64).unwrap_or(f64::NAN);
    let rect = Rect {
        x: number("x"),
        y: number("y"),
        width: number("width"),
        height: number("height"),
    };
    if !(rect.x.is_finite()
        && rect.y.is_finite()
        && rect.width.is_finite()
        && rect.height.is_finite())
    {
        return Err(ToolError(
            "browser: the element has no measurable position".to_string(),
        ));
    }
    Ok(rect)
}

/// 元素定位表达式：`ref` 优先，其次 `selector`，都没有则报错。
fn element_arg(args: &Value) -> Result<String, ToolError> {
    snapshot::element_expression(
        opt_str(args, "ref").as_deref(),
        opt_str(args, "selector").as_deref(),
    )
    .map_err(ToolError)
}

/// 参数里是否给了元素定位（`ref` 或 `selector`）。
fn has_element_arg(args: &Value) -> bool {
    opt_str(args, "ref").is_some() || opt_str(args, "selector").is_some()
}

/// 面向模型的目标描述（回显它给的定位方式）。
fn target_description(args: &Value) -> String {
    if let Some(reference) = opt_str(args, "ref") {
        format!("ref {reference}")
    } else if let Some(selector) = opt_str(args, "selector") {
        format!("selector `{selector}`")
    } else {
        "the focused element".to_string()
    }
}

/// 元素找不到时的提示（引用会随导航失效）。
fn element_missing_hint() -> String {
    "browser: element not found; references go stale after a navigation - call snapshot again"
        .to_string()
}

/// 派发一次按键（按下 + 抬起）；未知键名返回 `Err`。
async fn dispatch_key(cdp: &Cdp, session: &str, key: &str) -> Result<(), ToolError> {
    let spec =
        key_spec(key).ok_or_else(|| ToolError(format!("browser: unsupported key `{key}`")))?;

    let mut down = json!({
        "type": if spec.text.is_some() { "keyDown" } else { "rawKeyDown" },
        "key": spec.key,
        "code": spec.code,
        "windowsVirtualKeyCode": spec.vk,
        "nativeVirtualKeyCode": spec.vk,
    });
    if let Some(text) = &spec.text {
        down["text"] = json!(text);
    }
    cdp.send_session(session, "Input.dispatchKeyEvent", down)
        .await
        .map_err(ToolError)?;

    let up = json!({
        "type": "keyUp",
        "key": spec.key,
        "code": spec.code,
        "windowsVirtualKeyCode": spec.vk,
        "nativeVirtualKeyCode": spec.vk,
    });
    cdp.send_session(session, "Input.dispatchKeyEvent", up)
        .await
        .map_err(ToolError)?;
    Ok(())
}

/// 一次按键的 CDP 参数。
struct KeySpec {
    /// `key` 字段（如 `Enter`、`a`）。
    key: String,
    /// `code` 字段（如 `Enter`、`KeyA`；无对应码的符号为空串）。
    code: String,
    /// `windowsVirtualKeyCode` 与 `nativeVirtualKeyCode`（ASCII 键码）。
    vk: i64,
    /// 该键输入的文本；无文本的键（方向键、Esc 等）为 `None`。
    text: Option<String>,
}

/// 按键名 → CDP 参数；支持具名键与单个 ASCII 字符，其余返回 `None`。
fn key_spec(key: &str) -> Option<KeySpec> {
    let named = |key: &str, code: &str, vk: i64, text: Option<&str>| KeySpec {
        key: key.to_string(),
        code: code.to_string(),
        vk,
        text: text.map(str::to_string),
    };

    match key {
        "Enter" | "Return" => return Some(named("Enter", "Enter", 13, Some("\r"))),
        "Tab" => return Some(named("Tab", "Tab", 9, Some("\t"))),
        "Escape" | "Esc" => return Some(named("Escape", "Escape", 27, None)),
        "Backspace" => return Some(named("Backspace", "Backspace", 8, None)),
        "Delete" => return Some(named("Delete", "Delete", 46, None)),
        "ArrowUp" => return Some(named("ArrowUp", "ArrowUp", 38, None)),
        "ArrowDown" => return Some(named("ArrowDown", "ArrowDown", 40, None)),
        "ArrowLeft" => return Some(named("ArrowLeft", "ArrowLeft", 37, None)),
        "ArrowRight" => return Some(named("ArrowRight", "ArrowRight", 39, None)),
        "Home" => return Some(named("Home", "Home", 36, None)),
        "End" => return Some(named("End", "End", 35, None)),
        "PageUp" => return Some(named("PageUp", "PageUp", 33, None)),
        "PageDown" => return Some(named("PageDown", "PageDown", 34, None)),
        "Space" | " " => return Some(named(" ", "Space", 32, Some(" "))),
        _ => {}
    }

    let mut chars = key.chars();
    let first = chars.next()?;
    if chars.next().is_some() || !first.is_ascii() {
        return None;
    }
    let upper = first.to_ascii_uppercase();
    let code = if first.is_ascii_alphabetic() {
        format!("Key{upper}")
    } else if first.is_ascii_digit() {
        format!("Digit{first}")
    } else {
        String::new()
    };
    Some(KeySpec {
        key: first.to_string(),
        code,
        vk: u32::from(upper) as i64,
        text: Some(first.to_string()),
    })
}

/// 补全 URL 的 scheme：没有 `://` 也不是 `about:` / `data:` / `file:` 时按 `https://` 处理。
fn normalize_url(input: &str) -> Result<String, ToolError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(ToolError("browser: `url` must not be empty".to_string()));
    }
    if trimmed.contains("://")
        || trimmed.starts_with("about:")
        || trimmed.starts_with("data:")
        || trimmed.starts_with("file:")
    {
        return Ok(trimmed.to_string());
    }
    Ok(format!("https://{trimmed}"))
}

/// 截图落盘提示：成功给路径与大小，失败只给原因（截图本身仍会作为附件回给模型）。
fn save_screenshot(bytes: &[u8]) -> String {
    match output_files::write_output_file("browser", ".png", bytes) {
        Ok(path) => format!(
            "[Screenshot saved to {} (image/png, {})]",
            path.display(),
            format_size(bytes.len())
        ),
        Err(error) => format!("[Screenshot could not be saved: {error}]"),
    }
}

/// 压成一行并截断（标题 / URL 展示用）。
fn one_line(text: &str, max: usize) -> String {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars_with_postfix(&collapsed, max, "…")
}

/// 可选非负整数参数；给了但类型不对时报错。
fn optional_usize(args: &Value, key: &str) -> Result<Option<usize>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(|v| Some(v as usize))
            .ok_or_else(|| ToolError(format!("browser: `{key}` must be a non-negative integer"))),
    }
}

/// 可选布尔参数；给了但类型不对时报错。
fn optional_bool(args: &Value, key: &str) -> Result<Option<bool>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        Some(_) => Err(ToolError(format!("browser: `{key}` must be true or false"))),
    }
}

/// 可选数值参数；给了但类型不对时报错。
fn optional_f64(args: &Value, key: &str) -> Result<Option<f64>, ToolError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(Some)
            .ok_or_else(|| ToolError(format!("browser: `{key}` must be a finite number"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 具名键给出正确的 CDP 参数（Enter 带 `\r`，Esc 无文本）。
    #[test]
    fn key_spec_maps_named_keys() {
        let enter = key_spec("Enter").unwrap();
        assert_eq!(
            (enter.key.as_str(), enter.code.as_str(), enter.vk),
            ("Enter", "Enter", 13)
        );
        assert_eq!(enter.text.as_deref(), Some("\r"));

        let escape = key_spec("Esc").unwrap();
        assert_eq!(
            (escape.key.as_str(), escape.code.as_str(), escape.vk),
            ("Escape", "Escape", 27)
        );
        assert_eq!(escape.text, None);

        let down = key_spec("ArrowDown").unwrap();
        assert_eq!((down.key.as_str(), down.vk), ("ArrowDown", 40));
    }

    /// 单字符键映射到 `KeyA` / `Digit1`，多字符与非 ASCII 拒绝。
    #[test]
    fn key_spec_maps_single_characters() {
        let a = key_spec("a").unwrap();
        assert_eq!((a.key.as_str(), a.code.as_str(), a.vk), ("a", "KeyA", 65));
        assert_eq!(a.text.as_deref(), Some("a"));

        let one = key_spec("1").unwrap();
        assert_eq!((one.code.as_str(), one.vk), ("Digit1", 49));

        assert!(key_spec("ab").is_none());
        assert!(key_spec("中").is_none());
        assert!(key_spec("").is_none());
    }

    /// URL 补全：无 scheme 时按 https 处理，特殊 scheme 原样保留。
    #[test]
    fn normalize_url_adds_https_only_when_missing() {
        assert_eq!(
            normalize_url("example.com/a").unwrap(),
            "https://example.com/a"
        );
        assert_eq!(
            normalize_url("http://example.com").unwrap(),
            "http://example.com"
        );
        assert_eq!(normalize_url("about:blank").unwrap(), "about:blank");
        assert_eq!(
            normalize_url("file:///tmp/x.html").unwrap(),
            "file:///tmp/x.html"
        );
        assert!(normalize_url("   ").is_err());
    }

    /// 可选参数的类型校验：类型不对必须报错，而不是静默用默认值。
    #[test]
    fn optional_arguments_reject_wrong_types() {
        let args = json!({ "n": 3, "b": true, "f": 1.5, "s": "x" });
        assert_eq!(optional_usize(&args, "n").unwrap(), Some(3));
        assert_eq!(optional_bool(&args, "b").unwrap(), Some(true));
        assert_eq!(optional_f64(&args, "f").unwrap(), Some(1.5));
        assert_eq!(optional_usize(&args, "missing").unwrap(), None);

        assert!(optional_usize(&args, "s").is_err());
        assert!(optional_bool(&args, "s").is_err());
        assert!(optional_f64(&args, "s").is_err());
    }

    /// 元素参数：`ref` 优先，`selector` 次之，都没有则报错。
    #[test]
    fn element_argument_requires_a_locator() {
        let with_ref = json!({ "ref": "e3" });
        assert!(element_arg(&with_ref).unwrap().contains("e3"));
        assert_eq!(target_description(&with_ref), "ref e3");

        let with_selector = json!({ "selector": "#go" });
        assert!(element_arg(&with_selector).unwrap().contains("#go"));
        assert_eq!(target_description(&with_selector), "selector `#go`");

        assert!(element_arg(&json!({})).is_err());
        assert!(!has_element_arg(&json!({})));
    }

    /// 未知动作的报错里列出全部合法动作，且**不碰网络**（校验先于连接）。
    #[test]
    fn unknown_action_lists_the_supported_ones() {
        let error = validate_action(&json!({ "action": "nope" })).unwrap_err();
        assert!(error.0.contains("unknown action `nope`"), "{}", error.0);
        assert!(error.0.contains("screenshot"), "{}", error.0);
    }

    /// 合法动作原样通过；缺 `action` 时给出通用参数报错。
    #[test]
    fn validate_action_accepts_every_listed_action() {
        for action in ACTIONS {
            assert_eq!(
                validate_action(&json!({ "action": action })).unwrap(),
                action
            );
        }
        assert!(
            validate_action(&json!({}))
                .unwrap_err()
                .0
                .contains("action")
        );
        assert!(validate_action(&json!({ "action": "  " })).is_err());
    }
}

//! 工具实现：14 个 `computer_*` 工具的执行体。
//!
//! 阻塞动作（截图、窗口枚举、输入注入、剪贴板命令）全部放进 `spawn_blocking`，
//! 不在异步运行时的工作线程上做；每个动作执行前都重新读一遍当前窗口，
//! 而不是相信上次观察的结果。

use crate::{
    core::tools::{ToolError, ToolResult, ToolResultAttachment},
    extensions::{
        computer::{a11y, backend, input, schema, state},
        util::{attachment, opt_bool, opt_f64, opt_i64, opt_str},
    },
    utils::{clipboard, output_files, process},
};
use a11y::{A11yNode, A11yOptions, NodeId};
use backend::{CaptureRequest, ShotFormat};
use schema::UI_ACTIONS;
use serde_json::Value;
use std::{
    io::{Read, Seek, SeekFrom},
    time::Duration,
};

/// 启动日志尾部最多保留的字符数（够看清 chromium 之类的启动告警，又不至于塞满上下文）。
const LAUNCH_LOG_TAIL_CHARS: usize = 600;

/// 把阻塞动作丢到阻塞线程池并统一错误类型。
async fn blocking<T, F>(f: F) -> Result<T, ToolError>
where
    F: FnOnce() -> Result<T, String> + Send + 'static,
    T: Send + 'static,
{
    process::spawn_blocking(f)
        .await
        .map_err(|e| ToolError(format!("computer: worker task failed: {e}")))?
        .map_err(ToolError)
}

/// 读一个整数参数。
fn arg_i64(args: &Value, key: &str) -> Result<i64, ToolError> {
    args.get(key).and_then(Value::as_i64).ok_or_else(|| {
        ToolError(format!(
            "computer: `{key}` is required and must be an integer"
        ))
    })
}

/// 读一个必填字符串参数。
fn arg_str<'a>(args: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    opt_str(args, key).ok_or_else(|| {
        ToolError(format!(
            "computer: `{key}` is required and must be a non-empty string"
        ))
    })
}

/// 把图像字节落盘并生成 `file:` 行。
fn save_image(bytes: &[u8], format: ShotFormat) -> String {
    let ext = match format {
        backend::ShotFormat::Png => ".png",
        backend::ShotFormat::Jpeg => ".jpg",
    };
    match output_files::write_output_file("computer", ext, bytes) {
        Ok(path) => path.display().to_string(),
        Err(error) => format!("(could not be saved: {error})"),
    }
}

/// 解析坐标：`from_frame` 时按帧投影，否则按屏幕像素。
async fn resolve_point(args: &Value, x_key: &str, y_key: &str) -> Result<(i32, i32), ToolError> {
    let x = args
        .get(x_key)
        .and_then(Value::as_f64)
        .ok_or_else(|| ToolError(format!("computer: `{x_key}` is required")))?;
    let y = args
        .get(y_key)
        .and_then(Value::as_f64)
        .ok_or_else(|| ToolError(format!("computer: `{y_key}` is required")))?;
    let Some(frame_id) = opt_str(args, "from_frame") else {
        return Ok((x.round() as i32, y.round() as i32));
    };
    let frame = state::frame(frame_id).ok_or_else(|| {
        ToolError(format!(
            "[EXPIRED_FRAME] frame {frame_id:?} is not in this session (it may have been evicted); take a new screenshot and use the new frame id"
        ))
    })?;

    if let (Some(window_id), Some(recorded)) = (frame.window, frame.window_rect) {
        let current = blocking(move || backend::window(window_id)).await?;
        let now = current.rect();
        let moved = (now.x - recorded.x).abs() > 4
            || (now.y - recorded.y).abs() > 4
            || now.width.abs_diff(recorded.width) > 4
            || now.height.abs_diff(recorded.height) > 4;

        if moved {
            return Err(ToolError(format!(
                "[FRAME_STALE] window {window_id} moved or resized since frame {frame_id} was captured \
                 (was {},{} {}x{}, now {},{} {}x{}); take a new screenshot and use the new frame",
                recorded.x,
                recorded.y,
                recorded.width,
                recorded.height,
                now.x,
                now.y,
                now.width,
                now.height
            )));
        }
    }
    frame.project(x, y).map_err(ToolError)
}

/// 截图并落盘、登记帧，返回回执文本与附件。
async fn capture_and_describe(
    req: CaptureRequest,
) -> Result<(String, Option<ToolResultAttachment>), ToolError> {
    let scale = req.scale;
    let window_id = match &req.target {
        backend::CaptureTarget::Window(id) => Some(*id),
        _ => None,
    };
    let shot = blocking(move || backend::capture(&req)).await?;
    let path = save_image(&shot.bytes, shot.format);
    let screen_w = ((shot.width as f64) / scale.max(0.0001)).round().max(1.0) as u32;
    let screen_h = ((shot.height as f64) / scale.max(0.0001)).round().max(1.0) as u32;
    let window = match window_id {
        Some(id) => blocking(move || backend::window(id))
            .await
            .ok()
            .map(|w| (w.id, w.rect())),
        None => None,
    };
    let frame = state::register_frame(
        shot.origin,
        (screen_w, screen_h),
        (shot.width, shot.height),
        window,
    );
    let windows = blocking(backend::windows).await.unwrap_or_default();
    let focused = windows.iter().find(|w| w.focused);
    let displays = blocking(backend::displays).await.unwrap_or_default();

    let mut text = String::from("<screen_capture>\n");
    text.push_str(&format!("frame: {frame}\n"));
    text.push_str(&format!("file: {path}\n"));
    text.push_str(&format!(
        "image: {}x{} px ({}) captured at ({}, {}) of the desktop\n",
        shot.width,
        shot.height,
        match shot.format {
            ShotFormat::Png => "png",
            ShotFormat::Jpeg => "jpeg",
        },
        shot.origin.0,
        shot.origin.1
    ));

    if scale != 1.0 {
        text.push_str(&format!(
            "scale: image pixels are {scale}x the screen pixels of the captured region; to click something measured in this image, pass x,y with from_frame: \"{frame}\" (or divide by {scale})\n"
        ));
    } else {
        text.push_str("scale: image pixels are screen pixels (1:1)\n");
    }
    text.push_str(&format!("capture: {}\n", shot.display));

    if let Some(window) = window_id
        && let Ok(info) = blocking(move || backend::window(window)).await
    {
        text.push_str(&format!(
            "source: window {} {:?} — {}\n",
            info.id, info.app, info.title
        ));
    }

    match backend::cursor() {
        Some((x, y)) => text.push_str(&format!("cursor: ({x}, {y})\n")),
        None => {
            text.push_str("cursor: unknown (this backend cannot report the pointer position)\n")
        }
    }

    if let Some(w) = focused {
        text.push_str(&format!("focused window: {}\n", w.render()));
    }
    text.push_str("displays:\n");

    for line in backend::render_displays(&displays).lines() {
        text.push_str(&format!("  {line}\n"));
    }

    text.push_str(&format!(
        "coordinates: every pixel coordinate in the computer_* tools is an absolute desktop pixel in this same unit. Pass from_frame: \"{frame}\" to give a position measured inside this image and let the projection be done for you.\n</screen_capture>"
    ));
    Ok((text, attachment(&shot.bytes, shot.format.mime())))
}

/// `computer_screenshot`。
pub async fn screenshot(args: &Value) -> Result<ToolResult, ToolError> {
    let target = match opt_i64(args, "window") {
        Some(id) => backend::CaptureTarget::Window(id as u64),
        None => match args.get("region").and_then(Value::as_array) {
            Some(values) if values.len() == 4 => {
                let nums: Vec<i64> = values.iter().filter_map(Value::as_i64).collect();
                if nums.len() != 4 {
                    return Err(ToolError(
                        "computer: `region` must be [x, y, width, height] as integers".to_string(),
                    ));
                }
                backend::CaptureTarget::Region(state::Rect {
                    x: nums[0] as i32,
                    y: nums[1] as i32,
                    width: nums[2].max(1) as u32,
                    height: nums[3].max(1) as u32,
                })
            }
            Some(_) => {
                return Err(ToolError(
                    "computer: `region` must be [x, y, width, height] as integers".to_string(),
                ));
            }
            None => match opt_i64(args, "display") {
                Some(index) => {
                    let displays = blocking(backend::displays).await?;
                    let display = displays.get(index.max(0) as usize).ok_or_else(|| {
                        ToolError(format!(
                            "computer: display {index} does not exist; available: {}",
                            backend::render_displays(&displays)
                        ))
                    })?;
                    backend::CaptureTarget::Display(display.name.clone())
                }
                None => backend::CaptureTarget::All,
            },
        },
    };

    let req = CaptureRequest {
        target,
        scale: opt_f64(args, "scale").unwrap_or(1.0).clamp(0.05, 4.0),
        format: opt_str(args, "format")
            .and_then(ShotFormat::parse)
            .unwrap_or(ShotFormat::Jpeg),
        jpeg_quality: 80,
        cursor: false,
    };
    let (text, attachment) = capture_and_describe(req.clone()).await?;

    if let Some(path) = opt_str(args, "save_path") {
        let shot = blocking(move || backend::capture(&req)).await?;
        std::fs::write(path, &shot.bytes)
            .map_err(|e| ToolError(format!("computer: could not write {path:?}: {e}")))?;
    }

    let mut out = ToolResult::text(text);
    if let Some(attachment) = attachment {
        out = out.with_attachment(attachment);
    }
    Ok(out)
}

/// `computer_click`。
pub async fn click(args: &Value) -> Result<ToolResult, ToolError> {
    let (x, y) = resolve_point(args, "x", "y").await?;
    let button = opt_str(args, "button").unwrap_or("left").to_string();
    let clicks = opt_i64(args, "clicks").unwrap_or(1).clamp(1, 3) as u32;
    let modifiers = opt_str(args, "modifiers").unwrap_or("").to_string();
    let (bx, by, b, c, m) = (x, y, button.clone(), clicks, modifiers.clone());

    blocking(move || input::click(bx, by, &b, c, &m)).await?;

    Ok(ToolResult::text(format!(
        "clicked {button} x{clicks} at ({x}, {y}){}",
        if modifiers.is_empty() {
            String::new()
        } else {
            format!(" with {modifiers}")
        }
    )))
}

/// `computer_drag`。
pub async fn drag(args: &Value) -> Result<ToolResult, ToolError> {
    let modifiers = opt_str(args, "modifiers").unwrap_or("").to_string();
    let button = opt_str(args, "button").unwrap_or("left").to_string();
    let (from, to, path) = match args.get("path").and_then(Value::as_array) {
        Some(points) if !points.is_empty() => {
            let mut path = Vec::new();
            for point in points {
                let x = point.get("x").and_then(Value::as_f64).ok_or_else(|| {
                    ToolError("computer: each path point needs x and y".to_string())
                })?;
                let y = point.get("y").and_then(Value::as_f64).ok_or_else(|| {
                    ToolError("computer: each path point needs x and y".to_string())
                })?;
                path.push((x.round() as i32, y.round() as i32));
            }
            let from = *path.first().unwrap();
            let to = *path.last().unwrap();
            (from, to, path)
        }
        _ => {
            let from = match opt_str(args, "continue_from") {
                Some(handle) => state::drag_anchor(Some(handle)).map_err(ToolError)?,
                None if opt_bool(args, "from_last") == Some(true) => {
                    state::drag_anchor(None).map_err(ToolError)?
                }
                None => (
                    arg_i64(args, "from_x")? as i32,
                    arg_i64(args, "from_y")? as i32,
                ),
            };
            let to = (arg_i64(args, "to_x")? as i32, arg_i64(args, "to_y")? as i32);
            (from, to, Vec::new())
        }
    };

    let (fx, fy) = from;
    let (tx, ty) = to;
    let (p, b, m) = (path.clone(), button.clone(), modifiers.clone());
    blocking(move || input::drag((fx, fy), (tx, ty), &p, &b, &m)).await?;

    let handle = state::remember_drag(to);
    Ok(ToolResult::text(format!(
        "dragged {button} from ({}, {}) to ({}, {}){}; handle: {handle}",
        from.0,
        from.1,
        to.0,
        to.1,
        if modifiers.is_empty() {
            String::new()
        } else {
            format!(" with {modifiers}")
        }
    )))
}

/// `computer_scroll`。
pub async fn scroll(args: &Value) -> Result<ToolResult, ToolError> {
    let direction = arg_str(args, "direction")?;
    let clicks = opt_i64(args, "clicks").unwrap_or(3).clamp(1, 20) as i32;
    let (vertical, horizontal) = match direction {
        "up" => (-clicks, 0),
        "down" => (clicks, 0),
        "left" => (0, -clicks),
        "right" => (0, clicks),
        other => {
            return Err(ToolError(format!(
                "computer: `direction` must be up, down, left or right (got {other:?})"
            )));
        }
    };

    if opt_i64(args, "x").is_some() || opt_i64(args, "y").is_some() {
        let (x, y) = resolve_point(args, "x", "y").await?;
        blocking(move || input::move_to(x, y)).await?;
    }

    blocking(move || input::scroll(vertical, horizontal)).await?;

    Ok(ToolResult::text(format!(
        "scrolled {direction} {clicks} notch(es)"
    )))
}

/// 校验 `into_window`：要求那个窗口当前持有焦点。
async fn require_focus(window: Option<i64>) -> Result<(), ToolError> {
    let Some(wanted) = window else {
        return Ok(());
    };
    let windows = blocking(backend::windows).await?;
    let focused = windows.iter().find(|w| w.focused);
    match focused {
        Some(w) if w.id == wanted as u64 => Ok(()),
        Some(w) => Err(ToolError(format!(
            "[FRONT_NOT_TARGET] the keyboard focus is window {} ({:?}), not {wanted}; focus the target first (computer_windows with focus), then retry",
            w.id, w.title
        ))),
        None => Err(ToolError(
            "[FOREGROUND_REQUIRED] no window holds the keyboard focus; focus the target first"
                .to_string(),
        )),
    }
}

/// `computer_type`。
pub async fn type_text(args: &Value) -> Result<ToolResult, ToolError> {
    let text = arg_str(args, "text")?.to_string();
    require_focus(opt_i64(args, "into_window")).await?;
    let len = text.chars().count();
    let payload = text.clone();
    blocking(move || input::type_text(&payload)).await?;
    Ok(ToolResult::text(format!("typed {len} character(s)")))
}

/// `computer_key`。
pub async fn key(args: &Value) -> Result<ToolResult, ToolError> {
    let chord = arg_str(args, "keys")?.to_string();
    require_focus(opt_i64(args, "into_window")).await?;
    let repeat = opt_i64(args, "repeat").unwrap_or(1).clamp(1, 100) as u32;
    let hold = opt_f64(args, "hold_seconds").unwrap_or(0.0);
    let payload = chord.clone();
    blocking(move || input::press_chord(&payload, repeat, hold)).await?;
    Ok(ToolResult::text(format!("pressed {chord}")))
}

/// `computer_windows`。
pub async fn windows(args: &Value) -> Result<ToolResult, ToolError> {
    if let Some(id) = opt_i64(args, "focus") {
        let id = id as u64;
        blocking(move || backend::focus_window(id)).await?;
        let info = blocking(move || backend::window(id)).await?;
        return Ok(ToolResult::text(format!("focused {}", info.render())));
    }

    let filter = opt_str(args, "filter").unwrap_or("").to_string();
    let list = blocking(backend::windows).await?;

    let matched = backend::find_window(&list, &filter);
    if matched.is_empty() {
        return Ok(ToolResult::text(format!(
            "no window matches {filter:?}; {} window(s) are visible:\n{}",
            list.len(),
            backend::render_windows(&list)
        )));
    }

    let mut text = format!("{} window(s):\n", matched.len());
    for window in matched {
        text.push_str(&window.render());
        text.push('\n');
    }
    Ok(ToolResult::text(text))
}

/// `computer_clipboard`。
pub async fn clipboard(args: &Value) -> Result<ToolResult, ToolError> {
    let op = arg_str(args, "op")?;
    match op {
        "get" => {
            let text = clipboard::read_clipboard().unwrap_or_default();
            Ok(ToolResult::text(format!(
                "clipboard ({} character(s)):\n{}",
                text.chars().count(),
                text
            )))
        }
        "set" => {
            let text = arg_str(args, "text")?.to_string();
            clipboard::write_clipboard(&text).map_err(ToolError)?;
            Ok(ToolResult::text(format!(
                "clipboard set ({} character(s))",
                text.chars().count()
            )))
        }
        "clear" => {
            clipboard::write_clipboard("").map_err(ToolError)?;
            Ok(ToolResult::text("clipboard cleared".to_string()))
        }
        other => Err(ToolError(format!(
            "computer: `op` must be get, set or clear (got {other:?})"
        ))),
    }
}

/// `computer_paste`。
pub async fn paste(args: &Value) -> Result<ToolResult, ToolError> {
    let text = arg_str(args, "text")?.to_string();
    let restore = opt_bool(args, "restore").unwrap_or(true);
    let require_restore = opt_bool(args, "require_restore").unwrap_or(false);
    let previous = clipboard::read_clipboard();

    if require_restore && restore && previous.is_none() {
        return Err(ToolError(
            "computer: the previous clipboard is not readable text, so it cannot be restored; pass require_restore: false to paste anyway".to_string(),
        ));
    }

    clipboard::write_clipboard(&text).map_err(ToolError)?;
    let typed = blocking(|| input::press_chord("ctrl+v", 1, 0.0)).await;
    let restore_status = if restore {
        match &previous {
            Some(old) => match clipboard::write_clipboard(old) {
                Ok(()) => "restored".to_string(),
                Err(e) => format!("restore failed: {e}"),
            },
            None => match clipboard::write_clipboard("") {
                Ok(()) => "cleared (the previous content was not text)".to_string(),
                Err(e) => format!("restore failed: {e}"),
            },
        }
    } else {
        "kept (restore: false)".to_string()
    };

    typed?;
    Ok(ToolResult::text(format!(
        "pasted {} character(s); clipboard {restore_status}",
        text.chars().count()
    )))
}

/// `computer_launch`。
pub async fn launch(args: &Value) -> Result<ToolResult, ToolError> {
    let target = arg_str(args, "target")?.to_string();
    let arguments: Vec<String> = args
        .get("arguments")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let wait_ms = (opt_f64(args, "wait_seconds")
        .unwrap_or(8.0)
        .clamp(0.5, 30.0)
        * 1000.0) as u64;
    let (t, a) = (target.clone(), arguments.clone());
    let launched = blocking(move || backend::launch(&t, &a, wait_ms)).await?;
    let log_note = match launched.log.as_deref() {
        Some(path) => format!("log: {path}"),
        None => "log: (could not be created, output was discarded)".to_string(),
    };

    let text = match launched.window {
        Some(window) => format!(
            "started {target:?} (pid {}); window: {}\n{log_note}",
            launched.pid,
            window.render()
        ),
        None => {
            let mut text = format!(
                "started {target:?} (pid {}), but no window appeared within {} s; it may still be starting (check computer_windows)\n{log_note}",
                launched.pid,
                wait_ms / 1000
            );
            if let Some(tail) = launch_log_tail(launched.log.as_deref()) {
                text.push_str(&format!("\noutput so far:\n{tail}"));
            }
            text
        }
    };
    Ok(ToolResult::text(text))
}

/// 读启动日志的尾部；`path` 为 `None`、文件读不出来或内容为空时返回 `None`。
fn launch_log_tail(path: Option<&str>) -> Option<String> {
    let mut file = std::fs::File::open(path?).ok()?;
    let len = file.metadata().ok()?.len();

    // 按 4 字节/字符预留，避免多字节字符被从中间切开后 `from_utf8_lossy` 又多丢一段。
    let window = (LAUNCH_LOG_TAIL_CHARS * 4) as u64;
    if len > window {
        file.seek(SeekFrom::Start(len - window)).ok()?;
    }

    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let tail: String = text
        .chars()
        .rev()
        .take(LAUNCH_LOG_TAIL_CHARS)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();

    let tail = tail.trim();
    if tail.is_empty() {
        None
    } else {
        Some(tail.to_string())
    }
}

/// `computer_stop`。
pub async fn stop(args: &Value) -> Result<ToolResult, ToolError> {
    let release = opt_bool(args, "release_input").unwrap_or(true);
    let released = if release {
        blocking(input::release_all).await.unwrap_or_default()
    } else {
        Vec::new()
    };

    let text = if released.is_empty() {
        "nothing was held down".to_string()
    } else {
        format!("released: {}", released.join(", "))
    };
    Ok(ToolResult::text(text))
}

/// 从参数里读无障碍选项。
fn a11y_options(args: &Value) -> A11yOptions {
    let mut options = A11yOptions::default();
    if let Some(depth) = opt_i64(args, "max_depth") {
        options.max_depth = depth.clamp(1, 10) as usize;
    }
    if let Some(nodes) = opt_i64(args, "max_nodes") {
        options.max_nodes = nodes.clamp(10, 1500) as usize;
    }
    options.max_visited = opt_i64(args, "max_visited")
        .map(|v| {
            if v <= 0 {
                options.max_nodes.saturating_mul(200)
            } else {
                v as usize
            }
        })
        .unwrap_or_else(|| options.max_nodes.saturating_mul(40));
    options.patterns = opt_bool(args, "patterns").unwrap_or(true);
    options.skip_empty = opt_bool(args, "skip_empty").unwrap_or(true);
    options
}

/// 过滤树：保留匹配节点及其祖先（缩进因此仍然成立）。
fn filter_nodes(nodes: &[A11yNode], needle: &str) -> Vec<A11yNode> {
    let needle = needle.to_lowercase();
    if needle.is_empty() {
        return nodes.to_vec();
    }

    // 先标记匹配项，再把它们的祖先一起留下。
    let mut keep = vec![false; nodes.len()];
    for (index, node) in nodes.iter().enumerate() {
        let hit = node.name.to_lowercase().contains(&needle)
            || node.role.to_lowercase().contains(&needle)
            || node.automation_id.to_lowercase().contains(&needle);
        if !hit {
            continue;
        }
        keep[index] = true;
        let mut depth = node.depth;
        let mut cursor = index;
        while depth > 0 && cursor > 0 {
            cursor -= 1;
            if nodes[cursor].depth < depth {
                depth = nodes[cursor].depth;
                keep[cursor] = true;
            }
        }
    }

    nodes
        .iter()
        .zip(keep)
        .filter(|(_, kept)| *kept)
        .map(|(node, _)| node.clone())
        .collect()
}

/// `computer_ui`。
pub async fn ui(args: &Value) -> Result<ToolResult, ToolError> {
    let window = opt_i64(args, "window").map(|v| v as u64);
    let options = a11y_options(args);
    let (nodes, ids) = a11y::read_tree(window, options.clone())
        .await
        .map_err(ToolError)?;
    let prefix = a11y::install_snapshot(ids);
    let nodes = filter_nodes(&nodes, opt_str(args, "filter").unwrap_or(""));
    let compact = opt_bool(args, "compact").unwrap_or(true);
    let mut text = format!(
        "{} node(s){}:\n{}",
        nodes.len(),
        match window {
            Some(id) => format!(" in window {id}"),
            None => " in the focused window".to_string(),
        },
        a11y::render(&nodes, &prefix, compact)
    );

    if opt_bool(args, "screenshot") == Some(true) {
        let req = CaptureRequest {
            target: window.map_or(backend::CaptureTarget::All, backend::CaptureTarget::Window),
            scale: 1.0,
            format: backend::ShotFormat::Jpeg,
            jpeg_quality: 80,
            cursor: false,
        };
        let (shot_text, shot_attachment) = capture_and_describe(req).await?;
        text.push_str("\n\n");
        text.push_str(&shot_text);

        let mut out = ToolResult::text(text);
        if let Some(attachment) = shot_attachment {
            out = out.with_attachment(attachment);
        }
        return Ok(out);
    }
    Ok(ToolResult::text(text))
}

/// 找节点的第一条可用动作（`invoke` 用）。
async fn first_action(target: &NodeId) -> Result<(), String> {
    for name in ["click", "press", "activate", "invoke", "toggle"] {
        match a11y::action_by_name(target, name).await {
            Ok(()) => return Ok(()),
            Err(_) => continue,
        }
    }
    a11y::do_action(target, 0).await
}

/// 在树里按文本找一个节点（`click_text` / `locate_text`）。
async fn find_node_by_text(window: Option<u64>, needle: &str) -> Result<NodeId, String> {
    let options = A11yOptions {
        max_nodes: 800,
        max_visited: 20_000,
        patterns: false,
        ..A11yOptions::default()
    };
    let (nodes, ids) = a11y::read_tree(window, options).await?;
    let lowered = needle.to_lowercase();
    nodes
        .iter()
        .position(|node| node.name.to_lowercase().contains(&lowered))
        .and_then(|index| ids.get(index).cloned())
        .ok_or_else(|| {
            format!("no node whose name contains {needle:?} in that window's accessibility tree")
        })
}

/// `computer_ui_action`。
pub async fn ui_action(args: &Value) -> Result<ToolResult, ToolError> {
    let action = arg_str(args, "action")?.to_string();
    if !UI_ACTIONS.contains(&action.as_str()) {
        return Err(ToolError(format!(
            "computer: unknown action {action:?}; one of: {}",
            UI_ACTIONS.join(", ")
        )));
    }
    let window = opt_i64(args, "window").map(|v| v as u64);
    let target = if let Some(reference) = opt_str(args, "ref") {
        Some(a11y::resolve_ref(reference).map_err(ToolError)?)
    } else if matches!(action.as_str(), "click_text" | "locate_text") {
        let needle = arg_str(args, "name")?;
        Some(find_node_by_text(window, needle).await.map_err(ToolError)?)
    } else {
        None
    };

    let text = match action.as_str() {
        "invoke" => {
            let node = require_ref(&target, &action)?;
            first_action(node).await.map_err(ToolError)?;
            format!("invoked {}", describe(node))
        }
        "set_value" => {
            let node = require_ref(&target, &action)?;
            let value = arg_str(args, "text")?;
            a11y::set_text(node, value).await.map_err(ToolError)?;
            format!("set {} on {}", value.chars().count(), describe(node))
        }
        "get_value" => {
            let node = require_ref(&target, &action)?;
            let value = a11y::read_value(node).await.map_err(ToolError)?;
            format!("{} = {value:?}", describe(node))
        }
        "get_text" => {
            let node = require_ref(&target, &action)?;
            let value = a11y::read_text(node).await.map_err(ToolError)?;
            format!(
                "{} text ({} character(s)):\n{value}",
                describe(node),
                value.chars().count()
            )
        }
        "select_text" => {
            let node = require_ref(&target, &action)?;
            let start = opt_i64(args, "start").unwrap_or(0) as i32;
            let end = opt_i64(args, "end").unwrap_or(-1) as i32;
            a11y::select_text(node, start, end)
                .await
                .map_err(ToolError)?;
            format!("selected {start}..{end} in {}", describe(node))
        }
        "scroll_into_view" => {
            let node = require_ref(&target, &action)?;
            a11y::action_by_name(node, "scroll")
                .await
                .map_err(ToolError)?;
            format!("scrolled {} into view", describe(node))
        }
        "focus" => {
            let node = require_ref(&target, &action)?;
            a11y::grab_focus(node).await.map_err(ToolError)?;
            format!("focused {}", describe(node))
        }
        "toggle" => {
            let node = require_ref(&target, &action)?;
            a11y::action_by_name(node, "toggle")
                .await
                .map_err(ToolError)?;
            format!("toggled {}", describe(node))
        }
        "select" => {
            let node = require_ref(&target, &action)?;
            let index = opt_i64(args, "index").unwrap_or(0) as i32;
            a11y::select_child(node, index).await.map_err(ToolError)?;
            format!("selected child {index} of {}", describe(node))
        }
        "expand" | "collapse" => {
            let node = require_ref(&target, &action)?;
            a11y::action_by_name(node, &action)
                .await
                .map_err(ToolError)?;
            format!("{action}ed {}", describe(node))
        }
        "close" | "minimize" | "maximize" | "restore" => {
            let id = match (&target, window) {
                (Some(_), Some(id)) => id,
                (None, Some(id)) => id,
                _ => {
                    return Err(ToolError(
                        "computer: close/minimize/maximize/restore need a `window`".to_string(),
                    ));
                }
            };
            let name = action.clone();
            blocking(move || backend::window_action(id, &name)).await?;
            format!("{action}: window {id}")
        }
        "locate_text" => {
            let node = require_ref(&target, &action)?;
            let rect = a11y::node_rect(node).await.ok_or_else(|| {
                ToolError(format!("{} has no on-screen rectangle", describe(node)))
            })?;
            format!(
                "{} is at ({}, {}) {}x{}",
                describe(node),
                rect.x,
                rect.y,
                rect.width,
                rect.height
            )
        }
        "click_text" => {
            let node = require_ref(&target, &action)?;
            let rect = a11y::node_rect(node).await.ok_or_else(|| {
                ToolError(format!(
                    "{} has no on-screen rectangle to click",
                    describe(node)
                ))
            })?;
            let (x, y) = (
                rect.x + rect.width as i32 / 2,
                rect.y + rect.height as i32 / 2,
            );
            blocking(move || input::click(x, y, "left", 1, "")).await?;
            format!("clicked {} at ({x}, {y})", describe(node))
        }
        "get_caret" => {
            let node = require_ref(&target, &action)?;
            match a11y::caret_offset(node).await {
                Some(offset) => format!("{} caret offset {offset}", describe(node)),
                None => format!("{} does not report a caret", describe(node)),
            }
        }
        other => unreachable!("action {other} was validated above"),
    };
    Ok(ToolResult::text(text))
}

/// 要求给出 ref。
fn require_ref<'a>(target: &'a Option<NodeId>, action: &str) -> Result<&'a NodeId, ToolError> {
    target.as_ref().ok_or_else(|| {
        ToolError(format!(
            "computer: action {action:?} needs `ref` (a [s1.4] marker from computer_ui)"
        ))
    })
}

/// 节点的一句话描述。
fn describe(node: &NodeId) -> String {
    if node.name.is_empty() {
        format!("{} ({})", node.role, node.path)
    } else {
        format!("{} {:?}", node.role, node.name)
    }
}

/// `computer_actions`：顺序执行多步。
pub async fn batch(args: &Value) -> Result<ToolResult, ToolError> {
    let steps = args
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError("computer: `steps` must be a non-empty array".to_string()))?;
    if steps.is_empty() {
        return Err(ToolError("computer: `steps` is empty".to_string()));
    }

    let mut lines = Vec::new();
    for (index, step) in steps.iter().enumerate() {
        let action = step
            .get("action")
            .and_then(Value::as_str)
            .ok_or_else(|| ToolError(format!("computer: step {index} has no action")))?;
        let result = run_step(action, step).await.map_err(|e| {
            ToolError(format!(
                "step {index} ({action}) failed: {}; the {} earlier step(s) already happened",
                e.0, index
            ))
        })?;
        lines.push(format!("{}. {action}: {result}", index + 1));
    }

    if opt_bool(args, "screenshot") == Some(true) {
        let req = CaptureRequest {
            target: backend::CaptureTarget::All,
            scale: opt_f64(args, "scale").unwrap_or(1.0).clamp(0.05, 4.0),
            format: backend::ShotFormat::Jpeg,
            jpeg_quality: 80,
            cursor: false,
        };

        let (text, attachment) = capture_and_describe(req).await?;
        let mut out = ToolResult::text(format!("{}\n\n{text}", lines.join("\n")));
        if let Some(attachment) = attachment {
            out = out.with_attachment(attachment);
        }
        return Ok(out);
    }
    Ok(ToolResult::text(lines.join("\n")))
}

/// 执行一步（batch 用）：把 step 当参数交给对应实现。
async fn run_step(action: &str, step: &Value) -> Result<String, ToolError> {
    match action {
        "click" => one_line(click(step).await),
        "move" => {
            let (x, y) = resolve_point(step, "x", "y").await?;
            blocking(move || input::move_to(x, y)).await?;
            Ok(format!("moved to ({x}, {y})"))
        }
        "drag" => one_line(drag(step).await),
        "scroll" => one_line(scroll(step).await),
        "type" => one_line(type_text(step).await),
        "key" => one_line(key(step).await),
        "wait" => {
            let seconds = opt_f64(step, "seconds").unwrap_or(1.0).clamp(0.0, 10.0);
            tokio::time::sleep(Duration::from_secs_f64(seconds)).await;
            Ok(format!("waited {seconds} s"))
        }
        "screenshot" => {
            let out = screenshot(step).await?;
            Ok(out.text.lines().next().unwrap_or("captured").to_string())
        }
        "paste" => one_line(paste(step).await),
        "focus" => {
            let id = arg_i64(step, "window")? as u64;
            blocking(move || backend::focus_window(id)).await?;
            Ok(format!("focused window {id}"))
        }
        "clipboard" => one_line(clipboard(step).await),
        "locate_text" | "click_text" => {
            // batch 里的文本定位：先在当前焦点窗口里找，再点/报坐标。
            let needle = arg_str(step, "name")?.to_string();
            ui_action(step)
                .await
                .map(|r| r.text)
                .map_err(|e| ToolError(format!("{e} (looked for {needle:?})")))
        }
        other => {
            if UI_ACTIONS.contains(&other) {
                ui_action(step).await.map(|r| r.text)
            } else {
                Err(ToolError(format!(
                    "computer: unknown step action {other:?}"
                )))
            }
        }
    }
}

/// 取工具结果的最后一行，作为 batch 的简短回执。
fn one_line(result: Result<ToolResult, ToolError>) -> Result<String, ToolError> {
    result.map(|out| out.text.lines().next().unwrap_or("").to_string())
}

/// 按工具名分发到实现。
pub async fn dispatch(name: &str, args: &Value) -> Result<ToolResult, ToolError> {
    match name {
        "computer_screenshot" => screenshot(args).await,
        "computer_actions" => batch(args).await,
        "computer_click" => click(args).await,
        "computer_drag" => drag(args).await,
        "computer_scroll" => scroll(args).await,
        "computer_type" => type_text(args).await,
        "computer_key" => key(args).await,
        "computer_windows" => windows(args).await,
        "computer_ui" => ui(args).await,
        "computer_ui_action" => ui_action(args).await,
        "computer_clipboard" => clipboard(args).await,
        "computer_paste" => paste(args).await,
        "computer_launch" => launch(args).await,
        "computer_stop" => stop(args).await,
        other => Err(ToolError(format!("computer: unknown tool {other:?}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 日志尾部回执：只取末段，空文件与缺失路径都当没有内容。
    #[test]
    fn launch_log_tail_keeps_the_end_only() {
        let long = "x".repeat(LAUNCH_LOG_TAIL_CHARS * 2) + "the-reason";
        let path =
            output_files::write_output_file("computer-test", ".log", long.as_bytes()).unwrap();
        let tail = launch_log_tail(Some(&path.display().to_string())).unwrap();
        assert!(tail.ends_with("the-reason"));
        assert_eq!(tail.chars().count(), LAUNCH_LOG_TAIL_CHARS);

        let empty = output_files::write_output_file("computer-test", ".log", b"").unwrap();
        assert!(launch_log_tail(Some(&empty.display().to_string())).is_none());
        assert!(launch_log_tail(None).is_none());
    }
}

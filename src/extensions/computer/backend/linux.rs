//! Linux（Wayland）后端实现。
//!
//! - 截图：优先 `grim`（wlroots 的 screencopy 工具，同步、无授权弹窗），环境里没有时退回
//!   `xcap`（它自己走 screencopy/pipewire）。两条路都按**逻辑坐标**输出，与窗口矩形同一坐标系。
//! - 窗口：`swaymsg -t get_tree`（Wayland 协议不提供全局窗口枚举，这是 wlroots 系合成器给出的
//!   唯一可靠来源；非 sway 会话会得到一条能读懂的错误，而不是空列表）。
//! - 输入：`enigo` 的 Wayland 后端（`zwp_virtual_keyboard_v1` + `zwlr_virtual_pointer_v1`）。
//! - 指针位置：Wayland 没有这个协议，`cursor()` 返回 `None`。

use super::{CaptureRequest, CaptureTarget, Display, Launched, Shot, ShotFormat, Window};
use crate::{
    extensions::computer::state::Rect,
    utils::{paths::find_in_path, process::run_stdout},
};
use serde_json::Value;
use std::{
    process::{Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};

/// `swaymsg` 找不到时的统一说明：Wayland 下窗口枚举没有平台中立来源。
const SWAY_HINT: &str = "window enumeration on Wayland needs `swaymsg` (sway/wlroots); this session has no usable sway IPC";

/// 后端是否可用：要求 Wayland 会话，且至少有一种截图手段。
pub fn available() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        && (has_grim() || xcap::Monitor::all().map(|m| !m.is_empty()).unwrap_or(false))
}

/// 后端描述（回执/诊断用）。
pub fn describe() -> String {
    let session = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        "wayland"
    } else {
        "no wayland socket"
    };
    let capture = if has_grim() { "grim" } else { "xcap" };
    let windows = if has_swaymsg() {
        "swaymsg"
    } else {
        "no sway IPC"
    };
    format!("linux {session}, capture: {capture}, windows: {windows}, input: enigo")
}

/// `grim` 是否在 PATH 上。
fn has_grim() -> bool {
    find_in_path("grim").is_some()
}

/// `swaymsg` 是否在 PATH 上。
fn has_swaymsg() -> bool {
    find_in_path("swaymsg").is_some()
}

/// 调 `swaymsg -t <kind> -r` 并把 JSON 解析出来。
fn sway_json(kind: &str) -> Result<Value, String> {
    let mut cmd = Command::new("swaymsg");
    cmd.args(["-t", kind, "-r"]);
    let bytes = run_stdout(cmd, &format!("swaymsg -t {kind}"))?;
    serde_json::from_slice(&bytes).map_err(|e| format!("swaymsg -t {kind}: invalid JSON: {e}"))
}

/// 列出显示器：从 sway 的 output 列表来（Wayland 没有等价的公开协议）。
pub fn displays() -> Result<Vec<Display>, String> {
    if !has_swaymsg() {
        return Err(SWAY_HINT.to_string());
    }

    let tree = sway_json("get_outputs")?;
    let mut out = Vec::new();
    for item in tree.as_array().into_iter().flatten() {
        let active = item.get("active").and_then(Value::as_bool).unwrap_or(true);
        let dpms = item.get("dpms").and_then(Value::as_bool).unwrap_or(true);
        if !active || !dpms {
            continue;
        }

        let rect = item.get("rect").unwrap_or(&Value::Null);
        let name = item
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("output")
            .to_string();
        out.push(Display {
            name,
            x: rect.get("x").and_then(Value::as_i64).unwrap_or(0) as i32,
            y: rect.get("y").and_then(Value::as_i64).unwrap_or(0) as i32,
            width: rect.get("width").and_then(Value::as_i64).unwrap_or(0) as u32,
            height: rect.get("height").and_then(Value::as_i64).unwrap_or(0) as u32,
            scale: item.get("scale").and_then(Value::as_f64).unwrap_or(1.0),
            primary: item
                .get("focused")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }

    if out.is_empty() {
        return Err("sway reported no active output".to_string());
    }

    if !out.iter().any(|d| d.primary) {
        out[0].primary = true;
    }
    Ok(out)
}

/// 递归收集 sway 树里的窗口节点。
fn collect_nodes(node: &Value, out: &mut Vec<Window>) {
    if node.get("type").and_then(Value::as_str) == Some("con")
        || node.get("type").and_then(Value::as_str) == Some("floating_con")
    {
        let visible = node
            .get("visible")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let rect = node.get("rect").unwrap_or(&Value::Null);
        let width = rect.get("width").and_then(Value::as_i64).unwrap_or(0);
        let height = rect.get("height").and_then(Value::as_i64).unwrap_or(0);
        let app_id = node
            .get("app_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .or_else(|| {
                node.get("window_properties")
                    .and_then(|p| p.get("class"))
                    .and_then(Value::as_str)
                    .map(str::to_string)
            });
        if visible
            && width > 0
            && height > 0
            && let Some(id) = node.get("id").and_then(Value::as_i64)
        {
            out.push(Window {
                id: id as u64,
                title: node
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                app: app_id.unwrap_or_default(),
                pid: node.get("pid").and_then(Value::as_i64).unwrap_or(0) as u32,
                x: rect.get("x").and_then(Value::as_i64).unwrap_or(0) as i32,
                y: rect.get("y").and_then(Value::as_i64).unwrap_or(0) as i32,
                width: width as u32,
                height: height as u32,
                focused: node
                    .get("focused")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
    }

    for key in ["nodes", "floating_nodes"] {
        for child in node
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            collect_nodes(child, out);
        }
    }
}

/// 列出可见顶层窗口。
pub fn windows() -> Result<Vec<Window>, String> {
    if !has_swaymsg() {
        return Err(SWAY_HINT.to_string());
    }

    let tree = sway_json("get_tree")?;
    let mut out = Vec::new();
    collect_nodes(&tree, &mut out);
    Ok(out)
}

/// 把窗口置前。
pub fn focus_window(id: u64) -> Result<(), String> {
    if !has_swaymsg() {
        return Err(SWAY_HINT.to_string());
    }

    let mut cmd = Command::new("swaymsg");
    cmd.args(["-q", &format!("[con_id={id}] focus")]);
    run_stdout(cmd, &format!("swaymsg focus {id}")).map(|_| ())
}

/// 指针位置：Wayland 不提供，返回 `None`。
pub fn cursor() -> Option<(i32, i32)> {
    None
}

/// 一次截图的几何计划：屏幕矩形 + 目标 output + 属于哪个窗口。
struct Plan {
    /// 屏幕矩形（逻辑坐标）。
    rect: Rect,
    /// 目标显示器名（整屏时为 `None`）。
    output: Option<String>,
    /// 窗口标识（窗口截图时）。
    window: Option<u64>,
}

/// 把请求翻译成截图几何；越界就裁剪并报告。
fn plan(req: &CaptureRequest) -> Result<Plan, String> {
    let list = displays()?;
    let x0 = list.iter().map(|d| d.x).min().unwrap_or(0);
    let y0 = list.iter().map(|d| d.y).min().unwrap_or(0);
    let x1 = list.iter().map(|d| d.x + d.width as i32).max().unwrap_or(0);
    let y1 = list
        .iter()
        .map(|d| d.y + d.height as i32)
        .max()
        .unwrap_or(0);
    let all = Rect {
        x: x0,
        y: y0,
        width: (x1 - x0).max(1) as u32,
        height: (y1 - y0).max(1) as u32,
    };
    let clip = |rect: Rect| -> Result<Rect, String> {
        let x = rect.x.max(all.x);
        let y = rect.y.max(all.y);
        let right = rect.right().min(all.right());
        let bottom = rect.bottom().min(all.bottom());
        if right <= x || bottom <= y {
            return Err(format!(
                "the requested region ({},{} {}x{}) is outside the virtual desktop ({}x{} at {},{})",
                rect.x, rect.y, rect.width, rect.height, all.width, all.height, all.x, all.y
            ));
        }
        Ok(Rect {
            x,
            y,
            width: (right - x) as u32,
            height: (bottom - y) as u32,
        })
    };
    match &req.target {
        CaptureTarget::All => Ok(Plan {
            rect: all,
            output: None,
            window: None,
        }),
        CaptureTarget::Display(name) => {
            let d = list.iter().find(|d| &d.name == name).ok_or_else(|| {
                format!(
                    "no display named {name:?}; available: {}",
                    super::render_displays(&list)
                )
            })?;
            Ok(Plan {
                rect: Rect {
                    x: d.x,
                    y: d.y,
                    width: d.width,
                    height: d.height,
                },
                output: Some(d.name.clone()),
                window: None,
            })
        }
        CaptureTarget::Region(rect) => Ok(Plan {
            rect: clip(*rect)?,
            output: None,
            window: None,
        }),
        CaptureTarget::Window(id) => {
            let w = super::window(*id)?;
            Ok(Plan {
                rect: clip(w.rect())?,
                output: None,
                window: Some(*id),
            })
        }
    }
}

/// 执行截图：grim 优先，缺失则 xcap。
pub fn capture(req: &CaptureRequest) -> Result<Shot, String> {
    let plan = plan(req)?;
    if has_grim() {
        grim_capture(req, &plan).or_else(|err| {
            xcap_capture(req, &plan)
                .map_err(|fallback| format!("{err}; xcap fallback also failed: {fallback}"))
        })
    } else {
        xcap_capture(req, &plan)
    }
}

/// 用 grim 截取计划里的矩形。
fn grim_capture(req: &CaptureRequest, plan: &Plan) -> Result<Shot, String> {
    let mut cmd = Command::new("grim");
    let scale = if req.scale > 0.0 { req.scale } else { 1.0 };
    cmd.arg("-s").arg(format!("{scale}"));
    cmd.arg("-g").arg(format!(
        "{},{} {}x{}",
        plan.rect.x, plan.rect.y, plan.rect.width, plan.rect.height
    ));
    match req.format {
        ShotFormat::Png => {
            cmd.args(["-t", "png"]);
        }
        ShotFormat::Jpeg => {
            cmd.args(["-t", "jpeg"]);
            cmd.args(["-q", &req.jpeg_quality.clamp(1, 100).to_string()]);
        }
    }
    if req.cursor {
        cmd.arg("-c");
    }
    if let Some(output) = &plan.output {
        cmd.args(["-o", output]);
    }
    cmd.arg("-");
    let child = cmd
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("grim: {e}"))?;
    let out = child.wait_with_output().map_err(|e| format!("grim: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(format!(
            "grim failed ({}): {}",
            out.status,
            if err.is_empty() {
                "no stderr (is this a wlroots compositor?)"
            } else {
                &err
            }
        ));
    }
    let (width, height) = image::ImageReader::new(std::io::Cursor::new(&out.stdout))
        .with_guessed_format()
        .map_err(|e| format!("grim returned an unreadable image: {e}"))?
        .into_dimensions()
        .map_err(|e| format!("grim returned an unreadable image: {e}"))?;
    Ok(Shot {
        bytes: out.stdout,
        format: req.format,
        width,
        height,
        origin: (plan.rect.x, plan.rect.y),
        display: plan
            .output
            .clone()
            .unwrap_or_else(|| "all outputs".to_string()),
    })
}

/// 用 xcap 截取计划里的矩形（grim 不可用时的兜底）。
fn xcap_capture(req: &CaptureRequest, plan: &Plan) -> Result<Shot, String> {
    let image = match plan.window {
        Some(id) => {
            let windows = xcap::Window::all().map_err(|e| format!("xcap: {e}"))?;
            let window = windows
                .into_iter()
                .find(|w| window_matches(w, id, &plan.rect))
                .ok_or_else(|| format!("xcap: window {id} is not capturable"))?;
            window
                .capture_image()
                .map_err(|e| format!("xcap window {id}: {e}"))?
        }
        None => {
            let monitors = xcap::Monitor::all().map_err(|e| format!("xcap: {e}"))?;
            let monitor = monitors
                .into_iter()
                .find(|m| monitor_matches(m, &plan.rect))
                .or_else(|| xcap::Monitor::from_point(plan.rect.x, plan.rect.y).ok())
                .ok_or_else(|| "xcap: no monitor covers the requested region".to_string())?;
            monitor
                .capture_image()
                .map_err(|e| format!("xcap monitor: {e}"))?
        }
    };
    // xcap 的像素按显示器的物理分辨率给出；先缩回逻辑尺寸，再按请求缩放。
    let (mw, mh) = (image.width(), image.height());
    let target_w = plan.rect.width;
    let target_h = plan.rect.height;
    let logical = if mw == target_w && mh == target_h {
        image
    } else {
        image::imageops::resize(
            &image,
            target_w.max(1),
            target_h.max(1),
            image::imageops::FilterType::Triangle,
        )
    };
    let scale = if req.scale > 0.0 { req.scale } else { 1.0 };
    let (out_w, out_h) = (
        ((logical.width() as f64) * scale).round().max(1.0) as u32,
        ((logical.height() as f64) * scale).round().max(1.0) as u32,
    );
    let scaled = if (out_w, out_h) == (logical.width(), logical.height()) {
        logical
    } else {
        image::imageops::resize(
            &logical,
            out_w,
            out_h,
            image::imageops::FilterType::Triangle,
        )
    };
    let mut bytes = Vec::new();
    match req.format {
        ShotFormat::Png => {
            scaled
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .map_err(|e| format!("xcap encode png: {e}"))?;
        }
        ShotFormat::Jpeg => {
            let mut cursor = std::io::Cursor::new(&mut bytes);
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut cursor,
                req.jpeg_quality.clamp(1, 100),
            );
            encoder
                .encode_image(&scaled)
                .map_err(|e| format!("xcap encode jpeg: {e}"))?;
        }
    }
    Ok(Shot {
        bytes,
        format: req.format,
        width: scaled.width(),
        height: scaled.height(),
        origin: (plan.rect.x, plan.rect.y),
        display: plan.output.clone().unwrap_or_else(|| "xcap".to_string()),
    })
}

/// xcap 的窗口是否对应请求的窗口（id 可能不同源，按矩形与标题兜底匹配）。
fn window_matches(window: &xcap::Window, id: u64, rect: &Rect) -> bool {
    let _ = id;
    let (x, y) = (
        window.x().unwrap_or(i32::MIN),
        window.y().unwrap_or(i32::MIN),
    );
    let (w, h) = (window.width().unwrap_or(0), window.height().unwrap_or(0));
    x == rect.x && y == rect.y && w == rect.width && h == rect.height
}

/// xcap 的显示器是否覆盖请求的矩形。
fn monitor_matches(monitor: &xcap::Monitor, rect: &Rect) -> bool {
    monitor.x().ok() == Some(rect.x)
        && monitor.y().ok() == Some(rect.y)
        && monitor.width().ok() == Some(rect.width)
        && monitor.height().ok() == Some(rect.height)
}

/// 读一个进程的进程组 id（`/proc/<pid>/stat` 里 `comm` 之后的第 3 个字段）。
///
/// `computer_launch` 把子进程放进它自己的进程组，所以进程组是「同一个应用」的可靠标识：
/// Chromium 这类应用会把窗口交给派生出来的进程（`brave` 启动时 pid 496662，窗口属于 496689），
/// 只按 pid 比对永远等不到窗口。读不进程（已退出、pid 不存在）时返回 `None`。
fn process_group(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.get(stat.rfind(')')? + 1..)?;
    rest.split_whitespace().nth(2)?.parse().ok()
}

/// 启动应用并等待窗口出现；输出进临时日志，不落在终端上。
pub fn launch(target: &str, args: &[String], wait_ms: u64) -> Result<Launched, String> {
    if target.trim().is_empty() {
        return Err("target is required".to_string());
    }
    let (pid, log) = super::launch_process(target, args)?;
    let deadline = Instant::now() + Duration::from_millis(wait_ms.max(200));
    let window = loop {
        sleep(Duration::from_millis(150));
        let list = windows().unwrap_or_default();
        if let Some(w) = list
            .iter()
            .find(|w| w.pid == pid || process_group(w.pid) == Some(pid))
        {
            break Some(w.clone());
        }
        if Instant::now() >= deadline {
            break None;
        }
    };
    Ok(Launched {
        window,
        pid,
        log: log.map(|path| path.display().to_string()),
    })
}

/// 窗口级动作。
///
/// sway 没有"最小化"这个状态（Wayland 由合成器决定可见性），所以 `minimize` 明确报错而不是
/// 假装做了事；`maximize` 映射成全屏（sway 的平铺布局里没有"最大化"的独立状态）。
pub fn window_action(id: u64, action: &str) -> Result<String, String> {
    match action {
        "close" => {
            let mut cmd = Command::new("swaymsg");
            cmd.args(["-q", &format!("[con_id={id}] kill")]);
            run_stdout(cmd, &format!("swaymsg close {id}"))?;
            Ok(format!("closed window {id}"))
        }
        "maximize" => {
            let mut cmd = Command::new("swaymsg");
            cmd.args(["-q", &format!("[con_id={id}] fullscreen enable")]);
            run_stdout(cmd, &format!("swaymsg fullscreen {id}"))?;
            Ok(format!("window {id} is now fullscreen"))
        }
        "restore" => {
            let mut cmd = Command::new("swaymsg");
            cmd.args(["-q", &format!("[con_id={id}] fullscreen disable")]);
            run_stdout(cmd, &format!("swaymsg un-fullscreen {id}"))?;
            Ok(format!("window {id} left fullscreen"))
        }
        "minimize" => Err(format!(
            "window {id} cannot be minimized: Wayland compositors decide visibility, and sway has no minimized state (hide it behind another window or close it)"
        )),
        other => Err(format!("unknown window action {other:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 能读到自己的进程组；不存在的 pid 返回 `None` 而不是 panic。
    #[test]
    fn process_group_reads_proc_stat() {
        assert!(process_group(std::process::id()).is_some());
        assert_eq!(process_group(u32::MAX), None);
    }
}

//! macOS 后端：`xcap` 截图 + `enigo` 输入 + `open -a` 聚焦。
//!
//! macOS 的窗口激活需要 `NSRunningApplication` 或辅助功能授权；这里走 `open -a <app>`，
//! 它对已经在运行的应用是"激活"而不是新开一个实例。窗口动作没有系统级 API 一一对应，
//! 用该平台约定的快捷键（⌘W / ⌘M / ⌃⌘F）完成，并在回执里说清是哪一种。
//!
//! 坐标单位与 `xcap` 一致：`CGDisplayBounds` 的逻辑点（全局桌面坐标，主显示器左上为原点，y 向下）。

use super::{CaptureRequest, Display, Shot, Window, xcap_common as common};
use crate::extensions::computer::input;
use core_graphics::{
    event::CGEvent,
    event_source::{CGEventSource, CGEventSourceStateID},
};
use std::process::Command;

/// 后端是否可用（macOS 上总是可用；没有屏幕录制权限时截图会报错）。
pub fn available() -> bool {
    true
}

/// 后端描述。
pub fn describe() -> String {
    "macos, capture: xcap (needs Screen Recording permission), input: enigo (CGEvent, needs Accessibility permission), windows: CoreGraphics".to_string()
}

/// 列出显示器。
pub fn displays() -> Result<Vec<Display>, String> {
    common::displays()
}

/// 列出可见窗口。
pub fn windows() -> Result<Vec<Window>, String> {
    common::windows()
}

/// 把窗口置前（通过 `open -a <应用名>` 激活已在运行的实例）。
pub fn focus_window(id: u64) -> Result<(), String> {
    let window = super::window(id)?;
    if window.app.is_empty() {
        return Err(format!(
            "window {id} reports no application name to activate"
        ));
    }

    let status = Command::new("open")
        .args(["-a", &window.app])
        .status()
        .map_err(|e| format!("could not run open -a {:?}: {e}", window.app))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "open -a {:?} failed with {status}; macOS may need the Accessibility permission",
            window.app
        ))
    }
}

/// 窗口动作（先激活窗口，再发该平台的约定快捷键）。
pub fn window_action(id: u64, action: &str) -> Result<String, String> {
    focus_window(id)?;
    let chord = match action {
        "close" => "cmd+w",
        "minimize" => "cmd+m",
        "maximize" | "restore" => "ctrl+cmd+f",
        other => return Err(format!("unknown window action {other:?}")),
    };
    input::press_chord(chord, 1, 0.0)?;
    Ok(format!("{action}: sent {chord} to window {id}"))
}

/// 执行截图。
pub fn capture(req: &CaptureRequest) -> Result<Shot, String> {
    common::capture(req)
}

/// 指针位置（全局桌面坐标，单位与 `displays()` / `windows()` 相同：逻辑点）。
///
/// 读 `CGEvent` 的当前光标位置——它给出的就是 `CGDisplayBounds` 那套坐标系，所以与截图
/// 回执的 `origin`、点击用的 `x/y` 同一单位，Retina 缩放不影响。
/// 建不出事件源（无图形会话）时返回 `None`。
pub fn cursor() -> Option<(i32, i32)> {
    let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState).ok()?;
    let event = CGEvent::new(source).ok()?;
    let point = event.location();
    Some((point.x as i32, point.y as i32))
}

/// 启动应用。
pub fn launch(target: &str, args: &[String], wait_ms: u64) -> Result<Option<Window>, String> {
    common::launch(target, args, wait_ms)
}

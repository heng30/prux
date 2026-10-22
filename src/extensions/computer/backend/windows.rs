//! Windows 后端：`xcap` 截图 + `enigo` 输入 + Win32 窗口控制。
//!
//! 截图与窗口枚举走两个平台共用的 `xcap_common`；这里只补 Windows 独有的
//! 聚焦（`SetForegroundWindow`）、窗口动作（`ShowWindow` / `WM_CLOSE`）与指针位置（`GetCursorPos`）。

use super::{CaptureRequest, Display, Shot, Window, xcap_common as common};
use windows::Win32::{
    Foundation::{HWND, LPARAM, POINT, WPARAM},
    UI::WindowsAndMessaging::{
        GetCursorPos, PostMessageW, SW_MAXIMIZE, SW_MINIMIZE, SW_RESTORE, SetForegroundWindow,
        ShowWindow, WM_CLOSE,
    },
};

/// 后端是否可用（Windows 上总是可用）。
pub fn available() -> bool {
    true
}

/// 后端描述。
pub fn describe() -> String {
    "windows, capture: xcap, input: enigo (SendInput), windows: Win32".to_string()
}

/// 列出显示器。
pub fn displays() -> Result<Vec<Display>, String> {
    common::displays()
}

/// 列出可见窗口。
pub fn windows() -> Result<Vec<Window>, String> {
    common::windows()
}

/// 把窗口置前。
pub fn focus_window(id: u64) -> Result<(), String> {
    let hwnd = HWND(id as usize as *mut core::ffi::c_void);
    let ok = unsafe { SetForegroundWindow(hwnd) };
    if ok.as_bool() {
        Ok(())
    } else {
        Err(format!(
            "Windows refused to bring window {id} to the foreground (the foreground lock is held by another process)"
        ))
    }
}

/// 倍率换算：`window_action` 用的参数名与工具一致。
pub fn window_action(id: u64, action: &str) -> Result<String, String> {
    let hwnd = HWND(id as usize as *mut core::ffi::c_void);
    match action {
        "close" => unsafe {
            PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0))
                .map_err(|e| format!("closing window {id} failed: {e}"))?;
            Ok(format!("asked window {id} to close"))
        },
        "minimize" => unsafe {
            _ = ShowWindow(hwnd, SW_MINIMIZE);
            Ok(format!("minimized window {id}"))
        },
        "maximize" => unsafe {
            _ = ShowWindow(hwnd, SW_MAXIMIZE);
            Ok(format!("maximized window {id}"))
        },
        "restore" => unsafe {
            _ = ShowWindow(hwnd, SW_RESTORE);
            Ok(format!("restored window {id}"))
        },
        other => Err(format!("unknown window action {other:?}")),
    }
}

/// 执行截图。
pub fn capture(req: &CaptureRequest) -> Result<Shot, String> {
    common::capture(req)
}

/// 指针位置。
pub fn cursor() -> Option<(i32, i32)> {
    let mut point = POINT::default();
    unsafe { GetCursorPos(&mut point).ok()? };
    Some((point.x, point.y))
}

/// 启动应用。
pub fn launch(target: &str, args: &[String], wait_ms: u64) -> Result<Option<Window>, String> {
    common::launch(target, args, wait_ms)
}

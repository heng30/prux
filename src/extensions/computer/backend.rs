//! 桌面后端：截图、窗口、指针/键盘注入与启动应用的平台适配层。
//!
//! 上层（工具实现）只认这里的中立类型，不认任何平台的窗口句柄或截图机制；
//! 每个平台在 `linux` / `windows` / `macos` 子模块里提供同名函数，
//! 未实现的平台由本模块的兜底实现统一返回"不支持"。

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod xcap_common;

#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(target_os = "windows")]
use windows as imp;

use crate::{extensions::computer::state::Rect, utils::process::spawn_detached};
use serde_json::{Value, json};
use std::path::PathBuf;

/// 一个显示器（或 Wayland 的一个 output）。
#[derive(Debug, Clone)]
pub struct Display {
    /// 显示器名称（Wayland 下是 output 名，如 `DP-1`）。
    pub name: String,
    /// 左上角 x（该平台坐标系的像素）。
    pub x: i32,
    /// 左上角 y。
    pub y: i32,
    /// 宽度（像素）。
    pub width: u32,
    /// 高度（像素）。
    pub height: u32,
    /// 缩放系数（1.0 表示未缩放）。
    pub scale: f64,
    /// 是否是主显示器。
    pub primary: bool,
}

impl Display {
    /// 转成模型可读的一行：`DP-1 0,0 1920x1080 scale 1.00 (primary)`。
    pub fn render(&self) -> String {
        format!(
            "{} {},{} {}x{} scale {:.2}{}",
            self.name,
            self.x,
            self.y,
            self.width,
            self.height,
            self.scale,
            if self.primary { " (primary)" } else { "" }
        )
    }
}

/// 一个可见的顶层窗口。
#[derive(Debug, Clone)]
pub struct Window {
    /// 平台窗口标识（交给 focus / 窗口截图使用）。
    pub id: u64,
    /// 窗口标题。
    pub title: String,
    /// 应用标识（Wayland 的 app_id，Windows 的进程名，macOS 的 bundle 名）。
    pub app: String,
    /// 所属进程 id；未知为 0。
    pub pid: u32,
    /// 左上角 x。
    pub x: i32,
    /// 左上角 y。
    pub y: i32,
    /// 宽度。
    pub width: u32,
    /// 高度。
    pub height: u32,
    /// 当前是否持有焦点。
    pub focused: bool,
}

impl Window {
    /// 窗口矩形。
    pub fn rect(&self) -> Rect {
        Rect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }

    /// 转成模型可读的一行。
    pub fn render(&self) -> String {
        format!(
            "{} {:?} — {} [{}x{} at {},{}]{}",
            self.id,
            self.app,
            if self.title.is_empty() {
                "(untitled)"
            } else {
                &self.title
            },
            self.width,
            self.height,
            self.x,
            self.y,
            if self.focused { " (focused)" } else { "" }
        )
    }
}

/// 截图的编码格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShotFormat {
    /// PNG（无损，文字锐利，体积大）。
    Png,
    /// JPEG（默认；屏幕文字的清晰度与体积的折中）。
    Jpeg,
}

impl ShotFormat {
    /// 对应的 mime 类型。
    pub fn mime(&self) -> &'static str {
        match self {
            ShotFormat::Png => "image/png",
            ShotFormat::Jpeg => "image/jpeg",
        }
    }

    /// 参数里接受的名字（`jpg` 视作 `jpeg`）。
    pub fn parse(name: &str) -> Option<Self> {
        match name.to_ascii_lowercase().as_str() {
            "png" => Some(ShotFormat::Png),
            "jpeg" | "jpg" => Some(ShotFormat::Jpeg),
            _ => None,
        }
    }
}

/// 一次应用启动的结果。
#[derive(Debug, Clone)]
pub struct Launched {
    /// 等到的窗口；等不到时为 `None`（进程可能还在启动，也可能已经失败退出）。
    pub window: Option<Window>,
    /// 子进程 pid（回执里给模型看，窗口没出现时可对照日志与进程表）。
    pub pid: u32,
    /// 子进程 stdout/stderr 落到的临时日志路径；建不出临时文件时为 `None`（输出被丢弃）。
    pub log: Option<String>,
}

/// 一张截好的图。
#[derive(Debug, Clone)]
pub struct Shot {
    /// 图像字节（已是请求的格式）。
    pub bytes: Vec<u8>,
    /// 图像格式。
    pub format: ShotFormat,
    /// 图像宽度（像素）。
    pub width: u32,
    /// 图像高度（像素）。
    pub height: u32,
    /// 该图在屏幕上的原点。
    pub origin: (i32, i32),
    /// 贡献了这张图的显示器名（整屏截图时按分隔符拼接）。
    pub display: String,
}

/// 截图目标。
#[derive(Debug, Clone)]
pub enum CaptureTarget {
    /// 整个虚拟桌面。
    All,
    /// 单个显示器（按名称）。
    Display(String),
    /// 屏幕上的一块绝对矩形。
    Region(Rect),
    /// 一个窗口（按平台窗口标识）。
    Window(u64),
}

/// 一次截图的完整请求。
#[derive(Debug, Clone)]
pub struct CaptureRequest {
    /// 截哪里。
    pub target: CaptureTarget,
    /// 输出缩放（1.0 = 原始像素）。
    pub scale: f64,
    /// 输出格式。
    pub format: ShotFormat,
    /// JPEG 质量（1–100）。
    pub jpeg_quality: u8,
    /// 是否把鼠标指针画进图里。
    pub cursor: bool,
}

/// 平台后端是否可用（Wayland 会话、无障碍总线等）。
pub fn available() -> bool {
    imp::available()
}

/// 当前会话/后端的简短描述，用于回执与错误信息。
pub fn describe() -> String {
    imp::describe()
}

/// 列出显示器。
pub fn displays() -> Result<Vec<Display>, String> {
    imp::displays()
}

/// 列出可见顶层窗口。
pub fn windows() -> Result<Vec<Window>, String> {
    imp::windows()
}

/// 按标识取一个窗口。
pub fn window(id: u64) -> Result<Window, String> {
    windows()?.into_iter().find(|w| w.id == id).ok_or_else(|| {
        format!(
            "window {id} is not in the current window list; list them again with computer_windows"
        )
    })
}

/// 把窗口置前并给它焦点。
pub fn focus_window(id: u64) -> Result<(), String> {
    imp::focus_window(id)
}

/// 执行一次截图。
pub fn capture(req: &CaptureRequest) -> Result<Shot, String> {
    imp::capture(req)
}

/// 指针当前的屏幕位置；Wayland 协议不提供，返回 `None`。
pub fn cursor() -> Option<(i32, i32)> {
    imp::cursor()
}

/// 启动一个应用并等它出现窗口（等不到时 `window` 为 `None`）。
pub fn launch(target: &str, args: &[String], wait_ms: u64) -> Result<Launched, String> {
    imp::launch(target, args, wait_ms)
}

/// 拉起子进程并让它脱离 prux：输出进临时日志、Unix 上另起进程组（见 [`crate::utils::process::spawn_detached`]）。
///
/// 返回 `(pid, 日志路径)`；日志建不出来时输出被丢弃，路径为 `None`。
/// 可执行文件起不来（不在 PATH 上、没有显示环境等）返回 `Err`。
fn launch_process(target: &str, args: &[String]) -> Result<(u32, Option<PathBuf>), String> {
    spawn_detached("computer-launch", target, args)
        .map_err(|e| format!("could not start {target:?}: {e}"))
}

/// 对窗口执行一个窗口级动作（`close` / `minimize` / `maximize` / `restore`）。
pub fn window_action(id: u64, action: &str) -> Result<String, String> {
    imp::window_action(id, action)
}

/// 从窗口列表里挑出与请求最接近的窗口（按 filter 子串，标题或应用名）。
pub fn find_window<'a>(list: &'a [Window], filter: &str) -> Vec<&'a Window> {
    let needle = filter.to_lowercase();
    if needle.is_empty() {
        return list.iter().collect();
    }

    list.iter()
        .filter(|w| {
            w.title.to_lowercase().contains(&needle) || w.app.to_lowercase().contains(&needle)
        })
        .collect()
}

/// 把显示器列表渲染成 `displays` 段的多行文本。
pub fn render_displays(list: &[Display]) -> String {
    list.iter()
        .map(Display::render)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 把窗口列表渲染成 JSON 之外的紧凑文本（工具回执用）。
pub fn render_windows(list: &[Window]) -> String {
    list.iter()
        .map(Window::render)
        .collect::<Vec<_>>()
        .join("\n")
}

/// 平台相关诊断信息（供 `computer_windows` 之外的排障使用）。
pub fn diagnose() -> Value {
    json!({
        "backend": describe(),
        "available": available(),
        "displays": displays().map(|d| d.iter().map(Display::render).collect::<Vec<_>>()).unwrap_or_default(),
        "cursor": cursor(),
    })
}

// ---------------------------------------------------------------------------
// 未实现平台：把"不支持"说清楚，而不是让工具调用在运行时抛内部错误。
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
mod imp {
    use super::*;

    /// 任何平台都不假装可用。
    pub fn available() -> bool {
        false
    }

    /// 说明当前平台没有桌面后端。
    pub fn describe() -> String {
        format!("no desktop backend for {}", std::env::consts::OS)
    }

    /// 没有显示器列表。
    pub fn displays() -> Result<Vec<Display>, String> {
        Err(format!(
            "computer control has no backend for {} (supported: Linux Wayland, Windows, macOS)",
            std::env::consts::OS
        ))
    }

    /// 没有窗口列表。
    pub fn windows() -> Result<Vec<Window>, String> {
        displays().map(|_| Vec::new())
    }

    /// 不能聚焦窗口。
    pub fn focus_window(_id: u64) -> Result<(), String> {
        displays().map(|_| ())
    }

    /// 不能截图。
    pub fn capture(_req: &CaptureRequest) -> Result<Shot, String> {
        displays().map(|_| unreachable!())
    }

    /// 没有指针位置。
    pub fn cursor() -> Option<(i32, i32)> {
        None
    }

    /// 不能启动应用。
    pub fn launch(_target: &str, _args: &[String], _wait_ms: u64) -> Result<Launched, String> {
        displays().map(|_| unreachable!())
    }

    /// 没有窗口动作。
    pub fn window_action(_id: u64, _action: &str) -> Result<String, String> {
        displays().map(|_| String::new())
    }
}

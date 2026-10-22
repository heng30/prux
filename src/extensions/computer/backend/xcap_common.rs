//! Windows / macOS 共用的截图与窗口实现：两个平台的 `xcap` API 完全一致。
//!
//! 与 Linux 的差别只在"窗口枚举"和"聚焦/窗口动作"要各自的系统 API（见 `windows.rs` /
//! `macos.rs`），截图本身在这里统一。

use super::{CaptureRequest, CaptureTarget, Display, Shot, ShotFormat, Window};
use crate::extensions::computer::state::Rect;
use image::RgbaImage;
use std::time::{Duration, Instant};

/// 列出显示器。
pub fn displays() -> Result<Vec<Display>, String> {
    let monitors = xcap::Monitor::all().map_err(|e| format!("xcap: {e}"))?;
    let mut out = Vec::new();
    for monitor in monitors {
        out.push(Display {
            name: monitor
                .name()
                .or_else(|_| monitor.friendly_name())
                .unwrap_or_else(|_| "display".to_string()),
            x: monitor.x().unwrap_or(0),
            y: monitor.y().unwrap_or(0),
            width: monitor.width().unwrap_or(0),
            height: monitor.height().unwrap_or(0),
            scale: f64::from(monitor.scale_factor().unwrap_or(1.0)),
            primary: monitor.is_primary().unwrap_or(false),
        });
    }

    if out.is_empty() {
        return Err(
            "xcap reported no monitor; on macOS this usually means the Screen Recording permission is missing"
                .to_string(),
        );
    }
    Ok(out)
}

/// 列出可见顶层窗口。
pub fn windows() -> Result<Vec<Window>, String> {
    let list = xcap::Window::all().map_err(|e| format!("xcap: {e}"))?;
    let mut out = Vec::new();
    for window in list {
        if window.is_minimized().unwrap_or(false) {
            continue;
        }

        let width = window.width().unwrap_or(0);
        let height = window.height().unwrap_or(0);
        if width == 0 || height == 0 {
            continue;
        }

        out.push(Window {
            id: u64::from(window.id().unwrap_or(0)),
            title: window.title().unwrap_or_default(),
            app: window.app_name().unwrap_or_default(),
            pid: window.pid().unwrap_or(0),
            x: window.x().unwrap_or(0),
            y: window.y().unwrap_or(0),
            width,
            height,
            focused: window.is_focused().unwrap_or(false),
        });
    }
    Ok(out)
}

/// 虚拟桌面矩形（所有显示器的最小外接矩形）。
fn virtual_bounds() -> Result<Rect, String> {
    let list = displays()?;
    let x0 = list.iter().map(|d| d.x).min().unwrap_or(0);
    let y0 = list.iter().map(|d| d.y).min().unwrap_or(0);
    let x1 = list.iter().map(|d| d.x + d.width as i32).max().unwrap_or(0);
    let y1 = list
        .iter()
        .map(|d| d.y + d.height as i32)
        .max()
        .unwrap_or(0);
    Ok(Rect {
        x: x0,
        y: y0,
        width: (x1 - x0).max(1) as u32,
        height: (y1 - y0).max(1) as u32,
    })
}

/// 把所有显示器拼成一张覆盖整个虚拟桌面的图。
fn compose(bounds: Rect) -> Result<RgbaImage, String> {
    let monitors = xcap::Monitor::all().map_err(|e| format!("xcap: {e}"))?;
    let mut canvas = RgbaImage::new(bounds.width, bounds.height);
    for monitor in monitors {
        let (mx, my) = (monitor.x().unwrap_or(0), monitor.y().unwrap_or(0));
        let pixels = monitor
            .capture_image()
            .map_err(|e| format!("xcap monitor capture: {e}"))?;
        let offset_x = (mx - bounds.x) as i64;
        let offset_y = (my - bounds.y) as i64;
        for (px, py, pixel) in pixels.enumerate_pixels() {
            let x = offset_x + i64::from(px);
            let y = offset_y + i64::from(py);
            if x >= 0 && y >= 0 && (x as u32) < bounds.width && (y as u32) < bounds.height {
                canvas.put_pixel(x as u32, y as u32, *pixel);
            }
        }
    }
    Ok(canvas)
}

/// 执行截图。
pub fn capture(req: &CaptureRequest) -> Result<Shot, String> {
    let bounds = virtual_bounds()?;
    let full = match &req.target {
        CaptureTarget::Window(id) => {
            let list = xcap::Window::all().map_err(|e| format!("xcap: {e}"))?;
            let window = list
                .into_iter()
                .find(|w| u64::from(w.id().unwrap_or(0)) == *id)
                .ok_or_else(|| format!("window {id} is not capturable"))?;
            let origin = (window.x().unwrap_or(0), window.y().unwrap_or(0));
            let image = window
                .capture_image()
                .map_err(|e| format!("xcap window {id}: {e}"))?;
            return finish(image, origin, req, "window".to_string());
        }
        CaptureTarget::All => compose(bounds)?,
        CaptureTarget::Display(name) => {
            let monitors = xcap::Monitor::all().map_err(|e| format!("xcap: {e}"))?;
            let monitor = monitors
                .into_iter()
                .find(|m| &m.name().unwrap_or_default() == name)
                .ok_or_else(|| format!("no display named {name:?}"))?;
            let origin = (monitor.x().unwrap_or(0), monitor.y().unwrap_or(0));
            let image = monitor
                .capture_image()
                .map_err(|e| format!("xcap monitor {name}: {e}"))?;
            return finish(image, origin, req, name.clone());
        }
        CaptureTarget::Region(rect) => {
            let canvas = compose(bounds)?;
            let x = (rect.x - bounds.x).max(0) as u32;
            let y = (rect.y - bounds.y).max(0) as u32;
            let w = rect.width.min(canvas.width().saturating_sub(x));
            let h = rect.height.min(canvas.height().saturating_sub(y));
            if w == 0 || h == 0 {
                return Err(format!(
                    "the requested region ({},{} {}x{}) is outside the desktop",
                    rect.x, rect.y, rect.width, rect.height
                ));
            }
            let cropped = image::imageops::crop_imm(&canvas, x, y, w, h).to_image();
            return finish(cropped, (rect.x, rect.y), req, "region".to_string());
        }
    };
    finish(full, (bounds.x, bounds.y), req, "all displays".to_string())
}

/// 缩放、编码，并组装成 [`Shot`]。
fn finish(
    image: RgbaImage,
    origin: (i32, i32),
    req: &CaptureRequest,
    display: String,
) -> Result<Shot, String> {
    let scale = if req.scale > 0.0 { req.scale } else { 1.0 };
    let (width, height) = (
        ((image.width() as f64) * scale).round().max(1.0) as u32,
        ((image.height() as f64) * scale).round().max(1.0) as u32,
    );

    let scaled = if (width, height) == (image.width(), image.height()) {
        image
    } else {
        image::imageops::resize(&image, width, height, image::imageops::FilterType::Triangle)
    };

    let mut bytes = Vec::new();
    match req.format {
        ShotFormat::Png => {
            scaled
                .write_to(
                    &mut std::io::Cursor::new(&mut bytes),
                    image::ImageFormat::Png,
                )
                .map_err(|e| format!("encode png: {e}"))?;
        }
        ShotFormat::Jpeg => {
            let mut cursor = std::io::Cursor::new(&mut bytes);
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(
                &mut cursor,
                req.jpeg_quality.clamp(1, 100),
            );
            encoder
                .encode_image(&scaled)
                .map_err(|e| format!("encode jpeg: {e}"))?;
        }
    }
    Ok(Shot {
        bytes,
        format: req.format,
        width: scaled.width(),
        height: scaled.height(),
        origin,
        display,
    })
}

/// 启动应用并轮询它的窗口。
pub fn launch(target: &str, args: &[String], wait_ms: u64) -> Result<Option<Window>, String> {
    let child = std::process::Command::new(target)
        .args(args)
        .stdin(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start {target:?}: {e}"))?;
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_millis(wait_ms.max(250));

    loop {
        std::thread::sleep(Duration::from_millis(100));
        if let Ok(list) = windows()
            && let Some(window) = list.into_iter().find(|w| w.pid == pid)
        {
            return Ok(Some(window));
        }

        if std::time::Instant::now() >= deadline {
            return Ok(None);
        }
    }
}

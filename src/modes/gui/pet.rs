//! 桌宠窗口：一个透明、置顶、不抢焦点的小窗口，跟着 agent 姿态播 spritesheet 动画。
//!
//! 平台差异只在建窗这一处：Wayland 用 layer-shell（贴屏底、不进任务栏、不吃键盘焦点），
//! 其它平台退回 `Floating` 普通悬浮窗（透明与置顶由窗口系统给，穿透拿不到）。
//! 资源解码放后台线程，UI 线程只做「换帧 + 通知重绘」。

use crate::extensions::desktop_pet::{self, Pose};
use gpui::{
    App, AppContext, Bounds, Context, ImageSource, IntoElement, MouseButton, ParentElement, Pixels,
    Render, RenderImage, Styled, Window, WindowBackgroundAppearance, WindowBounds, WindowKind,
    WindowOptions, div, img, layer_shell, point, prelude::*, px, size,
};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, mpsc::Receiver},
    time::{Duration, Instant},
};

/// 一帧的显示尺寸（逻辑像素）。原帧 640×360 在桌面上太大，解码时缩到这个尺寸。
const PET_W: f32 = 320.0;

/// 一帧的显示高度（逻辑像素）。
const PET_H: f32 = 180.0;

/// 播放帧率（与 spritesheet 一致）。
const FPS: u64 = 12;

/// 每个 spritesheet 的帧数（8 列 × 15 行）。
const FRAMES: usize = 120;

/// spritesheet 的列数；行数 = [`FRAMES`] / 列数。
const COLS: usize = 8;

/// 同时缓存的动画数：当前姿态 + 上一个（换姿态时不会闪白）。
const CACHE: usize = 2;

/// 点击回应播多久（毫秒）。
const CLICK_MS: u64 = 3_000;

/// 距屏幕边缘的留白（逻辑像素）。
const MARGIN: f32 = 24.0;

/// 解码好的一个动作：按播放顺序排好的 120 帧。
struct Sheet {
    /// 每帧一张已上传的纹理。
    frames: Vec<Arc<RenderImage>>,
}

/// 桌宠视图。
pub struct PetView {
    /// 当前正在播的动作。
    pose: Pose,
    /// 当前动作的起始时刻（换算帧号用）。
    started: Instant,
    /// 当前动作已解码的帧。
    sheet: Option<Arc<Sheet>>,
    /// [`Self::sheet`] 对应的动作名（`idle` 等）。
    sheet_key: Option<String>,
    /// 后台解码中的动作名与它的接收端。
    pending: Option<(String, Receiver<Option<Sheet>>)>,
    /// 点击回应的截止时刻。
    click_until: Option<Instant>,
    /// 已解码动画的 LRU（键是动作名）。
    cache: VecDeque<(String, Arc<Sheet>)>,
}

/// 打开桌宠窗口；平台不支持或没有资源时返回 `None`（静默跳过，不打扰用户）。
///
/// 调用方应先用 [`desktop_pet::assets_ready`] 判断资源是否就位。
pub fn open(cx: &mut App) -> Option<gpui::WindowHandle<PetView>> {
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds(cx))),
        titlebar: None,
        focus: false,
        show: true,
        kind: window_kind(),
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        window_background: WindowBackgroundAppearance::Transparent,
        app_id: Some("prux-pet".to_string()),
        ..Default::default()
    };

    cx.open_window(options, |_, cx| cx.new(PetView::new)).ok()
}

/// 建窗参数里的位置与尺寸：Wayland 层壳按 anchor + margin 贴左下角，其余平台靠窗口坐标。
fn bounds(cx: &mut App) -> Bounds<Pixels> {
    let size = size(px(PET_W), px(PET_H));
    let Some(display) = cx.primary_display() else {
        return Bounds::new(point(px(MARGIN), px(MARGIN)), size);
    };
    let area = display.bounds();
    let x = area.origin.x + px(MARGIN);
    let y = area.origin.y + area.size.height - px(PET_H + MARGIN);
    Bounds::new(point(x, y), size)
}

/// 窗口类型：Wayland 用层壳（贴底、不吃焦点），其它平台用浮动窗。
fn window_kind() -> WindowKind {
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return WindowKind::Floating;
    }

    WindowKind::LayerShell(layer_shell::LayerShellOptions {
        namespace: "prux-pet".to_string(),
        layer: layer_shell::Layer::Overlay,
        anchor: layer_shell::Anchor::BOTTOM | layer_shell::Anchor::LEFT,
        margin: Some((px(0.), px(0.), px(0.), px(MARGIN))),
        keyboard_interactivity: layer_shell::KeyboardInteractivity::None,
        ..Default::default()
    })
}

impl PetView {
    /// 建视图、起播放循环，并开始解当前动作。
    fn new(cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            pose: desktop_pet::pose(),
            started: Instant::now(),
            sheet: None,
            sheet_key: None,
            pending: None,
            click_until: None,
            cache: VecDeque::new(),
        };
        view.want(desktop_pet::anim_path(view.pose));

        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(1000 / FPS))
                    .await;

                let alive = this
                    .update(cx, |pet, cx| {
                        pet.step();
                        cx.notify()
                    })
                    .is_ok();
                if !alive {
                    break;
                }
            }
        })
        .detach();

        view
    }

    /// 每帧一次：切姿态、收解码结果、推进帧号。
    fn step(&mut self) {
        if self.click_until.is_some_and(|t| Instant::now() >= t) {
            self.click_until = None;
        }
        let pose = match self.click_until {
            Some(_) => Pose::Click,
            None => desktop_pet::pose(),
        };
        if pose != self.pose {
            self.pose = pose;
            self.started = Instant::now();
            self.want(desktop_pet::anim_path(pose));
        }
        self.poll_decoded();
    }

    /// 点击回应：播 [`Pose::Click`]，期间盖住 agent 姿态。
    fn clicked(&mut self, cx: &mut Context<Self>) {
        self.click_until = Some(Instant::now() + Duration::from_millis(CLICK_MS));
        self.pose = Pose::Click;
        self.started = Instant::now();
        self.want(desktop_pet::anim_path(Pose::Click));
        cx.notify();
    }

    /// 让当前动作变成 `path` 对应的动画：缓存里有就换、没有就在后台解一份。
    fn want(&mut self, path: PathBuf) {
        let key = key_of(&path);
        if self.sheet_key.as_deref() == Some(key.as_str()) {
            return;
        }
        if let Some(hit) = self
            .cache
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, s)| s.clone())
        {
            self.promote(key, hit);
            return;
        }

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(decode(&path));
        });
        self.pending = Some((key, rx));
    }

    /// 换上已解码的动作并进 LRU（超出 [`CACHE`] 的丢掉）。
    fn promote(&mut self, key: String, sheet: Arc<Sheet>) {
        self.sheet = Some(sheet.clone());
        self.sheet_key = Some(key.clone());
        self.cache.retain(|(k, _)| *k != key);
        self.cache.push_back((key, sheet));
        while self.cache.len() > CACHE {
            self.cache.pop_front();
        }
    }

    /// 收后台解码结果（每帧最多收一个）。
    fn poll_decoded(&mut self) {
        let Some((key, rx)) = self.pending.as_ref() else {
            return;
        };
        let Ok(decoded) = rx.try_recv() else {
            return;
        };
        let key = key.clone();
        self.pending = None;
        if let Some(sheet) = decoded {
            self.promote(key, Arc::new(sheet));
        }
    }

    /// 当前该显示的那一帧。
    fn frame(&self) -> Option<Arc<RenderImage>> {
        let sheet = self.sheet.as_ref()?;
        let index = (self.started.elapsed().as_millis() * FPS as u128 / 1000) as usize % FRAMES;
        sheet.frames.get(index).cloned()
    }
}

impl Render for PetView {
    /// 只在窗口大小的方框里画当前帧，其余透明。
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let frame = self.frame();
        div()
            .w(px(PET_W))
            .h(px(PET_H))
            .when_some(frame, |this, frame| {
                this.child(img(ImageSource::from(frame)).w(px(PET_W)).h(px(PET_H)))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.clicked(cx)),
            )
    }
}

/// 动作名（文件名去掉扩展名）。
fn key_of(path: &Path) -> String {
    path.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// 解码一个 spritesheet：切成 [`FRAMES`] 帧并缩到显示尺寸（在后台线程跑）。
///
/// 解码失败（文件损坏 / 不是 webp / 尺寸对不上网格）返回 `None`，视图保持上一张图。
fn decode(path: &Path) -> Option<Sheet> {
    let bytes = std::fs::read(path).ok()?;
    let sheet = image::load_from_memory_with_format(&bytes, image::ImageFormat::WebP)
        .ok()?
        .to_rgba8();
    let rows = (FRAMES / COLS) as u32;
    let (fw, fh) = (sheet.width() / COLS as u32, sheet.height() / rows);
    if fw == 0 || fh == 0 {
        return None;
    }

    let mut frames = Vec::with_capacity(FRAMES);
    for index in 0..FRAMES {
        let x = (index % COLS) as u32 * fw;
        let y = (index / COLS) as u32 * fh;
        let cell = image::imageops::crop_imm(&sheet, x, y, fw, fh).to_image();
        let scaled = image::imageops::resize(
            &cell,
            PET_W as u32,
            PET_H as u32,
            image::imageops::FilterType::Triangle,
        );
        frames.push(Arc::new(RenderImage::new(vec![image::Frame::new(scaled)])));
    }
    Some(Sheet { frames })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_matches_the_animation_name() {
        assert_eq!(key_of(Path::new("/x/anim/tool-error.webp")), "tool-error");
    }

    #[test]
    fn decode_returns_none_for_a_broken_file() {
        let dir = std::env::temp_dir().join("prux-pet-decode-broken");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("broken.webp");
        std::fs::write(&path, b"not an image").unwrap();
        assert!(decode(&path).is_none());
    }

    #[test]
    fn decode_returns_none_when_the_grid_is_too_small() {
        // 1×1 的合法 PNG 塞进 `.webp` 后缀：能解码，但切不出 8×15 网格，必须安全返回 None。
        let dir = std::env::temp_dir().join("prux-pet-decode-tiny");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("tiny.webp");
        image::RgbaImage::new(1, 1).save(&path).unwrap();
        assert!(decode(&path).is_none());
    }
}

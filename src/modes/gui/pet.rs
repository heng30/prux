//! 桌宠层：主窗口里的一层可拖动动画，跟着 agent 姿态播 spritesheet。
//!
//! 它不是独立窗口：画在 prux 主窗口内，所以不受窗口管理器管、主窗口一收起来就跟着消失，
//! 位置也能按窗口坐标记住。资源解码放后台线程，UI 线程只做「换帧 + 通知重绘」。

use crate::extensions::pet::{self as pet_ext, Pose};
use gpui::{
    ImageSource, ParentElement, Pixels, RenderImage, Size, Styled, Window, div, img, prelude::*, px,
};
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Arc, mpsc::Receiver},
    time::Instant,
};

/// 一帧的显示尺寸（逻辑像素）。原帧 640×360 在界面上太大，解码时缩到这个尺寸。
pub const PET_W: f32 = 320.0;

/// 一帧的显示高度（逻辑像素）。
pub const PET_H: f32 = 180.0;

/// 播放帧率（与 spritesheet 一致）。
pub const FPS: u64 = 12;

/// 每个 spritesheet 的帧数（8 列 × 15 行）。
const FRAMES: usize = 120;

/// spritesheet 的列数；行数 = [`FRAMES`] / 列数。
const COLS: usize = 8;

/// 同时缓存的动画数：当前姿态 + 上一个（换姿态时不会闪白）。
const CACHE: usize = 2;

/// 一段 clip 播多久（毫秒）：120 帧 ÷ 12fps。播完才换姿态，免得来回抖。
const CLIP_MS: u64 = 10_000;

/// 点击回应的时长（毫秒）：它插队播完就交回 agent 姿态，不必等满一整段。
const CLICK_MS: u64 = 3_000;

/// 默认位置距窗口边缘的留白（逻辑像素）。
const MARGIN: f32 = 24.0;

/// 判定「这是拖动而不是点击」的最小位移（逻辑像素）。
const DRAG_SLOP: f32 = 4.0;

/// 解码好的一个动作：按播放顺序排好的 120 帧。
struct Sheet {
    /// 每帧一张已上传的纹理。
    frames: Vec<Arc<RenderImage>>,
}

/// 拖动状态。
struct Drag {
    /// 按下时鼠标与宠物左上角的差，拖动期间保持不变。
    grab: (f32, f32),
    /// 是否真的移动过：小于 [`DRAG_SLOP`] 当点击，不写盘。
    moved: bool,
}

/// 桌宠视图（由主界面持有，不是独立窗口）。
pub struct Pet {
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
    /// 已解码动画的 LRU（键是动作名）。
    cache: VecDeque<(String, Arc<Sheet>)>,
    /// 左上角在窗口内的位置（逻辑像素）。
    offset: (f32, f32),
    /// 进行中的拖动。
    drag: Option<Drag>,
}

impl Pet {
    /// 按上次保存的位置建视图（没存过就贴左下角），并开始解当前动作。
    pub fn new(window: &Window) -> Self {
        let default = default_offset(window.viewport_size());
        let mut pet = Self {
            pose: pet_ext::pose(),
            started: Instant::now(),
            sheet: None,
            sheet_key: None,
            pending: None,
            cache: VecDeque::new(),
            offset: pet_ext::saved_offset().unwrap_or(default),
            drag: None,
        };
        pet.want(pet_ext::anim_path(pet.pose));
        pet
    }

    /// 每帧一次：收解码结果、按 agent 姿态换动作（一段 clip 播完才换，点击/报错/庆祝插队）。
    pub fn tick(&mut self) {
        self.poll_decoded();

        let want = pet_ext::pose();
        let urgent = matches!(want, Pose::ToolError | Pose::Celebrate);
        if want != self.pose && (urgent || self.clip_done()) {
            self.play(want);
        }
    }

    /// 开始拖动（顺带播一下点击回应）。
    pub fn down(&mut self, at: gpui::Point<Pixels>) {
        self.drag = Some(Drag {
            grab: (
                f32::from(at.x) - self.offset.0,
                f32::from(at.y) - self.offset.1,
            ),
            moved: false,
        });
        self.play(Pose::Click);
    }

    /// 跟随鼠标移动；返回是否真的移动了（调用方据此重绘）。
    pub fn drag_to(&mut self, at: gpui::Point<Pixels>) -> bool {
        let Some(drag) = self.drag.as_mut() else {
            return false;
        };
        let next = (f32::from(at.x) - drag.grab.0, f32::from(at.y) - drag.grab.1);
        let moved = (next.0 - self.offset.0).abs() > DRAG_SLOP
            || (next.1 - self.offset.1).abs() > DRAG_SLOP;
        self.offset = next;
        if moved {
            drag.moved = true;
        }
        moved
    }

    /// 松手：真的拖动过就把位置写进配置，下次启动照原样摆。
    pub fn release(&mut self) {
        if self.drag.take().is_some_and(|drag| drag.moved) {
            pet_ext::save_offset(self.offset.0, self.offset.1);
        }
    }

    /// 画出这一帧（绝对定位、透明底），位置夹在窗口可见范围内。
    pub fn element(&mut self, window: &Window) -> gpui::Stateful<gpui::Div> {
        let (x, y) = clamp(self.offset, window.viewport_size());
        let frame = self.frame();
        div()
            .id("pet")
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(PET_W))
            .h(px(PET_H))
            .when_some(frame, |this, frame| {
                this.child(img(ImageSource::from(frame)).w(px(PET_W)).h(px(PET_H)))
            })
    }

    /// 当前该显示的那一帧。
    fn frame(&self) -> Option<Arc<RenderImage>> {
        let sheet = self.sheet.as_ref()?;
        let index = (self.started.elapsed().as_millis() * FPS as u128 / 1000) as usize % FRAMES;
        sheet.frames.get(index).cloned()
    }

    /// 当前这段 clip 播完了没（点击回应按 [`CLICK_MS`] 单独算）。
    fn clip_done(&self) -> bool {
        let clip = if self.pose == Pose::Click {
            CLICK_MS
        } else {
            CLIP_MS
        };
        self.started.elapsed().as_millis() as u64 >= clip
    }

    /// 切到某个动作：从现在起重新计时，并让动画就位。
    fn play(&mut self, pose: Pose) {
        self.pose = pose;
        self.started = Instant::now();
        self.want(pet_ext::anim_path(pose));
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
}

/// 没存过位置时的默认摆放：窗口左下角，距边缘 [`MARGIN`]。
fn default_offset(viewport: Size<Pixels>) -> (f32, f32) {
    (MARGIN, f32::from(viewport.height) - PET_H - MARGIN)
}

/// 把位置夹回窗口可见范围（窗口缩小或换分辨率后不会跑到屏幕外）。
fn clamp(offset: (f32, f32), viewport: Size<Pixels>) -> (f32, f32) {
    let max_x = (f32::from(viewport.width) - PET_W).max(0.0);
    let max_y = (f32::from(viewport.height) - PET_H).max(0.0);
    (offset.0.clamp(0.0, max_x), offset.1.clamp(0.0, max_y))
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
    fn clamp_keeps_the_pet_inside_the_window() {
        let viewport = gpui::size(px(800.), px(600.));
        assert_eq!(clamp((100.0, 100.0), viewport), (100.0, 100.0));
        assert_eq!(
            clamp((-50.0, 900.0), viewport),
            (0.0, 600.0 - PET_H),
            "拖出边界后夹回可见区域"
        );
        assert_eq!(
            clamp((10.0, 10.0), gpui::size(px(100.), px(100.))),
            (0.0, 0.0),
            "窗口比宠物还小时贴左上角"
        );
    }

    #[test]
    fn default_offset_sits_above_the_bottom_edge() {
        let offset = default_offset(gpui::size(px(800.), px(600.)));
        assert_eq!(offset, (MARGIN, 600.0 - PET_H - MARGIN));
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

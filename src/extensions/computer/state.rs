//! `computer` 扩展的运行时状态：开关、截图帧注册表与仍按住的输入。
//!
//! 状态全部是**进程内**的：开关在重启后回到关闭（与「把鼠标键盘交给模型」这件事的风险相称），
//! 帧与按住记录也没有跨进程意义。所有入口都在锁内只做读写，不回调注册表（锁序：注册表 → 扩展状态）。

use std::{
    collections::VecDeque,
    sync::{Mutex, MutexGuard, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

/// 帧注册表保留的上限：只够回溯最近几次观察，多了就是无用的内存。
pub const FRAME_KEEP: usize = 32;

/// 拖拽锚点的有效期：接着上一笔画画只在一个手势之内有意义。
pub const DRAG_ANCHOR_TTL_MS: u128 = 60_000;

/// 屏幕矩形（虚拟桌面像素，左上角 + 尺寸）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    /// 左上角 x（虚拟桌面像素）。
    pub x: i32,
    /// 左上角 y（虚拟桌面像素）。
    pub y: i32,
    /// 宽度（像素）。
    pub width: u32,
    /// 高度（像素）。
    pub height: u32,
}

impl Rect {
    /// 右下角 x（不含）。
    pub fn right(&self) -> i32 {
        self.x.saturating_add(self.width as i32)
    }

    /// 右下角 y（不含）。
    pub fn bottom(&self) -> i32 {
        self.y.saturating_add(self.height as i32)
    }

    /// 是否包含某个屏幕点。
    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && y >= self.y && x < self.right() && y < self.bottom()
    }
}

/// 一次截图的来源登记：模型按图内像素给的坐标靠它换算回屏幕坐标。
#[derive(Debug, Clone)]
pub struct Frame {
    /// 帧 id（`f1`、`f2`…），工具参数里引用它。
    pub id: String,
    /// 这张图在屏幕上的原点（虚拟桌面像素）。
    pub origin: (i32, i32),
    /// 被截取的屏幕区域尺寸（虚拟桌面像素）。
    pub screen: (u32, u32),
    /// 交付给模型的图像尺寸（缩放后的像素）。
    pub image: (u32, u32),
    /// 图像像素 ÷ 屏幕像素（1.0 表示 1:1）。
    pub scale: f64,
    /// 截图所属窗口的标识；整屏截图为 `None`。
    pub window: Option<u64>,
    /// 截图时该窗口的矩形，用于发现"图拍完之后窗口动了"。
    pub window_rect: Option<Rect>,
    /// 登记时刻（unix 毫秒），用于诊断与过期判断。
    pub created_ms: u128,
}

impl Frame {
    /// 图像像素 → 屏幕像素。
    ///
    /// 图像坐标是**交付图**的坐标，屏幕坐标是工具参数与输入注入用的坐标；
    /// 两者只在 `scale == 1` 时相同。
    pub fn project(&self, x: f64, y: f64) -> Result<(i32, i32), String> {
        if !(x >= 0.0 && y >= 0.0 && x <= self.image.0 as f64 && y <= self.image.1 as f64) {
            return Err(format!(
                "the point ({x}, {y}) is outside frame {} ({}x{} px); coordinates passed with from_frame are pixels *in that image*",
                self.id, self.image.0, self.image.1
            ));
        }
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        let sx = self.origin.0 as f64 + x / scale;
        let sy = self.origin.1 as f64 + y / scale;
        Ok((sx.round() as i32, sy.round() as i32))
    }
}

/// 拖拽句柄：让"接着上一笔画"不必重新定位起点。
#[derive(Debug, Clone)]
pub struct DragAnchor {
    /// 句柄 id（回执里给模型）。
    pub handle: String,
    /// 结束点（屏幕像素）。
    pub end: (i32, i32),
    /// 记录时刻（unix 毫秒）。
    pub at_ms: u128,
}

/// 扩展的进程内状态。
#[derive(Debug, Default)]
pub struct State {
    /// 「电脑操作」是否已开启（`/computer on`）。
    pub enabled: bool,
    /// 帧注册表，最旧的在前面。
    pub frames: VecDeque<Frame>,
    /// 帧序号（只增不减，避免 id 复用）。
    pub frame_seq: u64,
    /// 最近一次拖拽的结束点。
    pub drag_anchor: Option<DragAnchor>,
    /// 拖拽句柄序号。
    pub drag_seq: u64,
    /// 仍按住的键（camel 名，如 `ctrl`、`a`）。
    pub held_keys: Vec<String>,
    /// 仍按住的鼠标键（`left` / `right` / `middle`）。
    pub held_buttons: Vec<String>,
}

/// 全局状态单元；首次访问时初始化。
fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(State::default()))
}

/// 取状态锁；锁中毒时恢复内部值（状态是纯内存记账，没有"半写坏"的结构）。
pub fn lock() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// 当前时间（unix 毫秒）；时钟异常时返回 0，调用方只用于比较新鲜度。
pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// 「电脑操作」是否开启。
pub fn enabled() -> bool {
    lock().enabled
}

/// 设置「电脑操作」开关；关闭时清掉帧、拖拽句柄与按住记录（不留跨开合的引用）。
pub fn set_enabled(on: bool) {
    let mut st = lock();
    st.enabled = on;
    if !on {
        st.frames.clear();
        st.drag_anchor = None;
        st.held_keys.clear();
        st.held_buttons.clear();
    }
}

/// 登记一张新截图并返回它的帧 id。
#[must_use]
pub fn register_frame(
    origin: (i32, i32),
    screen: (u32, u32),
    image: (u32, u32),
    window: Option<(u64, Rect)>,
) -> String {
    let mut st = lock();
    st.frame_seq += 1;
    let id = format!("f{}", st.frame_seq);
    let (window_id, window_rect) = match window {
        Some((id, rect)) => (Some(id), Some(rect)),
        None => (None, None),
    };
    let scale = if screen.0 == 0 || screen.1 == 0 {
        1.0
    } else {
        image.0 as f64 / screen.0 as f64
    };
    st.frames.push_back(Frame {
        id: id.clone(),
        origin,
        screen,
        image,
        scale,
        window: window_id,
        window_rect,
        created_ms: now_ms(),
    });
    while st.frames.len() > FRAME_KEEP {
        st.frames.pop_front();
    }
    id
}

/// 按 id 取一帧的副本。
pub fn frame(id: &str) -> Option<Frame> {
    lock().frames.iter().find(|f| f.id == id).cloned()
}

/// 登记一个拖拽结束点并返回句柄 id。
#[must_use]
pub fn remember_drag(end: (i32, i32)) -> String {
    let mut st = lock();
    st.drag_seq += 1;
    let handle = format!("d{}", st.drag_seq);
    st.drag_anchor = Some(DragAnchor {
        handle: handle.clone(),
        end,
        at_ms: now_ms(),
    });
    handle
}

/// 按句柄（或"最近一次"）取拖拽锚点；过期或不存在时返回可操作的错误。
pub fn drag_anchor(handle: Option<&str>) -> Result<(i32, i32), String> {
    let st = lock();
    let Some(anchor) = st.drag_anchor.as_ref() else {
        return Err(
            "there is no previous drag in this session; pass from_x/from_y (or path)".to_string(),
        );
    };
    if let Some(want) = handle
        && want != anchor.handle
    {
        return Err(format!(
            "drag handle {want} is not the most recent one ({}); the anchor only survives until the next drag",
            anchor.handle
        ));
    }
    if now_ms().saturating_sub(anchor.at_ms) > DRAG_ANCHOR_TTL_MS {
        return Err(format!(
            "drag handle {} has expired; pass from_x/from_y again",
            anchor.handle
        ));
    }
    Ok(anchor.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(origin: (i32, i32), screen: (u32, u32), image: (u32, u32)) -> Frame {
        Frame {
            id: "f1".to_string(),
            origin,
            screen,
            image,
            scale: image.0 as f64 / screen.0 as f64,
            window: None,
            window_rect: None,
            created_ms: now_ms(),
        }
    }

    #[test]
    fn project_is_identity_at_scale_one() {
        let f = make_frame((100, 50), (800, 600), (800, 600));
        assert_eq!(f.project(0.0, 0.0).unwrap(), (100, 50));
        assert_eq!(f.project(799.0, 599.0).unwrap(), (899, 649));
    }

    #[test]
    fn project_divides_by_the_delivered_scale() {
        let f = make_frame((0, 0), (800, 600), (400, 300));
        assert_eq!(f.project(200.0, 150.0).unwrap(), (400, 300));
    }

    #[test]
    fn project_rejects_points_outside_the_image() {
        let f = make_frame((0, 0), (800, 600), (800, 600));
        assert!(f.project(801.0, 10.0).is_err());
        assert!(f.project(-1.0, 10.0).is_err());
    }

    #[test]
    fn frames_are_kept_and_evicted() {
        for _ in 0..(FRAME_KEEP + 3) {
            register_frame((0, 0), (10, 10), (10, 10), None);
        }
        let st = lock();
        assert_eq!(st.frames.len(), FRAME_KEEP);
        let first = st.frames.front().unwrap().id.clone();
        drop(st);
        assert!(frame(&first).is_some());
    }
}

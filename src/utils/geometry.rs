//! 界面几何的纯算术：滚动条滑块、拖拽换算与浮层夹取。
//!
//! 全是 `f32` 进 `f32` 出，不碰任何窗口/渲染类型，所以 TUI 与 GUI 都能用；
//! 单位换算（物理像素 ↔ 设计像素）由调用方在传参前做。

/// 滑块高度：可视高在内容高里的占比 × 可视高，不低于 `min_thumb`。
///
/// `viewport` 是滚动区可视高度，`max` 是可滚动的额外距离（内容高 − 可视高）。
pub fn thumb_height(viewport: f32, max: f32, min_thumb: f32) -> f32 {
    (viewport * viewport / (viewport + max)).max(min_thumb)
}

/// 滑块能移动的长度（轨道高减滑块高），至少 1 以免除零。
pub fn thumb_travel(viewport: f32, max: f32, min_thumb: f32) -> f32 {
    (viewport - thumb_height(viewport, max, min_thumb)).max(1.)
}

/// 滑块顶端位置：`offset` 取 `[-max, 0]`，越界也不会把滑块甩出轨道。
pub fn thumb_top(offset: f32, max: f32, travel: f32) -> f32 {
    (-offset / max).clamp(0., 1.) * travel
}

/// 拖动换算：抓取点位移 `dy`（鼠标向下为正）→ 新滚动偏移，保证滑块跟着鼠标走。
///
/// `start` 是按下时的偏移，`travel` 是滑块可移动长度。
pub fn drag_offset(start: f32, dy: f32, max: f32, travel: f32) -> f32 {
    start - dy * max / travel
}

/// 把 `size` 大小的浮层贴在 `anchor` 处，同时收进 `viewport` 内并留出 `margin` 边距。
///
/// 浮层比窗口还大时退到 `margin`，不会算出负的位置。
pub fn clamp_within(anchor: f32, size: f32, viewport: f32, margin: f32) -> f32 {
    let max = (viewport - size - margin).max(margin);
    anchor.clamp(margin, max)
}

/// 拖拽分隔条后的宽度：`press` 是按下时的宽度，`dx` 是鼠标位移（往变宽的方向为正），
/// `scale` 是界面缩放系数，`limits` 是宽度上下限。
///
/// 位移与宽度往往不同单位（鼠标事件是物理像素、布局尺寸是设计像素 × 缩放系数），
/// 直接相加会让宽度按缩放倍数跑得比鼠标快，所以位移一律先折回宽度单位。
pub fn resized_width(press: f32, dx: f32, scale: f32, limits: (f32, f32)) -> f32 {
    (press + dx / scale).clamp(limits.0, limits.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 内容越长滑块越短，但不会短过最小高度。
    #[test]
    fn thumb_height_shrinks_with_content() {
        assert_eq!(thumb_height(1000., 9000., 24.), 100.);
        assert!(thumb_height(1000., 90_000., 24.) < 100.);
        assert_eq!(thumb_height(1000., 90_000., 24.), 24.);
    }

    /// 拖满整条轨道 = 滚到另一端，且滑块位移与内容偏移严格成比例。
    #[test]
    fn drag_offset_follows_the_thumb() {
        let (viewport, max) = (1000., 9000.);
        let travel = thumb_travel(viewport, max, 24.);
        // 抓在顶部往下拖满轨道 → 滚到底
        assert_eq!(drag_offset(0., travel, max, travel), -max);
        // 拖一半 → 滚一半
        assert_eq!(drag_offset(0., travel / 2., max, travel), -max / 2.);
        // 往上拖回一半 → 偏移跟着回来
        assert_eq!(drag_offset(-max, -travel / 2., max, travel), -max / 2.);
    }

    /// 偏移区间线性映射到轨道区间。
    #[test]
    fn thumb_top_maps_offset_range() {
        let (max, travel) = (9000., 900.);
        assert_eq!(thumb_top(0., max, travel), 0.);
        assert_eq!(thumb_top(-max, max, travel), travel);
        assert_eq!(thumb_top(-max / 2., max, travel), travel / 2.);
        // 越界偏移不会把滑块甩出轨道
        assert_eq!(thumb_top(-max * 2., max, travel), travel);
        assert_eq!(thumb_top(max, max, travel), 0.);
    }

    /// 浮层贴锚点，但不越过窗口边缘；尺寸超过窗口时退到边距。
    #[test]
    fn clamp_within_keeps_overlay_inside_the_window() {
        assert_eq!(clamp_within(300., 280., 1940., 16.), 300.);
        assert_eq!(clamp_within(1800., 280., 1940., 16.), 1644.);
        assert_eq!(clamp_within(10., 280., 200., 16.), 16.);
    }

    /// 拖拽宽度：位移按缩放折回宽度单位，越界夹到上下限。
    #[test]
    fn resized_width_folds_scale_and_clamps() {
        let limits = (180., 460.);
        assert_eq!(resized_width(252., 40., 1., limits), 292.);
        // 缩放 2 倍时同样的鼠标位移只对应一半的设计宽度
        assert_eq!(resized_width(252., 40., 2., limits), 272.);
        assert_eq!(resized_width(252., -9999., 1., limits), 180.);
        assert_eq!(resized_width(252., 9999., 1., limits), 460.);
    }
}

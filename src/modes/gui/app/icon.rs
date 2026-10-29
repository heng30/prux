//! 代码绘制的图标：不依赖字体字形，尺寸与颜色都跟主题走。
//!
//! 每个函数返回可直接塞进 `child()` 的元素。

use super::theme::{Theme, px};
use gpui::{AnyElement, IntoElement, div, prelude::*};

/// 停止方块的边长（逻辑像素）。
pub(super) const STOP_SIZE: f32 = 10.;

/// 播放器风格的停止方块（忙碌时发送按钮的内容）：品牌色实心方块。
pub(super) fn stop(theme: &Theme) -> AnyElement {
    div()
        .w(px(STOP_SIZE))
        .h(px(STOP_SIZE))
        .rounded(px(2.))
        .bg(theme.accent)
        .into_any_element()
}

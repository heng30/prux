//! 代码绘制的图标：不依赖字体字形，尺寸与颜色都跟主题走。
//!
//! 每个函数返回可直接塞进 `child()` 的元素。

use super::theme::{RADIUS_SM, Theme, px};
use gpui::{AnyElement, IntoElement, div, prelude::*};

/// 图标外框边长
const ICON_SIZE: f32 = 14.;

/// 播放器风格的停止方块（忙碌时发送按钮的内容）：品牌色实心方块。
pub(super) fn stop(theme: &Theme) -> AnyElement {
    const STOP_SIZE: f32 = 12.;
    div()
        .w(px(STOP_SIZE))
        .h(px(STOP_SIZE))
        .rounded(px(RADIUS_SM))
        .bg(theme.accent)
        .into_any_element()
}

/// 复制图标：两张叠放的圆角纸片（前排用卡片底色挡住后排的描边）。
///
/// 供代码块首行右侧的复制按钮用；颜色跟主题的次要文字色。
pub(crate) fn copy(theme: &Theme) -> AnyElement {
    /// 单张纸片的边长
    const SHEET_SIZE: f32 = 10.;
    /// 纸片圆角
    const SHEET_RADIUS: f32 = 2.;

    let sheet = || {
        div()
            .w(px(SHEET_SIZE))
            .h(px(SHEET_SIZE))
            .rounded(px(SHEET_RADIUS))
            .border_1()
            .border_color(theme.text_tertiary)
    };

    div()
        .relative()
        .w(px(ICON_SIZE))
        .h(px(ICON_SIZE))
        .child(sheet().absolute().bottom_0().left_0())
        .child(sheet().absolute().top_0().right_0().bg(theme.inset))
        .into_any_element()
}

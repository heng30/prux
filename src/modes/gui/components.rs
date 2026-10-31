//! 共享小部件：小标签、圆点、分节标题、空状态。全部只负责样式，交互由调用方挂。

use super::theme::{
    CONTROL_H_SM, FONT_CAPTION, FONT_SMALL, RADIUS_MD, SPACE_SM, SPACE_XS, Theme, px,
};
use gpui::{AnyElement, FontWeight, IntoElement, ParentElement, SharedString, Styled, div};

/// 圆角标签：`active` 为真时用强调描边与主文本色。
pub fn chip(theme: &Theme, text: impl Into<SharedString>, active: bool) -> gpui::Div {
    let text = text.into();
    let mut element = div()
        .h(px(CONTROL_H_SM))
        .px(px(SPACE_SM))
        .rounded(px(RADIUS_MD))
        .flex()
        .items_center()
        .text_size(px(FONT_SMALL))
        .border_1()
        .border_color(if active {
            theme.border_strong
        } else {
            theme.border
        })
        .text_color(if active {
            theme.text
        } else {
            theme.text_secondary
        })
        .child(text);
    if active {
        element = element.bg(theme.overlay);
    }
    element
}

/// 状态圆点：画在文本旁的品牌点与状态灯。
///
/// 自带 2px 下移——`items_center` 居中的是行盒，而字形视觉中心（x-height 中心）比行盒中心
/// 低约 2px（12px 字号实测），不补这一段圆点看起来就偏上。`mt` 取 4px 是因为 flex 居中
/// 按 margin box 对半分。
pub fn dot(color: gpui::Hsla, size: f32) -> AnyElement {
    div()
        .w(px(size))
        .h(px(size))
        .mt(px(SPACE_XS))
        .rounded(px(size / 2.))
        .bg(color)
        .flex_none()
        .into_any_element()
}

/// 分节标题（全大写式的小字，用作侧栏/面板的表头）。
pub fn section_label(theme: &Theme, text: impl Into<SharedString>) -> AnyElement {
    div()
        .px(px(SPACE_SM))
        .text_size(px(FONT_CAPTION))
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme.text_ghost)
        .child(text.into())
        .into_any_element()
}

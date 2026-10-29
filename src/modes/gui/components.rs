//! 共享小部件：小标签、圆点、分节标题、空状态。全部只负责样式，交互由调用方挂。

use super::theme::{CONTROL_H_SM, FONT_CAPTION, FONT_SMALL, RADIUS_MD, SPACE_SM, Theme, px};
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

/// 状态圆点。
pub fn dot(color: gpui::Hsla, size: f32) -> AnyElement {
    div()
        .w(px(size))
        .h(px(size))
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

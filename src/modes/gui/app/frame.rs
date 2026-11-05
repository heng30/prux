//! 自绘窗口边框：窗口装饰请求 + 标题栏（拖动、最小化/最大化/关闭）。
//!
//! `gui.frame` 为真时窗口不带系统边框，标题栏由这里绘制；为假时全部交给窗口管理器/系统。

use super::{
    theme::{FONT_CAPTION, FONT_SMALL, RADIUS_SM, SPACE_MD},
    *,
};
use crate::APP_NAME;
use gpui::Hsla;

/// 自绘标题栏高度（逻辑像素）。
const TITLEBAR_H: f32 = 24.;

/// macOS 红绿灯（关闭/最小化/最大化）按钮占掉的左边宽度（逻辑像素）：自绘标题栏要让开这段。
const MAC_TRAFFIC_LIGHT_W: f32 = 72.;

/// 把 `gui.frame` 应用到窗口装饰：值变了才请求一次，避免每帧都发请求。
///
/// Linux 上立刻生效；Windows / macOS 的 `request_decorations` 是空实现，装饰在建窗时定。
pub(super) fn apply_decoration(window: &mut Window, applied: &mut Option<bool>, frame_gui: bool) {
    if *applied == Some(frame_gui) {
        return;
    }

    *applied = Some(frame_gui);
    window.request_decorations(if frame_gui {
        WindowDecorations::Client
    } else {
        WindowDecorations::Server
    });
}

/// 自绘标题栏：左侧窗口名，中间拖动区，右侧三个窗口按钮。
///
/// macOS 的红绿灯由系统画在左上角：左边留白让开，不再画自己的按钮。
pub(super) fn titlebar(theme: &Theme) -> AnyElement {
    let ink = theme.text_secondary;
    let mut bar = div()
        .id("frame-titlebar")
        .flex()
        .flex_none()
        .items_center()
        .h(px(TITLEBAR_H))
        .w_full()
        .bg(theme.sidebar)
        .border_b_1()
        .border_color(theme.border)
        .on_click(|event, window, _| {
            if event.click_count() == 2 {
                window.titlebar_double_click();
            }
        })
        .pl(px(if cfg!(target_os = "macos") {
            MAC_TRAFFIC_LIGHT_W
        } else {
            SPACE_MD
        }))
        .child(
            div()
                .flex_none()
                .text_size(px(FONT_CAPTION))
                .text_color(ink)
                .child(SharedString::from(APP_NAME)),
        )
        // 拖动区是独立子元素：Windows 的命中区按注册顺序取第一个，盖在按钮上的命中区
        // 会把 HTCLOSE / HTMINBUTTON 抢成 HTCAPTION
        .child(
            div()
                .id("frame-drag")
                .flex_1()
                .h_full()
                .window_control_area(WindowControlArea::Drag)
                .on_mouse_down(MouseButton::Left, |_, window, cx| {
                    cx.stop_propagation();
                    window.start_window_move();
                }),
        );

    if !cfg!(target_os = "macos") {
        bar = bar
            .child(frame_button("frame-min", WindowControlArea::Min, ink))
            .child(frame_button("frame-max", WindowControlArea::Max, ink))
            .child(frame_button("frame-close", WindowControlArea::Close, ink));
    }

    bar.into_any_element()
}

/// 自绘标题栏上的一颗窗口按钮。
///
/// `area` 让 Windows 把命中区交回系统的 HTMINBUTTON / HTMAXBUTTON / HTCLOSE，
/// 其它平台在按下时直接调对应的窗口方法。图标是代码画的细线，不依赖字体里的符号。
fn frame_button(id: &'static str, area: WindowControlArea, ink: Hsla) -> AnyElement {
    let icon = match area {
        WindowControlArea::Min => div().w(px(9.)).h(px(1.)).bg(ink).into_any_element(),
        WindowControlArea::Max => div()
            .w(px(8.))
            .h(px(8.))
            .border_1()
            .border_color(ink)
            .into_any_element(),
        _ => div()
            .text_size(px(FONT_SMALL))
            .text_color(ink)
            .child(SharedString::from("×"))
            .into_any_element(),
    };

    div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(26.))
        .h(px(TITLEBAR_H - 6.))
        .rounded(px(RADIUS_SM))
        .window_control_area(area)
        .on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            match area {
                WindowControlArea::Min => window.minimize_window(),
                WindowControlArea::Max => window.zoom_window(),
                WindowControlArea::Close => window.remove_window(),
                WindowControlArea::Drag => window.start_window_move(),
            }
        })
        .child(icon)
        .into_any_element()
}

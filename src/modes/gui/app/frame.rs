//! 自绘窗口边框：窗口装饰请求 + 标题栏（拖动、最小化/最大化/关闭）。
//!
//! `gui.frame` 为真时窗口不带系统边框，标题栏由这里绘制；为假时全部交给窗口管理器/系统。
//!
//! Windows 是例外：gpui 的 Windows 后端建窗时从不给窗口画 `WS_CAPTION`，也没有系统标题栏可留，
//! 所以 `gui.frame` 在那里恒为 `true`（见 `settings_manager::read_settings_frame_gui`）。
//! 它的拖动与三颗按钮都由系统的非客户区命中（`HTCAPTION` / `HTMINBUTTON` / `HTMAXBUTTON` /
//! `HTCLOSE`）处理，因此**不能**在 `on_mouse_down` 里消费按下事件。

use super::{
    theme::{FONT_CAPTION, FONT_SMALL, RADIUS_SM, SPACE_MD},
    *,
};
use crate::APP_NAME;
use gpui::Hsla;

#[cfg(target_os = "windows")]
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
#[cfg(target_os = "windows")]
use windows::Win32::{
    Foundation::HWND,
    UI::WindowsAndMessaging::{SW_RESTORE, ShowWindowAsync},
};

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
/// `maximized` 为真时最大化按钮显示「还原」图标；放大/还原都会改窗口尺寸，
/// gpui 缓存命中不了就重跑渲染，所以这里读到的状态每帧都是新的。
pub(super) fn titlebar(theme: &Theme, maximized: bool) -> AnyElement {
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
        .child(drag_area());

    if !cfg!(target_os = "macos") {
        bar = bar
            .child(frame_button(
                "frame-min",
                WindowControlArea::Min,
                ink,
                theme.sidebar,
                maximized,
            ))
            .child(frame_button(
                "frame-max",
                WindowControlArea::Max,
                ink,
                theme.sidebar,
                maximized,
            ))
            .child(frame_button(
                "frame-close",
                WindowControlArea::Close,
                ink,
                theme.sidebar,
                maximized,
            ));
    }

    // 双击标题栏空白处放大 / 还原：macOS 交给系统标题栏行为，Linux 交给窗口管理器
    // （两者都会把抬起事件送到应用，按 click 次数判断就行）。
    // Windows 上 `titlebar_double_click` 是空实现、窗口类也没注册 `CS_DBLCLKS`
    // （系统不发 `WM_NCLBUTTONDBLCLK`），而且按下会被系统的拖动循环连同抬起一起吞掉，
    // 只能自己在**按下**时判断：双击那一次自己吃掉，单击放行给 `DefWindowProc` 进入拖动循环。
    #[cfg(target_os = "windows")]
    {
        bar = bar.on_mouse_down(MouseButton::Left, |event, window, cx| {
            if event.click_count == 2 {
                cx.stop_propagation();
                toggle_maximized(window);
            }
        });
    }
    #[cfg(not(target_os = "windows"))]
    {
        bar = bar.on_click(|event, window, _| {
            if event.click_count() == 2 {
                window.titlebar_double_click();
            }
        });
    }

    bar.into_any_element()
}

/// 标题栏中间的拖动区：窗口名与右侧按钮之间的全部宽度。
///
/// Windows 上 `WindowControlArea::Drag` 会让系统把这块命中成 `HTCAPTION`，按下后由
/// `DefWindowProc` 进入窗口拖动循环；因此那里**不能**再挂 `on_mouse_down`——应用一旦消费
/// 按下事件，gpui 就不再调 `DefWindowProc`，而 Windows 后端也没实现 `start_window_move`，
/// 窗口会彻底拖不动（`cx.stop_propagation()` 正是这样把它堵死的）。
/// macOS / Linux 相反，由应用自己发起拖动。
fn drag_area() -> AnyElement {
    #[allow(unused_mut)] // Windows 走系统拖动，下面那段不编译，`area` 不会再被赋值
    let mut area = div()
        .id("frame-drag")
        .flex_1()
        .h_full()
        .window_control_area(WindowControlArea::Drag);

    #[cfg(not(target_os = "windows"))]
    {
        area = area.on_mouse_down(MouseButton::Left, |_, window, cx| {
            cx.stop_propagation();
            window.start_window_move();
        });
    }

    area.into_any_element()
}

/// 自绘标题栏上的一颗窗口按钮。
///
/// `area` 让 Windows 把命中区交回系统的 HTMINBUTTON / HTMAXBUTTON / HTCLOSE，
/// 其它平台在按下时直接调对应的窗口方法。图标是代码画的细线，不依赖字体里的符号。
/// `bg` 是标题栏底色，只用来遮住「还原」图标里两个方框的重叠处。
/// `maximized` 为真时最大化按钮画成两个错开的方框（还原）。
fn frame_button(
    id: &'static str,
    area: WindowControlArea,
    ink: Hsla,
    bg: Hsla,
    maximized: bool,
) -> AnyElement {
    let icon = match area {
        WindowControlArea::Min => div().w(px(9.)).h(px(1.)).bg(ink).into_any_element(),
        WindowControlArea::Max if maximized => div()
            .relative()
            .w(px(9.))
            .h(px(9.))
            .child(
                div()
                    .absolute()
                    .top_0()
                    .right_0()
                    .w(px(6.5))
                    .h(px(6.5))
                    .border_1()
                    .border_color(ink),
            )
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .left_0()
                    .w(px(6.5))
                    .h(px(6.5))
                    .bg(bg)
                    .border_1()
                    .border_color(ink),
            )
            .into_any_element(),
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

    // Windows 的三颗按钮由平台在 `WM_NCLBUTTONUP` 里按 HT* 处理：同样不能消费按下事件，
    // 否则平台收不到这次点击；而且应用自己调 `zoom_window()` 只会最大化、还原不了。
    #[allow(unused_mut)] // Windows 走平台处理，下面那段不编译，`button` 不会再被赋值
    let mut button = div()
        .id(id)
        .flex()
        .flex_none()
        .items_center()
        .justify_center()
        .w(px(26.))
        .h(px(TITLEBAR_H - 6.))
        .rounded(px(RADIUS_SM))
        .window_control_area(area);

    #[cfg(not(target_os = "windows"))]
    {
        button = button.on_mouse_down(MouseButton::Left, move |_, window, cx| {
            cx.stop_propagation();
            match area {
                WindowControlArea::Min => window.minimize_window(),
                WindowControlArea::Max => window.zoom_window(),
                WindowControlArea::Close => window.remove_window(),
                WindowControlArea::Drag => window.start_window_move(),
            }
        });
    }

    button.child(icon).into_any_element()
}

/// Windows 上在「最大化 / 还原」之间切换。
///
/// gpui 的 `zoom()` 在 Windows 上只有一句 `ShowWindowAsync(SW_MAXIMIZE)`，没有还原分支，
/// 所以最大化之后再调它仍是最大化；还原要自己发 `SW_RESTORE`，窗口句柄从
/// `raw_window_handle::HasWindowHandle` 取（`gpui::Window` 实现了该 trait）。
#[cfg(target_os = "windows")]
fn toggle_maximized(window: &Window) {
    if !window.is_maximized() {
        window.zoom_window();
        return;
    }

    // 全限定调用：`Window::window_handle()` 的同名固有方法是取 gpui 自己的 `AnyWindowHandle`
    let Ok(handle) = HasWindowHandle::window_handle(window) else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };

    // SAFETY: 句柄来自当前正在使用的 gpui 平台窗口，`window` 借用期间窗口不会被销毁；
    // `ShowWindowAsync` 只投递一次窗口状态切换，不保留指针，也不修改任何 Rust 侧状态。
    unsafe {
        let hwnd = HWND(win32.hwnd.get() as *mut core::ffi::c_void);
        _ = ShowWindowAsync(hwnd, SW_RESTORE);
    }
}

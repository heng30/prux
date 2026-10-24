//! 输入注入：鼠标移动/点击/拖拽/滚轮、键盘按键与文本，统一走 `enigo`。
//!
//! 这里是**同步**代码：调用方必须在 `spawn_blocking` 里跑它，别占着异步运行时的线程。
//! Wayland 下 `enigo` 用 `zwp_virtual_keyboard_v1` / `zwlr_virtual_pointer_v1`（sway、Hyprland 等
//! wlroots 系合成器实现；GNOME/KDE 不开放这两个协议，会返回"无法注入"）。

use crate::extensions::computer::state;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use std::{
    ops::{Deref, DerefMut},
    sync::{Mutex, MutexGuard},
    thread::sleep,
    time::Duration,
};

/// 文本输入的字符间停顿：太快时应用会丢字符。
const TYPE_DELAY_MS: u64 = 8;

/// 拖拽路径的采样间隔（毫秒）：太小会让合成器合并移动事件。
const DRAG_STEP_MS: u64 = 8;

/// 一格滚轮折算的 Wayland axis 值（合成器按 15 处理一格是常见约定）。
const WHEEL_NOTCH: f64 = 15.0;

/// 进程内复用的输入连接。
///
/// `enigo` 每 `new` 一次都要在合成器上重新绑定 `zwp_virtual_keyboard_v1` /
/// `zwlr_virtual_pointer_v1`；反复建连会让后续的 `new` 直接失败（只报出聚合的
/// `no successful connection`），所以成功建立的那条连接留着复用。
static CONNECTION: Mutex<Option<Enigo>> = Mutex::new(None);

/// 一次输入操作借用的连接。
///
/// `Drop` 时只有 `reusable` 为真（本次操作成功）才把连接还回 `CONNECTION`；
/// 出错说明连接或合成器状态可疑，丢弃它让下次操作重建，从而能自愈。
struct Conn {
    /// 借到的输入连接；`Drop` 取走后为 `None`。
    inner: Option<Enigo>,
    /// `true` 表示本次操作成功后连接仍健康，可以还池复用。
    reusable: bool,
}

impl Deref for Conn {
    type Target = Enigo;

    fn deref(&self) -> &Self::Target {
        self.inner
            .as_ref()
            .expect("the connection is only taken in Drop")
    }
}

impl DerefMut for Conn {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.inner
            .as_mut()
            .expect("the connection is only taken in Drop")
    }
}

impl Drop for Conn {
    /// 还池时只取连接池锁，不碰 `state`：调用方可能仍持有别的锁。
    fn drop(&mut self) {
        if !self.reusable {
            return;
        }
        if let Some(enigo) = self.inner.take() {
            *pool() = Some(enigo);
        }
    }
}

/// 取连接池的锁；持锁线程 panic 留下的中毒状态不影响后续复用。
fn pool() -> MutexGuard<'static, Option<Enigo>> {
    match CONNECTION.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 借一条输入连接：优先复用池里的，没有就新建。
fn connect() -> Result<Conn, String> {
    if let Some(enigo) = pool().take() {
        return Ok(Conn {
            inner: Some(enigo),
            reusable: false,
        });
    }

    let settings = Settings {
        release_keys_when_dropped: true,
        ..Settings::default()
    };
    match Enigo::new(&settings) {
        Ok(enigo) => Ok(Conn {
            inner: Some(enigo),
            reusable: false,
        }),
        Err(e) => Err(format!(
            "could not open an input channel: {e}. {}",
            connect_hint()
        )),
    }
}

/// 借一条连接执行一次输入操作。
///
/// 操作成功时连接还池复用；失败时连接被丢弃，下次操作重新建立。
fn with_enigo<T>(f: impl FnOnce(&mut Enigo) -> Result<T, String>) -> Result<T, String> {
    let mut conn = connect()?;
    let out = f(&mut conn)?;
    conn.reusable = true;
    Ok(out)
}

/// `connect()` 失败时补一句可诊断的原因：Wayland 下直接探测 socket 能否连上。
#[cfg(target_os = "linux")]
fn connect_hint() -> String {
    let (Ok(runtime), Ok(display)) = (
        std::env::var("XDG_RUNTIME_DIR"),
        std::env::var("WAYLAND_DISPLAY"),
    ) else {
        return "no Wayland session: WAYLAND_DISPLAY or XDG_RUNTIME_DIR is unset".to_string();
    };

    let socket = std::path::PathBuf::from(runtime).join(display);
    match std::os::unix::net::UnixStream::connect(&socket) {
        Ok(_) => format!(
            "{} accepts connections, so this compositor exposes neither \
             zwp_virtual_keyboard_v1 nor zwlr_virtual_pointer_v1 (sway and Hyprland do; \
             GNOME and KDE do not)",
            socket.display()
        ),
        Err(err) => format!(
            "cannot connect to the Wayland socket {}: {err}",
            socket.display()
        ),
    }
}

/// 非 Linux 平台的 `connect()` 失败提示：没有协议级探测可做。
#[cfg(not(target_os = "linux"))]
fn connect_hint() -> String {
    "check that the session permits synthetic input (macOS: Accessibility permission; Windows: \
     an interactive desktop)"
        .to_string()
}

/// 解析鼠标键名。
fn parse_button(name: &str) -> Result<Button, String> {
    match name.to_ascii_lowercase().as_str() {
        "" | "left" => Ok(Button::Left),
        "right" => Ok(Button::Right),
        "middle" => Ok(Button::Middle),
        other => Err(format!(
            "unknown mouse button {other:?} (left, right, middle)"
        )),
    }
}

/// 解析一个键名：修饰键、功能键、命名键，或单个可见字符。
fn parse_key(name: &str) -> Result<Key, String> {
    let lower = name.trim().to_ascii_lowercase();
    let key = match lower.as_str() {
        "ctrl" | "control" => Key::Control,
        "shift" => Key::Shift,
        "alt" | "option" => Key::Alt,
        "meta" | "win" | "super" | "cmd" | "command" => Key::Meta,
        "enter" | "return" => Key::Return,
        "esc" | "escape" => Key::Escape,
        "tab" => Key::Tab,
        "space" => Key::Space,
        "backspace" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        #[cfg(not(target_os = "macos"))]
        "insert" => Key::Insert,
        #[cfg(target_os = "macos")]
        "insert" => return Err(missing_on_macos("insert")),
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "page_up" | "pgup" => Key::PageUp,
        "pagedown" | "page_down" | "pgdn" => Key::PageDown,
        "up" | "up_arrow" | "uparrow" => Key::UpArrow,
        "down" | "down_arrow" | "downarrow" => Key::DownArrow,
        "left" | "left_arrow" | "leftarrow" => Key::LeftArrow,
        "right" | "right_arrow" | "rightarrow" => Key::RightArrow,
        "capslock" | "caps_lock" => Key::CapsLock,
        #[cfg(not(target_os = "macos"))]
        "printscreen" | "print_screen" => Key::PrintScr,
        #[cfg(target_os = "macos")]
        "printscreen" | "print_screen" => return Err(missing_on_macos("print screen")),
        other => {
            if let Some(num) = other.strip_prefix('f')
                && let Ok(n) = num.parse::<u8>()
                && (1..=12).contains(&n)
            {
                return Ok(match n {
                    1 => Key::F1,
                    2 => Key::F2,
                    3 => Key::F3,
                    4 => Key::F4,
                    5 => Key::F5,
                    6 => Key::F6,
                    7 => Key::F7,
                    8 => Key::F8,
                    9 => Key::F9,
                    10 => Key::F10,
                    11 => Key::F11,
                    _ => Key::F12,
                });
            }
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => Key::Unicode(c),
                _ => return Err(format!("unknown key {name:?}")),
            }
        }
    };
    Ok(key)
}

/// macOS 的键盘接口没有这些键（`enigo` 在该平台上不提供对应变体）时的报错文案。
#[cfg(target_os = "macos")]
fn missing_on_macos(name: &str) -> String {
    format!("macOS has no {name} key; use a different key or computer_text to insert text")
}

/// 把 `ctrl+shift+t` 拆成修饰键列表与主键。
fn parse_chord(chord: &str) -> Result<(Vec<Key>, Key), String> {
    let parts: Vec<&str> = chord
        .split('+')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return Err("keys is empty".to_string());
    }

    let mut modifiers = Vec::new();
    for part in &parts[..parts.len() - 1] {
        let key = parse_key(part)?;
        if !matches!(key, Key::Control | Key::Shift | Key::Alt | Key::Meta) {
            return Err(format!(
                "{part:?} is not a modifier; only the last part of a chord is the key itself"
            ));
        }
        modifiers.push(key);
    }

    let main = parse_key(parts[parts.len() - 1])?;
    Ok((modifiers, main))
}

/// 把指针移到绝对坐标（屏幕像素）。
pub fn move_to(x: i32, y: i32) -> Result<(), String> {
    with_enigo(|enigo| {
        enigo
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| format!("pointer move to ({x}, {y}) failed: {e}"))
    })
}

/// 在坐标处点击（可选按住修饰键与连击次数）。
pub fn click(x: i32, y: i32, button: &str, clicks: u32, modifiers: &str) -> Result<(), String> {
    let button = parse_button(button)?;
    let held = held_modifiers(modifiers)?;
    with_enigo(|enigo| {
        enigo
            .move_mouse(x, y, Coordinate::Abs)
            .map_err(|e| format!("pointer move to ({x}, {y}) failed: {e}"))?;
        for key in &held {
            enigo
                .key(*key, Direction::Press)
                .map_err(|e| format!("holding a modifier failed: {e}"))?;
            state::lock().held_keys.push(format!("{key:?}"));
        }

        let count = clicks.clamp(1, 3);
        let mut result = Ok(());
        for _ in 0..count {
            if let Err(e) = enigo.button(button, Direction::Click) {
                result = Err(format!("click at ({x}, {y}) failed: {e}"));
                break;
            }
            sleep(Duration::from_millis(40));
        }

        for key in held.iter().rev() {
            let _ = enigo.key(*key, Direction::Release);
            state::lock().held_keys.retain(|k| k != &format!("{key:?}"));
        }
        result
    })
}

/// 解析 `ctrl+shift` 这类修饰键串（点击/拖拽用）。
fn held_modifiers(spec: &str) -> Result<Vec<Key>, String> {
    let mut out = Vec::new();
    for part in spec.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        let key = parse_key(part)?;
        if !matches!(key, Key::Control | Key::Shift | Key::Alt | Key::Meta) {
            return Err(format!("{part:?} is not a modifier"));
        }
        out.push(key);
    }
    Ok(out)
}

/// 从起点拖到终点；给了 `path` 就沿途经点走。
pub fn drag(
    from: (i32, i32),
    to: (i32, i32),
    path: &[(i32, i32)],
    button: &str,
    modifiers: &str,
) -> Result<(), String> {
    let button = parse_button(button)?;
    let held = held_modifiers(modifiers)?;
    with_enigo(|enigo| {
        enigo
            .move_mouse(from.0, from.1, Coordinate::Abs)
            .map_err(|e| format!("pointer move to the start failed: {e}"))?;
        for key in &held {
            enigo
                .key(*key, Direction::Press)
                .map_err(|e| format!("holding a modifier failed: {e}"))?;
        }
        enigo
            .button(button, Direction::Press)
            .map_err(|e| format!("pressing the mouse button failed: {e}"))?;
        state::lock()
            .held_buttons
            .push(format!("{button:?}").to_lowercase());

        let mut points: Vec<(i32, i32)> = Vec::new();
        if path.is_empty() {
            let steps = 12;
            for i in 1..=steps {
                let t = f64::from(i) / f64::from(steps);
                let x = f64::from(from.0) + (f64::from(to.0) - f64::from(from.0)) * t;
                let y = f64::from(from.1) + (f64::from(to.1) - f64::from(from.1)) * t;
                points.push((x.round() as i32, y.round() as i32));
            }
        } else {
            points.extend_from_slice(path);
        }

        let mut failure = None;
        for (x, y) in points {
            if let Err(e) = enigo.move_mouse(x, y, Coordinate::Abs) {
                failure = Some(format!("drag move to ({x}, {y}) failed: {e}"));
                break;
            }
            sleep(Duration::from_millis(DRAG_STEP_MS));
        }

        // 释放永远要执行：拖拽中途失败时按住的键也必须放开，否则桌面会卡住一个按钮。
        _ = enigo.button(button, Direction::Release);
        state::lock()
            .held_buttons
            .retain(|b| b != &format!("{button:?}").to_lowercase());
        for key in held.iter().rev() {
            _ = enigo.key(*key, Direction::Release);
        }
        match failure {
            Some(err) => Err(err),
            None => Ok(()),
        }
    })
}

/// 滚轮：`notches` 格（正数向下/向右）。
pub fn scroll(vertical: i32, horizontal: i32) -> Result<(), String> {
    with_enigo(|enigo| {
        if vertical != 0 {
            enigo
                .scroll((f64::from(vertical) * WHEEL_NOTCH) as i32, Axis::Vertical)
                .map_err(|e| format!("vertical scroll failed: {e}"))?;
        }

        if horizontal != 0 {
            enigo
                .scroll(
                    (f64::from(horizontal) * WHEEL_NOTCH) as i32,
                    Axis::Horizontal,
                )
                .map_err(|e| format!("horizontal scroll failed: {e}"))?;
        }
        Ok(())
    })
}

/// 输入一段文本（Unicode，中文可用）。
pub fn type_text(text: &str) -> Result<(), String> {
    with_enigo(|enigo| {
        for ch in text.chars() {
            let s = ch.to_string();
            enigo
                .text(&s)
                .map_err(|e| format!("typing {ch:?} failed: {e}"))?;
            sleep(Duration::from_millis(TYPE_DELAY_MS));
        }
        Ok(())
    })
}

/// 按一次组合键（可重复、可按住若干秒）。
pub fn press_chord(chord: &str, repeat: u32, hold_seconds: f64) -> Result<(), String> {
    let (modifiers, main) = parse_chord(chord)?;
    with_enigo(|enigo| {
        if hold_seconds > 0.0 {
            for key in &modifiers {
                enigo
                    .key(*key, Direction::Press)
                    .map_err(|e| format!("holding a modifier failed: {e}"))?;
            }

            enigo
                .key(main, Direction::Press)
                .map_err(|e| format!("pressing {chord:?} failed: {e}"))?;
            sleep(Duration::from_secs_f64(hold_seconds.min(10.0)));

            _ = enigo.key(main, Direction::Release);
            for key in modifiers.iter().rev() {
                _ = enigo.key(*key, Direction::Release);
            }
            return Ok(());
        }

        for _ in 0..repeat.clamp(1, 100) {
            for key in &modifiers {
                enigo
                    .key(*key, Direction::Press)
                    .map_err(|e| format!("holding a modifier failed: {e}"))?;
            }
            enigo
                .key(main, Direction::Click)
                .map_err(|e| format!("pressing {chord:?} failed: {e}"))?;
            for key in modifiers.iter().rev() {
                let _ = enigo.key(*key, Direction::Release);
            }
            sleep(Duration::from_millis(20));
        }
        Ok(())
    })
}

/// 释放本会话记录的所有按住输入，返回被释放的项。
pub fn release_all() -> Result<Vec<String>, String> {
    let (keys, buttons) = {
        let st = state::lock();
        (st.held_keys.clone(), st.held_buttons.clone())
    };
    if keys.is_empty() && buttons.is_empty() {
        return Ok(Vec::new());
    }

    with_enigo(|enigo| {
        let mut released = Vec::new();
        for name in &buttons {
            if let Ok(button) = parse_button(name) {
                let _ = enigo.button(button, Direction::Release);
                released.push(format!("mouse:{name}"));
            }
        }

        for name in &keys {
            if let Ok(key) = parse_key(name.trim_matches('"')) {
                let _ = enigo.key(key, Direction::Release);
                released.push(format!("key:{name}"));
            }
        }

        let mut st = state::lock();
        st.held_keys.clear();
        st.held_buttons.clear();
        Ok(released)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords_split_modifiers_from_the_key() {
        let (modifiers, key) = parse_chord("ctrl+shift+t").unwrap();
        assert_eq!(modifiers, vec![Key::Control, Key::Shift]);
        assert_eq!(key, Key::Unicode('t'));
    }

    #[test]
    fn a_bare_key_is_a_chord_without_modifiers() {
        let (modifiers, key) = parse_chord("enter").unwrap();
        assert!(modifiers.is_empty());
        assert_eq!(key, Key::Return);
    }

    #[test]
    fn named_keys_and_function_keys_parse() {
        assert_eq!(parse_key("f5").unwrap(), Key::F5);
        assert_eq!(parse_key("esc").unwrap(), Key::Escape);
        assert_eq!(parse_key("win").unwrap(), Key::Meta);
        assert!(parse_key("not-a-key").is_err());
    }

    #[test]
    fn only_the_last_part_of_a_chord_may_be_a_non_modifier() {
        assert!(parse_chord("ctrl+a+b").is_err());
        assert!(parse_chord("a+ctrl").is_err());
    }
}

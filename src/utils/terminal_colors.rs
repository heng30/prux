//! 终端配色查询：OSC 10/11/4 一次往返取默认前景/背景与 16 个 ANSI 色。
//!
//! 查询串以**主设备属性请求（DA1，`CSI c`）结尾**：所有终端都会回 DA1，且终端按顺序回复，
//! 因此 DA1 的回复标志着配色回复结束（对完全忽略配色查询的终端同样成立）。
//!
//! 只在 stdin/stdout 都是 TTY 时查询；超时（默认 100ms）返回已收到的部分。
//! 查询会**短暂开启 raw 模式**（否则行缓冲会把回复攒到换行、`echo` 会把回复打到屏幕上），
//! 结束后恢复——因此必须在 TUI 接管输入**之前**调用一次。

use crate::utils::color::Rgb;
use std::{
    io::Write,
    sync::OnceLock,
    time::{Duration, Instant},
};

/// 配色查询默认超时（毫秒）
pub const QUERY_TIMEOUT_MS: u64 = 100;

/// ANSI 调色板大小（OSC 4 查询 0-15）
const PALETTE_SIZE: usize = 16;

/// 关闭查询的覆盖开关值（`PRUX_TERMINAL_COLORS=0`）
#[cfg(unix)]
const ENV_OVERRIDE: &str = "PRUX_TERMINAL_COLORS";

/// 终端上报的当前配色
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalColors {
    /// 默认前景（OSC 10）
    pub foreground: Option<Rgb>,
    /// 默认背景（OSC 11）
    pub background: Option<Rgb>,
    /// ANSI 0-15（OSC 4）；只有 16 个都收到时才置位
    pub palette: Option<Vec<Rgb>>,
}

impl TerminalColors {
    /// 是否三项配色（前景 / 背景 / 调色板）都未取到。
    pub fn is_empty(&self) -> bool {
        self.foreground.is_none() && self.background.is_none() && self.palette.is_none()
    }
}

/// 一次 OSC 配色回复的目标
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OscColorTarget {
    /// OSC 10：终端默认前景色。
    Foreground,
    /// OSC 11：终端默认背景色。
    Background,
    /// ANSI 调色板索引
    Palette(u8),
}

/// 解析一条 OSC 10 / 11 / 4 配色回复（不含前导 `ESC ]`，`data` 形如 `10;#rrggbb`）。
/// 不是配色回复返回 `None`；是回复但颜色无法解析时 `rgb` 为 `None`。
pub fn parse_osc_color_response(data: &str) -> Option<(OscColorTarget, Option<Rgb>)> {
    let (target, value) = split_osc_response(data)?;
    Some((target, parse_osc_color_value(value)))
}

/// 拆分 `"10;..."` / `"11;..."` / `"4;<index>;..."`
fn split_osc_response(data: &str) -> Option<(OscColorTarget, &str)> {
    let mut parts = data.splitn(3, ';');
    let first = parts.next()?;
    match first {
        "10" => Some((OscColorTarget::Foreground, parts.next()?)),
        "11" => Some((OscColorTarget::Background, parts.next()?)),
        "4" => {
            let index: u16 = parts.next()?.trim().parse().ok()?;
            if index >= PALETTE_SIZE as u16 {
                return None;
            }
            Some((OscColorTarget::Palette(index as u8), parts.next()?))
        }
        _ => None,
    }
}

/// 解析颜色值：`#rrggbb` / `#rrrrggggbbbb` / `rgb:rr/gg/bb` / `rgba:...`
fn parse_osc_color_value(raw: &str) -> Option<Rgb> {
    let value = raw.trim();
    if let Some(hex) = value.strip_prefix('#') {
        if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&hex[range], 16).ok();
            return Some(Rgb::new(channel(0..2)?, channel(2..4)?, channel(4..6)?));
        }

        if hex.len() == 12 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
            let channel = |range: std::ops::Range<usize>| hex_channel_any(&hex[range]);
            return Some(Rgb::new(channel(0..4)?, channel(4..8)?, channel(8..12)?));
        }
        return None;
    }

    let rgb = value
        .strip_prefix("rgb:")
        .or_else(|| value.strip_prefix("rgba:"))
        .unwrap_or(value);
    let mut channels = rgb.split('/');
    let r = hex_channel_any(channels.next()?)?;
    let g = hex_channel_any(channels.next()?)?;
    let b = hex_channel_any(channels.next()?)?;
    Some(Rgb::new(r, g, b))
}

/// 任意位宽的十六进制通道 → 0-255（按该位宽的最大值归一），对齐 pi `parseOscHexChannel`
fn hex_channel_any(channel: &str) -> Option<u8> {
    if channel.is_empty() || channel.len() > 8 {
        return None;
    }
    if !channel.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(channel, 16).ok()?;
    let max = 16u32.pow(channel.len() as u32) - 1;
    if max == 0 {
        return None;
    }
    Some(((value as f64 / max as f64) * 255.0).round() as u8)
}

/// 查询串：默认前景/背景 + 16 个调色板色 + 结尾 DA1
fn query_string() -> String {
    let mut out = String::from("\x1b]10;?\x07\x1b]11;?\x07");
    for index in 0..PALETTE_SIZE {
        out.push_str(&format!("\x1b]4;{index};?\x07"));
    }
    out.push_str("\x1b[c");
    out
}

/// 累积缓冲区里的一条完整回复
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// OSC 序列体（`ESC ]` 与结束符之间）
    Osc(String),
    /// 主设备属性回复（DA1）
    DeviceAttributes,
}

/// 从缓冲区头部取出一条完整回复；不足一条时返回 `None` 且不消费。
/// 前导的杂散字节（终端回显、用户输入等）会被丢弃。
pub fn take_reply(buf: &mut Vec<u8>) -> Option<Reply> {
    while buf.first().is_some_and(|b| *b != 0x1b) {
        buf.remove(0);
    }

    if buf.len() < 2 {
        return None;
    }

    match buf[1] {
        b']' => {
            let mut i = 2;
            while i < buf.len() {
                if buf[i] == 0x07 {
                    let osc = String::from_utf8_lossy(&buf[2..i]).to_string();
                    buf.drain(..=i);
                    return Some(Reply::Osc(osc));
                }
                if buf[i] == 0x1b && buf.get(i + 1) == Some(&b'\\') {
                    let osc = String::from_utf8_lossy(&buf[2..i]).to_string();
                    buf.drain(..i + 2);
                    return Some(Reply::Osc(osc));
                }
                i += 1;
            }
            None
        }
        b'[' => {
            let mut i = 2;
            while i < buf.len() {
                let byte = buf[i];
                if (0x40..=0x7e).contains(&byte) {
                    let body = String::from_utf8_lossy(&buf[2..=i]).to_string();
                    buf.drain(..=i);
                    // DA1：`CSI ? <params> c`
                    return Some(if byte == b'c' && body.starts_with('?') {
                        Reply::DeviceAttributes
                    } else {
                        Reply::Osc(format!("\x1b[{body}"))
                    });
                }
                if !(0x20..=0x3f).contains(&byte) {
                    // 非法 CSI 字节：丢掉这个 ESC 继续
                    buf.remove(0);
                    return None;
                }
                i += 1;
            }
            None
        }
        _ => {
            buf.remove(0);
            None
        }
    }
}

/// 把原始字节流解析成终端配色（纯函数，便于单测）
pub fn parse_terminal_colors(bytes: &[u8]) -> TerminalColors {
    let mut buf = bytes.to_vec();
    let mut colors = TerminalColors::default();
    let mut palette: Vec<Option<Rgb>> = vec![None; PALETTE_SIZE];

    while let Some(reply) = take_reply(&mut buf) {
        let Reply::Osc(osc) = reply else {
            continue;
        };
        let Some((target, rgb)) = parse_osc_color_response(&osc) else {
            continue;
        };
        match target {
            OscColorTarget::Foreground => colors.foreground = rgb,
            OscColorTarget::Background => colors.background = rgb,
            OscColorTarget::Palette(index) => palette[index as usize] = rgb,
        }
    }

    if palette.iter().all(|c| c.is_some()) {
        colors.palette = Some(palette.into_iter().flatten().collect());
    }
    colors
}

/// 本进程是否允许查询终端配色（环境变量覆盖 + TTY 判定，不含 raw 模式）
fn query_allowed() -> bool {
    #[cfg(unix)]
    {
        match std::env::var(ENV_OVERRIDE).ok().as_deref() {
            Some("0") | Some("false") | Some("no") => false,
            _ => terminal_is_tty(),
        }
    }

    // 非 unix 无带超时的 fd 轮询实现：宁可不查（不向终端写无意义序列）
    #[cfg(not(unix))]
    {
        return false;
    }
}

/// stdin 与 stdout 都是 TTY（查询终端能力的前提）。
#[cfg(unix)]
fn terminal_is_tty() -> bool {
    use std::io::IsTerminal;
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

/// 进程内缓存的终端配色。
///
/// 首次访问时真正查询一次（非 TTY / 覆盖关闭时直接返回空，不会阻塞）；
/// TUI 启动前调用 [`prime_terminal_colors`] 可把这一次查询提前到锁定输入之前。
pub fn terminal_colors() -> &'static TerminalColors {
    /// 终端配色的进程内单次缓存，首次查询后复用，避免重复阻塞读取终端。
    static CACHE: OnceLock<TerminalColors> = OnceLock::new();
    CACHE.get_or_init(query_terminal_colors)
}

/// 提前查询并缓存终端配色（在 TUI 接管输入前调用，保证 raw 模式开关不与事件循环打架）
pub fn prime_terminal_colors() {
    _ = terminal_colors();
}

/// 查询终端配色（默认 100ms 超时）。非 TTY / 覆盖关闭 / 查询失败一律返回空结果。
pub fn query_terminal_colors() -> TerminalColors {
    query_terminal_colors_with_timeout(Duration::from_millis(QUERY_TIMEOUT_MS))
}

/// 在 `timeout` 内查询终端配色。
///
/// 非 TTY / 被覆盖关闭 / 开启 raw 模式失败 / 写入失败时返回空结果；
/// 收到 DA1 回复或超时即结束读取。
pub fn query_terminal_colors_with_timeout(timeout: Duration) -> TerminalColors {
    if !query_allowed() {
        return TerminalColors::default();
    }

    parse_terminal_colors(&query_terminal_replies(&query_string(), timeout))
}

/// 向终端写入 `query` 并在 `timeout` 内读取回复，直到收到 DA1 哨兵或超时。
///
/// 查询串必须以 DA1（`CSI c`）结尾：所有终端都会回 DA1 且按顺序回复，因此它标志回复结束。
/// 返回累积的原始字节（未解析）；stdin/stdout 不是 TTY、非 Unix 平台、开启 raw 模式失败或写入
/// 失败时返回空。会短暂开关 raw 模式（否则行缓冲会把回复攒到换行、echo 会把回复打到屏幕上），
/// 因此调用方必须保证在 TUI 接管输入**之前**执行。
pub(crate) fn query_terminal_replies(query: &str, timeout: Duration) -> Vec<u8> {
    #[cfg(not(unix))]
    {
        let _ = (query, timeout);
        Vec::new()
    }

    #[cfg(unix)]
    {
        if !terminal_is_tty() {
            return Vec::new();
        }

        let Some(_raw) = RawModeGuard::acquire() else {
            return Vec::new();
        };

        {
            let mut stdout = std::io::stdout();
            if stdout.write_all(query.as_bytes()).is_err() || stdout.flush().is_err() {
                return Vec::new();
            }
        }

        let deadline = Instant::now() + timeout;
        let mut buf: Vec<u8> = Vec::new();
        let mut chunk = [0u8; 1024];

        loop {
            match read_available(deadline, &mut chunk) {
                Some(0) => break,
                Some(n) => buf.extend_from_slice(&chunk[..n]),
                None => break,
            }

            // 收到 DA1（或超时）即认为回复结束
            if saw_device_attributes(buf.clone()) {
                break;
            }
        }

        buf
    }
}

/// 缓冲区里是否已经出现 DA1 回复
fn saw_device_attributes(mut buf: Vec<u8>) -> bool {
    while let Some(reply) = take_reply(&mut buf) {
        if reply == Reply::DeviceAttributes {
            return true;
        }
    }
    false
}

/// raw 模式守卫：进入时开启，析构时关闭
struct RawModeGuard;

impl RawModeGuard {
    /// 开启终端 raw 模式并返回守卫；开启失败返回 `None`。
    fn acquire() -> Option<Self> {
        crossterm::terminal::enable_raw_mode().ok()?;
        Some(RawModeGuard)
    }
}

impl Drop for RawModeGuard {
    /// 析构时关闭 raw 模式（恢复之前的终端状态）。
    fn drop(&mut self) {
        _ = crossterm::terminal::disable_raw_mode();
    }
}

/// 在 deadline 前等到可读数据；超时返回 `None`
#[cfg(unix)]
fn read_available(deadline: Instant, buf: &mut [u8]) -> Option<usize> {
    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    use std::os::fd::BorrowedFd;

    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        return None;
    }

    let fd = unsafe { BorrowedFd::borrow_raw(0) };
    let mut fds = [PollFd::new(fd, PollFlags::POLLIN)];
    let timeout = PollTimeout::try_from(remaining).unwrap_or(PollTimeout::MAX);
    match poll(&mut fds, timeout) {
        Ok(0) | Err(_) => None,
        Ok(_) => nix::unistd::read(fd, buf).ok(),
    }
}

/// 非 Unix 平台没有 poll 可等，恒返回 `None`（不读取任何输入）。
#[cfg(not(unix))]
fn read_available(_deadline: Instant, _buf: &mut [u8]) -> Option<usize> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_and_rgb_replies() {
        assert_eq!(
            parse_osc_color_response("10;#ff8800"),
            Some((OscColorTarget::Foreground, Some(Rgb::new(255, 136, 0))))
        );
        assert_eq!(
            parse_osc_color_response("11;rgb:00/00/00"),
            Some((OscColorTarget::Background, Some(Rgb::new(0, 0, 0))))
        );
        // 12 位十六进制（kitty 风格）
        assert_eq!(
            parse_osc_color_response("11;#ffffffffffff"),
            Some((OscColorTarget::Background, Some(Rgb::new(255, 255, 255))))
        );
        assert_eq!(
            parse_osc_color_response("4;1;rgb:80/00/00"),
            Some((OscColorTarget::Palette(1), Some(Rgb::new(128, 0, 0))))
        );
        // 无法解析的颜色 / 非配色回复 / 越界索引
        assert_eq!(
            parse_osc_color_response("10;nope"),
            Some((OscColorTarget::Foreground, None))
        );
        assert_eq!(parse_osc_color_response("8;;"), None);
        assert_eq!(parse_osc_color_response("4;99;#ffffff"), None);
    }

    #[test]
    fn parses_full_reply_stream() {
        // 前景 + 背景 + 16 色 + DA1（BEL 与 ST 两种结束符都要认）
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b]10;#e5e5e7\x07");
        bytes.extend_from_slice(b"\x1b]11;rgb:1e/1e/1e\x1b\\");
        for index in 0..16u8 {
            bytes.extend_from_slice(
                format!("\x1b]4;{index};#{index:02x}{index:02x}{index:02x}\x07").as_bytes(),
            );
        }
        bytes.extend_from_slice(b"\x1b[?62;c");

        let colors = parse_terminal_colors(&bytes);
        assert_eq!(colors.foreground, Some(Rgb::new(0xe5, 0xe5, 0xe7)));
        assert_eq!(colors.background, Some(Rgb::new(0x1e, 0x1e, 0x1e)));
        let palette = colors.palette.expect("16 色齐全才置位");
        assert_eq!(palette.len(), 16);
        assert_eq!(palette[3], Rgb::new(3, 3, 3));
    }

    #[test]
    fn partial_palette_is_not_reported() {
        // 只有 2 个调色板色回复：palette 保持 None（不足 16 个不可用）
        let bytes = b"\x1b]4;0;#000000\x07\x1b]4;1;#800000\x07\x1b[?c";
        let colors = parse_terminal_colors(bytes);
        assert_eq!(colors.palette, None);
        assert!(colors.foreground.is_none());
    }

    #[test]
    fn incomplete_reply_waits_for_more_bytes() {
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = b"\x1b]10;#abcdef".to_vec();
        while take_reply(&mut buf).is_some() {}
        assert_eq!(buf, b"\x1b]10;#abcdef", "未结束的 OSC 不消费");
        // 补上结束符后可解析
        buf.extend_from_slice(b"\x07");
        let reply = take_reply(&mut buf).expect("补齐后应能取出");
        assert_eq!(reply, Reply::Osc("10;#abcdef".to_string()));
        assert!(buf.is_empty());

        // 前导杂散字节被丢弃
        tail.extend_from_slice(b"x\x1b]11;#000000\x07");
        let reply = take_reply(&mut tail).expect("应跳过杂散字节");
        assert_eq!(reply, Reply::Osc("11;#000000".to_string()));
    }

    #[test]
    fn detects_da1_terminator() {
        let mut buf = b"\x1b[?62;4;6;22c".to_vec();
        assert_eq!(take_reply(&mut buf), Some(Reply::DeviceAttributes));
        assert!(buf.is_empty());
        // 其他 CSI 不误判
        let mut buf = b"\x1b[0m".to_vec();
        assert_eq!(
            take_reply(&mut buf),
            Some(Reply::Osc("\x1b[0m".to_string()))
        );
    }

    #[test]
    fn query_string_shape() {
        let query = query_string();
        assert!(query.starts_with("\x1b]10;?\x07\x1b]11;?\x07"));
        assert!(query.contains("\x1b]4;15;?\x07"));
        assert!(query.ends_with("\x1b[c"));
    }
}

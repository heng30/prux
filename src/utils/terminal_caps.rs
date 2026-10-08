//! 终端能力检测：
//! - OSC8 超链接可用性（tmux 需转发 client_termfeatures 探测；screen 强制禁 OSC8）
//! - 环境变量覆盖 PRUX_HYPERLINKS / PRUX_TRUE_COLOR（1/0/auto）

use std::{
    io::Read,
    process::Command,
    sync::OnceLock,
    thread,
    time::{Duration, Instant},
};

/// 终端能力探测结果：是否支持 OSC8 超链接与 24bit 真彩色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalCapabilities {
    /// 是否可用 OSC8 超链接
    pub hyperlinks: bool,
    /// 是否可用 24bit 真彩色
    pub true_color: bool,
}

/// 缓存用的内部副本，与对外结果字段一致但避免额外派生。
#[derive(Clone, Copy)]
struct CapsInner {
    /// 进程级缓存的 OSC8 超链接可用性
    hyperlinks: bool,
    /// 进程级缓存的 24bit 真彩色可用性
    true_color: bool,
}

/// 读取环境变量并按 `1/true/yes`、`0/false/no` 解析为 bool；
/// 未设置或值无法识别（如 `auto`）返回 None，表示交由探测决定。
fn env_override(name: &str) -> Option<bool> {
    std::env::var(name).ok().and_then(|v| match v.as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    })
}

/// 是否运行在 tmux 内（tmux 默认不转发 OSC8，除非 client_termfeatures 声明）
/// TMUX 环境变量或 TERM=tmux* 都算（TMUX 变量在部分嵌套/派生环境下会丢失）
pub(crate) fn in_tmux() -> bool {
    std::env::var_os("TMUX").is_some()
        || std::env::var("TERM")
            .map(|t| t.starts_with("tmux"))
            .unwrap_or(false)
}

/// 限时运行 `tmux <args...>` 并捕获 stdout；命令缺失/超时/非零退出均返回 None（静默降级）。
/// 只读探测，绝不修改 tmux 配置。
fn tmux_output(args: &[&str], timeout_ms: u64) -> Option<String> {
    let mut child = Command::new("tmux")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                break;
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                _ = child.kill();
                _ = child.wait();
                return None;
            }
            Err(_) => {
                _ = child.kill();
                return None;
            }
        }
    }
    let mut out = String::new();
    child.stdout.as_mut()?.read_to_string(&mut out).ok()?;
    Some(out.trim().to_string())
}

/// tmux 下检查客户端是否声明了 hyperlinks 转发能力：
/// `tmux display-message -p '#{client_termfeatures}'` 声明了 hyperlinks 才发 OSC8；
/// 查询失败/未声明一律 false（保守关闭）。
fn tmux_client_supports_hyperlinks() -> bool {
    tmux_output(&["display-message", "-p", "#{client_termfeatures}"], 250)
        .map(|feats| feats.split(',').any(|f| f.trim() == "hyperlinks"))
        .unwrap_or(false)
}

/// tmux 客户端终端是否声明了 extkeys（扩展键）转发能力：
/// tmux 只有看到 terminal-features 里的 extkeys 特性才会主动向外部终端请求修饰键，
/// 缺失时 Shift+Enter / Ctrl+Shift+C 等修饰键在外部终端（foot/kitty 等）源头就丢失，
/// 表现为 Shift 系快捷键全部失效。查询失败/未声明返回 false（需要提示）。
#[allow(unused)]
pub(crate) fn tmux_client_supports_extkeys() -> bool {
    tmux_output(&["display-message", "-p", "#{client_termfeatures}"], 250)
        .map(|feats| feats.split(',').any(|f| f.trim() == "extkeys"))
        .unwrap_or(false)
}

/// 读取 tmux 全局选项值（`tmux show -gv <option>`）；非 tmux/查询失败返回 None。
/// 用于启动时检测 extended-keys / extended-keys-format。
pub(crate) fn tmux_show_global_value(option: &str) -> Option<String> {
    tmux_output(&["show", "-gv", option], 300)
}

/// 执行 tmux 的 `display-message -p <format>` 并返回结果；非 tmux/失败返回 None。
/// 只读探测，绝不修改 tmux 配置。
pub(crate) fn tmux_display(format: &str) -> Option<String> {
    tmux_output(&["display-message", "-p", format], 300)
}

/// 读取 tmux 当前 pane 的选项值（`tmux show -pv <option>`）；非 tmux/查询失败返回 None。
/// 只读探测，绝不修改 tmux 配置。
pub(crate) fn tmux_show_pane_value(option: &str) -> Option<String> {
    tmux_output(&["show", "-pv", option], 300)
}

/// 写 tmux 当前 pane 的选项（`tmux set -p <option> <value>`）。
///
/// 目前只用于内联图片：tmux 转发 sixel 受 `allow-passthrough` 管辖，
/// 没开时会显式打开当前 pane 的这个选项（只影响本 pane，不动全局配置）。
pub(crate) fn tmux_set_pane_value(option: &str, value: &str) {
    _ = Command::new("tmux")
        .args(["set", "-p", option, value])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

/// 探测终端是否支持 OSC8 超链接与 24bit 真彩色（结果进程级缓存，重复调用不重跑探测）。
pub fn detect_terminal_capabilities() -> TerminalCapabilities {
    let inner = detect_inner();
    TerminalCapabilities {
        hyperlinks: inner.hyperlinks,
        true_color: inner.true_color,
    }
}

/// 实际探测逻辑：先看 PRUX_HYPERLINKS / PRUX_TRUE_COLOR 硬覆盖，未覆盖时按
/// TERM / COLORTERM / tmux client_termfeatures 推断；结果写入进程级 OnceLock 缓存。
fn detect_inner() -> CapsInner {
    /// 探测结果的进程级缓存，避免每次渲染都重跑环境变量与 tmux 查询。
    static CACHE: OnceLock<CapsInner> = OnceLock::new();
    *CACHE.get_or_init(|| {
        // PRUX_HYPERLINKS / PRUX_TRUE_COLOR：1/0 硬覆盖；auto 或缺失走探测
        let hyperlinks = match env_override("PRUX_HYPERLINKS") {
            Some(v) => v,
            None => {
                let screen = std::env::var("TERM")
                    .map(|t| t.starts_with("screen"))
                    .unwrap_or(false);

                if screen {
                    false
                } else if in_tmux() {
                    tmux_client_supports_hyperlinks()
                } else {
                    true // 常规终端（kitty/ghostty/wezterm/iterm2/vscode 等）均支持 OSC8
                }
            }
        };

        let true_color = match env_override("PRUX_TRUE_COLOR") {
            Some(v) => v,
            None => {
                // COLORTERM=truecolor/24bit 或常见现代终端
                let term = std::env::var("TERM").unwrap_or_default();
                std::env::var("COLORTERM")
                    .map(|c| c.contains("truecolor") || c.contains("24bit"))
                    .unwrap_or(false)
                    || term.ends_with("-direct")
                    || term.contains("kitty")
                    || term.contains("ghostty")
                    || term.contains("wezterm")
                    || term.contains("alacritty")
            }
        };

        CapsInner {
            hyperlinks,
            true_color,
        }
    })
}

/// 是否可用 OSC8 超链接。
///
/// 供 `render::hyperlink` 在 buffer 定型时决定是否把哨兵换成超链接序列：
/// ratatui 的 text 渲染路径（`Buffer::set_stringn`）会丢弃含控制字符的 grapheme，
/// 但 `Cell::set_symbol` + `CellDiffOption::ForcedWidth` 能承载转义序列，见该模块说明。
pub fn hyperlinks_enabled() -> bool {
    detect_terminal_capabilities().hyperlinks
}

/// 是否是 Apple Terminal（macOS 自带终端）：它给块字符逐行留缝，
/// 启动横幅的 `████` art 会出现横向断层，需改用纯文本词标。
pub fn is_apple_terminal() -> bool {
    apple_terminal_from(
        cfg!(target_os = "macos"),
        std::env::var("TERM_PROGRAM").ok().as_deref(),
    )
}

/// [`is_apple_terminal`] 的纯函数内核：平台与 `TERM_PROGRAM` 由调用方传入，便于测试两种取值。
fn apple_terminal_from(is_macos: bool, term_program: Option<&str>) -> bool {
    is_macos && term_program == Some("Apple_Terminal")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apple_terminal_needs_macos_and_term_program() {
        assert!(apple_terminal_from(true, Some("Apple_Terminal")));
        assert!(!apple_terminal_from(false, Some("Apple_Terminal")));
        assert!(!apple_terminal_from(true, Some("iTerm.app")));
        assert!(!apple_terminal_from(true, None));
    }
}

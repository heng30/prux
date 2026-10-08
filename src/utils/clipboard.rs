//! 系统剪贴板：Wayland 命令优先，fallback arboard

use crate::utils::time::now_ms;
use std::{io::Write, path::Path};

/// 剪贴板内部操作的返回值别名，错误统一收敛为 [`ClipboardError`]。
type Result<T> = std::result::Result<T, ClipboardError>;

/// OSC52 base64 编码长度上限（超过则不发送，避免终端截断/拒绝）。
const MAX_OSC52_ENCODED_LENGTH: usize = 100_000;

/// OSC52 转义序列的写入结果：已发出、超出长度上限或写入失败。
enum Osc52Result {
    /// 转义序列已成功写入 stdout。
    Emitted,
    /// base64 编码超过 MAX_OSC52_ENCODED_LENGTH，未发送。
    TooLarge,
    /// 写入或 flush stdout 失败。
    Failed,
}

/// 剪贴板操作错误
#[derive(Debug, thiserror::Error)]
enum ClipboardError {
    /// 平台工具（wl-paste）失败的自由文本说明（仅 Linux）。
    #[cfg(target_os = "linux")]
    #[error("{0}")]
    Message(String),

    /// arboard 后端报错。
    #[error(transparent)]
    Clipboard(#[from] arboard::Error),

    /// 子进程或文件 IO 失败。
    #[error(transparent)]
    Io(#[from] std::io::Error),

    /// 剪贴板字节不是合法 UTF-8。
    #[error(transparent)]
    Utf8(#[from] std::string::FromUtf8Error),
}

/// 读剪贴板（Ctrl+Shift+V）：Wayland 下 wl-paste，否则 arboard
pub fn read_clipboard() -> Option<String> {
    paste_from_clipboard().ok().filter(|t| !t.is_empty())
}

/// 写剪贴板：本地系统剪贴板优先（可真实验证）。
/// OSC52 仅在「远程会话 / 无显示的 Linux」作为兜底——桌面终端忽略 OSC52 时不得虚报
/// 成功（pi #9618/#9688）。失败返回带平台指引的说明。
pub fn write_clipboard(text: &str) -> std::result::Result<(), String> {
    if copy_to_clipboard(text).is_ok() {
        return Ok(());
    }

    if is_remote_session() || is_headless_linux() {
        match write_osc52(text) {
            Osc52Result::Emitted => return Ok(()),
            Osc52Result::TooLarge => {
                return Err("Clipboard unavailable: text exceeds the OSC 52 size limit".to_string());
            }
            Osc52Result::Failed => {}
        }
    }
    Err(clipboard_failure_hint())
}

/// 是否处于远程会话（SSH / mosh）：`SSH_CONNECTION`、`SSH_CLIENT`、`MOSH_CONNECTION`
/// 任一环境变量存在即为真。
fn is_remote_session() -> bool {
    ["SSH_CONNECTION", "SSH_CLIENT", "MOSH_CONNECTION"]
        .iter()
        .any(|k| std::env::var_os(k).is_some())
}

/// 无显示环境的 Linux（容器 / WSL 无 WSLg）：终端是唯一剪贴板通道。
fn is_headless_linux() -> bool {
    cfg!(target_os = "linux")
        && std::env::var_os("DISPLAY").is_none()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
        && std::env::var_os("TERMUX_VERSION").is_none()
}

/// 无法写入时的平台指引。
fn clipboard_failure_hint() -> String {
    if cfg!(target_os = "linux") {
        if std::env::var_os("TERMUX_VERSION").is_some() {
            return "Clipboard unavailable: install the Termux:API app and `termux-api` package"
                .to_string();
        }
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            return "Clipboard unavailable: install `wl-clipboard` (`wl-copy`) or check Wayland access"
                .to_string();
        }
        if std::env::var_os("DISPLAY").is_some() {
            return "Clipboard unavailable: install `xclip` or `xsel`, or check X11 access"
                .to_string();
        }
        return "Clipboard unavailable: no Wayland or X11 display detected".to_string();
    }
    "Clipboard unavailable".to_string()
}

/// OSC52 终端转义写入剪贴板（\x1b]52;c;<base64>\x07）。
/// 无法验证终端是否真的写入了系统剪贴板（tmux 需 set-clipboard 转发、外部终端需支持 OSC52），
/// 因此只作为本地剪贴板不可用时的 best-effort。
fn write_osc52(text: &str) -> Osc52Result {
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, text.as_bytes());
    if b64.len() > MAX_OSC52_ENCODED_LENGTH {
        return Osc52Result::TooLarge;
    }

    let seq = format!("\x1b]52;c;{}\x07", b64);
    let mut out = std::io::stdout().lock();
    if out.write_all(seq.as_bytes()).is_ok() && out.flush().is_ok() {
        Osc52Result::Emitted
    } else {
        Osc52Result::Failed
    }
}

/// 复制到系统剪贴板：Wayland 下 wl-copy 优先（arboard 在纯 Wayland 无可靠后端），
/// 失败或非 Wayland 时 fallback arboard
fn copy_to_clipboard(msg: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        if is_wayland() && copy_to_wayland_clipboard(msg).is_ok() {
            return Ok(());
        }
    }

    let mut ctx = arboard::Clipboard::new()?;
    ctx.set_text(msg)?;
    Ok(())
}

/// 从系统剪贴板读取：Wayland 下 wl-paste 优先，失败或非 Wayland 时 fallback arboard
fn paste_from_clipboard() -> Result<String> {
    #[cfg(target_os = "linux")]
    {
        if is_wayland()
            && let Ok(text) = paste_from_wayland_clipboard()
        {
            return Ok(text);
        }
    }

    let mut ctx = arboard::Clipboard::new()?;
    Ok(ctx.get_text()?)
}

/// 当前会话是否运行在 Wayland 下：`WAYLAND_DISPLAY` 存在，或 `XDG_SESSION_TYPE == "wayland"`。
#[cfg(target_os = "linux")]
fn is_wayland() -> bool {
    std::env::var("WAYLAND_DISPLAY").is_ok()
        || std::env::var("XDG_SESSION_TYPE")
            .map(|t| t == "wayland")
            .unwrap_or(false)
}

/// 用 `wl-copy` 把文本写入 Wayland 剪贴板；只 spawn 不等待（见函数内注释）。
/// wl-copy 不存在或启动失败时返回错误，由调用方 fallback arboard。
#[cfg(target_os = "linux")]
fn copy_to_wayland_clipboard(text: &str) -> Result<()> {
    // wl-copy 必须保持运行以持有剪贴板所有权（Wayland 数据在提供者进程内），
    // .status() 会永久阻塞直到剪贴板被替换；detach 后由下一次 wl-copy 替换时自行退出。
    use std::process::Stdio;
    std::process::Command::new("wl-copy")
        .arg(text)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    Ok(())
}

/// 用 `wl-paste --no-newline` 读取 Wayland 剪贴板文本。
/// wl-paste 启动失败、退出码非 0 或输出非 UTF-8 时返回错误。
#[cfg(target_os = "linux")]
fn paste_from_wayland_clipboard() -> Result<String> {
    use std::process::Stdio;
    let out = std::process::Command::new("wl-paste")
        .arg("--no-newline")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;

    if !out.status.success() {
        return Err(ClipboardError::Message(format!(
            "wl-paste exited with {}",
            out.status
        )));
    }
    Ok(String::from_utf8(out.stdout)?)
}

/// 读剪贴板中的文件路径（macOS 在 Finder 里复制的文件、Linux 的 `text/uri-list`）。
/// 命中返回全部路径（保持剪贴板顺序）；剪贴板不含文件列表、或后端根本不可用时返回空 Vec
///（后者不当作错误：真正拿不到剪贴板时，后续的文本/图片分支会给出提示）。
/// 必须排在读图片之前——否则 Finder 复制的文件会被当成图片、插进图标临时文件
pub fn read_clipboard_file_paths() -> Vec<String> {
    #[cfg(target_os = "linux")]
    {
        if is_wayland()
            && let Some(paths) = wayland_file_paths()
        {
            return paths;
        }
    }

    arboard::Clipboard::new()
        .and_then(|mut ctx| ctx.get().file_list())
        .map(|paths| {
            paths
                .into_iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// 剪贴板是否声明了图片目标：`Some(true)` 声明了、`Some(false)` 明确没有、`None` 探测不可用。
/// 用于在读取图片前先看剪贴板自己声明的目标——剪贴板所有者可能对**未声明**的图片目标也返回数据，
/// 提前探到「没有图片」就能完全跳过这次请求。
pub fn clipboard_advertises_image() -> Option<bool> {
    #[cfg(target_os = "linux")]
    {
        linux_advertises_image()
    }

    // 其余平台不做探测：arboard 的 macOS 后端按类型取数据（NSPasteboard dataForType），
    // 不存在「未声明目标也返回数据」的问题。
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Linux 的图片目标探测：Wayland 用 `wl-paste -l`，X11 用 `xclip` 查 TARGETS。
/// 工具不存在、命令失败时返回 None（调用方退回直接读图）。
#[cfg(target_os = "linux")]
fn linux_advertises_image() -> Option<bool> {
    let mut command = if is_wayland() {
        let mut c = std::process::Command::new("wl-paste");
        c.arg("-l");
        c
    } else {
        // X11：TARGETS 是剪贴板自身的类型清单
        let mut c = std::process::Command::new("xclip");
        c.args(["-selection", "clipboard", "-t", "TARGETS", "-o"]);
        c
    };

    let listed = command.output().ok()?;
    if !listed.status.success() {
        return None;
    }

    Some(
        String::from_utf8_lossy(&listed.stdout)
            .lines()
            .any(|t| t.trim().starts_with("image/")),
    )
}

/// 读剪贴板图片：Wayland 下优先 wl-paste -t image/png，
/// 失败 / 无工具 / 非 Wayland 时 fallback arboard（读取 RGBA 并编码为 PNG）。
/// 命中则返回临时 PNG 文件路径；无图片返回 None。
pub fn read_clipboard_image(tmp_dir: &Path) -> Option<String> {
    // 剪贴板明确声明「没有图片」时不发图片请求。
    // 所有者可能对未声明的图片目标返回数据，把纯文本剪贴板误导成图片
    let advertised = clipboard_advertises_image();
    if advertised == Some(false) {
        return None;
    }

    #[cfg(target_os = "linux")]
    {
        if is_wayland()
            && advertised == Some(true)
            && let Ok(bytes) = std::process::Command::new("wl-paste")
                .arg("-t")
                .arg("image/png")
                .output()
            && bytes.status.success()
            && !bytes.stdout.is_empty()
            && let Some(path) = write_png_file(tmp_dir, &bytes.stdout)
        {
            return Some(path);
        }
    }

    // fallback arboard（X11 / 非 Wayland / wl-paste 不可用或无图）
    paste_image_from_clipboard(tmp_dir)
}

/// Wayland：`wl-paste -l` 列出剪贴板 MIME 类型，含 `text/uri-list` 时读出并解析为路径。
/// 无 wl-paste、无 file 列表、命令失败或全部条目都不是本地文件时返回 None
///（调用方回退 arboard 的 file_list）。
#[cfg(target_os = "linux")]
fn wayland_file_paths() -> Option<Vec<String>> {
    let listed = std::process::Command::new("wl-paste")
        .arg("-l")
        .output()
        .ok()?;
    let has_uri_list = String::from_utf8_lossy(&listed.stdout)
        .lines()
        .any(|t| t.trim() == "text/uri-list");

    if !has_uri_list {
        return None;
    }

    let out = std::process::Command::new("wl-paste")
        .arg("-t")
        .arg("text/uri-list")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }

    let text = String::from_utf8(out.stdout).ok()?;
    let paths = parse_file_uri_list(&text);
    (!paths.is_empty()).then_some(paths)
}

/// 解析 `text/uri-list`（RFC 2483）为本地路径：逐行取 `file://` 条目、
/// 跳过 `#` 注释行与空行、百分号解码；非 `file://` 或远程主机条目丢弃。
#[cfg(target_os = "linux")]
fn parse_file_uri_list(uri_list: &str) -> Vec<String> {
    uri_list
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            url::Url::parse(line)
                .ok()
                .and_then(|url| url.to_file_path().ok())
        })
        .map(|path| path.to_string_lossy().into_owned())
        .collect()
}

/// 将原始字节写入临时 PNG 文件，返回文件路径（仅 Linux wl-paste 图片路径使用）
#[cfg(target_os = "linux")]
fn write_png_file(tmp_dir: &Path, bytes: &[u8]) -> Option<String> {
    let file = tmp_dir.join(format!("clipboard-{}.png", now_ms()));
    std::fs::write(&file, bytes)
        .ok()
        .map(|_| file.to_string_lossy().to_string())
}

/// 通过 arboard 读取剪贴板图片：返回 RGBA 像素，编码为 PNG 写入临时文件
fn paste_image_from_clipboard(tmp_dir: &Path) -> Option<String> {
    let mut ctx = arboard::Clipboard::new().ok()?;
    let img = ctx.get_image().ok()?;
    let file = tmp_dir.join(format!("clipboard-{}.png", now_ms()));

    let rgba =
        image::RgbaImage::from_raw(img.width as u32, img.height as u32, img.bytes.into_owned())?;
    rgba.save(&file).ok()?;

    Some(file.to_string_lossy().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_rejects_oversized_payload() {
        // 超过上限时不发送（避免终端截断/拒绝），返回 TooLarge
        let big = "a".repeat(MAX_OSC52_ENCODED_LENGTH + 10);
        assert!(matches!(write_osc52(&big), Osc52Result::TooLarge));
    }

    #[test]
    fn file_uri_list_keeps_local_paths_in_order() {
        // 取本地 file:// 条目、按顺序保留、百分号解码；注释行与远程主机条目丢弃
        let list = "# comment\nfile:///tmp/My%20Photos/a.png\n\nfile:///tmp/b.png\nhttps://example.com/c.png\nfile://otherhost/tmp/d.png\n";
        assert_eq!(
            parse_file_uri_list(list),
            vec!["/tmp/My Photos/a.png", "/tmp/b.png"]
        );
    }

    #[test]
    fn failure_hint_is_actionable() {
        // 任何平台都必须给出可操作的失败说明，而非静默成功
        assert!(clipboard_failure_hint().starts_with("Clipboard unavailable"));
    }
}

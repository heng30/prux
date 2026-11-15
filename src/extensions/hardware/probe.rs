//! SWD/JTAG 通道：调系统的 `probe-rs` 命令行读取目标板信息与内存。
//!
//! 工具共用这里的实现：
//! - `hardware_board_info`：探针实时读芯片名（失败回落静态表）；没配置板卡时先用
//!   [`detect`] 让 probe-rs 自己认芯片；
//! - `hardware_update_board`：把 [`detect`] 认出的 target 名写进配置；
//! - `hardware_memory_map`：列出目标定义里的 RAM / Flash 区间；
//! - `hardware_memory_read`：按地址读一段内存并渲染成 hex dump。
//!
//! 约束：
//! - 这里**不内嵌** probe-rs 库，全部经外部 `probe-rs` 命令；机器上没装时
//!   [`INSTALL_HINT`] 会原样回到工具结果里，指明怎么装；
//! - 子进程按 AGENTS.md 的口径**不继承终端**：stdin 接 null、stdout/stderr 走管道，
//!   所以必须带 `--non-interactive`，否则 probe-rs 会交互式问选哪支探针、在 TUI 里挂住；
//! - 调用是阻塞的（等子进程 + USB 传输），调用方仍要走 `spawn_blocking`；
//! - 没有探针 / 芯片名不认识时返回**可读的错误串**，由工具层决定是否回落静态资料。

use crate::utils::display::sanitize_output;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
};

/// 外部探针工具名：走 PATH 查找。
const PROBE_BINARY: &str = "probe-rs";

/// hex dump 每行的字节数。
const DUMP_COLS: usize = 16;

/// 一次读内存的字节上限：再长就该直接跑 probe-rs CLI，不该经模型上下文。
pub const MAX_READ_BYTES: usize = 256;

/// 找不到 `probe-rs` 时回给用户的安装指南（原文进工具结果，所以写清命令与文档链接）。
pub const INSTALL_HINT: &str = "\
`probe-rs` was not found on PATH, and the probe tools shell out to it. Install it, then retry:
- cargo (Rust 1.89+): `cargo install probe-rs-tools --locked`
- prebuilt binaries: https://probe.rs/docs/getting-started/installation/
- Linux: also install the udev rules so probes are reachable without root: https://probe.rs/docs/getting-started/probe-setup/
Check the install with `probe-rs list`.";

/// 跑一次 `probe-rs <args>`，成功返回 stdout，失败返回最后一条错误行。
fn run(args: &[String]) -> Result<String, String> {
    run_with(PROBE_BINARY, args)
}

/// 按指定的二进制名跑一次，`binary` 抽成参数是为了能在离线测试里验证「找不到命令」的分支。
fn run_with(binary: &str, args: &[String]) -> Result<String, String> {
    let output = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|err| match err.kind() {
            std::io::ErrorKind::NotFound => INSTALL_HINT.to_string(),
            _ => format!("could not run `{binary}`: {err}"),
        })?;

    let stdout = sanitize_output(&String::from_utf8_lossy(&output.stdout));
    if output.status.success() {
        return Ok(stdout);
    }
    let stderr = sanitize_output(&String::from_utf8_lossy(&output.stderr));
    // probe-rs 先刷一串 WARN 再报真正的 Error，所以取最后一条非空行
    let reason = last_line(&stderr)
        .or_else(|| last_line(&stdout))
        .unwrap_or_else(|| "no output".to_string());
    Err(format!("`{binary} {}` failed: {reason}", args.join(" ")))
}

/// 最后一条非空行（去掉首尾空白）。
fn last_line(text: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(str::to_string)
}

/// 已连接探针的展示行（`[0]: …`，与 `probe-rs list` 同格式）；空 vec = 没插探针。
pub fn list_probes() -> Result<Vec<String>, String> {
    let stdout = run(&["list".to_string()])?;
    Ok(parse_probe_list(&stdout))
}

/// 从 `probe-rs list` 的输出里挑出 `[n]:` 行（无探针时它只打一句 `No debug probes were found.`）。
fn parse_probe_list(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('['))
        .map(str::to_string)
        .collect()
}

/// 让探针自己认芯片，返回 probe-rs 的 target 名（`probe-rs info` 的 `Detected chip:` 行）。
fn detected_chip() -> Result<String, String> {
    let stdout = run(&["info".to_string(), "--non-interactive".to_string()])?;
    parse_detected_chip(&stdout).ok_or_else(|| {
        format!(
            "probe-rs could not identify the attached chip:\n{}",
            stdout.trim()
        )
    })
}

/// 从 `probe-rs info` 的输出里取芯片名；没有这一行时返回 `None`。
fn parse_detected_chip(stdout: &str) -> Option<String> {
    stdout
        .lines()
        .find_map(|line| line.trim().strip_prefix("Detected chip:"))
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// 自动识别目标，返回 probe-rs 的 target 名与芯片信息（内核 + 内存区间）。
pub fn detect() -> Result<(String, String), String> {
    let chip = detected_chip()?;
    Ok((chip.clone(), describe_chip(&chip)?))
}

/// 探针视角下的芯片信息：先确认探针认出的芯片，再列它的内核与内存区间。
///
/// `chip` 是配置里的期望值（probe-rs target 名）；探针认出的名字与它不一致时一并说明，
/// 因为那通常意味着配置写错了芯片。
pub fn describe(chip: &str) -> Result<String, String> {
    let detected = detected_chip()?;
    let mut out = format!("Probe detected chip: {detected}\n");
    if detected != chip {
        out.push_str(&format!(
            "(config.json says `{chip}`; run `hardware_update_board` to save the detected name)\n"
        ));
    }
    out.push_str(&describe_chip(&detected)?);
    Ok(out)
}

/// 列芯片定义里的内核与 RAM / Flash 区间（`probe-rs chip info`，只查表、不需要探针）。
pub fn memory_map(chip: &str) -> Result<String, String> {
    describe_chip(chip)
}

/// `probe-rs chip info <chip>` 的渲染结果（芯片名、内核、内存区间）。
fn describe_chip(chip: &str) -> Result<String, String> {
    run(&["chip".to_string(), "info".to_string(), chip.to_string()])
}

/// 读一段内存并渲染成 hex dump。
pub fn read_memory(chip: &str, address: u64, length: usize) -> Result<String, String> {
    let length = length.clamp(1, MAX_READ_BYTES);
    let path = dump_file_path();
    let args: Vec<String> = [
        "read".to_string(),
        "b8".to_string(),
        format!("0x{address:08X}"),
        length.to_string(),
        "--chip".to_string(),
        chip.to_string(),
        "--output".to_string(),
        path.display().to_string(),
        "--format".to_string(),
        "binary".to_string(),
        "--non-interactive".to_string(),
    ]
    .to_vec();

    let result = run(&args);
    let bytes = std::fs::read(&path);
    let _ = std::fs::remove_file(&path);
    result?;
    let bytes = bytes.map_err(|err| {
        format!("probe-rs read {length} byte(s) at 0x{address:08X} but wrote no data: {err}")
    })?;

    Ok(hex_dump(address, &bytes))
}

/// 读内存用的临时文件名：进程内自增计数避免同进程并发调用互相覆盖。
fn dump_file_path() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("prux-probe-{}-{seq}.bin", std::process::id()))
}

/// 烧写固件：`format` 是 probe-rs 的 `--binary-format`（elf / hex / uf2 / bin）。
///
/// 固定带 `--verify`：写后读回校验，与工具描述里的承诺一致。
pub(super) fn download(
    chip: &str,
    file: &Path,
    format: &str,
    base_address: Option<u64>,
) -> Result<(), String> {
    let mut args: Vec<String> = [
        "download".to_string(),
        file.display().to_string(),
        "--chip".to_string(),
        chip.to_string(),
        "--binary-format".to_string(),
        format.to_string(),
        "--verify".to_string(),
        "--non-interactive".to_string(),
    ]
    .to_vec();
    if let Some(address) = base_address {
        args.push("--base-address".to_string());
        args.push(format!("0x{address:08X}"));
    }
    run(&args).map(|_| ())
}

/// 把字节渲染成 `0xADDR  XX XX …  ascii` 的三段式 hex dump。
fn hex_dump(address: u64, bytes: &[u8]) -> String {
    let mut out = format!(
        "Memory read from 0x{address:08X} ({} bytes):\n\n",
        bytes.len()
    );
    for (index, chunk) in bytes.chunks(DUMP_COLS).enumerate() {
        let line_address = address + (index * DUMP_COLS) as u64;
        let hex = chunk
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        let ascii: String = chunk
            .iter()
            .map(|&b| {
                if b.is_ascii_graphic() || b == b' ' {
                    b as char
                } else {
                    '.'
                }
            })
            .collect();
        out.push_str(&format!("0x{line_address:08X}  {hex:48}  {ascii}\n"));
    }
    out
}

/// 解析 `0x20000000` / `20000000` / `536870912` 三种写法的地址。
///
/// 带 `0x` 前缀按十六进制、纯数字按十六进制（嵌入式语境里裸数字几乎都是地址），
/// 解析失败返回 `None`。
pub fn parse_address(raw: &str) -> Option<u64> {
    let trimmed = raw.trim();
    let digits = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    if digits.is_empty() {
        return None;
    }
    u64::from_str_radix(digits, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_dump_is_three_column_and_address_stamped() {
        let dump = hex_dump(0x2000_0000, b"Hello, probe-rs!");
        assert!(
            dump.starts_with("Memory read from 0x20000000 (16 bytes):"),
            "{dump}"
        );
        assert!(
            dump.contains("0x20000000  48 65 6C 6C 6F 2C 20 70 72 6F 62 65 2D 72 73 21"),
            "{dump}"
        );
        assert!(dump.contains("Hello, probe-rs!"), "{dump}");
    }

    #[test]
    fn hex_dump_wraps_to_the_next_line() {
        let bytes: Vec<u8> = (0..20).collect();
        let dump = hex_dump(0x1000, &bytes);
        assert!(dump.contains("0x00001000  "), "{dump}");
        assert!(dump.contains("0x00001010  "), "{dump}");
    }

    #[test]
    fn non_printable_bytes_become_dots() {
        let dump = hex_dump(0, &[0x00, 0x7F, 0x41]);
        assert!(dump.trim_end().ends_with("..A"), "{dump}");
    }

    #[test]
    fn addresses_parse_with_or_without_a_prefix() {
        assert_eq!(parse_address("0x20000000"), Some(0x2000_0000));
        assert_eq!(parse_address(" 0X08000000 "), Some(0x0800_0000));
        assert_eq!(parse_address("20000000"), Some(0x2000_0000));
        assert_eq!(parse_address("0x"), None);
        assert_eq!(parse_address("not-hex"), None);
    }

    #[test]
    fn probe_list_keeps_only_index_lines() {
        let out = "The following debug probes were found:\n[0]: ST-Link V2 0483:3748:SERIAL\n[1]: ESP JTAG 303a:1001\n";
        assert_eq!(
            parse_probe_list(out),
            vec![
                "[0]: ST-Link V2 0483:3748:SERIAL".to_string(),
                "[1]: ESP JTAG 303a:1001".to_string()
            ]
        );
        assert!(parse_probe_list("No debug probes were found.\n").is_empty());
    }

    #[test]
    fn detected_chip_is_read_from_the_info_output() {
        let out = "Detected chip: esp32c6\nFor more detailed information about the target, run with the --verbose flag.\n";
        assert_eq!(parse_detected_chip(out).as_deref(), Some("esp32c6"));
        assert_eq!(parse_detected_chip("Could not attach to target.\n"), None);
    }

    #[test]
    fn missing_binary_reports_the_install_hint() {
        // 不碰 PATH，只换二进制名：错误串必须是安装指南而不是空 vec
        let err = run_with("prux-no-such-probe-rs", &[]).unwrap_err();
        assert!(err.contains("cargo install probe-rs-tools"), "{err}");
    }
}

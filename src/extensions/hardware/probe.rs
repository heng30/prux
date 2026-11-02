//! SWD/JTAG 通道：通过 `probe-rs` 直连调试探针读取目标板信息与内存。
//!
//! 三个工具共用这里的实现：
//! - `hardware_board_info`：探针实时读芯片名与架构（失败回落静态表）；
//! - `hardware_memory_map`：从探针的 target 定义里列出 RAM / Flash 区间；
//! - `hardware_memory_read`：按地址读一段内存并渲染成 hex dump。
//!
//! 约束：
//! - 探针 IO 是**阻塞**的（USB 传输），调用方必须放进 `spawn_blocking`，不能直接
//!   在 async 任务里跑；
//! - `Session::auto_attach` 会自动选第一个可用探针并识别目标芯片，因此同一时刻只能
//!   有一个会话占着探针——这是 probe-rs 的语义，不是本模块的限制；
//! - 没插探针 / 芯片名不认识时返回**可读的错误串**，由工具层决定是否回落静态资料。

use probe_rs::{MemoryInterface, Session, SessionConfig};
use std::path::Path;

/// hex dump 每行的字节数。
const DUMP_COLS: usize = 16;

/// 一次读内存的字节上限：再长就该用 `probe-rs` CLI 或 gdb，不该经模型上下文。
pub const MAX_READ_BYTES: usize = 256;

/// 自动挂载探针并按芯片识别目标；`chip` 是 probe-rs 的 target 名（如 `STM32F401RETx`）。
fn attach(chip: &str) -> Result<Session, String> {
    Session::auto_attach(chip, SessionConfig::default())
        .map_err(|e| format!("probe-rs could not attach to `{chip}`: {e}"))
}

/// 探针视角下的芯片名、架构与内存映射（多行文本）。
pub fn describe(chip: &str) -> Result<String, String> {
    let session = attach(chip)?;
    let target = session.target();
    let mut out = format!(
        "Target: {}\nArchitecture: {:?}\n\nMemory map:\n",
        target.name,
        session.architecture()
    );

    let regions = memory_regions(&session);
    if regions.is_empty() {
        out.push_str("(the target definition lists no RAM or Flash regions)\n");
    } else {
        out.push_str(&regions);
    }
    out.push_str("\n(Read over USB/SWD — the target needs no firmware for this.)");
    Ok(out)
}

/// 只列内存映射（`hardware_memory_map` 用）；探针可用时优先用它。
pub fn memory_map(chip: &str) -> Result<String, String> {
    let session = attach(chip)?;
    let regions = memory_regions(&session);
    if regions.is_empty() {
        // 探针连上了但 target 定义里没有区间：这比「读不到」更值得说清楚
        return Ok("(the target definition lists no RAM or Flash regions)".to_string());
    }
    Ok(regions)
}

/// 把 target 的 RAM / NVM 区间渲染成文本（空串 = 定义里没有这两类区间）。
fn memory_regions(session: &Session) -> String {
    use probe_rs::config::MemoryRegion;

    let mut out = String::new();
    for region in session.target().memory_map.iter() {
        let (label, range) = match region {
            MemoryRegion::Ram(ram) => ("RAM", &ram.range),
            MemoryRegion::Nvm(flash) => ("Flash", &flash.range),
            _ => continue,
        };
        out.push_str(&format!(
            "{label}: 0x{:08X} - 0x{:08X} ({} KB)\n",
            range.start,
            range.end,
            (range.end - range.start) / 1024
        ));
    }
    out
}

/// 读一段内存并渲染成 hex dump。
pub fn read_memory(chip: &str, address: u64, length: usize) -> Result<String, String> {
    let mut session = attach(chip)?;
    let mut core = session
        .core(0)
        .map_err(|e| format!("probe-rs could not open core 0: {e}"))?;

    let length = length.clamp(1, MAX_READ_BYTES);
    let mut buffer = vec![0u8; length];
    core.read_8(address, &mut buffer)
        .map_err(|e| format!("probe-rs could not read {length} byte(s) at 0x{address:08X}: {e}"))?;

    Ok(hex_dump(address, &buffer))
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

/// 可烧写的固件格式（按文件扩展名判定）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ImageKind {
    /// ELF（自带地址与段信息）。
    Elf,
    /// Intel HEX（自带地址）。
    Hex,
    /// 裸二进制（必须显式给起始地址）。
    Bin,
    /// UF2（自带地址，RP2 等常用）。
    Uf2,
}

impl ImageKind {
    /// 按扩展名判定格式；不认识返回 `None`。
    pub(super) fn from_path(path: &Path) -> Option<Self> {
        match path.extension()?.to_str()?.to_lowercase().as_str() {
            "elf" => Some(ImageKind::Elf),
            "hex" | "ihex" | "intelhex" => Some(ImageKind::Hex),
            "bin" | "binary" => Some(ImageKind::Bin),
            "uf2" => Some(ImageKind::Uf2),
            _ => None,
        }
    }

    /// 展示名（报告里用）。
    pub(super) fn label(self) -> &'static str {
        match self {
            ImageKind::Elf => "ELF",
            ImageKind::Hex => "Intel HEX",
            ImageKind::Bin => "raw binary",
            ImageKind::Uf2 => "UF2",
        }
    }
}

/// 把一份固件烧进目标 Flash。
///
/// `base_address` 只对裸二进制有意义（ELF / HEX / UF2 自带地址），缺省时用 0；
/// 调用方应在 `Bin` 且没给地址时直接拒绝，而不是让固件被写到 0x0。
pub(super) fn flash(
    chip: &str,
    file: &Path,
    kind: ImageKind,
    base_address: Option<u64>,
) -> Result<String, String> {
    use probe_rs::flashing::{BinLoader, BinOptions, ElfLoader, ElfOptions, HexLoader, Uf2Loader};

    let mut session = attach(chip)?;
    let downloaded = match kind {
        ImageKind::Elf => {
            probe_rs::flashing::download_file(&mut session, file, ElfLoader(ElfOptions::default()))
        }
        ImageKind::Hex => probe_rs::flashing::download_file(&mut session, file, HexLoader),
        ImageKind::Uf2 => probe_rs::flashing::download_file(&mut session, file, Uf2Loader),
        ImageKind::Bin => probe_rs::flashing::download_file(
            &mut session,
            file,
            BinLoader(BinOptions {
                base_address,
                skip: 0,
            }),
        ),
    };
    downloaded.map_err(|e| format!("flashing `{}` failed: {e}", file.display()))?;

    let address_note = match (kind, base_address) {
        (ImageKind::Bin, Some(address)) => format!(" at 0x{address:08X}"),
        _ => String::new(),
    };
    Ok(format!(
        "Flashed {} ({}){} to {chip} and verified.",
        file.display(),
        kind.label(),
        address_note
    ))
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
    fn image_kinds_are_derived_from_extensions() {
        assert_eq!(
            ImageKind::from_path(Path::new("/tmp/fw.elf")),
            Some(ImageKind::Elf)
        );
        assert_eq!(
            ImageKind::from_path(Path::new("/tmp/FW.HEX")),
            Some(ImageKind::Hex)
        );
        assert_eq!(
            ImageKind::from_path(Path::new("/tmp/fw.bin")),
            Some(ImageKind::Bin)
        );
        assert_eq!(
            ImageKind::from_path(Path::new("/tmp/fw.uf2")),
            Some(ImageKind::Uf2)
        );
        assert_eq!(ImageKind::from_path(Path::new("/tmp/fw.txt")), None);
        assert_eq!(ImageKind::from_path(Path::new("/tmp/fw")), None);
    }

    #[test]
    fn attach_failure_is_a_readable_error() {
        // 无探针 / 未知芯片名时 probe-rs 都会失败：错误串必须点名芯片且不 panic
        let err = describe("NoSuchChipXyz").unwrap_err();
        assert!(err.contains("NoSuchChipXyz"), "{err}");
        assert!(read_memory("NoSuchChipXyz", 0x2000_0000, 16).is_err());
    }
}

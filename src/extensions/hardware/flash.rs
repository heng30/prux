//! 烧写固件：探针烧录（`hardware_flash`）与 RP2040 的 UF2 拖放（`pico_flash`）。
//!
//! 两条完全不同的路径，对应两种板子：
//!
//! - **探针烧录**：通过 SWD/JTAG 直写目标 Flash（ST-Link / CMSIS-DAP），支持
//!   `.elf` / `.hex` / `.uf2`（自带地址）与 `.bin`（必须给 `address`，否则会写到 0x0）。
//! - **UF2 拖放**：树莓派 Pico 这类板子进 bootloader 后挂成 U 盘，把 `.uf2` 拷进去，
//!   板子自己校验并重启。这条路径不需要探针，只需要挂载点。
//!
//! 两者都是**破坏性**操作：写错固件或写错地址会把板子刷成砖，因此工具标注
//! `destructive`，并受 `permissions` 扩展的审批约束（启用它时）。

use super::{
    config::{config_path, load_config},
    probe, serial, tool_labels,
};
use crate::{
    core::{
        extensions::{ExtensionTool, ToolAnnotations, ToolExposure},
        tools::{ToolError, ToolResult},
    },
    utils::paths::resolve_path,
};
use probe_rs::flashing::{
    BinLoader, BinOptions, ElfLoader, ElfOptions, HexLoader, Uf2Loader, download_file,
};
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// 工具名：探针烧录。
pub(super) const TOOL_FLASH: &str = "hardware_flash";

/// 工具名：UF2 拖放。
pub(super) const TOOL_PICO_FLASH: &str = "pico_flash";

/// UF2 板子 bootloader 挂载点的候选目录名。
const BOOTLOADER_VOLUMES: &[&str] = &["RPI-RP2", "RP2350", "RPI-RP2 (U2F)"];

/// 等 bootloader 挂载点的最长时间。
const MOUNT_WAIT: Duration = Duration::from_secs(10);

/// 轮询挂载点的间隔。
const MOUNT_POLL: Duration = Duration::from_millis(250);

/// 构造一个烧写工具（延迟 + 破坏性）。
fn flash_tool(name: &str, description: &str, parameters: Value, snippet: &str) -> ExtensionTool {
    let mut tool = ExtensionTool::simple(name, description, parameters, snippet)
        .with_exposure(ToolExposure::Deferred)
        .with_annotations(ToolAnnotations {
            read_only: false,
            destructive: true,
            idempotent: false,
        });
    tool.label = Some(tool_labels());
    tool
}

/// `hardware_flash` 的工具定义。
pub(super) fn flash_tool_def() -> ExtensionTool {
    flash_tool(
        TOOL_FLASH,
        "Flash a firmware image into a board's flash over SWD/JTAG. Accepts .elf / .hex / .uf2 \
         (addresses come from the file) and .bin (an `address` is required). Erases and verifies \
         the written regions.",
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Firmware file path (absolute, or relative to the working directory)." },
                "chip": { "type": "string", "description": "probe-rs target name (e.g. `STM32F401RETx`). Omit to use the configured chip / board." },
                "board": { "type": "string", "description": "Board from config.json used to pick the target when `chip` is omitted (defaults to the first configured board)." },
                "address": { "type": "string", "description": "Base address for a raw `.bin` (e.g. `0x08000000`). Required for `.bin`, ignored otherwise." }
            },
            "required": ["file"]
        }),
        "Flash firmware over SWD/JTAG (elf/hex/bin/uf2)",
    )
}

/// `pico_flash` 的工具定义。
pub(super) fn pico_flash_tool_def() -> ExtensionTool {
    flash_tool(
        TOOL_PICO_FLASH,
        "Copy a `.uf2` firmware onto a Pico-style bootloader volume (RPI-RP2). Set `reboot` to ask \
         the configured serial device to reboot into the bootloader first, then wait for the volume.",
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string", "description": "Path to the `.uf2` firmware." },
                "mount": { "type": "string", "description": "Bootloader volume path (e.g. `/Volumes/RPI-RP2`). Omit to auto-detect." },
                "reboot": { "type": "boolean", "description": "Send `reboot_bootsel` to the configured serial device first, then wait up to 10s for the volume." },
                "device": { "type": "string", "description": "Device alias used for `reboot` (defaults to the only configured device)." }
            },
            "required": ["file"]
        }),
        "Copy a .uf2 onto a Pico bootloader volume",
    )
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
    let mut session = probe::attach(chip)?;
    let downloaded = match kind {
        ImageKind::Elf => download_file(&mut session, file, ElfLoader(ElfOptions::default())),
        ImageKind::Hex => download_file(&mut session, file, HexLoader),
        ImageKind::Uf2 => download_file(&mut session, file, Uf2Loader),
        ImageKind::Bin => download_file(
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

/// `.bin` 必须有显式地址：默认落到 0x0 会把固件写到不该写的区域。
fn required_bin_address(args: &Value, kind: ImageKind) -> Result<Option<u64>, ToolError> {
    let raw = args.get("address").and_then(Value::as_str).map(str::trim);
    match (kind, raw) {
        (ImageKind::Bin, None) => Err(ToolError(
            "`address` is required for a raw `.bin` image (e.g. `0x08000000`).".to_string(),
        )),
        (ImageKind::Bin, Some(raw)) => match probe::parse_address(raw) {
            Some(address) => Ok(Some(address)),
            None => Err(ToolError(format!(
                "`address` must be hex (e.g. `0x08000000`), got `{raw}`."
            ))),
        },
        (_, _) => Ok(None),
    }
}

/// 执行 `hardware_flash`。
pub(super) fn run_flash(args: &Value, cwd: &str) -> Result<ToolResult, ToolError> {
    let file = args
        .get("file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError("missing required parameter: `file`".to_string()))?;
    let path = resolve_path(file, cwd);

    let Some(kind) = ImageKind::from_path(&path) else {
        return Err(ToolError(format!(
            "unsupported firmware format for `{}`; expected .elf / .hex / .bin / .uf2",
            path.display()
        )));
    };
    if !path.is_file() {
        return Err(ToolError(format!(
            "firmware file not found: {}",
            path.display()
        )));
    }

    let base_address = required_bin_address(args, kind)?;

    let config = load_config();
    let chip = args
        .get("chip")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| config.probe_chip_for(args.get("board").and_then(Value::as_str)));
    let Some(chip) = chip else {
        return Ok(ToolResult::error(format!(
            "No probe target. Pass `chip` (e.g. `STM32F401RETx`) or add \"chip\" / a probe-capable \
             board to {}.",
            config_path().display()
        )));
    };

    match flash(&chip, &path, kind, base_address) {
        Ok(report) => Ok(ToolResult::text(report)),
        Err(err) => Ok(ToolResult::error(err)),
    }
}

/// 找一个看起来像 Pico bootloader 的挂载点。
fn find_bootloader_volume() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    for name in BOOTLOADER_VOLUMES {
        candidates.push(PathBuf::from("/Volumes").join(name));
        candidates.push(PathBuf::from("/media").join(name));
    }
    for user_dir in ["/media", "/run/media"] {
        let Ok(entries) = fs::read_dir(user_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            for name in BOOTLOADER_VOLUMES {
                candidates.push(entry.path().join(name));
            }
        }
    }
    candidates.into_iter().find(|dir| dir.is_dir())
}

/// 等 bootloader 挂载点出现（最多 [`MOUNT_WAIT`]）。
fn wait_for_bootloader_volume() -> Option<PathBuf> {
    let started = Instant::now();
    while started.elapsed() < MOUNT_WAIT {
        if let Some(dir) = find_bootloader_volume() {
            return Some(dir);
        }
        std::thread::sleep(MOUNT_POLL);
    }
    find_bootloader_volume()
}

/// 执行 `pico_flash`。
pub(super) async fn run_pico_flash(args: &Value, cwd: &str) -> Result<ToolResult, ToolError> {
    let file = args
        .get("file")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError("missing required parameter: `file`".to_string()))?;
    let path = resolve_path(file, cwd);
    if path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        != Some("uf2".to_string())
    {
        return Err(ToolError(format!(
            "`{}` is not a .uf2 file; the bootloader volume only accepts UF2",
            path.display()
        )));
    }
    if !path.is_file() {
        return Err(ToolError(format!(
            "firmware file not found: {}",
            path.display()
        )));
    }

    let mut mount = args
        .get("mount")
        .and_then(Value::as_str)
        .map(|m| resolve_path(m, cwd))
        .filter(|dir| dir.is_dir());

    if mount.is_none() && args.get("reboot").and_then(Value::as_bool).unwrap_or(false) {
        let config = load_config();
        match config.serial_target(args) {
            Ok((alias, path, baud)) => {
                let command = serial::Command::new("reboot_bootsel", json!({}));
                if let Err(err) = serial::send(&path, baud, command).await {
                    return Ok(ToolResult::error(format!(
                        "could not ask {alias} ({path}) to reboot into the bootloader: {err}"
                    )));
                }
            }
            Err(err) => return Ok(ToolResult::error(err)),
        }
        mount = wait_for_bootloader_volume();
    }
    if mount.is_none() {
        mount = find_bootloader_volume();
    }

    let Some(mount) = mount else {
        return Ok(ToolResult::error(format!(
            "No bootloader volume found (looked for {}). Put the board into bootloader mode \
             (hold BOOTSEL while plugging in), or pass `mount` / set `reboot`.",
            BOOTLOADER_VOLUMES.join(", ")
        )));
    };

    let name = path
        .file_name()
        .ok_or_else(|| ToolError("firmware path has no file name".to_string()))?;
    let target = mount.join(name);
    if let Err(err) = fs::copy(&path, &target) {
        return Ok(ToolResult::error(format!(
            "could not copy `{}` to `{}`: {err}",
            path.display(),
            target.display()
        )));
    }

    Ok(ToolResult::text(format!(
        "Copied {} to {} ({} bytes). The board verifies the UF2 and reboots into the new firmware.",
        path.display(),
        target.display(),
        fs::metadata(&target)
            .map(|meta| meta.len().to_string())
            .unwrap_or_else(|_| "?".to_string())
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{AUTH_TEST_LOCK, AgentDirGuard};
    use std::sync::MutexGuard;

    /// 串行锁 + 临时 agent_dir。
    fn guard() -> (MutexGuard<'static, ()>, AgentDirGuard) {
        let lock = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        (lock, AgentDirGuard::temp())
    }

    #[test]
    fn bin_images_require_an_address() {
        let err = required_bin_address(&json!({}), ImageKind::Bin).unwrap_err();
        assert!(err.0.contains("required"), "{}", err.0);
        assert_eq!(
            required_bin_address(&json!({ "address": "0x08000000" }), ImageKind::Bin).unwrap(),
            Some(0x0800_0000)
        );
        // ELF 自带地址：给了也忽略
        assert_eq!(
            required_bin_address(&json!({ "address": "0x08000000" }), ImageKind::Elf).unwrap(),
            None
        );
    }

    #[test]
    fn missing_firmware_file_is_reported_before_touching_the_probe() {
        let (_g, _ad) = guard();
        let err = run_flash(&json!({ "file": "no-such-fw.elf" }), "/tmp").unwrap_err();
        assert!(err.0.contains("not found"), "{}", err.0);
    }

    #[test]
    fn unsupported_formats_are_rejected() {
        let (_g, _ad) = guard();
        let dir = std::env::temp_dir().join("prux-hardware-flash-test");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("fw.txt");
        fs::write(&file, b"nope").unwrap();
        let err = run_flash(&json!({ "file": file }), "/tmp").unwrap_err();
        assert!(err.0.contains("unsupported firmware format"), "{}", err.0);
    }

    #[test]
    fn pico_flash_rejects_non_uf2_files() {
        let (_g, _ad) = guard();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = std::env::temp_dir().join("prux-hardware-uf2-test");
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("fw.elf");
        fs::write(&file, b"nope").unwrap();
        let err = runtime
            .block_on(run_pico_flash(&json!({ "file": file }), "/tmp"))
            .unwrap_err();
        assert!(err.0.contains("not a .uf2"), "{}", err.0);
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
}

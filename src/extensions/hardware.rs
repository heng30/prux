//! `hardware` 扩展：芯片手册检索 + SWD/JTAG 探针通道。
//!
//! 两块能力，对应嵌入式排障的两个方向：
//!
//! **1. 查手册**（[`search`]）——资料放 `agent_dir()/extensions/hardware/datasheets/`
//! （可递归建芯片子目录），收 `.md` / `.markdown` / `.txt` / `.csv` / `.tsv` / `.json` /
//! `.rst` / `.adoc`；`hardware_search` 按词元给每块打分，返回 `file:line` + 片段。
//! 引脚别名不需要第二套机制：引脚表的一行（`PA9,USART1_TX,TIM1_CH2`）就是一块，
//! 引脚名与复用功能名都能命中。
//!
//! **2. 看板子**（[`probe`]）——通过 `probe-rs` 直连调试探针（板载 ST-Link 也算）：
//! `hardware_board_info` 读芯片名与架构，`hardware_memory_map` 列 RAM/Flash 区间，
//! `hardware_memory_read` 按地址读一段内存并渲染 hex dump。探针不可用时前两个工具
//! **回落 datasheet 静态表**（[`boards`]）——「这块板是什么芯片」不需要插板就能答。
//!
//! 配置在 `agent_dir()/extensions/hardware/config.json`：
//!
//! ```json
//! { "boards": ["nucleo-f401re"], "chip": "STM32F401RETx" }
//! ```
//!
//! - `boards`：板卡名（对应 [`boards::BOARDS`]），工具省略 `board` 参数时取第一个；
//! - `chip`：probe-rs 的 target 名覆盖（非 Nucleo 的板子靠它直连）。
//!
//! 探针 IO 是阻塞的（USB 传输），因此三个探针工具都在 `spawn_blocking` 里跑；
//! 同一时刻只能有一个会话占着探针——这是 probe-rs 的语义。
//!
//! 工具全部是延迟工具（`ToolExposure::Deferred`）：模型先用 `tool_search` 检索到
//! `hardware_*`，下一轮才拿到声明，因此隐式依赖 `tool-search` 扩展。
//!
//! 默认不启用，且仅在 `Dev` 模式下可用：它不是通用工具，占工具位只在嵌入式场景值。

mod arduino;
mod boards;
mod bridge;
mod flash;
mod gpio;
mod micropython;
mod probe;
mod rpi;
mod search;
mod serial;

use crate::{
    core::{
        config_cache::ConfigCache,
        extensions::{
            CommandHost, Extension, ExtensionCommand, ExtensionMode, ExtensionTool,
            ForkProjectInfo, ToolAnnotations, ToolExecCtx, ToolExposure, UiNotifyLevel,
        },
        settings_manager::agent_dir,
        tools::{ToolError, ToolResult},
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_HARDWARE, command_arg,
        util::{notify_plain, opt_string},
    },
    modes::tui::handlers::register_host_command,
    utils::paths::read_json_file,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, RwLock},
};

/// 扩展名（也是 `/extension` 面板中的标识与配置目录名）。
const EXT: &str = "hardware";

/// 斜杠命令名（不含 `/`）。
const CMD: &str = "hardware";

/// 配置文件名（位于 `agent_dir()/extensions/hardware/` 下）。
const CONFIG_FILE: &str = "config.json";

/// 配置文件里的键：板卡名列表。
const CONFIG_BOARDS: &str = "boards";

/// 配置文件里的键：probe-rs 的 target 名覆盖。
const CONFIG_CHIP: &str = "chip";

/// 配置文件里的键：设备别名 → 串口路径。
const CONFIG_DEVICES: &str = "devices";

/// 配置文件里的键：串口波特率。
const CONFIG_BAUD: &str = "baud";

/// 工具名：板卡信息。
const TOOL_BOARD_INFO: &str = "hardware_board_info";

/// 工具名：内存映射。
const TOOL_MEMORY_MAP: &str = "hardware_memory_map";

/// 工具名：内存读取。
const TOOL_MEMORY_READ: &str = "hardware_memory_read";

/// 工具名：串口设备清单。
const TOOL_DEVICES: &str = "hardware_devices";

/// 未给地址时默认读的起点：STM32 的 RAM 基底（zeroclaw 的默认值）。
const DEFAULT_READ_ADDRESS: u64 = 0x2000_0000;

/// 未给长度时默认读的字节数。
const DEFAULT_READ_BYTES: usize = 128;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static HARDWARE_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_HARDWARE,
    make: || -> Arc<dyn Extension> { Arc::new(Hardware) },
};

/// 配置文件读写锁（`/hardware` 与工具读取串行化）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// 配置文件的读缓存（锁序恒为「RwLock → 缓存内层 Mutex」）。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 资料目录：`agent_dir()/extensions/hardware/datasheets`。
fn datasheet_dir() -> PathBuf {
    agent_dir().join(search::DATASHEET_REL)
}

/// 一台设备怎么连。
#[derive(Debug, Clone, PartialEq, Eq)]
enum DeviceTarget {
    /// 串口设备路径（`/dev/ttyACM0`、`COM3`）。
    Serial(String),
    /// Arduino Uno Q Bridge 的 `host:port`。
    Bridge(String),
}

impl std::fmt::Display for DeviceTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeviceTarget::Serial(path) => write!(f, "{path}"),
            DeviceTarget::Bridge(addr) => write!(f, "bridge:{addr}"),
        }
    }
}

/// 配置文件路径：`agent_dir()/extensions/hardware/config.json`。
fn config_path() -> PathBuf {
    agent_dir().join("extensions").join(EXT).join(CONFIG_FILE)
}

/// 运行期配置。
#[derive(Debug, Clone, Default)]
struct Config {
    /// 板卡名列表（对应 [`boards::BOARDS`]）；空 = 没配置，工具要求显式给 `board`。
    boards: Vec<String>,
    /// probe-rs 的 target 名覆盖；空 = 按板卡表推导。
    chip: Option<String>,
    /// 设备别名 → 传输方式（`pico0` → 串口，`unoq` → 本地 Bridge）。
    devices: BTreeMap<String, DeviceTarget>,
    /// 串口波特率。
    baud: u32,
}

impl Config {
    /// 省略 `board` 参数时的默认板卡。
    fn default_board(&self) -> Option<&str> {
        self.boards.first().map(String::as_str)
    }

    /// 探针 target 名：显式 `chip` 覆盖优先，否则按板卡表推导。
    fn probe_chip_for(&self, board: Option<&str>) -> Option<String> {
        if let Some(chip) = &self.chip
            && !chip.is_empty()
        {
            return Some(chip.clone());
        }
        let board = board.or_else(|| self.default_board())?;
        boards::lookup(board)
            .and_then(|spec| spec.probe_chip)
            .map(str::to_string)
    }

    /// 设备目标：`device` 别名 → `(别名, 传输方式)`。
    ///
    /// 省略别名时要求**恰好**配置了一个设备：多设备时让模型显式选，避免「随手操作第一个」
    /// 这种在真板子上会烧错的默认。
    fn device_target(&self, args: &Value) -> std::result::Result<(String, DeviceTarget), String> {
        if let Some(alias) = opt_string(args, "device") {
            return self
                .devices
                .get(&alias)
                .map(|target| (alias.clone(), target.clone()))
                .ok_or_else(|| {
                    format!(
                        "unknown device `{alias}`; configured: {}",
                        self.device_aliases()
                    )
                });
        }
        match self.devices.len() {
            0 => Err(format!(
                "no device configured; add \"devices\": {{\"pico0\": \"/dev/ttyACM0\"}} to {}",
                config_path().display()
            )),
            1 => {
                let (alias, target) = self.devices.iter().next().expect("len == 1");
                Ok((alias.clone(), target.clone()))
            }
            _ => Err(format!(
                "multiple devices configured; pass `device` (one of: {})",
                self.device_aliases()
            )),
        }
    }

    /// 串口路径：只接受串口设备（`mpremote` / `arduino-cli` / `pico_flash` 要的是路径）。
    fn serial_target(&self, args: &Value) -> std::result::Result<(String, String), String> {
        let (alias, target) = self.device_target(args)?;
        match target {
            DeviceTarget::Serial(path) => Ok((alias, path)),
            DeviceTarget::Bridge(addr) => Err(format!(
                "device `{alias}` is a Uno Q Bridge ({addr}), not a serial port"
            )),
        }
    }

    /// 已配置别名的展示串（错误提示用）。
    fn device_aliases(&self) -> String {
        if self.devices.is_empty() {
            "(none)".to_string()
        } else {
            self.devices.keys().cloned().collect::<Vec<_>>().join(", ")
        }
    }
}

/// 读配置（缺失 / 非法时返回空配置，不影响其它工具）。
fn load_config() -> Config {
    let path = config_path();
    let _guard = CONFIG_LOCK.read().unwrap_or_else(|e| e.into_inner());
    let value = CONFIG_CACHE.read(&path, || read_json_file(&path));

    let boards = value
        .get(CONFIG_BOARDS)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let chip = value
        .get(CONFIG_CHIP)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let devices = value
        .get(CONFIG_DEVICES)
        .and_then(Value::as_object)
        .map(|map| {
            map.iter()
                .filter_map(|(alias, value)| {
                    let target = match value {
                        // 字面量 `"bridge"` = 用 Uno Q Bridge 的默认地址
                        Value::String(word) if word.trim() == "bridge" => {
                            DeviceTarget::Bridge(bridge::DEFAULT_ADDR.to_string())
                        }
                        Value::String(path) if !path.trim().is_empty() => {
                            DeviceTarget::Serial(path.trim().to_string())
                        }
                        Value::Object(obj) => {
                            let addr = obj.get("bridge")?.as_str()?.trim();
                            if addr.is_empty() {
                                return None;
                            }
                            DeviceTarget::Bridge(addr.to_string())
                        }
                        _ => return None,
                    };
                    Some((alias.trim().to_lowercase(), target))
                })
                .collect()
        })
        .unwrap_or_default();
    let baud = value
        .get(CONFIG_BAUD)
        .and_then(Value::as_u64)
        .filter(|baud| (1..=4_000_000).contains(baud))
        .map(|baud| baud as u32)
        .unwrap_or(serial::DEFAULT_BAUD);

    Config {
        boards,
        chip,
        devices,
        baud,
    }
}

/// 某块板卡的静态资料块（工具省略 `board` 时用默认板卡）。
fn resolve_board(args: &Value, config: &Config) -> Option<String> {
    opt_string(args, "board").or_else(|| config.default_board().map(str::to_string))
}

/// 板卡静态信息文本（含内存映射与探针不可用的说明）。
fn static_board_info(spec: &boards::BoardSpec, probe_error: Option<&str>) -> String {
    let mut out = format!(
        "**Board:** {}\n**Chip:** {}\n**Description:** {}\n\n**Memory map:**\n{}",
        spec.board, spec.chip, spec.description, spec.memory_map
    );
    if let Some(err) = probe_error {
        out.push_str(&format!(
            "\n\n(probe-rs unavailable, showing datasheet info: {err})"
        ));
    }
    out
}

/// `hardware_board_info`：探针优先，失败回落静态表。
fn board_info(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let config = load_config();
    let Some(board) = resolve_board(args, &config) else {
        return Ok(ToolResult::error(format!(
            "No board selected. Pass `board` or add one to {}: known boards: {}.",
            config_path().display(),
            boards::names().join(", ")
        )));
    };

    let spec = boards::lookup(&board);
    let probe_chip = config.probe_chip_for(Some(&board));

    if let Some(chip) = &probe_chip {
        match probe::describe(chip) {
            Ok(info) => {
                return Ok(ToolResult::text(match spec {
                    Some(spec) => format!(
                        "**Board:** {}\n**Chip:** {}\n\n{}",
                        spec.board, spec.chip, info
                    ),
                    None => info,
                }));
            }
            Err(err) => {
                let Some(spec) = spec else {
                    return Ok(ToolResult::error(format!(
                        "probe-rs could not attach and `{board}` has no datasheet entry: {err}"
                    )));
                };
                return Ok(ToolResult::text(static_board_info(spec, Some(&err))));
            }
        }
    }

    match spec {
        Some(spec) => Ok(ToolResult::text(static_board_info(spec, None))),
        None => Ok(ToolResult::error(format!(
            "Board `{board}` is configured but has no datasheet entry. Known boards: {}.",
            boards::names().join(", ")
        ))),
    }
}

/// `hardware_memory_map`：探针优先，失败回落 datasheet。
fn memory_map(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let config = load_config();
    let Some(board) = resolve_board(args, &config) else {
        return Ok(ToolResult::error(format!(
            "No board selected. Pass `board` or add one to {}: known boards: {}.",
            config_path().display(),
            boards::names().join(", ")
        )));
    };

    if let Some(chip) = config.probe_chip_for(Some(&board)) {
        match probe::memory_map(&chip) {
            Ok(map) => {
                return Ok(ToolResult::text(format!(
                    "**{board}** (via probe-rs):\n{map}"
                )));
            }
            Err(err) => {
                if let Some(spec) = boards::lookup(&board) {
                    return Ok(ToolResult::text(format!(
                        "**{board}** (from datasheet; probe-rs failed: {err}):\n{}",
                        spec.memory_map
                    )));
                }
            }
        }
    }

    match boards::lookup(&board) {
        Some(spec) => Ok(ToolResult::text(format!(
            "**{board}** (from datasheet):\n{}",
            spec.memory_map
        ))),
        None => Ok(ToolResult::error(format!(
            "No memory map for board `{board}`. Known boards: {}.",
            boards::names().join(", ")
        ))),
    }
}

/// 解析 `hardware_memory_read` 的目标芯片（显式 `chip` > 配置 `chip` > 板卡表）。
fn read_chip(args: &Value, config: &Config) -> Option<String> {
    opt_string(args, "chip").or_else(|| config.probe_chip_for(None))
}

/// `hardware_memory_read`：必须走探针（读的是板子上真实的寄存器）。
fn memory_read(args: &Value) -> std::result::Result<ToolResult, ToolError> {
    let config = load_config();
    let Some(chip) = read_chip(args, &config) else {
        return Ok(ToolResult::error(format!(
            "No probe target. Pass `chip` (e.g. `STM32F401RETx`) or add \"chip\" / a probe-capable \
             board to {}.",
            config_path().display()
        )));
    };

    let address = match opt_string(args, "address") {
        Some(raw) => match probe::parse_address(&raw) {
            Some(address) => address,
            None => {
                return Err(ToolError(format!(
                    "`address` must be hex (e.g. `0x20000000`), got `{raw}`."
                )));
            }
        },
        None => DEFAULT_READ_ADDRESS,
    };

    let length = match args.get("length").and_then(Value::as_u64) {
        Some(0) => return Err(ToolError("`length` must be greater than 0.".to_string())),
        Some(n) => (n as usize).min(probe::MAX_READ_BYTES),
        None => DEFAULT_READ_BYTES,
    };

    let text = probe::read_memory(&chip, address, length)
        .map_err(|e| ToolError(format!("hardware_memory_read failed: {e}")))?;
    Ok(ToolResult::text(text))
}

/// 在 blocking 线程池里跑探针 IO（USB 传输是阻塞的）。
async fn run_blocking<T, F>(job: F) -> std::result::Result<T, ToolError>
where
    T: Send + 'static,
    F: FnOnce() -> std::result::Result<T, ToolError> + Send + 'static,
{
    match tokio::task::spawn_blocking(job).await {
        Ok(result) => result,
        Err(err) => Err(ToolError(format!("hardware probe task failed: {err}"))),
    }
}

/// 设备清单工具的 UI 标签。
fn tool_labels() -> String {
    "Hardware Devices".to_string()
}

/// 构造一个探针工具的公共外壳（延迟工具 + 只读注解）。
fn probe_tool(name: &str, description: &str, parameters: Value, snippet: &str) -> ExtensionTool {
    let mut tool = ExtensionTool::simple(name, description, parameters, snippet)
        .with_exposure(ToolExposure::Deferred)
        .with_annotations(ToolAnnotations {
            read_only: true,
            destructive: false,
            idempotent: true,
        });
    tool.label = Some("Hardware Probe".to_string());
    tool
}

/// `hardware_board_info` 的工具定义。
fn board_info_tool() -> ExtensionTool {
    probe_tool(
        TOOL_BOARD_INFO,
        "Return chip, architecture and memory map of a connected board. Reads the target over \
         SWD/JTAG when a debug probe is attached; otherwise falls back to the built-in datasheet \
         table. Use when asked what board / chip is connected.",
        json!({
            "type": "object",
            "properties": {
                "board": {
                    "type": "string",
                    "description": "Board name from the datasheet table (e.g. nucleo-f401re, arduino-uno). Omit to use the configured default."
                }
            }
        }),
        "Read connected board info (chip, architecture, memory map)",
    )
}

/// `hardware_memory_map` 的工具定义。
fn memory_map_tool() -> ExtensionTool {
    probe_tool(
        TOOL_MEMORY_MAP,
        "Return the flash / RAM address ranges of a board. Reads them from the probe's target \
         definition when a debug probe is attached; otherwise falls back to the datasheet table.",
        json!({
            "type": "object",
            "properties": {
                "board": {
                    "type": "string",
                    "description": "Board name from the datasheet table (e.g. nucleo-f401re). Omit to use the configured default."
                }
            }
        }),
        "Return flash / RAM address ranges of a board",
    )
}

/// `hardware_memory_read` 的工具定义。
fn memory_read_tool() -> ExtensionTool {
    probe_tool(
        TOOL_MEMORY_READ,
        &format!(
            "Read live memory / register values from a target over SWD/JTAG and render a hex dump. \
             Requires an attached debug probe. Max {} bytes per call.",
            probe::MAX_READ_BYTES
        ),
        json!({
            "type": "object",
            "properties": {
                "address": {
                    "type": "string",
                    "description": "Start address in hex (e.g. `0x20000000`). Default: 0x20000000 (STM32 RAM base)."
                },
                "length": {
                    "type": "integer",
                    "description": format!("Bytes to read (default {DEFAULT_READ_BYTES}, max {}).", probe::MAX_READ_BYTES)
                },
                "chip": {
                    "type": "string",
                    "description": "probe-rs target name (e.g. `STM32F401RETx`). Omit to use the configured chip / board."
                }
            }
        }),
        "Read live memory / registers over SWD/JTAG (hex dump)",
    )
}

/// `/hardware` 命令：打印资料目录、板卡配置与工具清单（只读诊断）。
fn command_hardware(_host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    if !arg.is_empty() {
        notify_plain(
            "Usage: /hardware (no arguments) — prints the datasheet directory, board config and tools.",
            UiNotifyLevel::Warning,
        );
        return false;
    }

    let config = load_config();
    let root = datasheet_dir();
    let (files, chunks) = search::stats(&root);
    let boards = if config.boards.is_empty() {
        "(none — pass `board` to the tools or add \"boards\" to config.json)".to_string()
    } else {
        config.boards.join(", ")
    };
    let chip = config
        .chip
        .as_deref()
        .unwrap_or("(derived from the board table)");

    notify_plain(
        format!(
            "hardware\n  datasheets: {}\n  files: {files}, chunks: {chunks} ({})\n  boards: {boards}\n  probe chip: {chip}\n  serial devices: {}\n  tools: {} (deferred; enable `tool-search` to discover them)",
            root.display(),
            if root.is_dir() {
                search::text_extensions().join(", ")
            } else {
                "directory missing".to_string()
            },
            config.device_aliases(),
            Hardware
                .tools()
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        UiNotifyLevel::Info,
    );
    false
}

/// `hardware` 扩展主体。
pub struct Hardware;

impl Extension for Hardware {
    /// 返回固定扩展名 `hardware`。
    fn name(&self) -> &str {
        EXT
    }

    /// `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Search local datasheets and pinout tables, and read boards over SWD/JTAG (hardware_* tools)"
    }

    /// 声明本扩展移植自 zeroclaw 0.8.7，供 `/extension` 面板展示来源链接。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "zeroclaw".to_string(),
            plugin_version: "0.8.7".to_string(),
            url: "https://github.com/zeroclaw-labs/zeroclaw".to_string(),
        })
    }

    /// 默认关闭：只在嵌入式场景值，不该默认占工具位。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` 扩展模式下可用（嵌入式是细分场景，不给 `Creator` 档放行）。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 工具都是延迟工具：模型得先用 `tool_search` 才能发现它们。
    fn prerequisite_tools(&self) -> Vec<String> {
        vec![crate::extensions::tool_search::TOOL_NAME.to_string()]
    }

    /// 全部工具：手册检索、探针读写、引脚、烧写、Arduino 上传与 MicroPython 执行。
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![
            search::search_tool(),
            board_info_tool(),
            memory_map_tool(),
            memory_read_tool(),
            gpio::devices_tool(),
            gpio::read_tool(),
            gpio::write_tool(),
            gpio::capabilities_tool(),
            rpi::read_tool(),
            rpi::write_tool(),
            rpi::blink_tool(),
            rpi::info_tool(),
            flash::flash_tool_def(),
            flash::pico_flash_tool_def(),
            arduino::upload_tool(),
        ]
        .into_iter()
        .chain(micropython::tools())
        .collect()
    }

    /// 声明 `/hardware` 命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Print the datasheet directory, board config and the hardware_* tool list"
                .to_string(),
            busy_safe: true,
            subcommands: Vec::new(),
        }]
    }

    /// 绑定 `/hardware` 的执行入口。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_hardware);
    }

    /// 执行 `hardware_*` 工具；探针类工具在 blocking 线程池里跑。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        ctx: ToolExecCtx,
    ) -> BoxFuture<'static, std::result::Result<ToolResult, ToolError>> {
        let cwd = ctx.cwd.clone();
        Box::pin(async move {
            match name.as_str() {
                search::TOOL_SEARCH => search::run_search(&args),
                TOOL_BOARD_INFO => run_blocking(move || board_info(&args)).await,
                TOOL_MEMORY_MAP => run_blocking(move || memory_map(&args)).await,
                TOOL_MEMORY_READ => run_blocking(move || memory_read(&args)).await,
                TOOL_DEVICES => gpio::run_devices(&args).await,
                gpio::TOOL_GPIO_READ => gpio::run_read(&args).await,
                gpio::TOOL_GPIO_WRITE => gpio::run_write(&args).await,
                gpio::TOOL_CAPABILITIES => gpio::run_capabilities(&args).await,
                rpi::TOOL_RPI_READ => rpi::run_read(&args).await,
                rpi::TOOL_RPI_WRITE => rpi::run_write(&args).await,
                rpi::TOOL_RPI_BLINK => rpi::run_blink(&args).await,
                rpi::TOOL_RPI_INFO => rpi::run_info().await,
                flash::TOOL_FLASH => run_blocking(move || flash::run_flash(&args, &cwd)).await,
                flash::TOOL_PICO_FLASH => flash::run_pico_flash(&args, &cwd).await,
                arduino::TOOL_UPLOAD => arduino::run_upload(&args).await,
                micropython::TOOL_READ_CODE => micropython::run_read(&args).await,
                micropython::TOOL_WRITE_CODE => micropython::run_write(&args).await,
                micropython::TOOL_EXEC => micropython::run_exec(&args).await,
                other => Err(ToolError(format!("extension {EXT}: unknown tool: {other}"))),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{AUTH_TEST_LOCK, AgentDirGuard};
    use std::sync::MutexGuard;

    /// 串行锁 + 临时 agent_dir：配置与资料目录都落在沙箱里。
    fn guard() -> (MutexGuard<'static, ()>, AgentDirGuard) {
        let lock = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        (lock, AgentDirGuard::temp())
    }

    /// 写一份板卡配置。
    fn write_config(body: &Value) {
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body.to_string()).unwrap();
    }

    #[test]
    fn board_info_falls_back_to_the_datasheet_table() {
        let (_g, _ad) = guard();
        let out = board_info(&json!({ "board": "arduino-uno" })).unwrap();
        assert!(out.text.contains("ATmega328P"), "{}", out.text);
        assert!(out.text.contains("Flash: 0x0000"), "{}", out.text);
    }

    #[test]
    fn an_empty_config_asks_for_a_board_and_lists_the_known_ones() {
        let (_g, _ad) = guard();
        let out = board_info(&json!({})).unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("nucleo-f401re"), "{}", out.text);
    }

    #[test]
    fn the_configured_board_is_the_default() {
        let (_g, _ad) = guard();
        write_config(&json!({ "boards": ["nucleo-f411re"] }));
        let out = board_info(&json!({})).unwrap();
        // 探针不可用 → 静态表兜底（不是报错），并点明失败原因
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("STM32F411RET6"), "{}", out.text);
        assert!(out.text.contains("probe-rs unavailable"), "{}", out.text);
    }

    #[test]
    fn memory_map_falls_back_to_the_datasheet() {
        let (_g, _ad) = guard();
        let out = memory_map(&json!({ "board": "esp32" })).unwrap();
        assert!(out.text.contains("IRAM"), "{}", out.text);
    }

    #[test]
    fn memory_read_without_a_chip_explains_how_to_configure_one() {
        let (_g, _ad) = guard();
        let out = memory_read(&json!({})).unwrap();
        assert!(out.is_error);
        assert!(out.text.contains("config.json"), "{}", out.text);
    }

    #[test]
    fn memory_read_rejects_a_bad_address() {
        let (_g, _ad) = guard();
        assert!(memory_read(&json!({ "chip": "STM32F401RETx", "address": "zzz" })).is_err());
        assert!(memory_read(&json!({ "chip": "STM32F401RETx", "length": 0 })).is_err());
    }

    #[test]
    fn memory_read_reports_probe_failure_instead_of_panicking() {
        let (_g, _ad) = guard();
        let out =
            memory_read(&json!({ "chip": "NoSuchChipXyz", "address": "0x20000000", "length": 8 }));
        assert!(out.is_err(), "无探针时应是错误结果: {out:?}");
    }

    #[test]
    fn chip_override_beats_the_board_table() {
        let config = Config {
            boards: vec!["nucleo-f401re".to_string()],
            chip: Some("STM32F411RETx".to_string()),
            ..Config::default()
        };
        assert_eq!(
            config.probe_chip_for(Some("nucleo-f401re")),
            Some("STM32F411RETx".to_string())
        );
        let derived = Config {
            boards: vec!["nucleo-f401re".to_string()],
            ..Config::default()
        };
        assert_eq!(
            derived.probe_chip_for(None),
            Some("STM32F401RETx".to_string())
        );
    }

    #[test]
    fn tools_are_all_deferred_and_named_consistently() {
        let names: Vec<String> = Hardware.tools().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "hardware_search",
                "hardware_board_info",
                "hardware_memory_map",
                "hardware_memory_read",
                "hardware_devices",
                "gpio_read",
                "gpio_write",
                "hardware_capabilities",
                "gpio_rpi_read",
                "gpio_rpi_write",
                "gpio_rpi_blink",
                "rpi_system_info",
                "hardware_flash",
                "pico_flash",
                "arduino_upload",
                "device_read_code",
                "device_write_code",
                "device_exec"
            ]
        );
        for tool in Hardware.tools() {
            assert_eq!(tool.exposure, ToolExposure::Deferred, "{}", tool.name);
        }
    }
}

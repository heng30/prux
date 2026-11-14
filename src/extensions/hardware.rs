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
//! `hardware_memory_read` 按地址读一段内存并渲染 hex dump。没配置板卡时
//! `hardware_board_info` 先列探针、让 probe-rs 自己认芯片，`hardware_update_board` 把
//! 认出的 target 名写进配置；探针不可用时前两个工具**回落 datasheet 静态表**
//! （[`boards`]）——「这块板是什么芯片」不需要插板就能答。
//!
//! 配置在 `agent_dir()/extensions/hardware/config.json`，一个数组，每块板子一项：
//!
//! ```json
//! [
//!   { "board": "nucleo-f401re", "chip": "STM32F401RETx",
//!     "devices": { "pico0": "/dev/ttyACM0" }, "baud": 115200 }
//! ]
//! ```
//!
//! - `board`：板卡名（对应 [`boards::BOARDS`]，表外板子也能写）；数组第一项是默认板卡；
//! - `chip`：这块钱的 probe-rs target 名；省略则按板卡表推导；
//! - `devices`：这块板上的设备别名（`"bridge"` = Uno Q Bridge 默认地址）；
//! - `baud`：这块板的串口波特率，省略用串口默认值。
//!
//! 探针 IO 是阻塞的（USB 传输），因此探针类工具都在 `spawn_blocking` 里跑；
//! 同一时刻只能有一个会话占着探针——这是 probe-rs 的语义。
//!
//! 工具全部是延迟工具（`ToolExposure::Deferred`）：模型先用 `tool_search` 检索到
//! `hardware_*`，下一轮才拿到声明，因此隐式依赖 `tool-search` 扩展。
//!
//! 默认不启用，且仅在 `Dev` 模式下可用：它不是通用工具，占工具位只在嵌入式场景值。

mod arduino;
mod boards;
mod bridge;
mod config;
mod flash;
mod gpio;
mod micropython;
mod probe;
mod rpi;
mod search;
mod serial;

use crate::{
    core::{
        extensions::{
            CommandHost, Extension, ExtensionCommand, ExtensionMode, ExtensionTool,
            ForkProjectInfo, SelectOption, ToolAnnotations, ToolExecCtx, ToolExposure,
            UiNotifyLevel, ask_pending, ask_prompt, ui_ready,
        },
        settings_manager::agent_dir,
        tools::{ToolError, ToolResult},
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_HARDWARE, command_arg, tool_search,
        util::{notify_plain, opt_string},
    },
    modes::tui::handlers::register_host_command,
    utils::{paths::write_if_missing, process},
};
use boards::BoardSpec;
use config::{
    CONFIG_TEMPLATE, CONFIG_TEMPLATE_FILE, Config, config_path, load_config, upsert_board,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{fs, path::PathBuf, sync::Arc, time::Duration};

/// 扩展名（也是 `/extension` 面板中的标识与配置目录名）。
const EXT: &str = "hardware";

/// 斜杠命令名（不含 `/`）。
const CMD: &str = "hardware";

/// 工具名：板卡信息。
const TOOL_BOARD_INFO: &str = "hardware_board_info";

/// 工具名：内存映射。
const TOOL_MEMORY_MAP: &str = "hardware_memory_map";

/// 工具名：内存读取。
const TOOL_MEMORY_READ: &str = "hardware_memory_read";

/// 工具名：串口设备清单。
const TOOL_DEVICES: &str = "hardware_devices";

/// 工具名：把探测到的芯片写进 `config.json`。
const TOOL_UPDATE_BOARD: &str = "hardware_update_board";

/// 写入确认面板的「同意」选项串（`starts_with("save")` 判定，故前缀须为 `save`）。
const CONFIRM_SAVE: &str = "save  Write this board into config.json";

/// 写入确认面板的「取消」选项串。
const CONFIRM_CANCEL: &str = "cancel  Leave config.json untouched";

/// 未给地址时默认读的起点：STM32 的 RAM 基底
const DEFAULT_READ_ADDRESS: u64 = 0x2000_0000;

/// 未给长度时默认读的字节数。
const DEFAULT_READ_BYTES: usize = 128;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static HARDWARE_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_HARDWARE,
    make: || -> Arc<dyn Extension> { Arc::new(Hardware) },
};

/// 资料目录：`agent_dir()/extensions/hardware/datasheets`。
fn datasheet_dir() -> PathBuf {
    agent_dir().join(search::DATASHEET_REL)
}

/// 某块板卡的静态资料块（工具省略 `board` 时用默认板卡）。
fn resolve_board(args: &Value, config: &Config) -> Option<String> {
    opt_string(args, "board").or_else(|| config.default_board().map(str::to_string))
}

/// 板卡静态信息文本（含内存映射与探针不可用的说明）。
fn static_board_info(spec: &BoardSpec, probe_error: Option<&str>) -> String {
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
fn board_info(args: &Value) -> Result<ToolResult, ToolError> {
    let config = load_config();
    let Some(board) = resolve_board(args, &config) else {
        // 没配置板卡：先问探针（不需要事先知道芯片是谁）
        return Ok(detect_board_info());
    };

    let spec = boards::lookup(&board); // 按板卡名查静态资料
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

/// 没配置板卡时的主动探测：列探针，再让 probe-rs 自己认芯片。
fn detect_board_info() -> ToolResult {
    let probes = probe::list_probes();
    if probes.is_empty() {
        return ToolResult::error(format!(
            "No board selected and no debug probe found. Pass `board` or add one to {}: known boards: {}.",
            config_path().display(),
            boards::names().join(", ")
        ));
    }
    match probe::detect() {
        Ok((chip, info)) => ToolResult::text(format!(
            "**Probes:**\n{}\n\n{info}\n\nDetected target `{chip}` — `{TOOL_UPDATE_BOARD}` saves it to {}.",
            probes.join("\n"),
            config_path().display()
        )),
        Err(err) => ToolResult::error(format!(
            "**Probes:**\n{}\n\n{err}\n\nPass `board` (known: {}) or `{TOOL_UPDATE_BOARD}` to configure one.",
            probes.join("\n"),
            boards::names().join(", ")
        )),
    }
}

/// 解析要写入的 `(板卡名, chip)`；省略 `chip` 时走探针。
fn resolve_update(args: &Value) -> Result<(String, String), ToolError> {
    let chip = match opt_string(args, "chip") {
        Some(chip) => chip,
        None => probe::detect()
            .map_err(|e| {
                ToolError(format!(
                    "hardware_update_board needs a chip: pass `chip` or attach a debug probe: {e}"
                ))
            })?
            .0,
    };
    let board = opt_string(args, "board").unwrap_or_else(|| chip.to_lowercase());
    Ok((board, chip))
}

/// `hardware_update_board`：探测芯片 → 用户确认 → 写进 `config.json`。
///
/// 写配置文件要改动用户的板卡清单，因此**必须**当场拿到批准：面板被 Esc / 超时 / 非交互
/// 模式下拿不到答复时一律不写（fail closed），与 `permissions`、`hooks` 的门控口径一致。
async fn update_board(args: Value) -> Result<ToolResult, ToolError> {
    let (board, chip) = run_blocking(move || resolve_update(&args)).await?;
    if !ui_ready() || ask_pending() {
        // 没有面板可弹、或已有别的确认在等（再发一个会把它的面板顶掉）
        return Ok(ToolResult::error(format!(
            "Not saved: `{TOOL_UPDATE_BOARD}` needs your approval, but no confirm panel could be \
             shown (non-interactive run, or another prompt is already open). {} was left untouched; \
             add the board to it by hand if you want it saved.",
            config_path().display()
        )));
    }
    let answer = ask_prompt(
        format!(
            "hardware: save board `{board}` (chip `{chip}`) to {}?",
            config_path().display()
        ),
        vec![
            SelectOption::new(CONFIRM_SAVE),
            SelectOption::new(CONFIRM_CANCEL),
        ],
        Some(Duration::from_secs(120)),
    )
    .await;
    if !answer.as_deref().is_some_and(|a| a.starts_with("save")) {
        return Ok(ToolResult::error(format!(
            "Not saved: `{TOOL_UPDATE_BOARD}` needs your approval (the confirm panel was cancelled, \
             timed out, or there is no interactive prompt). {} was left untouched.",
            config_path().display()
        )));
    }
    let path = upsert_board(&board, &chip)
        .map_err(|e| ToolError(format!("hardware_update_board failed: {e}")))?;
    Ok(ToolResult::text(format!(
        "Saved board `{board}` with chip `{chip}` to {}.",
        path.display()
    )))
}

/// `hardware_memory_map`：探针优先，失败回落 datasheet。
fn memory_map(args: &Value) -> Result<ToolResult, ToolError> {
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

/// 解析 `hardware_memory_read` 的目标芯片（显式 `chip` > 该板 / 默认板的配置 `chip` > 板卡表）。
fn read_chip(args: &Value, config: &Config) -> Option<String> {
    opt_string(args, "chip").or_else(|| config.probe_chip_for(opt_string(args, "board").as_deref()))
}

/// `hardware_memory_read`：必须走探针（读的是板子上真实的寄存器）。
fn memory_read(args: &Value) -> Result<ToolResult, ToolError> {
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

/// 在 blocking 线程池里跑探针 IO（USB 传输是阻塞的），把 panic / 取消统一成工具错误。
async fn run_blocking<T, F>(job: F) -> Result<T, ToolError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ToolError> + Send + 'static,
{
    process::spawn_blocking(job)
        .await
        .map_err(|err| ToolError(format!("hardware probe task failed: {err}")))?
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
                    "description": "Board name: a built-in datasheet name (e.g. nucleo-f401re, arduino-uno) or a board from config.json. Omit to use the configured default."
                }
            }
        }),
        "Read connected board info (chip, architecture, memory map)",
    )
}

/// `hardware_update_board` 的工具定义。
fn update_board_tool() -> ExtensionTool {
    let mut tool = probe_tool(
        TOOL_UPDATE_BOARD,
        "Detect the chip behind an attached debug probe and save it as a board in config.json. \
         Add `board` / `chip` to write them without probing. Asks the user to approve the write first.",
        json!({
            "type": "object",
            "properties": {
                "board": {
                    "type": "string",
                    "description": "Board name to save (default: the detected target name, lowercased). An existing entry with the same name keeps its other keys."
                },
                "chip": {
                    "type": "string",
                    "description": "probe-rs target name to save (default: detected over the attached probe)."
                }
            }
        }),
        "Save the probed chip as a board in config.json (asks first)",
    );
    // 写配置文件，不是只读操作
    tool.annotations = ToolAnnotations {
        read_only: false,
        destructive: false,
        idempotent: true,
    };
    tool
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
                    "description": "Board name: a built-in datasheet name (e.g. nucleo-f401re) or a board from config.json. Omit to use the configured default."
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
                },
                "board": {
                    "type": "string",
                    "description": "Board from config.json used to pick the target when `chip` is omitted (defaults to the first configured board)."
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
        "(none — pass `board` to the tools or add an entry to config.json)".to_string()
    } else {
        config
            .boards
            .iter()
            .map(|b| b.board.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let chip = if config.boards.iter().all(|b| b.chip.is_none()) {
        "(derived from the board table)".to_string()
    } else {
        config
            .boards
            .iter()
            .filter_map(|b| b.chip.as_ref().map(|chip| format!("{}={chip}", b.board)))
            .collect::<Vec<_>>()
            .join(", ")
    };

    notify_plain(
        format!(
            "hardware\n  datasheets: {}\n  files: {files}, chunks: {chunks} ({})\n  boards: {boards}\n  probe chip: {chip}\n  serial devices: {}\n  tools: {} (deferred; enable `tool-search` to discover them)",
            root.display(),
            if root.is_dir() {
                search::text_extensions().join(", ")
            } else {
                "directory missing".to_string()
            },
            config.device_summary(),
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
        vec![tool_search::TOOL_NAME.to_string()]
    }

    /// 全部工具：手册检索、探针读写、引脚、烧写、Arduino 上传与 MicroPython 执行。
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![
            search::search_tool(),
            board_info_tool(),
            update_board_tool(),
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

    /// 启用后建好资料目录（含启动时按 settings 恢复的启用态），用户直接往里放手手册。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            // 建不出来不影响检索：缺目录时 `hardware_search` 自己会提示路径。
            _ = fs::create_dir_all(datasheet_dir());
            let config = config_path();
            if let Some(dir) = config.parent() {
                _ = fs::create_dir_all(dir);
            }
            write_if_missing(
                &config.with_file_name(CONFIG_TEMPLATE_FILE),
                CONFIG_TEMPLATE,
            );
            write_if_missing(&config, "[]\n");
        }
    }

    /// 执行 `hardware_*` 工具；探针类工具在 blocking 线程池里跑。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        ctx: ToolExecCtx,
    ) -> BoxFuture<'static, Result<ToolResult, ToolError>> {
        let cwd = ctx.cwd.clone();
        Box::pin(async move {
            match name.as_str() {
                search::TOOL_SEARCH => search::run_search(&args),
                TOOL_BOARD_INFO => run_blocking(move || board_info(&args)).await,
                TOOL_UPDATE_BOARD => update_board(args).await,
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

    /// 跑一个 async 测试体（独立 runtime；UI 队列是进程级全局态，靠 `guard()` 串行）。
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(fut)
    }

    /// 跑 `update_board` 并在确认面板上按 `answer` 作答；面板没出现则用例失败。
    fn update_board_answered(args: Value, answer: &'static str) -> Result<ToolResult, ToolError> {
        use crate::core::extensions::{
            ExtensionUiRequest, set_ui_ready, submit_ui_choice, take_pending_ui,
        };
        set_ui_ready(true);
        let out = block_on(async move {
            let call = update_board(args);
            let respond = async {
                for _ in 0..400 {
                    if let Some(ExtensionUiRequest::Select { id, .. }) = take_pending_ui() {
                        submit_ui_choice(id, Some(answer.to_string()));
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                panic!("no confirm panel was queued");
            };
            tokio::join!(call, respond).0
        });
        set_ui_ready(false);
        out
    }

    /// 写一份板卡配置。
    fn write_config(body: &Value) {
        let path = config_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body.to_string()).unwrap();
    }

    #[test]
    fn enabling_the_extension_creates_the_datasheet_dir() {
        let (_g, _ad) = guard();
        let root = datasheet_dir();
        assert!(!root.exists(), "沙箱里初始没有资料目录");
        Hardware.on_enabled_changed(true);
        assert!(root.is_dir(), "启用后应建出 {}", root.display());
    }

    #[test]
    fn enabling_writes_a_template_and_an_empty_config() {
        let (_g, _ad) = guard();
        Hardware.on_enabled_changed(true);
        let template =
            fs::read_to_string(config_path().with_file_name(CONFIG_TEMPLATE_FILE)).unwrap();
        assert!(template.contains("\"board\""), "{template}");
        assert!(template.contains("\"devices\""), "{template}");
        assert_eq!(fs::read_to_string(config_path()).unwrap().trim(), "[]");
    }

    #[test]
    fn an_existing_config_is_never_overwritten() {
        let (_g, _ad) = guard();
        write_config(&json!([{ "board": "esp32" }]));
        Hardware.on_enabled_changed(true);
        assert_eq!(
            load_config().default_board(),
            Some("esp32"),
            "启用不得清掉用户配置"
        );
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
        // 插着探针时主动探测成功；没有探针才该报错并点出已知板卡
        if out.is_error {
            assert!(out.text.contains("nucleo-f401re"), "{}", out.text);
        } else {
            assert!(out.text.contains("Detected target"), "{}", out.text);
        }
    }

    #[test]
    fn the_configured_board_is_the_default() {
        let (_g, _ad) = guard();
        write_config(&json!([{ "board": "nucleo-f411re" }]));
        let out = board_info(&json!({})).unwrap();
        // 探针不可用 → 静态表兜底（不是报错），并点明失败原因
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("STM32F411RET6"), "{}", out.text);
        assert!(out.text.contains("probe-rs unavailable"), "{}", out.text);
    }

    #[test]
    fn update_board_writes_a_new_entry_and_keeps_the_existing_keys() {
        let (_g, _ad) = guard();
        write_config(&json!([{ "board": "nucleo-f401re", "baud": 9600 }]));
        let saved = update_board_answered(
            json!({ "board": "esp32c6", "chip": "esp32c6" }),
            CONFIRM_SAVE,
        )
        .unwrap();
        assert!(saved.text.contains("Saved"), "{}", saved.text);
        let config = load_config();
        assert_eq!(
            config
                .boards
                .iter()
                .map(|b| b.board.as_str())
                .collect::<Vec<_>>(),
            vec!["nucleo-f401re", "esp32c6"]
        );
        assert_eq!(
            config.probe_chip_for(Some("esp32c6")).as_deref(),
            Some("esp32c6")
        );

        // 同名只更新 chip，既有键（baud / devices）必须还在
        update_board_answered(
            json!({ "board": "nucleo-f401re", "chip": "STM32F401RETx" }),
            CONFIRM_SAVE,
        )
        .unwrap();
        assert_eq!(load_config().default_baud(), 9600, "既有键必须原样保留");
        assert_eq!(
            load_config()
                .probe_chip_for(Some("nucleo-f401re"))
                .as_deref(),
            Some("STM32F401RETx")
        );
    }

    #[test]
    fn update_board_can_be_told_the_chip_without_a_probe() {
        let (_g, _ad) = guard();
        // 不接探针时也必须能用：板上板卡名默认取 chip 的小写
        update_board_answered(json!({ "chip": "esp32c6" }), CONFIRM_SAVE).unwrap();
        assert_eq!(load_config().default_board(), Some("esp32c6"));
    }

    #[test]
    fn update_board_writes_nothing_when_the_user_declines() {
        let (_g, _ad) = guard();
        write_config(&json!([{ "board": "nucleo-f401re" }]));
        let out = update_board_answered(
            json!({ "board": "esp32c6", "chip": "esp32c6" }),
            CONFIRM_CANCEL,
        )
        .unwrap();
        assert!(out.is_error, "{}", out.text);
        assert_eq!(
            load_config().boards.len(),
            1,
            "declined write must not touch config.json"
        );
    }

    #[test]
    fn update_board_refuses_without_an_interactive_prompt() {
        let (_g, _ad) = guard();
        write_config(&json!([{ "board": "nucleo-f401re" }]));
        // 非交互模式（无 TUI 事件循环）→ fail closed，不写文件
        let out = block_on(update_board(
            json!({ "board": "esp32c6", "chip": "esp32c6" }),
        ))
        .unwrap();
        assert!(out.is_error, "{}", out.text);
        assert_eq!(load_config().boards.len(), 1);
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
    fn a_chip_map_picks_the_target_per_board_and_falls_back_to_the_table() {
        let (_g, _ad) = guard();
        write_config(&json!([
            { "board": "nucleo-f401re" },
            { "board": "my-board", "chip": "NRF52840_xxAA" }
        ]));
        let config = load_config();
        assert_eq!(
            config.probe_chip_for(Some("my-board")),
            Some("NRF52840_xxAA".to_string())
        );
        // 没写 chip 的板子仍按内置表推导
        assert_eq!(
            config.probe_chip_for(Some("nucleo-f401re")),
            Some("STM32F401RETx".to_string())
        );
        assert_eq!(
            config.probe_chip_for(None),
            Some("STM32F401RETx".to_string()),
            "未给 board 时回落默认板卡"
        );
    }

    #[test]
    fn devices_are_scoped_to_their_board_and_carry_that_board_bbox_baud() {
        let (_g, _ad) = guard();
        write_config(&json!([
            { "board": "nucleo-f401re", "devices": { "pico0": "/dev/ttyACM0" } },
            { "board": "nucleo-f411re", "baud": 9600, "devices": { "pico1": "/dev/ttyACM1" } }
        ]));
        let config = load_config();
        let (alias, _, baud) = config.device_target(&json!({ "device": "pico1" })).unwrap();
        assert_eq!(alias, "pico1");
        assert_eq!(baud, 9600, "波特率取这块板自己的配置");
        assert!(
            config.device_target(&json!({})).is_err(),
            "多设备时要求显式选"
        );
    }

    #[test]
    fn a_device_alias_on_two_boards_is_an_error() {
        let (_g, _ad) = guard();
        write_config(&json!([
            { "board": "nucleo-f401re", "devices": { "pico0": "/dev/ttyACM0" } },
            { "board": "nucleo-f411re", "devices": { "pico0": "/dev/ttyACM1" } }
        ]));
        let config = load_config();
        let err = config
            .device_target(&json!({ "device": "pico0" }))
            .unwrap_err();
        assert!(err.contains("more than one board"), "{err}");
    }

    #[test]
    fn memory_read_picks_the_chip_of_the_named_board() {
        let (_g, _ad) = guard();
        write_config(&json!([
            { "board": "nucleo-f401re" },
            { "board": "my-board", "chip": "NRF52840_xxAA" }
        ]));
        let config = load_config();
        assert_eq!(
            read_chip(&json!({ "board": "my-board" }), &config),
            Some("NRF52840_xxAA".to_string())
        );
        assert_eq!(
            read_chip(&json!({}), &config),
            Some("STM32F401RETx".to_string()),
            "未给 board 时取默认板卡"
        );
    }

    #[test]
    fn duplicate_board_entries_keep_the_first() {
        let (_g, _ad) = guard();
        write_config(&json!([
            { "board": "my-board", "chip": "FIRST_TARGET" },
            { "board": "my-board", "chip": "SECOND_TARGET", "devices": { "pico0": "/dev/ttyACM0" } }
        ]));
        let config = load_config();
        assert_eq!(config.boards.len(), 1);
        assert_eq!(
            config.probe_chip_for(Some("my-board")),
            Some("FIRST_TARGET".to_string())
        );
        assert!(
            config.all_devices().is_empty(),
            "重复项的 devices 不得掺进来"
        );
    }

    #[test]
    fn ping_uses_the_baud_of_the_board_that_owns_the_port() {
        let (_g, _ad) = guard();
        write_config(&json!([
            { "board": "nucleo-f401re", "baud": 115200 },
            { "board": "nucleo-f411re", "baud": 9600, "devices": { "pico1": "/dev/ttyACM1" } }
        ]));
        let config = load_config();
        assert_eq!(config.baud_for_port("/dev/ttyACM1"), 9600);
        assert_eq!(
            config.baud_for_port("/dev/ttyACM9"),
            115200,
            "未配置的端口用第一块板的"
        );
    }

    #[test]
    fn fqbn_comes_from_the_config_board_entry() {
        let (_g, _ad) = guard();
        write_config(&json!([{ "board": "my-board", "fqbn": "esp32:esp32:esp32" }]));
        let config = load_config();
        assert_eq!(
            config.fqbn_for(" My-Board "),
            Some("esp32:esp32:esp32".to_string())
        );
        assert_eq!(config.fqbn_for("nucleo-f401re"), None);
    }

    #[test]
    fn tools_are_all_deferred_and_named_consistently() {
        let names: Vec<String> = Hardware.tools().into_iter().map(|t| t.name).collect();
        assert_eq!(
            names,
            vec![
                "hardware_search",
                "hardware_board_info",
                "hardware_update_board",
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

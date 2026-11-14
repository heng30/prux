//! `hardware` 扩展的配置：`agent_dir()/extensions/hardware/config.json`。
//!
//! 一个数组，每块板子一项（`board` / `chip` / `fqbn` / `devices` / `baud`）；
//! 数组第一项是默认板卡。读不到 / 解析不出时返回空配置，由工具层给提示。

use super::{EXT, boards, bridge, serial};
use crate::{
    core::{config_cache::ConfigCache, settings_manager::agent_dir},
    extensions::util::opt_string,
    utils::paths::{read_json_file, write_json_file},
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    sync::RwLock,
};

/// 配置文件名（位于 `agent_dir()/extensions/hardware/` 下）。
pub(super) const CONFIG_FILE: &str = "config.json";

/// 配置示例文件名：启用时缺失则写入，供用户复制成 `config.json`。
pub(super) const CONFIG_TEMPLATE_FILE: &str = "config.template";

/// 写进 `config.template` 的示例：一个数组，每块板子一项。
pub(super) const CONFIG_TEMPLATE: &str = r#"[
  {
    "board": "nucleo-f401re",
    "chip": "STM32F401RETx",
    "devices": { "pico0": "/dev/ttyACM0" },
    "baud": 115200
  },
  {
    "board": "nucleo-f411re",
    "chip": "STM32F411RETx",
    "devices": { "pico1": "/dev/ttyACM1", "unoq": "bridge" },
    "baud": 115200
  }
]
"#;

/// 配置项里的键：板卡名。
pub(super) const CONFIG_BOARD: &str = "board";

/// 配置项里的键：probe-rs 的 target 名。
pub(super) const CONFIG_CHIP: &str = "chip";

/// 配置项里的键：arduino-cli 的 FQBN。
pub(super) const CONFIG_FQBN: &str = "fqbn";

/// 配置文件里的键：设备别名 → 串口路径。
pub(super) const CONFIG_DEVICES: &str = "devices";

/// 配置文件里的键：串口波特率。
pub(super) const CONFIG_BAUD: &str = "baud";

/// 配置文件读写锁（`/hardware` 与工具读取串行化）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// 配置文件的读缓存（锁序恒为「RwLock → 缓存内层 Mutex」）。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 一台设备怎么连。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum DeviceTarget {
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
pub(super) fn config_path() -> PathBuf {
    agent_dir().join("extensions").join(EXT).join(CONFIG_FILE)
}

/// 一块板子的配置（`config.json` 数组里的一项）。
#[derive(Debug, Clone)]
pub(super) struct BoardConfig {
    /// 板卡名（对应 [`boards::BOARDS`]，已转小写；内置表没有的板子靠 `chip` 直连探针）。
    pub(super) board: String,
    /// probe-rs 的 target 名；`None` = 按板卡表推导。
    pub(super) chip: Option<String>,
    /// arduino-cli 的 FQBN（`arduino:avr:uno`）；`None` = 按板卡表推导。
    fqbn: Option<String>,
    /// 设备别名 → 传输方式（`pico0` → 串口，`unoq` → 本地 Bridge）。
    devices: BTreeMap<String, DeviceTarget>,
    /// 该板串口波特率。
    baud: u32,
}

/// 运行期配置。
#[derive(Debug, Clone, Default)]
pub(super) struct Config {
    /// 板子列表；数组顺序即默认板卡的优先顺序，空 = 没配置。
    pub(super) boards: Vec<BoardConfig>,
}

impl Config {
    /// 省略 `board` 参数时的默认板卡（数组第一项）。
    pub(super) fn default_board(&self) -> Option<&str> {
        self.boards.first().map(|b| b.board.as_str())
    }

    /// 探针 target 名：该板配置的 `chip` 优先，否则按板卡表推导。
    pub(super) fn probe_chip_for(&self, board: Option<&str>) -> Option<String> {
        let board = board.or_else(|| self.default_board())?;
        let key = board.trim().to_lowercase();
        if let Some(chip) = self
            .boards
            .iter()
            .find(|b| b.board == key)
            .and_then(|b| b.chip.as_ref())
        {
            return Some(chip.clone());
        }
        boards::lookup(board)
            .and_then(|spec| spec.probe_chip)
            .map(str::to_string)
    }

    /// 所有板子上的设备：`(别名, 板卡名, 传输方式, 该板波特率)`，按板序。
    pub(super) fn all_devices(&self) -> Vec<(String, String, DeviceTarget, u32)> {
        self.boards
            .iter()
            .flat_map(|b| {
                b.devices.iter().map(move |(alias, target)| {
                    (alias.clone(), b.board.clone(), target.clone(), b.baud)
                })
            })
            .collect()
    }

    /// 设备目标：`device` 别名 → `(别名, 传输方式, 波特率)`。
    ///
    /// 省略别名时要求**恰好**配置了一个设备：多设备时让模型显式选，避免「随手操作第一个」
    /// 这种在真板子上会烧错的默认。
    pub(super) fn device_target(
        &self,
        args: &Value,
    ) -> Result<(String, DeviceTarget, u32), String> {
        let devices = self.all_devices();
        if let Some(alias) = opt_string(args, "device") {
            let key = alias.to_lowercase();
            let mut matches = devices.into_iter().filter(|(name, _, _, _)| *name == key);
            let Some((alias, _, target, baud)) = matches.next() else {
                return Err(format!(
                    "unknown device `{alias}`; configured: {}",
                    self.device_summary()
                ));
            };
            // 同名别名挂在不同板子上时不能静默取第一个：那是写错板子的代价。
            if matches.next().is_some() {
                return Err(format!(
                    "device `{alias}` is configured on more than one board; rename one of them in {}",
                    config_path().display()
                ));
            }
            return Ok((alias, target, baud));
        }
        match devices.len() {
            0 => Err(format!(
                "no device configured; add \"devices\": {{\"pico0\": \"/dev/ttyACM0\"}} under a board in {}",
                config_path().display()
            )),
            1 => {
                let (alias, _, target, baud) = devices.into_iter().next().expect("len == 1");
                Ok((alias, target, baud))
            }
            _ => Err(format!(
                "multiple devices configured; pass `device` (one of: {})",
                self.device_summary()
            )),
        }
    }

    /// 串口路径与波特率：只接受串口设备（`mpremote` / `arduino-cli` / `pico_flash` 要的是路径）。
    pub(super) fn serial_target(&self, args: &Value) -> Result<(String, String, u32), String> {
        let (alias, target, baud) = self.device_target(args)?;
        match target {
            DeviceTarget::Serial(path) => Ok((alias, path, baud)),
            DeviceTarget::Bridge(addr) => Err(format!(
                "device `{alias}` is a Uno Q Bridge ({addr}), not a serial port"
            )),
        }
    }

    /// 探活所有端口时用的波特率：取第一块板的配置。
    pub(super) fn default_baud(&self) -> u32 {
        self.boards.first().map_or(serial::DEFAULT_BAUD, |b| b.baud)
    }

    /// 某条串口路径的波特率：配置里挂在这条路径上的设备优先，否则取第一块板的。
    pub(super) fn baud_for_port(&self, path: &str) -> u32 {
        self.boards
            .iter()
            .find(|b| {
                b.devices
                    .values()
                    .any(|target| matches!(target, DeviceTarget::Serial(p) if p == path))
            })
            .map_or(self.default_baud(), |b| b.baud)
    }

    /// 该板的 arduino-cli FQBN：配置优先，否则由调用方沿用内置表推导。
    pub(super) fn fqbn_for(&self, board: &str) -> Option<String> {
        let key = board.trim().to_lowercase();
        self.boards
            .iter()
            .find(|b| b.board == key)
            .and_then(|b| b.fqbn.clone())
    }

    /// `别名@板卡` 的展示串：多板场景下看得出设备属于哪块板子。
    pub(super) fn device_summary(&self) -> String {
        let items: Vec<String> = self
            .all_devices()
            .into_iter()
            .map(|(alias, board, _, _)| format!("{alias}@{board}"))
            .collect();
        if items.is_empty() {
            "(none)".to_string()
        } else {
            items.join(", ")
        }
    }
}

/// 解析一块板子的配置项；缺 `board` 或板卡名为空时整项忽略。
fn parse_board(value: &Value) -> Option<BoardConfig> {
    let board = value.get(CONFIG_BOARD)?.as_str()?.trim().to_lowercase();
    if board.is_empty() {
        return None;
    }
    let chip = value
        .get(CONFIG_CHIP)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|chip| !chip.is_empty())
        .map(str::to_string);
    let fqbn = value
        .get(CONFIG_FQBN)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|fqbn| !fqbn.is_empty())
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
    Some(BoardConfig {
        board,
        chip,
        fqbn,
        devices,
        baud,
    })
}

/// 把一块板子写进配置：同名（大小写不敏感）只更新 `chip`，其它键原样保留；没有则追加。
pub(super) fn upsert_board(board: &str, chip: &str) -> Result<PathBuf, String> {
    let path = config_path();
    let _guard = CONFIG_LOCK.write().unwrap_or_else(|e| e.into_inner());
    let mut value = CONFIG_CACHE.read(&path, || read_json_file(&path));
    if !value.is_array() {
        value = Value::Array(Vec::new());
    }
    let key = board.trim().to_lowercase();
    let items = value.as_array_mut().expect("just ensured array");
    let existing = items.iter_mut().find(|item| {
        item.get(CONFIG_BOARD)
            .and_then(Value::as_str)
            .is_some_and(|name| name.trim().to_lowercase() == key)
    });
    match existing {
        Some(item) => item[CONFIG_CHIP] = Value::String(chip.to_string()),
        None => items.push(json!({ CONFIG_BOARD: key, CONFIG_CHIP: chip })),
    }
    write_json_file(&path, &value)
        .map_err(|e| format!("could not write {}: {e}", path.display()))?;
    CONFIG_CACHE.store(&path, value);
    Ok(path)
}

/// 读配置（缺失 / 非法时返回空配置，不影响其它工具）。
pub(super) fn load_config() -> Config {
    let path = config_path();
    let _guard = CONFIG_LOCK.read().unwrap_or_else(|e| e.into_inner());
    let value = CONFIG_CACHE.read(&path, || read_json_file(&path));

    // 同名板卡只留第一项：否则 chip 取一条、devices 却全列，同一块板自相矛盾。
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let boards = value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(parse_board)
                .filter(|b| seen.insert(b.board.clone()))
                .collect()
        })
        .unwrap_or_default();
    Config { boards }
}

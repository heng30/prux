//! `hardware_project_template`：给一块芯片预定一套「能一起编译」的 Rust 依赖。
//!
//! 输出三份文件的内容：`Cargo.toml`（依赖 + release profile）、`.cargo/config.toml`
//! （target 三元组与 probe-rs runner）、`rust-toolchain.toml`。版本分两步来：
//! 内置表给**离线基线**（一组验证过能一起编译的版本，ESP32 那套来自仓库里
//! `blink-async` 参考工程），调用时再从 crates.io 稀疏索引取每个 crate 的**最新稳定版**
//! 覆盖基线；索引取不到就保留基线并在结果里点名，不让一次网络故障毁掉整个答案。
//!
//! 芯片族的划分决定依赖栈：ESP32 用 `esp-hal` + `esp-rtos`，STM32 / RP / nRF 用
//! embassy（`embassy-*` 支持这些芯片），SAM D21 与 AVR 没有 embassy HAL，退回各自
//! 的 `*-hal`（AVR 的 `arduino-hal` 只在 git 上，结果里明说）。

use crate::{
    core::{
        extensions::{ExtensionTool, ToolAnnotations, ToolExposure},
        tools::{ToolError, ToolResult},
    },
    extensions::util::{opt_string, req_str},
    utils::http::{self, ClientOptions},
};
use futures_util::future::join_all;
use serde_json::{Value, json};
use std::time::Duration;

/// 工具名。
pub(super) const TOOL_TEMPLATE: &str = "hardware_project_template";

/// 查 crates.io 的单次请求超时：版本更新只是加分项，慢就回落到内置基线。
const FETCH_TIMEOUT: Duration = Duration::from_secs(6);

/// crates.io 稀疏索引根地址。
const INDEX_ROOT: &str = "https://index.crates.io";

/// 一条推荐依赖。
struct Dep {
    /// crate 名，也是 `Cargo.toml` 里的键。
    name: &'static str,
    /// 离线基线版本：索引里查不到这个 crate 时用它。
    baseline: &'static str,
    /// 要启用的 feature；`$chip` 会替换成该芯片的 HAL feature 名。
    features: &'static [&'static str],
    /// `true` 时在 `Cargo.toml` 里注释掉（只有地址灯一类场景才需要）。
    optional: bool,
}

/// 一个 HAL 生态的依赖栈。
struct Family {
    /// 展示用的族名。
    label: &'static str,
    /// 依赖里是否已经包含 embassy 栈。
    embassy: bool,
    /// 该生态的依赖清单。
    deps: &'static [Dep],
    /// 附在结果末尾的族特有说明。
    notes: &'static [&'static str],
}

/// 一块（或一族）芯片的编译设定。
struct Chip {
    /// 规范化后的芯片名前缀（小写、无分隔符）；最长前缀命中的那条生效。
    prefixes: &'static [&'static str],
    /// 写进 HAL feature 的芯片名；`None` 表示直接用用户传入的名字。
    feature: Option<&'static str>,
    /// 编译目标三元组。
    target: &'static str,
    /// `rust-toolchain.toml` 的 channel。
    channel: &'static str,
    /// `probe-rs run --chip=` 的取值；`None` 时输出占位符让用户填。
    probe: Option<&'static str>,
    /// 所属依赖栈。
    family: &'static Family,
}

const ESP_DEPS: &[Dep] = &[
    Dep {
        name: "esp-hal",
        baseline: "1.0",
        features: &["defmt", "$chip", "unstable"],
        optional: false,
    },
    Dep {
        name: "esp-rtos",
        baseline: "0.3",
        features: &["defmt", "embassy", "$chip"],
        optional: false,
    },
    Dep {
        name: "esp-bootloader-esp-idf",
        baseline: "0.5",
        features: &["$chip"],
        optional: false,
    },
    Dep {
        name: "embassy-executor",
        baseline: "0.10",
        features: &["defmt"],
        optional: false,
    },
    Dep {
        name: "embassy-time",
        baseline: "0.5",
        features: &["defmt"],
        optional: false,
    },
    Dep {
        name: "embassy-futures",
        baseline: "0.1",
        features: &[],
        optional: false,
    },
    Dep {
        name: "critical-section",
        baseline: "1.2",
        features: &[],
        optional: false,
    },
    Dep {
        name: "static_cell",
        baseline: "2.1",
        features: &[],
        optional: false,
    },
    Dep {
        name: "defmt",
        baseline: "1.0",
        features: &[],
        optional: false,
    },
    Dep {
        name: "rtt-target",
        baseline: "0.6",
        features: &["defmt"],
        optional: false,
    },
    Dep {
        name: "panic-rtt-target",
        baseline: "0.2",
        features: &["defmt"],
        optional: false,
    },
    Dep {
        name: "esp-hal-smartled",
        baseline: "0.18",
        features: &[],
        optional: true,
    },
    Dep {
        name: "smart-leds",
        baseline: "0.4",
        features: &[],
        optional: true,
    },
];

const ESP_NOTES: &[&str] = &[
    "An `esp-hal` project must call `esp_bootloader_esp_idf::esp_app_desc!()`, otherwise probe-rs refuses the ESP-IDF image layout with `app descriptor missing`.",
    "Linker scripts go through `build.rs`: `cargo:rustc-link-arg=-Tdefmt.x` and `-Tlinkall.x` (`linkall.x` must be the last one).",
    "Xtensa chips (esp32 / esp32s2 / esp32s3) need the `esp` toolchain installed by `espup`, not upstream stable.",
];

const STM32_DEPS: &[Dep] = &[
    Dep {
        name: "embassy-executor",
        baseline: "0.10",
        features: &["defmt", "platform-cortex-m", "executor-thread"],
        optional: false,
    },
    Dep {
        name: "embassy-time",
        baseline: "0.5",
        features: &["defmt"],
        optional: false,
    },
    Dep {
        name: "embassy-stm32",
        baseline: "0.6",
        features: &[
            "defmt",
            "$chip",
            "time-driver-any",
            "exti",
            "memory-x",
            "rt",
        ],
        optional: false,
    },
    Dep {
        name: "defmt",
        baseline: "1.0",
        features: &[],
        optional: false,
    },
    Dep {
        name: "defmt-rtt",
        baseline: "1.0",
        features: &[],
        optional: false,
    },
    Dep {
        name: "panic-probe",
        baseline: "1.0",
        features: &[],
        optional: false,
    },
    Dep {
        name: "cortex-m",
        baseline: "0.7",
        features: &[],
        optional: false,
    },
    Dep {
        name: "static_cell",
        baseline: "2.1",
        features: &[],
        optional: false,
    },
];

const STM32_NOTES: &[&str] = &[
    "`chip` is the `embassy-stm32` feature name (`stm32f401re` = series + pins + flash size); the full part number (`STM32F401RET6`) is rejected by Cargo.",
    "Cortex-M0/M0+ chips need the `thumbv6m-none-eabi` target (the config above assumes M3/M4/M7).",
];

const NRF_DEPS: &[Dep] = &[
    Dep {
        name: "embassy-executor",
        baseline: "0.10",
        features: &["defmt", "platform-cortex-m", "executor-thread"],
        optional: false,
    },
    Dep {
        name: "embassy-time",
        baseline: "0.5",
        features: &["defmt"],
        optional: false,
    },
    Dep {
        name: "embassy-nrf",
        baseline: "0.11",
        features: &["defmt", "$chip", "time-driver-rtc1", "gpiote", "rt"],
        optional: false,
    },
    Dep {
        name: "defmt",
        baseline: "1.0",
        features: &[],
        optional: false,
    },
    Dep {
        name: "defmt-rtt",
        baseline: "1.0",
        features: &[],
        optional: false,
    },
    Dep {
        name: "panic-probe",
        baseline: "1.0",
        features: &[],
        optional: false,
    },
    Dep {
        name: "cortex-m",
        baseline: "0.7",
        features: &[],
        optional: false,
    },
];

const NRF_NOTES: &[&str] = &[
    "`chip` is the `embassy-nrf` feature name (`nrf52840`, `nrf52833`); nRF5340 uses `nrf5340-app-s` / `nrf5340-app-ns`.",
];

const RP_DEPS: &[Dep] = &[
    Dep {
        name: "embassy-executor",
        baseline: "0.10",
        features: &["defmt", "platform-cortex-m", "executor-thread"],
        optional: false,
    },
    Dep {
        name: "embassy-time",
        baseline: "0.5",
        features: &["defmt"],
        optional: false,
    },
    Dep {
        name: "embassy-rp",
        baseline: "0.10",
        features: &[
            "defmt",
            "$chip",
            "time-driver",
            "rt",
            "critical-section-impl",
        ],
        optional: false,
    },
    Dep {
        name: "defmt",
        baseline: "1.0",
        features: &[],
        optional: false,
    },
    Dep {
        name: "defmt-rtt",
        baseline: "1.0",
        features: &[],
        optional: false,
    },
    Dep {
        name: "panic-probe",
        baseline: "1.0",
        features: &[],
        optional: false,
    },
    Dep {
        name: "cortex-m",
        baseline: "0.7",
        features: &[],
        optional: false,
    },
];

const RP_NOTES: &[&str] = &[
    "The `embassy-rp` feature (`rp2040` / `rp235xa`) is already picked; add `boot2-*` and `imagedef-*` for the flash chip and image layout of the board.",
];

const SAMD_DEPS: &[Dep] = &[
    Dep {
        name: "atsamd21-hal",
        baseline: "0.3",
        features: &["$chip", "$chip-rt"],
        optional: false,
    },
    Dep {
        name: "cortex-m",
        baseline: "0.7",
        features: &[],
        optional: false,
    },
    Dep {
        name: "panic-halt",
        baseline: "1.0",
        features: &[],
        optional: false,
    },
];

const SAMD_NOTES: &[&str] = &[
    "embassy has no HAL for SAM D21, so this is bare-metal `atsamd21-hal` (to go async, pull in `embassy-executor` with `platform-cortex-m` yourself).",
    "`chip` is the `atsamd21-hal` feature name (`samd21g18a`); `$chip-rt` enables its runtime feature as well.",
];

const AVR_DEPS: &[Dep] = &[
    Dep {
        name: "avr-device",
        baseline: "0.9",
        features: &["$chip"],
        optional: false,
    },
    Dep {
        name: "panic-halt",
        baseline: "1.0",
        features: &[],
        optional: false,
    },
];

const AVR_NOTES: &[&str] = &[
    "embassy has no AVR HAL; the AVR HALs (`arduino-hal` / `avr-hal`) are published on git only and crates.io has just the `avr-device` register bindings, so add a git dependency for a HAL.",
    "AVR needs nightly with `build-std` and `-C target-cpu=<chip>` (`atmega328p`) in the config.",
];

const ESP: Family = Family {
    label: "ESP32 (esp-hal + esp-rtos + embassy)",
    embassy: true,
    deps: ESP_DEPS,
    notes: ESP_NOTES,
};

const STM32: Family = Family {
    label: "STM32 (embassy-stm32)",
    embassy: true,
    deps: STM32_DEPS,
    notes: STM32_NOTES,
};

const NRF: Family = Family {
    label: "nRF52 (embassy-nrf)",
    embassy: true,
    deps: NRF_DEPS,
    notes: NRF_NOTES,
};

const RP: Family = Family {
    label: "RP2040 / RP2350 (embassy-rp)",
    embassy: true,
    deps: RP_DEPS,
    notes: RP_NOTES,
};

const SAMD: Family = Family {
    label: "SAM D21 (atsamd21-hal)",
    embassy: false,
    deps: SAMD_DEPS,
    notes: SAMD_NOTES,
};

const AVR: Family = Family {
    label: "AVR 8-bit (avr-device)",
    embassy: false,
    deps: AVR_DEPS,
    notes: AVR_NOTES,
};

/// 芯片表：先按最长前缀匹配，再进 [`Chip::family`] 拿依赖栈。
const CHIPS: &[Chip] = &[
    Chip {
        prefixes: &["esp32c2"],
        feature: Some("esp32c2"),
        target: "riscv32imc-unknown-none-elf",
        channel: "stable",
        probe: Some("esp32c2"),
        family: &ESP,
    },
    Chip {
        prefixes: &["esp32c3"],
        feature: Some("esp32c3"),
        target: "riscv32imc-unknown-none-elf",
        channel: "stable",
        probe: Some("esp32c3"),
        family: &ESP,
    },
    Chip {
        prefixes: &["esp32c6"],
        feature: Some("esp32c6"),
        target: "riscv32imac-unknown-none-elf",
        channel: "stable",
        probe: Some("esp32c6"),
        family: &ESP,
    },
    Chip {
        prefixes: &["esp32h2"],
        feature: Some("esp32h2"),
        target: "riscv32imac-unknown-none-elf",
        channel: "stable",
        probe: Some("esp32h2"),
        family: &ESP,
    },
    Chip {
        prefixes: &["esp32s2"],
        feature: Some("esp32s2"),
        target: "xtensa-esp32s2-none-elf",
        channel: "esp",
        probe: Some("esp32s2"),
        family: &ESP,
    },
    Chip {
        prefixes: &["esp32s3"],
        feature: Some("esp32s3"),
        target: "xtensa-esp32s3-none-elf",
        channel: "esp",
        probe: Some("esp32s3"),
        family: &ESP,
    },
    Chip {
        prefixes: &["esp32"],
        feature: Some("esp32"),
        target: "xtensa-esp32-none-elf",
        channel: "esp",
        probe: Some("esp32"),
        family: &ESP,
    },
    Chip {
        prefixes: &["stm32"],
        feature: None,
        target: "thumbv7em-none-eabihf",
        channel: "stable",
        probe: None,
        family: &STM32,
    },
    Chip {
        prefixes: &["nrf5340"],
        feature: Some("nrf5340-app-s"),
        target: "thumbv8m.main-none-eabihf",
        channel: "stable",
        probe: None,
        family: &NRF,
    },
    Chip {
        prefixes: &["nrf52"],
        feature: None,
        target: "thumbv7em-none-eabihf",
        channel: "stable",
        probe: None,
        family: &NRF,
    },
    Chip {
        prefixes: &["rp2040"],
        feature: Some("rp2040"),
        target: "thumbv6m-none-eabi",
        channel: "stable",
        probe: None,
        family: &RP,
    },
    Chip {
        prefixes: &["rp2350"],
        feature: Some("rp235xa"),
        target: "thumbv8m.main-none-eabihf",
        channel: "stable",
        probe: None,
        family: &RP,
    },
    Chip {
        prefixes: &["samd21"],
        feature: None,
        target: "thumbv6m-none-eabi",
        channel: "stable",
        probe: None,
        family: &SAMD,
    },
    Chip {
        prefixes: &["atmega", "attiny", "avr"],
        feature: None,
        target: "avr-unknown-gnu-atmega328p",
        channel: "nightly",
        probe: None,
        family: &AVR,
    },
];

/// 规范化芯片名：小写并去掉 `-` `_` 空格，`ESP32-C6` 与 `esp32c6` 等价。
fn normalize(raw: &str) -> String {
    raw.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// 按最长前缀找芯片；都不命中返回 `None`。
fn find_chip(id: &str) -> Option<&'static Chip> {
    CHIPS
        .iter()
        .filter_map(|chip| {
            let hit = chip
                .prefixes
                .iter()
                .filter(|p| id.starts_with(*p))
                .map(|p| p.len())
                .max()?;
            Some((hit, chip))
        })
        .max_by_key(|(hit, _)| *hit)
        .map(|(_, chip)| chip)
}

/// 已知芯片族（错误提示里给模型看）。
fn known_families() -> String {
    CHIPS
        .iter()
        .map(|c| c.prefixes.join("/"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// crates.io 稀疏索引里某个 crate 的地址。
///
/// 索引的路径规则：名字 1 字符放 `1/`，2 字符放 `2/`，3 字符放 `3/<首字符>/`，
/// 其余放 `<前两字符>/<第 3-4 字符>/`。
fn index_url(name: &str) -> String {
    let bytes = name.as_bytes();
    match bytes.len() {
        1 => format!("{INDEX_ROOT}/1/{name}"),
        2 => format!("{INDEX_ROOT}/2/{name}"),
        3 => format!("{INDEX_ROOT}/3/{}/{name}", &name[..1]),
        _ => format!("{INDEX_ROOT}/{}/{}/{name}", &name[..2], &name[2..4]),
    }
}

/// 版本号 → 可比较的 `(major, minor, patch)`；预发布版（`1.0.0-rc.1`）返回 `None`。
fn version_key(vers: &str) -> Option<(u64, u64, u64)> {
    let mut parts = vers.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// 从索引的 JSON Lines 里取最高稳定版；全是预发布 / 全被 yank 时返回 `None`。
fn max_stable(body: &str) -> Option<String> {
    let mut best: Option<((u64, u64, u64), String)> = None;
    for line in body.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry
            .get("yanked")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            continue;
        }
        let Some(vers) = entry.get("vers").and_then(Value::as_str) else {
            continue;
        };
        let Some(key) = version_key(vers) else {
            continue;
        };
        if best.as_ref().is_none_or(|(seen, _)| key > *seen) {
            best = Some((key, vers.to_string()));
        }
    }
    best.map(|(_, vers)| vers)
}

/// 查一个 crate 的最新稳定版；网络或索引出错一律返回 `None`（调用方回落到基线）。
async fn latest_version(client: &reqwest::Client, name: &str) -> Option<String> {
    let body = client
        .get(index_url(name))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .text()
        .await
        .ok()?;
    max_stable(&body)
}

/// 渲染一行 `Cargo.toml` 依赖。
fn dep_line(dep: &Dep, version: &str, chip_feature: &str) -> String {
    let features = dep
        .features
        .iter()
        .map(|f| f.replace("$chip", chip_feature))
        .collect::<Vec<_>>();
    let line = if features.is_empty() {
        format!("{} = \"{version}\"", dep.name)
    } else {
        let list = features
            .iter()
            .map(|f| format!("\"{f}\""))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "{} = {{ version = \"{version}\", features = [{list}] }}",
            dep.name
        )
    };
    if dep.optional {
        format!("# {line}")
    } else {
        line
    }
}

/// `hardware_project_template` 的工具定义。
pub(super) fn tool() -> ExtensionTool {
    let mut tool = ExtensionTool::simple(
        TOOL_TEMPLATE,
        "Return the recommended Rust crate set for a chip: newest stable versions from crates.io, \
         with embassy where the chip is supported. Gives Cargo.toml, .cargo/config.toml and \
         rust-toolchain.toml contents. Use when starting a firmware project or when asked which \
         HAL / crates to use for a chip. Covers esp32c2/c3/c6/h2/s2/s3, esp32, stm32, nrf52/nrf5340, \
         rp2040, rp2350, samd21 and avr.",
        json!({
            "type": "object",
            "properties": {
                "chip": {
                    "type": "string",
                    "description": "Chip or chip family: `esp32c6`, `esp32s3`, `stm32f401re`, `nrf52840`, `rp2040`, `rp2350`, `samd21g18a`, `atmega328p`."
                },
                "name": {
                    "type": "string",
                    "description": "Package name to put in the generated Cargo.toml (default `firmware`)."
                }
            },
            "required": ["chip"]
        }),
        "Recommend the newest stable Rust crates for a chip",
    )
    .with_exposure(ToolExposure::Deferred)
    .with_annotations(ToolAnnotations {
        read_only: true,
        destructive: false,
        idempotent: true,
    });
    tool.label = Some("Hardware Project".to_string());
    tool
}

/// 渲染完整模板。`versions` 与 `family.deps` 一一对应，`None` 表示该项回落到基线。
fn render(
    chip: &Chip,
    chip_name: &str,
    feature: &str,
    name: &str,
    versions: &[Option<String>],
) -> String {
    let mut deps = String::new();
    let mut stale = Vec::new();
    for (dep, found) in chip.family.deps.iter().zip(versions) {
        let version = match found {
            Some(v) => v.as_str(),
            None => {
                stale.push(dep.name);
                dep.baseline
            }
        };
        deps.push_str(&dep_line(dep, version, feature));
        deps.push('\n');
    }

    let runner = match chip.probe {
        Some(target) => format!("probe-rs run --chip={target}"),
        None => "probe-rs run --chip=<probe-rs target>".to_string(),
    };
    // ESP 的链接脚本与 frame-pointer 是 esp-hal 的硬要求；ARM 侧用不到，别乱写。
    let targets_extra = if std::ptr::eq(chip.family, &ESP) {
        "\n[unstable]\nbuild-std = [\"core\"]\n\n[env]\nDEFMT_LOG = \"info\"\n"
    } else {
        "\n"
    };

    let embassy = if chip.family.embassy {
        "embassy included"
    } else {
        "embassy has no HAL here, bare-metal HAL instead"
    };

    let mut notes = String::new();
    for note in chip.family.notes {
        notes.push_str(&format!("- {note}\n"));
    }
    notes.push_str(
        "- Versions are the newest stable release in the crates.io index at call time; lines starting with `# ` are optional dependencies, delete them when unused.\n",
    );
    if !stale.is_empty() {
        notes.push_str(&format!(
            "- Not found in the crates.io index (renamed, or a local alias?), so the built-in baseline version was used: {}.\n",
            stale.join(", ")
        ));
    }

    format!(
        "Recommended crates for `{chip_name}` → {family} ({embassy})\n\
         Target triple `{target}`, toolchain `{channel}`.\n\n\
         ## Cargo.toml\n\n```toml\n[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n{deps}\n[profile.release]\ndebug = 2\nlto = \"fat\"\nopt-level = \"s\"\ncodegen-units = 1\nincremental = false\noverflow-checks = false\n```\n\n\
         ## .cargo/config.toml\n\n```toml\n[target.{target}]\nrunner = \"{runner}\"\n\n[build]\ntarget = \"{target}\"\nrustflags = [\"-C\", \"force-frame-pointers\"]\n{targets_extra}```\n\n\
         ## rust-toolchain.toml\n\n```toml\n[toolchain]\nchannel = \"{channel}\"\ncomponents = [\"rust-src\"]\ntargets = [\"{target}\"]\n```\n\n\
         ## Notes\n\n{notes}",
        family = chip.family.label,
        target = chip.target,
        channel = chip.channel,
    )
}

/// 执行 `hardware_project_template`。
pub(super) async fn run(args: &Value) -> Result<ToolResult, ToolError> {
    let raw = req_str(args, "chip")?;
    let id = normalize(&raw);
    let Some(chip) = find_chip(&id) else {
        return Ok(ToolResult::error(format!(
            "No template for `{raw}`. Known chips: {}.",
            known_families()
        )));
    };

    let feature = chip.feature.unwrap_or(&id);
    let name = opt_string(args, "name").unwrap_or_else(|| "firmware".to_string());

    let client = http::build_client_with_options(&ClientOptions {
        timeout: FETCH_TIMEOUT,
        user_agent: Some(concat!("prux/", env!("CARGO_PKG_VERSION"))),
        ..ClientOptions::default()
    })
    .ok();
    let versions: Vec<Option<String>> = match &client {
        Some(client) => {
            join_all(
                chip.family
                    .deps
                    .iter()
                    .map(|d| latest_version(client, d.name)),
            )
            .await
        }
        None => vec![None; chip.family.deps.len()],
    };

    Ok(ToolResult::text(render(
        chip, &id, feature, &name, &versions,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_paths_follow_the_sparse_index_layout() {
        assert_eq!(index_url("a"), format!("{INDEX_ROOT}/1/a"));
        assert_eq!(index_url("ab"), format!("{INDEX_ROOT}/2/ab"));
        assert_eq!(index_url("abc"), format!("{INDEX_ROOT}/3/a/abc"));
        assert_eq!(index_url("defmt"), format!("{INDEX_ROOT}/de/fm/defmt"));
        assert_eq!(index_url("esp-hal"), format!("{INDEX_ROOT}/es/p-/esp-hal"));
    }

    #[test]
    fn max_stable_skips_prereleases_and_yanked_builds() {
        let body = concat!(
            r#"{"vers":"0.9.0","yanked":false}"#,
            "\n",
            r#"{"vers":"1.0.0","yanked":false}"#,
            "\n",
            r#"{"vers":"1.2.0","yanked":true}"#,
            "\n",
            r#"{"vers":"2.0.0-rc.1","yanked":false}"#,
            "\n",
            r#"{"vers":"1.0.10","yanked":false}"#,
            "\n",
        );
        assert_eq!(max_stable(body).as_deref(), Some("1.0.10"));
        assert_eq!(max_stable(""), None);
    }

    #[test]
    fn chips_resolve_to_their_family_and_target() {
        let esp = find_chip(&normalize("ESP32-C6")).expect("esp32c6");
        assert_eq!(esp.target, "riscv32imac-unknown-none-elf");
        assert!(esp.family.embassy);
        assert_eq!(esp.feature, Some("esp32c6"));

        let stm = find_chip(&normalize("STM32F401RE")).expect("stm32");
        assert_eq!(stm.target, "thumbv7em-none-eabihf");
        assert_eq!(
            stm.feature, None,
            "STM32 uses the feature name given by the user"
        );

        assert!(find_chip(&normalize("esp32s3")).is_some());
        assert_eq!(find_chip(&normalize("esp32s3")).unwrap().channel, "esp");
        assert!(find_chip(&normalize("rp2350")).is_some());
        assert!(find_chip(&normalize("atmega328p")).is_some());
        assert!(find_chip(&normalize("unknown-chip")).is_none());
    }

    #[test]
    fn render_substitutes_the_chip_feature_and_flags_fallbacks() {
        let chip = find_chip(&normalize("esp32c6")).unwrap();
        let versions = vec![None; chip.family.deps.len()];
        let text = render(chip, "esp32c6", "esp32c6", "blink", &versions);
        assert!(text.contains(
            "esp-hal = { version = \"1.0\", features = [\"defmt\", \"esp32c6\", \"unstable\"] }"
        ));
        assert!(text.contains("runner = \"probe-rs run --chip=esp32c6\""));
        assert!(text.contains("Not found in the crates.io index"));
        assert!(text.contains("name = \"blink\""));
    }

    #[test]
    fn render_keeps_non_esp_configs_free_of_build_std() {
        let chip = find_chip(&normalize("rp2040")).unwrap();
        let versions = vec![Some("1.0.0".to_string()); chip.family.deps.len()];
        let text = render(chip, "rp2040", "rp2040", "fw", &versions);
        assert!(!text.contains("build-std"));
        assert!(text.contains("embassy-rp = { version = \"1.0.0\""));
        assert!(text.contains("thumbv6m-none-eabi"));
    }
}

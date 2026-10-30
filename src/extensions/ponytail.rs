//! `ponytail` 扩展：懒资深开发者模式（最小完整改动）。
//!
//! **问题**：模型默认倾向于「多写」——没人要的抽象、多一层的包装、为将来准备的配置、
//! 为一行逻辑新拉的依赖。代码写得越多，之后要读、要测、要修的就越多。
//!
//! **本扩展**：把一套「能省就省、但绝不省校验/错误处理/安全/无障碍」的规则**常驻**到系统提示里，
//! 并按档位调节强度。规则本体是 [`SKILL_PONYTAIL`]（先理解问题，再爬「最小改动梯子」：
//! 需要存在吗 → 仓库里有吗 → 标准库/平台有吗 → 已装依赖有吗 → 能一行吗 → 最小实现），
//! 注入时按档位裁掉其它档位的表格行与示例行，因此只有一份真源。
//!
//! 功能面：
//! - **常驻注入**（[`ExtensionHook::ContextWithSystem`]）：每次请求前把规则追加到系统提示末尾；
//!   档位为 `off` 时什么也不追加。注入**不进会话历史**（`transform_context_with_system` 的结果
//!   原样发送、不改写 `agent.messages`），关掉档位后上下文里不留残渣。
//! - **换挡**：`/ponytail [lite|full|ultra|off|status|default <档位>]`，裸 `/ponytail` 开启或报档。
//!   本会话档位写进会话自定义条目（`customType = ponytail-mode`），随会话保存/恢复；
//!   默认档位写 `agent_dir()/extensions/ponytail.json` 的 `defaultMode`。
//! - **四个检查清单**（[`Action`]）：[`SKILL_REVIEW`] / [`SKILL_AUDIT`] / [`SKILL_DEBT`] /
//!   [`SKILL_GAIN`] 的正文随二进制内嵌，`/ponytail review|audit|debt|gain` 直接投递对应正文
//!   （不依赖技能目录、不落盘，因此档位为 `off` 时也不会有技能描述漏进系统提示）。
//! - **速查卡**：[`SKILL_HELP`] 只给用户看，`/ponytail help` 在本机渲染成一条系统消息，
//!   不进模型上下文、不花一个回合。
//!
//! **技能清单**（`assets/ponytail/skills/<目录>/SKILL.md`，均随二进制内嵌）：
//!
//! - `ponytail` → [`SKILL_PONYTAIL`]：主规则正文，常驻注入系统提示。要求先读懂问题，
//!   再爬「最小改动梯子」，末尾交代省了什么、有什么风险；档位 `lite` / `full` / `ultra` 由
//!   [`filter_body`] 裁剪表格与示例行实现。
//! - `ponytail-review` → [`SKILL_REVIEW`]：审一处改动（未提交/分支/PR/指定文件）。
//!   按「正确 → 安全 → 扛得住负载 → 有测试 → 够快 → 够精简」排序，读改动周边的代码而非只看 diff，
//!   逐条用平实语言解释，只出报告、不动代码。
//! - `ponytail-audit` → [`SKILL_AUDIT`]：审整个仓库。找 bug、安全漏洞、高负载下会崩的点、
//!   无测试的高风险代码、慢路径，以及该删/该合并/该拆分的东西，按严重程度排序输出，只出报告。
//! - `ponytail-debt` → [`SKILL_DEBT`]：把全仓每个 `shortcut:`（及更早的 `ponytail:`）注释
//!   收成一份技术债账本，防止「暂时这样」悄悄变成永久；也支持用户自定义标记词。
//! - `ponytail-gain` → [`SKILL_GAIN`]：展示 ponytail 在基准测试里量到的收益（代码量、成本、速度），
//!   纯展示，不改档位、不落盘。
//! - `ponytail-help` → [`SKILL_HELP`]：档位与命令的速查卡，`/ponytail help` 在本机渲染，
//!   不进模型上下文。
//!
//! **默认不启用**，声明 `Dev` 模式：它改写的是模型每一轮的写作风格，只能由用户显式打开。

use crate::{
    core::extensions::{CommandHost, UiNotifyLevel},
    core::{
        config_cache::ConfigCache,
        extensions::{
            Extension, ExtensionCommand, ExtensionHook, ExtensionMode, ExtensionTool,
            ExtensionUiRequest, ForkProjectInfo, SubcommandDef, request_ui,
        },
        provider::AgentMessage,
        session_manager::Session,
        settings_manager::agent_dir,
        skills::skill_body,
    },
    embedded,
    error::Result,
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_PONYTAIL, command_arg},
    modes::interactive::handlers::register_host_command,
    utils::paths::{read_json_file, write_json_file},
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex, RwLock},
};
use strum::{EnumString, IntoStaticStr};

/// 扩展名（也是 `/extension` 面板里的条目名与命令归属）。
const EXT: &str = "ponytail";

/// 主命令名（不含前导 `/`）。
const CMD: &str = "ponytail";

/// 会话自定义条目的 `customType`：本会话档位。
const CUSTOM_TYPE: &str = "ponytail-mode";

/// 配置文件相对 agent 目录的路径。
const CONFIG_REL: &str = "extensions/ponytail.json";

/// 配置文件的 JSON 键：新会话的默认档位。
const CONFIG_DEFAULT_MODE: &str = "defaultMode";

/// 注入块的首行标记：模型据此知道自己处在什么档位。
const INJECT_MARKER: &str = "PONYTAIL MODE ACTIVE";

/// 主技能正文（规则本体；注入与换挡都以它为唯一真源）。
const SKILL_PONYTAIL: &str = embedded!("ponytail/skills/ponytail/SKILL.md");

/// `/ponytail review` 投递的正文：按严重度排的改动审查清单。
const SKILL_REVIEW: &str = embedded!("ponytail/skills/ponytail-review/SKILL.md");

/// `/ponytail audit` 投递的正文：全仓同套审查清单。
const SKILL_AUDIT: &str = embedded!("ponytail/skills/ponytail-audit/SKILL.md");

/// `/ponytail debt` 投递的正文：`shortcut:` 注释的债务账本。
const SKILL_DEBT: &str = embedded!("ponytail/skills/ponytail-debt/SKILL.md");

/// `/ponytail gain` 投递的正文：benchmark 记分牌。
const SKILL_GAIN: &str = embedded!("ponytail/skills/ponytail-gain/SKILL.md");

/// `/ponytail help` 本机打印的正文：命令与档位速查（不投递给模型）。
const SKILL_HELP: &str = embedded!("ponytail/skills/ponytail-help/SKILL.md");

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static PONYTAIL_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_PONYTAIL,
    make: || -> Arc<dyn Extension> { Arc::new(Ponytail) },
};

/// 全进程共享的档位状态（无 App 访问的钩子路径也要读它）。
static STATE: Mutex<State> = Mutex::new(State { level: None });

/// ponytail.json 的读写锁（换档写盘与每轮读默认档可能并发）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// ponytail.json 的读缓存（锁序恒为「RwLock → 缓存内层 Mutex」）。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 强度档位。
///
/// `Off` 是「关掉」而不是「最弱」：注入与整句关闭都不生效，只剩 `/ponytail` 命令本身还在。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
enum Level {
    /// 关闭：不注入规则，模型按各自的默认风格写。
    Off,
    /// 最弱：按用户要求做，但在回复里用一句话点名更小的做法，让用户自己选。
    Lite,
    /// 默认档：完整规则梯子 + 每轮结尾自陈「跳过了什么 / 有什么风险」。
    Full,
    /// 最强：额外质疑需求本身，在动手前对没必要的部分先反驳。
    Ultra,
}

impl Level {
    /// 运行期可选档位，也是 `/ponytail` 候选的展示顺序：off → lite → full → ultra。
    const RUNTIME: [Level; 4] = [Level::Off, Level::Lite, Level::Full, Level::Ultra];

    /// 配置与候选列表里用的稳定标识（小写，与 [`Level::from_str`] 互逆）。
    fn label(self) -> &'static str {
        self.into()
    }
}

/// `/ponytail` 能直接投递的检查清单：一份内嵌正文一个子命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
enum Action {
    /// 改动审查清单：按严重度排的 bug、安全、负载、缺测试、可删项。
    Review,
    /// 全仓审查清单：同一套审查跑整个仓库并按严重度排序。
    Audit,
    /// `shortcut:` 注释的债务账本。
    Debt,
    /// benchmark 记分牌。
    Gain,
    /// 档位与子命令速查：唯一一个只在本机打印、不投递给模型的子命令。
    Help,
}

impl Action {
    /// 全部子命令（含只在本机打印的 [`Action::Help`]），也是子命令候选的展示顺序。
    const ALL: [Action; 5] = [
        Action::Review,
        Action::Audit,
        Action::Debt,
        Action::Gain,
        Action::Help,
    ];

    /// 子命令名，同时是投递块 `<skill name>` 的后缀（`ponytail-<label>`）。
    fn label(self) -> &'static str {
        self.into()
    }

    /// 投递给模型的技能块名，与 `assets/ponytail/skills/<名字>/` 的目录名一致。
    fn skill_name(self) -> String {
        format!("ponytail-{}", self.label())
    }

    /// 该子命令投递给模型的正文；`None` 表示只在本机打印（[`Action::Help`]）。
    fn body(self) -> Option<&'static str> {
        Some(match self {
            Action::Review => SKILL_REVIEW,
            Action::Audit => SKILL_AUDIT,
            Action::Debt => SKILL_DEBT,
            Action::Gain => SKILL_GAIN,
            Action::Help => return None,
        })
    }

    /// 子命令候选里的一句话说明。
    fn description(self) -> &'static str {
        match self {
            Action::Review => "Review a change: bugs, security, load, tests, speed, bloat",
            Action::Audit => "Audit the whole repo: bugs, security, load, tests, speed, bloat",
            Action::Debt => "Harvest shortcut: comments into a debt ledger",
            Action::Gain => "Show ponytail's measured impact scoreboard",
            Action::Help => "Print the ponytail levels and commands reference here",
        }
    }
}

/// `/ponytail` 参数的解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd<'a> {
    /// 报告本会话档位与默认档位。
    Status,
    /// 设置本会话档位。
    Set(Level),
    /// 设置新会话的默认档位。
    SetDefault(Level),
    /// 投递某份检查清单，携带子命令之后的其余参数（目标 / 标记词，保留原样大小写）。
    Skill(Action, &'a str),
    /// 在本机打印速查卡（[`Action::Help`]）：不进模型上下文，不花一个回合。
    Help,
    /// 参数无法识别（触发错误提示）。
    Invalid,
}

/// 扩展的进程内状态。
struct State {
    /// 本会话档位；`None` = 还没确定，按配置默认值现算。
    level: Option<Level>,
}

/// 取当前档位：会话内已设定则用它，否则取配置默认值。
fn level() -> Level {
    STATE
        .lock()
        .map(|st| st.level)
        .ok()
        .flatten()
        .unwrap_or_else(default_level)
}

/// 设置当前档位（只改内存；落盘由调用方决定）。
fn set_level(level: Level) {
    if let Ok(mut st) = STATE.lock() {
        st.level = Some(level);
    }
}

/// 配置文件的绝对路径：`agent_dir()/extensions/ponytail.json`。
fn config_path() -> PathBuf {
    agent_dir().join(CONFIG_REL)
}

/// 读配置里的默认档位；文件缺失、内容非法或档位无法识别时回落 [`Level::Full`]。
fn default_level() -> Level {
    load_default(&config_path())
}

/// 从指定路径读默认档位（便于测试直接给路径）；读盘走 [`CONFIG_CACHE`]。
fn load_default(path: &Path) -> Level {
    let _guard = CONFIG_LOCK.read().unwrap_or_else(|e| e.into_inner());
    let value = CONFIG_CACHE.read(path, || read_json_file(path));

    value
        .get(CONFIG_DEFAULT_MODE)
        .and_then(Value::as_str)
        .and_then(|s| Level::from_str(s.trim().to_lowercase().as_str()).ok())
        .unwrap_or(Level::Full)
}

/// 把默认档位写进指定路径的配置文件：保留文件里其它键，缺失时新建目录。
///
/// 落盘成功后回填 [`CONFIG_CACHE`]，下次读取直接命中。
fn store_default(path: &Path, level: Level) -> std::io::Result<()> {
    let _guard = CONFIG_LOCK.write().unwrap_or_else(|e| e.into_inner());

    let mut root = CONFIG_CACHE.read(path, || read_json_file(path));
    if !root.is_object() {
        root = json!({});
    }

    if let Some(map) = root.as_object_mut() {
        map.insert(
            CONFIG_DEFAULT_MODE.to_string(),
            Value::String(level.label().to_string()),
        );
    }

    write_json_file(path, &root)?;
    CONFIG_CACHE.store(path, root);
    Ok(())
}

/// 解析 `/ponytail` 的参数。
///
/// 裸命令按「开 / 报档」二选一：已开启就只报告当前档，关闭时按默认档打开，
/// 而默认档本身是 `off` 时落到 `full`——否则裸 `/ponytail` 会原地不动。
fn parse_cmd(arg: &str, current: Level, default: Level) -> Cmd<'_> {
    let arg = arg.trim();
    if arg.is_empty() {
        if current == Level::Off {
            return Cmd::Set(if default == Level::Off {
                Level::Full
            } else {
                default
            });
        }
        return Cmd::Status;
    }

    // 只有首个词参与关键字匹配，其余原样交给子命令（路径大小写不能被折掉）。
    let (primary, rest) = match arg.split_once(char::is_whitespace) {
        Some((p, rest)) => (p, rest.trim()),
        None => (arg, ""),
    };
    let keyword = primary.to_lowercase();

    if let Ok(action) = Action::from_str(&keyword) {
        return match action.body() {
            Some(_) => Cmd::Skill(action, rest),
            None => Cmd::Help,
        };
    }

    match keyword.as_str() {
        "status" => Cmd::Status,
        "default" => match Level::from_str(&rest.to_lowercase()) {
            Ok(level) => Cmd::SetDefault(level),
            Err(_) => Cmd::Invalid,
        },
        other => match Level::from_str(other) {
            Ok(level) => Cmd::Set(level),
            Err(_) => Cmd::Invalid,
        },
    }
}

/// 取表格行第二格里被 `**` 包住的标签：`| **lite** | ...` → `lite`。
///
/// 这是 [`SKILL_PONYTAIL`] 里 Levels 表的档位列形状；表格其余行（表头、分隔行、
/// 没有加粗标签的行）都返回 `None`，因而不会被裁剪。
fn table_label(line: &str) -> Option<&str> {
    let line = line.trim_start();
    if !line.starts_with('|') {
        return None;
    }
    let cell = line.split('|').nth(1)?.trim();
    let inner = cell.strip_prefix("**")?.strip_suffix("**")?;
    Some(inner.trim())
}

/// 取示例行里的档位标签：`- lite: "..."` → `lite`。
///
/// 必须有引号才认：普通规则条目（如 `- Full: ...`）看着像示例，实则要原样保留。
fn example_label(line: &str) -> Option<&str> {
    let rest = line.trim_start().strip_prefix("- ")?;
    let (label, value) = rest.split_once(':')?;
    if !value.trim_start().starts_with('"') {
        return None;
    }
    Some(label.trim())
}

/// 按档位裁剪规则正文：只保留本档位的 Levels 表行与示例行。
fn filter_body(body: &str, level: Level) -> String {
    skill_body(body)
        .lines()
        .filter(|line| {
            for label in [table_label(line), example_label(line)]
                .into_iter()
                .flatten()
            {
                if let Ok(l) = Level::from_str(&label.to_lowercase()) {
                    return l == level;
                }
            }
            true
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 构造追加到系统提示末尾的注入块（首行带档位标记）。
fn instructions(level: Level) -> String {
    format!(
        "{INJECT_MARKER} — level: {}\n\n{}",
        level.label(),
        filter_body(SKILL_PONYTAIL, level).trim()
    )
}

/// 把规则接在系统提示后面；`off` 时返回 `None`（调用方什么也不改）。
fn injected_system(system: &str, level: Level) -> Option<String> {
    if level == Level::Off {
        return None;
    }
    Some(format!("{}\n\n{}", system, instructions(level)))
}

/// 从会话自定义条目里取最近一次记录的档位。
fn latest_from_session(session: Option<&Session>) -> Option<Level> {
    for v in session?.get_entries().iter().rev() {
        if v.get("customType").and_then(Value::as_str) != Some(CUSTOM_TYPE) {
            continue;
        }
        let Some(mode) = v
            .get("data")
            .and_then(|d| d.get("mode"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        if let Ok(level) = Level::from_str(mode) {
            return Some(level);
        }
    }
    None
}

/// 把档位写进会话（异步落盘，经 TUI 的会话 worker）。
fn persist(level: Level) {
    request_ui(ExtensionUiRequest::PersistSessionEntry {
        custom_type: CUSTOM_TYPE.to_string(),
        data: json!({ "mode": level.label() }),
    });
}

/// 把技能正文包成 `<skill>` 块投递给模型。
///
/// 不走 `/skill:name` 展开：那两个路径要求技能已落盘并在启动时被发现，
/// 而本扩展的技能随二进制走，off 时也不该出现在 `<available_skills>` 里。
fn skill_block(name: &str, body: &str, args: &str) -> String {
    let block = format!("<skill name=\"{name}\">\n{}\n</skill>", skill_body(body));
    if args.is_empty() {
        block
    } else {
        format!("{block}\n\n{args}")
    }
}

/// `/ponytail` 命令族入口：换挡 / 报档 / 改默认档。
fn command_ponytail(host: &mut dyn CommandHost, cmd: &str) -> bool {
    let current = level();
    match parse_cmd(command_arg(cmd), current, default_level()) {
        Cmd::Status => host.push_message(
            &format!(
                "Ponytail: current {} · default {}",
                current.label(),
                default_level().label()
            ),
            UiNotifyLevel::Info,
        ),
        Cmd::Set(level) => {
            set_level(level);
            persist(level);
            host.push_message(
                &format!("Ponytail level set to {}.", level.label()),
                UiNotifyLevel::Info,
            );
        }
        Cmd::SetDefault(level) => {
            let path = config_path();
            match store_default(&path, level) {
                Ok(()) => host.push_message(
                    &format!(
                        "Default ponytail level set to {} ({}).",
                        level.label(),
                        path.display()
                    ),
                    UiNotifyLevel::Info,
                ),
                Err(e) => host.push_message(
                    &format!("Failed to save default level: {e}"),
                    UiNotifyLevel::Error,
                ),
            }
        }
        Cmd::Skill(action, args) => {
            // 清单走与输入框同一条投递路径：空闲开新回合，忙碌时 steer 进当前回合。
            // `help` 在 `parse_cmd` 已分流到 [`Cmd::Help`]，这里取不到 `None`。
            if let Some(body) = action.body() {
                host.send_prompt(&skill_block(&action.skill_name(), body, args));
            }
        }
        Cmd::Help => {
            // 速查卡只给人看：本地按 Markdown 渲染，不进 `messages`、不走 steer 队列。
            host.push_markdown(&skill_body(SKILL_HELP), UiNotifyLevel::Info);
        }
        Cmd::Invalid => host.push_message(
            &format!(
                "Unknown /ponytail argument. Usage: /ponytail [{}|status|{}|default <level>]",
                Level::RUNTIME
                    .iter()
                    .map(|l| l.label())
                    .collect::<Vec<_>>()
                    .join("|"),
                Action::ALL
                    .iter()
                    .map(|a| a.label())
                    .collect::<Vec<_>>()
                    .join("|")
            ),
            UiNotifyLevel::Warning,
        ),
    }
    false
}

/// 扩展实例（状态全在 [`STATE`] 里，本身无字段）。
struct Ponytail;

impl Extension for Ponytail {
    /// 返回固定扩展名 `ponytail`。
    fn name(&self) -> &str {
        EXT
    }

    /// `/extension` 二级详情展示的来源信息。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "ponytail".to_string(),
            plugin_version: "5.3.2".to_string(),
            url: String::from("https://github.com/DietrichGebert/ponytail"),
        })
    }

    /// `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Lazy senior dev mode: the smallest complete change, injected into every request"
    }

    /// 声明只在 Dev 模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 默认关闭：它改写模型每一轮的写作风格，只能由用户显式开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供任何工具：本扩展只改提示词与命令。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明唯一的 `/ponytail` 命令：换挡、报档、改默认档与五份检查清单都走它的子命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        let mut subcommands = Vec::new();
        for level in Level::RUNTIME {
            subcommands.push(SubcommandDef {
                name: level.label(),
                description: "Set the level for this session",
            });
        }

        subcommands.push(SubcommandDef {
            name: "status",
            description: "Report the session level and the default level",
        });

        for action in Action::ALL {
            subcommands.push(SubcommandDef {
                name: action.label(),
                description: action.description(),
            });
        }

        // 子命令名不允许空白：`default` 的档位只写在说明里，Tab 补全后由用户敲档位
        subcommands.push(SubcommandDef {
            name: "default",
            description: "Set the default level for future sessions (off|lite|full|ultra)",
        });

        vec![ExtensionCommand {
            name: CMD.to_string(),
            description:
                "Set the ponytail level (off|lite|full|ultra), report it, set the default, run a checklist (review|audit|debt|gain), or print the quick reference (help)"
                    .to_string(),
            busy_safe: true,
            subcommands,
        }]
    }

    /// 注册命令的 TUI 执行入口。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_ponytail);
    }

    /// 声明常驻注入钩子。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::ContextWithSystem]
    }

    /// 每次请求前把规则追加到系统提示末尾（`off` 时什么也不加）。
    ///
    /// 只改本次请求的副本：`transform_context_with_system` 的结果原样发送，
    /// 不会进会话历史，也不会被后续压缩当成真实对话。
    fn transform_context_with_system(&self, messages: &mut Vec<AgentMessage>) -> Result<()> {
        let level = level();
        let Some(system) = messages.first().filter(|m| m.role == "system") else {
            return Ok(());
        };

        let Some(text) = injected_system(&system.text(), level) else {
            return Ok(());
        };

        let mut replaced = AgentMessage::user_text(&text);
        replaced.role = "system".to_string();
        messages[0] = replaced;
        Ok(())
    }

    /// 会话启动：优先取会话条目里的档位，没有就用配置默认值。
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], session: Option<&Session>) {
        let restored = latest_from_session(session).unwrap_or_else(default_level);
        set_level(restored);
    }

    /// 会话切换：从目标会话文件重放档位（拿不到会话时回落到配置默认值）。
    fn on_session_switched(&self, session_path: Option<&str>, _messages: &[AgentMessage]) {
        let restored = session_path
            .filter(|p| !p.is_empty())
            .and_then(|p| Session::open(p).ok())
            .and_then(|s| latest_from_session(Some(&s)))
            .unwrap_or_else(default_level);
        set_level(restored);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::app::App;

    #[test]
    fn level_roundtrips_through_strum() {
        for level in Level::RUNTIME {
            assert_eq!(Level::from_str(level.label()).unwrap(), level);
        }
    }

    #[test]
    fn bare_command_reports_when_on_and_enables_when_off() {
        assert_eq!(parse_cmd("", Level::Full, Level::Full), Cmd::Status);
        assert_eq!(parse_cmd("", Level::Lite, Level::Full), Cmd::Status);
        assert_eq!(
            parse_cmd("", Level::Off, Level::Full),
            Cmd::Set(Level::Full)
        );
        // 默认档是 off 时，裸命令落到 full，而不是原地关闭
        assert_eq!(parse_cmd("", Level::Off, Level::Off), Cmd::Set(Level::Full));
    }

    #[test]
    fn explicit_levels_and_default_are_parsed() {
        assert_eq!(
            parse_cmd("ultra", Level::Full, Level::Full),
            Cmd::Set(Level::Ultra)
        );
        assert_eq!(
            parse_cmd("  LITE ", Level::Full, Level::Full),
            Cmd::Set(Level::Lite)
        );
        assert_eq!(
            parse_cmd("off", Level::Full, Level::Full),
            Cmd::Set(Level::Off)
        );
        assert_eq!(parse_cmd("status", Level::Full, Level::Full), Cmd::Status);
        assert_eq!(
            parse_cmd("default lite", Level::Full, Level::Full),
            Cmd::SetDefault(Level::Lite)
        );
        assert_eq!(parse_cmd("default", Level::Full, Level::Full), Cmd::Invalid);
        assert_eq!(parse_cmd("turbo", Level::Full, Level::Full), Cmd::Invalid);
    }

    #[test]
    fn body_keeps_only_the_current_level_rows() {
        let body = filter_body(SKILL_PONYTAIL, Level::Full);
        assert!(body.contains("| **full** |"), "full 档行应保留: {body}");
        assert!(!body.contains("| **lite** |"), "其它档行应裁掉");
        assert!(!body.contains("| **ultra** |"), "其它档行应裁掉");
        assert!(body.contains("Never cut:"), "普通规则必须原样保留");
        assert!(!body.contains("name: ponytail"), "frontmatter 应去掉");
    }

    #[test]
    fn instructions_start_with_the_active_marker() {
        let block = instructions(Level::Ultra);
        assert!(block.starts_with("PONYTAIL MODE ACTIVE — level: ultra"));
        assert!(block.contains("| **ultra** |"));
    }

    #[test]
    fn off_injects_nothing_and_other_levels_append_to_the_system_prompt() {
        assert_eq!(injected_system("base", Level::Off), None);
        let full = injected_system("base", Level::Full).expect("full 档应注入");
        assert!(full.starts_with("base\n\nPONYTAIL MODE ACTIVE — level: full"));
    }

    #[test]
    fn skill_block_wraps_body_and_appends_args() {
        let block = skill_block("ponytail-debt", SKILL_DEBT, "TODO");
        assert!(block.starts_with("<skill name=\"ponytail-debt\">"));
        assert!(block.contains("</skill>"));
        assert!(block.ends_with("TODO"));
        assert!(!block.contains("name: ponytail-debt"), "frontmatter 应去掉");
    }

    #[test]
    fn default_mode_roundtrips_through_the_config_file() {
        let dir = std::env::temp_dir().join(format!("prux-ponytail-{}", std::process::id()));
        let path = dir.join("ponytail.json");
        let _ = std::fs::remove_file(&path);

        assert_eq!(load_default(&path), Level::Full, "缺文件回落 full");
        store_default(&path, Level::Ultra).unwrap();
        assert_eq!(load_default(&path), Level::Ultra);

        // 其它键保留
        std::fs::write(&path, r#"{"other": 1, "defaultMode": "lite"}"#).unwrap();
        store_default(&path, Level::Off).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"other\": 1"), "未知键应保留: {text}");
        assert_eq!(load_default(&path), Level::Off);

        // 非法内容不 panic，回落 full
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(load_default(&path), Level::Full);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fork_project_reports_the_upstream_source() {
        let info = Ponytail.fork_project().expect("ponytail 应声明 fork 来源");
        assert_eq!(info.plugin_name, "ponytail");
        assert_eq!(info.url, "https://github.com/DietrichGebert/ponytail");
    }

    #[test]
    fn declared_commands_cover_the_documented_surface() {
        let commands = Ponytail.commands();
        assert_eq!(commands.len(), 1, "检查清单也走子命令：{commands:?}");
        let main = &commands[0];
        assert_eq!(main.name, "ponytail");
        assert!(main.busy_safe, "换档只改内存，忙碌时也要能执行");

        for expected in [
            "off", "lite", "full", "ultra", "status", "review", "audit", "debt", "gain", "help",
        ] {
            assert!(
                main.subcommands.iter().any(|s| s.name == expected),
                "缺子命令 {expected}"
            );
        }

        // `default` 只有裸写法：子命令名不允许空白，档位写在它的说明里
        assert!(main.subcommands.iter().any(|s| s.name == "default"));
        assert!(
            main.subcommands
                .iter()
                .all(|s| !s.name.chars().any(char::is_whitespace)),
            "子命令名不得含空白：{:?}",
            main.subcommands.iter().map(|s| s.name).collect::<Vec<_>>()
        );
    }

    /// 声明的子命令要能穿过注册表的合法性过滤：一旦名字被判定非法，
    /// 候选会从补全列表里消失，同时刷出「invalid name」告警。
    #[test]
    fn declared_subcommands_survive_the_registry() {
        use crate::core::extensions::{
            register_extension, registered_commands, registration_issues, set_extension_enabled,
            set_extension_mode, unregister_extension,
        };

        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        set_extension_mode(crate::core::extensions::ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        // 本测试自带干净起点：同进程其它测试可能已注册过同名扩展
        unregister_extension(EXT);
        register_extension(Ponytail);
        set_extension_enabled(EXT, true);

        let issues = registration_issues().join("\n");
        assert!(
            !issues.contains("ponytail"),
            "子命令声明不应被告警：{issues}"
        );

        let declared = registered_commands()
            .into_iter()
            .find(|c| c.name == CMD)
            .expect("命令应出现在候选列表");
        let expected: Vec<&str> = Ponytail.commands()[0]
            .subcommands
            .iter()
            .map(|s| s.name)
            .collect();
        assert_eq!(
            declared
                .subcommands
                .iter()
                .map(|s| s.name)
                .collect::<Vec<_>>(),
            expected,
            "声明过的子命令不得被注册表过滤掉"
        );
        assert!(declared.subcommands.iter().any(|s| s.name == "default"));

        assert!(unregister_extension(EXT));
    }

    #[test]
    fn subcommands_are_parsed_as_checklists() {
        assert_eq!(
            parse_cmd("review", Level::Full, Level::Full),
            Cmd::Skill(Action::Review, "")
        );
        assert_eq!(
            parse_cmd("Audit the whole repo", Level::Full, Level::Full),
            Cmd::Skill(Action::Audit, "the whole repo")
        );
        // 参数按原样交给子命令，大小写不能被关键字匹配折掉
        assert_eq!(
            parse_cmd("debt TODO", Level::Full, Level::Full),
            Cmd::Skill(Action::Debt, "TODO")
        );
        assert_eq!(
            parse_cmd("review src/Foo.rs", Level::Full, Level::Full),
            Cmd::Skill(Action::Review, "src/Foo.rs")
        );
    }

    /// `help` 与四份清单必须分到两条路：只有它在本机打印。
    #[test]
    fn help_is_parsed_as_a_local_print() {
        assert_eq!(parse_cmd("help", Level::Full, Level::Full), Cmd::Help);
        assert_eq!(parse_cmd("HELP", Level::Full, Level::Full), Cmd::Help);
        assert_eq!(Action::Help.body(), None);
    }

    /// `/ponytail help` 是本机渲染的速查卡：不投递、不占上下文，忙碌态也一样。
    #[test]
    fn help_prints_locally_and_never_reaches_the_model() {
        let _guard = crate::test_support::AgentDirGuard::temp();

        for busy in [false, true] {
            let mut st = App::new();
            st.busy = busy;
            st.runtime_steer_inbox.lock().unwrap().clear();
            st.messages.clear();
            st.system_messages.clear();

            assert!(!command_ponytail(&mut st, "/ponytail help"));

            assert!(st.messages.is_empty(), "busy={busy}: help 不得进 messages");
            assert!(
                st.runtime_steer_inbox.lock().unwrap().is_empty(),
                "busy={busy}: help 不得走 steer 队列"
            );
            assert!(
                st.status_start.is_none(),
                "busy={busy}: help 不得置 status_start"
            );

            let msg = st
                .system_messages
                .last()
                .expect("help 应打印一条系统消息")
                .1
                .clone();
            assert!(msg.markdown, "速查卡按 Markdown 渲染（含表格）");
            let text = crate::modes::interactive::app::sys_msg_text(&msg);
            assert!(
                text.starts_with("# Ponytail Help"),
                "frontmatter 应已剥掉：{text}"
            );
            assert!(
                text.contains("| `/ponytail status` |"),
                "正文应完整：{text}"
            );
        }
    }

    /// 忙碌时投递清单改走 steer 注入箱：忙碌态的 `status_start` 不会被消费，写了就等于丢消息。
    #[test]
    fn checklist_steers_when_busy() {
        let _guard = crate::test_support::AgentDirGuard::temp();
        let mut st = App::new();
        st.busy = true;
        st.runtime_steer_inbox.lock().unwrap().clear();
        st.messages.clear();

        assert!(!command_ponytail(&mut st, "/ponytail review src/Foo.rs"));

        {
            let inbox = st.runtime_steer_inbox.lock().unwrap();
            assert_eq!(inbox.len(), 1, "忙碌态应把清单作为 steer 注入");
            assert!(inbox[0].text().contains("<skill name=\"ponytail-review\">"));
            assert!(inbox[0].text().ends_with("src/Foo.rs"));
        }
        assert!(st.messages.is_empty(), "忙碌态不得把消息塞进 messages");
        assert!(st.status_start.is_none(), "忙碌态不得置 status_start");
    }
}

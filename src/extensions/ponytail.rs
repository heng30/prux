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
//! - **整句关闭**（[`ExtensionHook::UserPrompt`]）：输入整句 "stop ponytail" / "normal mode" 即关闭。
//!   必须整句匹配——子串匹配会让「add a normal mode toggle」这种正常请求顺手关掉模式。
//! - **五个附属命令**：[`SKILL_REVIEW`] / [`SKILL_AUDIT`] / [`SKILL_DEBT`] / [`SKILL_GAIN`] /
//!   [`SKILL_HELP`] 的正文随二进制内嵌，`/ponytail-review` 等命令直接投递对应正文
//!   （不依赖技能目录、不落盘，因此档位为 `off` 时也不会有技能描述漏进系统提示）。
//!
//! **默认不启用**，声明 `Dev` 模式：它改写的是模型每一轮的写作风格，只能由用户显式打开。

use crate::{
    core::{
        extensions::{
            Extension, ExtensionCommand, ExtensionHook, ExtensionMode, ExtensionUiRequest,
            SubcommandDef, UiNotifyLevel, UserPromptAction, request_ui,
        },
        provider::AgentMessage,
        session_manager::Session,
        settings_manager::agent_dir,
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_PONYTAIL, command_arg},
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::Mutex,
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

/// 主技能正文（规则本体；注入与换挡都以它为唯一真源）。
const SKILL_PONYTAIL: &str = include_str!("../../assets/ponytail/skills/ponytail/SKILL.md");

/// `/ponytail-review` 投递的正文：按严重度排的改动审查清单。
const SKILL_REVIEW: &str = include_str!("../../assets/ponytail/skills/ponytail-review/SKILL.md");

/// `/ponytail-audit` 投递的正文：全仓同套审查清单。
const SKILL_AUDIT: &str = include_str!("../../assets/ponytail/skills/ponytail-audit/SKILL.md");

/// `/ponytail-debt` 投递的正文：`shortcut:` 注释的债务账本。
const SKILL_DEBT: &str = include_str!("../../assets/ponytail/skills/ponytail-debt/SKILL.md");

/// `/ponytail-gain` 投递的正文：benchmark 记分牌。
const SKILL_GAIN: &str = include_str!("../../assets/ponytail/skills/ponytail-gain/SKILL.md");

/// `/ponytail-help` 投递的正文：命令与档位速查。
const SKILL_HELP: &str = include_str!("../../assets/ponytail/skills/ponytail-help/SKILL.md");

/// 注入块的首行标记：模型据此知道自己处在什么档位。
const INJECT_MARKER: &str = "PONYTAIL MODE ACTIVE";

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static PONYTAIL_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_PONYTAIL,
    make: || -> std::sync::Arc<dyn Extension> { std::sync::Arc::new(Ponytail) },
};

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

    /// `/ponytail default <档位>` 候选里的完整写法。
    ///
    /// 输入框一提交就执行，所以候选不能只写 `default`——那样会立刻以缺参执行一次。
    fn default_arg(self) -> &'static str {
        match self {
            Level::Off => "default off",
            Level::Lite => "default lite",
            Level::Full => "default full",
            Level::Ultra => "default ultra",
        }
    }
}

/// `/ponytail` 参数的解析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmd {
    /// 报告本会话档位与默认档位。
    Status,
    /// 设置本会话档位。
    Set(Level),
    /// 设置新会话的默认档位。
    SetDefault(Level),
    /// 参数无法识别（触发错误提示）。
    Invalid,
}

/// 扩展的进程内状态。
struct State {
    /// 本会话档位；`None` = 还没确定，按配置默认值现算。
    level: Option<Level>,
}

/// 全进程共享的档位状态（无 App 访问的钩子路径也要读它）。
static STATE: Mutex<State> = Mutex::new(State { level: None });

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

/// 从指定路径读默认档位（便于测试直接给路径）。
fn load_default(path: &Path) -> Level {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Level::Full;
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Level::Full;
    };
    value
        .get(CONFIG_DEFAULT_MODE)
        .and_then(Value::as_str)
        .and_then(|s| Level::from_str(s.trim().to_lowercase().as_str()).ok())
        .unwrap_or(Level::Full)
}

/// 把默认档位写进指定路径的配置文件：保留文件里其它键，缺失时新建目录。
fn store_default(path: &Path, level: Level) -> std::io::Result<()> {
    let mut root = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}));
    if let Some(map) = root.as_object_mut() {
        map.insert(
            CONFIG_DEFAULT_MODE.to_string(),
            Value::String(level.label().to_string()),
        );
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = format!("{}\n", serde_json::to_string_pretty(&root)?);
    std::fs::write(path, text)
}

/// 解析 `/ponytail` 的参数。
///
/// 裸命令按「开 / 报档」二选一：已开启就只报告当前档，关闭时按默认档打开，
/// 而默认档本身是 `off` 时落到 `full`——否则裸 `/ponytail` 会原地不动。
fn parse_cmd(arg: &str, current: Level, default: Level) -> Cmd {
    let arg = arg.trim().to_lowercase();
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

    let (primary, secondary) = match arg.split_once(char::is_whitespace) {
        Some((p, rest)) => (p, rest.trim()),
        None => (arg.as_str(), ""),
    };

    match primary {
        "status" => Cmd::Status,
        "default" => match Level::from_str(secondary) {
            Ok(level) => Cmd::SetDefault(level),
            Err(_) => Cmd::Invalid,
        },
        other => match Level::from_str(other) {
            Ok(level) => Cmd::Set(level),
            Err(_) => Cmd::Invalid,
        },
    }
}

/// 整句关闭判定：忽略大小写与结尾标点/空白，只认完整的两句话。
///
/// 子串匹配会让「add a normal mode toggle」这类正常请求顺手把模式关掉。
fn is_deactivation(text: &str) -> bool {
    let t = text
        .trim()
        .to_lowercase()
        .trim_end_matches(['.', '!', '?', ' ', '\t', '\n', '\r'])
        .to_string();
    t == "stop ponytail" || t == "normal mode"
}

/// 去掉 SKILL.md 开头的 YAML frontmatter，返回正文。
fn strip_frontmatter(body: &str) -> &str {
    let Some(rest) = body.strip_prefix("---") else {
        return body;
    };
    match rest.find("\n---") {
        Some(idx) => {
            let after = &rest[idx + "\n---".len()..];
            after.trim_start_matches(['\r', '\n'])
        }
        None => body,
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
    strip_frontmatter(body)
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
    let block = format!(
        "<skill name=\"{name}\">\n{}\n</skill>",
        strip_frontmatter(body).trim()
    );
    if args.is_empty() {
        block
    } else {
        format!("{block}\n\n{args}")
    }
}

/// 把一条消息作为下一轮输入投出（与 `/plan <text>` 同路径：写消息 + 置 `status_start`）。
fn send_prompt(st: &mut App, text: String) {
    st.messages.push(AgentMessage::user_text(&text));
    st.status_start = Some(text);
}

/// `/ponytail` 命令族入口：换挡 / 报档 / 改默认档。
fn command_ponytail(st: &mut App, cmd: &str) -> bool {
    let current = level();
    match parse_cmd(command_arg(cmd), current, default_level()) {
        Cmd::Status => st.push_msg(
            format!(
                "Ponytail: current {} · default {}",
                current.label(),
                default_level().label()
            ),
            MsgLevel::Info,
        ),
        Cmd::Set(level) => {
            set_level(level);
            persist(level);
            st.push_msg(
                format!("Ponytail level set to {}.", level.label()),
                MsgLevel::Info,
            );
        }
        Cmd::SetDefault(level) => {
            let path = config_path();
            match store_default(&path, level) {
                Ok(()) => st.push_msg(
                    format!(
                        "Default ponytail level set to {} ({}).",
                        level.label(),
                        path.display()
                    ),
                    MsgLevel::Info,
                ),
                Err(e) => st.push_msg(
                    format!("Failed to save default level: {e}"),
                    MsgLevel::Error,
                ),
            }
        }
        Cmd::Invalid => st.push_msg(
            format!(
                "Unknown /ponytail argument. Usage: /ponytail [{}|status|default <level>]",
                Level::RUNTIME
                    .iter()
                    .map(|l| l.label())
                    .collect::<Vec<_>>()
                    .join("|")
            ),
            MsgLevel::Warning,
        ),
    }
    false
}

/// `/ponytail-review [目标]`：投递改动审查清单。
fn command_review(st: &mut App, cmd: &str) -> bool {
    send_prompt(
        st,
        skill_block("ponytail-review", SKILL_REVIEW, command_arg(cmd)),
    );
    false
}

/// `/ponytail-audit [目标]`：投递全仓审查清单。
fn command_audit(st: &mut App, cmd: &str) -> bool {
    send_prompt(
        st,
        skill_block("ponytail-audit", SKILL_AUDIT, command_arg(cmd)),
    );
    false
}

/// `/ponytail-debt [标记词]`：投递 `shortcut:` 债务账本清单。
fn command_debt(st: &mut App, cmd: &str) -> bool {
    send_prompt(
        st,
        skill_block("ponytail-debt", SKILL_DEBT, command_arg(cmd)),
    );
    false
}

/// `/ponytail-gain`：投递 benchmark 记分牌。
fn command_gain(st: &mut App, cmd: &str) -> bool {
    send_prompt(
        st,
        skill_block("ponytail-gain", SKILL_GAIN, command_arg(cmd)),
    );
    false
}

/// `/ponytail-help`：投递档位与命令速查。
fn command_help(st: &mut App, cmd: &str) -> bool {
    send_prompt(
        st,
        skill_block("ponytail-help", SKILL_HELP, command_arg(cmd)),
    );
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
    fn fork_project(&self) -> Option<crate::core::extensions::ForkProjectInfo> {
        Some(crate::core::extensions::ForkProjectInfo {
            plugin_name: "ponytail".to_string(),
            plugin_version: "5.3.2".to_string(),
            url: String::from("https://github.com/DietrichGebert/ponytail"),
        })
    }

    /// `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Lazy senior dev mode: the smallest complete change, injected into every request"
    }

    /// 声明在 Dev 与 Creator 两种并排模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认关闭：它改写模型每一轮的写作风格，只能由用户显式开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供任何工具：本扩展只改提示词与命令。
    fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/ponytail` 与五个附属命令（存在性随扩展启用状态动态增删）。
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
        for level in Level::RUNTIME {
            subcommands.push(SubcommandDef {
                name: level.default_arg(),
                description: "Set the default level for future sessions",
            });
        }

        let alias = |name: &str, description: &str| ExtensionCommand {
            name: name.to_string(),
            description: description.to_string(),
            busy_safe: false,
            subcommands: Vec::new(),
        };

        vec![
            ExtensionCommand {
                name: CMD.to_string(),
                description:
                    "Set the ponytail level (off|lite|full|ultra), report it, or set the default"
                        .to_string(),
                busy_safe: true,
                subcommands,
            },
            alias(
                "ponytail-review",
                "Review a change: bugs, security, load, tests, speed, bloat",
            ),
            alias(
                "ponytail-audit",
                "Audit the whole repo: bugs, security, load, tests, speed, bloat",
            ),
            alias(
                "ponytail-debt",
                "Harvest shortcut: comments into a debt ledger",
            ),
            alias(
                "ponytail-gain",
                "Show ponytail's measured impact scoreboard",
            ),
            alias(
                "ponytail-help",
                "Quick reference for ponytail levels and commands",
            ),
        ]
    }

    /// 注册命令的 TUI 执行入口。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_ponytail);
        register_slash_command(EXT, "ponytail-review", command_review);
        register_slash_command(EXT, "ponytail-audit", command_audit);
        register_slash_command(EXT, "ponytail-debt", command_debt);
        register_slash_command(EXT, "ponytail-gain", command_gain);
        register_slash_command(EXT, "ponytail-help", command_help);
    }

    /// 声明常驻注入与整句关闭两个钩子。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::ContextWithSystem, ExtensionHook::UserPrompt]
    }

    /// 每次请求前把规则追加到系统提示末尾（`off` 时什么也不加）。
    ///
    /// 只改本次请求的副本：`transform_context_with_system` 的结果原样发送，
    /// 不会进会话历史，也不会被后续压缩当成真实对话。
    fn transform_context_with_system(
        &self,
        messages: &mut Vec<AgentMessage>,
    ) -> crate::error::Result<()> {
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

    /// 用户提交前的拦截：整句 "stop ponytail" / "normal mode" 关闭模式，且本轮不再发给模型。
    fn on_user_prompt(&self, text: &str) -> Option<UserPromptAction> {
        if level() == Level::Off || !is_deactivation(text) {
            return None;
        }
        set_level(Level::Off);
        persist(Level::Off);
        request_ui(ExtensionUiRequest::Notify {
            text: "Ponytail off. Reply /ponytail to turn it back on.".to_string(),
            level: UiNotifyLevel::Info,
        });
        Some(UserPromptAction::Handled)
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
    fn deactivation_needs_the_whole_message() {
        assert!(is_deactivation("stop ponytail"));
        assert!(is_deactivation("  Stop Ponytail. "));
        assert!(is_deactivation("normal mode!"));
        assert!(!is_deactivation("add a normal mode toggle"));
        assert!(!is_deactivation("please stop ponytail and refactor"));
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
        let names: Vec<String> = Ponytail.commands().into_iter().map(|c| c.name).collect();
        for expected in [
            "ponytail",
            "ponytail-review",
            "ponytail-audit",
            "ponytail-debt",
            "ponytail-gain",
            "ponytail-help",
        ] {
            assert!(names.iter().any(|n| n == expected), "缺命令 {expected}");
        }

        // `default` 必须是完整写法，否则会在输入框里被立刻执行成非法参数
        let main = Ponytail
            .commands()
            .into_iter()
            .find(|c| c.name == "ponytail")
            .expect("主命令");
        assert!(main.subcommands.iter().all(|s| s.name != "default"));
        assert!(main.subcommands.iter().any(|s| s.name == "default ultra"));
    }
}

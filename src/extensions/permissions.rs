//! `permissions` 扩展：工具调用的集中式权限门控。
//!
//! 策略写在 `agent_dir()/extensions/permissions/config.json`（全局）与
//! `<cwd>/.prux/extensions/permissions/config.json`（项目，**仅受信时**加载），
//! 支持四个层，按 **most-restrictive-wins** 组合：
//!
//! | 层 | 回答的问题 | 作用面 |
//! |---|---|---|
//! | `path` | 这个路径模式允许吗？ | 所有工具 + bash 的路径参数 |
//! | `external_directory` | 访问工作目录之外可以吗？ | 所有工具 + bash |
//! | 工具表面（`read` / `write` / `bash` / `mcp` / 任意工具名） | 这个工具允许吗？ | 对应工具 |
//! | `bash` 命令模式 | 这条命令允许吗？ | bash |
//!
//! 每层的值都是 `allow` / `ask` / `deny`，或 `{action, reason}` 对象；一张 pattern 表内
//! **last-match-wins**（兜底写前面、例外写后面）。
//!
//! # `ask` 在这里是什么意思
//!
//! prux 的工具前钩子是**同步**返回的，没有「弹窗阻塞等用户按键」这条路径
//! （UI 请求总线是异步排队，阻塞等它就会死锁）。因此 `ask` 按 **fail-closed**
//! 处理：本次调用被拒绝，拒绝理由里写明是哪条规则要确认、以及如何放行。放行方式有两种：
//!
//! - 会话级：`/permissions allow <surface> <pattern>`，只对本次会话有效，`deny` 不受影响；
//! - 永久：改 `config.json` 里对应规则（`/permissions init` 生成一份可编辑的缺省配置）。
//!
//! 也就是说 `ask` 永远不会静默放行，最坏情况是「多拦一次」。`yoloMode: true` 会把
//! `ask` 当 `allow`，那是显式的逃生门，缺省关闭。
//!
//! # 只看得到的东西
//!
//! - bash：命令按 `;` `&&` `||` `|` `&` 与换行切成语句逐条判定，命令替换 `$(…)`、
//!   反引号、子 shell 里的命令**也**判；字面 `cd` 会移动后续命令的工作目录，
//!   路径参数同时按原文与绝对拼写匹配；重定向目标按写方向判定。
//! - 无法静态判定的包装（`bash -c`、`eval`、`sudo`、`xargs`、`find -exec`、解释器 `-c` 等）
//!   整条调用按 `ask` 处理——宁可可问一句，也不猜它要干什么。
//! - 文件类工具读 `path` 参数；其它工具的 `path`（如 MCP 的 `arguments.path`）不解析，
//!   只按工具名表面判定。
//!
//! **默认不启用**，声明 `Dev` / `Creator` 两个模式（`Minimal` 下不可用）。

mod bash;
mod command;
mod config;
mod paths;
mod policy;
mod review;

use crate::{
    core::{
        extensions::{
            BeforeToolCallOutcome, Extension, ExtensionCommand, ExtensionHook, ExtensionMode,
            ExtensionSetting, ExtensionTool, ExtensionUiRequest, ForkProjectInfo, SelectOption,
            SubcommandDef, UiNotifyLevel, ask_pending, ask_prompt, request_ui, ui_ready,
        },
        provider::AgentMessage,
        session_manager::Session,
        tools::ToolError,
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_PERMISSIONS},
    modes::interactive::handlers::register_slash_command,
    utils::{
        glyphs::DEF_ELLIPSIS,
        paths::{absolutize, cwd, is_outside},
        truncate::truncate_chars,
    },
};
use config::Settings;
use futures_util::future::BoxFuture;
use policy::{Action, Decision, Direction, Policy};
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// 扩展注册名，也是 `/extension` 面板里的名字。
const EXT: &str = "permissions";

/// 斜杠命令名（`/permissions`）。
const CMD: &str = "permissions";

/// `/permissions` 子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "show",
        description: "Print the effective policy, its sources and session grants",
    },
    SubcommandDef {
        name: "settings",
        description: "Open this extension's settings panel (reviewLog / yoloMode)",
    },
    SubcommandDef {
        name: "allow",
        description: "Grant a pattern for this session: /permissions allow <surface> <pattern>",
    },
    SubcommandDef {
        name: "drop",
        description: "Revoke a session grant: /permissions drop <surface> <pattern>",
    },
    SubcommandDef {
        name: "allowlist",
        description: "List session grants",
    },
    SubcommandDef {
        name: "init",
        description: "Write a conservative default config (add --force to overwrite)",
    },
    SubcommandDef {
        name: "log",
        description: "Show the last decisions from the review log: /permissions log [n]",
    },
    SubcommandDef {
        name: "reset",
        description: "Clear every session grant",
    },
];

/// 确认面板的选项文本（也是回传值）：只放行这一次。
const ASK_ONCE: &str = "Approve once";
/// 确认面板的选项文本：本次会话放行同一个 `(surface, pattern)`。
const ASK_SESSION: &str = "Approve for this session";
/// 确认面板的选项文本：拒绝。
const ASK_DENY: &str = "Deny";

/// 确认面板的等待上限：超时按拒绝处理（没人看就当作不同意）。
///
/// 不设无限等待——面板可能因为终端被挂起、SSH 断开而永远没人按，
/// 那样 agent 会一直吊着且看不出原因。
const ASK_TIMEOUT: Duration = std::time::Duration::from_secs(600);

/// 面板标题与拦截消息里展示值的字符上限（过长的命令不该撑爆聊天区）。
const MAX_MESSAGE_VALUE_CHARS: usize = 300;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static PERMISSIONS_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_PERMISSIONS,
    make: || -> Arc<dyn Extension> { Arc::new(Permissions) },
};

/// 当前会话记住的 cwd（会话建立 / 切换时更新；拿不到时回落到进程 cwd）。
static SESSION_CWD: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 本次进程内的会话级放行表：`(surface, pattern)`。
///
/// 只影响 `ask` 的判定结果（把它变成放行），**不会**覆盖 `deny`——fail-closed 的底线
/// 不因为一次放行而失效。进程退出即清空，不落盘。
static GRANTS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

/// 配置告警只提示一次的开关（避免每次工具调用都刷屏）。
static WARNED: AtomicBool = AtomicBool::new(false);

/// 取会话放行表；锁被毒化时取回内部数据继续用。
fn grants() -> MutexGuard<'static, Vec<(String, String)>> {
    GRANTS.lock().unwrap_or_else(|e| e.into_inner())
}

/// 记下本次会话的 cwd（`on_session_start` 调用）。
fn remember_cwd(cwd: &str) {
    let mut slot = SESSION_CWD.lock().unwrap_or_else(|e| e.into_inner());
    *slot = Some(PathBuf::from(cwd));
}

/// 当前生效的 cwd：会话记录的优先，其次进程 cwd，都拿不到时用 `.`。
fn current_cwd() -> PathBuf {
    if let Some(cwd) = SESSION_CWD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return cwd;
    }

    cwd()
}

/// `permissions` 扩展。
pub struct Permissions;

impl Extension for Permissions {
    /// 扩展注册名（`/extension` 面板与 `/permissions` 命令共用）。
    fn name(&self) -> &str {
        EXT
    }

    /// 面板里的功能说明。
    fn description(&self) -> &str {
        "Centralized allow/ask/deny gates over tools, bash commands and file paths. Policy lives \
         in agent_dir()/extensions/permissions/config.json (plus a project config when the project \
         is trusted). 'ask' fails closed: the call is refused with the rule that wants \
         confirmation, and /permissions allow <surface> <pattern> grants it for the session"
    }

    /// 来源声明：本扩展移植自 pi-permission-system。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "pi-permission-system".to_string(),
            plugin_version: "41.0.0".to_string(),
            url: "https://github.com/gotgenes/pi-packages/tree/main/packages/pi-permission-system"
                .to_string(),
        })
    }

    /// 声明 `Dev` / `Creator` 两个并排模式：`Minimal` 下不可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认不启用：门控会改变模型能做什么，属于用户显式选择的能力。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供工具：本扩展只拦截别人的工具调用。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/permissions` 命令（show / allow / drop / allowlist / init / log / reset）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Inspect and grant permissions: /permissions show|allow|drop|allowlist|init|log|reset"
                .to_string(),
            // handler 只读写扩展自身的配置与放行表，不锁 agent。
            busy_safe: true,
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 接线 `/permissions` 执行入口。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command::handle);
    }

    /// 只订阅**异步**工具前钩子：命中 `ask` 时要弹面板等用户作答。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::BeforeToolCallAsync]
    }

    /// 启用时先加载一次配置（顺带把配置告警提示出去）；禁用时清空会话放行。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            let settings = config::load(&current_cwd());
            surface_warnings(&settings);
        } else {
            grants().clear();
            WARNED.store(false, Ordering::Relaxed);
        }
    }

    /// 记录会话 cwd（项目配置据此定位、路径判定据此归一化）。
    fn on_session_start(&self, cwd: &str, _messages: &[AgentMessage], _session: Option<&Session>) {
        remember_cwd(cwd);
    }

    /// 会话切换后项目配置可能换了，重新记 cwd 并让告警可以再提示一次。
    fn on_session_switched(&self, _session_path: Option<&str>, _messages: &[AgentMessage]) {
        WARNED.store(false, Ordering::Relaxed);
    }

    /// 该扩展的可调开关（`/extension` 面板里的设置项）。
    fn settings(&self) -> Vec<ExtensionSetting> {
        let settings = config::load(&current_cwd());
        vec![
            ExtensionSetting::cycle(
                "reviewLog",
                "permissionReviewLog",
                "Write every decision to agent_dir()/extensions/permissions/logs/permissions-review.jsonl",
                if settings.review_log { "on" } else { "off" },
                ["on", "off"],
            ),
            ExtensionSetting::cycle(
                "yoloMode",
                "yoloMode",
                "Treat 'ask' as 'allow' (explicitly gives up the confirmation semantics)",
                if settings.yolo_mode { "on" } else { "off" },
                ["off", "on"],
            ),
        ]
    }

    /// 应用设置面板的变更（立即落盘到全局配置）。
    fn apply_setting(&self, key: &str, value: &str) -> std::result::Result<(), String> {
        let (field, on) = match key {
            "reviewLog" => ("permissionReviewLog", value == "on"),
            "yoloMode" => ("yoloMode", value == "on"),
            other => return Err(format!("unknown setting: {other}")),
        };
        config::set_switch(field, on).map_err(|e| format!("write config failed: {e}"))
    }

    /// 把策略判成「整表 deny」的工具从模型可见工具里摘掉。
    ///
    /// 只有**每一条**模式都是 `deny` 才摘：`"bash": {"*": "deny", "git *": "ask"}` 仍然保留
    /// bash，否则那条 `git *` 例外永远没有机会被触发。
    fn filter_tools(&self, selected: Vec<String>) -> Vec<String> {
        let settings = config::load(&current_cwd());
        selected
            .into_iter()
            .filter(|tool| !settings.policy.surface_all_deny(tool))
            .collect()
    }

    /// 工具调用前门控：`deny` 直接拦，`ask` 按 fail-closed 拦并给出放行办法。
    fn before_tool_call_async(
        &self,
        name: String,
        args: Value,
        _assistant_message: Option<AgentMessage>,
        _context: Vec<AgentMessage>,
        _all_args: Value,
    ) -> BoxFuture<'static, std::result::Result<BeforeToolCallOutcome, ToolError>> {
        Box::pin(async move {
            let cwd = current_cwd();
            let settings = config::load(&cwd);
            surface_warnings(&settings);

            let outcome = evaluate(&settings.policy, &name, &args, &cwd);
            let (action, approved) = settle_action(&name, &settings, &outcome).await;

            let decision = Decision {
                action,
                ..outcome.decision.clone()
            };
            review::record(&settings, &name, &decision, &outcome.value, approved);

            match action {
                Action::Allow => Ok(BeforeToolCallOutcome::allow()),
                Action::Deny | Action::Ask => Ok(BeforeToolCallOutcome {
                    block: true,
                    reason: Some(denial_message(
                        &name,
                        &outcome,
                        approved,
                        settings.yolo_mode,
                    )),
                    terminate: false,
                    args: None,
                }),
            }
        })
    }
}

/// 把评估结果折算成最终动作，必要时弹面板等用户作答。
///
/// 返回 `(动作, 是否由会话放行)`。四条路径：
/// 1. 不是 `ask`（`allow` / `deny`）→ 原样返回，不问；
/// 2. `yoloMode` 开着 → `ask` 直接当 `allow`；
/// 3. 会话放行表已覆盖这个 `(surface, pattern)` → 放行；
/// 4. 其余情况弹面板。面板拿不到答复（无 TUI / 超时 / 用户按 Esc / 已有别的提问在等）
///    一律按 `deny` 处理——这就是 fail-closed。
async fn settle_action(tool: &str, settings: &Settings, outcome: &Outcome) -> (Action, bool) {
    let decision = &outcome.decision;
    if decision.action != Action::Ask {
        return (decision.action, false);
    }
    if settings.yolo_mode {
        return (Action::Allow, false);
    }
    if grant_covers(decision) {
        return (Action::Allow, true);
    }
    if !ui_ready() || ask_pending() {
        // 没有面板可弹，或者已有别的确认在等（再发一个会把它的面板顶掉）。
        return (Action::Deny, false);
    }

    let title = ask_title(tool, outcome);
    let options = vec![
        SelectOption::styled(ASK_ONCE, "success"),
        SelectOption::styled(ASK_SESSION, "accent"),
        SelectOption::styled(ASK_DENY, "error"),
    ];
    // 等待期间不持任何锁；回合被 abort 时 future 被 drop，等同于取消。
    match ask_prompt(title, options, Some(ASK_TIMEOUT))
        .await
        .as_deref()
    {
        Some(ASK_ONCE) => (Action::Allow, true),
        Some(ASK_SESSION) => {
            set_grant(&decision.surface, &decision.pattern, true);
            (Action::Allow, true)
        }
        // 用户按 Esc / 超时 / 面板被别处替换：不改策略，按拒绝处理。
        _ => (Action::Deny, false),
    }
}

/// 确认面板的一行标题：先说要执行什么，再说按哪条规则。
///
/// 面板标题只占一行，被宽度截断时先丢尾部——所以命令（用户真正要判断的东西）放前面，
/// 规则说明与嵌套语境放后面。命令里的换行压成空格，保证标题确实是单行。
fn ask_title(tool: &str, outcome: &Outcome) -> String {
    let d = &outcome.decision;
    let value = truncate_chars(
        &outcome.value,
        MAX_MESSAGE_VALUE_CHARS,
        Some(DEF_ELLIPSIS),
        true,
    )
    .replace(['\n', '\r'], " ");

    let what = if value.is_empty() {
        tool.to_string()
    } else {
        format!("{tool}: {value}")
    };

    let mut title = format!("{what} — {} `{}`", d.surface, d.pattern);
    if !outcome.note.is_empty() {
        title.push_str(&format!(" ({})", outcome.note));
    }
    title
}

/// 一次门控评估的结果（门控消息与 review log 共用）。
struct Outcome {
    /// 决定本次调用的判定（层组合后的最终动作）。
    decision: Decision,
    /// 被检查的值（bash 命令原文、路径，或工具的兜底占位）。
    value: String,
    /// 嵌套语境说明（「inside command substitution」这类），顶层为空。
    note: String,
}

/// 组合四层策略，给出一次工具调用的判定。
///
/// 顺序与严格度都按 most-restrictive-wins：任何一层 `deny` 直接定案；否则任何一层 `ask`
/// 把结果收紧成 `ask`；全都没有命中时落到 `permission["*"]` 兜底。
fn evaluate(policy: &Policy, tool: &str, args: &Value, cwd: &Path) -> Outcome {
    let mut decision: Option<Decision> = None;
    let mut value = String::new();
    let mut note = String::new();

    if is_shell_tool(tool) {
        if let Some(command) = args.get("command").and_then(Value::as_str) {
            value = command.to_string();
            let scanned = bash::scan(command);

            for cmd in scanned.all_commands() {
                // 命令文本：原文 + 绝对拼写 + 剥掉透明包装后的两种拼写。
                let mut texts = vec![cmd.text.clone()];
                if let Some(abs) = cmd.absolute_text() {
                    texts.push(abs);
                }
                if let Some(inner) = bash::unwrap_transparent(&cmd.text)
                    && inner != cmd.text
                {
                    if let Some(abs) = absolutize_text(&inner, &cmd.cwd) {
                        texts.push(abs);
                    }
                    texts.push(inner);
                }
                if let Some(hit) = policy.bash_decision(&texts) {
                    decision = Some(match decision.take() {
                        Some(prev) => prev.tighten(Some(hit)),
                        None => hit,
                    });
                    if note.is_empty() {
                        note = cmd.origin.note().to_string();
                    }
                }

                // 路径层与外部目录层：这条命令碰到的每个路径目标。
                for (path, dir) in cmd.path_targets() {
                    let forms = paths::canonical_forms(&path, &cmd.cwd);
                    decision = tighten_with(decision, policy.path_decision(&forms, dir));
                    if any_form_outside(&forms, &cmd.cwd) {
                        decision = tighten_with(decision, policy.external_decision(&forms, dir));
                    }
                }
            }

            // 看不透的包装 / 没解析完：一律按 ask（随后 fail-closed）。
            if let Some(evidence) = scanned.indirect.first() {
                decision = tighten_with(
                    decision,
                    Some(Decision {
                        action: Action::Ask,
                        surface: "bash".to_string(),
                        pattern: wrapper_pattern(evidence),
                        reason: Some(format!("cannot statically verify: {evidence}")),
                        matched_as: None,
                    }),
                );
            }
            if !scanned.complete {
                decision = tighten_with(
                    decision,
                    Some(Decision {
                        action: Action::Ask,
                        surface: "bash".to_string(),
                        pattern: "*".to_string(),
                        reason: Some("command could not be fully parsed".to_string()),
                        matched_as: None,
                    }),
                );
            }
        }
    } else {
        let dir = tool_direction(tool);
        if let Some(path) = args.get("path").and_then(Value::as_str) {
            value = path.to_string();
            let forms = paths::canonical_forms(path, cwd);
            decision = tighten_with(decision, policy.path_decision(&forms, dir));
            if any_form_outside(&forms, cwd) {
                decision = tighten_with(decision, policy.external_decision(&forms, dir));
            }
            decision = tighten_with(decision, policy.tool_decision(tool, &forms));
        } else {
            decision = tighten_with(decision, policy.tool_decision(tool, &["*".to_string()]));
        }
    }

    let decision = decision.unwrap_or_else(|| Decision {
        action: policy.universal.action,
        surface: "*".to_string(),
        pattern: "<fallback>".to_string(),
        reason: policy.universal.reason.clone(),
        matched_as: None,
    });

    Outcome {
        decision,
        value,
        note,
    }
}

/// 取更严格者（`None` 不影响已有结果）。
fn tighten_with(prev: Option<Decision>, next: Option<Decision>) -> Option<Decision> {
    match (prev, next) {
        (None, None) => None,
        (Some(p), None) => Some(p),
        (None, Some(n)) => Some(n),
        (Some(p), Some(n)) => Some(p.tighten(Some(n))),
    }
}

/// 把一段已剥掉包装的命令文本按 cwd 绝对化（只用于再匹配一次）。
fn absolutize_text(text: &str, cwd: &Path) -> Option<String> {
    let mut changed = false;
    let parts: Vec<String> = text
        .split_whitespace()
        .map(|word| {
            if word.contains('/') && !word.contains(['$', '*', '?', '`']) {
                let abs = absolutize(word, cwd).to_string_lossy().to_string();
                if abs != word {
                    changed = true;
                    return abs;
                }
            }
            word.to_string()
        })
        .collect();
    changed.then(|| parts.join(" "))
}

/// 路径是否落在工作目录之外（任一种拼写被判为外部就算外部）。
fn any_form_outside(forms: &[String], cwd: &Path) -> bool {
    forms.iter().any(|form| is_outside(Path::new(form), cwd))
}

/// 该工具是否走 bash 分析路径。
fn is_shell_tool(tool: &str) -> bool {
    matches!(tool, "bash" | "powershell")
}

/// 工具访问方向：`write` / `edit` 是写，其余按读处理。
fn tool_direction(tool: &str) -> Direction {
    match tool {
        "write" | "edit" => Direction::Write,
        _ => Direction::Read,
    }
}

/// 把包装证据（如 `bash (shell wrapper)`）转成一个可被 `/permissions allow` 使用的模式。
fn wrapper_pattern(evidence: &str) -> String {
    match evidence.split_whitespace().next() {
        Some(head) => format!("{head} *"),
        None => "*".to_string(),
    }
}

/// 会话放行是否覆盖这条判定（按 `(surface, pattern)` 精确匹配；支持 surface 为 `*`）。
fn grant_covers(decision: &Decision) -> bool {
    let table = grants();
    table.iter().any(|(surface, pattern)| {
        (surface == "*" || surface == &decision.surface) && pattern == &decision.pattern
    })
}

/// 拦截文案：说明拦了谁、按哪条规则、以及怎么放行。
fn denial_message(tool: &str, outcome: &Outcome, approved: bool, yolo: bool) -> String {
    let d = &outcome.decision;
    let mut out = format!(
        "Blocked by permissions: {tool} — {}({}) matched rule `{}`",
        d.surface, d.action, d.pattern
    );
    if !outcome.note.is_empty() {
        out.push_str(&format!(" [{}]", outcome.note));
    }
    out.push('.');
    if let Some(reason) = &d.reason {
        out.push_str(&format!(" Reason: {reason}."));
    }
    if let Some(matched) = &d.matched_as {
        out.push_str(&format!(" Matched as `{matched}`."));
    }
    if !outcome.value.is_empty() {
        out.push_str(&format!(
            " Value: `{}`.",
            truncate_chars(
                &outcome.value,
                MAX_MESSAGE_VALUE_CHARS,
                Some(DEF_ELLIPSIS),
                true
            )
        ));
    }

    if d.action == Action::Ask {
        if yolo {
            // yolo 开着还走到这里，说明拦住的是 deny（或者状态刚被改回关闭）。
            out.push_str(
                " yoloMode is on, so 'ask' would have been allowed — fix the rule to proceed.",
            );
        } else if approved {
            out.push_str(" The session grant was not enough: this call is denied by another rule.");
        } else {
            let why = if ui_ready() {
                "The approval panel was not answered (cancelled, timed out, or another prompt was \
                 already open), so 'ask' fails closed."
            } else {
                "There is no interactive prompt available in this run, so 'ask' fails closed."
            };
            out.push_str(&format!(
                " {why} Grant it for this session with `/permissions allow {surface} \"{pattern}\"`, \
                 or change the rule in {cfg}.",
                surface = d.surface,
                pattern = d.pattern,
                cfg = config::global_config_path().display()
            ));
        }
    } else {
        out.push_str(" Change or remove that rule in the config to allow it.");
    }
    out
}

/// 配置告警（无效规则值、被跳过的未受信项目配置）只在首次提示一次。
fn surface_warnings(settings: &Settings) {
    if settings.warnings.is_empty() || WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    let text = format!(
        "permissions: {} config issue(s): {}",
        settings.warnings.len(),
        settings.warnings.join("; ")
    );
    request_ui(ExtensionUiRequest::Notify {
        level: UiNotifyLevel::Warning,
        text,
    });
}

/// 命令层读取当前会话的 cwd。
pub(super) fn session_cwd() -> PathBuf {
    current_cwd()
}

/// 命令层操作会话放行表：`grant = true` 放行、`false` 撤销。返回是否发生了改动。
pub(super) fn set_grant(surface: &str, pattern: &str, grant: bool) -> bool {
    let mut table = grants();
    let existed = table.iter().any(|(s, p)| s == surface && p == pattern);
    if grant {
        if existed {
            return false;
        }
        table.push((surface.to_string(), pattern.to_string()));
        true
    } else {
        table.retain(|(s, p)| !(s == surface && p == pattern));
        existed
    }
}

/// 命令层清空会话放行表，返回清掉了多少条。
pub(super) fn clear_grants() -> usize {
    let mut table = grants();
    let n = table.len();
    table.clear();
    n
}

/// 命令层列出会话放行表。
pub(super) fn list_grants() -> Vec<(String, String)> {
    grants().clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn policy_with(config: Value) -> Policy {
        config::parse_settings_for_tests(&config)
    }

    #[test]
    fn subcommand_candidates_cover_the_handler() {
        let names: Vec<&str> = SUBCOMMANDS.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "show",
                "settings",
                "allow",
                "drop",
                "allowlist",
                "init",
                "log",
                "reset"
            ]
        );
    }

    #[test]
    fn denies_dotenv_through_file_tool() {
        let policy = policy_with(json!({
            "permission": {"*": "allow", "path": {"*": "allow", "*.env": "deny"}}
        }));
        let outcome = evaluate(
            &policy,
            "read",
            &json!({"path": ".env"}),
            Path::new("/work/p"),
        );
        assert_eq!(outcome.decision.action, Action::Deny);
        assert_eq!(outcome.decision.surface, "path");
    }

    #[test]
    fn allows_env_example() {
        let policy = policy_with(json!({
            "permission": {"*": "allow", "path": {"*": "allow", "*.env": "deny", "*.env.example": "allow"}}
        }));
        let outcome = evaluate(
            &policy,
            "read",
            &json!({"path": ".env.example"}),
            Path::new("/work/p"),
        );
        assert_eq!(outcome.decision.action, Action::Allow);
    }

    #[test]
    fn per_tool_deny_survives_path_allow() {
        let policy = policy_with(json!({
            "permission": {"*": "allow", "path": {"*": "allow"}, "read": "deny"}
        }));
        let outcome = evaluate(
            &policy,
            "read",
            &json!({"path": "src/a.rs"}),
            Path::new("/work/p"),
        );
        assert_eq!(outcome.decision.action, Action::Deny);
        assert_eq!(outcome.decision.surface, "read");
    }

    #[test]
    fn bash_chain_evaluates_every_command() {
        let policy = policy_with(json!({
            "permission": {"*": "allow", "bash": {"*": "allow", "rm -rf *": "deny"}}
        }));
        let outcome = evaluate(
            &policy,
            "bash",
            &json!({"command": "cd /tmp && rm -rf build"}),
            Path::new("/work/p"),
        );
        assert_eq!(outcome.decision.action, Action::Deny);
        assert_eq!(outcome.decision.surface, "bash");
    }

    #[test]
    fn bash_nested_substitution_is_denied() {
        let policy = policy_with(json!({
            "permission": {"*": "allow", "bash": {"*": "allow", "rm *": "deny"}}
        }));
        let outcome = evaluate(
            &policy,
            "bash",
            &json!({"command": "echo $(rm -rf x)"}),
            Path::new("/work/p"),
        );
        assert_eq!(outcome.decision.action, Action::Deny);
        assert!(outcome.note.contains("command substitution"));
    }

    #[test]
    fn indirect_wrapper_fails_closed_as_ask() {
        let policy = policy_with(json!({
            "permission": {"*": "allow", "bash": {"*": "allow"}}
        }));
        let outcome = evaluate(
            &policy,
            "bash",
            &json!({"command": "bash -c 'ls'"}),
            Path::new("/work/p"),
        );
        assert_eq!(outcome.decision.action, Action::Ask);
        assert_eq!(outcome.decision.pattern, "bash *");
    }

    #[test]
    fn unparsable_command_fails_closed() {
        let policy = policy_with(json!({
            "permission": {"*": "allow", "bash": {"*": "allow"}}
        }));
        let outcome = evaluate(
            &policy,
            "bash",
            &json!({"command": "echo 'unterminated"}),
            Path::new("/work/p"),
        );
        assert_eq!(outcome.decision.action, Action::Ask);
    }

    #[test]
    fn external_directory_asks_outside_cwd() {
        let policy = policy_with(json!({
            "permission": {"*": "allow", "external_directory": "ask"}
        }));
        let outcome = evaluate(
            &policy,
            "read",
            &json!({"path": "../outside.txt"}),
            Path::new("/work/p"),
        );
        assert_eq!(outcome.decision.action, Action::Ask);
        assert_eq!(outcome.decision.surface, "external_directory");
    }

    #[test]
    fn external_directory_allows_inside_cwd() {
        let policy = policy_with(json!({
            "permission": {"*": "allow", "external_directory": "ask"}
        }));
        let outcome = evaluate(
            &policy,
            "read",
            &json!({"path": "src/a.rs"}),
            Path::new("/work/p"),
        );
        assert_eq!(outcome.decision.action, Action::Allow);
    }

    #[test]
    fn universal_fallback_applies_when_nothing_matches() {
        let policy = policy_with(json!({"permission": {"*": "deny"}}));
        let outcome = evaluate(&policy, "some_tool", &json!({}), Path::new("/work/p"));
        assert_eq!(outcome.decision.action, Action::Deny);
        assert_eq!(outcome.decision.surface, "*");
    }

    #[test]
    fn bash_redirect_target_is_gated_by_path_layer() {
        let policy = policy_with(json!({
            "permission": {"*": "allow", "path": {"*": "allow", "*.env": "deny"}}
        }));
        let outcome = evaluate(
            &policy,
            "bash",
            &json!({"command": "echo secret > .env"}),
            Path::new("/work/p"),
        );
        assert_eq!(outcome.decision.action, Action::Deny);
        assert_eq!(outcome.decision.surface, "path");
    }

    #[test]
    fn transparent_wrapper_unwraps_before_matching() {
        let policy = policy_with(json!({
            "permission": {"*": "allow", "bash": {"*": "allow", "npm publish": "deny"}}
        }));
        let outcome = evaluate(
            &policy,
            "bash",
            &json!({"command": "timeout 30 npm publish"}),
            Path::new("/work/p"),
        );
        assert_eq!(outcome.decision.action, Action::Deny);
    }

    #[test]
    fn denial_message_offers_session_grant() {
        let policy = policy_with(json!({
            "permission": {"*": "allow", "bash": {"*": "allow", "sudo *": "ask"}}
        }));
        let outcome = evaluate(
            &policy,
            "bash",
            &json!({"command": "sudo ls"}),
            Path::new("/work/p"),
        );
        assert_eq!(outcome.decision.action, Action::Ask);
        let msg = denial_message("bash", &outcome, false, false);
        assert!(msg.contains("/permissions allow bash \"sudo *\""), "{msg}");
    }

    #[test]
    fn ask_title_leads_with_the_command_on_one_line() {
        let policy = policy_with(json!({
            "permission": {"*": "allow", "bash": {"*": "allow", "rm -rf *": "ask"}}
        }));
        let outcome = evaluate(
            &policy,
            "bash",
            &json!({"command": "rm -rf build\nrm -rf dist"}),
            Path::new("/work/p"),
        );
        let title = ask_title("bash", &outcome);
        // 命令在前（被截断时先丢规则说明），且换行已压成空格
        assert!(title.starts_with("bash: rm -rf build"), "{title}");
        assert!(!title.contains('\n'), "标题必须是单行: {title:?}");
        assert!(title.contains("rm -rf *"), "带上命中的规则: {title}");
    }
}

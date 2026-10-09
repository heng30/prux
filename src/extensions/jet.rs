//! `jet` 扩展：规划用强模型、实现用便宜模型的虚拟模型路由器。
//!
//! 注册虚拟模型 `jet/auto`，按**会话相位**把请求路由到不同物理模型：
//!
//! - **规划阶段**：复杂任务用 `--planning-complex`，其余用 `--planning`；
//!   复杂度由 `classifier` 配置的分类器模型判定（配了 `--planning-complex` 才调用）；
//! - **实现阶段**：本轮出现第一次**成功**的 `edit` / `write` 工具调用后，同一轮的下一个
//!   请求起切到 `--implementation`，此后整个会话留在那里——因此每个会话最多切换一次模型、
//!   最多吃一次 prompt-cache miss。
//!
//! 相位是**路由器状态**：宿主把返回的 `state` 落成会话分支上的 custom 条目，所以它跟着
//! 会话树走、也能扛压缩。用户选中的 thinking 级别原样透传给目标模型（宿主按目标能力钳制）。
//!
//! **配置**：`agent_dir()/extensions/jet.json`，经 `/jet set`（分类器）与 `/jet router`
//!（路由目标）写入，见 [`config`]。本扩展**没有默认值**：路由目标三项（`provider` /
//! `planning` / `implementation`）齐备且都能在模型目录里解析到时，`jet/auto` 才会注册进
//! 目录（否则连 `jet` 这个 provider 都不出现）；每次写入后立即重注册，无需重启。
//!
//! **命令**：
//! - `/jet` —— 打印配置与注册状态；
//! - `/jet set <provider> <model-id>` —— 配置分类器模型（必须是 `type: classifier` 条目）；
//! - `/jet router [--provider <p>] [--planning <id>] [--planning-complex <id>] [--implementation <id>]`
//!   `[--planning-provider <p>] [--planning-complex-provider <p>] [--implementation-provider <p>]`
//!   —— 增量覆盖路由目标（只改给到的项）：`--provider` 一次给三档设同一个 provider，
//!   `--planning` / `--planning-complex` / `--implementation` 只改对应档的模型 id，
//!   三个 `--*-provider` 只改对应档的 provider（id 与 provider 可分别改）；
//! - `/jet reset` —— 清空全部配置；若当前会话正选中 `jet/auto`，自动切回最近一次路由到的
//!   物理模型（拿不到就回落到 provider/model 解析的兜底模型）。
//!
//! **依赖**：路由目标模型的 provider 凭据、分类器模型的凭据（分类器缺失或调用失败时
//! 一律回退到 `--planning`，不报错）。分类器调用的 token 用量**不计入**会话成本
//!
//! **默认不启用**，声明 `Dev` / `Creator` 两个模式（`Minimal` 下不可用）。

mod config;

use super::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_JET, command_arg, util::tighter_u32};
use crate::{
    core::{
        extensions::{
            Extension, ExtensionCommand, ExtensionMode, ExtensionTool, ForkProjectInfo, ModelRoute,
            ModelRouteReason, ModelRouteRequest, RouteTarget, SubcommandDef,
            VirtualModelDefinition, VirtualModelRouter,
        },
        model_resolver::{self, ModelEntry},
        provider::{
            AgentMessage, ClassifierAnswer, ClassifierContext, ClassifierQuestion, ModelType,
            classify,
        },
        settings_manager, virtual_models,
    },
    error::Result,
    extensions::util::truncate_chars_with_postfix,
    modes::interactive::{
        agent_actor::AgentCommand,
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
};
use config::{ClassifierConfig, JetConfig, RouterConfig, Target};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{collections::BTreeMap, sync::Arc};
use strum_macros::{EnumString, IntoStaticStr};

/// 扩展名（注册名，也是 `/extension` 面板里的名字）
const EXT: &str = "jet";

/// 斜杠命令名（`/jet`）
const CMD: &str = "jet";

/// 虚拟模型挂载的 provider id
const VIRTUAL_PROVIDER: &str = "jet";

/// 虚拟模型 id
const VIRTUAL_ID: &str = "auto";

/// 虚拟模型的展示名（`/model` 选择器）
const VIRTUAL_NAME: &str = "Auto (Jet)";

/// 分类器问题的 id（答案按同一 id 取回）
const COMPLEXITY_ID: &str = "complexity";

/// 分类器题面里状态字段名（放进 `state.prompt`）
const PROMPT_FIELD: &str = "prompt";

/// 送去分类的题面上限（字符数，超出截断）
const PROMPT_MAX_CHARS: usize = 16_000;

/// `complex` 概率达到该阈值即判为复杂任务
const COMPLEX_THRESHOLD: f64 = 0.5;

/// 视为"实现已开始"的工具名（成功结果才作数）
const EDIT_TOOLS: &[&str] = &["edit", "write"];

/// `/jet` 子命令（顺序 = 输入框候选顺序）
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "set",
        description: "Set the classifier model: /jet set <provider> <model-id>",
    },
    SubcommandDef {
        name: "router",
        description: "Set routing targets: /jet router [--provider <p>] [--planning <id>] \
                      [--planning-complex <id>] [--implementation <id>] \
                      [--planning-provider <p>] [--planning-complex-provider <p>] \
                      [--implementation-provider <p>]",
    },
    SubcommandDef {
        name: "reset",
        description: "Clear the jet configuration (unregisters jet/auto)",
    },
];

/// `/jet set` 的用法提示（错误文案复用）
const SET_USAGE: &str =
    "Usage: /jet set <provider> <model-id> (e.g. /jet set openrouter ~typesafe/jev-latest)";

/// `/jet router` 的用法提示（错误文案复用）
const ROUTER_USAGE: &str = "Usage: /jet router [--provider <p>] [--planning <id>] \
                             [--planning-complex <id>] [--implementation <id>] \
                             [--planning-provider <p>] [--planning-complex-provider <p>] \
                             [--implementation-provider <p>]";

/// 自声明工厂：linkme 分布式切片
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static JET_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_JET,
    make: || -> Arc<dyn Extension> { Arc::new(Jet) },
};

/// 路由器的会话相位：规划中还是实现中。
///
/// 与状态里字符串（`planning` / `implementation`）的互转由 `strum` 派生。
#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
enum Phase {
    /// 规划阶段：探索、规划、做第一次编辑。
    Planning,
    /// 实现阶段：第一次成功的编辑之后，后续全部交给实现模型。
    Implementation,
}

impl Phase {
    /// 相位在状态里的字符串形式。
    fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析状态里的相位字符串；未知取值返回 None（当作没有状态重新判定）。
    fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// 路由器状态（落成会话分支上的 custom 条目，随会话树走）。
#[derive(Clone, Debug, PartialEq, Eq)]
struct JetState {
    /// 当前相位。
    phase: Phase,
    /// 当前相位使用的物理模型 id。
    model: String,
}

impl JetState {
    /// 组装一个状态。
    fn new(phase: Phase, model: &str) -> Self {
        JetState {
            phase,
            model: model.to_string(),
        }
    }

    /// 从宿主给回的状态 JSON 解析；字段缺失/类型不符/相位未知时返回 None。
    fn from_value(value: &Value) -> Option<Self> {
        let phase = Phase::parse(value.get("phase")?.as_str()?)?;
        let model = value.get("model")?.as_str()?;
        if model.trim().is_empty() {
            return None;
        }
        Some(JetState::new(phase, model))
    }

    /// 转成落盘用的 JSON
    fn to_value(&self) -> Value {
        json!({ "phase": self.phase.as_str(), "model": self.model })
    }
}

/// 一次路由决策需要的输入（从 [`ModelRouteRequest`] 里摘出决策相关字段，便于单测）。
struct RouteInput<'a> {
    /// 该请求被路由的原因。
    reason: ModelRouteReason,
    /// 请求里选中的 thinking 级别（原样透传给目标模型）。
    thinking_level: Option<&'a str>,
    /// `messages` 里最近一次成功响应命中的物理模型。
    previous: Option<&'a RouteTarget>,
    /// 会话分支上最近一次由路由器返回的状态。
    state: Option<&'a Value>,
    /// 本次请求的完整对话（含 system 消息）。
    messages: &'a [AgentMessage],
}

impl<'a> From<&'a ModelRouteRequest<'a>> for RouteInput<'a> {
    /// 摘出决策相关字段；
    fn from(request: &'a ModelRouteRequest<'a>) -> Self {
        RouteInput {
            reason: request.reason,
            thinking_level: request.thinking_level,
            previous: request.previous.as_ref(),
            state: request.state,
            messages: request.messages,
        }
    }
}

/// 分类器对当前题面的判定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Complexity {
    /// 判为复杂任务（用复杂规划模型）。
    Complex,
    /// 判为普通任务（用缺省规划模型）。
    Standard,
    /// 未判定：没有分类器、没配复杂档、调用失败或答案格式不对。
    Unknown,
}

/// `jet` 的虚拟模型路由器：按相位与复杂度挑选物理模型。
struct JetRouter {
    /// 路由目标配置（注册时的快照；配置改动会重新注册）。
    router: RouterConfig,
    /// 分类器配置；未配置时为 None。
    classifier: Option<ClassifierConfig>,
}

impl JetRouter {
    /// 单次路由决策。
    async fn decide(&self, input: RouteInput<'_>) -> Result<ModelRoute> {
        // agent 循环之外的请求（压缩摘要、扩展直调）：直接给实现档，不碰状态。
        if input.reason == ModelRouteReason::Direct {
            return Ok(self.route_to(&self.router.implementation, &input, None));
        }

        let state = input.state.and_then(JetState::from_value);

        // 规划档做出了本轮第一次成功编辑：交棒给实现档，此后不再回头。
        if state.as_ref().is_some_and(|s| s.phase == Phase::Planning)
            && edited_this_turn(input.messages)
        {
            let target = &self.router.implementation;
            let next = JetState::new(Phase::Implementation, &target.model);
            return Ok(self.route_to(target, &input, Some(next)));
        }

        // 已有状态且仍对应当前配置里的某一档：沿用（状态原样保留，不重复落条目）。
        if let Some(state) = state
            && let Some(target) = self.target_for_state(&state)
        {
            return Ok(self.route_to(target, &input, None));
        }

        let target = self.choose_planning(&input).await;
        let state = JetState::new(Phase::Planning, &target.model);
        Ok(self.route_to(target, &input, Some(state)))
    }

    /// 组装一条路由结果：provider 取自目标档，thinking 级别透传。
    fn route_to(
        &self,
        target: &Target,
        input: &RouteInput<'_>,
        state: Option<JetState>,
    ) -> ModelRoute {
        ModelRoute {
            provider: target.provider.clone(),
            model_id: target.model.clone(),
            thinking_level: input.thinking_level.map(str::to_string),
            state: state.map(|s| s.to_value()),
        }
    }

    /// 状态对应的目标档：相位与模型 id 都要与当前配置对得上，否则按新会话重判。
    ///
    /// 只看模型 id 不看 provider：配置改过 provider 之后，同一档应当继续沿用新 provider。
    fn target_for_state(&self, state: &JetState) -> Option<&Target> {
        let router = &self.router;
        match state.phase {
            Phase::Planning => [Some(&router.planning), router.complex()]
                .into_iter()
                .flatten()
                .find(|target| target.model == state.model),
            Phase::Implementation => {
                (router.implementation.model == state.model).then_some(&router.implementation)
            }
        }
    }

    /// 新会话（或状态失效）时的规划档选择。
    async fn choose_planning(&self, input: &RouteInput<'_>) -> &Target {
        let router = &self.router;

        // 上一轮已经在本配置的某一规划档上（同 provider + 同模型）：
        // 沿用，省一次分类调用与一次 prompt-cache miss。
        if let Some(previous) = input.previous
            && let Some(target) = [Some(&router.planning), router.complex()]
                .into_iter()
                .flatten()
                .find(|target| target.matches(&previous.provider, &previous.model_id))
        {
            return target;
        }

        let complexity = self.complexity(input.messages).await;
        plan_target(router, complexity)
    }

    /// 调用分类器判定题面复杂度；任何缺失/失败都返回 [`Complexity::Unknown`]。
    async fn complexity(&self, messages: &[AgentMessage]) -> Complexity {
        if self.router.complex().is_none() {
            return Complexity::Unknown;
        }
        let Some(classifier) = self.classifier.as_ref().filter(|c| c.is_complete()) else {
            return Complexity::Unknown;
        };

        match classify_complexity(classifier, messages).await {
            Some(true) => Complexity::Complex,
            Some(false) => Complexity::Standard,
            None => Complexity::Unknown,
        }
    }
}

impl VirtualModelRouter for JetRouter {
    /// 解析一次请求；见 [`JetRouter::decide`]。
    fn route<'a>(&'a self, request: ModelRouteRequest<'a>) -> BoxFuture<'a, Result<ModelRoute>> {
        Box::pin(async move {
            let input = RouteInput::from(&request);
            self.decide(input).await
        })
    }
}

/// 由复杂度判定决定规划档（纯函数，便于单测）。
fn plan_target(router: &RouterConfig, complexity: Complexity) -> &Target {
    match (complexity, router.complex()) {
        (Complexity::Complex, Some(complex)) => complex,
        _ => &router.planning,
    }
}

/// 本轮（最后一条 user 消息之后）是否出现了**成功**的 `edit` / `write` 工具结果。
fn edited_this_turn(messages: &[AgentMessage]) -> bool {
    let Some(last_user) = messages.iter().rposition(|m| m.role == "user") else {
        return false;
    };

    messages[last_user + 1..].iter().any(|m| {
        m.role == "toolResult"
            && !m.is_error
            && m.tool_name
                .as_deref()
                .is_some_and(|name| EDIT_TOOLS.contains(&name))
    })
}

/// 最后一条 user 消息的文本（拼接文本块），按 [`PROMPT_MAX_CHARS`] 截断。
fn last_user_text(messages: &[AgentMessage]) -> String {
    let text = messages
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .map(AgentMessage::text)
        .unwrap_or_default();
    truncate_chars_with_postfix(&text, PROMPT_MAX_CHARS, "")
}

/// 组装分类器请求：一道 `choice` 题判定题面的工程复杂度
fn classifier_context(prompt: &str) -> ClassifierContext {
    ClassifierContext {
        state: json!({ PROMPT_FIELD: prompt }),
        images: None,
        questions: BTreeMap::from([(
            COMPLEXITY_ID.to_string(),
            ClassifierQuestion::Choice {
                instructions:
                    "How demanding is the software engineering work requested in `prompt`?"
                        .to_string(),
                criteria: BTreeMap::from([
                    (
                        "standard".to_string(),
                        "Ordinary features, fixes, reviews, or questions".to_string(),
                    ),
                    (
                        "complex".to_string(),
                        "Subtle design, cross-cutting changes, or hard debugging".to_string(),
                    ),
                ]),
            },
        )]),
    }
}

/// 调用分类器判定复杂度：返回 `Some(true)` 表示 `complex` 概率达到阈值。
///
/// 分类器缺失凭据 / 请求失败 / 答案缺失或类型不符一律返回 None（调用方回退普通规划模型）；
/// 分类器用时**不**计入会话成本。
async fn classify_complexity(
    classifier: &config::ClassifierConfig,
    messages: &[AgentMessage],
) -> Option<bool> {
    let model = model_resolver::configured_model_of_type(
        &classifier.provider,
        &classifier.model,
        ModelType::Classifier,
        None,
        None,
    )
    .ok()?;

    let context = classifier_context(&last_user_text(messages));
    let result = classify(&model, &context).await;

    if result.stop_reason != "stop" {
        return None;
    }

    match result.answers.get(COMPLEXITY_ID)? {
        ClassifierAnswer::Choice { probabilities, .. } => {
            Some(probabilities.get("complex").copied().unwrap_or(0.0) >= COMPLEX_THRESHOLD)
        }
        _ => None,
    }
}

/// 校验一个物理模型位：provider 已知、条目存在、类型匹配，且不是虚拟条目本身。
fn check_physical(
    provider: &str,
    id: &str,
    kind: ModelType,
    label: &str,
) -> std::result::Result<ModelEntry, String> {
    let provider = provider.trim();
    if provider.is_empty() {
        return Err(format!(
            "{label} needs a provider (/jet set <provider> <model-id> first)"
        ));
    }

    let id = id.trim();
    if id.is_empty() {
        return Err(format!(
            "{label} needs a model id (/jet set <provider> <model-id> first)"
        ));
    }

    if provider == VIRTUAL_PROVIDER && id == VIRTUAL_ID {
        return Err(format!(
            "{label} must not be the virtual model {VIRTUAL_PROVIDER}/{VIRTUAL_ID}"
        ));
    }

    if virtual_models::is_active_virtual(provider, id) {
        return Err(format!(
            "{label} must be a physical model, got the virtual model {provider}/{id}"
        ));
    }

    model_resolver::find_model_of_type(provider, id, kind).map_err(|e| format!("{label}: {e}"))
}

/// 目标模型的 thinking 档位（与 [`crate::core::provider::ModelConfig::supported_thinking_levels`] 同口径）。
fn supported_levels(entry: &ModelEntry) -> Vec<String> {
    model_resolver::model_config_from_entry(entry, None, None)
        .supported_thinking_levels()
        .into_iter()
        .map(str::to_string)
        .collect()
}

/// 全部目标模型都支持的 thinking 档位（交集，保持目录顺序；空交集由定义归一化为 `off`）。
fn thinking_intersection(entries: &[&ModelEntry]) -> Vec<String> {
    let Some((first, rest)) = entries.split_first() else {
        return Vec::new();
    };

    let mut levels = supported_levels(first);
    for entry in rest {
        let supported = supported_levels(entry);
        levels.retain(|level| supported.contains(level));
    }
    levels
}

/// 由配置组装虚拟模型定义；配置不完整或目标解析不到时返回原因（此时**不注册**）。
///
/// 三档可以来自**不同 provider**：每档各自解析（自己的 provider + 模型 id）。
fn build_definition(cfg: &JetConfig) -> std::result::Result<VirtualModelDefinition, String> {
    let router = &cfg.router;
    if !router.is_complete() {
        return Err(format!(
            "routing targets are incomplete (need --planning and --implementation; {ROUTER_USAGE})"
        ));
    }

    let planning = check_physical(
        &router.planning.provider,
        &router.planning.model,
        ModelType::Chat,
        "--planning",
    )?;
    let implementation = check_physical(
        &router.implementation.provider,
        &router.implementation.model,
        ModelType::Chat,
        "--implementation",
    )?;
    let complex = match router.complex() {
        Some(target) => Some(check_physical(
            &target.provider,
            &target.model,
            ModelType::Chat,
            "--planning-complex",
        )?),
        None => None,
    };

    // 首次响应前的上下文窗口 / 输出上限取规划两档中较小者：规划阶段不会超窗。
    let context_window = match &complex {
        Some(complex) => tighter_u32(planning.context_window, complex.context_window),
        None => planning.context_window,
    };
    let max_tokens = match &complex {
        Some(complex) => tighter_u32(planning.max_tokens, complex.max_tokens),
        None => planning.max_tokens,
    };

    let mut targets = vec![&planning, &implementation];
    if let Some(complex) = &complex {
        targets.push(complex);
    }
    let levels = thinking_intersection(&targets);

    let classifier = cfg.classifier.is_complete().then(|| cfg.classifier.clone());
    let router = Arc::new(JetRouter {
        router: router.clone(),
        classifier,
    });

    Ok(
        VirtualModelDefinition::new(VIRTUAL_PROVIDER, VIRTUAL_ID, VIRTUAL_NAME, router)
            .with_thinking_levels(levels)
            .with_limits(context_window, max_tokens),
    )
}

/// 按当前配置同步注册：可注册则注册（覆盖旧定义），否则注销 `jet/auto`。
fn sync_registration() {
    match build_definition(&config::load()) {
        Ok(definition) => virtual_models::register(definition, EXT),
        Err(_) => _ = virtual_models::unregister(VIRTUAL_PROVIDER, VIRTUAL_ID),
    }
}

/// `jet` 扩展。
pub struct Jet;

impl Extension for Jet {
    /// 扩展注册名（`/extension` 面板与 `/jet` 命令共用）。
    fn name(&self) -> &str {
        EXT
    }

    /// 面板里的功能说明：虚拟模型 `jet/auto` 的按相位路由 + 两条配置命令。
    fn description(&self) -> &str {
        "Virtual model jet/auto: plan on a strong model and implement on a cheap one (routes on \
         the first successful edit/write). Configure with /jet set (classifier) and /jet router \
         (targets); stored in agent_dir()/extensions/jet.json"
    }

    /// 移植自 pi 的示例插件 `examples/extensions/jev-router.ts`。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "pi".to_string(),
            plugin_version: "0.99.1".to_string(),
            url: "https://github.com/earendil-works/pi".to_string(),
        })
    }

    /// 默认关闭：路由目标与分类器都要用户自己配，未配置前注册不了任何东西。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 在 Dev 与 Creator 两个模式下可用（Minimal 下不可用）。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 不提供任何模型可见工具：能力通过虚拟模型暴露。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 按配置注册 `jet/auto`；配置不完整时返回空（不往目录里塞不可用条目）。
    fn virtual_models(&self) -> Vec<VirtualModelDefinition> {
        build_definition(&config::load()).into_iter().collect()
    }

    /// `/jet`（子命令 set / router / reset）：写配置 + 立即重注册，不锁 agent。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Jev-style router: configure the jet/auto virtual model".to_string(),
            busy_safe: true,
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 接线 `/jet` 的 TUI 执行入口。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_jet);
    }

    /// 被启用时按当前配置补一次注册（禁用期间配置可能已变）。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            sync_registration();
        }
    }
}

/// `/jet router` 解析出的参数：`None` = 本次未给（保持现值）。
#[derive(Debug, Default)]
struct RouterArgs {
    /// `--provider`：一次给三档设同一个 provider。
    provider: Option<String>,
    /// `--planning` 的规划模型 id。
    planning: Option<String>,
    /// `--planning-complex` 的复杂任务规划模型 id。
    planning_complex: Option<String>,
    /// `--implementation` 的实现模型 id。
    implementation: Option<String>,
    /// `--planning-provider`：单独覆盖规划档的 provider。
    planning_provider: Option<String>,
    /// `--planning-complex-provider`：单独覆盖复杂规划档的 provider。
    planning_complex_provider: Option<String>,
    /// `--implementation-provider`：单独覆盖实现档的 provider。
    implementation_provider: Option<String>,
}

impl RouterArgs {
    /// 七个 flag 一个都没给。
    fn is_empty(&self) -> bool {
        self.provider.is_none()
            && self.planning.is_none()
            && self.planning_complex.is_none()
            && self.implementation.is_none()
            && self.planning_provider.is_none()
            && self.planning_complex_provider.is_none()
            && self.implementation_provider.is_none()
    }
}

/// 解析 `--flag value` / `--flag=value` 形式的参数；未知 flag、缺值、一个 flag 都没给时返回错误文案。
fn parse_router_args(rest: &str) -> std::result::Result<RouterArgs, String> {
    let mut args = RouterArgs::default();
    let mut tokens = rest.split_whitespace();

    while let Some(token) = tokens.next() {
        let (name, inline) = match token.split_once('=') {
            Some((name, value)) => (name, Some(value.to_string())),
            None => (token, None),
        };
        let slot = match name {
            "--provider" => &mut args.provider,
            "--planning" => &mut args.planning,
            "--planning-complex" => &mut args.planning_complex,
            "--implementation" => &mut args.implementation,
            "--planning-provider" => &mut args.planning_provider,
            "--planning-complex-provider" => &mut args.planning_complex_provider,
            "--implementation-provider" => &mut args.implementation_provider,
            other => return Err(format!("unknown flag `{other}`")),
        };
        let value = match inline {
            Some(value) => value,
            None => tokens
                .next()
                .map(str::to_string)
                .ok_or_else(|| format!("{name} requires a value"))?,
        };
        *slot = Some(value);
    }

    if args.is_empty() {
        return Err("no flags given".to_string());
    }

    Ok(args)
}

/// 校验本次 `/jet router` 给到的 flag：provider 必须已知，改动的目标必须能在目录里解析到
/// （未改的档不动；某个目标还没配全时留给注册门槛报因）。
fn validate_router_change(
    args: &RouterArgs,
    before: &RouterConfig,
    candidate: &RouterConfig,
) -> std::result::Result<(), String> {
    for (flag, provider) in [
        ("--provider", args.provider.as_deref()),
        ("--planning-provider", args.planning_provider.as_deref()),
        (
            "--planning-complex-provider",
            args.planning_complex_provider.as_deref(),
        ),
        (
            "--implementation-provider",
            args.implementation_provider.as_deref(),
        ),
    ] {
        let Some(provider) = provider else {
            continue;
        };

        let provider = provider.trim();
        if provider.is_empty() {
            return Err(format!("{flag} must not be empty"));
        }

        if !model_resolver::is_known_provider(provider) {
            return Err(format!(
                "unknown provider `{provider}` (no model catalog; check /login or models.json)"
            ));
        }
    }

    for (flag, value) in [
        ("--planning", args.planning.as_deref()),
        ("--planning-complex", args.planning_complex.as_deref()),
        ("--implementation", args.implementation.as_deref()),
    ] {
        if value.is_some_and(|v| v.trim().is_empty()) {
            return Err(format!(
                "{flag} must not be empty (omit the flag to keep the current value)"
            ));
        }
    }

    // 只校验本次真正改动的档（未动的档即使目录里已消失也不拦）。
    for ((flag, old), (_, new)) in before
        .targets_all()
        .into_iter()
        .zip(candidate.targets_all())
    {
        if old == new || !new.is_complete() {
            continue;
        }
        check_physical(&new.provider, &new.model, ModelType::Chat, flag)?;
    }
    Ok(())
}

/// 一个目标的目标摘要（`provider/model`；未配置时 `(unset)`）。
fn target_summary(target: &Target) -> String {
    if target.is_complete() {
        format!("{}/{}", target.provider, target.model)
    } else {
        "(unset)".to_string()
    }
}

/// 把配置里的路由小节渲染成一行（`/jet` 状态与写入回执共用）。
fn router_summary(router: &RouterConfig) -> String {
    router
        .targets()
        .into_iter()
        .map(|(flag, target)| {
            format!(
                "{} {}",
                flag.trim_start_matches("--"),
                target_summary(target)
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// 打印 `jet/auto` 的注册状态（可注册 / 不可注册的原因）。
fn report_registration(st: &mut App, cfg: &JetConfig) {
    match build_definition(cfg) {
        Ok(_) => st.push_msg(
            format!("jet/auto: ready ({})", router_summary(&cfg.router)),
            MsgLevel::Success,
        ),
        Err(reason) => st.push_msg(
            format!("jet/auto: not registered — {reason}"),
            MsgLevel::Warning,
        ),
    }
}

/// 打印分类器配置状态。
fn report_classifier(st: &mut App, cfg: &JetConfig) {
    let classifier = &cfg.classifier;
    if !classifier.is_complete() {
        st.push_msg(
            "jet classifier: (unset — planning complexity is not classified)".to_string(),
            MsgLevel::Info,
        );
        return;
    }

    let label = format!("{}/{}", classifier.provider, classifier.model);
    match check_physical(
        &classifier.provider,
        &classifier.model,
        ModelType::Classifier,
        "jet classifier",
    ) {
        Ok(_) => st.push_msg(format!("jet classifier: {label}"), MsgLevel::Info),
        Err(err) => st.push_msg(
            format!("jet classifier: {label} is unusable — {err}"),
            MsgLevel::Warning,
        ),
    }
}

/// `/jet`（无参数）：打印配置与注册状态。
fn show_status(st: &mut App) {
    let cfg = config::load();
    report_registration(st, &cfg);
    report_classifier(st, &cfg);
}

/// `/jet set <provider> <model-id>`：配置分类器模型（必须是 `type: classifier` 条目）。
fn set_command(st: &mut App, rest: &str) {
    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() != 2 {
        st.push_msg(format!("jet: {SET_USAGE}"), MsgLevel::Warning);
        return;
    }

    let (provider, model) = (parts[0], parts[1]);
    if let Err(err) = check_physical(provider, model, ModelType::Classifier, "jet classifier") {
        st.push_msg(format!("jet: {err}"), MsgLevel::Warning);
        return;
    }

    let cfg = config::set_classifier(provider, model);
    sync_registration();
    st.push_msg(
        format!("jet classifier set: {provider}/{model}"),
        MsgLevel::Success,
    );
    report_registration(st, &cfg);
}

/// `/jet router [flags]`：增量覆盖路由目标（只改给到的项）。
///
/// `--provider` 一次给三档设同一个 provider，`--*-provider` 单独覆盖某一档；
/// 两者同时给时以单项为准。每档的模型 id 由 `--planning` / `--planning-complex` / `--implementation` 指定。
fn router_command(st: &mut App, rest: &str) {
    let args = match parse_router_args(rest) {
        Ok(args) => args,
        Err(err) => {
            st.push_msg(format!("jet: {err}. {ROUTER_USAGE}"), MsgLevel::Warning);
            return;
        }
    };

    let before = config::load().router;
    let mut candidate = before.clone();

    if let Some(provider) = args.provider.as_deref() {
        let provider = provider.trim().to_string();
        candidate.planning.provider = provider.clone();
        candidate.planning_complex.provider = provider.clone();
        candidate.implementation.provider = provider;
    }

    // 单项覆盖在批量之后应用：两者同时给时单项说了算。
    for (slot, value) in [
        (&mut candidate.planning.provider, &args.planning_provider),
        (
            &mut candidate.planning_complex.provider,
            &args.planning_complex_provider,
        ),
        (
            &mut candidate.implementation.provider,
            &args.implementation_provider,
        ),
    ] {
        if let Some(value) = value {
            *slot = value.trim().to_string();
        }
    }

    for (slot, value) in [
        (&mut candidate.planning.model, &args.planning),
        (
            &mut candidate.planning_complex.model,
            &args.planning_complex,
        ),
        (&mut candidate.implementation.model, &args.implementation),
    ] {
        if let Some(value) = value {
            *slot = value.trim().to_string();
        }
    }

    if let Err(err) = validate_router_change(&args, &before, &candidate) {
        st.push_msg(format!("jet: {err}"), MsgLevel::Warning);
        return;
    }

    let cfg = config::set_router(&candidate);
    sync_registration();
    st.push_msg(
        format!("jet router set: {}", router_summary(&cfg.router)),
        MsgLevel::Success,
    );
    report_registration(st, &cfg);
}

/// `/jet reset`：清空配置并注销 `jet/auto`。
fn reset_command(st: &mut App) {
    config::reset();
    sync_registration();
    st.push_msg(
        "jet: configuration cleared (jet/auto unregistered)".to_string(),
        MsgLevel::Success,
    );
    switch_away_from_virtual(st);
}

/// reset 之后 `jet/auto` 已注销：若当前会话正选中它，切回最近一次路由到的物理模型
/// （拿不到就回落到 provider/model 解析的兜底模型），避免留下必然报错的选中项。
fn switch_away_from_virtual(st: &mut App) {
    let selected_virtual = st.current_provider.as_deref() == Some(VIRTUAL_PROVIDER)
        && st.current_model.as_deref() == Some(VIRTUAL_ID);

    if !selected_virtual {
        return;
    }

    let Some((provider, model_id)) = switch_target(&st.messages) else {
        st.push_msg(
            format!(
                "jet: switch away from {VIRTUAL_PROVIDER}/{VIRTUAL_ID} manually (/model): no usable fallback model"
            ),
            MsgLevel::Warning,
        );
        return;
    };

    st.worker.send(AgentCommand::SwitchModel {
        provider: provider.clone(),
        model_id: model_id.clone(),
        persist: false,
    });
    st.push_msg(
        format!(
            "jet: current session was on {VIRTUAL_PROVIDER}/{VIRTUAL_ID}; switched to {provider}/{model_id}"
        ),
        MsgLevel::Info,
    );
}

/// reset 时切回的目标：最近一次路由到的物理模型；拿不到则回落到 provider/model 解析的兜底模型。
fn switch_target(messages: &[AgentMessage]) -> Option<(String, String)> {
    virtual_models::previous_route(messages)
        .map(|route| (route.provider, route.model_id))
        .or_else(|| {
            model_resolver::resolve_provider_model(None, None, &settings_manager::read_settings())
                .ok()
                .map(|(provider, model_id, _)| (provider, model_id))
        })
}

/// `/jet` 的 TUI 执行入口：返回 `false`（永不退出 TUI）。
fn command_jet(st: &mut App, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((sub, rest)) => (sub, rest.trim()),
        None => (arg, ""),
    };

    match sub {
        "" => show_status(st),
        "set" => set_command(st, rest),
        "router" => router_command(st, rest),
        "reset" => reset_command(st),
        other => st.push_msg(
            format!("Unknown /jet subcommand: {other}. Use `set`, `router` or `reset`."),
            MsgLevel::Warning,
        ),
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::app::sys_spans_text;
    use std::sync::MutexGuard;
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    /// 测试护栏：临时 agent_dir + 全局 auth 锁（配置/凭据读写走进程级单例）。
    fn setup() -> (crate::test_support::AgentDirGuard, MutexGuard<'static, ()>) {
        let guard = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (crate::test_support::AgentDirGuard::temp(), guard)
    }

    /// 消息夹具：由 JSON 构造 [`AgentMessage`]（缺省字段走 serde 默认）。
    fn msg(value: Value) -> AgentMessage {
        serde_json::from_value(value).expect("valid AgentMessage fixture")
    }

    /// 用户文本消息。
    fn user(text: &str) -> AgentMessage {
        AgentMessage::user_text(text)
    }

    /// 工具结果消息。
    fn tool_result(name: &str, is_error: bool) -> AgentMessage {
        msg(json!({
            "role": "toolResult",
            "content": [],
            "toolName": name,
            "isError": is_error,
        }))
    }

    /// assistant 消息（带命中的物理 provider / 模型）。
    fn assistant(provider: &str, model: &str) -> AgentMessage {
        msg(json!({
            "role": "assistant",
            "content": [],
            "provider": provider,
            "model": model,
        }))
    }

    /// 一个路由目标（测试夹具）。
    fn target(provider: &str, model: &str) -> config::Target {
        config::Target {
            provider: provider.to_string(),
            model: model.to_string(),
        }
    }

    /// 取目录里的 openai chat 模型 id（目录变动时这里最先失败，提示改夹具）。
    fn catalog_model(id: &str) -> String {
        model_resolver::find_model_of_type("openai", id, ModelType::Chat)
            .unwrap_or_else(|e| panic!("catalog fixture {id} missing: {e}"))
            .id
    }

    /// 取某个 provider 的第一个 chat 模型 id。
    fn first_chat(provider: &str) -> String {
        model_resolver::list_models(provider)
            .first()
            .map(|(id, _)| id.clone())
            .unwrap_or_else(|| panic!("provider {provider} 应有 chat 模型"))
    }

    /// 三档都在 openai 上的路由配置。
    fn router_cfg() -> config::RouterConfig {
        config::RouterConfig {
            planning: target("openai", &catalog_model("gpt-5.6-terra")),
            planning_complex: target("openai", &catalog_model("gpt-5.6-sol")),
            implementation: target("openai", &catalog_model("gpt-5.6-luna")),
        }
    }

    /// 路由器夹具：直接构造（不起网络）。
    fn router(
        router: config::RouterConfig,
        classifier: Option<config::ClassifierConfig>,
    ) -> JetRouter {
        JetRouter { router, classifier }
    }

    /// 配置夹具（路由三档 + 空分类器）。
    fn cfg_of(router: config::RouterConfig) -> config::JetConfig {
        config::JetConfig {
            router,
            classifier: config::ClassifierConfig::default(),
        }
    }

    /// 路由输入夹具。
    fn input<'a>(
        reason: ModelRouteReason,
        previous: Option<&'a RouteTarget>,
        state: Option<&'a Value>,
        messages: &'a [AgentMessage],
    ) -> RouteInput<'a> {
        RouteInput {
            reason,
            thinking_level: Some("high"),
            previous,
            state,
            messages,
        }
    }

    /// 注册门槛的错误文案（断言用；通过即 panic）。
    fn gate_err(cfg: &config::JetConfig) -> String {
        match build_definition(cfg) {
            Ok(_) => panic!("expected the registration gate to reject {cfg:?}"),
            Err(err) => err,
        }
    }

    /// 全部系统消息文本（断言失败时打印用）。
    fn all_msgs(st: &App) -> Vec<String> {
        st.system_messages
            .iter()
            .map(|(_, msg)| sys_spans_text(&msg.spans))
            .collect()
    }

    /// 是否存在包含 `needle` 的系统消息。
    fn has_msg(st: &App, needle: &str) -> bool {
        all_msgs(st).iter().any(|text| text.contains(needle))
    }

    #[test]
    fn state_round_trips_and_rejects_invalid() {
        let state = JetState::new(Phase::Planning, "m1");
        assert_eq!(
            state.to_value(),
            json!({ "phase": "planning", "model": "m1" })
        );
        assert_eq!(JetState::from_value(&state.to_value()), Some(state));

        assert_eq!(JetState::from_value(&json!({ "phase": "planning" })), None);
        assert_eq!(JetState::from_value(&json!({ "model": "m1" })), None);
        assert_eq!(
            JetState::from_value(&json!({ "phase": "paused", "model": "m1" })),
            None
        );
        assert_eq!(
            JetState::from_value(&json!({ "phase": "planning", "model": " " })),
            None
        );
    }

    #[test]
    fn edited_this_turn_detects_only_successful_edits_after_last_user() {
        assert!(edited_this_turn(&[user("hi"), tool_result("edit", false)]));
        assert!(edited_this_turn(&[user("hi"), tool_result("write", false)]));

        // 失败的编辑不算
        assert!(!edited_this_turn(&[user("hi"), tool_result("edit", true)]));
        // 非编辑工具不算
        assert!(!edited_this_turn(&[user("hi"), tool_result("read", false)]));
        // 上一条 user 之前发生的编辑不算
        assert!(!edited_this_turn(&[
            user("old"),
            tool_result("edit", false),
            user("new"),
        ]));
        // 没有任何 user 消息
        assert!(!edited_this_turn(&[tool_result("edit", false)]));
    }

    #[test]
    fn plan_target_prefers_complex_only_when_available() {
        let mut router = router_cfg();
        assert_eq!(
            plan_target(&router, Complexity::Complex),
            &router.planning_complex
        );
        assert_eq!(plan_target(&router, Complexity::Standard), &router.planning);
        assert_eq!(plan_target(&router, Complexity::Unknown), &router.planning);

        // 没配复杂档：复杂判定也回落到普通规划档
        router.planning_complex = config::Target::default();
        assert_eq!(plan_target(&router, Complexity::Complex), &router.planning);
    }

    #[test]
    fn last_user_text_joins_blocks_and_truncates() {
        let messages = [user("first"), assistant("openai", "m"), user("second")];
        assert_eq!(last_user_text(&messages), "second");

        let long = "x".repeat(PROMPT_MAX_CHARS + 10);
        let messages = [user(&long)];
        assert_eq!(last_user_text(&messages).chars().count(), PROMPT_MAX_CHARS);

        assert_eq!(last_user_text(&[assistant("openai", "m")]), "");
    }

    #[test]
    fn classifier_context_asks_the_complexity_choice() {
        let context = classifier_context("do the thing");
        assert_eq!(context.state, json!({ "prompt": "do the thing" }));
        let question = context.questions.get(COMPLEXITY_ID).expect("question");
        match question {
            ClassifierQuestion::Choice {
                instructions,
                criteria,
            } => {
                assert!(instructions.contains("software engineering work"));
                assert_eq!(
                    criteria.keys().collect::<Vec<_>>(),
                    vec!["complex", "standard"]
                );
            }
            other => panic!("expected a choice question, got {other:?}"),
        }
    }

    #[test]
    fn parse_router_args_accepts_both_forms_and_rejects_bad_input() {
        let args = parse_router_args(
            "--provider openai --planning=a --implementation b --implementation-provider anthropic",
        )
        .unwrap();
        assert_eq!(args.provider.as_deref(), Some("openai"));
        assert_eq!(args.planning.as_deref(), Some("a"));
        assert_eq!(args.implementation.as_deref(), Some("b"));
        assert_eq!(args.implementation_provider.as_deref(), Some("anthropic"));
        assert_eq!(args.planning_complex, None);

        assert!(
            parse_router_args("--provider")
                .unwrap_err()
                .contains("requires a value")
        );
        assert!(
            parse_router_args("--nope x")
                .unwrap_err()
                .contains("unknown flag")
        );
        assert!(
            parse_router_args("   ")
                .unwrap_err()
                .contains("no flags given")
        );
    }

    #[tokio::test]
    async fn route_direct_goes_to_implementation_without_state() {
        let router = router(router_cfg(), None);
        let state = json!({ "phase": "planning", "model": router.router.planning.model });
        let out = router
            .decide(input(ModelRouteReason::Direct, None, Some(&state), &[]))
            .await
            .unwrap();

        assert_eq!(out.provider, router.router.implementation.provider);
        assert_eq!(out.model_id, router.router.implementation.model);
        assert_eq!(out.thinking_level.as_deref(), Some("high"));
        assert_eq!(out.state, None);
    }

    #[tokio::test]
    async fn route_new_session_plans_and_writes_state() {
        // 无分类器 → 缺省规划档
        let router = router(router_cfg(), None);
        let out = router
            .decide(input(ModelRouteReason::User, None, None, &[user("hi")]))
            .await
            .unwrap();

        assert_eq!(out.provider, router.router.planning.provider);
        assert_eq!(out.model_id, router.router.planning.model);
        assert_eq!(
            out.state,
            Some(json!({ "phase": "planning", "model": router.router.planning.model }))
        );
    }

    #[tokio::test]
    async fn route_reuses_previous_planning_model() {
        let router = router(router_cfg(), None);
        let complex = router.router.planning_complex.clone();

        // 上一轮就在本配置的复杂规划档上：沿用，且新状态记的是它
        let previous = RouteTarget {
            provider: complex.provider.clone(),
            model_id: complex.model.clone(),
            thinking_level: None,
        };
        let out = router
            .decide(input(ModelRouteReason::User, Some(&previous), None, &[]))
            .await
            .unwrap();
        assert_eq!(out.model_id, complex.model);
        assert_eq!(
            out.state,
            Some(json!({ "phase": "planning", "model": complex.model }))
        );

        // provider 不是本配置的：不沿用，回落普通规划档
        let previous = RouteTarget {
            provider: "anthropic".to_string(),
            model_id: complex.model,
            thinking_level: None,
        };
        let out = router
            .decide(input(ModelRouteReason::User, Some(&previous), None, &[]))
            .await
            .unwrap();
        assert_eq!(out.model_id, router.router.planning.model);
    }

    #[tokio::test]
    async fn route_switches_to_implementation_after_first_edit() {
        let router = router(router_cfg(), None);
        let state = json!({ "phase": "planning", "model": router.router.planning.model });
        let messages = [user("do it"), tool_result("edit", false)];

        let out = router
            .decide(input(
                ModelRouteReason::Continuation,
                None,
                Some(&state),
                &messages,
            ))
            .await
            .unwrap();
        assert_eq!(out.model_id, router.router.implementation.model);
        assert_eq!(
            out.state,
            Some(json!({ "phase": "implementation", "model": router.router.implementation.model }))
        );

        // 已在实现阶段：即使又编辑也保持不动，且不再重复落状态
        let state =
            json!({ "phase": "implementation", "model": router.router.implementation.model });
        let out = router
            .decide(input(
                ModelRouteReason::Continuation,
                None,
                Some(&state),
                &messages,
            ))
            .await
            .unwrap();
        assert_eq!(out.model_id, router.router.implementation.model);
        assert_eq!(out.state, None);
    }

    #[tokio::test]
    async fn route_keeps_state_model_without_rewriting_state() {
        let router = router(router_cfg(), None);
        let state = json!({ "phase": "planning", "model": router.router.planning.model });
        let out = router
            .decide(input(
                ModelRouteReason::Continuation,
                None,
                Some(&state),
                &[user("more")],
            ))
            .await
            .unwrap();

        assert_eq!(out.model_id, router.router.planning.model);
        assert_eq!(out.state, None, "沿用状态时不得重复落条目");
    }

    #[tokio::test]
    async fn route_replans_when_state_model_left_the_config() {
        let router = router(router_cfg(), None);
        let state = json!({ "phase": "implementation", "model": "retired-model" });
        let out = router
            .decide(input(
                ModelRouteReason::Continuation,
                None,
                Some(&state),
                &[],
            ))
            .await
            .unwrap();

        assert_eq!(out.model_id, router.router.planning.model);
        assert_eq!(
            out.state,
            Some(json!({ "phase": "planning", "model": router.router.planning.model }))
        );
    }

    #[test]
    fn definition_gate_requires_complete_and_resolvable_targets() {
        let (_ad, _g) = setup();

        let definition =
            build_definition(&cfg_of(router_cfg())).expect("完整且可解析的配置应可注册");
        assert_eq!(definition.provider, VIRTUAL_PROVIDER);
        assert_eq!(definition.id, VIRTUAL_ID);
        assert_eq!(definition.name, VIRTUAL_NAME);

        // 缺必填档
        let mut incomplete = router_cfg();
        incomplete.implementation = config::Target::default();
        assert!(gate_err(&cfg_of(incomplete)).contains("incomplete"));

        // 模型 id 缺了（provider 有值）也算未配置
        let mut half = router_cfg();
        half.planning.model = String::new();
        assert!(gate_err(&cfg_of(half)).contains("incomplete"));

        // 目录里没有的模型
        let mut unknown = router_cfg();
        unknown.implementation.model = "no-such-model".to_string();
        assert!(gate_err(&cfg_of(unknown)).contains("--implementation"));

        // 未知 provider
        let mut unknown_provider = router_cfg();
        unknown_provider.implementation.provider = "no-such-provider".to_string();
        assert!(gate_err(&cfg_of(unknown_provider)).contains("no-such-provider"));

        // 类型不对：图片条目 / 分类器条目都不能当 chat 目标（openrouter 上两家都有）
        let openrouter = |kind: ModelType| {
            model_resolver::list_models_of_type("openrouter", kind)
                .into_iter()
                .map(|(id, _)| id)
                .collect::<Vec<_>>()
        };
        let chats = openrouter(ModelType::Chat);
        let image = openrouter(ModelType::Image)
            .first()
            .cloned()
            .expect("image 条目");
        let classifier = openrouter(ModelType::Classifier)
            .first()
            .cloned()
            .expect("classifier 条目");

        let mut wrong_type = router_cfg();
        wrong_type.implementation = target("openrouter", &image);
        assert!(gate_err(&cfg_of(wrong_type)).contains("--implementation"));

        let mut wrong_type = router_cfg();
        wrong_type.planning = target("openrouter", &classifier);
        wrong_type.planning_complex = target("openrouter", &chats[0]);
        assert!(gate_err(&cfg_of(wrong_type)).contains("--planning"));

        // 不能把虚拟模型自己当目标
        let mut self_ref = router_cfg();
        self_ref.planning = target(VIRTUAL_PROVIDER, VIRTUAL_ID);
        assert!(gate_err(&cfg_of(self_ref)).contains("virtual model"));
    }

    #[test]
    fn definition_inherits_limits_and_thinking_levels() {
        let (_ad, _g) = setup();
        let router_cfg = router_cfg();
        let definition = build_definition(&cfg_of(router_cfg.clone())).unwrap();

        let planning = check_physical(
            &router_cfg.planning.provider,
            &router_cfg.planning.model,
            ModelType::Chat,
            "p",
        )
        .unwrap();
        let complex = check_physical(
            &router_cfg.planning_complex.provider,
            &router_cfg.planning_complex.model,
            ModelType::Chat,
            "c",
        )
        .unwrap();
        assert_eq!(
            definition.context_window,
            tighter_u32(planning.context_window, complex.context_window)
        );
        assert_eq!(
            definition.max_tokens,
            tighter_u32(planning.max_tokens, complex.max_tokens)
        );

        let implementation = check_physical(
            &router_cfg.implementation.provider,
            &router_cfg.implementation.model,
            ModelType::Chat,
            "i",
        )
        .unwrap();
        let expected = thinking_intersection(&[&planning, &implementation, &complex]);
        let expected: Vec<String> = if expected.is_empty() {
            vec!["off".to_string()]
        } else {
            expected
        };
        assert_eq!(definition.resolved_thinking_levels(), expected);
    }

    /// 三档可以来自不同 provider：每档各自解析、各自路由，能力取交集。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 与其他用例共用进程级 agent_dir：串行化避免污染
    async fn targets_may_come_from_different_providers() {
        let (_ad, _g) = setup();
        let anthropic = first_chat("anthropic");

        let mut router_cfg = router_cfg();
        router_cfg.implementation = target("anthropic", &anthropic);
        let definition =
            build_definition(&cfg_of(router_cfg.clone())).expect("跨 provider 的三档应可注册");

        let planning =
            check_physical("openai", &router_cfg.planning.model, ModelType::Chat, "p").unwrap();
        let implementation = check_physical("anthropic", &anthropic, ModelType::Chat, "i").unwrap();
        let complex = check_physical(
            "openai",
            &router_cfg.planning_complex.model,
            ModelType::Chat,
            "c",
        )
        .unwrap();
        let expected = thinking_intersection(&[&planning, &implementation, &complex]);
        let expected: Vec<String> = if expected.is_empty() {
            vec!["off".to_string()]
        } else {
            expected
        };
        assert_eq!(
            definition.resolved_thinking_levels(),
            expected,
            "thinking 档位是三个目标（可能来自不同 provider）的交集"
        );

        let jet = router(router_cfg, None);
        let out = jet
            .decide(input(ModelRouteReason::Direct, None, None, &[]))
            .await
            .unwrap();
        assert_eq!(
            (out.provider.as_str(), out.model_id.as_str()),
            ("anthropic", anthropic.as_str()),
            "实现档用自己的 provider"
        );

        let out = jet
            .decide(input(ModelRouteReason::User, None, None, &[]))
            .await
            .unwrap();
        assert_eq!(out.provider, "openai", "规划档仍是 openai");
        assert_eq!(out.model_id, jet.router.planning.model);
    }

    /// 只换某档的 provider 时，已有状态按模型 id 继续沿用新 provider。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 与其他用例共用进程级 agent_dir：串行化避免污染
    async fn state_follows_target_after_provider_change() {
        let (_ad, _g) = setup();
        let model = catalog_model("gpt-5.6-terra");

        let mut before = router_cfg();
        before.planning = target("openai", &model);
        let state = json!({ "phase": "planning", "model": model });

        // 只是把规划档的 provider 换成另一家（模型 id 不变）→ 沿用，不重新判定
        let mut after = before.clone();
        after.planning = target("openrouter", &model);
        let jet = router(after, None);
        let out = jet
            .decide(input(
                ModelRouteReason::Continuation,
                None,
                Some(&state),
                &[user("more")],
            ))
            .await
            .unwrap();
        assert_eq!(
            (out.provider.as_str(), out.model_id.as_str()),
            ("openrouter", model.as_str())
        );
        assert_eq!(out.state, None, "沿用状态时不重复落条目");
    }

    #[test]
    fn virtual_models_follow_configuration() {
        let (_ad, _g) = setup();
        let ext = Jet;

        // 未配置：不注册任何条目
        assert!(ext.virtual_models().is_empty());

        // 完整配置：注册一个 jet/auto
        config::set_router(&router_cfg());
        let definitions = ext.virtual_models();
        assert_eq!(definitions.len(), 1);
        assert_eq!(definitions[0].provider, VIRTUAL_PROVIDER);
        assert_eq!(definitions[0].id, VIRTUAL_ID);

        // 清空配置：又不注册了
        config::reset();
        assert!(ext.virtual_models().is_empty());
    }

    #[test]
    fn router_command_writes_incrementally_and_rejects_bad_input() {
        let (_ad, _g) = setup();
        let mut st = App::new();

        command_jet(&mut st, "jet");
        assert!(has_msg(&st, "not registered"), "{:?}", all_msgs(&st));

        // 只配 provider：配置不完整，仍不注册
        command_jet(&mut st, "jet router --provider openai");
        assert!(has_msg(&st, "not registered"), "{:?}", all_msgs(&st));
        let router = config::load().router;
        assert_eq!(router.planning.provider, "openai");
        assert_eq!(router.implementation.provider, "openai");
        assert_eq!(router.planning_complex.provider, "openai", "批量设三档");

        // 未知 flag：不写盘
        command_jet(&mut st, "jet router --planning nope --nope x");
        assert!(has_msg(&st, "unknown flag"), "{:?}", all_msgs(&st));
        assert_eq!(config::load().router.planning.model, "");

        // 目录里没有的模型：不写盘
        command_jet(&mut st, "jet router --planning no-such-model");
        assert!(has_msg(&st, "--planning"), "{:?}", all_msgs(&st));
        assert_eq!(config::load().router.planning.model, "");

        // 未知 provider：不写盘
        command_jet(&mut st, "jet router --provider no-such-provider");
        assert!(has_msg(&st, "unknown provider"), "{:?}", all_msgs(&st));
        assert_eq!(config::load().router.planning.provider, "openai");

        // 某档的 provider 未知：不写盘
        command_jet(
            &mut st,
            "jet router --implementation-provider no-such-provider",
        );
        assert!(has_msg(&st, "unknown provider"), "{:?}", all_msgs(&st));
        assert_eq!(config::load().router.implementation.provider, "openai");

        // 补齐两档模型 → 注册成功
        command_jet(
            &mut st,
            &format!(
                "jet router --planning {} --implementation {}",
                catalog_model("gpt-5.6-terra"),
                catalog_model("gpt-5.6-luna")
            ),
        );
        assert!(has_msg(&st, "ready"), "{:?}", all_msgs(&st));
        let cfg = config::load();
        assert!(cfg.router.is_complete());
        assert!(build_definition(&cfg).is_ok());

        // 增量：只改实现档的模型，其余不动
        command_jet(&mut st, "jet router --implementation=gpt-5.6-sol");
        let cfg = config::load();
        assert_eq!(cfg.router.implementation.model, "gpt-5.6-sol");
        assert_eq!(cfg.router.planning.model, catalog_model("gpt-5.6-terra"));

        // 增量：只把实现档换成另一个 provider（模型 id 也随之给）
        let anthropic = first_chat("anthropic");
        command_jet(
            &mut st,
            &format!("jet router --implementation-provider anthropic --implementation {anthropic}"),
        );
        let cfg = config::load();
        assert_eq!(
            (
                cfg.router.implementation.provider.as_str(),
                cfg.router.implementation.model.as_str()
            ),
            ("anthropic", anthropic.as_str())
        );
        assert_eq!(cfg.router.planning.provider, "openai", "其他档不受影响");
        assert!(build_definition(&cfg).is_ok());

        // 批量 --provider 与单项同时给：单项说了算
        command_jet(
            &mut st,
            "jet router --provider openai --implementation-provider anthropic",
        );
        let cfg = config::load();
        assert_eq!(cfg.router.planning.provider, "openai");
        assert_eq!(cfg.router.implementation.provider, "anthropic");
    }

    #[test]
    fn set_command_requires_a_classifier_entry() {
        let (_ad, _g) = setup();
        let mut st = App::new();

        command_jet(&mut st, "jet set openrouter");
        assert!(has_msg(&st, "Usage: /jet set"), "{:?}", all_msgs(&st));

        // chat 模型不是分类器：拒绝
        command_jet(&mut st, "jet set openai gpt-5.6-terra");
        assert!(has_msg(&st, "type: classifier"), "{:?}", all_msgs(&st));
        assert!(!config::load().classifier.is_complete());

        // 真分类器条目：接受
        let classifier = model_resolver::list_models_of_type("openrouter", ModelType::Classifier)
            .first()
            .map(|(id, _)| id.clone())
            .expect("openrouter 目录应有 classifier 条目");
        command_jet(&mut st, &format!("jet set openrouter {classifier}"));
        assert!(has_msg(&st, "not registered"), "{:?}", all_msgs(&st));
        assert_eq!(config::load().classifier.model, classifier);
    }

    #[test]
    fn reset_clears_config_and_switches_away_from_virtual() {
        let (_ad, _g) = setup();
        config::set_router(&router_cfg());
        config::set_classifier("openrouter", "~typesafe/jev-latest");

        let mut st = App::new();
        st.current_provider = Some(VIRTUAL_PROVIDER.to_string());
        st.current_model = Some(VIRTUAL_ID.to_string());
        st.messages = vec![assistant("anthropic", "claude-real")];

        command_jet(&mut st, "jet reset");

        assert_eq!(config::load(), config::JetConfig::default());
        assert!(build_definition(&config::load()).is_err());
        assert!(
            has_msg(&st, "switched to anthropic/claude-real"),
            "reset 应切走当前会话选中的 jet/auto：{:?}",
            all_msgs(&st)
        );

        // 没选中虚拟模型时只清配置，不提切换
        let mut st = App::new();
        st.current_provider = Some("anthropic".to_string());
        st.current_model = Some("claude-real".to_string());
        command_jet(&mut st, "jet reset");
        assert!(
            !has_msg(&st, "switched to"),
            "未选中 jet/auto 时不应切换：{:?}",
            all_msgs(&st)
        );
    }

    #[test]
    fn switch_target_prefers_previous_physical_model() {
        let (_ad, _g) = setup();

        let messages = [user("hi"), assistant("anthropic", "claude-real")];
        assert_eq!(
            switch_target(&messages),
            Some(("anthropic".to_string(), "claude-real".to_string()))
        );

        // 没有任何物理响应：回落到 provider/model 解析的兜底模型
        let fallback = switch_target(&[]).expect("应能解析兜底模型");
        assert!(!fallback.0.is_empty() && !fallback.1.is_empty());
    }

    /// 启动假分类器服务：读完整请求后回固定 JSON，返回 (base_url, 收到的请求文本)。
    async fn mock_classifier(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            let mut header_end = None;
            loop {
                let n = socket.read(&mut tmp).await.unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
                if header_end.is_none() {
                    header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
                }
                if let Some(he) = header_end {
                    let head = String::from_utf8_lossy(&buf[..he]).to_lowercase();
                    let len: usize = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse().ok())
                        .unwrap_or(0);
                    if buf.len() >= he + len {
                        break;
                    }
                }
            }

            let request = String::from_utf8_lossy(&buf).to_string();
            let reason = if status == 200 { "OK" } else { "Bad Request" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            request
        });
        (format!("http://127.0.0.1:{}", addr.port()), handle)
    }

    /// 把分类器模型写进 models.json（自定义 provider，baseUrl 指向假服务）。
    fn write_classifier_provider(base_url: &str) {
        std::fs::write(
            settings_manager::agent_dir().join("models.json"),
            json!({"providers":{"jet-cls":{
                "apiKey":"sk-test",
                "baseUrl":base_url,
                "api":"typesafe-system-one",
                "models":[{"id":"jev","name":"Jev","type":"classifier","api":"typesafe-system-one"}]
            }}})
            .to_string(),
        )
        .unwrap();
    }

    /// 判为复杂：走复杂规划档，且分类器拿到的是最后一条 user 文本。
    #[test]
    #[allow(clippy::await_holding_lock)] // 与 auth/theme 等测试共用进程级 agent_dir：串行化避免污染
    fn classifier_picks_complex_planning_model() {
        crate::test_support::run_with_timeout(std::time::Duration::from_secs(90), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let (base, handle) = mock_classifier(
                    200,
                    r#"{"answers":{"complexity":{"type":"choice","choice":"complex",
                        "probabilities":{"complex":0.9,"standard":0.1},"confidence":0.9}}}"#,
                )
                .await;

                let (_ad, _g) = setup();
                write_classifier_provider(&base);

                let router_cfg = router_cfg();
                let complex = router_cfg.planning_complex.clone();
                let jet = router(
                    router_cfg.clone(),
                    Some(config::ClassifierConfig {
                        provider: "jet-cls".to_string(),
                        model: "jev".to_string(),
                    }),
                );

                let messages = [user("refactor the scheduler")];
                let out = jet
                    .decide(input(ModelRouteReason::User, None, None, &messages))
                    .await
                    .unwrap();
                assert_eq!(out.model_id, complex.model);
                assert_eq!(out.provider, complex.provider);

                let request = handle.await.unwrap();
                assert!(request.starts_with("POST /systemone "), "{request}");
                assert!(request.contains("refactor the scheduler"), "{request}");
            });
        });
    }

    /// 同 provider 同 id 兼有 chat 条目时，分类器仍按类型取到 classifier 条目。
    ///
    /// 回归：调用点曾用不限类型的解析，同 id 的 chat 条目排在前时会被误取，
    /// `classify` 报类型不符后静默回退普通规划档。
    #[test]
    #[allow(clippy::await_holding_lock)] // 与 auth/theme 等测试共用进程级 agent_dir：串行化避免污染
    fn classifier_entry_wins_over_same_id_chat_entry() {
        crate::test_support::run_with_timeout(std::time::Duration::from_secs(90), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let (base, handle) = mock_classifier(
                    200,
                    r#"{"answers":{"complexity":{"type":"choice","choice":"complex",
                        "probabilities":{"complex":0.9,"standard":0.1},"confidence":0.9}}}"#,
                )
                .await;

                let (_ad, _g) = setup();
                // chat 条目排在 classifier 之前：不限类型的查找会先命中它
                std::fs::write(
                    settings_manager::agent_dir().join("models.json"),
                    json!({"providers":{"jet-cls":{
                        "apiKey":"sk-test",
                        "baseUrl":base,
                        "api":"typesafe-system-one",
                        "models":[
                            {"id":"jev","name":"Jev Chat","type":"chat","api":"openai-completions"},
                            {"id":"jev","name":"Jev","type":"classifier","api":"typesafe-system-one"}
                        ]
                    }}})
                    .to_string(),
                )
                .unwrap();

                let router_cfg = router_cfg();
                let complex = router_cfg.planning_complex.clone();
                let jet = router(
                    router_cfg,
                    Some(config::ClassifierConfig {
                        provider: "jet-cls".to_string(),
                        model: "jev".to_string(),
                    }),
                );

                let out = jet
                    .decide(input(
                        ModelRouteReason::User,
                        None,
                        None,
                        &[user("refactor the scheduler")],
                    ))
                    .await
                    .unwrap();
                // 走了分类器判定才会落到复杂规划档
                assert_eq!(out.model_id, complex.model);
                assert_eq!(out.provider, complex.provider);
                let request = handle.await.unwrap();
                assert!(request.starts_with("POST /systemone "), "{request}");
            });
        });
    }

    /// 分类器不可用（HTTP 失败）：回退普通规划档，不报错。
    #[test]
    #[allow(clippy::await_holding_lock)] // 与 auth/theme 等测试共用进程级 agent_dir：串行化避免污染
    fn classifier_failure_falls_back_to_plain_planning() {
        crate::test_support::run_with_timeout(std::time::Duration::from_secs(90), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let (base, _handle) = mock_classifier(400, "{}").await;

                let (_ad, _g) = setup();
                write_classifier_provider(&base);

                let router_cfg = router_cfg();
                let planning = router_cfg.planning.clone();
                let jet = router(
                    router_cfg,
                    Some(config::ClassifierConfig {
                        provider: "jet-cls".to_string(),
                        model: "jev".to_string(),
                    }),
                );

                let out = jet
                    .decide(input(ModelRouteReason::User, None, None, &[user("hi")]))
                    .await
                    .unwrap();
                assert_eq!(out.model_id, planning.model);
                assert_eq!(out.provider, planning.provider);
            });
        });
    }
}

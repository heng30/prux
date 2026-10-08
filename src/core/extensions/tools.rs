//! 扩展工具定义与子代理物化接缝。
//!
//! 核心只提供**机制**：按 [`SubAgentSpec`] 物化一个状态隔离的 child，并把它包成
//! [`SubAgentRunner`] trait object + [`SubAgentControls`] 句柄交回扩展；
//! 谁 spawn、何时 await、并发怎么排、结果怎么存、通知怎么发，全是扩展的**策略**。

use super::{ToolError, ToolResult};
use crate::core::{
    provider::{AgentMessage, Usage},
    tools::ToolExecutionMode,
};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use strum_macros::{EnumString, IntoStaticStr};

/// [`NestedCallLog`] 的共享句柄，存于 [`ToolExecCtx`]。
pub type NestedCallLogHandle = Arc<Mutex<NestedCallLog>>;

/// 子代理事件 sink（与 [`crate::core::agent_session::JsonSink`] 同型）。
///
/// 扩展提供、核心直接挂到 child 的 `json_sink` 上：于是每个子代理有独立事件流，
/// 扩展借此累加 token 用量、跟踪工具活动、攒 transcript，而无需核心新增查询 API。
pub type SubAgentEventSink = Arc<Mutex<Box<dyn FnMut(Value) + Send>>>;

/// 按 spec 物化子代理的工厂（由核心在 [`ToolExecCtx`] 里提供给扩展）。
pub type MakeSubAgentFn =
    dyn Fn(SubAgentSpec) -> std::result::Result<Box<dyn SubAgentRunner>, String> + Send + Sync;

/// 注入工具的执行体类型（[`InjectedChildTool::handler`]）。
pub type InjectedToolHandler =
    Arc<dyn Fn(Value) -> std::result::Result<ToolResult, ToolError> + Send + Sync>;

/// 嵌套执行一个工具（[`ToolExecCtx::execute_tool`]）。
///
/// 宿主实现：内置 / 扩展 / 注入工具都走同一条解析链，并按 [`ToolExposure`] 门控。
pub type ExecuteToolFn = Arc<
    dyn Fn(String, Value) -> BoxFuture<'static, std::result::Result<ToolResult, ToolError>>
        + Send
        + Sync,
>;

/// 一次子代理物化的完整描述。
///
/// **核心只解释字段，不做任何策略判断**：类型名、名称、时间上限的取舍由扩展决定；
/// 核心负责把这里的每一项落到 child 的模型 / thinking / 工具白名单 / 系统提示 / 会话上。
#[derive(Clone)]
pub struct SubAgentSpec {
    /// 类型名（诊断与展示；核心不解释，扩展用于通知与记录）
    pub agent_type: String,
    /// `prompt_mode = replace`：作为完整系统提示，且同时清空 `context_files`
    pub system_prompt: Option<String>,
    /// `prompt_mode = append`：追加到父级系统提示之后
    pub append_system_prompt: Option<String>,
    /// replace 模式下是否清空父级 `context_files`（AGENTS.md / `.prux/SYSTEM.md`）
    pub clear_context_files: bool,
    /// 精确 `provider/modelId`；None = 继承父级。解析失败由核心返回 `Err`
    pub model: Option<String>,
    /// thinking 级别；None = 继承父级。不被目标模型支持时由核心钳制
    pub thinking: Option<String>,
    /// 工具白名单（`*`/`all` 表示父级全集，`none`/空表示无工具）；
    /// 核心会再剔除子代理工具本身（递归防护），**除非**它们被显式列在这里
    /// （那是扩展为嵌套委派自己放行的，见 `agent_session.rs`）
    pub tools: Option<Vec<String>>,
    /// 最大轮数；None 或 0 = 无上限
    pub max_turns: Option<u32>,
    /// 达 `max_turns` 后允许的收尾宽限轮数（对齐上游 `graceTurns`）；
    /// None = 核心默认 5。`0` 表示到限立即停机。
    pub grace_turns: Option<u32>,
    /// 把父级当前对话复制进 child
    pub inherit_context: bool,
    /// 显式指定 child 的初始对话（覆盖 `inherit_context`）；None = 不设。
    /// 供 `mention` clone 用：把**实时、压缩感知**的对话交给一次性副本，而不是用模板里那份可能陈旧的快照。
    pub context_messages: Option<Vec<AgentMessage>>,
    /// 覆盖 cwd；None = 继承父级
    pub cwd: Option<String>,
    /// 是否落盘子会话
    pub persist_session: bool,
    /// 续跑：打开既有子会话并以其上下文继续（None = 新建）
    pub resume_session_path: Option<String>,
    /// 显式覆盖父会话 id；None = 由核心自动取父 agent 的 session id
    pub session_parent_id: Option<String>,
    /// 覆盖子会话目录
    pub session_dir: Option<PathBuf>,
    /// 可寻址句柄（诊断与展示；核心不解释）
    pub name: Option<String>,
    /// 每 child 独立事件流（核心把它挂到 child 的 `json_sink`）
    pub events: Option<SubAgentEventSink>,
    /// 这个 child 是谁 / 第几层：核心原样带进它的工具执行上下文（`ToolExecCtx.agent_id` / `depth`），
    /// 扩展据此做嵌套限深与属主作用域。
    pub agent_id: Option<String>,
    /// 嵌套深度（主会话 0、其子代理 1，依次递增），扩展据此做限深
    pub depth: u32,
    /// `isolated`：child 不带**扩展工具**（只保留内置工具
    pub isolated: bool,
    /// 从最终白名单里强制剔除的工具名（在 `tools` 收窄与递归防护之后应用）
    pub disallowed_tools: Vec<String>,
    /// 扩展作用域白名单（`extensions:` frontmatter）；None = 全部扩展工具可用。只影响 child 的**工具面**（模型可见/可调）
    pub extensions: Option<Vec<String>>,
    /// 扩展作用域黑名单（`exclude_extensions:` frontmatter）
    pub exclude_extensions: Vec<String>,
    /// 清空继承自父级的技能索引
    pub clear_skills: bool,
    /// 只注入给这个 child 的工具（`agent({schema})` → `StructuredOutput`）。
    ///
    /// 元数据复用 [`ExtensionTool`]；注入发生在白名单收窄**之后**且无条件，
    /// 因此 `tools:` / `disallowed_tools` / `isolated` / `extensions:` 都摘不掉它。
    pub injected_tools: Vec<InjectedChildTool>,
}

impl SubAgentSpec {
    /// 最小构造：只指定类型名，其余继承父级。
    pub fn new(agent_type: impl Into<String>) -> Self {
        SubAgentSpec {
            agent_type: agent_type.into(),
            system_prompt: None,
            append_system_prompt: None,
            clear_context_files: false,
            model: None,
            thinking: None,
            tools: None,
            max_turns: None,
            grace_turns: None,
            inherit_context: false,
            context_messages: None,
            cwd: None,
            persist_session: true,
            resume_session_path: None,
            session_parent_id: None,
            session_dir: None,
            name: None,
            events: None,
            isolated: false,
            disallowed_tools: Vec::new(),
            extensions: None,
            exclude_extensions: Vec::new(),
            clear_skills: false,
            injected_tools: Vec::new(),
            agent_id: None,
            depth: 0,
        }
    }
}

/// 运行期控制句柄。
///
/// 关键约束：[`SubAgentRunner::run`] 会独占 runner，但本句柄可从任意线程/任务持有，
/// 因此 steer 与 abort 必须走 `Arc` 内部共享状态，而不是 `&mut self`。
///
/// **不携带 id**：排队中的 agent 还没有 runner，id 必须在入队时就存在，
/// 因此 id 由扩展自行分配（核心不知道、也不需要知道）。
pub struct SubAgentControls {
    /// 注入 steer（内部写入 child 的 `runtime_steer_inbox`，在下一轮 LLM 调用前生效）
    pub steer: Arc<dyn Fn(String) + Send + Sync>,
    /// 置位即中止 child
    pub abort: Arc<AtomicBool>,
    /// 子会话落盘路径（`persist_session = true` 时 Some）
    pub session_path: Option<String>,
}

impl SubAgentControls {
    /// 是否已被请求中止。
    pub fn is_aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }

    /// 请求中止。
    pub fn request_abort(&self) {
        self.abort.store(true, Ordering::Relaxed);
    }

    /// 注入一条 steer；空文本被忽略。
    pub fn steer_text(&self, text: impl Into<String>) {
        let text = text.into();
        if !text.trim().is_empty() {
            (self.steer)(text);
        }
    }
}

/// 已物化的子代理运行体。扩展拥有它并自行 spawn / await。
pub trait SubAgentRunner: Send {
    /// 运行期控制句柄（可在 `run` 进行中反复调用，返回的句柄共享同一份内部状态）。
    fn controls(&self) -> SubAgentControls;

    /// 驱动一次 prompt（含其内部多 turn），返回最终文本。
    ///
    /// `resume` 由扩展重新物化 + 回灌 messages 完成，核心不感知"哪一次是续跑"。
    fn run(&mut self, prompt: String) -> BoxFuture<'_, std::result::Result<String, String>>;
}

/// 工具暴露方式：决定模型能不能看到、能不能直接调，
/// 以及别的工具能不能经 [`ToolExecCtx::execute_tool`] 嵌套调用它。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, EnumString, IntoStaticStr)]
#[strum(serialize_all = "kebab-case", ascii_case_insensitive)]
pub enum ToolExposure {
    /// 模型可见、可直接调（默认；等同于此前的普通扩展工具）。
    #[default]
    Direct,
    /// 模型可见，但**不允许**其它工具嵌套调用（只能由模型自己直接调）。
    ModelOnly,
    /// 模型不可见，只允许 `codemode` JS 沙箱内经 `execute_tool` 调（沙箱未移植前等价 `Hidden`）。
    Codemode,
    /// 模型不可见，经 `tool_search` 激活后才进工具表（`with_exposure(ToolExposure::Deferred)` 声明）。
    Deferred,
    /// 模型不可见、也不进 codemode，只能由宿主或其它扩展经 `execute_tool` 调（内部工具）。
    Hidden,
}

impl ToolExposure {
    /// 模型可见性：`direct`/`model-only` 恒可见；`deferred` 需已激活；其余永不可见。
    pub fn is_model_visible(self, deferred_activated: bool) -> bool {
        match self {
            ToolExposure::Direct | ToolExposure::ModelOnly => true,
            ToolExposure::Deferred => deferred_activated,
            ToolExposure::Codemode | ToolExposure::Hidden => false,
        }
    }

    /// 是否允许其它工具/扩展经 [`ToolExecCtx::execute_tool`] 调：
    /// `model-only` 拒绝（专属模型），`deferred` 需已激活，其余允许。
    pub fn is_nested_callable(self, deferred_activated: bool) -> bool {
        match self {
            ToolExposure::ModelOnly => false,
            ToolExposure::Deferred => deferred_activated,
            ToolExposure::Direct | ToolExposure::Codemode | ToolExposure::Hidden => true,
        }
    }

    /// 是否允许 `codemode` 脚本经 `tools.<name>()` 调
    ///
    /// 与 [`Self::is_nested_callable`] 的差别只在 `Hidden`：`hidden` 是「宿主或其它扩展直接调的
    /// 内部工具」，不该出现在脚本的工具表里；`direct` 仍要求已激活（未激活＝模型也看不到）。
    pub fn is_script_callable(self, deferred_activated: bool) -> bool {
        match self {
            ToolExposure::Direct | ToolExposure::Codemode => true,
            ToolExposure::Deferred => deferred_activated,
            ToolExposure::ModelOnly | ToolExposure::Hidden => false,
        }
    }

    /// 声明与诊断用的字符串（`/extension` 详情、嵌套拒绝的错误文案）
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析声明/命令行传入的字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    pub fn parse(s: &str) -> Option<Self> {
        s.trim().parse().ok()
    }
}

/// 工具注解：给宿主与 UI 的提示。
///
/// 没有权限系统，因此这些标志**不改变执行语义**，只用于展示与扩展自判。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ToolAnnotations {
    /// `readOnlyHint`：调用不修改任何状态（文件/网络/进程）。
    pub read_only: bool,
    /// `destructiveHint`：可能造成不可逆改动（删除、覆盖、对外发送）。
    pub destructive: bool,
    /// `idempotentHint`：相同参数重复调用与一次调用等效。
    pub idempotent: bool,
}

impl ToolAnnotations {
    /// 三个标志均为 false（未声明任何注解）。
    pub fn is_empty(&self) -> bool {
        !self.read_only && !self.destructive && !self.idempotent
    }

    /// 展示用标签（如 `read-only`/`destructive`/`idempotent`）；未声明的标志不出现在结果里。
    pub fn labels(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.read_only {
            out.push("read-only");
        }
        if self.destructive {
            out.push("destructive");
        }
        if self.idempotent {
            out.push("idempotent");
        }
        out
    }
}

/// 工具集装载信息：`prepare_loadout` 的输入
#[derive(Debug, Clone, Default)]
pub struct ToolLoadout {
    /// 模型可见的工具声明（内置 + 扩展，已按可见性过滤；未激活的 deferred 不在内）。
    pub declared: Vec<ExtensionTool>,
    /// 脚本/嵌套可调的工具（含 `codemode` 等模型不可见的；通常 ⊇ `declared`）。
    pub callable: Vec<ExtensionTool>,
}

/// `prepare_loadout` 对本次请求的改写
#[derive(Debug, Clone, Default)]
pub struct ToolLoadoutChanges {
    /// 覆盖某个工具在**本次请求**里的描述（按工具名）；未列出的工具保持原描述。
    pub descriptions: HashMap<String, String>,
    /// 本次请求中不声明 schema 的工具（仍激活、仍可被脚本/嵌套调用）。
    pub hidden_declarations: Vec<String>,
}

/// 未接线嵌套调用时的占位实现：返回错误而不是静默失败。
pub fn unavailable_tool_exec() -> ExecuteToolFn {
    Arc::new(|name, _| {
        Box::pin(async move {
            Err(ToolError(format!(
                "nested tool execution is unavailable in this context (tool: {name})"
            )))
        })
    })
}

/// 嵌套调用记录与用量合计（[`ToolExecCtx`] 的一份共享账本，克隆上下文共享同一份）。
#[derive(Debug, Default, Clone)]
pub struct NestedCallLog {
    /// 已完成的嵌套调用记录（键：`toolCallId`/`toolName`/`args`/`isError`/`text`/`nestedCalls` 等）。
    pub calls: Vec<Value>,
    /// 超出条数上限后被丢弃的记录数（>0 时宿主会在调用方结果里标注）。
    pub dropped: u32,
    /// 全部嵌套调用的用量合计；无用量为 None。
    pub usage: Option<Usage>,
}

/// 扩展工具执行上下文：扩展工具在引擎内可用的能力（cwd、子代理调度、嵌套调工具）。
/// Arc 化以便跨 thunk 分享。
#[derive(Clone)]
pub struct ToolExecCtx {
    /// 当前工作目录
    pub cwd: String,
    /// 按 spec 物化一个隔离子代理
    pub make_sub_agent: Arc<MakeSubAgentFn>,
    /// 父级当前回合的取消信号；前台子代理可据此联动中止
    pub parent_abort: Arc<AtomicBool>,
    /// 这个上下文属于哪个 agent（`None` = 主会话）。
    ///
    /// 由扩展在 `SubAgentSpec.agent_id` 里声明、核心原样带回：扩展据此知道"现在说话的是
    /// 哪一个子代理"，从而做嵌套限深与属主作用域（核心不认识这些语义，只是把身份传下去）。
    pub agent_id: Option<String>,
    /// 嵌套深度（主会话 0、它的子代理 1…）
    pub depth: u32,
    /// 当前 agent 生效的型号 `provider/id`（子代理扩展据此在继承父级型号时记录实际型号）。核心不解释它，只是透出。
    pub parent_model: Option<String>,
    /// 嵌套执行其它工具（`ctx.execute_tool(name, args)`）：内置/扩展/注入工具统一解析，
    /// 按 [`ToolExposure`] 门控，并受嵌套递归深度与 `parent_abort` 约束。
    pub execute_tool: ExecuteToolFn,
    /// 本次工具调用的 id（嵌套调用的事件与记录里作为 `parentToolCallId`）；
    /// None = 非工具执行期上下文（如 [`crate::core::agent_session::Agent::exec_ctx`]）。
    pub parent_tool_call_id: Option<String>,
    /// 本次工具执行期间经 `execute_tool` 产生的嵌套调用记录与用量合计；
    /// 宿主在工具返回后取走（见 [`Self::take_nested_calls`]）。
    pub nested_calls: NestedCallLogHandle,
    /// 本次执行里**允许脚本编排**的工具表（`codemode` 的 `tools.<name>()`）。
    ///
    /// 口径：激活的内置工具 + [`ToolExposure::is_script_callable`] 成立的扩展工具；由宿主在构造上下文时算好
    pub script_tools: Arc<Vec<ExtensionTool>>,
    /// 当前会话活动分支上的条目（原始 JSON，根在前）；供 `codemode` 的 `store()` 按**当前分支**重建快照
    ///
    /// `None` = 拿不到分支数据（无会话 / 非脚本宿主的工具调用 / 子代理上下文），
    /// 消费方应保持既有快照；`Some(空)` = 当前分支确实没有条目（快照应为空）。
    /// 由宿主在构造上下文时算好，并沿嵌套调用继承。
    pub session_branch_entries: Option<Arc<Vec<Value>>>,
    /// 本次执行是否来自脚本宿主（`codemode` 的 `tools.<name>()`）：
    /// `true` = 调用方是脚本，`codemode` 暴露方式的工具对其可见可调；`false` = 模型/其它工具直接调。
    pub script_call: bool,
}

impl ToolExecCtx {
    /// 嵌套执行一个工具（`ctx.execute_tool(name, args)`）。
    ///
    /// 等价于调用 [`Self::execute_tool`] 字段：内置 / 扩展 / 注入工具由宿主统一解析，
    /// 并按 [`ToolExposure`]、递归深度与取消信号门控。
    pub fn execute_tool(
        &self,
        name: impl Into<String>,
        args: Value,
    ) -> BoxFuture<'static, std::result::Result<ToolResult, ToolError>> {
        (self.execute_tool)(name.into(), args)
    }

    /// 取走嵌套调用账本（记录 + 丢弃计数 + 用量合计），取走后上下文内清空。
    pub fn take_nested_calls(&self) -> NestedCallLog {
        std::mem::take(&mut *self.nested_calls.lock().unwrap())
    }
}

/// 扩展工具定义（并入模型可见的工具列表；对应内置 [`crate::core::tools::index::ToolDef`]）
#[derive(Debug, Clone)]
pub struct ExtensionTool {
    /// 工具名（用于模型调用与分发）
    pub name: String,
    /// 工具描述
    pub description: String,
    /// Human-readable label（UI 展示；未接入 TUI 渲染时为 None）
    pub label: Option<String>,
    /// JSON Schema 参数定义
    pub parameters: Value,
    /// 系统提示中 Available tools 段的单行片段
    pub snippet: String,
    /// 系统提示 Guidelines 段追加的要点
    pub prompt_guidelines: Vec<String>,
    /// provider 侧 constrained sampling（true=请求约束采样）
    pub constrained_sampling: bool,
    /// 工具渲染外壳（"default" | "self"；TUI 未消费，仅声明）
    pub render_shell: Option<String>,
    /// 单工具执行模式覆盖
    pub execution_mode: Option<ToolExecutionMode>,
    /// 工具参数准备（LLM 参数发到执行前先变换）
    pub prepare_arguments: Option<fn(&Value) -> Value>,
    /// 暴露方式（见 [`ToolExposure`]）；默认 `Direct`。
    pub exposure: ToolExposure,
    /// 工具分组名（`/extension` 详情与诊断用，**不参与**工具名解析）；None = 不分组。
    pub namespace: Option<String>,
    /// 工具注解（只读/破坏性/幂等）；仅声明与展示，不改变执行语义。
    pub annotations: ToolAnnotations,
    /// 结构化输出（`structuredContent`）的 JSON Schema；None = 不声明。
    /// 声明后该 schema 的字段名会进 `tool_search` 检索文本（按输出内容也能搜到）。
    pub output_schema: Option<Value>,
    /// 源码语法约束采样，None = 不声明。模型声明支持时（目录 `compat.supportsOpenAIGrammarTools`），
    /// 工具的单个必填 string 参数按裸文本下发；否则退回普通 function 工具。
    pub grammar_sampling: Option<GrammarSampling>,
}

/// 源码语法约束采样：把工具的单个必填 string 参数当裸输入下发（codemode 的 `code`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrammarSampling {
    /// Lark 语法定义（OpenAI `format: { type: "grammar", syntax: "lark" }`）。
    pub openai_lark: Option<String>,
    /// 正则定义（`syntax: "regex"`）；与 lark 同时给出时优先 lark。
    pub openai_regex: Option<String>,
}

impl ExtensionTool {
    /// 简洁构造：name/description/parameters/snippet，其余字段取默认。
    pub fn simple(name: &str, description: &str, parameters: Value, snippet: &str) -> Self {
        ExtensionTool {
            exposure: ToolExposure::Direct,
            namespace: None,
            annotations: ToolAnnotations::default(),
            output_schema: None,
            grammar_sampling: None,
            name: name.to_string(),
            label: None,
            parameters,
            snippet: snippet.to_string(),
            prompt_guidelines: Vec::new(),
            constrained_sampling: false,
            render_shell: None,
            execution_mode: None,
            prepare_arguments: None,
            description: description.to_string(),
        }
    }

    /// 设置暴露方式（链式；构造 [`ExtensionTool`] 字面量时也可直接写字段）。
    pub fn with_exposure(mut self, exposure: ToolExposure) -> Self {
        self.exposure = exposure;
        self
    }

    /// 设置分组名（链式）。
    pub fn with_namespace(mut self, namespace: &str) -> Self {
        self.namespace = Some(namespace.to_string());
        self
    }

    /// 设置工具注解（链式）。
    pub fn with_annotations(mut self, annotations: ToolAnnotations) -> Self {
        self.annotations = annotations;
        self
    }

    /// 设置结构化输出的 JSON Schema（链式）。
    pub fn with_output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// 设置源码语法约束采样（链式）。
    pub fn with_grammar_sampling(mut self, sampling: GrammarSampling) -> Self {
        self.grammar_sampling = Some(sampling);
        self
    }
}

/// 注入给单个 child 的工具：元数据 + handler。
///
/// 与 [`ExtensionTool`] 的区别只在**生命周期与作用域**：扩展工具属于某个扩展、对全部 agent 可见；
/// 注入工具属于一次 `SubAgentSpec`，只在这个 child 的工具表里存在，
/// 并且**不能被 `tools:` 选中、不能被孙代理继承**（不进 `get_all_tools()`）。
pub struct InjectedChildTool {
    /// 工具元数据（name/description/snippet/prompt_guidelines/parameters/
    /// prepare_arguments/execution_mode 都从这里取）
    pub meta: ExtensionTool,
    /// 执行体。同步：注入工具的语义是"记录 / 短路"，不驱动别的执行；
    /// 也不接收 [`ToolExecCtx`]（它不是扩展工具，与 cwd / 子代理调度无关）。
    pub handler: InjectedToolHandler,
}

impl Clone for InjectedChildTool {
    /// 克隆元数据并共享执行体（`handler` 内部是 `Arc`，克隆只增引用计数）。
    fn clone(&self) -> Self {
        InjectedChildTool {
            meta: self.meta.clone(),
            handler: self.handler.clone(),
        }
    }
}

impl std::fmt::Debug for InjectedChildTool {
    /// 只输出工具名，其余字段略去（handler 无法 Debug）。
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InjectedChildTool")
            .field("name", &self.meta.name)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::ToolExposure;

    #[test]
    fn exposure_strum_round_trip() {
        let all = [
            (ToolExposure::Direct, "direct"),
            (ToolExposure::ModelOnly, "model-only"),
            (ToolExposure::Codemode, "codemode"),
            (ToolExposure::Deferred, "deferred"),
            (ToolExposure::Hidden, "hidden"),
        ];
        for (exposure, name) in all {
            assert_eq!(exposure.as_str(), name);
            assert_eq!(ToolExposure::parse(name), Some(exposure));
        }
        assert_eq!(
            ToolExposure::parse("  MODEL-ONLY "),
            Some(ToolExposure::ModelOnly)
        );
        assert_eq!(ToolExposure::parse("nope"), None);
    }
}

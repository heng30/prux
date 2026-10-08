//! App 状态模型：消息历史、流式缓冲、滚动、输入、状态栏。
//!
//! 本文件保留「状态本身 + 事件循环契约」，具体行为按领域分居各处：
//! - 字段类型与事件载荷（`SysMsg` / `MsgLevel` / `Pending*` / `SessionSwitched` …）定义在此；
//! - `new()` / `take_next_action()` / `has_pending()` / `should_repaint()` 是事件循环接口；
//! - 消息流写入 → `handlers::messages`；历史行渲染与缓存 → `render::history`；
//! - agent 事件与 worker 回执 → `handlers::events`；确认面板 → `handlers::confirms`；
//! - 会话修复 / /import 执行 → `handlers::session_repair`；状态栏派生数据 → `handlers::metrics`。
//!
//! 状态驱动渲染：`dirty` 标志决定是否重绘。

use super::{
    editor::Editor,
    handlers::RetryUi,
    overlay::OverlayState,
    panel::Panel,
    render::image::ImageRect,
    render::messages::{BlockId, CachedMsg, EditRenderData, ToolResultView},
    theme::Theme,
};
use crate::{
    core::{
        cache_warmer::WarmStatus, model_scope::ScopedModel, prompt_templates::PromptTemplate,
        provider::AgentMessage, session_manager::SessionRepairPlan,
        settings_manager::HISTORY_MAX_ENTRIES_DEFAULT, skills::Skill,
    },
    modes::interactive::{
        agent_actor::WorkerHandle, ext_settings::ExtSettingsPanel, oauth_flow::OAuthLogin, render,
        render::messages::NestedCallView, session_selector::SessionSelector,
        settings_selector::SettingsSelector, skills_panel::SkillRow,
    },
    utils::{
        self,
        glyphs::{DEF_SPINNER_BRAILLE, DEF_SPINNER_DENSE},
        tokens::TokenMeter,
        wheel::WheelScrollAccelerator,
    },
};
use ratatui::{layout::Rect, text::Line};
use std::{
    collections::{HashMap, VecDeque},
    path::PathBuf,
    sync::{Arc, Mutex, atomic::AtomicBool},
    time::{Duration, Instant},
};

pub use crate::utils::display::{char_width, display_width, truncate_display, wrap_line};

/// 消息渲染行
pub type MsgLine = Line<'static>;

/// 修复完成后的动作（面板确认后按此分发）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepairAction {
    /// 直接打开（ResumeSession）
    Resume,
    /// 修复后 fork 到指定会话目录（--fork 启动路径）
    Fork { dir: Option<PathBuf> },
}

/// 待确认的会话修复请求：`Session::open_checked` 检测到损坏时产生，
/// 由修复面板确认后按计划（抢救/截断）写回并重新打开。
#[derive(Debug, Clone)]
pub struct SessionRepairRequest {
    /// 检测到损坏、待修复的会话文件路径。
    pub path: PathBuf,
    /// 修复计划（档位 + 截断/抢救两条路径的内容）
    pub plan: SessionRepairPlan,
    /// 修复后的动作（Resume / Fork）
    pub action: RepairAction,
}

/// 待确认的"未完成 operation"提示请求：打开会话成功但检测到 ≥2 个未关闭的
/// operation（崩溃残留的僵尸）时产生。面板选项：Continue（默认，只读容忍）/ Rewrite（压平）/ Cancel。
#[derive(Debug, Clone)]
pub struct ZombieOperationsRequest {
    /// 含未关闭 operation、待处置的会话文件路径。
    pub path: PathBuf,
    /// 检测到的未完成 operation 数（≥2）
    pub count: usize,
}

/// /import 确认面板的待处理导入：确认后替换当前会话。
#[derive(Debug, Clone)]
pub struct PendingImport {
    /// 解析后的会话文件路径（~/绝对/相对当前目录）
    pub source: PathBuf,
}

/// /import 会话 cwd 缺失确认面板的待处理恢复：
/// 会话存储的 cwd 不存在，确认是否继续在当前 cwd 恢复。
#[derive(Debug, Clone)]
pub struct PendingImportCwd {
    /// 复制后真正恢复的会话文件路径（会话目录内的副本）
    pub destination: PathBuf,
    /// 会话文件里存储的原 cwd（与当前 cwd 不一致）
    pub session_cwd: String,
    /// true = 原 cwd 不存在；false = 存在但与当前 cwd 不同（决定弹框标题与说明文案）
    pub cwd_missing: bool,
}

/// settings.json 损坏时的覆写确认面板待处理请求：
/// 弹窗选择「Overwrite」后由 settings_manager 强制写入（内容暂存在 core 侧）。
#[derive(Debug, Clone)]
pub struct PendingSettingsOverwrite {
    /// 出问题的 settings.json 路径（弹窗展示）
    pub path: String,
    /// 解析失败原因（弹窗展示）
    pub detail: String,
}

/// `/history @clear` / `@clear-all` 的确认面板待处理请求。
///
/// 删除不可恢复，所以先弹 Yes/No；`detail` 是**爆炸半径**（路径 + 文件数与条目数），
/// 让用户在按下去之前就知道会没掉什么。
#[derive(Debug, Clone)]
pub struct PendingHistoryClear {
    /// true = `@clear-all`（删所有项目的历史文件），false = `@clear`（仅当前项目）
    pub all: bool,
    /// 爆炸半径（弹窗展示）
    pub detail: String,
}

/// `/bug` 交互流程的待处理状态：收集 hint 与两个布尔选择，面板逐级推进。
#[derive(Debug, Clone, Default)]
pub struct PendingBugReport {
    /// `/bug <description>` 的描述（也可能来自 hint 输入框）
    pub hint: String,
    /// 是否附带会话 transcript（jsonl）
    pub include_session: bool,
    /// 不附带 transcript 时是否用当前模型生成摘要
    pub include_summary: bool,
}

/// 系统消息片段：支持按段配色与保留 \n 换行。
/// `fg` 为主题键（如 "text"/"dim"/"success"/"accent"）或 hex（如 "#8abeb7"）；
/// 为 None 时用默认系统色（`dim`）
#[derive(Debug, Clone)]
pub struct SysSpan {
    /// 前景色：主题键（如 "text"/"dim"/"success"）或 hex（如 "#8abeb7"）；
    /// None = 用默认系统色（`dim`）
    pub fg: Option<String>,
    /// 是否加粗
    pub bold: bool,
    /// 片段文本（保留 \n 换行）
    pub text: String,
}

impl SysSpan {
    /// 无颜色、不加粗的纯文本片段（回落成默认系统色）。
    pub fn plain(text: &str) -> Self {
        SysSpan {
            fg: None,
            bold: false,
            text: text.to_string(),
        }
    }

    /// 指定前景色的片段；`key` 为主题键或 hex，不存在的键在渲染时回落默认色。
    pub fn fg(key: &str, text: &str) -> Self {
        SysSpan {
            fg: Some(key.to_string()),
            bold: false,
            text: text.to_string(),
        }
    }

    /// 加粗、不覆盖颜色的片段。
    pub fn bold(text: &str) -> Self {
        SysSpan {
            fg: None,
            bold: true,
            text: text.to_string(),
        }
    }
}

/// 系统消息级别：决定默认渲染色（info→灰、success→绿、warning→黄、error→红）。
/// 级别是消息级属性（一次 push = 一条消息 = 一个级别）；span 的 `fg` 可覆盖级别默认色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgLevel {
    /// 普通信息（灰）
    Info,
    /// 成功（绿）
    Success,
    /// 警告（黄）
    Warning,
    /// 错误（红）
    Error,
}

/// 一条系统消息：级别 + 多段片段（级别决定默认渲染色，span fg 覆盖）
#[derive(Debug, Clone)]
pub struct SysMsg {
    /// 消息级别（决定默认渲染色）
    pub level: MsgLevel,
    /// 消息片段（每段可单独覆盖颜色/加粗）
    pub spans: Vec<SysSpan>,
}

impl SysMsg {
    /// 由级别与片段组装一条消息（片段可逐个覆盖级别默认色）。
    pub fn new(level: MsgLevel, spans: Vec<SysSpan>) -> Self {
        SysMsg { level, spans }
    }

    /// 单段纯文本消息
    pub fn plain(level: MsgLevel, text: &str) -> Self {
        SysMsg::new(level, vec![SysSpan::plain(text)])
    }
}

/// 拼接系统消息全部片段的纯文本
pub fn sys_spans_text(spans: &[SysSpan]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}

/// 拼接一条系统消息的纯文本
pub fn sys_msg_text(msg: &SysMsg) -> String {
    sys_spans_text(&msg.spans)
}

/// 候选列表类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionKind {
    /// `/` 输入的命令候选
    Commands,
    /// `/命令 ` 后输入的二级子命令候选（如 `/goal pause`）
    Subcommands,
    /// `@` 输入的文件候选
    Files,
    /// `/history` 后的 prompt 历史候选（实时过滤；整段替换语义）
    History,
    /// `/history @clear` / `@clear-all` 控制词候选（Enter 执行，Tab 不生效）
    HistoryAction,
}

/// 单个候选条目：名称 + 描述
#[derive(Debug, Clone)]
pub struct SuggestionItem {
    /// 列表显示的名称（相对路径/命令名）
    pub name: String,
    /// 列表显示的描述（命令用途/子命令说明等，可为空）
    pub description: String,
    /// 应用（Enter/Tab）时插入编辑器的文本；文件候选为相对路径
    pub insert: String,
}

/// 异步 @ 文件扫描结果（后台 ignore 遍历完成后回传主循环应用）
#[derive(Debug, Clone)]
pub struct FileScanResult {
    /// 候选类型（@ 文件扫描固定为 `Files`）
    pub kind: SuggestionKind,
    /// 触发锚点（`@`）在光标行内的字符索引
    pub start: usize,
    /// 发起扫描时的过滤词（回传时用于判断是否已过期）
    pub query: String,
    /// 扫描出的候选条目
    pub items: Vec<SuggestionItem>,
}

/// 输入框候选列表状态（/ 命令、子命令、@ 文件）
#[derive(Debug, Clone)]
pub struct Suggestion {
    /// 候选列表是否展开
    pub active: bool,
    /// 当前候选列表类型
    pub kind: SuggestionKind,
    /// 候选条目（已过滤）
    pub items: Vec<SuggestionItem>,
    /// 当前高亮项下标
    pub selected: usize,
    /// 触发锚点（`/`、命令后的空格、或 `@`）在光标行内的字符索引；用于应用时替换
    pub start: usize,
    /// 上次触发时的过滤词（内容未变时保持选中项，避免导航被刷新重置）
    pub query: String,
}

impl Suggestion {
    /// 空的候选列表：未展开、无条目、高亮第 0 项。
    pub fn new() -> Self {
        Suggestion {
            active: false,
            kind: SuggestionKind::Commands,
            items: Vec::new(),
            selected: 0,
            start: 0,
            query: String::new(),
        }
    }

    /// 收起候选列表并清空条目/高亮/过滤词（取消补全时用）。
    pub fn deselect(&mut self) {
        self.active = false;
        self.items.clear();
        self.selected = 0;
        self.query.clear();
    }
}

impl Default for Suggestion {
    /// 与 [`Suggestion::new`] 等价。
    fn default() -> Self {
        Self::new()
    }
}

/// /reload 重载资源所需、无法从运行时恢复的启动参数（CLI 传入）。
/// 重载 skills/prompts/context files 时，
/// 需沿用这些启动决策（显式路径、默认发现开关、信任状态）。
#[derive(Clone, Debug, Default)]
pub struct ReloadCtx {
    /// --skill 显式技能路径（不受信任/过滤门控）
    pub skill_paths: Vec<String>,
    /// --prompt-template 显式模板路径
    pub prompt_paths: Vec<String>,
    /// 是否加载默认技能（--no-skills 取反）
    pub include_default_skills: bool,
    /// 是否加载默认提示模板（--no-prompt-templates 取反）
    pub include_default_prompt_templates: bool,
    /// --no-context-files
    pub no_context_files: bool,
    /// 启动时的项目信任状态
    pub trusted: bool,
    /// 启动参数 -a/--approve / --no-approve 的显式信任覆盖（Some 时不弹信任提示）
    pub trust_override: Option<bool>,
    /// CLI --system-prompt 显式替换系统提示（重载时优先于项目 SYSTEM.md）
    pub cli_system_prompt: Option<String>,
    /// CLI --append-system-prompt 显式追加段（重载时优先于项目 APPEND_SYSTEM.md）
    pub cli_append_system_prompt: Option<String>,
}

/// 忙碌时状态指示器种类：决定状态栏 spinner 帧与基态文案。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum WorkingKind {
    /// 默认工作态
    #[default]
    Working,
    /// /share 会话分享中
    Sharing,
    /// 上下文压缩（/compact 或自动压缩）
    Compaction,
    /// /navigate 带分支摘要
    BranchSummary,
    /// /bug 报告构建（可选摘要生成 + 写归档）
    BugReport,
}

impl WorkingKind {
    /// spinner 帧序列（帧推进 80ms）
    pub fn spinner(self) -> &'static [&'static str] {
        match self {
            WorkingKind::Compaction => DEF_SPINNER_DENSE,
            _ => DEF_SPINNER_BRAILLE,
        }
    }

    /// 忙碌基态文案：status 为空或 2s 临时提示过期时回落的默认文本
    pub fn label(self) -> &'static str {
        match self {
            WorkingKind::Working => "Working...",
            WorkingKind::Sharing => "Sharing session...",
            WorkingKind::Compaction => "Compacting...",
            WorkingKind::BranchSummary => "Summarizing branch...",
            WorkingKind::BugReport => "Building bug report...",
        }
    }
}

/// 交互模式的整体应用状态：消息历史与流式缓冲、滚动与鼠标选择、输入编辑器、
/// 状态栏、各类面板与待确认请求、以及渲染缓存。
/// 事件循环通过 `new()` / `take_next_action()` / `has_pending()` / `should_repaint()`
/// 与它交互，并以 `dirty` 标志驱动重绘。
pub struct App {
    /// /reload 资源重载上下文（CLI 启动参数）
    pub reload_ctx: ReloadCtx,
    /// 渲染历史（含 user/assistant/toolResult/summary）
    pub messages: Vec<AgentMessage>,
    /// 流式缓冲
    pub streaming_text: String,
    /// 流式 thinking 缓冲（reasoning 内容逐块追加）
    pub streaming_thinking: String,
    /// 流式工具调用缓冲（toolCall 逐块追加）
    pub streaming_tools: Vec<String>,
    /// 流式输出 token 计数（tiktoken BPE，按当前模型选词表）；
    /// 未开始流式时为 None，避免启动/建 App 时就解析词表。
    /// 跨模块（handlers/metrics、handlers/events）经 `stream_meter_mut` / 本字段访问，
    /// 故为 crate 可见。
    pub(crate) stream_meter: Option<TokenMeter>,
    /// 输入编辑器
    pub editor: Editor,
    /// 滚动偏移（向上滚动为正值）
    pub scroll: usize,
    /// 最近渲染时的最大滚动量（scroll=max_scroll 即滚动到顶部；渲染时更新）
    pub max_scroll: usize,
    /// 最近渲染时的内容总行数：暂停跟随（scroll>0）时锚定视口内容行用，
    /// 新输出导致 total 增长时同步增大 scroll 抵消，防止视口被往下推。
    pub last_render_total: usize,
    /// 是否正在执行（忙碌态）
    pub busy: bool,
    /// 当前流式回合锚点：assistant message_start 时 st.messages 的长度；
    /// 渲染把流式输出插在该序号对应的历史位置之后（避免后注入的 steer 插到输出前）
    pub stream_anchor_index: Option<usize>,
    /// 忙碌时状态指示器种类（决定 spinner 帧与基态文案）
    pub working_kind: WorkingKind,
    /// 状态栏文本（可多行）
    pub status: String,
    /// 状态栏临时提示截止时间：set_status_msg 设置后到期自动消失
    pub status_deadline: Option<Instant>,
    /// 待提交的消息（Enter 后消费）
    pub status_start: Option<String>,
    /// 请求退出标志（连续 Ctrl+C 或 /quit 置位）
    pub quit_requested: bool,
    /// 当前模型 id（footer/badge 显示）
    pub current_model: Option<String>,
    /// 当前是否无可用模型：兜底默认模型的 provider 未配置凭据，模型实际不可用。
    pub no_model_available: bool,
    /// 模型面板 scope：true=all，false=scoped（Tab 切换）
    pub model_scope_all: bool,
    /// 当前使用的模型所属 provider（与 current_model 配对；
    /// /model 面板勾选与移顶按 provider+model 完整匹配，避免同名模型全部勾选）
    pub current_provider: Option<String>,
    /// 持久化的默认模型 provider（settings.defaultProvider；/model 面板 `· default` 标记用）
    pub default_provider: Option<String>,
    /// 持久化的默认模型 id（settings.defaultModel；/model、/scoped-models 面板 `· default` 标记用）
    pub default_model: Option<String>,
    /// 持久化的默认思考级别（settings.defaultThinkingLevel；/thinking 面板 `· default` 标记用）
    pub default_thinking_level: Option<String>,
    /// 当前模型是否支持 thinking（footer 显示 `model:level` 用；与 current_model 同步更新）
    pub model_reasoning: bool,
    /// 当前 thinking 级别（footer 显示用；None 未设置时按 "off" 显示）
    pub thinking_level: Option<String>,
    /// 虚拟模型最近一次请求路由到的物理模型 id（footer 显示 `→ model`；非虚拟模型为 None）
    pub routed_model: Option<String>,
    /// 路由后实际使用的 thinking 级别（footer 显示 `→ model:level`）
    pub routed_thinking_level: Option<String>,
    /// 当前模型支持的 thinking 级别镜像（worker model_changed 回执更新）
    pub thinking_levels: Vec<String>,
    /// 模型目录刷新状态消息
    pub refresh_status_message: String,
    /// 刷新状态是否成功
    pub refresh_status_success: bool,
    /// 当前主题（渲染样式）
    pub theme: Theme,
    /// 展开全部消息块（shift+enter 切换显示源/详情）
    pub expand_all: bool,
    /// 逐块展开覆盖（块标识 → 是否展开/可见）：鼠标 Alt+点击折叠块切换；
    /// 缺失时回退全局（thinking 跟随 `show_thinking`，摘要/skill/工具块跟随 `expand_all`）
    pub expand_overrides: HashMap<BlockId, bool>,
    /// 界面脏标记：置位后下一帧重绘
    pub dirty: bool,
    /// 外部编辑器返回后需要终端全量重绘（clear + 重置 ratatui previous buffer）
    pub full_redraw: bool,
    /// /compact 请求待处理（轮结束/reset 时执行压缩）
    pub pending_compact: bool,
    /// /compact 自定义摘要指令
    pub compact_instructions: Option<String>,
    /// 待执行的回退请求是否把被回退的用户输入回填输入框（与 `AgentCommand::Rewind` 配对，回执后复位）
    pub pending_rewind_restore: bool,
    /// 最近一次终端 resize 的时间（节流触发）
    pub last_resize: Instant,
    /// 启动时等待用户确认信任（ask-trust 流程）
    pub ask_trust: bool,
    /// 启动时注入的初始消息（/resume 等恢复场景）
    pub initial_queue: VecDeque<AgentMessage>,
    /// banner 已隐藏（用户首次提交输入后永久置位；隐藏后不再恢复，本进程内）
    pub banner_hidden: bool,
    /// 提示模板列表（/ 命令候选与补全）
    pub prompt_templates: Vec<PromptTemplate>,
    /// 已发现技能（/skill:name 命令候选 + 展开）
    pub skills: Vec<Skill>,
    /// 待执行的树导航 (目标 entry_id, 是否生成 branch_summary)
    pub pending_navigate: Option<(String, bool)>,
    /// 无可用主题（/theme 面板空态提示）
    pub no_themes: bool,
    /// --models / enabledModels 解析出的 scope 快照（Ctrl+P 循环，有序，含 thinking）
    pub model_cycle: Vec<ScopedModel>,
    /// scoped-models 面板启用名单（None = 全部启用；Some = 有序 canonical provider/id，不可解析 pattern 以 \0 前缀形式存于其中）
    pub model_scope_enabled: Option<Vec<String>>,
    /// scoped-models 面板未保存改动标记（footer 显示 unsaved）
    pub model_scope_dirty: bool,
    /// 输入框候选列表（/ 命令、@ 文件）
    pub suggestion: Suggestion,
    /// 上次刷新候选列表时的编辑器内容版本：版本未变（纯光标移动/翻页）不触发刷新
    pub last_suggestion_version: u64,
    /// 进行中的 @ 文件扫描中止开关：新扫描接管或候选关闭时置位，让旧遍历尽快退出
    pub file_scan_cancel: Option<Arc<AtomicBool>>,
    /// 模态选择面板
    pub panel: Panel,
    /// 待用户确认的会话修复请求（损坏检测后面板确认，None=无）
    pub pending_session_repair: Option<SessionRepairRequest>,
    /// 待用户确认的"未完成 operation"请求（僵尸 op 检测后面板确认，None=无）
    pub pending_zombie_operations: Option<ZombieOperationsRequest>,
    /// 待用户确认的 /import 请求（确认后替换当前会话，None=无）
    pub pending_import: Option<PendingImport>,
    /// 待用户确认的 /import 会话 cwd 缺失请求（确认后继续在当前 cwd 恢复，None=无）
    pub pending_import_cwd: Option<PendingImportCwd>,
    /// 待用户确认的 settings.json 覆写请求（None=无）
    pub pending_settings_overwrite: Option<PendingSettingsOverwrite>,
    /// 待用户确认的 `/history @clear` / `@clear-all` 请求（None=无）
    pub pending_history_clear: Option<PendingHistoryClear>,
    /// 进行中的 `/bug` 流程状态（None=无）
    pub pending_bug_report: Option<PendingBugReport>,
    /// 系统消息（显示在消息流，不进 LLM 上下文；带时间戳与级别，渲染时与 LLM 消息按时间线合并）
    pub system_messages: Vec<(u64, SysMsg)>,
    /// 消息区重试提示状态（auto_retry_start/end 事件驱动；None = 无进行中重试）
    pub retry_ui: Option<RetryUi>,
    /// 上一次重试失败提示行的时间戳：下一次 start 覆盖它（重试之间不堆积失败行）
    pub retry_failed_ts: Option<u64>,
    /// 工作目录（footer 扩展展示用）
    pub cwd: String,
    /// 当前模型上下文窗口（token，context 使用率计算用）
    pub context_window: u32,
    /// 上下文使用率（0-100，message_end/turn_end 时刷新）
    pub context_percent: f64,
    /// 压缩后的上下文 token 覆盖值（`compaction_end` 的 `estimatedTokensAfter`）：
    /// 压缩后上下文 = 摘要 + 保留尾部，摘要不带 usage、旧 assistant 的 usage 已是
    /// 压缩前的陈旧值，因此用它在压缩后立即给出正确的窗口占用。
    /// 下一条带可用 usage 的 assistant 落地（那时 usage 已是压缩后的真实值）、
    /// 或消息流被整体替换（切会话 / 树导航 / /new）时清除。
    pub context_tokens_override: Option<u64>,
    /// 当前 git 分支（渲染时刷新，变化触发重绘）
    pub git_branch: Option<String>,
    /// 鼠标选择状态（消息区拖选文本）
    pub mouse: MouseSel,
    /// 进行中工具的启动墙钟毫秒（toolCallId → ms；渲染 `Elapsed X.Xs` 实时计时）
    pub tool_started_at: HashMap<String, u64>,
    /// 上次强制重建含 Elapsed 计时块的时刻（1s 节流）
    pub last_elapsed_rebuild: Option<u64>,
    /// 上次 Ctrl+C 时间（连续两次退出）
    pub last_ctrl_c: Option<Instant>,
    /// kill-ring 缓冲（Ctrl+U/K/W 删除文本，Ctrl+Y 粘贴）
    pub kill_buffer: String,
    /// kill-ring 历史（Ctrl+Y 循环粘贴）
    pub kill_ring: Vec<String>,
    /// 当前 yank 位置（Ctrl+Y 在历史间循环）
    pub yank_index: usize,
    /// 上次 kill 是否词删除（连续词 kill 累积 prepend/append）
    pub last_kill_was_word: bool,
    /// 隐藏思考块
    pub show_thinking: bool,
    /// 内联显示图片（settings.json `showImages`，默认关闭）：
    /// 开启且终端支持图形协议时，消息与工具结果里的图片按单元格绘制；
    /// 关闭时图片块整块不渲染。
    pub show_images: bool,
    /// 最近一帧收集到的图片绘制区域（消息区渲染时重建，帧末尾绘制）
    pub image_rects: Vec<ImageRect>,
    /// 当前会话文件路径（session 面板标记当前会话用）
    pub current_session_path: Option<String>,
    /// /login 进行中选中的 provider id（LoginKey 面板保存时用）
    pub login_provider_id: String,
    /// /login 进行中选中的 provider 显示名（LoginKey 标题用）
    pub login_provider_name: String,
    /// /login 进行中选中的认证类型（"api_key" / "oauth"，LoginProvider 面板分流用）
    pub login_auth_type: String,
    /// /login OAuth 流程状态（活跃时事件循环 tick 轮询回调）
    pub oauth_login: Option<OAuthLogin>,
    /// 面板渲染区域（鼠标 Ctrl+click 打开 OAuth 授权 URL 用）
    pub panel_area: Option<Rect>,
    /// /extension 二级详情面板的 fork 项目 URL（有值时 Ctrl+click 打开浏览器）
    pub extension_detail_url: Option<String>,
    /// 停靠面板（状态栏上方可滚动面板）是否可见（/dock 切换或 ShowDock 请求；
    /// 渲染时仍需任一扩展提供非空内容，否则自动隐藏）
    pub dock_visible: bool,
    /// 停靠面板滚动偏移（行；整块平铺内容的视口起点，滚轮命中 dock_area 时改动）
    pub dock_offset: usize,
    /// 最近一帧停靠面板的渲染区域（滚轮命中测试用；隐藏/无内容时为 None）
    pub dock_area: Option<Rect>,
    /// 扩展覆盖层（输入区上方导航面板 / 全屏查看器）。内容由扩展持有，核心只持快照。
    pub overlay: Option<OverlayState>,
    /// /extension 二级菜单详情行（ExtensionDetail 面板渲染）
    pub extension_detail: Vec<String>,
    /// 扩展启用状态被切换过（面板关闭时重建 Agent 工具列表）
    pub extensions_changed: bool,
    /// /extension 面板未保存的启用状态草稿（name → 期望 enabled，按插入顺序）。
    /// 空格 / Ctrl+A / Ctrl+X 只写草稿（面板勾选即时可见，底栏/主题/工具链不动），
    /// Ctrl+S 才应用到注册表并写盘，关闭面板丢弃。非空即面板显示 `(unsaved)`。
    pub extension_draft: Vec<(String, bool)>,
    /// /skills 面板的行数据（与面板 items 同序，按技能名索引）。
    pub skill_rows: Vec<SkillRow>,
    /// /skills 面板未保存的启用状态草稿（技能名 → 期望 enabled，按插入顺序）。
    /// 空格只写草稿（勾选即时可见），Ctrl+S 才写 `settings.json` 并落盘扩展技能，关闭丢弃。
    pub skill_draft: Vec<(String, bool)>,
    /// /skills 二级详情面板的行（SkillDetail 渲染：frontmatter 头信息 + 来源 + 路径）。
    pub skill_detail: Vec<String>,
    /// /skills 面板最近一次应用失败的原因（渲染在面板提示行）。
    pub skill_panel_error: Option<String>,
    /// TUI 本地 follow-up 队列（busy 时 Alt+Enter 入队，轮结束/中断后取回）
    pub runtime_followup_inbox: VecDeque<String>,
    /// busy 时 Enter 的 steer 走这里，run_loop 在 tool call 后中途注入；
    /// 渲染读它显示未注入的 Steering 待发送。
    /// 与 runtime_followup_inbox（仅 follow-up）分离：steer 运行中注入，follow-up settle 后发送。
    pub runtime_steer_inbox: Arc<Mutex<VecDeque<AgentMessage>>>,
    /// 待执行的远程目录刷新队列（启动缺失项 / /login 成功后入队）
    pub pending_refresh: VecDeque<String>,
    /// agent actor：UI → worker 命令句柄。闭包内可直接 `ui.worker.send(...)` 发命令。
    pub worker: WorkerHandle,
    /// /resume、/session 会话选择器
    pub session_selector: SessionSelector,
    /// /settings 设置选择器
    pub settings_selector: SettingsSelector,
    /// 扩展设置面板（占据输入框区域）：Space/Enter 循环切换扩展设置
    pub ext_settings: ExtSettingsPanel,
    /// /settings Autocomplete max items：候选列表最大展示行数
    pub autocomplete_max_visible: usize,
    /// /settings Wheel scroll lines：滚轮事件 → 行数（auto = 按速度加速）
    pub wheel_accel: WheelScrollAccelerator,
    /// 滚轮加速器的时间基准（单调时钟，启动时固定）
    pub wheel_epoch: Instant,
    /// /settings History max entries：prompt 历史保留条数（0 = 不落盘）。
    /// 启动时读设置；提交时用于追加落盘与内存截断。
    pub history_max_entries: usize,
    /// 消息级渲染缓存：key = (timestamp, timeline seq)，命中缓存时零重渲染
    pub msg_cache: HashMap<(u64, usize), CachedMsg>,
    /// edit 工具 call 阶段预览 diff 的 memo：toolCallId → 预览 diff（None=计算失败）
    pub edit_previews: HashMap<String, Option<String>>,
    /// 跨帧派生的渲染输入指纹（消息/系统提示集）；宽度/主题/展开态变化不改变它
    pub derived_fp: Option<u64>,
    /// 进行中工具的嵌套调用（codemode 脚本调起的工具，toolCallId → 行）：来自
    /// `tool_execution_start/end`（带 `parentToolCallId` 的事件），渲染时注入父工具块。
    pub nested_calls: HashMap<String, Vec<NestedCallView>>,
    /// 工具结果表（toolCallId → (文本, 是否错误, 耗时)）：按 [`App::derived_fp`] 缓存，
    /// 否则每帧都要克隆全部工具输出
    pub derived_tool_results: HashMap<String, ToolResultView>,
    /// edit 渲染表（result_diff / preview）：同样按 [`App::derived_fp`] 缓存
    pub derived_edit_map: HashMap<String, EditRenderData>,
    /// 缓存对应的渲染参数（变化时整体失效）
    pub cache_width: usize,
    /// 缓存对应的全局展开态（`expand_all`）
    pub cache_expand_all: bool,
    /// 缓存对应的思考块可见态（`show_thinking`）
    pub cache_show_thinking: bool,
    /// 缓存对应的图片显示开关与图片布局（`show_images` + 单元格像素尺寸）
    pub cache_images_fp: (bool, Option<(u16, u16)>),
    /// 缓存对应的主题指纹（/theme 切换或主题文件重载后变化 → 整体失效）
    pub cache_theme_fp: String,
    /// 最近一帧滚动条是否存在（决定首选渲染宽度，避免宽度抖动反复重建缓存）
    pub last_has_scrollbar: bool,
    /// 恢复会话启动：历史缓存为 Fast（无语法高亮）版本，后台补全进行中
    pub highlight_backlog: bool,
    /// 补全线程已启动（防重复 spawn；宽度/消息变化丢弃结果时复位重试）
    pub highlight_started: bool,
    /// 最近一次补全快照的递增 ID（切换会话/重新快照后，旧线程结果按 ID 丢弃）
    pub highlight_snap_id: u64,
}

/// 消息区鼠标选择状态
#[derive(Debug, Clone, Default)]
pub struct MouseSel {
    /// 最近一帧消息区的位置（命中测试用）
    pub log_area: u16,
    /// 最近一帧消息区渲染的可见行文本（行 = visible_lines 索引）
    pub visible: Vec<String>,
    /// 选择区间：(起始行, 起始字符, 结束行, 结束字符)
    /// 行号为**内容全局行**（`view_start + visible 行`），
    /// 滚动/流式追加时稳定，选中钉在真实文本上而非视口上。
    pub sel: Option<(usize, usize, usize, usize)>,
    /// 最近一帧视口顶行的内容全局行号（visible[i] ↔ 内容行 view_start+i）
    pub view_start: usize,
    /// 最近一帧消息区渲染宽度（选中文本重建用；滚动条存在/缺失时宽度不同）
    pub view_width: usize,
    /// 拖拽进行中
    pub dragging: bool,
    /// 滚动条所在列（渲染时记录；无滚动条为 None）
    pub scroll_x: Option<u16>,
    /// 最近一帧消息区高度（滚动条拖动比例计算用）
    pub log_area_h: u16,
    /// 最近一帧消息区内容总行数（滚动条拖动比例计算用）
    pub log_total: usize,
    /// 滚动条拖拽中（按下位于滚动条列）
    pub scroll_dragging: bool,
    /// 最近一帧滚动条 thumb 在 track 内的相对区间 (start, len)（抓取命中判断用）
    pub scroll_thumb: Option<(usize, usize)>,
    /// 滚动条按下基准：(按下 row, 按下时 scroll)（抓取偏移拖动用）
    pub scroll_press: Option<(u16, usize)>,
    /// 最近一次拖拽事件的屏幕行（边缘持续自动滚动 tick 用）
    pub drag_row: Option<u16>,
    /// 指针停留在视口边缘的 tick 计数（满 `EDGE_DWELL_TICKS` 才开始自动滚动，减少误触）
    pub edge_dwell: u8,
    /// 最近一帧可点击折叠条目的屏幕行命中区（渲染时记录，每帧重建）
    pub click_targets: Vec<ClickRow>,
    /// 最近一帧可点击链接的屏幕区域（渲染时记录，每帧重建）
    pub links: Vec<LinkRow>,
}

/// 可点击折叠块在屏幕上的命中区：行范围 `[start, end)`（半开）+ 块标识
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClickRow {
    /// 命中区起始屏幕行（含）
    pub start: u16,
    /// 命中区结束屏幕行（不含）
    pub end: u16,
    /// 被点击的块标识
    pub id: BlockId,
    /// 缺省展开态（点击切换：`cur = override.unwrap_or(default)`）
    pub default_expanded: bool,
}

/// 消息区里一个可点击链接在屏幕上的命中区（列区间半开）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkRow {
    /// 所在屏幕行（绝对行号）。
    pub row: u16,
    /// 起始屏幕列（含）。
    pub col_start: u16,
    /// 结束屏幕列（不含）。
    pub col_end: u16,
    /// 点击时要打开的 URL（已含 `mailto:` 等修饰）。
    pub url: String,
}

impl MouseSel {
    /// 命中测试：屏幕 (col,row) → 内容全局行号 + 字符偏移（消息区内）
    pub fn hit(&self, col: u16, row: u16, area_y: u16, area_h: u16) -> Option<(usize, usize)> {
        if row < area_y || row >= area_y + area_h {
            return None;
        }
        let line = (row - area_y) as usize;
        let text = self.visible.get(line)?;
        let mut w = 0usize; // 列 → 字符偏移（按显示宽度，含 CJK/emoji）

        for (i, c) in text.chars().enumerate() {
            let cw = utils::display::char_width(c);
            if col as usize <= w {
                return Some((self.view_start + line, i));
            }
            w += cw;
        }
        Some((self.view_start + line, text.chars().count()))
    }

    /// 点击命中测试：屏幕行落在某个折叠块命中区内 → 返回该命中区
    pub fn click_target_at(&self, row: u16) -> Option<ClickRow> {
        self.click_targets
            .iter()
            .find(|t| row >= t.start && row < t.end)
            .copied()
    }

    /// 链接命中测试：屏幕 (col,row) 落在某个链接的列区间内 → 返回该链接。
    pub fn link_at(&self, col: u16, row: u16) -> Option<&LinkRow> {
        self.links
            .iter()
            .find(|l| l.row == row && col >= l.col_start && col < l.col_end)
    }

    /// 拖拽时更新选择终点（起始点为按下位置，保持不动；参数为内容全局坐标）
    pub fn set_drag_point(&mut self, line: usize, ch: usize) {
        if let Some((ls, cs, _, _)) = self.sel {
            self.sel = Some((ls, cs, line, ch));
        }
    }
}

/// 按全局行 g 与选中区间 [ls..=le] 提取该行片段到 out，返回是否写入了内容。
/// `ce` 允许为 `usize::MAX`（边缘下滚时终点钉行尾，行长度未知），由内层 min 钳制。
fn append_sel_line(
    out: &mut String,
    line: &MsgLine,
    g: usize,
    ls: usize,
    cs: usize,
    le: usize,
    ce: usize,
) -> bool {
    if g < ls || g > le {
        return false;
    }
    let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let chars: Vec<char> = text.chars().collect();
    let a = if g == ls { cs.min(chars.len()) } else { 0 };
    let b = if g == le {
        ce.min(chars.len())
    } else {
        chars.len()
    };
    if a >= b {
        return false;
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.extend(&chars[a..b]);
    true
}

impl App {
    /// 状态栏临时提示默认停留时长
    pub const STATUS_MSG_TTL: Duration = Duration::from_secs(2);

    /// 构造全空的初始界面状态：无消息、无流式输出、空编辑器、默认主题，
    /// 各种待处理请求/面板均为 None；`dirty = true` 以便首帧必绘。
    pub fn new() -> Self {
        App {
            reload_ctx: ReloadCtx::default(),
            messages: Vec::new(),
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            streaming_tools: Vec::new(),
            stream_meter: None,
            editor: Editor::new(),
            scroll: 0,
            max_scroll: 0,
            last_render_total: 0,
            busy: false,
            stream_anchor_index: None,
            working_kind: WorkingKind::Working,
            status: String::new(),
            status_deadline: None,
            status_start: None,
            quit_requested: false,
            current_model: None,
            no_model_available: false,
            model_scope_all: true,
            current_provider: None,
            default_provider: None,
            default_model: None,
            default_thinking_level: None,
            model_reasoning: false,
            thinking_level: None,
            routed_model: None,
            routed_thinking_level: None,
            thinking_levels: Vec::new(),
            refresh_status_message: String::new(),
            refresh_status_success: false,
            theme: Theme::default(),
            expand_all: false,
            expand_overrides: HashMap::new(),
            dirty: true,
            full_redraw: false,
            pending_compact: false,
            compact_instructions: None,
            pending_rewind_restore: false,
            last_resize: Instant::now(),
            ask_trust: false,
            initial_queue: VecDeque::new(),
            banner_hidden: false,
            prompt_templates: Vec::new(),
            skills: Vec::new(),
            pending_navigate: None,
            no_themes: false,
            model_cycle: Vec::new(),
            model_scope_enabled: None,
            model_scope_dirty: false,
            suggestion: Suggestion::new(),
            last_suggestion_version: 0,
            file_scan_cancel: None,
            panel: Panel::new(),
            system_messages: Vec::new(),
            retry_ui: None,
            retry_failed_ts: None,
            cwd: String::new(),
            context_window: 0,
            context_percent: 0.0,
            context_tokens_override: None,
            git_branch: None,
            mouse: MouseSel::default(),
            tool_started_at: HashMap::new(),
            last_elapsed_rebuild: None,
            last_ctrl_c: None,
            kill_buffer: String::new(),
            kill_ring: Vec::new(),
            yank_index: 0,
            last_kill_was_word: false,
            show_thinking: true,
            show_images: false,
            image_rects: Vec::new(),
            current_session_path: None,
            login_provider_id: String::new(),
            login_provider_name: String::new(),
            login_auth_type: String::new(),
            oauth_login: None,
            panel_area: None,
            extension_detail_url: None,
            dock_visible: false,
            overlay: None,
            dock_offset: 0,
            dock_area: None,
            pending_session_repair: None,
            pending_zombie_operations: None,
            pending_import: None,
            pending_import_cwd: None,
            pending_settings_overwrite: None,
            pending_history_clear: None,
            pending_bug_report: None,
            extension_detail: Vec::new(),
            extensions_changed: false,
            extension_draft: Vec::new(),
            skill_rows: Vec::new(),
            skill_draft: Vec::new(),
            skill_detail: Vec::new(),
            skill_panel_error: None,
            runtime_followup_inbox: VecDeque::new(),
            runtime_steer_inbox: Arc::new(Mutex::new(VecDeque::new())),
            pending_refresh: VecDeque::new(),
            worker: WorkerHandle::null(),
            session_selector: SessionSelector::new(),
            settings_selector: SettingsSelector::new(),
            ext_settings: ExtSettingsPanel::new(),
            autocomplete_max_visible: 5,
            wheel_accel: WheelScrollAccelerator::default(),
            wheel_epoch: Instant::now(),
            history_max_entries: HISTORY_MAX_ENTRIES_DEFAULT,
            msg_cache: HashMap::new(),
            edit_previews: HashMap::new(),
            derived_fp: None,
            nested_calls: HashMap::new(),
            derived_tool_results: HashMap::new(),
            derived_edit_map: HashMap::new(),
            cache_width: 0,
            cache_expand_all: false,
            cache_show_thinking: true,
            cache_images_fp: (false, None),
            cache_theme_fp: String::new(),
            last_has_scrollbar: false,
            highlight_backlog: false,
            highlight_started: false,
            highlight_snap_id: 0,
        }
    }

    /// 提取选中文本：按内容全局行区间重建（支持跨屏/跨视口选择与反向拖拽）。
    /// 选中范围可能超出当前视口（滚动/边缘自动滚动后），因此不能用 `mouse.visible`，
    /// 而是以最近一帧的渲染宽度重新渲染历史 + 流式段，按全局行号切片。
    pub fn selected_text(&mut self) -> Option<String> {
        let (ls, cs, le, ce) = self.mouse.sel?;
        let (ls, cs, le, ce) = if (ls, cs) <= (le, ce) {
            (ls, cs, le, ce)
        } else {
            (le, ce, ls, cs)
        };

        let width = self.mouse.view_width.max(1);
        let hist = self.render_history_lines(width);
        let mut out = String::new();

        for (seg_start, lines, _) in &hist.segments {
            let seg_end = seg_start + lines.len();
            if seg_end <= ls || *seg_start > le {
                continue;
            }
            for (i, line) in lines.iter().enumerate() {
                append_sel_line(&mut out, line, seg_start + i, ls, cs, le, ce);
            }
        }

        // 流式段：选中范围越过历史行数时，用当前流式状态尽力补充（边缘场景）
        if le >= hist.total {
            let mut pending = hist.pending.clone();
            let stream = render::messages::render_streaming(
                &self.streaming_thinking,
                &self.streaming_tools,
                &self.streaming_text,
                width,
                &self.theme,
                self.expand_all,
                self.show_thinking,
                &mut pending,
                &self.tool_started_at,
                &mut Vec::new(),
            );
            for (i, line) in stream.iter().enumerate() {
                append_sel_line(&mut out, line, hist.total + i, ls, cs, le, ce);
            }
        }

        if out.is_empty() { None } else { Some(out) }
    }

    /// 是否应重绘
    pub fn should_repaint(&self) -> bool {
        self.dirty || self.busy
    }

    /// 是否有待执行任务
    pub fn has_pending(&self) -> bool {
        self.status_start.is_some()
            || self.pending_compact
            || self.pending_navigate.is_some()
            || !self.initial_queue.is_empty()
    }

    /// 取出待执行动作（空闲时由事件循环调用）
    pub fn take_next_action(&mut self) -> NextAction {
        if let Some(provider) = self.pending_refresh.pop_front() {
            return NextAction::RefreshModels(provider);
        }
        if self.pending_compact {
            self.pending_compact = false;
            let instructions = self.compact_instructions.take();
            return NextAction::Compact(instructions);
        }
        if let Some((target, summarize)) = self.pending_navigate.take() {
            return NextAction::Navigate(target, summarize);
        }
        if let Some(text) = self.status_start.take() {
            return NextAction::Prompt(text);
        }
        if !self.ask_trust
            && let Some(msg) = self.initial_queue.pop_front()
        {
            return NextAction::PromptMessage(Box::new(msg));
        }
        NextAction::None
    }
}

impl Default for App {
    /// 与 [`App::new`] 等价。
    fn default() -> Self {
        Self::new()
    }
}

/// 空闲时的事件循环动作
#[derive(Debug)]
pub enum NextAction {
    /// 无待执行动作
    None,
    /// 手动压缩；携带 /compact [instructions] 自定义摘要指令
    Compact(Option<String>),
    /// 会话树导航：(目标 entry_id, 是否生成 branch_summary)
    Navigate(String, bool),
    /// 提交用户输入（进入正常对话回合）
    Prompt(String),
    /// 直接注入一条消息（/resume 等恢复场景的初始消息）
    PromptMessage(Box<AgentMessage>),
    /// 后台刷新某 provider 的远程模型目录
    RefreshModels(String),
}

/// /model 切换回执载荷
pub struct ModelChanged {
    /// 模型所属 provider id
    pub provider: String,
    /// 模型 id
    pub model: String,
    /// 模型显示名
    pub name: String,
    /// 该模型是否支持 thinking
    pub reasoning: bool,
    /// 切换后的 thinking 级别（None = 未设置，按 "off" 处理）
    pub thinking_level: Option<String>,
    /// 该模型支持的 thinking 级别列表
    pub thinking_levels: Vec<String>,
    /// 本次切换是否由 Ctrl+S 触发并已写盘为默认模型
    pub persisted: bool,
}

/// /new、/resume 会话切换回执载荷
pub struct SessionSwitched {
    /// 新会话 id
    pub session_id: String,
    /// 会话名（None = 未命名）
    pub name: Option<String>,
    /// 会话文件路径（None = 全新会话，不提示 "Resumed"）
    pub file: Option<String>,
    /// 切换后的消息历史
    pub messages: Vec<AgentMessage>,
}

/// 会话树导航回执载荷
pub struct NavigateDone {
    /// 导航后回填输入框的文本（None = 不回填）
    pub editor_text: Option<String>,
    /// 命中分支的消息历史（空 = 无变化，不替换消息流）
    pub messages: Vec<AgentMessage>,
}

/// `/rewind` 回退最后一次用户输入的回执载荷
pub struct RewindDone {
    /// 被回退的用户输入文本；`None` = 分支上没有 user 消息（无变化）
    pub removed_input: Option<String>,
    /// 回退后的消息历史（来自会话投影；回退第一条 user 消息时可为空）
    pub messages: Vec<AgentMessage>,
}

/// /login 应用凭据回执载荷
pub struct AppliedKey {
    /// fallback 模型时自动选中该 provider 默认模型：携带新模型信息以同步 UI 镜像
    /// （None = 未改选模型，保持当前模型）
    pub applied: Option<ModelChanged>,
}

/// /session 成本分列的一项：按 `provider/model` 归集，
/// 无模型归属的用量（嵌套工具调用、摘要生成）统一进 `Tools/summaries` 桶。
#[derive(Debug)]
pub struct CostBreakdownEntry {
    /// 归集键（`provider/model`，或无归属用量桶 `Tools/summaries`）
    pub key: String,
    /// 该项累计成本
    pub cost: f64,
    /// 该项累计 token 数
    pub tokens: u64,
}

/// /session 统计载荷（原始数值，格式化留在 UI 侧）
pub struct SessionStats {
    /// 会话名（None = 未命名）
    pub name: Option<String>,
    /// 会话文件路径（无会话文件时为 "In-memory"）
    pub file: String,
    /// 会话 id（无会话时为 "—"）
    pub id: String,
    /// 消息总数（user + assistant + toolResult）
    pub messages_total: u64,
    /// 工具调用次数
    pub tool_calls: usize,
    /// 提示侧 token 合计（input + cache_read + cache_write）
    pub prompt_tokens: u64,
    /// 全部 token 合计（input + output + cache_read + cache_write）
    pub total_tokens: u64,
    /// 未命中缓存的输入 token
    pub input: u64,
    /// 输出 token
    pub output: u64,
    /// 缓存读取 token
    pub cache_read: u64,
    /// 缓存写入 token
    pub cache_write: u64,
    /// 累计成本（provider 计价口径，合计与 `cost_breakdown` 一致）
    pub cost_total: f64,
    /// 成本分列（成本降序；合计与 `cost_total` 一致）
    pub cost_breakdown: Vec<CostBreakdownEntry>,
    /// 提示缓存保温状态（`/session` 的 `Cache Warming` 块）
    pub cache_warming: WarmStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_instructions_flow_to_next_action() {
        // /compact [instructions]：指令经 take_next_action 透传（对齐 pi compact(customInstructions)）
        let mut app = App {
            pending_compact: true,
            compact_instructions: Some("focus on API changes".to_string()),
            ..Default::default()
        };
        match app.take_next_action() {
            NextAction::Compact(Some(i)) => assert_eq!(i, "focus on API changes"),
            other => panic!("expected Compact(Some), got {:?}", other),
        }
        // 无指令 → Compact(None)
        app.pending_compact = true;
        app.compact_instructions = None;
        match app.take_next_action() {
            NextAction::Compact(None) => {}
            other => panic!("expected Compact(None), got {:?}", other),
        }
    }

    #[test]
    fn wrap_line_respects_display_width() {
        // full-width chars count as 2 columns
        let lines = wrap_line("ａｂｃｄabcdef", 6);
        assert!(lines.len() >= 2);
        assert!(lines[0].chars().all(|c| c == 'ａ'
            || c == 'ｂ'
            || c == 'ｃ'
            || c == 'ｄ'
            || c == 'a'
            || c == 'b'
            || c == 'c'
            || c == 'd'
            || c == 'e'
            || c == 'f'));
    }

    #[test]
    fn hit_maps_visible_row_to_content_line() {
        // hit 返回**内容全局行号**：视口行 + view_start；滚动/流式后选中钉在内容上
        // （回归 bug：此前返回 visible 行索引，滚动后选中跟着视口漂移）
        let mut app = App::new();
        app.mouse.visible = vec![
            "line one".to_string(),
            "line two".to_string(),
            "line three".to_string(),
        ];
        app.mouse.view_start = 7; // 视口顶行 = 内容行 7
        let (l0, c0) = app.mouse.hit(2, 0, 0, 3).unwrap();
        assert_eq!((l0, c0), (7, 2), "视口行 0 col2 → 内容行 7 'n'");
        let (l1, c1) = app.mouse.hit(6, 2, 0, 3).unwrap();
        assert_eq!((l1, c1), (9, 6), "视口行 2 col6 → 内容行 9 't'");
        // 行尾命中：列超宽 → 行尾字符偏移（内容行 8 = visible 行 1）
        let (l2, c2) = app.mouse.hit(99, 1, 0, 3).unwrap();
        assert_eq!((l2, c2), (8, 8), "超宽列 → 行尾: 'line two' 8 chars");
        // 消息区外点击：命中失败应清除选择（对齐 tact outside click clears）
        assert!(app.mouse.hit(2, 30, 0, 3).is_none());
    }
}

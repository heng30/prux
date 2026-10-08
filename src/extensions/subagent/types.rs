//! 子代理扩展的数据类型。

use super::{
    config::SubagentConfig,
    manager::GRACE_TURNS,
    model::ModelSource,
    notify,
    worktree::{Worktree, WorktreeResult},
};
use crate::{
    core::extensions::InjectedChildTool,
    extensions::util::truncate_chars,
    utils::{
        glyphs::{DEF_DONE, DEF_FAILED, DEF_QUEUED, DEF_SPINNER_BRAILLE, DEF_STOPPED},
        time::now_ms,
    },
};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};
use strum_macros::{EnumString, IntoStaticStr};
use tokio::sync::watch::Receiver;

/// 代理状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum AgentStatus {
    /// 已入队，等待后台额度
    Queued,
    /// 正在运行
    Running,
    /// 自然结束
    Completed,
    /// 到达 max_turns 但在宽限轮内收尾成功
    Steered,
    /// 到达 max_turns 且宽限期用尽被停机
    Aborted,
    /// 用户主动中止
    Stopped,
    /// 执行出错
    Error,
}

impl AgentStatus {
    /// 状态的规范小写名（`queued` / `running` / `error` …），由 strum 派生。
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 状态图标（通知与 `/agents` 列表）。
    pub fn icon(self) -> &'static str {
        match self {
            AgentStatus::Queued => DEF_QUEUED,
            AgentStatus::Stopped => DEF_STOPPED,
            AgentStatus::Running => DEF_SPINNER_BRAILLE[0],
            AgentStatus::Completed | AgentStatus::Steered => DEF_DONE,
            AgentStatus::Aborted | AgentStatus::Error => DEF_FAILED,
        }
    }

    /// 是否已终结（可以移除/可 resume）。
    pub fn is_terminal(self) -> bool {
        !matches!(self, AgentStatus::Queued | AgentStatus::Running)
    }

    /// 状态配色（主题键 + 缺省色）：widget 与结果卡片共用。
    pub fn color(self) -> (&'static str, &'static str) {
        match self {
            AgentStatus::Running => ("accent", "#8abeb7"),
            AgentStatus::Completed | AgentStatus::Steered => ("success", "#b5bd68"),
            AgentStatus::Queued => ("muted", "#999999"),
            AgentStatus::Aborted | AgentStatus::Error => ("error", "#cc6666"),
            AgentStatus::Stopped => ("warning", "#f0c674"),
        }
    }
}

/// 类型定义来源（`/agents types` 展示）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum AgentSource {
    /// 扩展内嵌默认类型
    Builtin,
    /// `<cwd>/PROJECT_SCOPE_NAME/agents/`
    Project,
    /// `<agent_dir()>/extensions/agent/`
    Global,
}

impl AgentSource {
    /// 定义来源的规范小写名（`builtin` / `project` / `global`）。
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// 持久记忆作用域
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum MemoryScope {
    /// `<agent_dir()>/agent-memory/<name>/`
    User,
    /// `<cwd>/PROJECT_SCOPE_NAME/agent-memory/<name>/`（需项目受信任）
    Project,
    /// `<cwd>/PROJECT_SCOPE_NAME/agent-memory-local/<name>/`（需项目受信任）
    Local,
}

impl MemoryScope {
    /// 记忆作用域的规范小写名（`user` / `project` / `local`）。
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析作用域名（去空白、大小写不敏感）；无法识别时返回 `None`。
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        raw.trim().parse().ok()
    }
}

/// 系统提示模式
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum PromptMode {
    /// 正文即完整系统提示，且不继承父级 `context_files`
    Replace,
    /// 正文追加到父级系统提示之后（"父级孪生"）
    Append,
}

impl PromptMode {
    /// 提示模式的规范小写名（`replace` / `append`）。
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析提示模式名（去空白、大小写不敏感）；无法识别时返回 `None`。
    pub(crate) fn parse(raw: &str) -> Option<Self> {
        raw.trim().parse().ok()
    }
}

/// 一个可派发的 agent 类型。
#[derive(Debug, Clone)]
pub struct AgentType {
    /// 派发身份（`subagent_type` 与 `name` 的匹配键，大小写不敏感）
    pub name: String,
    /// UI 标签（cosmetic，与 `name` 无关）
    pub display_name: String,
    /// frontmatter `description`：工具描述里的用途说明
    pub description: String,
    /// badge 颜色
    pub color: Option<String>,
    /// 工具白名单；None = 继承父级全部
    pub tools: Option<Vec<String>>,
    /// frontmatter 钉死的型号 `provider/id`（None = 由调用参数/父级决定）
    pub model: Option<String>,
    /// frontmatter 钉死的思考档位（None = 由调用参数/父级决定）
    pub thinking: Option<String>,
    /// frontmatter 钉死的最大轮数（None = 用全局默认）
    pub max_turns: Option<u32>,
    /// 系统提示模式（replace = 完整替换；append = 追加到子代理桥接提示后）
    pub prompt_mode: PromptMode,
    /// 是否启用（`false` 的类型不参与发现/派发）
    pub enabled: bool,
    /// 是否落盘子会话（None = 用默认 true）
    pub persist_session: Option<bool>,
    /// 覆盖子会话目录（相对路径按 cwd 解析）
    pub session_dir: Option<String>,
    /// 从 child 工具白名单里强制剔除的工具名（frontmatter `disallowed_tools`）
    pub disallowed_tools: Option<Vec<String>>,
    /// frontmatter `allowed_subagents`：父 agent 的**嵌套开关 + 类型白名单**。
    ///
    /// - `None` = 缺省/`false`：不开启嵌套（上游 opt-in：子代理不写这个字段就拿不到委派工具）
    /// - `Some(vec![])` = `all`/`*`/`true`：开启嵌套且不限型
    /// - `Some(list)` = 开启嵌套且只允许列表内类型
    pub allowed_subagents: Option<Vec<String>>,
    /// frontmatter `memory`：持久记忆作用域（None = 无记忆）
    pub memory: Option<MemoryScope>,
    /// frontmatter `isolated: true` = child 不带扩展工具（只保留内置）
    pub isolated: Option<bool>,
    /// `isolation: worktree`（frontmatter 钉死；调用参数不可覆盖）
    pub isolation: Option<String>,
    /// frontmatter `extensions:`：`false` = 无扩展工具；列表 = 只用这些扩展的工具；None = 全部
    pub extensions: Option<Vec<String>>,
    /// frontmatter `extensions: false`（与 `Some(vec![])` 等价，单独记以便序列化回 `false`）
    pub extensions_none: bool,
    /// frontmatter `exclude_extensions:`
    pub exclude_extensions: Vec<String>,
    /// frontmatter `skills:`：`Some(None)` = `false`（不继承）；`Some(Some(list))` = 预载这些；
    /// `None` = 继承父级（缺省）
    pub skills: Option<Option<Vec<String>>>,
    /// frontmatter `output_transcript`（None = 用全局设置）
    pub output_transcript: Option<bool>,
    /// frontmatter 锁定后台执行（None = 由调用参数/全局默认决定）
    pub run_in_background: Option<bool>,
    /// frontmatter 锁定 fork 父级上下文（None = 由调用参数决定）
    pub inherit_context: Option<bool>,
    /// 系统提示正文（replace = 完整提示；append = 追加段）
    pub system_prompt: String,
    /// 定义来源（内置 / 全局目录 / 项目目录）
    pub source: AgentSource,
    /// 定义文件路径（内置类型无来源文件 = None）
    pub source_path: Option<PathBuf>,
    /// 上游存在但本版本不支持的 frontmatter 字段（已忽略，供一次性告警）
    pub ignored_fields: Vec<String>,
}

impl AgentType {
    /// 工具集展示串（`/agents types` 与 `Agent` 工具描述里用）。
    pub fn tools_label(&self) -> String {
        match &self.tools {
            None => "all tools".to_string(),
            Some(list) if list.is_empty() => "no tools".to_string(),
            Some(list) => list.join(", "),
        }
    }

    /// 展示名（UI 标签优先，回退派发名）。
    pub fn label(&self) -> &str {
        if self.display_name.is_empty() {
            &self.name
        } else {
            &self.display_name
        }
    }
}

/// 解析后的 spawn 请求：frontmatter 权威性已应用（被锁定的字段不再是"可覆盖"）。
#[derive(Debug, Clone)]
pub struct SpawnRequest {
    /// 交给子代理的任务正文
    pub prompt: String,
    /// UI/通知用的简述
    pub description: String,
    /// 调用方给的记忆名（用于分配 `@handle`/`@alias`）
    pub name: Option<String>,
    /// 已按权威性合并的类型约束
    pub tools: Option<Vec<String>>,
    /// 已按权威性合并的型号（`provider/id`；None = 继承父级）
    pub model: Option<String>,
    /// 这个型号是谁定的（frontmatter / 调用方 / 继承）：scope 越界时决定拒绝还是告警
    pub model_source: ModelSource,
    /// 不占后台并发额度、也不排队（定时任务用：墙钟到点就该跑，不该排在长任务后面）
    pub bypass_queue: bool,
    /// `isolation`：`"worktree"` 在仓库副本里跑，`"off"`/None 在当前目录跑
    pub isolation: Option<String>,
    /// 嵌套：谁派发的这个 child（None = 主会话）；属主作用域的委派工具据此判断归属
    pub parent_agent_id: Option<String>,
    /// 嵌套深度（主会话 0、它的子代理 1、孙代理 2…）
    pub depth: u32,
    /// 已按权威性合并的思考档位
    pub thinking: Option<String>,
    /// 已按权威性合并的最大轮数
    pub max_turns: Option<u32>,
    /// 最终执行方式：true = 后台（完成时通知）；false = 前台阻塞取结果
    pub run_in_background: bool,
    /// 是否 fork 父会话给子代理
    pub inherit_context: bool,
    /// 已合并 frontmatter 的剔除表
    pub disallowed_tools: Option<Vec<String>>,
    /// 已合并 frontmatter 的 isolated
    pub isolated: bool,
    /// 扩展作用域白名单（None = 全部）
    pub extensions: Option<Vec<String>>,
    /// 扩展作用域黑名单
    pub exclude_extensions: Vec<String>,
    /// 预载技能名（空 = 不预载）
    pub skills: Vec<String>,
    /// 不继承父级技能索引（`skills: false`）
    pub clear_skills: bool,
    /// 已合并 frontmatter 与全局设置的 `.output` 开关
    pub output_transcript: bool,
    /// 是否落盘子会话（resume 的前提）
    pub persist_session: bool,
    /// 子会话目录覆盖
    pub session_dir: Option<String>,
    /// `resume` 目标（agent id 或 name）
    pub resume: Option<String>,
    /// 只注入给这个 child 的工具（`agent({schema})` → `StructuredOutput`）。
    /// 不受 `tools` / `disallowed_tools` / 作用域约束，直接进 child 的工具表；
    pub injected_tools: Vec<InjectedChildTool>,
    /// 由**工作流调度**派发（不是嵌套子代理，也不是顶层）：
    /// 不占后台额度、不发 `subagents:*` 生命周期事件、RPC 也不可停
    pub workflow_owned: bool,
}

impl SpawnRequest {
    /// 由类型定义构造**基线**请求：frontmatter 字段全量透传，`prompt`/`description` 由调用点给。
    ///
    /// 各调用点（嵌套委派、工作流、定时任务、mention）随后按 frontmatter 权威性覆盖调用参数。
    /// 所有 frontmatter 合并规则集中在 `from_type`，避免四处复制后漂移。
    /// 基线默认后台执行——要求前台的调用方须显式改 `run_in_background`。
    pub(crate) fn from_type(
        ty: &AgentType,
        cfg: &SubagentConfig,
        prompt: String,
        description: String,
    ) -> Self {
        Self {
            prompt,
            description,
            name: None,
            tools: ty.tools.clone(),
            model: ty.model.clone(),
            model_source: model_source_from_type(ty),
            bypass_queue: false,
            isolation: ty.isolation.clone(),
            parent_agent_id: None,
            depth: 0,
            thinking: ty.thinking.clone(),
            max_turns: ty.max_turns,
            run_in_background: true,
            inherit_context: false,
            disallowed_tools: ty.disallowed_tools.clone(),
            isolated: ty.isolated.unwrap_or(false) || ty.extensions_none,
            extensions: ty.extensions.clone(),
            exclude_extensions: ty.exclude_extensions.clone(),
            clear_skills: matches!(ty.skills, Some(None)) || ty.isolated.unwrap_or(false),
            skills: match &ty.skills {
                Some(Some(l)) => l.clone(),
                _ => Vec::new(),
            },
            output_transcript: ty.output_transcript.unwrap_or(cfg.output_transcript),
            persist_session: ty.persist_session.unwrap_or(cfg.remember_agents),
            session_dir: ty.session_dir.clone(),
            resume: None,
            injected_tools: Vec::new(),
            workflow_owned: false,
        }
    }
}

/// frontmatter 定了型号 → `Frontmatter`；否则继承。调用参数覆盖前的默认权威来源。
pub(crate) fn model_source_from_type(ty: &AgentType) -> ModelSource {
    if ty.model.is_some() {
        ModelSource::Frontmatter
    } else {
        ModelSource::Inherited
    }
}

/// token 用量累加器。
///
/// `display_total` ：`input + output + cacheWrite`，
/// **不含 cacheRead**（每轮的 cacheRead 是累计前缀重读，累加会高估工作量）。
/// `cost` 是 provider 按模型价目表报出的估算成本（累加 `usage.cost.total`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageTotals {
    /// 输入 token
    pub input: u64,
    /// 输出 token
    pub output: u64,
    /// 缓存读 token（不计入 [`UsageTotals::display_total`]）
    pub cache_read: u64,
    /// 缓存写 token
    pub cache_write: u64,
    /// provider 估算成本
    pub cost: f64,
}

impl UsageTotals {
    /// 展示用总量：`input + output + cache_write`，**不含** cache_read（重复读前缀会高估工作量）。
    pub fn display_total(&self) -> u64 {
        self.input + self.output + self.cache_write
    }

    /// 累加另一份用量（嵌套子代理的用量向上汇报给父代理
    pub fn merge(&mut self, other: &UsageTotals) {
        self.input = self.input.saturating_add(other.input);
        self.output = self.output.saturating_add(other.output);
        self.cache_read = self.cache_read.saturating_add(other.cache_read);
        self.cache_write = self.cache_write.saturating_add(other.cache_write);
        self.cost += other.cost;
    }

    /// 从事件里的 message JSON 累加 `usage`（缺失或形状不符时静默跳过）。
    pub fn add_from_message(&mut self, message: &Value) {
        let Some(u) = message.get("usage") else {
            return;
        };

        let g = |k: &str| u.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        self.input += g("input");
        self.output += g("output");
        self.cache_read += g("cacheRead");
        self.cache_write += g("cacheWrite");
        self.cost += u
            .get("cost")
            .and_then(|c| c.get("total"))
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
    }
}

/// 运行期代理记录。
#[derive(Clone)]
pub struct AgentRecord {
    /// 记录 id（8 位十六进制，见 `extensions::util::next_id`；resume 沿用）
    pub id: String,
    /// 调用方给的记忆名（None = 未命名）
    pub name: Option<String>,
    /// `@handle` 类型派生句柄（spawn 时分配；resume 保留）
    pub handle: Option<String>,
    /// `@alias` 由调用方 `name` slug 化后编号（与 handle 共用命名空间）
    pub alias: Option<String>,
    /// 派发用的类型名（canonical）
    pub agent_type: String,
    /// 类型的 UI 标签（展示用）
    pub display_name: String,
    /// spawn 时的简述
    pub description: String,
    /// 当前状态（queued / running / 终态）
    pub status: AgentStatus,
    /// 是否后台执行
    pub background: bool,
    /// 类型定义里的 badge 色（色名或 `#RRGGBB`；未配置为 None）
    pub color: Option<String>,
    /// 当前正在执行的工具名（仅有运行中的子代理会置位；widget 展示用）
    pub activity: Option<String>,
    /// 已完成的 agentic 轮数
    pub turns: u32,
    /// 工具调用次数
    pub tool_uses: u32,
    /// token / 成本累计
    pub usage: UsageTotals,
    /// 开始时间（epoch ms）
    pub started_ms: u64,
    /// 结束时间（None = 未结束）
    pub ended_ms: Option<u64>,
    /// 最终文本（成功）或错误文本（失败）
    pub result: Option<String>,
    /// 完整对话转写（`get_subagent_result(verbose)` 用）
    pub transcript: Vec<serde_json::Value>,
    /// 子会话文件路径（resume 用；None = 未落盘）
    pub session_path: Option<String>,
    /// `.output` 运行转写路径（`output_transcript` 开启时才有）
    pub output_path: Option<String>,
    /// max_turns（状态推断用：区分 steered / aborted）
    pub max_turns: Option<u32>,
    /// 运行期控制句柄（排队中为 None）
    pub steer: Option<Arc<dyn Fn(String) + Send + Sync>>,
    /// 中止标志（运行期可置位）
    pub abort: Option<Arc<AtomicBool>>,
    /// 用户显式请求停止（区分 `stopped` 与 `aborted`）
    pub stop_requested: bool,
    /// 结果已读（`get_subagent_result` / RPC consume）：抑制完成通知
    pub consumed: bool,
    /// 完成信号（`get_subagent_result(wait: true)` 用；竞态安全）
    /// 嵌套：派发者（None = 主会话）
    pub parent_agent_id: Option<String>,
    /// 嵌套深度（主会话 0、子代理 1…）
    pub depth: u32,
    /// 由工作流调度派发（不发顶层生命周期事件、RPC 不可停）
    pub workflow_owned: bool,
    /// 本次运行**实际生效**的型号（`provider/id`；显式解析或继承父级；None = 未知）
    pub model: Option<String>,
    /// 子会话压缩次数（`compaction_end` 计数；`subagents:compacted` 事件用）
    pub compaction_count: u32,
    /// worktree 隔离：本次运行的副本（收尾后写结果）
    pub worktree: Option<Worktree>,
    /// worktree 收尾结果（有没有改动、提交到哪个分支）
    pub worktree_result: Option<WorktreeResult>,
    /// 完成信号（`get_subagent_result(wait: true)` 用；竞态安全）
    pub done_rx: Option<Receiver<bool>>,
}

impl AgentRecord {
    /// 运行耗时（毫秒）：已结束取 `ended_ms - started_ms`，仍在运行则取当前时刻；起始晚于结束时饱和为 0。
    pub fn duration_ms(&self) -> u64 {
        let end = self.ended_ms.unwrap_or_else(now_ms);
        end.saturating_sub(self.started_ms)
    }

    /// `get_subagent_result` / `/agents` 用的状态头（无结果正文）。
    pub fn status_header(&self) -> String {
        format!(
            "[subagent {} {} {} · {} turns · {} tok · {:.1}s]",
            self.id,
            self.display_name,
            self.status.as_str(),
            self.turns,
            notify::format_tokens(self.usage.display_total()),
            self.duration_ms() as f64 / 1000.0,
        )
    }

    /// `/agents` 列表一行。
    pub fn list_line(&self) -> String {
        let name = match &self.name {
            Some(n) => format!("{} ({})", self.display_name, n),
            None => self.display_name.clone(),
        };
        let handle = self
            .handle
            .as_deref()
            .map(|h| format!("@{h} "))
            .unwrap_or_default();
        let desc = truncate_chars(&self.description, 40);
        format!(
            "{} {}{} [{}] {} · {} turns · {} tok · {:.1}s",
            self.status.icon(),
            handle,
            name,
            self.status.as_str(),
            desc,
            self.turns,
            notify::format_tokens(self.usage.display_total()),
            self.duration_ms() as f64 / 1000.0,
        )
    }

    /// 结局状态推断：在 `run` 返回后根据是否被停、是否触及轮数上限决定。
    pub fn infer_terminal_status(&self, run_failed: bool) -> AgentStatus {
        if self.stop_requested {
            return AgentStatus::Stopped;
        }
        if run_failed {
            return AgentStatus::Error;
        }
        match self.max_turns.filter(|m| *m > 0) {
            // 宽限期用尽（核心停机点）：turn 数必然触顶
            Some(max) if self.turns >= max.saturating_add(GRACE_TURNS) => AgentStatus::Aborted,
            Some(max) if self.turns >= max => AgentStatus::Steered, // 触顶但及时收尾
            _ => AgentStatus::Completed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// strum 派生的双向转换：`as_str` 是规范名、`parse` 去空白且大小写不敏感。
    #[test]
    fn strum_enum_conversions() {
        assert_eq!(AgentStatus::Queued.as_str(), "queued");
        assert_eq!(AgentStatus::Error.as_str(), "error");
        assert_eq!(AgentSource::Global.as_str(), "global");
        assert_eq!(MemoryScope::Local.as_str(), "local");
        assert_eq!(MemoryScope::parse("PROJECT"), Some(MemoryScope::Project));
        assert_eq!(PromptMode::Replace.as_str(), "replace");
        assert_eq!(PromptMode::parse(" Append "), Some(PromptMode::Append));
        assert_eq!(PromptMode::parse("nope"), None);
    }
}

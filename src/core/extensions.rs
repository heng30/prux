//! 扩展机制
//!
//! - 扩展作者用 Rust 实现 [`Extension`] trait（提供工具与事件钩子）；
//! - 在入口处调用 [`register_extension`] 注册到全局注册表；
//! - 扩展工具定义并入模型可见的工具列表，工具调用未命中内置工具时按名字分发；
//! - 钩子在 Agent 循环的对应挂点被调用
//!
//! 内置工具（read/bash/edit/write/grep/find/ls）保持原有实现不动，
//! 扩展作为独立第二层接入（先查内置、再查扩展注册表）。

mod banner;
mod cancel;
mod dock;
mod footer;
mod hooks;
mod overlay;
mod registry;
mod renderers;
mod tools;
mod ui;

pub mod events;
pub mod markdown;

use crate::{
    core::{
        provider::{AgentMessage, ContentBlock, Usage},
        session_manager::Session,
        tools::{ToolError, ToolResult},
    },
    error::Result,
};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{
    collections::HashSet,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
use strum_macros::{EnumString, IntoStaticStr};

pub use super::{
    provider::api_impls::{RegisteredClassifierApi, RegisteredImageApi},
    virtual_models::{
        FailedRoute, ModelRoute, ModelRouteReason, ModelRouteRequest, RouteTarget,
        VirtualModelDefinition, VirtualModelRouter,
    },
};
pub use banner::{BannerCtx, BannerExtension, BannerLine, BannerSpan};
pub use cancel::{
    BackgroundCancelGuard, background_work_count, cancel_background_work,
    register_background_cancel,
};
pub use dock::{DockLine, DockSection, DockSpan, dock_sections};
pub use footer::{FooterCtx, FooterExtension, FooterLine, FooterSpan, git_branch};
pub use hooks::ExtensionHook;
pub use markdown::render as render_markdown_lines;
pub use overlay::{
    OverlayEditor, OverlayEvent, OverlayInput, OverlayKey, OverlaySize, OverlayView, overlay_event,
    overlay_view,
};
pub use registry::{
    apply_extension_setting, banner_extension_count, banner_names, command_busy_safe,
    command_provider, current_extension_mode, documentation_suppressed, extension_count,
    extension_detail_lines, extension_modes, footer_extension_count, footer_names,
    is_extension_active, is_extension_enabled, panel_entries, persist_extension_states,
    register_banner_extension, register_banner_extension_arc, register_extension,
    register_extension_arc, register_footer_extension, register_footer_extension_arc,
    registered_commands, registration_issues, reload_from_settings, set_all_extensions_disabled,
    set_extension_enabled, set_extension_mode, settings_for, unregister_extension,
};
pub use registry::{registered, registered_banners, registered_footers};
pub use renderers::{
    RegisteredToolRenderer, RichLine, ToolRender, ToolRenderCtx, ToolRenderer,
    active_tool_renderer_count, register_tool_renderer, register_tool_renderers, render_tool_block,
    tool_renderer_for, tool_renderers_fingerprint, unregister_tool_renderers,
};
pub use tools::{
    ExecuteToolFn, ExtensionTool, GrammarSampling, InjectedChildTool, InjectedToolHandler,
    MakeSubAgentFn, NestedCallLog, NestedCallLogHandle, SubAgentControls, SubAgentEventSink,
    SubAgentRunner, SubAgentSpec, ToolAnnotations, ToolExecCtx, ToolExposure, ToolLoadout,
    ToolLoadoutChanges, unavailable_tool_exec,
};
pub use ui::{
    ExtensionUiRequest, RichSpan, SelectOption, UiNotifyLevel, next_ui_id, render_custom_message,
    request_show_dock, request_ui, submit_ui_choice, take_pending_ui,
};

/// 扩展声明的 MCP 服务器条目：`(服务器名, 与 `mcp.json` 的 `mcpServers[name]` 同形的 JSON)`。
///
/// 解析与校验由 MCP 扩展负责（见 [`Extension::mcp_servers`]），核心只负责收集与归属。
pub type DeclaredMcpServer = (String, Value);

/// 已被 `tool_search` 激活的延迟工具名集合，决定下次组装工具表时是否可见。
static ACTIVATED_DEFERRED_TOOLS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

/// 激活集合自上次重建工具表以来是否变化（agent 循环据此重建）
static DEFERRED_ACTIVATION_DIRTY: AtomicBool = AtomicBool::new(false);

/// 扩展提供的 `@` 候选（中立类型，TUI 映射到自己的 `SuggestionItem`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionSuggestion {
    /// 列表中显示的名称（如 `@explore`）
    pub name: String,
    /// 一行说明
    pub description: String,
    /// 插入编辑器的文本（不含 `@`；`apply_suggestion` 会补前缀）
    pub insert: String,
}

/// 扩展返回的一组 `@` 候选。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionSuggestions {
    /// 候选条目
    pub items: Vec<ExtensionSuggestion>,
    /// 是否**独占**这个 `@` 上下文：true 时 TUI 不再并入文件候选、也不发起目录扫描。
    ///
    /// 命令实参补全（`/agents stop @` 的代理 id、`/agents enable @` 的类型名）应为 true：
    /// 候选是 id / 类型名，混进文件名没有意义。
    pub exclusive: bool,
}

/// 收集已启用、当前模式可用且声明了 [`ExtensionHook::Suggestions`] 的扩展的 `@` 候选。
///
/// `query` 是 `@` 之后、光标之前的一段（不含 `@`）；`line_prefix` 是光标前的整行前缀
/// （如 `/agents stop @exp`），供扩展按所在命令/上下文给出不同候选。
/// 任一扩展声明 `exclusive`，结果就独占（不再并入文件候选）。
pub fn suggestion_candidates(query: &str, line_prefix: &str) -> ExtensionSuggestions {
    let mut out = ExtensionSuggestions::default();
    for ext in registered() {
        if ext.hooks().contains(&ExtensionHook::Suggestions) {
            let s = ext.suggestions(query, line_prefix);
            out.items.extend(s.items);
            out.exclusive |= s.exclusive;
        }
    }
    out
}

/// 扩展声明的命令行 flag。
///
/// 仅当扩展已启用时注入 clap 解析；解析结果经 [`Extension::apply_cli_flag`]（bool）
/// 或 [`Extension::apply_cli_flag_value`]（带值）回传给声明它的扩展。未启用扩展的 flag 不注入，`--xxx` 按未知参数报错。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliFlagDef {
    /// flag 名，不含前导 `--`。
    pub name: &'static str,
    /// clap 帮助里显示的说明。
    pub description: &'static str,
    /// `true` = 需要值（`--name <value>`），走 [`Extension::apply_cli_flag_value`]；`false` = bool 开关（`--name`）。
    pub takes_value: bool,
}

/// 扩展声明的全局快捷键
///
/// 不进 keybindings.json 定制体系：内置快捷键优先，扩展快捷键由 TUI
/// 键盘分发作兜底遍历；busy 态跳过整段分发（扩展 handler 可能锁 agent
/// rebuild_tools，与内置需锁快捷键同语义防死锁）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionKeybinding {
    /// 动作 id（keybindings 解析用的键 id，如 `alt+p` 所在条目的标识）
    pub id: String,
    /// 默认键列表（keybindings 解析语法，如 `["alt+p"]`）
    pub keys: Vec<String>,
    /// 快捷键用途说明。
    pub description: String,
}

/// 扩展提供的斜杠命令（名称, 描述, 忙碌安全）。
///
/// 命令随扩展生命周期动态增删：扩展被禁用（/extension 面板）或当前模式不可用时，
/// [`registered_commands`] 不再返回其命令，TUI 输入框候选列表随之移除、分发按未知命令处理。
///
/// `busy_safe`：命令是否在忙碌（流式输出）状态下可安全执行。忙碌时 prompt future
/// 持续持有 agent 锁，任何 handler 内调用 `agent.lock()` 都会阻塞 TUI 线程直至本轮结束。
/// 因此仅当 handler **完全不锁 agent**（只读/视图类命令，如 plan-mode 的 `/todos`）时
/// 才置为 `true`；否则（如 `/plan` 需读/改 agent）必须置为 `false`，忙碌时该命令
/// 仍作为普通文本入 steer 队列，待空闲后执行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionCommand {
    /// 命令名，不含前导 `/`。
    pub name: String,
    /// 命令候选列表里显示的一行说明。
    pub description: String,
    /// 忙碌（流式输出）时是否可安全执行（handler 不锁 agent）。
    pub busy_safe: bool,
    /// 该命令的子命令（`/goal pause` 的 `pause` 等）。
    ///
    /// 供 TUI 输入框在 `/<命令> ` 后弹出子命令候选列表；声明顺序 = 候选顺序
    /// （即空查询时默认选中的第一项）。空表示该命令没有子命令。
    pub subcommands: Vec<SubcommandDef>,
}

/// 扩展声明的子命令（第二级候选，如 `/goal pause` 的 `pause`）。
///
/// 仅作候选列表元数据：解析仍由命令自身 handler 负责，两者需保持一致
/// （扩展侧建议用同一个 `const` 声明，并配一致性测试）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubcommandDef {
    /// 子命令名（插入编辑器时原样写入，如 `pause`）
    pub name: &'static str,
    /// 候选列表里的一行说明
    pub description: &'static str,
}

/// 扩展声明的一项可调设置（TUI「扩展设置面板」用）。
///
/// 每项是一个离散枚举：面板用 Space/Enter 在 [`ExtensionSetting::choices`] 里
/// 循环切换，选中后经 [`Extension::apply_setting`] 回写扩展。`choices` 为空表示只读展示（不接受切换）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionSetting {
    /// 稳定键（回传给 `apply_setting`）
    pub key: String,
    /// 显示标签
    pub label: String,
    /// 选中项下方的一行说明
    pub description: String,
    /// 当前值（通常是 `choices` 之一；只读项可为任意展示文本）
    pub value: String,
    /// 可循环的选项（空 = 只读）
    pub choices: Vec<String>,
}

impl ExtensionSetting {
    /// 构造一个可循环设置项
    pub fn cycle(
        key: impl Into<String>,
        label: impl Into<String>,
        description: impl Into<String>,
        value: impl Into<String>,
        choices: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        Self {
            key: key.into(),
            label: label.into(),
            description: description.into(),
            value: value.into(),
            choices: choices.into_iter().map(Into::into).collect(),
        }
    }

    /// 下一选项（当前值不在 `choices` 中时回到首项）。
    pub fn next_value(&self) -> Option<&str> {
        if self.choices.is_empty() {
            return None;
        }
        let idx = self
            .choices
            .iter()
            .position(|c| c == &self.value)
            .unwrap_or(usize::MAX);
        let next = if idx == usize::MAX {
            0
        } else {
            (idx + 1) % self.choices.len()
        };
        self.choices.get(next).map(String::as_str)
    }
}

/// 扩展模式（/extension 面板 Tab 切换）。
///
/// 模式按“级别（level）”排序：`Minimal` < `Dev` = `Creator` < `All`。
/// 扩展声明**一个或多个**模式（[`Extension::modes`]），每个声明即一个级别；
/// 同一级别的多个模式（`Dev` 与 `Creator`）是**并排**关系。
///
/// `All` 是兜底模式：**所有扩展隐式属于 All**，声明列表无需也不应写 `All`，
/// 只用于声明 All 之外更低的可用级别。某扩展在当前模式 `M` 下可用，当且仅当：
/// `M == All`，或存在声明 `d` 满足 `level(d) < level(M)` 或 `d == M`——
/// 即“比 M 级别低的模式对应的扩展 + 恰好声明为 M 的扩展”；与 M 同级但不同模式
/// 的扩展（并排模式）不可用。因此声明 `[Minimal]` 即“Minimal 及以上都可用”，
/// 需要同时覆盖并排模式时列出多个（如 `[Dev, Creator]`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum ExtensionMode {
    /// 极简模式（级别 0）：基础扩展（底栏、主题、横幅）
    Minimal,
    /// 开发模式（级别 1）：agentic 工作流扩展
    Dev,
    /// 创作模式（级别 1，与 [`ExtensionMode::Dev`] 并排）
    Creator,
    /// 全部模式（级别 2，默认）：兜底全集，已启用扩展全部可用
    All,
}

impl ExtensionMode {
    /// 全部模式（按级别/展示顺序：Minimal → Dev → Creator → All）
    pub const ALL: [ExtensionMode; 4] = [
        ExtensionMode::Minimal,
        ExtensionMode::Dev,
        ExtensionMode::Creator,
        ExtensionMode::All,
    ];

    /// 配置/UI 用的稳定标识（与 settings.json extensionMode 一致）
    pub fn as_str(&self) -> &'static str {
        self.into()
    }

    /// 从配置字符串解析（未知值回落全部模式）
    pub fn parse(s: &str) -> ExtensionMode {
        s.parse().unwrap_or(ExtensionMode::All)
    }

    /// 级别值：数值小 = 更基础（更低级别）；`Dev` 与 `Creator` 同级。
    pub fn level(&self) -> u8 {
        match self {
            ExtensionMode::Minimal => 0,
            ExtensionMode::Dev | ExtensionMode::Creator => 1,
            ExtensionMode::All => 2,
        }
    }

    /// 声明为 `self` 的扩展在当前模式 `current` 下是否可用。
    pub fn usable_in(self, current: ExtensionMode) -> bool {
        self.level() < current.level() || self == current
    }

    /// 声明为 `self` 的扩展实际覆盖的模式集合（展示/查询用）。
    pub fn covered_modes(self) -> Vec<ExtensionMode> {
        ExtensionMode::ALL
            .into_iter()
            .filter(|m| self.usable_in(*m))
            .collect()
    }

    /// Tab 循环顺序（降序）：`All → Dev → Creator → Minimal → All`。
    pub fn next(self) -> ExtensionMode {
        match self {
            ExtensionMode::All => ExtensionMode::Dev,
            ExtensionMode::Dev => ExtensionMode::Creator,
            ExtensionMode::Creator => ExtensionMode::Minimal,
            ExtensionMode::Minimal => ExtensionMode::All,
        }
    }
}

/// 扩展来源信息：标记当前扩展是从哪个插件的哪个版本移植（fork）而来。
///
/// 返回 `None` 表示原创扩展；返回 `Some(ForkProjectInfo)` 则在 `/extension` 二级详情面板
/// 展示来源插件名、版本与链接（可 Ctrl+click 打开浏览器）。
#[derive(Debug, Clone)]
pub struct ForkProjectInfo {
    /// 来源插件名称
    pub plugin_name: String,
    /// 来源版本号（如 `1.2.3`）
    pub plugin_version: String,
    /// 来源项目 URL（打开浏览器）
    pub url: String,
}

/// 扩展接口：扩展通过 trait 提供工具与事件钩子，
/// 再用 [`register_extension`] 注册到全局注册表。
pub trait Extension: Send + Sync {
    /// 扩展名（用于诊断）
    fn name(&self) -> &str;

    /// 扩展来源信息：返回 `None` 表示原创扩展；
    /// 返回 `Some(ForkProjectInfo)` 表示从某个插件移植而来，
    /// `/extension` 二级详情面板展示来源并支持 Ctrl+click 打开浏览器。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        None
    }

    /// 扩展描述（/extension 面板详情展示）
    fn description(&self) -> &str {
        ""
    }

    /// 该扩展声明的扩展模式列表（每个声明即一个“级别”），默认空。
    ///
    /// `All` 是兜底模式，**所有扩展隐式属于 All**，无需也不应写进返回值；
    /// 列表只声明 All 之外更低的可用级别。当前模式 `M` 下可用当且仅当
    /// `M == All`，或存在声明 `d` 满足 [`ExtensionMode::usable_in`]
    /// （`level(d) < level(M)` 或 `d == M`）。因此声明 `[Minimal]` 即
    /// “Minimal 及以上都可用”；需要同时覆盖并排模式（如 Dev 与 Creator）时
    /// 列出多个：`[Dev, Creator]`。
    fn modes(&self) -> Vec<ExtensionMode> {
        Vec::new()
    }

    /// 该扩展是否默认启用（settings.json 未显式配置时的初始状态），默认 `true`。
    ///
    /// 返回 `false` 的扩展注册后处于禁用态，模型可见工具/斜杠命令/快捷键/CLI flag
    /// 都不生效，需在 `/extension` 面板空格开启（或写 settings.json 的
    /// `enabledExtensions`）才生效；开启状态持久化，重启保持。
    /// 基础扩展（footer/banner/extra-themes）保持默认启用；会主动注入工具与提示、
    /// 改变默认 agentic 行为的工作流扩展（plan-mode、goal）声明 `false` 即“按需开启”。
    fn default_enabled(&self) -> bool {
        true
    }

    /// 是否请求从**默认**系统提示词中移除文档段（README/docs/examples 指针
    /// 与主题映射 bullet，见 [`crate::core::system_prompt::documentation_section`]）。
    ///
    /// 返回 `true` 的已启用扩展会让 [`crate::core::system_prompt::build_system_prompt`]
    /// 跳过整段文档描述，用于把帮助文档改为按需询问（如 `doc-helper` 的 `/help`）。
    /// 自定义系统提示词（`custom_prompt`）本就不含该段，不受影响。
    /// 判定按“已启用且当前模式可用”的扩展汇总，默认 `false`（保留文档段）。
    fn suppress_documentation(&self) -> bool {
        false
    }

    /// 该扩展提供的工具定义
    fn tools(&self) -> Vec<ExtensionTool>;

    /// 该扩展提供的虚拟模型定义（在 [`register_extension`] 时收集进全局注册表）。
    ///
    /// 虚拟模型在 `/model` 等目录里与物理模型并列；每次请求经其路由器挑选物理模型与
    /// thinking 级别（见 [`crate::core::virtual_models`]）。扩展被禁用或当前模式不可用时，
    /// 它提供的虚拟模型自动从目录与路由中消失（不需重新注册）。默认无虚拟模型。
    ///
    /// 用 [`VirtualModelDefinition::operation`] 注册的 image / classifier 条目不参与路由，
    /// 只给目录添一条可调用的操作型模型；实现可以随条目一起给
    /// （[`VirtualModelDefinition::with_image_impl`] / `with_classifier_impl`）。
    fn virtual_models(&self) -> Vec<VirtualModelDefinition> {
        Vec::new()
    }

    /// 该扩展为某些工具提供的**渲染器**（`registerToolRenderer` 等价物），默认无。
    ///
    /// 用途是给「宿主没有专门渲染逻辑」的工具（典型是 MCP 聚合工具 `mcp`）画自定义块：
    /// TUI 渲染工具块时先查本表，命中就用扩展给的行，未命中才走宿主内置渲染。
    /// 同一工具名以最后一次注册为准（与 [`Extension::image_apis`] 同口径），
    /// 扩展被禁用 / 当前模式不可用时立即回退内置渲染。
    ///
    /// 渲染实现必须同步、无 I/O（它在渲染路径上被每个工具块调用一次），
    /// 也拿不到 agent / 工具执行句柄——不要在渲染里发工具调用或改状态。
    fn tool_renderers(&self) -> Vec<RegisteredToolRenderer> {
        Vec::new()
    }

    /// 该扩展提供的图片生成协议实现（`api` 名 → 实现），用于**没有自注册目录条目**的场景：
    /// 覆盖同名内置实现，或给用户 `models.json` 里的条目供货。
    ///
    /// 扩展自己声明 image 条目时不必用这个：
    /// [`VirtualModelDefinition::with_image_impl`] 把实现挂在条目上，`api` 名只写一遍。
    /// 两者都按 `api` 名进 [`crate::core::provider::api_impls`] 的分发表，同名以最后一次注册为准。
    /// 声明的 `api` 被目录条目引用后，[`crate::core::provider::generate_images`] 会优先
    /// 走它（覆盖同名内置实现）。扩展被禁用时其实现立即不参与分发。默认无实现。
    fn image_apis(&self) -> Vec<RegisteredImageApi> {
        Vec::new()
    }

    /// 该扩展提供的分类器协议实现（`api` 名 → 实现）。
    ///
    /// 与 [`Extension::image_apis`] 同口径，供 [`crate::core::provider::classify`] 分发；
    /// 自注册分类器条目时改用 [`VirtualModelDefinition::with_classifier_impl`]。
    fn classifier_apis(&self) -> Vec<RegisteredClassifierApi> {
        Vec::new()
    }

    /// 该扩展以**会话为作用域**注册的 MCP 服务器
    ///
    /// - 返回值是 `(服务器名, 条目 JSON)`，条目形状与 `mcp.json` 的 `mcpServers[name]` 一致
    ///   （`command`/`args`/`env`/`cwd`、或 `url`/`headers`/`oauth`，加 `exposure`/
    ///   `toolExposure`/`disabled`/`timeout`）；核心不解释它，由 MCP 扩展解析并校验。
    /// - **不落盘**：每次加载 MCP 配置时调用，因此可按扩展自身的设置动态决定注册哪些；
    ///   扩展被禁用时其服务器随之消失。
    /// - `mcp.json`（全局/项目）里的**同名条目优先**，被覆盖的这条会在 `/mcp` 里标出。
    /// - 名字非法、与其它扩展重名、条目校验不通过时，注册不生效并在 `/mcp` 里报错。
    ///
    /// 默认不注册任何服务器。
    fn mcp_servers(&self) -> Vec<DeclaredMcpServer> {
        Vec::new()
    }

    /// 该扩展提供的斜杠命令（/plan、/todos 等）。
    ///
    /// 命令候选列表与分发的"存在性"由这些声明动态决定：
    /// 扩展禁用或当前模式不可用时其命令自动移除。执行入口由扩展在自身初始化处经
    /// [`crate::modes::interactive::handlers::register_slash_command`] 注册。
    fn commands(&self) -> Vec<ExtensionCommand> {
        Vec::new()
    }

    /// 该扩展声明的命令行 flag（bool 开关）。仅启用扩展注入解析。
    fn cli_flags(&self) -> Vec<CliFlagDef> {
        Vec::new()
    }

    /// 应用解析后的 CLI flag 值（仅注入过的 flag 会被分发）。
    fn apply_cli_flag(&self, _name: &str, _value: bool) {}

    /// 应用解析后的**带值** CLI flag（`CliFlagDef::takes_value` 为 true 的那些，
    /// 如 `--subagents-workflow-file <path>`）。默认空实现。
    fn apply_cli_flag_value(&self, _name: &str, _value: &str) {}

    /// 该扩展声明的全局快捷键（TUI 键盘分发兜底遍历用）。
    fn keybindings(&self) -> Vec<ExtensionKeybinding> {
        Vec::new()
    }

    /// 压缩钩子：
    /// 返回 Some(摘要文本) 时替代内置摘要生成；返回 None 走内置路径。
    /// 压缩开始（生成摘要前）与失败（内置/自定义摘要出错）时各回调一次。
    fn on_before_compact(&self, _tokens_before: u32, _reason: &str) {}

    /// 自定义压缩摘要：默认返回 `None` 走内置路径，返回 `Some` 时替代内置摘要。
    fn custom_summarize(
        &self,
        _system_prompt: &str,
        _conversation: &str,
        _summary_instructions: &str,
    ) -> Option<String> {
        None
    }

    /// 压缩失败回调：内置/自定义摘要出错时触发，默认空实现。
    fn on_compact_failure(&self, _reason: &str) {}

    /// 通用 UI 请求回传：TUI 面板（Select）Enter/Esc 时按 id 分发。
    /// `choice: Some(值)` = 选中；`None` = 取消。返回 `Some(text)` 表示
    /// 要触发下一轮 prompt（follow-up 文本）；`None` 无动作。
    fn on_ui_choice(&self, _id: u64, _choice: Option<String>) -> Option<String> {
        None
    }

    /// 用户 Enter 提交前的输入拦截（空闲提交与忙碌时的 steer 提交都会触发；
    /// 仅当 `hooks()` 声明了 [`ExtensionHook::UserPrompt`] 时被调用）。
    ///
    /// - `None` = 不认领，继续原提交路径；
    /// - `Some(Continue)` = 不认领（显式版 `None`）；
    /// - `Some(Rewrite(text))` = 用改写后的文本继续原提交路径；
    /// - `Some(Handled)` = 认领：不启动回合、不排队（扩展自行投递）。
    fn on_user_prompt(&self, _text: &str) -> Option<UserPromptAction> {
        None
    }

    /// [`Self::on_user_prompt`] 的上下文增强版：额外提供当前对话与忙碌标志。
    ///
    /// 默认桥接到 [`Self::on_user_prompt`]（未覆写本方法的扩展行为不变）。
    /// 需要"实时、压缩感知的对话上下文"的扩展（如 `@mention` clone）覆写本方法。
    /// `busy` = 当前是否正在跑一个回合（提交会作为 steer）。
    fn on_user_prompt_with_context(
        &self,
        text: &str,
        _messages: &[AgentMessage],
        _busy: bool,
    ) -> Option<UserPromptAction> {
        self.on_user_prompt(text)
    }

    /// 用户提交 prompt 的**通知**（无动作，仅当 `hooks()` 声明了
    /// [`ExtensionHook::UserPrompt`] 时被调用）。
    ///
    /// 与 [`Self::on_user_prompt`] 的"首个认领者生效"不同：本方法广播给**所有**
    /// 声明了该 hook 的扩展，无论输入是否被别人认领/改写。供扩展在用户"翻篇"
    /// （新回合开始）时清理上一轮的展示状态（如子代理把已完成的代理记录
    /// 从停靠面板隐去——见 `dispatch_user_submit`）。默认空实现。
    fn on_user_submit(&self, _text: &str, _busy: bool) {}

    /// 会话执行上下文钩子：核心在会话建立 / 每次 turn 开始 / 会话切换后把当前 agent 的 [`ToolExecCtx`] 交给扩展。
    ///
    /// 使扩展在**工具执行之外**（如从 TUI 输入发起的子代理 spawn/resume）也能拿到 `make_sub_agent`
    /// 原先只有 `execute_tool_async` 期间才有。默认空实现。
    fn on_exec_ctx(&self, _ctx: &ToolExecCtx) {}

    /// 会话加载/恢复钩子（TUI 模式启动时调用）：cwd 校验、resume 重建、
    /// 读取/还原会话内持久化状态（如自定义条目 `Entry::Custom`）等。
    ///
    /// `session` 为当前会话引用；需要会话内持久化状态的扩展覆写本方法即可。
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], _session: Option<&Session>) {
    }

    /// 会话切换钩子（/new /resume /import /fork /clone 汇合时调用）：
    /// 额外提供新会话文件路径与消息历史，扩展在此清理跨会话状态
    /// （如 plan-mode 隐藏停靠面板）或按会话恢复持久化状态（如 goal 读取新会话里的
    /// 自定义条目）。路径 None = 全新会话（无文件）。默认空实现。
    fn on_session_switched(&self, _session_path: Option<&str>, _messages: &[AgentMessage]) {}

    /// 注册钩子：`register_extension` 时锁外调用一次。扩展在此接线需要
    /// TUI 状态执行入口的注册（斜杠命令 handler、快捷键 handler 等）。
    fn on_registered(&self) {}

    /// 该扩展关心的事件钩子（决定分发时哪些方法会被调用）。
    ///
    /// # 分发门控契约（框架按此跳过不关心的扩展）
    /// - [`ExtensionHook::TransformContext`] → 仅在该 hook 被声明时调用 [`Self::transform_context`]（每轮 LLM 调用前）；
    /// - [`ExtensionHook::BeforeToolCall`] / [`ExtensionHook::AfterToolCall`] → 门控工具前后钩子；
    /// - [`ExtensionHook::AgentEvent`] → 门控 [`Self::on_agent_event`]（**高频**：流式 `message_update` 等）；
    /// - [`ExtensionHook::Boundary`] → 门控 [`Self::on_boundary`]（每轮一次 + 每次结算一次）；
    /// - [`ExtensionHook::Dock`] → 门控 [`Self::dock_lines`]（TUI **每帧**采集）。
    ///
    /// `filter_tools` / `filter_extension_tools` **不受本门控**：它们在 compose/rebuild 时
    /// 调用（非高频），需保持"调用时直查状态"的动态性，避免启动/rebuild 时序读到陈旧掩码。
    ///
    /// # 动态兴趣与锁序
    /// 本方法在每次分发前**实时求值**（非缓存）：扩展可据自身状态返回不同集合，空闲时返回空
    /// 即被框架完全跳过。约束：
    /// - 实现必须廉价（每次分发一调；避免全量扫描/重 IO）；
    /// - **不得**在持有自身状态锁时被调用（即锁序固定为 `registry → 扩展 state`，禁止反向），
    ///   否则与分发循环的再次加锁形成自锁。
    fn hooks(&self) -> Vec<ExtensionHook> {
        Vec::new()
    }

    /// 运行时过滤模型可见的内置工具
    ///
    /// 在组装系统提示/工具列表（`Agent::new` / `rebuild_tools`）时对所有已启用扩展依次调用；
    /// 返回的列表作为内置工具白名单。默认原样返回。
    /// plan-mode 扩展用它在 plan 模式下移除 `edit`/`write`。
    fn filter_tools(&self, selected: Vec<String>) -> Vec<String> {
        selected
    }

    /// 运行时过滤模型可见的**扩展工具**（本扩展自己的工具）。
    ///
    /// 在组装系统提示/工具列表（`Agent::new` / `rebuild_tools`）时对每个已启用扩展的
    /// `tools()` 结果分别调用；返回的列表决定该扩展哪些工具对模型可见。默认原样返回。
    /// 需要动态显隐自己工具的扩展（如 goal 按 active 状态隐藏 `get_goal`/`update_goal`）
    /// 覆写本方法；显隐变化后需触发 `rebuild_tools` 才在下一轮生效（可经
    /// [`crate::core::extensions::request_ui`] 的 `RebuildTools` 请求，参考 goal 扩展）。
    fn filter_extension_tools(&self, tools: Vec<ExtensionTool>) -> Vec<ExtensionTool> {
        tools
    }

    /// 该扩展是否承载脚本执行（`codemode`）：它调起的嵌套工具视为「脚本调用」。
    ///
    /// 决定 `codemode` 暴露方式的工具对调用方是否可见（见 [`ToolExecCtx::script_call`]）。默认 `false`。
    fn hosts_scripts(&self) -> bool {
        false
    }

    /// 按当前工具集装载改写**模型看到的声明**。
    ///
    /// 在组装工具表（`Agent::new` / `rebuild_tools`）时调用一次，`loadout` 含本次的
    /// 模型可见声明与可脚本调用的工具集。编排类扩展用它动态生成自己的工具描述
    /// （例如 `codemode` 把可调工具的 TypeScript 声明内联进自己的 description），
    /// 或用 `hidden_declarations` 在 `codemode.mode = only` 时把目标工具的声明从请求里摘掉
    /// （工具仍激活、仍可被脚本调）。默认不做任何改写。
    fn prepare_loadout(&self, _loadout: &ToolLoadout) -> ToolLoadoutChanges {
        ToolLoadoutChanges::default()
    }

    /// agent 内核事件流入（agent_start / turn_end / agent_end / tool_execution_* / 自定义事件）。
    /// 事件订阅：由 TUI 事件 sink 处的 [`super::footer::dispatch_agent_event`] 分发。默认空实现。
    ///
    /// `agent_settled`（整轮真正结束）由 UI 在转发前补上 `hasQueuedMessages`：结算时
    /// steer 注入箱或 follow-up 本地队列是否仍有内容。为 true 表示 UI 会立即起下一轮，
    /// 并非真正空闲——需要“等 agent 空下来”才动作的扩展（如 notify）应据此跳过。
    fn on_agent_event(&self, _event: &Value) {}

    /// 供应商原始流事件（归一化之前），仅在声明 [`ExtensionHook::ProviderStreamEvent`] 时调用。
    ///
    /// 事件形状：`{type, provider, api, model, data}`；`data` 是该条 SSE 载荷的原始 JSON，
    /// 含 assistant 消息不会保留的 provider 特有字段（如 OpenAI 的 `output_index`、Anthropic 的 `cache_creation` 拆分）。
    ///
    /// **高频**：每次流式 delta 一次，且在 HTTP 流读取路径上。实现只应缓冲（如写入
    /// 有界队列），**不得**做 IO、解析或加 agent 锁。默认空实现。
    fn provider_stream_event(&self, _event: &Value) {}

    /// 轮收尾 / 结算边界（[`ExtensionHook::Boundary`]）：`turn_end` 与
    /// `agent_before_settle` 两个时机各投一次，事件 `type` 区分。
    ///
    /// - `turn_end`：assistant 与全部工具结果 finalize 之后、`turn_end` 事件发出**之前**调用；
    ///   决策在 `turn_end` **之后**生效（与 `finish_turn` 同一时机）。字段：
    ///   `turnIndex` / `message` / `toolResults` / `messageEntryId` / `toolResultEntryIds` /
    ///   `outcome`（`completed` | `aborted` | `error`）。error/aborted 轮仍会投递，
    ///   但硬退出、决策被忽略。
    /// - `agent_before_settle`：整轮（含重试/溢出恢复）结算前、`agent_settled` 之前调用；
    ///   字段：`outcome` / `contextMessages` / `hasQueuedMessages`（运行中 steer 注入箱是否非空）
    ///   / `continuationIndex`（本 prompt 内第几次结算续跑，从 0 起）。返回 `continue` 时本回合
    ///   再发起一轮（`run_abort`（Esc）期间不再续跑，扩展应按自身状态自限）；
    ///   此处 `end` 无意义（结算已在即），如需在本轮后结束 run 请用 `turn_end` 边界。
    ///
    /// 两个时机的字段都含 `agentScope`（`main` | `subagent`）：
    /// 统一投递主/子 agent，由扩展自行决定是否只在主 agent 生效（goal 即如此）。
    ///
    /// 返回 `None` = 不参与决策；多个扩展按并集合并（`end` 优先于 `continue`，`append` 按注册顺序拼接）。
    fn on_boundary(&self, _event: &Value) -> Option<BoundaryOutcome> {
        None
    }

    /// 提示缓存保温决策（[`ExtensionHook::CacheWarmingDecision`]）。
    ///
    /// 负载字段：`warmCost` / `missCost` / `continuationProbability` / `action`。
    /// 返回 `Some("warm")` / `Some("stop")` 覆盖成本评估结论，
    /// `None` = 不参与；多个扩展**最后一个给出 action 的生效**。
    /// 仅当 `hooks()` 声明了 [`ExtensionHook::CacheWarmingDecision`] 时被询问。
    fn cache_warming_decision(&self, _event: &Value) -> Option<&'static str> {
        None
    }

    /// 扩展↔扩展事件（[`events::emit`] 广播）。**仅当 `hooks()` 声明了 [`ExtensionHook::ExtensionEvent`] 时被投递**。
    ///
    /// `payload` 是总线原样投递的 [`Value`]；总线**不解释**通道名与 payload 结构
    /// （`requestId`/回执信封是扩展之间的线协议）。实现不得在持自身状态锁时被调用
    /// （锁序固定 `registry → 扩展 state`）；同一调用里再 `emit` 会被队列化（见 [`events`]）。
    fn on_extension_event(&self, _name: &str, _payload: &Value) {}

    /// 输入框 `@` 候选来源（**仅当 `hooks()` 声明了 [`ExtensionHook::Suggestions` 时被轮询**）。
    ///
    /// `query` 是 `@` 之后、光标之前的一段（不含 `@`）；`line_prefix` 是光标前的整行前缀
    /// （如 `/agents stop @exp`），可用于按命令上下文区分候选。返回的候选默认会被并到文件
    /// 候选**之前**；若 [`ExtensionSuggestions::exclusive`] 为 true，则 TUI 只显示这些候选，
    /// 不再扫描/并入文件（命令实参补全应如此）。
    /// **必须便宜且只读扩展自身状态**（每个键入都调，高频）；不得加 agent 锁。
    fn suggestions(&self, _query: &str, _line_prefix: &str) -> ExtensionSuggestions {
        ExtensionSuggestions::default()
    }

    /// 该扩展声明的可调设置（TUI 扩展设置面板）。
    ///
    /// 返回空 = 该扩展无设置项，不进入面板。面板占据输入框区域（对齐 /settings：
    /// 打开时隐藏输入框，顶部带搜索框），Space/Enter 在 [`ExtensionSetting::choices`]
    /// 里循环；变更经 [`Extension::apply_setting`] 应用（实现方决定是否落盘）。
    fn settings(&self) -> Vec<ExtensionSetting> {
        Vec::new()
    }

    /// 应用一项设置变更（面板循环到新值时调用）。
    ///
    /// `Err` 用于拒绝非法值（面板保持原值并提示）。实现方自行决定是否持久化。
    fn apply_setting(&self, _key: &str, _value: &str) -> std::result::Result<(), String> {
        Err(format!("unknown setting: {_key}"))
    }

    /// 停靠面板内容行（状态栏上方的可滚动面板）。
    ///
    /// 返回每个显示行（含扩展自带的标题行）。TUI 每帧收集**所有**返回非空行的
    /// 已启用扩展，逐段堆叠渲染；空列表 = 无停靠内容（面板自动隐藏）。
    /// 渲染时按行截断、可滚动，行数上限由 TUI 布局决定。
    /// 每段可见性由提供方自己控制（如 plan-mode 在 /todos 里切换）。
    fn dock_lines(&self) -> Vec<DockLine> {
        Vec::new()
    }

    /// 是否请求 TUI 持续重绘（后台活儿在跑时让 dock 里的 spinner/计数动起来）。
    ///
    /// TUI 本来只在 `dirty || busy` 时重绘，因此后台代理/工作流运行时（回合已结束、
    /// 界面 idle）dock 会冻结（背景代理的 spinner 以前就有这个毛病）。返回 `true` 会让
    /// 80ms 的动画 tick 持续置脏。**必须便宜**（每帧都调；别扫盘、别加 agent 锁）。
    ///
    /// 覆盖层打开时 TUI 本就每帧重绘，不依赖这里。
    fn wants_redraw(&self) -> bool {
        false
    }

    /// 渲染自定义消息卡片（`ExtensionUiRequest::CustomMessage`）。
    ///
    /// 按 `custom_type` 认领：返回 `Some(spans)` 即由本扩展渲染该卡片。
    /// **同一份 `data` 会在两处调用**——收到请求时（实时）与会话恢复时（重放），
    /// 因此实现应是纯函数（只读 `data`，不依赖进程内状态）。
    fn render_custom_message(&self, _custom_type: &str, _data: &Value) -> Option<Vec<RichSpan>> {
        None
    }

    /// 覆盖层内容拉取（每帧；仅在声明了 [`ExtensionHook::Overlay`] 时被调用）。
    ///
    /// 返回 `None` = 不认领该 `id`（TUI 会当作"该覆盖层已消失"并关闭）。
    /// `cols` 是终端宽度：核心的 [`OverlayView`] 只是一列行，需要两栏之类的排版时
    /// 由扩展自己按这个宽度拼（核心不认识"栏"）。
    /// 实现应只读自身状态：**不得加 agent 锁**（TUI 线程调用，busy 时会死锁）。
    fn overlay_view(&self, _id: u64, _cols: u16) -> Option<OverlayView> {
        None
    }

    /// 覆盖层事件回传（按键 / 内联输入提交 / 关闭）。
    ///
    /// 返回 `true` 表示已消费（TUI 不再处理该键）；`false` 则走 TUI 默认语义
    /// （Esc 关闭、↑↓/PgUp/PgDn 滚动、有输入行时 Char/Backspace 编辑、Enter 提交）。
    /// 同样**不得加 agent 锁**。
    fn on_overlay_event(&self, _id: u64, _ev: &OverlayEvent) -> bool {
        false
    }

    /// 扩展**有效**启用状态变化回调（`enabled && 当前模式可用`）。
    ///
    /// 在以下时机被调用：
    /// - [`register_extension`] 注册时（按 settings.json 恢复的初始状态同步一次）；
    /// - [`set_extension_enabled`]（/extension 面板空格切换）后；
    /// - [`set_extension_mode`]（/extension 面板 Tab 切模式）后，仅当本扩展的有效状态因模式变化而翻转时；
    /// - [`reload_from_settings`]（/reload）恢复状态后。
    ///
    /// 需要响应有效启用态的扩展（如把资源文件同步进/移出用户目录的插件、
    /// 开端口/起后台线程的基础设施扩展）覆写本方法。默认空实现。
    ///
    /// 注意：语义是“当前模式下是否可用且启用”，因此声明 `[Dev, Creator]` 的扩展
    /// 在 Minimal 模式下永远是 `false`（即使 `enabled` 为真）——实现不应把它当成
    /// 持久化的开关状态（持久化状态见 [`Self::default_enabled`] 与 /extension 面板）。
    fn on_enabled_changed(&self, _enabled: bool) {}

    /// 执行扩展工具（按名字分发）。实现工具时覆写本方法。
    fn execute_tool(
        &self,
        name: &str,
        _args: &Value,
    ) -> std::result::Result<ToolResult, ToolError> {
        Err(ToolError(format!(
            "extension {}: unknown tool: {}",
            self.name(),
            name
        )))
    }

    /// 异步执行扩展工具：默认桥接同步 [`Self::execute_tool`]；需要异步能力（如子代理调度）的扩展覆写本方法。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        _ctx: ToolExecCtx,
    ) -> BoxFuture<'static, std::result::Result<ToolResult, ToolError>> {
        let r = self.execute_tool(&name, &args);
        Box::pin(async move { r })
    }

    /// LLM 调用前修改上下文钩子）
    fn transform_context(&self, _messages: &mut Vec<AgentMessage>) -> Result<()> {
        Ok(())
    }

    /// 请求前变换（**仅当 `hooks()` 声明了 [`ExtensionHook::ContextWithSystem`] 时被调用**）。
    ///
    /// `messages` 是**含 system 消息**的完整 transcript（`[0]` 为 system，若提示词非空），
    /// 在 [`Self::transform_context`] 之后、每次 LLM 调用前投递。与 `transform_context` 的
    /// 区别：这里能读写系统提示词，且**结果原样发送**——返回后 `[0]`（若仍是 system）
    /// 作为请求的系统提示词，其余作为消息列表；删掉 `[0]` 即等于请求无系统提示词。只影响本次请求，不改写会话历史。
    fn transform_context_with_system(&self, _messages: &mut Vec<AgentMessage>) -> Result<()> {
        Ok(())
    }

    /// 工具调用前拦截钩子。
    /// `Err` 阻止执行；`Ok(Some(v))` 改写参数；`Ok(None)` 放行。
    fn before_tool_call(
        &self,
        _name: &str,
        _args: &Value,
    ) -> std::result::Result<Option<Value>, ToolError> {
        Ok(None)
    }

    /// 工具调用后处理钩子。可改写结果文本。
    fn after_tool_call(
        &self,
        _name: &str,
        _args: &Value,
        result: &str,
    ) -> std::result::Result<String, ToolError> {
        Ok(result.to_string())
    }

    /// 工具调用前拦截钩子（增强接口）。
    /// 默认桥接旧接口：`Err` → block + reason；`Ok(Some(v))` → 改写参数；`Ok(None)` → 放行。
    /// 新扩展可直接覆写本方法获得 block/terminate 语义。
    fn before_tool_call_ext(
        &self,
        name: &str,
        args: &Value,
        _assistant_message: Option<&AgentMessage>,
        _context: &[AgentMessage],
        _all_args: &Value,
    ) -> std::result::Result<BeforeToolCallOutcome, ToolError> {
        match self.before_tool_call(name, args) {
            Ok(Some(new_args)) => Ok(BeforeToolCallOutcome {
                block: false,
                reason: None,
                terminate: false,
                args: Some(new_args),
            }),
            Ok(None) => Ok(BeforeToolCallOutcome::allow()),
            Err(e) => Ok(BeforeToolCallOutcome {
                block: true,
                reason: Some(e.0),
                terminate: false,
                args: None,
            }),
        }
    }

    /// 工具调用后处理钩子（增强接口）。
    /// 默认桥接旧接口：仅改写文本；isError/usage/terminate/content 覆盖留给新实现。
    fn after_tool_call_ext(
        &self,
        name: &str,
        args: &Value,
        result_text: &str,
        _is_error: bool,
    ) -> std::result::Result<AfterToolCallOutcome, ToolError> {
        Ok(AfterToolCallOutcome {
            text: self.after_tool_call(name, args, result_text)?,
            content: None,
            details: None,
            is_error: None,
            usage: None,
            terminate: None,
        })
    }
}

/// beforeToolCall 增强结果
#[derive(Debug, Clone)]
pub struct BeforeToolCallOutcome {
    /// 是否阻止执行（block true 时 reason 进入错误 toolResult 文本）
    pub block: bool,
    /// 阻止原因；None 时回退为默认文案「Tool execution was blocked」。
    pub reason: Option<String>,
    /// 阻止时是否参与批次提前终止（全部 blocked 均 terminate 才停批）
    pub terminate: bool,
    /// 改写后的参数（None = 保持原参数）
    pub args: Option<Value>,
}

impl BeforeToolCallOutcome {
    /// 放行结果：不阻止执行、不改写参数（各字段取默认）。
    pub fn allow() -> Self {
        BeforeToolCallOutcome {
            block: false,
            reason: None,
            terminate: false,
            args: None,
        }
    }
}

/// afterToolCall 增强结果（content 替换整个 content 数组、details 替换整个 details 载荷）
#[derive(Debug, Clone)]
pub struct AfterToolCallOutcome {
    /// 改写后的结果文本
    pub text: String,
    /// 替换整个 content 数组（None = 保持原结果）
    pub content: Option<Vec<ContentBlock>>,
    /// 替换 details 载荷（None = 保持原结果）
    pub details: Option<Value>,
    /// 覆盖是否标记为失败结果（None = 保持原判定）。
    pub is_error: Option<bool>,
    /// 覆盖本次工具执行的 token 用量（None = 不覆盖）。
    pub usage: Option<Usage>,
    /// 覆盖终止语义（None = 保持原值）：true 时本批工具跑完不再请求模型。
    pub terminate: Option<bool>,
}

/// 用户输入拦截动作（[`Extension::on_user_prompt`]）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserPromptAction {
    /// 不认领，继续原提交路径。
    Continue,
    /// 用改写后的文本继续原提交路径。
    Rewrite(String),
    /// 认领：不启动回合、不排队（扩展自行投递）。
    Handled,
}

/// 边界要追加的一条**上下文编辑**（append-only）。
#[derive(Debug, Clone)]
pub struct ContextEditDraft {
    /// 目标条目 id（必须在活动分支上，且是 user / assistant / toolResult 消息条目）。
    pub target_id: String,
    /// `None` = 在模型上下文中省略该条目；`Some(_)` = 只替换其内容。
    pub replacement: Option<Vec<ContentBlock>>,
}

/// 边界要追加的一条 **retain-none 压缩**条目（只有摘要、不保留任何前置条目）；
#[derive(Debug, Clone)]
pub struct BoundaryCompaction {
    /// 摘要文本（替代被压缩掉的历史）。
    pub summary: String,
    /// 压缩前 token 数；`None` 由内核按当前上下文估算。
    pub tokens_before: Option<u64>,
    /// provider 特有的附加信息（如摘要用到的分段结构）。
    pub details: Option<Value>,
}

/// 边界决策（[`Extension::on_boundary`]；
///
/// 多个扩展的决策按「并集」合并：任一扩展要求 `end` 即结束；否则任一要求 `continue` 即续跑。
/// 追加类草稿（`append` / `edits` / `compactions`）按注册顺序累加，
/// 且与 `continue` / `end` 决策无关——即使扩展只追加草稿、不要求续跑也会落库生效
#[derive(Debug, Clone, Default)]
pub struct BoundaryOutcome {
    /// 保证一次后续 provider 请求（工具结果/steering 已能满足时不额外新增）。
    pub r#continue: bool,
    /// 在本轮之后优雅结束 run（不消耗 steering 队列、跳过下一轮 prepareNextTurn）。`end` 优先于 `continue`。
    pub end: bool,
    /// 追加进上下文的 user 消息：仅 `agent_before_settle` 的 `continue` 会消费；
    /// 先落库注入这批消息，再发起下一轮（`turn_end` 边界忽略本字段）。
    pub append: Vec<AgentMessage>,
    /// 追加的**上下文编辑**：落库后立即重建模型上下文（不改写历史）。
    pub edits: Vec<ContextEditDraft>,
    /// 追加的 **retain-none 压缩**条目。
    pub compactions: Vec<BoundaryCompaction>,
}

impl BoundaryOutcome {
    /// 要求续跑一次。
    pub fn continue_once() -> Self {
        BoundaryOutcome {
            r#continue: true,
            ..Default::default()
        }
    }

    /// 追加消息并要求续跑一轮：消息落库后作为下一轮的 user 消息。
    pub fn continue_with(append: Vec<AgentMessage>) -> Self {
        BoundaryOutcome {
            r#continue: true,
            append,
            ..Default::default()
        }
    }

    /// 要求优雅结束 run。
    pub fn end() -> Self {
        BoundaryOutcome {
            end: true,
            ..Default::default()
        }
    }

    /// 追加一条「在模型上下文中省略该条目」的编辑（不改变 `continue` / `end` 决策）。
    pub fn with_omit(mut self, target_id: impl Into<String>) -> Self {
        self.edits.push(ContextEditDraft {
            target_id: target_id.into(),
            replacement: None,
        });
        self
    }

    /// 追加一条「只替换该条目内容」的编辑（不改变 `continue` / `end` 决策）。
    pub fn with_content_replace(
        mut self,
        target_id: impl Into<String>,
        content: Vec<ContentBlock>,
    ) -> Self {
        self.edits.push(ContextEditDraft {
            target_id: target_id.into(),
            replacement: Some(content),
        });
        self
    }

    /// 追加一条 retain-none 压缩（不改变 `continue` / `end` 决策）。
    pub fn with_retain_none_compaction(mut self, summary: impl Into<String>) -> Self {
        self.compactions.push(BoundaryCompaction {
            summary: summary.into(),
            tokens_before: None,
            details: None,
        });
        self
    }

    /// 是否携带追加类草稿（决定是否需要落库 + 重建上下文）。
    pub fn has_drafts(&self) -> bool {
        !self.edits.is_empty() || !self.compactions.is_empty()
    }
}

/// 是否存在声明了 [`ExtensionHook::ContextWithSystem`] 的已启用扩展。
/// 无此类扩展时调用方可完全跳过完整 transcript 的组装（普通路径零开销）。
pub fn has_context_with_system_handlers() -> bool {
    registered()
        .iter()
        .any(|ext| ext.hooks().contains(&ExtensionHook::ContextWithSystem))
}

/// 含 system 消息的完整 transcript 变换（[`ExtensionHook::ContextWithSystem`]）：
/// 按注册顺序就地变换（处理器可增删改，包括首位的 system 消息）。
pub fn dispatch_context_with_system(messages: &mut Vec<AgentMessage>) -> Result<()> {
    for ext in registered() {
        if ext.hooks().contains(&ExtensionHook::ContextWithSystem) {
            ext.transform_context_with_system(messages)?;
        }
    }
    Ok(())
}

/// 是否存在声明了 [`ExtensionHook::Boundary`] 的已启用扩展。
/// 无此类扩展时调用方可完全跳过边界事件构造与分发（普通路径零开销）。
pub fn has_boundary_handlers() -> bool {
    registered()
        .iter()
        .any(|ext| ext.hooks().contains(&ExtensionHook::Boundary))
}

/// 边界分发（`turn_end` / `agent_before_settle`）：把所有声明了
/// [`ExtensionHook::Boundary`] 的扩展的决策合并为一个 [`BoundaryOutcome`]。
/// 事件 `type` 由调用方在 `event` 里给出（与同步发出的内核事件同形）。
pub fn dispatch_boundary(event: &Value) -> BoundaryOutcome {
    let mut out = BoundaryOutcome::default();
    for ext in registered() {
        if !ext.hooks().contains(&ExtensionHook::Boundary) {
            continue;
        }

        if let Some(d) = ext.on_boundary(event) {
            out.r#continue |= d.r#continue;
            out.end |= d.end;
            out.append.extend(d.append);
            out.edits.extend(d.edits);
            out.compactions.extend(d.compactions);
        }
    }
    out
}

/// 提示缓存保温决策分发（`cache_warming_decision`）：把所有声明了
/// [`ExtensionHook::CacheWarmingDecision`] 的扩展的 action 合并
/// **最后一个给出 action 的生效**；无人参与返回 `None`。
pub fn dispatch_cache_warming_decision(event: &Value) -> Option<String> {
    let mut last: Option<String> = None;
    for ext in registered() {
        if !ext.hooks().contains(&ExtensionHook::CacheWarmingDecision) {
            continue;
        }
        if let Some(action) = ext.cache_warming_decision(event)
            && matches!(action, "warm" | "stop")
        {
            last = Some(action.to_string());
        }
    }
    last
}

/// 已激活的延迟工具名集合（进程级单例，供 `tool_search` 与工具表组装共享）。
fn activated_deferred() -> &'static Mutex<HashSet<String>> {
    ACTIVATED_DEFERRED_TOOLS.get_or_init(|| Mutex::new(HashSet::new()))
}

/// 某个延迟工具是否已被 `tool_search` 激活（未激活的不进模型可见列表）。
pub fn is_deferred_tool_activated(name: &str) -> bool {
    activated_deferred().lock().unwrap().contains(name)
}

/// 激活一批延迟工具（`tool_search` 命中项）。
///
/// 只影响**下一次组装工具表**：agent 循环在工具执行后经
/// [`take_deferred_activation_dirty`] 发现变化即 `rebuild_tools()`，因此最早的生效点是下一轮模型调用。
pub fn activate_deferred_tools(names: &[String]) {
    let mut set = activated_deferred().lock().unwrap();
    for name in names {
        if set.insert(name.clone()) {
            DEFERRED_ACTIVATION_DIRTY.store(true, Ordering::Relaxed);
        }
    }
}

/// 取走「激活集合已变化」标志（true = 调用方应重建工具表）
pub fn take_deferred_activation_dirty() -> bool {
    DEFERRED_ACTIVATION_DIRTY.swap(false, Ordering::Relaxed)
}

/// 会话恢复时，按被恢复历史的工具调用把延迟工具重新激活。
///
/// 激活集是进程级内存状态，重启 / `/resume` 后归零：历史里调用过的延迟工具若不再激活，
/// 下一轮的工具表里就没有它（「已由 tool_search 加载的延迟工具不应被丢弃」）。
/// 只激活**当前仍声明为延迟工具**的名字，扩展已下线的不算。
/// 返回 true 表示激活集确实变化（调用方随后必须重建工具表）；无历史 / 无命中也返回 false。
pub fn restore_deferred_activations(messages: &[AgentMessage]) -> bool {
    let pending: Vec<String> = deferred_tools()
        .into_iter()
        .filter(|t| !is_deferred_tool_activated(&t.name))
        .map(|t| t.name)
        .collect();
    if pending.is_empty() {
        return false;
    }

    let mut names: Vec<String> = Vec::new();
    for msg in messages {
        for block in &msg.content {
            if let ContentBlock::ToolCall { name, .. } = block
                && pending.contains(name)
                && !names.contains(name)
            {
                names.push(name.clone());
            }
        }
    }

    if names.is_empty() {
        return false;
    }

    activate_deferred_tools(&names);
    true
}

/// 清空激活集合（测试 / `--no-extensions` 复位用）
pub fn clear_deferred_activations() {
    activated_deferred().lock().unwrap().clear();
    DEFERRED_ACTIVATION_DIRTY.store(false, Ordering::Relaxed);
}

/// 该事件是否来自嵌套工具调用（`ctx.execute_tool`）：即带非空 `parentToolCallId`。
///
/// 嵌套调用是实现细节——底栏的工具计数、假死检测的“工具在跑”、子代理活动指示都只认
/// 顶层调用，否则一次编排会被算成 N 次工具使用（嵌套结果本身见调用方结果的 `details.nestedCalls`）。
pub fn is_nested_call_event(event: &Value) -> bool {
    event
        .get("parentToolCallId")
        .and_then(|v| v.as_str())
        .is_some()
}

/// 已启用扩展声明的全部延迟工具（含已激活的），按注册顺序。
pub fn deferred_tools() -> Vec<ExtensionTool> {
    let mut out: Vec<ExtensionTool> = Vec::new();
    for ext in registered() {
        let tools = ext.tools();
        if !tools.iter().any(|t| t.exposure == ToolExposure::Deferred) {
            continue;
        }

        for tool in ext.filter_extension_tools(tools) {
            if tool.exposure == ToolExposure::Deferred && !out.iter().any(|t| t.name == tool.name) {
                out.push(tool);
            }
        }
    }
    out
}

/// 尚未激活的延迟工具（`tool_search` 的搜索域）。
pub fn unactivated_deferred_tools() -> Vec<ExtensionTool> {
    deferred_tools()
        .into_iter()
        .filter(|t| !is_deferred_tool_activated(&t.name))
        .collect()
}

/// 是否有扩展订阅供应商原始流事件（无则 provider 不构造事件负载，零开销）。
pub fn has_provider_stream_event_handlers() -> bool {
    registered()
        .iter()
        .any(|ext| ext.hooks().contains(&ExtensionHook::ProviderStreamEvent))
}

/// 供应商原始流事件分发（归一化之前）。事件形状：
/// `{type, provider, api, model, data}`，`data` 为该条 SSE 载荷的原始 JSON。
///
/// **在 HTTP 流读取路径上、每个 delta 一次**：调用方应先经
/// [`has_provider_stream_event_handlers`] 门控（一次流只查一次）。
pub fn dispatch_provider_stream_event(
    provider: &str,
    api: &str,
    model: &str,
    data: &serde_json::Value,
) {
    let event = serde_json::json!({
        "type": "provider_stream_event",
        "provider": provider,
        "api": api,
        "model": model,
        "data": data,
    });

    for ext in registered() {
        if ext.hooks().contains(&ExtensionHook::ProviderStreamEvent) {
            ext.provider_stream_event(&event);
        }
    }
}

/// 是否有扩展参与缓存保温决策（无则内核不构造事件负载）。
pub fn has_cache_warming_decision_handlers() -> bool {
    registered()
        .iter()
        .any(|ext| ext.hooks().contains(&ExtensionHook::CacheWarmingDecision))
}

/// 是否有扩展请求持续重绘（[`Extension::wants_redraw`]；每帧调用，必须便宜）。
pub fn wants_redraw() -> bool {
    registered().iter().any(|ext| ext.wants_redraw())
}

/// 用户输入拦截分发：按注册顺序，首个返回 `Some` 的**已声明
/// [`ExtensionHook::UserPrompt`]** 扩展生效；无人认领返回 `None`。
///
/// `messages`/`busy` 透传给 [`Extension::on_user_prompt_with_context`]（默认桥接到
/// [`Extension::on_user_prompt`]）。
pub fn dispatch_user_prompt(
    text: &str,
    messages: &[AgentMessage],
    busy: bool,
) -> Option<UserPromptAction> {
    for ext in registered() {
        if !ext.hooks().contains(&ExtensionHook::UserPrompt) {
            continue;
        }

        if let Some(action) = ext.on_user_prompt_with_context(text, messages, busy) {
            return Some(action);
        }
    }
    None
}

/// 用户提交 prompt 的广播通知（无动作、不短路）：与 [`dispatch_user_prompt`]
/// 走同一批"声明了 [`ExtensionHook::UserPrompt`]"的扩展，但**每一个**都会收到
/// [`Extension::on_user_submit`]，无论输入是否被别的扩展认领/改写。
///
/// 提交路径（空闲 Enter、忙碌 steer、Alt+Enter follow-up）在拦截前后调用它，
/// 供扩展在用户开新回合时收敛上一轮的展示状态。
pub fn dispatch_user_submit(text: &str, busy: bool) {
    for ext in registered() {
        if ext.hooks().contains(&ExtensionHook::UserPrompt) {
            ext.on_user_submit(text, busy);
        }
    }
}

/// 会话执行上下文分发：与 `on_session_start` 同性质，不加门控。
pub fn dispatch_exec_ctx(ctx: &ToolExecCtx) {
    for ext in registered() {
        ext.on_exec_ctx(ctx);
    }
}

/// 收集所有已启用扩展声明的 MCP 服务器。
///
/// 返回 `(所属扩展名, 服务器名, 条目 JSON)`，按扩展注册顺序；同名以**先声明的为准**（后来者的重名声明被丢弃）。
pub fn declared_mcp_servers() -> Vec<(String, String, Value)> {
    let mut out: Vec<(String, String, Value)> = Vec::new();
    for ext in registered() {
        let owner = ext.name().to_string();
        for (name, entry) in ext.mcp_servers() {
            if out.iter().any(|(_, existing, _)| *existing == name) {
                continue;
            }
            out.push((owner.clone(), name, entry));
        }
    }
    out
}

/// 把 agent 内核事件分发给所有已注册扩展（普通扩展 + footer 扩展）。
/// TUI 事件 sink 处调用；事件类型：agent_start / turn_end / agent_end /
/// tool_execution_start|end / 扩展自定义事件（如 plan-mode:changed）。
pub fn dispatch_agent_event(event: &Value) {
    for ext in registered() {
        if ext.hooks().contains(&ExtensionHook::AgentEvent) {
            ext.on_agent_event(event);
        }
    }

    for ext in registered_footers() {
        ext.on_agent_event(event);
    }
}

/// 会话切换钩子分发：TUI 在 session 汇合处调用（/new /resume /import /fork /clone）。
/// 已启用且当前模式可用的扩展会收到 [`Extension::on_session_switched`]，
/// 并拿到新会话路径与消息历史；footer 扩展另经 [`dispatch_footer_session`] 收到同一份历史。
pub fn dispatch_session_switched(path: Option<&str>, messages: &[AgentMessage]) {
    for ext in registered() {
        ext.on_session_switched(path, messages);
    }

    dispatch_footer_session(path, messages);
}

/// footer 扩展的会话历史分发：TUI 启动（`main` 在会话就绪后调用一次）与会话切换
/// （[`dispatch_session_switched`]）共用。
///
/// footer 的计数状态是内存态，而渲染用到的 usage 从消息历史里算；不在历史就绪时
/// 重建一次，`--resume` / `/resume` 之后计数会停在上一会话或留在空白。
pub fn dispatch_footer_session(path: Option<&str>, messages: &[AgentMessage]) {
    for ext in registered_footers() {
        ext.on_session_switched(path, messages);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Result;
    use std::sync::Arc;

    struct Echo;
    impl Extension for Echo {
        fn name(&self) -> &str {
            "echo"
        }
        fn tools(&self) -> Vec<ExtensionTool> {
            vec![ExtensionTool::simple(
                "echo",
                "echo the input",
                serde_json::json!({ "type": "object" }),
                "echo the input",
            )]
        }
        fn hooks(&self) -> Vec<ExtensionHook> {
            vec![
                ExtensionHook::TransformContext,
                ExtensionHook::BeforeToolCall,
                ExtensionHook::AfterToolCall,
            ]
        }
        fn execute_tool(
            &self,
            name: &str,
            args: &Value,
        ) -> std::result::Result<ToolResult, ToolError> {
            if name != "echo" {
                return Err(ToolError("no such tool".to_string()));
            }
            Ok(ToolResult::text(
                args.get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
            ))
        }
        fn transform_context(&self, messages: &mut Vec<AgentMessage>) -> Result<()> {
            if messages.is_empty() {
                messages.push(AgentMessage::user_text("injected"));
            }
            Ok(())
        }
        fn before_tool_call(
            &self,
            _n: &str,
            _args: &Value,
        ) -> std::result::Result<Option<Value>, ToolError> {
            Ok(Some(serde_json::json!({"text": "rewritten"})))
        }
        fn after_tool_call(
            &self,
            _n: &str,
            _args: &Value,
            result: &str,
        ) -> std::result::Result<String, ToolError> {
            Ok(format!("{result} (wrapped)"))
        }
    }

    /// pi #10174：多个扩展声明同名工具 / 斜杠命令 / CLI flag 时给出告警
    /// （先注册者生效，后注册者被丢弃）。
    #[test]
    fn registration_issues_warn_on_shadowed_and_invalid_declarations() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        struct First;
        impl Extension for First {
            fn name(&self) -> &str {
                "collision-first"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![ExtensionTool::simple(
                    "collide-tool",
                    "d",
                    serde_json::json!({ "type": "object" }),
                    "s",
                )]
            }
            fn commands(&self) -> Vec<ExtensionCommand> {
                vec![ExtensionCommand {
                    name: "collide-cmd".to_string(),
                    description: String::new(),
                    busy_safe: true,
                    subcommands: Vec::new(),
                }]
            }
            fn cli_flags(&self) -> Vec<CliFlagDef> {
                vec![CliFlagDef {
                    name: "collide-flag",
                    description: "",
                    takes_value: false,
                }]
            }
        }
        struct Second;
        impl Extension for Second {
            fn name(&self) -> &str {
                "collision-second"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![ExtensionTool::simple(
                    "collide-tool",
                    "d",
                    serde_json::json!({ "type": "object" }),
                    "s",
                )]
            }
            fn commands(&self) -> Vec<ExtensionCommand> {
                vec![ExtensionCommand {
                    name: "collide-cmd".to_string(),
                    description: String::new(),
                    busy_safe: true,
                    subcommands: Vec::new(),
                }]
            }
            fn cli_flags(&self) -> Vec<CliFlagDef> {
                vec![CliFlagDef {
                    name: "collide-flag",
                    description: "",
                    takes_value: false,
                }]
            }
        }

        register_extension(First);
        register_extension(Second);
        let warnings = crate::core::extensions::registration_issues();
        let joined = warnings.join("\n");
        assert!(
            joined.contains("collision-second")
                && joined.contains("`collision-first`")
                && joined.contains("tool `collide-tool`"),
            "应告警同名工具：{joined}"
        );
        assert!(joined.contains("command `/collide-cmd`"), "{joined}");
        assert!(joined.contains("flag `--collide-flag`"), "{joined}");

        assert!(unregister_extension("collision-first"));
        assert!(unregister_extension("collision-second"));
        let after = crate::core::extensions::registration_issues().join("\n");
        assert!(!after.contains("collide-tool"), "注销后不再告警：{after}");
    }

    /// pi #10054：命令名为空 / 含 `/` 或空白时，声明整体丢弃并告警，
    /// 而不是让 `/` 候选列表出现空条目、并被任何前缀命中。
    #[test]
    fn invalid_command_names_are_reported_and_ignored() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        struct Bad;
        impl Extension for Bad {
            fn name(&self) -> &str {
                "bad-command-names"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn commands(&self) -> Vec<ExtensionCommand> {
                vec![
                    ExtensionCommand {
                        name: String::new(),
                        description: String::new(),
                        busy_safe: true,
                        subcommands: Vec::new(),
                    },
                    ExtensionCommand {
                        name: "bad/name".to_string(),
                        description: String::new(),
                        busy_safe: true,
                        subcommands: Vec::new(),
                    },
                    ExtensionCommand {
                        name: "good-cmd".to_string(),
                        description: String::new(),
                        busy_safe: true,
                        subcommands: vec![
                            SubcommandDef {
                                name: "",
                                description: "bad sub",
                            },
                            SubcommandDef {
                                name: "ok sub",
                                description: "bad sub",
                            },
                        ],
                    },
                ]
            }
        }

        register_extension(Bad);
        let joined = crate::core::extensions::registration_issues().join("\n");
        assert!(
            joined.contains("bad-command-names") && joined.contains("invalid name"),
            "应告警非法命令名：{joined}"
        );
        assert!(
            joined.contains("subcommand") && joined.contains("/good-cmd"),
            "应告警非法子命令名：{joined}"
        );

        let commands = crate::core::extensions::registered_commands();
        assert!(
            !commands
                .iter()
                .any(|c| c.name.is_empty() || c.name == "bad/name"),
            "非法命令名不应进入候选列表：{:?}",
            commands.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
        let good = commands
            .iter()
            .find(|c| c.name == "good-cmd")
            .expect("合法命令仍应保留");
        assert!(good.subcommands.is_empty(), "非法子命令应被丢弃");
        assert!(crate::core::extensions::command_provider("").is_none());
        assert!(crate::core::extensions::command_provider("bad/name").is_none());

        assert!(unregister_extension("bad-command-names"));
    }

    #[test]
    fn register_and_dispatch() {
        // 与 settings_manager/auth 测试共用 PRUX_AGENT_DIR：持锁串行，
        // 并重置模式环境（并行崩溃残留不影响本测试）
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        register_extension(Echo);
        // 全局注册表跨测试累积：只断言包含性，不断言总数
        let exts = registered();
        let echo = exts
            .iter()
            .find(|e| e.name() == "echo")
            .expect("echo registered");
        let out = echo.execute_tool("echo", &serde_json::json!({"text": "hi"}));
        assert!(matches!(out, Ok(ToolResult { text: t, .. }) if t == "hi"));
        // 未注册的工具名分发给扩展应返回错误
        let bad = echo.execute_tool("nope", &serde_json::json!({}));
        assert!(bad.is_err());
        // 全局注册表跨测试累积会污染并行的 agent 循环（before/after 钩子会改写其他测试的工具参数/结果）：
        // 测试结束必须注销，避免 run_loop 等套件测试被 Echo 的钩子干扰。
        assert!(unregister_extension("echo"));
    }

    /// `dispatch_user_prompt`：只询问声明了 `UserPrompt` 的扩展，动作三态透传。
    #[test]
    fn user_prompt_dispatch_gates_and_forwards_actions() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        set_extension_mode(ExtensionMode::All);
        // `dispatch_user_prompt` 会遍历全局注册表（可能命中 subagent 扩展），
        // 必须与触碰 manager 全局态的 subagent 测试串行。
        let _sub = crate::extensions::subagent::test_lock();

        struct Silent;
        impl Extension for Silent {
            fn name(&self) -> &str {
                "prompt-silent"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            // 故意实现但不声明 UserPrompt：不得被调用
            fn on_user_prompt(&self, _t: &str) -> Option<UserPromptAction> {
                Some(UserPromptAction::Handled)
            }
        }
        struct Claiming;
        impl Extension for Claiming {
            fn name(&self) -> &str {
                "prompt-claiming"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn hooks(&self) -> Vec<ExtensionHook> {
                vec![ExtensionHook::UserPrompt]
            }
            fn on_user_prompt(&self, text: &str) -> Option<UserPromptAction> {
                match text {
                    "claim" => Some(UserPromptAction::Handled),
                    "rewrite" => Some(UserPromptAction::Rewrite("RW".into())),
                    _ => None,
                }
            }
            fn on_user_submit(&self, _text: &str, _busy: bool) {
                SUBMITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        static SUBMITS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

        register_extension(Silent);
        assert_eq!(
            dispatch_user_prompt("claim", &[], false),
            None,
            "未声明 UserPrompt 的扩展不得被询问"
        );
        register_extension(Claiming);
        assert_eq!(
            dispatch_user_prompt("claim", &[], false),
            Some(UserPromptAction::Handled)
        );
        assert_eq!(
            dispatch_user_prompt("rewrite", &[], false),
            Some(UserPromptAction::Rewrite("RW".into()))
        );
        assert_eq!(dispatch_user_prompt("pass", &[], false), None);

        // `dispatch_user_submit`：只发给声明了 UserPrompt 的扩展，且**不短路**——
        // 无论输入会不会被别人认领，每个声明者都收到一份通知。
        assert_eq!(SUBMITS.load(std::sync::atomic::Ordering::Relaxed), 0);
        dispatch_user_submit("claim", false);
        dispatch_user_submit("rewrite", true);
        assert_eq!(
            SUBMITS.load(std::sync::atomic::Ordering::Relaxed),
            2,
            "提交广播不看拦截结果，也不止一次"
        );

        assert!(unregister_extension("prompt-claiming"));
        assert!(unregister_extension("prompt-silent"));
    }

    /// `dispatch_exec_ctx`：无门控，所有已注册扩展都收到。
    #[test]
    fn exec_ctx_dispatch_reaches_all_registered() {
        use std::sync::atomic::AtomicUsize;
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        struct CtxExt;
        impl Extension for CtxExt {
            fn name(&self) -> &str {
                "ctx-ext"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn on_exec_ctx(&self, _ctx: &ToolExecCtx) {
                CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        set_extension_mode(ExtensionMode::All);
        let _sub = crate::extensions::subagent::test_lock();
        register_extension(CtxExt);

        let ctx = ToolExecCtx {
            execute_tool: crate::core::extensions::unavailable_tool_exec(),
            parent_tool_call_id: None,
            nested_calls: Default::default(),
            session_branch_entries: Default::default(),
            script_tools: Default::default(),
            cwd: "/tmp/x".to_string(),
            make_sub_agent: std::sync::Arc::new(|_| Err("unused".to_string())),
            parent_abort: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            agent_id: None,
            depth: 0,
            parent_model: None,
            script_call: false,
        };
        let before = CALLS.load(std::sync::atomic::Ordering::Relaxed);
        dispatch_exec_ctx(&ctx);
        assert!(CALLS.load(std::sync::atomic::Ordering::Relaxed) > before);
        assert!(unregister_extension("ctx-ext"));
    }

    #[test]
    fn enable_disable_filters_registered() {
        // 与 settings_manager/auth 测试共用 PRUX_AGENT_DIR：持锁串行，结束时清盘
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        struct ToggleExt;
        impl Extension for ToggleExt {
            fn name(&self) -> &str {
                "toggle-ext"
            }
            fn description(&self) -> &str {
                "toggle description"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
        }
        register_extension(ToggleExt);
        assert!(is_extension_enabled("toggle-ext"));
        assert!(
            panel_entries()
                .iter()
                .any(|(n, e, _m)| n == "toggle-ext" && *e)
        );
        // 禁用后从 registered() 快照消失（hook/工具分发自动过滤）
        assert!(set_extension_enabled("toggle-ext", false));
        assert!(!is_extension_enabled("toggle-ext"));
        assert!(registered().iter().all(|e| e.name() != "toggle-ext"));
        // 详情行含描述
        let lines = extension_detail_lines("toggle-ext");
        assert!(
            lines.iter().any(|l| l.contains("toggle description")),
            "detail lines: {:?}",
            lines
        );
        // 恢复
        assert!(set_extension_enabled("toggle-ext", true));
        assert!(registered().iter().any(|e| e.name() == "toggle-ext"));
        // 注销测试扩展，避免污染并行套件（全局注册表跨测试累积）
        assert!(unregister_extension("toggle-ext"));
        // 清盘：避免 settings.json 残留影响下次测试进程
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }

    #[test]
    fn extension_detail_lines_survives_registry_reentry() {
        // 回归：`extension_detail_lines` 曾在持有注册表锁时调用 `ext.tools()`，
        // 而 `tool-search` 的 `tools()` 会回调 `registered()` 再取同一把不可重入的
        // 锁 → /extension 二级详情页直接死锁。此处用超时护栏捕获回归（而非挂住套件）。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        struct ReentrantDetailExt;
        impl Extension for ReentrantDetailExt {
            fn name(&self) -> &str {
                "reentrant-detail-ext"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                // 模拟 tool-search：tools() 内部再次访问全局注册表
                let _ = registered();
                Vec::new()
            }
        }

        register_extension(ReentrantDetailExt);
        let lines =
            crate::test_support::run_with_timeout(std::time::Duration::from_secs(10), || {
                extension_detail_lines("reentrant-detail-ext")
            });
        assert!(
            lines.iter().any(|l| l.starts_with("  mode: ")),
            "详情行应正常生成: {lines:?}"
        );
        assert!(unregister_extension("reentrant-detail-ext"));
    }

    #[test]
    fn extension_detail_lines_show_namespace_exposure_and_annotations() {
        // 与其它用例共用全局注册表：持锁串行
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        struct AnnotatedExt;
        impl Extension for AnnotatedExt {
            fn name(&self) -> &str {
                "annotated-tools-ext"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![
                    ExtensionTool::simple("plain_tool", "plain", serde_json::json!({}), "")
                        .with_namespace("ns")
                        .with_annotations(ToolAnnotations {
                            read_only: true,
                            ..Default::default()
                        }),
                    ExtensionTool::simple("inner_tool", "inner", serde_json::json!({}), "")
                        .with_exposure(ToolExposure::Hidden),
                ]
            }
        }

        register_extension(AnnotatedExt);
        let lines = extension_detail_lines("annotated-tools-ext");
        let tools_line = lines
            .iter()
            .find(|l| l.starts_with("  tools: "))
            .unwrap_or_else(|| panic!("缺少 tools 行: {lines:?}"));
        assert!(
            tools_line.contains("ns/plain_tool (read-only)"),
            "{tools_line}"
        );
        assert!(tools_line.contains("inner_tool (hidden)"), "{tools_line}");
        assert!(unregister_extension("annotated-tools-ext"));
    }

    #[test]
    fn extension_detail_lines_blank_after_description_and_fork_info() {
        // 与 settings_manager/auth 测试共用 PRUX_AGENT_DIR/全局注册表：持锁串行
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        struct ForkExt;
        impl Extension for ForkExt {
            fn name(&self) -> &str {
                "fork-detail-ext"
            }
            fn description(&self) -> &str {
                "fork detail description"
            }
            fn fork_project(&self) -> Option<ForkProjectInfo> {
                Some(ForkProjectInfo {
                    plugin_name: "pi-demo".to_string(),
                    plugin_version: "9.9.9".to_string(),
                    url: String::new(),
                })
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
        }

        register_extension(ForkExt);
        let lines = extension_detail_lines("fork-detail-ext");
        assert_eq!(lines[0], "  fork detail description", "首行应为描述");
        // 描述后紧跟一行空行，与其余信息（mode/tools/...）分隔
        assert_eq!(lines[1], "", "描述后应有一行空行: {lines:?}");
        // fork 来源信息在末尾独立成组（其前另有一行空行分隔）
        let fork_idx = lines
            .iter()
            .position(|l| l == "  forked from: pi-demo v9.9.9")
            .unwrap_or_else(|| panic!("缺少 fork 来源行: {lines:?}"));
        assert!(
            fork_idx > 0 && lines[fork_idx - 1].is_empty(),
            "fork 来源前应有空行: {lines:?}"
        );
        assert!(
            lines.iter().all(|l| !l.contains("fork url")),
            "空 url 不应渲染 fork url 行: {lines:?}"
        );
        assert!(unregister_extension("fork-detail-ext"));
    }

    #[test]
    fn default_disabled_extension_requires_explicit_opt_in() {
        // 与 settings_manager/auth 测试共用 PRUX_AGENT_DIR：持锁串行
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_extension_states(&[], &[]).ok();

        struct OptInExt;
        impl Extension for OptInExt {
            fn name(&self) -> &str {
                "opt-in-ext"
            }
            fn default_enabled(&self) -> bool {
                false
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
        }

        // 默认关闭：注册后仍在注册表但为禁用态，不进入 registered()（不参与分发）
        register_extension(OptInExt);
        assert!(!is_extension_enabled("opt-in-ext"));
        assert!(registered().iter().all(|e| e.name() != "opt-in-ext"));
        assert!(
            panel_entries()
                .iter()
                .any(|(n, e, _)| n == "opt-in-ext" && !*e),
            "面板应列出但为未勾选"
        );

        // /extension 面板开启 → 写盘到 enabledExtensions（而非 disabledExtensions）
        assert!(set_extension_enabled("opt-in-ext", true));
        assert!(is_extension_enabled("opt-in-ext"));
        persist_extension_states().unwrap();
        assert!(
            crate::core::settings_manager::read_enabled_extensions()
                .contains(&"opt-in-ext".to_string()),
            "开启应写 enabledExtensions"
        );
        assert!(
            !crate::core::settings_manager::read_disabled_extensions()
                .contains(&"opt-in-ext".to_string())
        );

        // 下次启动（重新注册）读取 enabledExtensions：初始即启用
        assert!(unregister_extension("opt-in-ext"));
        register_extension(OptInExt);
        assert!(is_extension_enabled("opt-in-ext"));
        assert!(registered().iter().any(|e| e.name() == "opt-in-ext"));

        // 面板关闭 → 两个列表都不再记录（回到默认关闭态）
        assert!(set_extension_enabled("opt-in-ext", false));
        persist_extension_states().unwrap();
        assert!(
            !crate::core::settings_manager::read_enabled_extensions()
                .contains(&"opt-in-ext".to_string())
        );
        assert!(
            !crate::core::settings_manager::read_disabled_extensions()
                .contains(&"opt-in-ext".to_string())
        );

        // 注销 + 清盘：避免污染其他测试
        assert!(unregister_extension("opt-in-ext"));
        crate::core::settings_manager::write_extension_states(&[], &[]).ok();
    }

    #[test]
    fn footer_extensions_appear_in_panel() {
        // 与 settings_manager/auth 测试共用 PRUX_AGENT_DIR：持锁串行
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        struct FooterSpy;
        impl FooterExtension for FooterSpy {
            fn name(&self) -> &str {
                "footer-spy"
            }
            fn description(&self) -> &str {
                "footer desc"
            }
            fn render(&self, _ctx: &FooterCtx) -> Vec<FooterLine> {
                Vec::new()
            }
        }
        register_footer_extension(FooterSpy);
        assert!(
            panel_entries()
                .iter()
                .any(|(n, e, _m)| n == "footer-spy" && *e)
        );
        // 禁用后 registered_footers 不再返回（底栏渲染/事件分发自动过滤）
        assert!(set_extension_enabled("footer-spy", false));
        assert!(
            registered_footers()
                .iter()
                .all(|e| e.name() != "footer-spy")
        );
        // 详情归类 footer 扩展
        let lines = extension_detail_lines("footer-spy");
        assert!(
            lines.iter().any(|l| l.contains("footer extension")),
            "detail lines: {:?}",
            lines
        );
        // 恢复，避免影响其他测试（dispatch_reaches_all_extensions 依赖 footer 生效）
        assert!(set_extension_enabled("footer-spy", true));
        // 清盘：避免 settings.json 残留影响下次测试进程
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }

    #[test]
    fn footer_extensions_are_mutually_exclusive() {
        // 与 settings_manager/auth 测试共用 PRUX_AGENT_DIR：持锁串行
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        struct FooterF1;
        impl FooterExtension for FooterF1 {
            fn name(&self) -> &str {
                "footer-f1"
            }
            fn render(&self, _ctx: &FooterCtx) -> Vec<FooterLine> {
                Vec::new()
            }
        }
        struct FooterF2;
        impl FooterExtension for FooterF2 {
            fn name(&self) -> &str {
                "footer-f2"
            }
            fn render(&self, _ctx: &FooterCtx) -> Vec<FooterLine> {
                Vec::new()
            }
        }
        register_footer_extension(FooterF1);
        // 注册启用时互斥：f1 已启用，f2 注册后 f1 被自动禁用
        register_footer_extension(FooterF2);
        assert!(!is_extension_enabled("footer-f1"), "f2 注册互斥禁用 f1");
        assert!(is_extension_enabled("footer-f2"));
        // 面板切换：启用 f1 自动禁用 f2
        assert!(set_extension_enabled("footer-f1", true));
        assert!(is_extension_enabled("footer-f1"));
        assert!(!is_extension_enabled("footer-f2"), "启用 f1 互斥禁用 f2");
        // 禁用不触发互斥（可全部关闭回退快捷键提示）
        assert!(set_extension_enabled("footer-f1", false));
        assert!(!is_extension_enabled("footer-f2"), "禁用 f1 不应动 f2");
        // footer_names 含两个
        let names = footer_names();
        assert!(
            names.contains(&"footer-f1".to_string()) && names.contains(&"footer-f2".to_string())
        );
        // 清盘：避免 settings.json 残留影响下次测试进程
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }

    #[test]
    fn unknown_extension_toggle_reports_not_found() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        assert!(!set_extension_enabled("no-such-ext", false));
        assert!(!is_extension_enabled("no-such-ext"));
        assert!(extension_detail_lines("no-such-ext").is_empty());
    }

    #[test]
    fn banner_extensions_appear_in_panel() {
        // 与 settings_manager/auth 测试共用 PRUX_AGENT_DIR：持锁串行
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        struct BannerSpy;
        impl BannerExtension for BannerSpy {
            fn name(&self) -> &str {
                "banner-spy"
            }
            fn description(&self) -> &str {
                "banner desc"
            }
            fn render(&self, _ctx: &BannerCtx) -> Vec<BannerLine> {
                Vec::new()
            }
        }
        register_banner_extension(BannerSpy);
        assert!(
            panel_entries()
                .iter()
                .any(|(n, e, _m)| n == "banner-spy" && *e)
        );
        assert!(banner_names().contains(&"banner-spy".to_string()));
        // 禁用后 registered_banners 不再返回（横幅渲染自动过滤）
        assert!(set_extension_enabled("banner-spy", false));
        assert!(
            registered_banners()
                .iter()
                .all(|e| e.name() != "banner-spy")
        );
        // 详情归类 banner 扩展
        let lines = extension_detail_lines("banner-spy");
        assert!(
            lines.iter().any(|l| l.contains("banner extension")),
            "detail lines: {:?}",
            lines
        );
        // 恢复并清盘
        assert!(set_extension_enabled("banner-spy", true));
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }

    #[test]
    fn banner_extensions_are_mutually_exclusive() {
        // 与 settings_manager/auth 测试共用 PRUX_AGENT_DIR：持锁串行
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        struct BannerF1;
        impl BannerExtension for BannerF1 {
            fn name(&self) -> &str {
                "banner-f1"
            }
            fn render(&self, _ctx: &BannerCtx) -> Vec<BannerLine> {
                Vec::new()
            }
        }
        struct BannerF2;
        impl BannerExtension for BannerF2 {
            fn name(&self) -> &str {
                "banner-f2"
            }
            fn render(&self, _ctx: &BannerCtx) -> Vec<BannerLine> {
                Vec::new()
            }
        }
        register_banner_extension(BannerF1);
        // 注册启用时互斥：f1 已启用，f2 注册后 f1 被自动禁用
        register_banner_extension(BannerF2);
        assert!(!is_extension_enabled("banner-f1"), "f2 注册互斥禁用 f1");
        assert!(is_extension_enabled("banner-f2"));
        // 面板切换：启用 f1 自动禁用 f2
        assert!(set_extension_enabled("banner-f1", true));
        assert!(is_extension_enabled("banner-f1"));
        assert!(!is_extension_enabled("banner-f2"), "启用 f1 互斥禁用 f2");
        // 禁用不触发互斥
        assert!(set_extension_enabled("banner-f1", false));
        assert!(!is_extension_enabled("banner-f2"), "禁用 f1 不应动 f2");
        assert!(registered_banners().is_empty(), "全部禁用 → 无 banner 渲染");
        // 清盘
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }

    #[test]
    fn toggling_banner_does_not_touch_footer_state() {
        // 回归：set_extension_enabled 曾对任何非工具扩展先跑 footer 互斥循环，
        // 于是「启用 banner」会顺带禁用当前启用的 footer，persist 又把启用中的
        // footer 写进 disabledExtensions → 重启后底栏丢失（footer(rich) 消失）。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_extension_states(&[], &[]).ok();

        struct FooterIso;
        impl FooterExtension for FooterIso {
            fn name(&self) -> &str {
                "footer-iso"
            }
            fn render(&self, _ctx: &FooterCtx) -> Vec<FooterLine> {
                Vec::new()
            }
        }
        struct BannerIso;
        impl BannerExtension for BannerIso {
            fn name(&self) -> &str {
                "banner-iso"
            }
            fn render(&self, _ctx: &BannerCtx) -> Vec<BannerLine> {
                Vec::new()
            }
        }

        register_footer_extension(FooterIso);
        register_banner_extension(BannerIso);
        assert!(is_extension_enabled("footer-iso"));

        // 关闭再开启 banner：footer 的启用状态必须保持不变
        assert!(set_extension_enabled("banner-iso", false));
        assert!(
            is_extension_enabled("footer-iso"),
            "禁用 banner 不应影响 footer"
        );
        assert!(set_extension_enabled("banner-iso", true));
        assert!(
            is_extension_enabled("footer-iso"),
            "启用 banner 不应禁用 footer"
        );

        // 写盘后 disabledExtensions 不得把仍启用中的 footer 写进去
        persist_extension_states().unwrap();
        let disabled = crate::core::settings_manager::read_disabled_extensions();
        assert!(
            !disabled.contains(&"footer-iso".to_string()),
            "启用中的 footer 泄漏进 disabledExtensions: {disabled:?}"
        );

        // 清理：全部禁用并清盘，避免影响其他用例
        assert!(set_extension_enabled("banner-iso", false));
        assert!(set_extension_enabled("footer-iso", false));
        crate::core::settings_manager::write_extension_states(&[], &[]).ok();
    }

    #[test]
    fn mode_levels_filter_extensions_by_priority() {
        // 与 settings_manager/auth 测试共用 PRUX_AGENT_DIR：持锁串行
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_extension_mode("all").ok();
        struct MinimalExt;
        impl Extension for MinimalExt {
            fn name(&self) -> &str {
                "minimal-ext"
            }
            fn modes(&self) -> Vec<ExtensionMode> {
                vec![ExtensionMode::Minimal]
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
        }
        struct DevExt;
        impl Extension for DevExt {
            fn name(&self) -> &str {
                "dev-ext"
            }
            fn modes(&self) -> Vec<ExtensionMode> {
                vec![ExtensionMode::Dev]
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
        }
        struct CreatorExt;
        impl Extension for CreatorExt {
            fn name(&self) -> &str {
                "creator-ext"
            }
            fn modes(&self) -> Vec<ExtensionMode> {
                vec![ExtensionMode::Creator]
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
        }
        // 并排覆盖：同时声明 Dev 与 Creator（不写 Minimal、也不必写兜底的 All）
        struct SiblingExt;
        impl Extension for SiblingExt {
            fn name(&self) -> &str {
                "sibling-ext"
            }
            fn modes(&self) -> Vec<ExtensionMode> {
                vec![ExtensionMode::Dev, ExtensionMode::Creator]
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
        }
        struct AllOnlyExt;
        impl Extension for AllOnlyExt {
            fn name(&self) -> &str {
                "all-only-ext"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
        }
        register_extension(MinimalExt);
        register_extension(DevExt);
        register_extension(CreatorExt);
        register_extension(SiblingExt);
        register_extension(AllOnlyExt);

        let available =
            || -> Vec<String> { registered().iter().map(|e| e.name().to_string()).collect() };
        let has = |names: &[String], n: &str| names.iter().any(|x| x == n);

        // 全部模式：All 是兜底，所有插件可用（含未声明的 all-only-ext）
        set_extension_mode(ExtensionMode::All);
        let names = available();
        for n in [
            "minimal-ext",
            "dev-ext",
            "creator-ext",
            "sibling-ext",
            "all-only-ext",
        ] {
            assert!(has(&names, n), "All 模式下 {n} 应可用: {names:?}");
        }

        // Dev 模式：Minimal + Dev + sibling("Dev,Creator") 可用；
        // 同级的 Creator 与仅 All 的 all-only-ext 不可用
        set_extension_mode(ExtensionMode::Dev);
        let names = available();
        for n in ["minimal-ext", "dev-ext", "sibling-ext"] {
            assert!(has(&names, n), "Dev 模式下 {n} 应可用: {names:?}");
        }
        assert!(
            !has(&names, "creator-ext"),
            "并排的 Creator 不可用: {names:?}"
        );
        assert!(!has(&names, "all-only-ext"), "{names:?}");

        // Creator 模式：Minimal + Creator + sibling 可用；同级的 Dev 不可用
        set_extension_mode(ExtensionMode::Creator);
        let names = available();
        for n in ["minimal-ext", "creator-ext", "sibling-ext"] {
            assert!(has(&names, n), "Creator 模式下 {n} 应可用: {names:?}");
        }
        assert!(!has(&names, "dev-ext"), "并排的 Dev 不可用: {names:?}");
        assert!(!has(&names, "all-only-ext"), "{names:?}");

        // 极简模式：仅声明 Minimal 的可用
        set_extension_mode(ExtensionMode::Minimal);
        let names = available();
        assert!(has(&names, "minimal-ext"), "{names:?}");
        for n in ["dev-ext", "creator-ext", "sibling-ext", "all-only-ext"] {
            assert!(!has(&names, n), "{n} 不应在 Minimal 可用: {names:?}");
        }
        // panel_entries 仍返回全部（不触碰 enabled 状态）
        let all: Vec<String> = panel_entries().iter().map(|(n, _, _)| n.clone()).collect();
        assert!(all.iter().any(|n| n == "all-only-ext"), "{all:?}");
        assert!(is_extension_enabled("all-only-ext"));
        // 声明查询：返回声明的列表（All 兜底不写，故 all-only-ext 为空列表）
        assert_eq!(extension_modes("minimal-ext"), vec![ExtensionMode::Minimal]);
        assert_eq!(extension_modes("dev-ext"), vec![ExtensionMode::Dev]);
        assert_eq!(extension_modes("creator-ext"), vec![ExtensionMode::Creator]);
        assert_eq!(
            extension_modes("sibling-ext"),
            vec![ExtensionMode::Dev, ExtensionMode::Creator]
        );
        assert_eq!(extension_modes("all-only-ext"), Vec::<ExtensionMode>::new());
        assert_eq!(extension_modes("no-such-ext"), Vec::<ExtensionMode>::new());

        // 切回全部模式：立即恢复（enabled 未被改动）
        set_extension_mode(ExtensionMode::All);
        assert!(has(&available(), "all-only-ext"));
        // 清盘还原
        crate::core::settings_manager::write_extension_mode("all").ok();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        // 注销测试扩展，避免污染并行套件（全局注册表跨测试累积）
        for n in [
            "minimal-ext",
            "dev-ext",
            "creator-ext",
            "sibling-ext",
            "all-only-ext",
        ] {
            assert!(unregister_extension(n));
        }
    }

    /// 生命周期回调按「有效启用态」（enabled ∧ 当前模式可用）求值：声明
    /// `[Dev, Creator]` 的扩展（如 proxy）即便在 settings.json 里开着，Minimal 模式下
    /// 也不能收到 `enabled = true`——否则会在极简模式占用端口 / 起后台线程。
    #[test]
    fn enabled_changed_respects_extension_mode() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        set_all_extensions_disabled(false);

        struct Probe {
            log: Mutex<Vec<bool>>,
        }
        impl Extension for Probe {
            fn name(&self) -> &str {
                "mode-callback-probe"
            }
            fn modes(&self) -> Vec<ExtensionMode> {
                vec![ExtensionMode::Dev, ExtensionMode::Creator]
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn on_enabled_changed(&self, enabled: bool) {
                self.log.lock().unwrap().push(enabled);
            }
        }

        // 启动即为 Minimal 模式（注册必须看到当前模式而非仅磁盘状态）
        set_extension_mode(ExtensionMode::Minimal);
        let probe = Arc::new(Probe {
            log: Mutex::new(Vec::new()),
        });
        register_extension_arc(probe.clone());
        assert_eq!(
            *probe.log.lock().unwrap(),
            vec![false],
            "Minimal 模式下注册不得通知启用"
        );

        // 切到 Dev：可用性翻转 → 通知启用
        set_extension_mode(ExtensionMode::Dev);
        assert_eq!(*probe.log.lock().unwrap(), vec![false, true]);

        // 切回 Minimal：再次翻转 → 通知停用
        set_extension_mode(ExtensionMode::Minimal);
        assert_eq!(*probe.log.lock().unwrap(), vec![false, true, false]);

        // 查询接口与回调同口径：enabled 开关与「当前是否生效」是两件事
        assert!(is_extension_enabled("mode-callback-probe"));
        assert!(!is_extension_active("mode-callback-probe"));

        // 模式未变：不重复通知（幂等，避免每次切模式重跑扩展初始化）
        set_extension_mode(ExtensionMode::Minimal);
        assert_eq!(probe.log.lock().unwrap().len(), 3);

        // Minimal 下面板显式开启：有效态仍为 false
        assert!(set_extension_enabled("mode-callback-probe", true));
        assert_eq!(*probe.log.lock().unwrap(), vec![false, true, false, false]);

        // 此时切到 Dev：持久化的 enabled 为真 → 生效并收到启用
        set_extension_mode(ExtensionMode::Dev);
        assert!(is_extension_active("mode-callback-probe"));
        assert_eq!(
            *probe.log.lock().unwrap(),
            vec![false, true, false, false, true]
        );

        assert!(set_extension_enabled("mode-callback-probe", false));
        assert!(!is_extension_active("mode-callback-probe"));
        assert!(unregister_extension("mode-callback-probe"));
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_extension_mode("all").ok();
    }

    #[test]
    fn mode_levels_are_ordered_and_tab_cycles_descending() {
        // 级别：Minimal(0) < Dev = Creator(1) < All(2)
        assert_eq!(ExtensionMode::Minimal.level(), 0);
        assert_eq!(ExtensionMode::Dev.level(), 1);
        assert_eq!(ExtensionMode::Creator.level(), 1);
        assert_eq!(ExtensionMode::All.level(), 2);
        // 并排：Dev 与 Creator 互不覆盖
        assert!(!ExtensionMode::Dev.usable_in(ExtensionMode::Creator));
        assert!(!ExtensionMode::Creator.usable_in(ExtensionMode::Dev));
        // 声明 Minimal 覆盖全部模式；声明 All 仅覆盖 All
        assert_eq!(
            ExtensionMode::Minimal.covered_modes(),
            ExtensionMode::ALL.to_vec()
        );
        assert_eq!(
            ExtensionMode::Dev.covered_modes(),
            vec![ExtensionMode::Dev, ExtensionMode::All]
        );
        assert_eq!(ExtensionMode::All.covered_modes(), vec![ExtensionMode::All]);
        // Tab 循环：All → Dev → Creator → Minimal → All
        let mut m = ExtensionMode::All;
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(m.as_str());
            m = m.next();
        }
        assert_eq!(seen, vec!["all", "dev", "creator", "minimal"]);
        assert_eq!(m, ExtensionMode::All, "循环一圈回到起点");
        // 解析往返 + 未知回落
        for m in ExtensionMode::ALL {
            assert_eq!(ExtensionMode::parse(m.as_str()), m);
        }
        assert_eq!(ExtensionMode::parse("bogus"), ExtensionMode::All);
    }

    #[test]
    fn hooks_declared_and_invoked() {
        let ext = Echo;
        assert_eq!(ext.hooks().len(), 3);
        let mut msgs = Vec::new();
        ext.transform_context(&mut msgs).expect("transform");
        assert_eq!(msgs.len(), 1);
        assert!(
            ext.before_tool_call("x", &serde_json::json!({}))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            ext.after_tool_call("x", &serde_json::json!({}), "r")
                .unwrap(),
            "r (wrapped)"
        );
    }

    #[test]
    fn agent_event_gated_by_hook_declaration() {
        // 框架按 hooks() 门控 on_agent_event：未声明 AgentEvent 的扩展不再被逐事件轮询。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        use std::sync::atomic::{AtomicUsize, Ordering};
        static WITH: AtomicUsize = AtomicUsize::new(0);
        static WITHOUT: AtomicUsize = AtomicUsize::new(0);

        struct WithHook;
        impl Extension for WithHook {
            fn name(&self) -> &str {
                "agent-event-with"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn hooks(&self) -> Vec<ExtensionHook> {
                vec![ExtensionHook::AgentEvent]
            }
            fn on_agent_event(&self, _e: &Value) {
                WITH.fetch_add(1, Ordering::Relaxed);
            }
        }
        struct WithoutHook;
        impl Extension for WithoutHook {
            fn name(&self) -> &str {
                "agent-event-without"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn on_agent_event(&self, _e: &Value) {
                WITHOUT.fetch_add(1, Ordering::Relaxed);
            }
        }

        register_extension(WithHook);
        register_extension(WithoutHook);
        // 事件类型用 "x" 以兼容 footer 测试遗留的全局 spy（该 spy 断言事件类型恒为 "x"）
        dispatch_agent_event(&serde_json::json!({ "type": "x" }));
        // 全局注册表跨测试共享：并行测试的 dispatch 也会命中本 spy，故只断言 >=1。
        // 关键断言是 WITHOUT == 0（未声明门控即不被调用）。先注销再断言，避免失败时泄漏。
        let with = WITH.load(Ordering::Relaxed);
        let without = WITHOUT.load(Ordering::Relaxed);
        assert!(unregister_extension("agent-event-with"));
        assert!(unregister_extension("agent-event-without"));
        assert!(with >= 1, "声明 AgentEvent 应收到事件");
        assert_eq!(without, 0, "未声明 AgentEvent 不应收到事件");
    }

    #[test]
    fn dock_sections_gated_by_dock_hook() {
        // 框架按 hooks() 门控 dock_lines：未声明 Dock 的扩展不再被每帧轮询。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        struct WithDock;
        impl Extension for WithDock {
            fn name(&self) -> &str {
                "dock-with"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn hooks(&self) -> Vec<ExtensionHook> {
                vec![ExtensionHook::Dock]
            }
            fn dock_lines(&self) -> Vec<DockLine> {
                vec![vec![DockSpan::plain("hello")]]
            }
        }
        struct WithoutDock;
        impl Extension for WithoutDock {
            fn name(&self) -> &str {
                "dock-without"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn dock_lines(&self) -> Vec<DockLine> {
                vec![vec![DockSpan::plain("nope")]]
            }
        }

        register_extension(WithDock);
        register_extension(WithoutDock);
        let has_with = dock_sections().iter().any(|s| s.provider == "dock-with");
        let has_without = dock_sections().iter().any(|s| s.provider == "dock-without");
        // 先注销再断言，避免失败时泄漏到全局注册表污染并行测试
        assert!(unregister_extension("dock-with"));
        assert!(unregister_extension("dock-without"));
        assert!(has_with, "声明 Dock 的扩展应有停靠段");
        assert!(!has_without, "未声明 Dock 的扩展不应被采集");
    }

    #[test]
    fn suggestion_candidates_gated_by_hook() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        struct Suggesting;
        impl Extension for Suggesting {
            fn name(&self) -> &str {
                "suggests"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn hooks(&self) -> Vec<ExtensionHook> {
                vec![ExtensionHook::Suggestions]
            }
            fn suggestions(&self, query: &str, _line_prefix: &str) -> ExtensionSuggestions {
                ExtensionSuggestions {
                    items: vec![ExtensionSuggestion {
                        name: format!("@{query}"),
                        description: "agent".to_string(),
                        insert: query.to_string(),
                    }],
                    exclusive: false,
                }
            }
        }
        struct Silent;
        impl Extension for Silent {
            fn name(&self) -> &str {
                "silent"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn suggestions(&self, _query: &str, _line_prefix: &str) -> ExtensionSuggestions {
                ExtensionSuggestions {
                    items: vec![ExtensionSuggestion {
                        name: "@nope".to_string(),
                        description: String::new(),
                        insert: "nope".to_string(),
                    }],
                    exclusive: false,
                }
            }
        }

        register_extension(Suggesting);
        register_extension(Silent);
        let items = suggestion_candidates("explore", "/agents stop @explore");
        assert!(unregister_extension("suggests"));
        assert!(unregister_extension("silent"));
        assert_eq!(items.items.len(), 1, "{items:?}");
        assert_eq!(items.items[0].name, "@explore");
        assert!(!items.exclusive, "未声明 exclusive 时不独占");
    }
}

#[cfg(test)]
mod cache_warming_decision_tests {
    use super::*;

    struct WarmForcing;
    impl Extension for WarmForcing {
        fn name(&self) -> &str {
            "warm-forcing"
        }
        fn tools(&self) -> Vec<ExtensionTool> {
            Vec::new()
        }
        fn hooks(&self) -> Vec<ExtensionHook> {
            vec![ExtensionHook::CacheWarmingDecision]
        }
        fn cache_warming_decision(&self, _event: &Value) -> Option<&'static str> {
            Some("warm")
        }
    }

    struct Vetoing;
    impl Extension for Vetoing {
        fn name(&self) -> &str {
            "vetoing"
        }
        fn tools(&self) -> Vec<ExtensionTool> {
            Vec::new()
        }
        fn hooks(&self) -> Vec<ExtensionHook> {
            vec![ExtensionHook::CacheWarmingDecision]
        }
        fn cache_warming_decision(&self, _event: &Value) -> Option<&'static str> {
            Some("stop")
        }
    }

    struct Silent;
    impl Extension for Silent {
        fn name(&self) -> &str {
            "silent-cache"
        }
        fn tools(&self) -> Vec<ExtensionTool> {
            Vec::new()
        }
        fn hooks(&self) -> Vec<ExtensionHook> {
            vec![ExtensionHook::CacheWarmingDecision]
        }
    }

    #[test]
    fn last_defined_action_wins_and_silent_handlers_ignored() {
        let event = serde_json::json!({
            "type": "cache_warming_decision",
            "warmCost": 0.03,
            "missCost": 0.345,
            "continuationProbability": 1.0,
            "action": "warm",
        });
        assert_eq!(dispatch_cache_warming_decision(&event), None);
        assert!(!has_cache_warming_decision_handlers());

        register_extension(Silent);
        assert_eq!(dispatch_cache_warming_decision(&event), None, "None 不覆盖");

        register_extension(WarmForcing);
        assert_eq!(
            dispatch_cache_warming_decision(&event).as_deref(),
            Some("warm")
        );

        register_extension(Vetoing);
        assert_eq!(
            dispatch_cache_warming_decision(&event).as_deref(),
            Some("stop"),
            "最后一个给出 action 的扩展生效"
        );
        assert!(has_cache_warming_decision_handlers());

        assert!(unregister_extension("warm-forcing"));
        assert!(unregister_extension("vetoing"));
        assert!(unregister_extension("silent-cache"));
    }
}

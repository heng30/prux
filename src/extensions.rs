//! 内置扩展（Rust 编译期静态注册版）。
//!
//! 每个模块实现 [`crate::core::extensions`] 中的 trait（工具扩展或 footer 扩展），
//! 并在模块内用 `#[linkme::distributed_slice]` 自声明工厂条目；应用启动时由
//! [`register_all`] 遍历分布式切片、按 priority 降序统一注册。
//!
//! 新增内置扩展 = 声明 `pub mod x` + 在插件模块内加一个工厂静态，
//! 无需再改 main.rs 的注册函数。

pub(crate) mod util;

pub mod banner;
pub mod codemode;
pub mod debug_provider;
pub mod doc_helper;
pub mod downloader;
pub mod extra_themes;
pub mod footer;
pub mod goal;
pub mod hang_detect;
pub mod jet;
pub mod loop_detect;
pub mod mcp;
pub mod notify;
pub mod plan_mode;
pub mod proxy;
pub mod rewind;
pub mod skills;
pub mod subagent;
pub mod tasks;
pub mod tool_search;
pub mod update_check;
pub mod web_access;

pub use plan_mode::PlanMode;

use crate::core::extensions::{self, BannerExtension, Extension, FooterExtension};
use std::sync::Arc;

/// 内置扩展优先级（值大者先注册）。
///
/// footer 三布局互斥且“最后注册的启用者生效”，故默认底栏是 priority 最小的那个；
/// 当前注册序 rich → minimal → normal，默认 `footer(normal)`。
/// 若想改默认底栏，把目标 footer 的 priority 调成三者最小即可。
pub const PRIORITY_EXTRA_THEMES: u32 = 100;
/// goal 持久化自主目标扩展的注册优先级。
pub const PRIORITY_GOAL: u32 = 90;
/// plan-mode 只读探索模式扩展的注册优先级。
pub const PRIORITY_PLAN_MODE: u32 = 85;
/// tasks 结构化任务跟踪扩展的注册优先级。
pub const PRIORITY_TASKS: u32 = 80;
/// subagent 子代理扩展的注册优先级。
pub const PRIORITY_SUBAGENT: u32 = 78;
/// loop-detect 循环行为检测扩展的注册优先级。
pub const PRIORITY_LOOP_DETECT: u32 = 75;
/// notify 空闲通知扩展的注册优先级。
pub const PRIORITY_NOTIFY: u32 = 74;
/// hang-detect 会话假死检测与恢复扩展的注册优先级。
pub const PRIORITY_HANG_DETECT: u32 = 73;
/// proxy 本地 OpenAI 兼容网关扩展的注册优先级。
pub const PRIORITY_PROXY: u32 = 72;
/// rewind 回退上一次用户输入扩展的注册优先级。
pub const PRIORITY_REWIND: u32 = 71;
/// web-access 联网搜索与抓取扩展的注册优先级。
pub const PRIORITY_WEB_ACCESS: u32 = 70;
/// mcp MCP 客户端扩展的注册优先级。
pub const PRIORITY_MCP: u32 = 69;
/// skills 扩展技能管理（`/skills` 面板）扩展的注册优先级。
pub const PRIORITY_SKILLS: u32 = 65;
/// downloader 外部依赖下载扩展的注册优先级。
pub const PRIORITY_DOWNLOADER: u32 = 60;
/// update-check 启动时版本检查扩展的注册优先级。
pub const PRIORITY_UPDATE_CHECK: u32 = 55;
/// doc-helper 按需文档查询扩展的注册优先级。
pub const PRIORITY_DOC_HELPER: u32 = 50;
/// debug-provider 供应商原始流事件查看扩展的注册优先级。
pub const PRIORITY_DEBUG_PROVIDER: u32 = 45;
/// tool_search 延迟工具检索扩展的注册优先级。
pub const PRIORITY_TOOL_SEARCH: u32 = 40;
/// codemode 工具编排扩展的注册优先级。
pub const PRIORITY_CODEMODE: u32 = 39;
/// jet 规划/实现双模型路由扩展的注册优先级。
pub const PRIORITY_JET: u32 = 38;
/// banner 启动横幅扩展的注册优先级。
pub const PRIORITY_BANNER: u32 = 30;
/// footer(rich) 底栏布局的注册优先级，三者中值大者先注册。
pub const PRIORITY_FOOTER_RICH: u32 = 20;
/// footer(minimal) 底栏布局的注册优先级，只显示模型统计行。
pub const PRIORITY_FOOTER_MINIMAL: u32 = 15;
/// footer(normal) 底栏布局的注册优先级，值最小故为默认底栏。
pub const PRIORITY_FOOTER_NORMAL: u32 = 10;

/// 主题样式键与缺省色
pub(crate) const DIM: (&str, &str) = ("dim", "#666666");
/// 成功/正常状态样式键，缺省绿色（下载完成、git 分支、费用等）。
pub(crate) const SUCCESS: (&str, &str) = ("success", "#b5bd68");
/// 强调/进行中状态样式键，缺省青色（横幅、目录名、token 速率等）。
pub(crate) const ACCENT: (&str, &str) = ("accent", "#8abeb7");
/// 次要信息样式键，缺省灰色（提示、计数、系统标记等）。
pub(crate) const MUTED: (&str, &str) = ("muted", "#999999");
/// 失败/错误状态样式键，缺省红色（下载失败、错误统计等）。
pub(crate) const ERROR: (&str, &str) = ("error", "#cc6666");
/// 警告状态样式键，缺省黄色（如高上下文占用提示）。
pub(crate) const WARNING: (&str, &str) = ("warning", "#ffff00");

/// 普通扩展工厂集合：插件模块用 `#[linkme::distributed_slice]` 自声明，
/// 启动时经 [`register_all`] 按 priority 降序注册。
#[linkme::distributed_slice]
pub static EXTENSION_FACTORIES: [ExtensionFactory];

/// 底栏扩展工厂集合：语义同 [`EXTENSION_FACTORIES`]。
#[linkme::distributed_slice]
pub static FOOTER_FACTORIES: [FooterFactory];

/// 横幅扩展工厂集合：语义同 [`EXTENSION_FACTORIES`]。
#[linkme::distributed_slice]
pub static BANNER_FACTORIES: [BannerFactory];

/// 普通扩展工厂条目（分布式切片元素）。
///
/// `priority` 值越大越先注册。注册顺序影响两类语义：
/// - footer 互斥：注册时新启用者会禁用已启用 footer，**最后注册的启用者生效**，
///   因此想作为默认底栏的 footer 应给最低 priority（最后注册）；
/// - 面板条目/同名命令的先到先得展示顺序。
#[derive(Clone, Copy)]
pub struct ExtensionFactory {
    /// 注册优先级，值越大越先注册。
    pub priority: u32,
    /// 惰性构造扩展实例的工厂函数。
    pub make: fn() -> Arc<dyn Extension>,
}

/// 底栏扩展工厂条目：语义同 [`ExtensionFactory`]。
#[derive(Clone, Copy)]
pub struct FooterFactory {
    /// 注册优先级，值越大越先注册。
    pub priority: u32,
    /// 惰性构造底栏扩展实例的工厂函数。
    pub make: fn() -> Arc<dyn FooterExtension>,
}

/// 横幅扩展工厂条目：语义同 [`ExtensionFactory`]。
#[derive(Clone, Copy)]
pub struct BannerFactory {
    /// 注册优先级，值越大越先注册。
    pub priority: u32,
    /// 惰性构造横幅扩展实例的工厂函数。
    pub make: fn() -> Arc<dyn BannerExtension>,
}

/// 启动时注册全部内置扩展：遍历分布式切片，按 priority 降序（值大者先）注册。
///
/// main 入口在 CLI flag 解析前调用（动态 flag 注入依赖已启用扩展声明）。
pub fn register_all() {
    let mut exts: Vec<ExtensionFactory> = EXTENSION_FACTORIES.iter().copied().collect();
    exts.sort_by_key(|f| std::cmp::Reverse(f.priority));
    for f in exts {
        extensions::register_extension_arc((f.make)());
    }

    let mut footers: Vec<FooterFactory> = FOOTER_FACTORIES.iter().copied().collect();
    footers.sort_by_key(|f| std::cmp::Reverse(f.priority));
    for f in footers {
        extensions::register_footer_extension_arc((f.make)());
    }

    let mut banners: Vec<BannerFactory> = BANNER_FACTORIES.iter().copied().collect();
    banners.sort_by_key(|f| std::cmp::Reverse(f.priority));
    for f in banners {
        extensions::register_banner_extension_arc((f.make)());
    }
}

/// 取命令之后的参数（原文首段为命令名，参数保留原大小写；无参数返回空串）
pub fn command_arg(cmd: &str) -> &str {
    match cmd.trim().split_once(char::is_whitespace) {
        Some((_, arg)) => arg.trim(),
        None => "",
    }
}

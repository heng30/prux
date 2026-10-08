//! 扩展工具渲染器与注册表。
//!
//! 渲染器只对「宿主没有专门渲染逻辑的工具」有意义（典型是 MCP 聚合工具 `mcp`）：
//! TUI 渲染工具块时先问本表，命中就用扩展给的行，未命中才走宿主内置渲染。
//!
//! - 归属扩展的渲染器随扩展启停 / 扩展模式过滤（扩展被禁用后立即回退内置渲染）；
//! - 同一工具名以**最后一次注册**为准（与 [`crate::core::provider::api_impls`] 同口径）；
//! - 渲染路径每个工具块查一次注册表，不做缓存（条目数为个位数）；
//!   渲染行的缓存由 `MessageCache` 按 [`tool_renderers_fingerprint`] 失效。

use super::ui::RichSpan;
use serde_json::Value;
use std::sync::{Arc, Mutex, OnceLock};

/// 渲染器产出的一行：由若干带样式片段组成（与扩展 UI 通知同形，见 [`RichSpan`]）。
pub type RichLine = Vec<RichSpan>;

/// 无归属（SDK 直接注册，非扩展声明）的渲染器归属名：恒生效。
pub const STANDALONE_OWNER: &str = "<standalone>";

/// 扩展工具渲染器的渲染结果。
#[derive(Debug, Clone, Default)]
pub struct ToolRender {
    /// 内容行（宿主负责铺背景、折行与裁剪）
    pub lines: Vec<RichLine>,
    /// 折叠时保留的**视觉行**数（按宽度折行后）；`None` = 折叠态也全部展示
    pub preview_lines: Option<usize>,
}

/// 渲染上下文：扩展渲染器能看到的一切（只读借用；由宿主按当前帧填入）。
#[derive(Debug)]
pub struct ToolRenderCtx<'a> {
    /// 命中的工具名（[`ToolRenderer::tools`] 里声明过的那个）
    pub tool: &'a str,
    /// 本次调用参数（原始 JSON）
    pub args: &'a Value,
    /// 结果文本与是否失败；工具仍在执行时为 `None`
    pub result: Option<(&'a str, bool)>,
    /// 已耗时毫秒数（执行中为「距开始」；调用方没带开始时间时为 `None`）
    pub duration_ms: Option<u64>,
    /// 用户是否展开了全部输出（Ctrl+O）
    pub expanded: bool,
    /// 工具块整宽（含左右各 1 列 padding）
    pub width: usize,
    /// 内侧可用宽度（已扣 padding）
    pub inner_width: usize,
}

/// 扩展为某个工具提供的渲染器（宿主按工具名分派；返回 `None` 时交回内置渲染）。
///
/// 实现必须**同步、无 I/O、无阻塞**：它在 TUI 渲染路径上被每个工具块调用一次；
/// 也拿不到 agent / 工具执行句柄，不要在渲染里发起工具调用或改动状态。
pub trait ToolRenderer: Send + Sync {
    /// 渲染器名（`/extension` 详情与诊断用）
    fn name(&self) -> &'static str;
    /// 生效的工具名（如 `"mcp"`、`"subagent"`）
    fn tools(&self) -> &[&'static str];
    /// 渲染一个工具块；返回 `None` 表示这次不接管（交回内置渲染）。
    fn render(&self, ctx: &ToolRenderCtx<'_>) -> Option<ToolRender>;
}

/// 扩展注册的一条工具渲染器。
pub struct RegisteredToolRenderer {
    /// 渲染器名（`/extension` 详情与诊断用）
    pub name: String,
    /// 实现本体
    pub implementation: Arc<dyn ToolRenderer>,
}

impl RegisteredToolRenderer {
    /// 用 trait 实现装箱一条注册项（`name` 只写一遍：取实现自报的名字）。
    pub fn new(implementation: Arc<dyn ToolRenderer>) -> Self {
        RegisteredToolRenderer {
            name: implementation.name().to_string(),
            implementation,
        }
    }
}

/// 注册表条目：渲染器 + 归属扩展名。
struct Entry {
    /// 注册它的扩展名；[`STANDALONE_OWNER`] 表示无归属。
    owner: String,
    /// 渲染器名（诊断用）
    name: String,
    /// 实现本体
    implementation: Arc<dyn ToolRenderer>,
}

/// 注册表（懒加载初始化）。
fn registry() -> &'static Mutex<Vec<Entry>> {
    static REGISTRY: OnceLock<Mutex<Vec<Entry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// 条目当前是否生效：无归属者恒生效，其余随扩展启用状态 / 扩展模式动态过滤。
fn is_entry_active(owner: &str) -> bool {
    owner == STANDALONE_OWNER || super::is_extension_active(owner)
}

/// 注册（或替换）`owner` 名下的渲染器：先清掉该 owner 已有的，再登记新的。
///
/// 扩展重新注册（或 `/reload` 后重登记）时同 owner 的旧渲染器必须整体消失，否则会留下指向旧实现的条目。
pub fn register_tool_renderers(owner: impl Into<String>, renderers: Vec<RegisteredToolRenderer>) {
    let owner = owner.into();
    let mut reg = registry().lock().unwrap();
    reg.retain(|e| e.owner != owner);
    for r in renderers {
        reg.push(Entry {
            owner: owner.clone(),
            name: r.name,
            implementation: r.implementation,
        });
    }
}

/// 直注册一条无归属渲染器（SDK / 内置扩展不走 [`Extension::tool_renderers`] 时用），始终生效。
pub fn register_tool_renderer(implementation: Arc<dyn ToolRenderer>) {
    register_tool_renderers(
        STANDALONE_OWNER,
        vec![RegisteredToolRenderer::new(implementation)],
    );
}

/// 注销某 owner 的全部渲染器；返回是否有条目被移除。
pub fn unregister_tool_renderers(owner: &str) -> bool {
    let mut reg = registry().lock().unwrap();
    let before = reg.len();
    reg.retain(|e| e.owner != owner);
    reg.len() != before
}

/// 为某个工具名找渲染器：取**最后一个**声明了该名字且当前生效的条目。
///
/// 没有渲染器声明这个工具名、或它的归属扩展当前不可用时返回 `None`（调用方回退内置渲染）。
pub fn tool_renderer_for(tool: &str) -> Option<Arc<dyn ToolRenderer>> {
    let reg = registry().lock().unwrap();
    reg.iter()
        .rev()
        .find(|e| is_entry_active(&e.owner) && e.implementation.tools().contains(&tool))
        .map(|e| e.implementation.clone())
}

/// 让渲染器渲染一个工具块；未注册 / 未命中 / 渲染器放弃接管时返回 `None`。
pub fn render_tool_block(tool: &str, ctx: &ToolRenderCtx<'_>) -> Option<ToolRender> {
    tool_renderer_for(tool)?.render(ctx)
}

/// 当前生效渲染器的指纹（`归属:渲染器名:工具名`，排序后拼接）：
/// 注册表或扩展启用状态一变就不同。
///
/// 渲染层用它判定 `MessageCache` 是否整表失效——渲染行由扩展产出，
/// 扩展 `/reload`、禁用后旧行必须重画。
pub fn tool_renderers_fingerprint() -> String {
    let reg = registry().lock().unwrap();
    let mut parts: Vec<String> = reg
        .iter()
        .filter(|e| is_entry_active(&e.owner))
        .flat_map(|e| {
            e.implementation
                .tools()
                .iter()
                .map(|t| format!("{}:{}:{}", e.owner, e.name, t))
        })
        .collect();
    parts.sort();
    parts.join(",")
}

/// 当前生效的「渲染器 × 工具名」条目数（诊断 / 测试用）。
pub fn active_tool_renderer_count() -> usize {
    let reg = registry().lock().unwrap();
    reg.iter()
        .filter(|e| is_entry_active(&e.owner))
        .map(|e| e.implementation.tools().len())
        .sum()
}

/// 清空整表（仅测试：全局注册表跨测试累积会污染并行的渲染断言）。
#[cfg(test)]
pub fn clear_all() {
    registry().lock().unwrap().clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{
        Extension, ExtensionMode, register_extension, set_extension_mode, unregister_extension,
    };

    /// 断言固定文本的假渲染器。
    struct Fake {
        /// 自报的渲染器名
        name: &'static str,
        /// 接管的工具名
        tools: &'static [&'static str],
        /// 产出的唯一一行文本
        text: &'static str,
        /// 折叠时保留的视觉行数
        preview: Option<usize>,
    }

    impl ToolRenderer for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn tools(&self) -> &[&'static str] {
            self.tools
        }
        fn render(&self, ctx: &ToolRenderCtx<'_>) -> Option<ToolRender> {
            Some(ToolRender {
                lines: vec![
                    vec![RichSpan::plain(self.text)],
                    vec![RichSpan::plain(format!(
                        "{}|{}|{}",
                        ctx.tool,
                        ctx.result
                            .map(|(t, e)| format!("{t}/{e}"))
                            .unwrap_or_default(),
                        ctx.expanded
                    ))],
                ],
                preview_lines: self.preview,
            })
        }
    }

    /// 渲染器随扩展启停生效：注册后能查到，禁用/注销后回退（返回 None）。
    #[test]
    fn renderer_follows_extension_activation() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        set_extension_mode(ExtensionMode::All);

        struct Faker;
        impl Extension for Faker {
            fn name(&self) -> &str {
                "renderer-owner"
            }
            fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
                Vec::new()
            }
            fn tool_renderers(&self) -> Vec<RegisteredToolRenderer> {
                vec![RegisteredToolRenderer::new(Arc::new(Fake {
                    name: "fake-mcp",
                    tools: &["fake-tool"],
                    text: "from-extension",
                    preview: Some(3),
                }))]
            }
        }

        clear_all();
        register_extension(Faker);
        assert!(tool_renderer_for("fake-tool").is_some(), "注册后应命中");
        assert!(tool_renderer_for("read").is_none(), "未声明的工具名不接管");
        assert_eq!(active_tool_renderer_count(), 1);
        let fp = tool_renderers_fingerprint();
        assert!(fp.contains("renderer-owner:fake-mcp:fake-tool"), "{fp}");

        // 禁用扩展 → 立即回退内置渲染
        crate::core::extensions::set_extension_enabled("renderer-owner", false);
        assert!(tool_renderer_for("fake-tool").is_none(), "禁用后不应再接管");
        assert_eq!(tool_renderers_fingerprint(), "", "禁用后指纹不应包含它");
        crate::core::extensions::set_extension_enabled("renderer-owner", true);
        assert!(tool_renderer_for("fake-tool").is_some());

        assert!(unregister_extension("renderer-owner"));
        assert!(tool_renderer_for("fake-tool").is_none(), "注销后不应再接管");
    }

    /// 同名工具以最后一次注册为准（扩展声明与直注册同理）；
    /// 渲染上下文把工具名 / 结果 / 展开态透传给实现，且同 owner 重登记是替换而非叠加。
    #[test]
    fn last_registration_wins_and_context_is_forwarded() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        set_extension_mode(ExtensionMode::All);
        clear_all();

        struct FakerA;
        impl Extension for FakerA {
            fn name(&self) -> &str {
                "renderer-owner-a"
            }
            fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
                Vec::new()
            }
            fn tool_renderers(&self) -> Vec<RegisteredToolRenderer> {
                vec![RegisteredToolRenderer::new(Arc::new(Fake {
                    name: "a",
                    tools: &["fake-tool"],
                    text: "a-text",
                    preview: None,
                }))]
            }
        }

        register_extension(FakerA);
        register_tool_renderer(Arc::new(Fake {
            name: "b",
            tools: &["fake-tool"],
            text: "b-text",
            preview: Some(2),
        }));

        let args = serde_json::json!({"server": "docs"});
        let ctx = ToolRenderCtx {
            tool: "fake-tool",
            args: &args,
            result: Some(("out", true)),
            duration_ms: Some(12),
            expanded: false,
            width: 40,
            inner_width: 38,
        };
        let rendered = render_tool_block("fake-tool", &ctx).expect("应命中最后注册的渲染器");
        assert_eq!(rendered.lines[0][0].text, "b-text");
        assert_eq!(rendered.lines[1][0].text, "fake-tool|out/true|false");
        assert_eq!(rendered.preview_lines, Some(2));
        assert_eq!(active_tool_renderer_count(), 2, "两个来源各一条");

        // 无归属（直注册）再登记一次：替换自己而不是叠加（否则旧实现永远胜出）
        register_tool_renderer(Arc::new(Fake {
            name: "b2",
            tools: &["fake-tool"],
            text: "b2-text",
            preview: None,
        }));
        assert_eq!(active_tool_renderer_count(), 2, "owner-a 一条 + 直注册一条");
        assert_eq!(
            render_tool_block("fake-tool", &ctx).unwrap().lines[0][0].text,
            "b2-text"
        );
        assert!(
            tool_renderers_fingerprint().contains(STANDALONE_OWNER),
            "直注册条目也进指纹"
        );

        unregister_tool_renderers(STANDALONE_OWNER);
        assert!(unregister_extension("renderer-owner-a"));
        clear_all();
    }
}

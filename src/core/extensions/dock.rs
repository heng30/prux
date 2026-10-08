//! 停靠面板扩展接口：状态栏上方的可滚动面板
//!
//! - 内容行由 [`crate::core::extensions::Extension::dock_lines`] 提供；
//!   每个已启用扩展都可声明；TUI 每帧收集**所有**返回非空行的扩展，
//!   逐段堆叠渲染（段间空行分割），各自独立滚动；
//! - 空列表 = 无停靠内容，TUI 自动隐藏面板（如 plan-mode 关闭后 todos 清空）；
//! - 每段可见性由提供方自己控制（如 plan-mode 经 /todos 或 ShowDock 决定）；
//!   提供方在"有内容要展示"时经 [`crate::core::extensions::request_show_dock`] 请求打开面板
//!   （plan-mode 执行 / goal 激活 / 子代理派发 / 工作流开跑），无需用户手动 /dock；
//!   反过来，"这一轮的事"结束后（如子代理全部完成）由提供方在用户提交输入时
//!   经 [`crate::core::extensions::dispatch_user_submit`] 收敛展示，面板随之自动隐藏；
//! - 锚点（[`crate::core::extensions::Extension::dock_anchor`]）：
//!   面板打开时视口对齐到该行（0 起，含标题行），None = 从顶部开始。

use super::{hooks::ExtensionHook, registry};
use std::borrow::Cow;

/// 停靠面板一行 = 多段带样式文本
pub type DockLine = Vec<DockSpan>;

/// 停靠面板中的一段带样式文本（主题键名 + 缺省色，渲染时经 `Theme::style` 着色；
/// `key` 为空且 `fallback` 也为空 = 终端默认前景，不套主题）。
///
/// `fallback` 用 `Cow` 而非 `&'static str`：动态色（如 agent 类型的 `color` 十六进制值）
/// 在每帧渲染时才知道，`key == ""` + `fallback == "#RRGGBB"` 即"按该十六进制着色"。
#[derive(Debug, Clone)]
pub struct DockSpan {
    /// 主题键名（如 "accent"）；空串 = 不查主题表。
    pub key: &'static str,
    /// 主题键缺失或为空时的缺省色（色名或 `#RRGGBB`）。
    pub fallback: Cow<'static, str>,
    /// 本段显示的文本内容。
    pub text: String,
}

impl DockSpan {
    /// 构造一段停靠文本：`key` 为主题键名（空串表示不查主题表），
    /// `fallback` 是主题键缺失时的缺省色。
    pub fn new(
        key: &'static str,
        fallback: impl Into<Cow<'static, str>>,
        text: impl Into<String>,
    ) -> Self {
        DockSpan {
            key,
            fallback: fallback.into(),
            text: text.into(),
        }
    }

    /// 按十六进制色着色（不查主题表；`#RRGGBB` 或主题色名如 `red` 均可）
    pub fn hex(fallback: impl Into<Cow<'static, str>>, text: impl Into<String>) -> Self {
        DockSpan {
            key: "",
            fallback: fallback.into(),
            text: text.into(),
        }
    }

    /// 无主题色（终端默认前景）
    pub fn plain(text: impl Into<String>) -> Self {
        DockSpan {
            key: "",
            fallback: Cow::Borrowed(""),
            text: text.into(),
        }
    }
}

/// 一个提供者的停靠段（扩展名 + 内容行；渲染层按段堆叠、独立滚动）
#[derive(Debug, Clone)]
pub struct DockSection {
    /// 提供该段的扩展名（按注册顺序堆叠）。
    pub provider: String,
    /// 该扩展当前返回的内容行（非空才成段）。
    pub lines: Vec<DockLine>,
}

/// 当前停靠内容：返回所有提供非空行的已启用扩展的段（按注册顺序）。
/// 无扩展提供内容时返回空（面板不渲染）。
/// 每段起始滚动位置由提供方在其切换命令里设置（见 plan-mode 锚定）。
pub fn dock_sections() -> Vec<DockSection> {
    let mut out = Vec::new();
    for ext in registry::registered() {
        if !ext.hooks().contains(&ExtensionHook::Dock) {
            continue;
        }

        let lines = ext.dock_lines();
        if lines.is_empty() {
            continue;
        }
        out.push(DockSection {
            provider: ext.name().to_string(),
            lines,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::extensions::{
            Extension, ExtensionMode, ExtensionTool, register_extension, set_extension_mode,
            unregister_extension,
        },
        test_support::AUTH_TEST_LOCK,
    };

    struct FakeDock(&'static str);

    impl Extension for FakeDock {
        fn name(&self) -> &str {
            self.0
        }

        fn tools(&self) -> Vec<ExtensionTool> {
            Vec::new()
        }

        fn hooks(&self) -> Vec<ExtensionHook> {
            vec![ExtensionHook::Dock]
        }

        fn dock_lines(&self) -> Vec<DockLine> {
            vec![vec![DockSpan::plain(self.0)]]
        }
    }

    /// 停靠段按注册顺序（= 扩展优先级）堆叠：goal(90) → plan-mode(85) → tasks(80) → subagent(78)。
    /// 各段在输出中连续出现、互不合并；渲染层随后在段间插空行、整块共用一个滚动视口。
    /// 这条顺序保证这些段连续相邻，不会被其它段穿插而难以阅读。
    #[test]
    fn sections_keep_registration_order() {
        let _g = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        let names = [
            "zz-dock-goal",
            "zz-dock-plan",
            "zz-dock-tasks",
            "zz-dock-subagent",
        ];
        for n in names {
            register_extension(FakeDock(n));
        }

        let providers: Vec<String> = dock_sections()
            .into_iter()
            .map(|s| s.provider)
            .filter(|p| p.starts_with("zz-dock-"))
            .collect();

        for n in names {
            assert!(unregister_extension(n), "清理 {n}");
        }

        assert_eq!(providers, names, "停靠段按注册顺序堆叠");
    }
}

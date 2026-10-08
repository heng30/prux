//! banner 扩展接口
//!
//! - 实现 [`BannerExtension`]（`render` 出多行带样式文本）；
//! - 用 [`super::registry::register_banner_extension`] 注册；
//! - TUI 仅在"干净启动"时于窗口顶部渲染 banner：扩展已启用、会话无历史消息、
//!   无启动初始消息，且用户尚未提交过输入。提交后 banner 永久隐藏（本进程内）。
//!
//! banner 与 footer 同构但用途不同：footer 常驻底部、订阅 agent 事件；banner
//! 只在启动时显示一次，无状态、不订阅事件，因此 ctx 只带渲染尺寸。

use super::ExtensionMode;

/// 横幅一行 = 多段带样式文本
pub type BannerLine = Vec<BannerSpan>;

/// 横幅中的一段带样式文本（主题键名 + 缺省色，渲染时经 `Theme::style` 着色）
#[derive(Debug, Clone)]
pub struct BannerSpan {
    /// 主题表里取色用的样式标识；与 fallback 同为空串时用默认样式
    pub key: &'static str,
    /// 主题中找不到该标识时使用的缺省色（十六进制色值）
    pub fallback: &'static str,
    /// 该片段实际显示的文本内容
    pub text: String,
    /// 是否加粗（渲染时叠加 `Modifier::BOLD`）
    pub bold: bool,
}

impl BannerSpan {
    /// 构造非加粗的横幅片段；`key`/`fallback` 为取色用的主题键与缺省色。
    pub fn new(key: &'static str, fallback: &'static str, text: impl Into<String>) -> Self {
        BannerSpan {
            key,
            fallback,
            text: text.into(),
            bold: false,
        }
    }

    /// 带加粗标记的 span（版本行用）
    pub fn new_bold(key: &'static str, fallback: &'static str, text: impl Into<String>) -> Self {
        BannerSpan {
            key,
            fallback,
            text: text.into(),
            bold: true,
        }
    }
}

/// 渲染一帧横幅所需的上下文
#[derive(Debug, Clone, Copy)]
pub struct BannerCtx {
    /// 横幅可用宽度（显示列数）
    pub width: usize,
    /// 终端总高度（行数）：横幅需与消息区/状态栏/输入区共享，太矮时自行隐藏
    pub height: usize,
}

/// 横幅扩展接口
///
/// 横幅是启动时的一次性静态展示：实现只提供 `render`，无事件订阅与状态。
/// 返回空行列表 = 不显示（扩展可按宽高自行决定隐藏，如窄终端放不下就不渲染）。
pub trait BannerExtension: Send + Sync {
    /// 扩展名（诊断用）
    fn name(&self) -> &str;

    /// 扩展描述（/extension 面板详情展示）
    fn description(&self) -> &str {
        ""
    }

    /// 该扩展声明的模式列表（每个声明即一个级别；`All` 为兜底隐式全有，无需写出）
    fn modes(&self) -> Vec<ExtensionMode> {
        Vec::new()
    }

    /// 渲染横幅，返回显示行（每行由带主题样式的片段组成；TUI 按行数分配高度）
    fn render(&self, ctx: &BannerCtx) -> Vec<BannerLine>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ctx_is_copy_and_defaults_to_all_implicitly() {
        struct Spy;
        impl BannerExtension for Spy {
            fn name(&self) -> &str {
                "spy"
            }
            fn render(&self, _ctx: &BannerCtx) -> Vec<BannerLine> {
                Vec::new()
            }
        }
        let ctx = BannerCtx {
            width: 80,
            height: 24,
        };
        let copied = ctx;
        assert_eq!(copied.width, 80);
        assert_eq!(copied.height, 24);
        assert_eq!(Spy.modes(), Vec::<ExtensionMode>::new());
        assert!(Spy.description().is_empty());
    }

    #[test]
    fn span_bold_flag_constructors() {
        let plain = BannerSpan::new("dim", "#666", "x");
        assert!(!plain.bold, "普通 span 不加粗");
        let bold = BannerSpan::new_bold("accent", "#8abeb7", "x");
        assert!(bold.bold, "new_bold 标记加粗");
        assert_eq!(bold.text, "x");
    }
}

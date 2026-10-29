//! 设计令牌：中性石墨表面 + 珊瑚强调色，明暗两套。
//!
//! 颜色只用来表达含义（状态、强调），结构一律靠中性层级与描边区分；
//! 这是 GUI 侧唯一的配色来源，不跟随 TUI 的多主题体系。

use crate::utils::color::Rgb;
use gpui::{Hsla, Pixels, rgb, rgba};
use std::sync::atomic::{AtomicU32, Ordering};
use syntect::highlighting::{
    Color, FontStyle, ScopeSelectors, StyleModifier, Theme as SynTheme, ThemeItem, ThemeSettings,
};

/// 界面缩放系数的下限：再小中文就糊了。
pub const UI_SCALE_MIN: f32 = 0.8;
/// 界面缩放系数的上限：放大到 5 倍仍允许（窗口不够宽时侧栏/输入框会被挤，用户自己缩回）。
pub const UI_SCALE_MAX: f32 = 5.0;
/// 每档缩放的步进（Ctrl+= / Ctrl+- 一次调多少）。
pub const UI_SCALE_STEP: f32 = 0.1;

/// 当前界面缩放系数，存千分比整数（1000 = 1.0）：全局唯一，渲染时由 [`px`] 读取。
static UI_SCALE_PERMILLE: AtomicU32 = AtomicU32::new(1000);

/// 界面默认字体族：编译期内嵌的思源黑体 CN（`assets/fonts/SourceHanSansCN.otf`）。
pub const UI_FONT_FAMILY: &str = "Source Han Sans CN";

// ── 间距 / 圆角 / 字号阶梯 ──────────────────────────────────────────
//
// 所有尺寸都是**设计稿值**，使用时包一层 [`px`] 随界面缩放。
// 布局的 padding / margin / gap 与圆角只允许取下面的阶梯值（全部为 4 的倍数），
// 不要在调用点写裸数字——否则同一个间距会在几十处各自变成 6、7、10、14。

/// 间距阶梯极细档：只用于 `gap` 这类相邻元素的细缝，不可用作外层 padding。
pub const SPACE_XXS: f32 = 2.;
/// 间距阶梯最小档：紧凑元信息行、图标与文本之间。
pub const SPACE_XS: f32 = 4.;
/// 间距阶梯次小档：列表行内边距、相邻块之间。
pub const SPACE_SM: f32 = 8.;
/// 间距阶梯中档：卡片内边距、区块之间。
pub const SPACE_MD: f32 = 12.;
/// 间距阶梯大档：面板内边距、分组之间。
pub const SPACE_LG: f32 = 16.;
/// 间距阶梯最大档：对话流两侧留白与窗口级外边距。
pub const SPACE_XL: f32 = 24.;

/// 圆角最小档：行内小标签、悬停涂层。
pub const RADIUS_SM: f32 = 4.;
/// 圆角中档：按钮、输入框、列表行。
pub const RADIUS_MD: f32 = 8.;
/// 圆角大档：卡片、弹出面板。
pub const RADIUS_LG: f32 = 12.;

/// 最小字号：元信息、时间戳、路径（可读下限）。
pub const FONT_CAPTION: f32 = 11.;
/// 小号字：次要说明、工具输出。
pub const FONT_SMALL: f32 = 12.;
/// 正文字号：对话流与输入框。
pub const FONT_BODY: f32 = 13.;
/// 大号字：标题、横幅。
pub const FONT_HERO: f32 = 22.;

/// 小控件高度：chip、标签、行内小按钮。
pub const CONTROL_H_SM: f32 = 20.;
/// 标准控件高度：页签、主按钮、列表行。
pub const CONTROL_H_MD: f32 = 24.;
/// 大控件高度：工具条按钮、搜索框。
pub const CONTROL_H_LG: f32 = 32.;
/// 标题栏高度（窗口头、面板头）。
pub const HEADER_H: f32 = 40.;
/// 图标尺寸：与文本基线对齐的小方块（圆点、状态灯）。
pub const ICON_SIZE: f32 = 16.;
/// 状态圆点直径：列表行前的品牌/状态点。
pub const DOT_SIZE: f32 = 8.;

/// 界面配色。
#[derive(Clone)]
pub struct Theme {
    /// 是否暗色
    pub is_dark: bool,
    /// 主画布底色
    pub canvas: Hsla,
    /// 侧栏底色
    pub sidebar: Hsla,
    /// 抬升表面（用户气泡、卡片）
    pub raised: Hsla,
    /// 输入框底色
    pub composer: Hsla,
    /// 内嵌区域（代码块、工具输出）
    pub inset: Hsla,
    /// 悬停/选中薄涂层
    pub overlay: Hsla,
    /// 更强的选中涂层
    pub overlay_strong: Hsla,
    /// 常规描边
    pub border: Hsla,
    /// 强调描边
    pub border_strong: Hsla,
    /// 主文本
    pub text: Hsla,
    /// 次要文本
    pub text_secondary: Hsla,
    /// 三级文本（提示、元信息）
    pub text_tertiary: Hsla,
    /// 几乎不可见（占位符）
    pub text_ghost: Hsla,
    /// 品牌强调色（光标、焦点、发送按钮外的高亮）
    pub accent: Hsla,
    /// 链接色
    pub link: Hsla,
    /// 成功
    pub success: Hsla,
    /// 警告
    pub warning: Hsla,
    /// 错误
    pub danger: Hsla,
    /// 行内代码前景
    pub code_text: Hsla,
    /// 主按钮底色（深色主题为浅色）
    pub inverse: Hsla,
    /// 主按钮前景
    pub on_inverse: Hsla,
    /// 文本选区
    pub selection: Hsla,
}

impl Theme {
    /// 暗色主题。
    pub fn dark() -> Self {
        Self {
            is_dark: true,
            canvas: rgb(0x1A1A1A).into(),
            sidebar: rgb(0x181818).into(),
            raised: rgb(0x232323).into(),
            composer: rgb(0x212121).into(),
            inset: rgb(0x151515).into(),
            overlay: rgba(0xFFFFFF0D).into(),
            overlay_strong: rgba(0xFFFFFF17).into(),
            border: rgba(0xFFFFFF12).into(),
            border_strong: rgba(0xFFFFFF24).into(),
            text: rgb(0xE2E2E2).into(),
            text_secondary: rgb(0xA3A3A3).into(),
            text_tertiary: rgb(0x7D7D7D).into(),
            text_ghost: rgb(0x575757).into(),
            accent: rgb(0xE2795B).into(),
            link: rgb(0x6BA6F5).into(),
            success: rgb(0x62C987).into(),
            warning: rgb(0xE0B36A).into(),
            danger: rgb(0xE2726A).into(),
            code_text: rgb(0xE0A882).into(),
            inverse: rgb(0xE7E9EC).into(),
            on_inverse: rgb(0x17181C).into(),
            selection: rgba(0x3B82F58C).into(),
        }
    }

    /// 亮色主题。
    pub fn light() -> Self {
        Self {
            is_dark: false,
            canvas: rgb(0xF6F5F6).into(),
            sidebar: rgb(0xF0EFF0).into(),
            raised: rgb(0xFFFFFF).into(),
            composer: rgb(0xFFFFFF).into(),
            inset: rgb(0xE6E6E6).into(),
            overlay: rgba(0x0000000D).into(),
            overlay_strong: rgba(0x00000017).into(),
            border: rgba(0x00000014).into(),
            border_strong: rgba(0x00000026).into(),
            text: rgb(0x242424).into(),
            text_secondary: rgb(0x666666).into(),
            text_tertiary: rgb(0x858585).into(),
            text_ghost: rgb(0xA4A4A4).into(),
            accent: rgb(0xC85F44).into(),
            link: rgb(0x2563EB).into(),
            success: rgb(0x2F8F52).into(),
            warning: rgb(0xA66B20).into(),
            danger: rgb(0xC64A42).into(),
            code_text: rgb(0x9A5528).into(),
            inverse: rgb(0x202227).into(),
            on_inverse: rgb(0xF8F8F9).into(),
            selection: rgba(0x3B82F559).into(),
        }
    }

    /// GUI 只有明暗两套令牌：`--theme light`（或 settings 里的亮色主题名）走亮色，其余暗色。
    pub fn is_dark_theme(name: Option<&str>) -> bool {
        match name {
            Some(name) => !name.to_lowercase().contains("light"),
            None => true,
        }
    }

    /// 按明暗选择。
    pub fn new(is_dark: bool) -> Self {
        if is_dark { Self::dark() } else { Self::light() }
    }

    /// RGB（语法高亮解析出来的具体色）→ gpui 颜色。
    pub fn rgb(&self, rgb: Rgb) -> Hsla {
        gpui::rgb(u32::from_be_bytes([0, rgb.r, rgb.g, rgb.b])).into()
    }

    /// syntect 主题：把语法作用域映射到本套令牌（注释/字符串/关键字/数字/函数名）。
    pub fn syntect(&self) -> SynTheme {
        let to_color = |color: Hsla| -> Color {
            let rgba: gpui::Rgba = color.into();
            Color {
                r: (rgba.r * 255.0).round() as u8,
                g: (rgba.g * 255.0).round() as u8,
                b: (rgba.b * 255.0).round() as u8,
                a: 255,
            }
        };

        let item = |scope: &str, fg: Hsla, italic: bool| ThemeItem {
            scope: scope.parse::<ScopeSelectors>().expect("scope invalid"),
            style: StyleModifier {
                foreground: Some(to_color(fg)),
                background: None,
                font_style: italic.then_some(FontStyle::ITALIC),
            },
        };

        SynTheme {
            name: None,
            author: None,
            settings: ThemeSettings {
                foreground: Some(to_color(self.text)),
                background: Some(to_color(self.inset)),
                ..Default::default()
            },
            scopes: vec![
                item("comment", self.text_tertiary, true),
                item("string", self.success, false),
                item("constant.numeric", self.warning, false),
                item("constant", self.warning, false),
                item("keyword", self.accent, false),
                item("storage", self.accent, false),
                item("entity.name.function", self.code_text, false),
                item("support.function", self.code_text, false),
                item("entity.name.type", self.warning, false),
                item("support.type", self.warning, false),
                item("variable", self.text, false),
                item("punctuation", self.text_secondary, false),
            ],
        }
    }
}

/// 当前界面缩放系数（1.0 = 设计稿尺寸）。
pub fn ui_scale() -> f32 {
    UI_SCALE_PERMILLE.load(Ordering::Relaxed) as f32 / 1000.0
}

/// 设置界面缩放系数，返回夹到 [`UI_SCALE_MIN`]..=[`UI_SCALE_MAX`] 后的生效值。
pub fn set_ui_scale(scale: f32) -> f32 {
    let clamped = scale.clamp(UI_SCALE_MIN, UI_SCALE_MAX);
    UI_SCALE_PERMILLE.store((clamped * 1000.0).round() as u32, Ordering::Relaxed);
    clamped
}

/// 按当前界面缩放系数换算设计稿像素：GUI 里所有尺寸都走它，字号与间距才会一起缩放。
pub fn px(value: f32) -> Pixels {
    gpui::px(value * ui_scale())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 两套主题的核心层级必须可分辨（底色/文本/描边互不相同）。
    #[test]
    fn tiers_are_distinct() {
        for theme in [Theme::dark(), Theme::light()] {
            assert_ne!(theme.canvas, theme.sidebar);
            assert_ne!(theme.canvas, theme.raised);
            assert_ne!(theme.text, theme.text_secondary);
            assert_ne!(theme.border, theme.border_strong);
            assert_ne!(theme.accent, theme.text);
        }
    }

    /// 布局阶梯必须是 4 的倍数（只有极细档 `SPACE_XXS` 允许 2），防止又冒出 15 这种随手值。
    #[test]
    fn layout_steps_are_multiples_of_four() {
        for value in [
            SPACE_XS,
            SPACE_SM,
            SPACE_MD,
            SPACE_LG,
            SPACE_XL,
            RADIUS_SM,
            RADIUS_MD,
            RADIUS_LG,
            CONTROL_H_SM,
            CONTROL_H_MD,
            CONTROL_H_LG,
            HEADER_H,
            ICON_SIZE,
            DOT_SIZE,
        ] {
            assert_eq!(value % 4.0, 0.0, "{value} 不是 4 的倍数");
        }
        assert!(SPACE_XXS < SPACE_XS);
        assert!(FONT_CAPTION < FONT_SMALL && FONT_SMALL < FONT_BODY);
    }

    /// syntect 主题能构建（作用域解析不 panic）。
    #[test]
    fn syntect_theme_builds() {
        let theme = Theme::dark().syntect();
        assert!(!theme.scopes.is_empty());
    }
}

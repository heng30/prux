//! 扩展与导出共用的「当前主题」只读视图。
//!
//! 主题解析（主题文件、`system` 主题的终端推导、`oklch()`/`okhsl()` 换算）在 TUI 侧完成，
//! 核心与扩展拿不到那份 `Theme`；因此 TUI 每帧把生效主题的**解析结果**发布到这里，供两处消费：
//!
//! - 扩展渲染（如需要具体颜色做配色运算、或按明暗外观选素材）；
//! - HTML 导出（`system` 这类运行时推导的主题只有 TUI 知道，按主题名从文件加载拿不到）。
//!
//! 未发布（CLI 导出、SDK/无 TUI 场景）时 [`get`] 返回 `None`，消费方各自回退。

use crate::utils::color::{Rgb, color_to_rgb, parse_color_str};
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
};

/// 已生效主题的只读视图：外观 + 解析后的语义色表。
///
/// 与 `RichSpan` / `DockSpan` 的「主题键」不同：这里的值是**具体颜色**，
/// 供需要真实色值的场景（配色运算、导出、按明暗选素材）使用；
/// 纯渲染仍应传主题键，让 TUI 每帧解析。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeView {
    /// 生效主题名（`system` / `dark` / 自定义名）。
    pub name: String,
    /// 明暗外观：`"dark"` 或 `"light"`。
    pub appearance: String,
    /// 语义色键 → 具体颜色（`#rrggbb`）；主题把某键设为终端默认色时，
    /// 这里给的是终端上报的默认前景色（未上报时按外观猜的黑/白）。
    pub colors: BTreeMap<String, String>,
    /// 终端默认前景色：主题色为空（终端默认）时的替代值。
    pub default_fg: String,
    /// 终端默认背景色：同上，供需要与背景混合的配色运算使用。
    pub default_bg: String,
}

impl ThemeView {
    /// 当前外观是否为浅色（`appearance == "light"`）。
    pub fn is_light(&self) -> bool {
        self.appearance == "light"
    }

    /// 查语义色的具体值；键不存在时返回 `None`。
    pub fn color(&self, key: &str) -> Option<&str> {
        self.colors.get(key).map(String::as_str)
    }

    /// 语义色 → 前景 ANSI 转义；键不存在时用 `default_fg`。
    pub fn fg_ansi(&self, key: &str) -> String {
        ansi(self.color(key).unwrap_or(&self.default_fg), false)
    }

    /// 语义色 → 背景 ANSI 转义；键不存在时用 `default_bg`。
    pub fn bg_ansi(&self, key: &str) -> String {
        ansi(self.color(key).unwrap_or(&self.default_bg), true)
    }
}

/// 颜色字符串 → `truecolor` ANSI 转义；无法解析时回退为终端默认（重置前景/背景）。
fn ansi(color: &str, background: bool) -> String {
    match parse_color_str(color) {
        Some(color) => {
            let Rgb { r, g, b } = color_to_rgb(color);
            if background {
                format!("\x1b[48;2;{r};{g};{b}m")
            } else {
                format!("\x1b[38;2;{r};{g};{b}m")
            }
        }
        None if background => "\x1b[49m".to_string(),
        None => "\x1b[39m".to_string(),
    }
}

/// 进程级当前主题视图槽位
fn slot() -> &'static Mutex<Option<ThemeView>> {
    static SLOT: OnceLock<Mutex<Option<ThemeView>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// 发布当前生效主题（TUI 在主题加载/切换后调用；主题未变时调用方应自行短路）。
pub fn set(view: ThemeView) {
    *slot().lock().unwrap() = Some(view);
}

/// 读取当前生效主题；未发布（无 TUI / 未加载主题）时返回 `None`。
pub fn get() -> Option<ThemeView> {
    slot().lock().unwrap().clone()
}

/// 清空发布（测试用：避免用例之间串主题）。
#[cfg(test)]
pub(crate) fn clear() {
    *slot().lock().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 视图查询与 ANSI 输出：命中语义色用该色，缺键用默认前景/背景。
    #[test]
    fn colors_and_ansi_helpers() {
        let view = ThemeView {
            name: "test".to_string(),
            appearance: "light".to_string(),
            colors: BTreeMap::from([("accent".to_string(), "#8abeb7".to_string())]),
            default_fg: "#000000".to_string(),
            default_bg: "#ffffff".to_string(),
        };

        assert!(view.is_light());
        assert_eq!(view.color("accent"), Some("#8abeb7"));
        assert_eq!(view.color("missing"), None);
        assert_eq!(view.fg_ansi("accent"), "\x1b[38;2;138;190;183m");
        assert_eq!(view.fg_ansi("missing"), "\x1b[38;2;0;0;0m");
        assert_eq!(view.bg_ansi("missing"), "\x1b[48;2;255;255;255m");
    }

    /// 未发布时 `get()` 为 `None`；发布后取到同一份视图。
    #[test]
    fn publish_and_read_back() {
        clear();
        assert!(get().is_none());

        let view = ThemeView {
            name: "system".to_string(),
            appearance: "dark".to_string(),
            ..Default::default()
        };
        set(view.clone());
        assert_eq!(get(), Some(view));
        clear();
        assert!(get().is_none());
    }

    /// 颜色无法解析时回退为终端默认（而不是吐出坏转义）。
    #[test]
    fn unparsable_color_resets() {
        assert_eq!(ansi("", false), "\x1b[39m");
        assert_eq!(ansi("not-a-color", true), "\x1b[49m");
    }
}

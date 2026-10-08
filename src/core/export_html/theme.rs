//! 主题变量与导出配色
//!
//! 默认主题内嵌于程序；额外主题从 `agent_dir()/themes` 加载；
//! 运行中 TUI 的主题（含 `system` 的终端推导结果）见 [`crate::core::theme_view`]。

use crate::{
    core::{
        settings_manager,
        theme_view::{self, ThemeView},
    },
    embedded,
    utils::color::{color_to_rgb, parse_color_str},
};
use serde_json::{Value, json};
use std::{collections::HashMap, path::PathBuf};

/// 内嵌的 dark 主题 JSON 源文本，用户主题文件缺失时作为回退。
const EMBEDDED_DARK: &str = embedded!("themes/dark.json");
/// 内嵌的 light 主题 JSON 源文本，供 `light` 主题名直接加载。
const EMBEDDED_LIGHT: &str = embedded!("themes/light.json");

/// 运行中 TUI 发布的当前主题（主题名一致时才认：导出显式指定了别的主题时不得被顶替）。
fn live_view(theme_name: &str) -> Option<ThemeView> {
    let view = theme_view::get()?;
    (view.name == theme_name).then_some(view)
}

/// 用户主题文件路径 `agent_dir()/themes/<name>.json`。
fn agent_theme_path(theme_name: &str) -> PathBuf {
    settings_manager::agent_dir()
        .join("themes")
        .join(format!("{}.json", theme_name))
}

/// 加载主题：优先 `agent_dir()/themes/<name>.json`（可覆盖默认），
/// 其次内嵌默认主题，最后回退内嵌 dark。
fn load_theme_value(theme_name: &str) -> Value {
    let agent_path = agent_theme_path(theme_name);
    if let Ok(text) = std::fs::read_to_string(&agent_path)
        && let Ok(v) = serde_json::from_str::<Value>(&text)
    {
        return v;
    }
    let embedded = match theme_name {
        "dark" => Some(EMBEDDED_DARK),
        "light" => Some(EMBEDDED_LIGHT),
        _ => None,
    };
    if let Some(src) = embedded
        && let Ok(v) = serde_json::from_str::<Value>(src)
    {
        return v;
    }
    serde_json::from_str(EMBEDDED_DARK).unwrap_or_else(|_| json!({}))
}

/// 解析 `#rrggbb` 十六进制颜色；格式不符时返回 `None`。
fn parse_color(color: &str) -> Option<(u8, u8, u8)> {
    if let Some(hex) = color.strip_prefix('#')
        && hex.len() == 6
    {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some((r, g, b));
    }
    None
}

/// 按 WCAG 相对亮度公式（sRGB 线性化后加权）计算亮度。
fn get_luminance(r: u8, g: u8, b: u8) -> f64 {
    let to_linear = |c: f64| {
        let s = c / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * to_linear(r as f64) + 0.7152 * to_linear(g as f64) + 0.0722 * to_linear(b as f64)
}

/// 按 factor 缩放各通道亮度；非 `#rrggbb` 颜色原样返回。
fn adjust_brightness(color: &str, factor: f64) -> String {
    if let Some((r, g, b)) = parse_color(color) {
        let adj = |c: u8| (c as f64 * factor).round().clamp(0.0, 255.0) as u8;
        format!("rgb({}, {}, {})", adj(r), adj(g), adj(b))
    } else {
        color.to_string()
    }
}

/// ansi256 索引转 hex（主题颜色可能用数字表示）
fn ansi256_to_hex(index: u16) -> String {
    /// 标准 16 个 ANSI 基色的 hex 值，用于换算 0~15 号颜色。
    const BASIC: [&str; 16] = [
        "#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0",
        "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff",
    ];
    if index < 16 {
        return BASIC[index as usize].to_string();
    }
    if index < 232 {
        let cube = index - 16;
        let to_hex = |n: u16| {
            let v = if n == 0 { 0 } else { 55 + n * 40 };
            format!("{:02x}", v)
        };
        return format!(
            "#{}{}{}",
            to_hex(cube / 36),
            to_hex((cube % 36) / 6),
            to_hex(cube % 6)
        );
    }
    let gray = 8 + (index - 232) * 10;
    let g = format!("{:02x}", gray);
    format!("#{}{}{}", g, g, g)
}

/// 解析 colors 引用：值可能是 vars 键名（如 `"muted": "neutral"`），
/// 也可能是 #hex / rgb() / 空串 / ansi256 数字。循环解析最多 8 层防循环。
fn resolve_theme_value(value: &Value, vars: &HashMap<String, Value>) -> String {
    let mut cur = value.clone();
    for _ in 0..8 {
        match &cur {
            Value::String(s) => {
                if let Some(v) = vars.get(s) {
                    cur = v.clone();
                    continue;
                }
                return s.clone();
            }
            Value::Number(n) => return ansi256_to_hex(n.as_u64().unwrap_or(0) as u16),
            _ => break,
        }
    }
    match cur {
        Value::String(s) => s,
        Value::Number(n) => ansi256_to_hex(n.as_u64().unwrap_or(0) as u16),
        _ => String::new(),
    }
}

/// 颜色规格 → CSS 可用值：`okhsl()` / `oklch()` 先转 `#rrggbb`
/// （CSS 认 `oklch()` 但不认 `okhsl()`，且导出页要的是具体色值），
/// 其余（`#hex` / `rgb()` / `ansi:` / 空串）原样返回。
fn to_css_color(spec: &str) -> String {
    let trimmed = spec.trim();
    if (trimmed.starts_with("okhsl(") || trimmed.starts_with("oklch("))
        && let Some(rgb) = parse_color_str(trimmed).map(color_to_rgb)
    {
        return format!("#{:02x}{:02x}{:02x}", rgb.r, rgb.g, rgb.b);
    }
    spec.to_string()
}

/// 从主题文件生成 CSS vars 与导出配色。
///
/// 优先用运行中 TUI 发布的当前主题视图（[`crate::core::theme_view`]）：`system` 这类
/// **运行时从终端推导**的主题在磁盘上没有文件，按名加载只会得到内嵌 dark，
/// 与用户在 TUI 里实际看到的配色不一致。视图不可用（CLI `--export`、SDK）或
/// 视图里的主题名与请求名不同时，回退到按名加载主题文件。
pub fn load_theme_vars(theme_name: &str) -> (String, String, String, String) {
    let theme = load_theme_value(theme_name);
    let live = live_view(theme_name);

    let mut resolved: HashMap<String, String> = HashMap::new();
    if let Some(view) = &live {
        // 视图里已是解析后的具体颜色（终端默认色也已按上报值/外观确定）
        for (k, v) in &view.colors {
            resolved.insert(k.clone(), v.clone());
        }
    } else {
        // 只输出 colors 段的键，值解析 vars 引用（不再输出 vars 原始键，
        // 否则 `--dim: neutral` 这类未解析引用会覆盖正确颜色导致黑字）。
        let vars: HashMap<String, Value> = theme
            .get("vars")
            .and_then(|v| v.as_object())
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();
        let raw_colors: HashMap<String, Value> = theme
            .get("colors")
            .and_then(|v| v.as_object())
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default();

        // 默认文本色（终端空值语义）：空值 fallback
        let is_light = theme_name == "light";
        let default_text = if is_light { "#000000" } else { "#e5e5e7" };

        for (k, v) in &raw_colors {
            let s = resolve_theme_value(v, &vars);
            let s = if s.is_empty() {
                default_text.to_string()
            } else {
                to_css_color(&s)
            };
            resolved.insert(k.clone(), s);
        }
    }

    if !resolved.contains_key("thinkingMax")
        && let Some(x) = resolved.get("thinkingXhigh")
    {
        resolved.insert("thinkingMax".to_string(), x.clone());
    }

    if !resolved.contains_key("scrollbarThumb")
        && let Some(s) = resolved.get("selectedBg")
    {
        resolved.insert("scrollbarThumb".to_string(), s.clone());
    }

    let mut var_lines: Vec<String> = Vec::new();
    for (k, v) in &resolved {
        var_lines.push(format!("--{}: {};", k, v));
    }

    // 导出配色：优先 theme.export，否则从 userMessageBg（解析后）推导
    let user_msg_bg = resolved
        .get("userMessageBg")
        .cloned()
        .unwrap_or_else(|| "#343541".to_string());
    let (page_bg, card_bg, info_bg) =
        if let Some(exp) = theme.get("export").and_then(|v| v.as_object()) {
            (
                to_css_color(exp.get("pageBg").and_then(|v| v.as_str()).unwrap_or("")),
                to_css_color(exp.get("cardBg").and_then(|v| v.as_str()).unwrap_or("")),
                to_css_color(exp.get("infoBg").and_then(|v| v.as_str()).unwrap_or("")),
            )
        } else {
            (String::new(), String::new(), String::new())
        };

    let (page_bg, card_bg, info_bg) = if page_bg.is_empty() {
        if let Some((r, g, b)) = parse_color(&user_msg_bg) {
            let is_light = get_luminance(r, g, b) > 0.5;
            if is_light {
                (
                    adjust_brightness(&user_msg_bg, 0.96),
                    user_msg_bg.clone(),
                    format!(
                        "rgb({}, {}, {})",
                        (r as u16 + 10).min(255),
                        (g as u16 + 5).min(255),
                        (b as u16).saturating_sub(20)
                    ),
                )
            } else {
                (
                    adjust_brightness(&user_msg_bg, 0.7),
                    adjust_brightness(&user_msg_bg, 0.85),
                    format!(
                        "rgb({}, {}, {})",
                        (r as u16 + 20).min(255),
                        (g as u16 + 15).min(255),
                        b
                    ),
                )
            }
        } else {
            (
                "rgb(24, 24, 30)".to_string(),
                "rgb(30, 30, 36)".to_string(),
                "rgb(60, 55, 40)".to_string(),
            )
        }
    } else {
        (page_bg, card_bg, info_bg)
    };

    var_lines.push(format!("--exportPageBg: {};", page_bg));
    var_lines.push(format!("--exportCardBg: {};", card_bg));
    var_lines.push(format!("--exportInfoBg: {};", info_bg));

    (var_lines.join("\n      "), page_bg, card_bg, info_bg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::theme_view::{self, ThemeView};

    /// 当前主题视图是进程级全局：与其它改动它的用例串行化。
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 无发布视图（CLI 导出）：按主题名从内嵌/文件加载，行为与引入视图前一致。
    #[test]
    fn falls_back_to_theme_file_without_live_view() {
        let _lock = lock();
        theme_view::clear();
        let (vars, page_bg, _, _) = load_theme_vars("dark");
        assert!(vars.contains("--accent:"), "{vars}");
        assert!(!vars.contains("--exportPageBg: ;"), "{vars}");
        assert!(!page_bg.is_empty(), "{page_bg}");
    }

    /// 有发布视图且主题名一致：用 TUI 的解析结果（`system` 这类磁盘上没有文件的主题只有它有）。
    #[test]
    fn uses_live_view_when_name_matches() {
        let _lock = lock();
        theme_view::clear();
        theme_view::set(ThemeView {
            name: "system".to_string(),
            appearance: "light".to_string(),
            colors: std::collections::BTreeMap::from([
                ("accent".to_string(), "#010203".to_string()),
                ("userMessageBg".to_string(), "#f4f4f4".to_string()),
            ]),
            default_fg: "#000000".to_string(),
            default_bg: "#ffffff".to_string(),
        });

        let (vars, _, _, _) = load_theme_vars("system");
        assert!(vars.contains("--accent: #010203;"), "{vars}");
        assert!(vars.contains("--userMessageBg: #f4f4f4;"), "{vars}");

        // 主题名不同（导出显式指定了别的主题）→ 忽略视图，回退按名加载
        let (vars, _, _, _) = load_theme_vars("dark");
        assert!(!vars.contains("--accent: #010203;"), "{vars}");
        theme_view::clear();
    }
}

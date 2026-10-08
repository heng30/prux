//! TUI 主题：从 JSON 主题文件（`agent_dir()/themes`）加载颜色变量

use crate::{
    PROJECT_SCOPE_NAME,
    core::{
        settings_manager,
        theme_view::{self, ThemeView},
    },
    embedded,
    modes::interactive::system_theme::{
        Appearance, SystemThemeInput, generate_indexed_theme_colors, generate_system_theme_colors,
    },
    utils::{
        color::{color_to_rgb, indexed_to_rgb, parse_color_str, rgb_to_oklch},
        terminal_caps::detect_terminal_capabilities,
        terminal_colors::terminal_colors,
    },
};
use ratatui::style::{Color, Modifier, Style};
use serde_json::Value;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

/// xterm 256 色中 6x6x6 颜色立方体每个通道的 6 个分档值（0/95/…/255），
/// 供 [`rgb_to_ansi256`] 做立方体近似。
const CUBE_VALUES: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// 未配置主题时使用的主题名
///
/// `system` 在终端未上报配色时自动回落到内置 dark/light（按 COLORFGBG），
pub const DEFAULT_THEME_NAME: &str = "system";

/// 由终端上报配色推导的主题名
pub const SYSTEM_THEME_NAME: &str = "system";

/// 已加载的主题：主题名 + 颜色变量表 + 渲染功能开关。
///
/// 通过 [`Theme::load`] 从 JSON 主题文件或内建主题构造，
/// 渲染时用 [`Theme::style`]/[`Theme::resolve_color`] 等取色。
#[derive(Debug, Clone)]
pub struct Theme {
    /// 实际生效的主题名（`auto` 已解析为 `dark`/`light`，`system` 保持原名）。
    pub name: String,

    /// 颜色变量表：键为语义色名（`accent`/`success`/…），值为颜色规格字符串
    /// （hex、`oklch()`/`okhsl()`、变量名或空串＝终端默认色）。
    /// 由主题 JSON 的 `vars` 与 `colors` 合并而来，`colors` 覆盖同名键。
    pub vars: HashMap<String, String>,

    /// 代码块语法高亮开关。恢复历史会话启动时先置 false（快速渲染、跳过 syntect），
    /// 后台补全渲染完成后置回 true。关闭时代码块以纯 codeBlock 色渲染。
    pub syntax_highlight: bool,

    /// Mermaid 代码块 → Unicode 流程图渲染开关。
    /// 关闭时 mermaid 代码块以普通代码块（```mermaid 围栏 + 源码）原样展示。
    pub mermaid: bool,

    /// `$...$`/`$$...$$` 公式 → Unicode 渲染开关。
    /// 关闭时保留原始文本。
    pub latex: bool,

    /// 主题面向的明暗外观：主题文件显式声明，或从主题颜色推导；
    /// `system` 主题取终端外观。未知时为 `None`（视作深色）。
    pub appearance: Option<Appearance>,

    /// 需要用 **SGR 2（faint）** 叠加渲染的 token（键名）。
    /// 只有 `system` 主题的索引兜底档会填（见
    /// [`crate::modes::interactive::system_theme::generate_indexed_theme_colors`]）；
    /// 主题文件里中性色本身就是具体颜色，故为空。
    pub dim_keys: HashSet<String>,
}

impl Default for Theme {
    /// 加载内置默认主题（`DEFAULT_THEME_NAME`）。
    fn default() -> Self {
        Theme::load(DEFAULT_THEME_NAME)
    }
}

impl Theme {
    /// 主题色 → （前景色 + 默认背景色）；`dim_keys` 里的键叠加 SGR 2（faint）。
    pub fn style(&self, key: &str, default: &str) -> Style {
        self.with_dim(key, Style::default().fg(color(&self.get(key, default))))
    }

    /// 主题色 → 仅背景色 Style（用于消息背景块，如 toolSuccessBg / userMessageBg）
    pub fn style_bg(&self, key: &str, default: &str) -> Style {
        Style::default().bg(color(&self.get(key, default)))
    }

    /// 前景 + 背景组合样式；前景键在 `dim_keys` 里时叠加 SGR 2（faint）。
    pub fn style_fg_bg(
        &self,
        fg_key: &str,
        fg_default: &str,
        bg_key: &str,
        bg_default: &str,
    ) -> Style {
        self.with_dim(
            fg_key,
            Style::default()
                .fg(color(&self.get(fg_key, fg_default)))
                .bg(color(&self.get(bg_key, bg_default))),
        )
    }

    /// 键在 [`Theme::dim_keys`] 里时叠加 `Modifier::DIM`，否则原样返回。
    fn with_dim(&self, key: &str, style: Style) -> Style {
        if self.dim_keys.contains(key) {
            style.add_modifier(Modifier::DIM)
        } else {
            style
        }
    }

    /// 主题色规格 → `Color`：主题键（`"success"`/`"dim"`/…）或 hex（`"#b5bd68"`）。
    ///
    /// `"text"` 特例：语义是“终端默认前景”（Reset），不走主题里同名的调色板值
    /// （`text` 键是给 userMessageText 等用的具体浅灰色）。
    pub fn resolve_color(&self, spec: &str) -> Color {
        if spec == "text" {
            return Color::Reset;
        }
        // 空串 = 终端默认色（`system` 主题在终端前景足够强时用它）
        let value = self.get(spec, "#d4d4d4");
        if value.is_empty() {
            Color::Reset
        } else {
            color(&value)
        }
    }

    /// 用户消息背景样式（userMessageBg + accent 前景）
    pub fn user_msg_style(&self) -> Style {
        Style::default()
            .fg(color(&self.get("accent", "#8abeb7")))
            .bg(color(&self.get("userMessageBg", "#343541")))
    }

    /// 依次搜索：显式 JSON 路径、项目 PROJECT_SCOPE_NAME/themes、agentDir/themes、内建主题
    /// 终端背景亮暗检测。COLORFGBG 的环境变量含 "fg;bg" 色号，bg 亮(>8) 判 light。
    fn detect_terminal_background() -> bool {
        let Some(cfg) = std::env::var("COLORFGBG").ok() else {
            return false;
        };
        let bg = cfg.rsplit(';').next().and_then(|s| s.parse::<u8>().ok());
        bg.map(|b| b > 8).unwrap_or(false)
    }

    /// 解析主题名：auto → 按终端背景解析为 light/dark
    fn resolve_name(name: &str) -> String {
        match name {
            "auto" => {
                if Self::detect_terminal_background() {
                    "light".to_string()
                } else {
                    "dark".to_string()
                }
            }
            other => other.to_string(),
        }
    }

    /// 按名加载主题：`auto` 先按终端背景解析成 light/dark，`system` 走终端配色推导；
    /// 依次尝试显式路径、项目/agentDir/自定义主题目录、内置主题。
    /// 全部未命中时回落成内置 dark（`light` 则回落 light），不报错。
    pub fn load(name: &str) -> Theme {
        let name = &Self::resolve_name(name);
        if name == SYSTEM_THEME_NAME {
            return Self::load_system();
        }

        let candidates = Self::theme_candidates(name);

        let mut found = false;
        let mut loaded_name = name.clone(); // auto 解析后显示实际主题名（light/dark），而非 "auto"
        let mut vars: HashMap<String, String> = HashMap::new();
        let mut explicit_appearance: Option<Appearance> = None;

        for path in candidates {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            ingest_theme_json(&v, &mut loaded_name, &mut vars, &mut explicit_appearance);
            found = true;
            break;
        }

        // 所有候选均未命中（如未安装外部主题/未配置主题目录）：回退内置默认主题
        if !found {
            let fallback = match name.as_str() {
                "light" => embedded!("themes/light.json"),
                _ => embedded!("themes/dark.json"),
            };

            if let Ok(v) = serde_json::from_str::<Value>(fallback) {
                ingest_theme_json(&v, &mut loaded_name, &mut vars, &mut explicit_appearance);
            }
        }

        let appearance = explicit_appearance
            .or_else(|| detect_theme_appearance(&vars))
            .or_else(|| Some(terminal_appearance_now()));

        Theme {
            name: loaded_name,
            vars,
            syntax_highlight: true,
            mermaid: true,
            latex: true,
            appearance,
            dim_keys: HashSet::new(),
        }
    }

    /// `system` 主题：用终端上报的配色生成整套色板。
    ///
    /// 终端未上报背景（`oklch` 推导无从下手）时改用**索引兜底档**：直接输出 ANSI 索引色，
    /// 由终端按自己的主题渲染（面板不带背景、中性色走 SGR 2），因此任何终端都能贴合。
    fn load_system() -> Theme {
        let colors = terminal_colors();
        let generated = generate_system_theme_colors(&SystemThemeInput {
            foreground: colors.foreground,
            background: colors.background,
            palette: colors.palette.clone(),
            saturation: None,
        })
        .unwrap_or_else(|| {
            // 饱和度 1（与 pi 的稳态一致）：彩色 token 用 ANSI 槽位，中性色走 SGR 2
            generate_indexed_theme_colors(1.0, terminal_appearance_now())
        });

        let vars: HashMap<String, String> = generated
            .colors
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
        let dim_keys: HashSet<String> = generated.dim.iter().map(|k| k.to_string()).collect();

        Theme {
            name: SYSTEM_THEME_NAME.to_string(),
            vars,
            syntax_highlight: true,
            mermaid: true,
            latex: true,
            appearance: Some(generated.appearance),
            dim_keys,
        }
    }

    /// 主题面向的明暗外观（未知时按终端外观）
    pub fn appearance(&self) -> Appearance {
        self.appearance.unwrap_or_else(terminal_appearance_now)
    }

    /// 当前主题的只读视图（解析后的具体色值 + 外观），供扩展与 HTML 导出使用。
    ///
    /// 每个语义键都解析成具体颜色：值为空或解析出终端默认色时，用终端上报的
    /// 默认前景色（未上报则按外观猜黑/白），因为下游（ANSI 输出、CSS）需要真值。
    pub fn view(&self) -> ThemeView {
        let appearance = match self.appearance() {
            Appearance::Light => "light",
            Appearance::Dark => "dark",
        };
        let colors = terminal_colors();
        let guess_fg = if appearance == "light" {
            "#000000"
        } else {
            "#ffffff"
        };
        let guess_bg = if appearance == "light" {
            "#ffffff"
        } else {
            "#000000"
        };
        let default_fg = colors
            .foreground
            .map(|c| c.to_hex())
            .unwrap_or_else(|| guess_fg.to_string());
        let default_bg = colors
            .background
            .map(|c| c.to_hex())
            .unwrap_or_else(|| guess_bg.to_string());

        let mut resolved = BTreeMap::new();
        for key in self.vars.keys() {
            let value = self.get(key, "");
            // 用未降级的 RGB（`theme::color` 在 256 色终端上会退成索引，导出的 CSS/ANSI 要真值）
            let concrete = match ansi_index(&value) {
                Some(index) => indexed_to_rgb(index).to_hex(),
                None => resolve_rgb(&value)
                    .map(|rgb| rgb.to_hex())
                    .unwrap_or_else(|| default_fg.clone()),
            };
            resolved.insert(key.clone(), concrete);
        }

        ThemeView {
            name: self.name.clone(),
            appearance: appearance.to_string(),
            colors: resolved,
            default_fg,
            default_bg,
        }
    }

    /// 把当前主题发布给扩展与 HTML 导出（见 [`ThemeView`]）。
    ///
    /// 每帧调用：指纹未变时直接返回（不重建颜色表），因此主题切换（`/theme`、
    /// 设置项、扩展改主题、系统外观变化）后自动同步，无需每个赋值点各自发布。
    pub fn publish_view(&self) {
        static PUBLISHED: OnceLock<Mutex<Option<String>>> = OnceLock::new();
        let fingerprint = self.render_fingerprint();
        let mut published = PUBLISHED.get_or_init(|| Mutex::new(None)).lock().unwrap();
        if published.as_deref() == Some(fingerprint.as_str()) {
            return;
        }

        *published = Some(fingerprint);
        theme_view::set(self.view());
    }

    /// 收集候选主题文件路径（显式路径 + 项目/配置目录 + 内置 dark 兜底）
    fn theme_candidates(name: &str) -> Vec<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        let name_path = Path::new(name);
        if name_path.extension().and_then(|e| e.to_str()) == Some("json") {
            candidates.push(name_path.to_path_buf());
        }

        if let Ok(cwd) = std::env::current_dir() {
            candidates.push(
                cwd.join(PROJECT_SCOPE_NAME)
                    .join("themes")
                    .join(format!("{}.json", name)),
            );
        }

        for raw in settings_manager::read_settings().themes {
            let p = if raw == "~" {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
            } else if let Some(rest) = raw.strip_prefix("~/") {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string())).join(rest)
            } else {
                PathBuf::from(raw)
            };

            if p.is_dir() {
                candidates.push(p.join(format!("{}.json", name)));
            } else if p.extension().and_then(|e| e.to_str()) == Some("json") {
                candidates.push(p);
            }
        }

        let themes_dir = settings_manager::agent_dir().join("themes");
        candidates.push(themes_dir.join(format!("{}.json", name)));
        candidates.push(themes_dir.join("dark.json"));
        candidates
    }

    /// 渲染缓存指纹：主题中影响渲染的颜色变量（不含 syntax_highlight，
    /// Fast→Full 过渡由后台补全专门处理，不应触发缓存整体失效）。
    /// 主题切换/主题文件重载后指纹变化，render_history_lines 据此重建缓存。
    pub fn render_fingerprint(&self) -> String {
        let mut keys: Vec<&String> = self.vars.keys().collect();
        keys.sort();
        let mut out = String::new();

        for k in keys {
            out.push_str(k);
            out.push('=');
            out.push_str(&self.vars[k]);
            out.push('|');
        }
        format!("{}#{}", self.name, out)
    }

    /// 查主题色值；缺键时返回 `default`。
    /// 值可能是变量名（如 `"accent": "cyan"`），最多沿 `vars` 间接解析 4 层。
    pub fn get(&self, key: &str, default: &str) -> String {
        let mut value = self
            .vars
            .get(key)
            .cloned()
            .unwrap_or_else(|| default.to_string());

        // colors 中的值可能是变量名（如 "accent": "cyan"），解析到 vars
        for _ in 0..4 {
            if let Some(resolved) = self.vars.get(&value) {
                value = resolved.clone();
            } else {
                break;
            }
        }

        value
    }
}

/// 列出 agentDir/themes 与项目 PROJECT_SCOPE_NAME/themes 中可用的主题名
pub fn available_theme_names(cwd: &str, agent_dir: &Path, include_discovered: bool) -> Vec<String> {
    let mut names = HashSet::new();

    if include_discovered {
        let mut dirs = vec![
            agent_dir.join("themes"),
            PathBuf::from(cwd).join(PROJECT_SCOPE_NAME).join("themes"),
        ];

        // settings.json 的 themes 字段：自定义主题目录（与 Theme::load 同一来源）
        for raw in settings_manager::read_settings().themes {
            let p = if raw == "~" {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
            } else if let Some(rest) = raw.strip_prefix("~/") {
                PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string())).join(rest)
            } else {
                PathBuf::from(raw)
            };

            if p.is_dir() {
                dirs.push(p);
            }
        }

        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };

            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }

                if let Ok(text) = std::fs::read_to_string(&path)
                    && let Ok(v) = serde_json::from_str::<Value>(&text)
                {
                    if let Some(n) = v.get("name").and_then(|v| v.as_str()) {
                        names.insert(n.to_string());
                    } else if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        names.insert(stem.to_string());
                    }
                }
            }
        }
    }

    names.insert("dark".to_string());
    names.insert("light".to_string());
    names.insert(SYSTEM_THEME_NAME.to_string()); // 由终端配色推导的系统主题（默认主题）

    let mut names: Vec<String> = names.into_iter().collect();
    names.sort();
    names
}

/// 把主题 JSON 的 vars 与 colors 合并进 vars 集合（colors 覆盖同名，供 get 间接解析）。
///
/// 颜色值可以是 `#rgb`/`#rrggbb`/`oklch()`/`okhsl()`（见 [`crate::utils::color`]）、
/// 数字（256 色索引，存为 `ansi:<n>`）或空串（终端默认色）。
fn ingest_theme_json(
    v: &Value,
    loaded_name: &mut String,
    vars: &mut HashMap<String, String>,
    explicit_appearance: &mut Option<Appearance>,
) {
    if let Some(n) = v.get("name").and_then(|v| v.as_str()) {
        *loaded_name = n.to_string();
    }

    if let Some(appearance) = v.get("appearance").and_then(|v| v.as_str()) {
        match appearance {
            "dark" => *explicit_appearance = Some(Appearance::Dark),
            "light" => *explicit_appearance = Some(Appearance::Light),
            _ => {}
        }
    }

    /// 把一段 `vars`/`colors` 对象合并进 `vars`：字符串原样存，数字（≤ 255）存为 `ansi:<n>`；
    /// 值等于键名且 `vars` 段已定义该键时跳过，避免自引用把真实颜色覆盖成终端默认色。
    fn ingest_section(section: Option<&Value>, vars: &mut HashMap<String, String>) {
        let Some(obj) = section.and_then(|v| v.as_object()) else {
            return;
        };
        for (k, val) in obj {
            match val {
                Value::String(s) => {
                    // 自引用（如 "text": "text"）：值等于键名且 vars 段已定义同名键时，保留 vars 的真实颜色，
                    // 否则 colors 会用键名覆盖它，解析成 Reset（终端默认色）
                    if s == k && vars.contains_key(k) {
                        continue;
                    }
                    vars.insert(k.clone(), s.clone());
                }
                // 数字 = 256 色索引
                Value::Number(n) => {
                    if let Some(index) = n.as_u64().filter(|i| *i <= 255) {
                        vars.insert(k.clone(), format!("ansi:{index}"));
                    }
                }
                _ => {}
            }
        }
    }

    ingest_section(v.get("vars"), vars);
    ingest_section(v.get("colors"), vars);
}

/// 从主题自身颜色推导明暗外观：背景 token 比前景 token 暗 → 深色
fn detect_theme_appearance(vars: &HashMap<String, String>) -> Option<Appearance> {
    let lightness = |key: &str| -> Option<f64> {
        let raw = vars.get(key)?;
        let rgb = parse_color_str(raw).map(color_to_rgb)?;
        Some(rgb_to_oklch(rgb).l)
    };

    let background = lightness("userMessageBg")
        .or_else(|| lightness("toolPendingBg"))
        .or_else(|| lightness("selectedBg"));
    let foreground = lightness("text").or_else(|| lightness("muted"));

    match (foreground, background) {
        (Some(fg), Some(bg)) => Some(if bg < fg {
            Appearance::Dark
        } else {
            Appearance::Light
        }),
        (None, Some(bg)) => Some(if bg < 0.5 {
            Appearance::Dark
        } else {
            Appearance::Light
        }),
        (Some(fg), None) => Some(if fg > 0.5 {
            Appearance::Dark
        } else {
            Appearance::Light
        }),
        (None, None) => None,
    }
}

/// 终端外观：有上报的默认前景/背景时按它判定，否则回落到 COLORFGBG
fn terminal_appearance_now() -> Appearance {
    let colors = terminal_colors();
    match colors.background {
        Some(background) => crate::modes::interactive::system_theme::terminal_appearance(
            background,
            colors.foreground,
        ),
        None => {
            if Theme::detect_terminal_background() {
                Appearance::Light
            } else {
                Appearance::Dark
            }
        }
    }
}

/// hex 颜色 → ANSI 24bit/256 色前景/背景序列
pub fn fg(color: &str) -> String {
    fg_with(color, true_color())
}

/// 主题色 → ANSI 背景序列；真彩不可用时降级为 256 色索引，无法解析时返回空串。
pub fn bg(color: &str) -> String {
    bg_with(color, true_color())
}

/// hex 颜色 → ratatui Color
pub fn color(color: &str) -> Color {
    color_with(color, true_color())
}

/// 终端是否支持 24bit 真彩（PRUX_TRUE_COLOR 覆盖 + COLORTERM/TERM 探测，进程内缓存）。
/// 不支持时必须降级为 256 色索引，否则主题会在 256 色终端上发 38;2 序列。
fn true_color() -> bool {
    detect_terminal_capabilities().true_color
}

/// 同 [`fg`]，但由调用方显式指定是否真彩（`ansi:<n>` 直接发索引，不做近似）。
fn fg_with(color: &str, true_color: bool) -> String {
    // 显式 256 色索引（`ansi:<n>`）：直接发该索引，不做近似
    if let Some(index) = ansi_index(color) {
        return format!("\x1b[38;5;{}m", index);
    }

    let Some(rgb) = resolve_rgb(color) else {
        return String::new();
    };

    if true_color {
        format!("\x1b[38;2;{};{};{}m", rgb.r, rgb.g, rgb.b)
    } else {
        format!("\x1b[38;5;{}m", rgb_to_ansi256(rgb.r, rgb.g, rgb.b))
    }
}

/// 同 [`bg`]，但由调用方显式指定是否真彩。
fn bg_with(color: &str, true_color: bool) -> String {
    if let Some(index) = ansi_index(color) {
        return format!("\x1b[48;5;{}m", index);
    }

    let Some(rgb) = resolve_rgb(color) else {
        return String::new();
    };

    if true_color {
        format!("\x1b[48;2;{};{};{}m", rgb.r, rgb.g, rgb.b)
    } else {
        format!("\x1b[48;5;{}m", rgb_to_ansi256(rgb.r, rgb.g, rgb.b))
    }
}

/// 主题色值 → ratatui `Color`：显式索引 → `Indexed`，可解析的颜色按真彩能力选 `Rgb`/`Indexed`，
/// 解析失败 → `Reset`（终端默认色）。
fn color_with(color: &str, true_color: bool) -> Color {
    if let Some(index) = ansi_index(color) {
        return Color::Indexed(index);
    }
    match resolve_rgb(color) {
        Some(rgb) if true_color => Color::Rgb(rgb.r, rgb.g, rgb.b),
        Some(rgb) => Color::Indexed(rgb_to_ansi256(rgb.r, rgb.g, rgb.b)),
        None => Color::Reset,
    }
}

/// `ansi:<n>` 形式的显式 256 色索引
fn ansi_index(color: &str) -> Option<u8> {
    color.strip_prefix("ansi:")?.trim().parse().ok()
}

/// 主题色值 → sRGB：支持 `#rgb` / `#rrggbb` / `oklch()` / `okhsl()` / `ansi:<n>`
fn resolve_rgb(color: &str) -> Option<crate::utils::color::Rgb> {
    if let Some(index) = ansi_index(color) {
        return Some(indexed_to_rgb(index));
    }
    parse_color_str(color).map(color_to_rgb)
}

/// 在候选值中找与 `target` 绝对差最小的下标（相等时取靠前者）；`values` 为空时返回 0。
fn closest_index(values: &[u8], target: u8) -> usize {
    let mut best = 0usize;
    let mut best_distance = u16::MAX;
    for (i, v) in values.iter().enumerate() {
        let distance = target.abs_diff(*v) as u16;
        if distance < best_distance {
            best = i;
            best_distance = distance;
        }
    }
    best
}

/// RGB → 最近 256 色索引（6x6x6 立方体 vs 24 级灰阶，感知加权距离）。
fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let (ri, gi, bi) = (
        closest_index(&CUBE_VALUES, r),
        closest_index(&CUBE_VALUES, g),
        closest_index(&CUBE_VALUES, b),
    );
    let cube = [CUBE_VALUES[ri], CUBE_VALUES[gi], CUBE_VALUES[bi]];
    let cube_index = 16 + 36 * ri as u16 + 6 * gi as u16 + bi as u16;

    let gray = (0.299 * r as f64 + 0.587 * g as f64 + 0.114 * b as f64).round() as u8;
    let gray_values: Vec<u8> = (0..24).map(|i| 8 + i * 10).collect();
    let gray_offset = closest_index(&gray_values, gray);
    let gray_value = gray_values[gray_offset];
    let spread = r.max(g).max(b) - r.min(g).min(b);

    let weighted = |c: [u8; 3]| -> f64 {
        let d = |a: u8, b: u8| (a as f64 - b as f64).powi(2);
        d(c[0], r) * 0.299 + d(c[1], g) * 0.587 + d(c[2], b) * 0.114
    };

    if spread < 10 && weighted([gray_value; 3]) < weighted(cube) {
        return 232 + gray_offset as u8;
    }
    cube_index as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 只读主题视图：外观、具体色值（`oklch()`/变量引用都解成 `#rrggbb`）、终端默认色兜底。
    #[test]
    fn view_resolves_keys_to_concrete_colors() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = crate::core::settings_manager::agent_dir().join("themes");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("view-test.json"),
            r##"{ "name": "view-test", "appearance": "light",
                   "vars": { "neutral": "oklch(0.5 0 0)" },
                   "colors": { "accent": "neutral", "text": "", "bg": "#123456" } }"##,
        )
        .unwrap();

        let view = Theme::load("view-test").view();
        assert_eq!(view.appearance, "light");
        assert!(view.is_light());
        assert_eq!(view.name, "view-test");
        // 变量引用的 oklch 被解成具体 hex；空串（终端默认色）用默认前景色兜底
        assert_eq!(view.color("accent"), Some("#636363"), "{:?}", view.colors);
        assert_eq!(view.color("bg"), Some("#123456"));
        assert_eq!(view.color("text"), Some(view.default_fg.as_str()));
        assert!(view.default_fg.starts_with('#'), "{:?}", view.default_fg);
        // 未知键不猜测
        assert_eq!(view.color("nope"), None);
    }

    /// `publish_view`：发布后全局可读，重复发布同一主题不改变已发布值。
    #[test]
    fn publish_view_exposes_current_theme() {
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::theme_view::clear();
        let theme = Theme::load("dark");
        theme.publish_view();
        let published = crate::core::theme_view::get().expect("已发布");
        assert_eq!(published.name, theme.name);
        assert_eq!(published.colors.len(), theme.vars.len());

        theme.publish_view();
        assert_eq!(crate::core::theme_view::get(), Some(published));
        crate::core::theme_view::clear();
    }

    #[test]
    fn loads_packaged_dark_theme() {
        let t = Theme::load("dark");
        assert_eq!(t.name, "dark");
        if t.vars.is_empty() {
            // 没有安装 pi npm 包时跳过颜色断言
            return;
        }
        assert!(t.get("accent", "#000000") != "#000000");
    }

    #[test]
    fn settings_themes_dir_appears_in_list_and_loads() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        // 线程本地 agent_dir 隔离：主题写入 agent_dir/themes
        // （Theme::load 与 available_theme_names 均扫描该目录）
        let dir = crate::core::settings_manager::agent_dir();
        let themes_dir = dir.join("themes");
        let _ = std::fs::create_dir_all(&themes_dir);
        std::fs::write(
            themes_dir.join("my-theme.json"),
            r##"{"name": "my-theme", "accent": "#ff0000"}"##,
        )
        .unwrap();
        // available_theme_names：agent_dir/themes 中的主题应出现在列表
        let names = available_theme_names("/nonexistent", &dir, true);
        assert!(names.iter().any(|n| n == "my-theme"), "list: {:?}", names);
        // Theme::load 从当前线程 agent_dir 加载
        let t = Theme::load("my-theme");
        assert_eq!(t.name, "my-theme");
        // 清理本次写入的主题文件（保留目录供其他测试使用）
        let _ = std::fs::remove_file(themes_dir.join("my-theme.json"));
    }
    #[test]
    fn theme_auto_resolves_from_terminal_background() {
        // COLORFGBG 亮背景 → light
        unsafe { std::env::set_var("COLORFGBG", "15;15") };
        assert!(Theme::detect_terminal_background(), "bg>8 判 light");
        let t = Theme::load("auto");
        assert!(t.name.contains("light"), "auto 解析为 {}", t.name);
        // 暗背景 → dark
        unsafe { std::env::set_var("COLORFGBG", "0;0") };
        assert!(!Theme::detect_terminal_background());
        let t2 = Theme::load("auto");
        assert!(t2.name.contains("dark"), "auto 解析为 {}", t2.name);
        unsafe { std::env::remove_var("COLORFGBG") };
    }

    /// colors 中自引用键（"text": "text"）不得覆盖 vars 的真实颜色：
    /// 解析结果必须是具体颜色（hex 或 `oklch()`/`okhsl()`），不能是键名本身、也不能是终端默认色。
    fn assert_resolved_color(t: &Theme, key: &str) -> String {
        let v = t.get(key, "");
        assert!(
            !v.is_empty() && v != key,
            "{key} 应解析为具体颜色，得到 {v:?}"
        );
        assert_ne!(color(&v), Color::Reset, "{key} 不应解析成终端默认色：{v:?}");
        v
    }

    /// pi #9973：颜色输出必须尊重终端能力（PRUX_TRUE_COLOR / 探测），
    /// 不支持 24bit 时降级为 256 色索引。
    #[test]
    fn color_output_honors_terminal_capability() {
        // 真彩：24bit 序列 / Rgb
        assert_eq!(fg_with("#ff0000", true), "\x1b[38;2;255;0;0m");
        assert_eq!(bg_with("#ff0000", true), "\x1b[48;2;255;0;0m");
        assert_eq!(color_with("#ff0000", true), Color::Rgb(255, 0, 0));
        // 非真彩：同一颜色降级为 256 色索引（纯红 = 196）
        assert_eq!(fg_with("#ff0000", false), "\x1b[38;5;196m");
        assert_eq!(bg_with("#ff0000", false), "\x1b[48;5;196m");
        assert_eq!(color_with("#ff0000", false), Color::Indexed(196));
        // 非法颜色：不产生序列 / Reset
        assert_eq!(fg_with("nope", false), "");
        assert_eq!(color_with("nope", false), Color::Reset);
    }

    /// 256 色近似：立方体 / 灰阶分支对齐 pi `rgbToAnsi256`
    #[test]
    fn rgb_to_ansi256_matches_pi() {
        assert_eq!(rgb_to_ansi256(0, 0, 0), 16);
        assert_eq!(rgb_to_ansi256(255, 255, 255), 231);
        assert_eq!(rgb_to_ansi256(255, 0, 0), 196);
        assert_eq!(rgb_to_ansi256(0, 255, 0), 46);
        assert_eq!(rgb_to_ansi256(0, 0, 255), 21);
        // 中性灰靠近灰阶带（#808080 → 244）；#d4d4d4 更接近立方体 215 → 188
        assert_eq!(rgb_to_ansi256(0x80, 0x80, 0x80), 244);
        assert_eq!(rgb_to_ansi256(0xd4, 0xd4, 0xd4), 188);
    }

    #[test]
    fn theme_json_supports_oklch_okhsl_and_short_hex() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = crate::core::settings_manager::agent_dir();
        let themes_dir = dir.join("themes");
        let _ = std::fs::create_dir_all(&themes_dir);
        std::fs::write(
            themes_dir.join("color-syntax.json"),
            r##"{
                "name": "color-syntax",
                "appearance": "light",
                "vars": { "accent": "oklch(60% 0.15 250)" },
                "colors": {
                    "text": "#abc",
                    "success": "okhsl(200 60% 40%)",
                    "error": "ansi:196",
                    "warning": ""
                }
            }"##,
        )
        .unwrap();

        let t = Theme::load("color-syntax");
        assert_eq!(t.name, "color-syntax");
        assert_eq!(t.appearance(), Appearance::Light, "显式 appearance 生效");
        // oklch()/okhsl()/#rgb 都解析为具体 sRGB（显式真彩：解析结果与终端能力无关）
        assert_eq!(t.get("accent", ""), "oklch(60% 0.15 250)");
        assert_eq!(
            color_with(&t.get("text", ""), true),
            Color::Rgb(0xaa, 0xbb, 0xcc)
        );
        assert_eq!(
            color_with("oklch(60% 0.15 250)", true),
            Color::Rgb(0x27, 0x84, 0xd5)
        );
        assert_eq!(
            color_with("okhsl(200 60% 40%)", true),
            Color::Rgb(0x2c, 0x69, 0x6c)
        );
        // 显式 256 色索引
        assert_eq!(color("ansi:196"), Color::Indexed(196));
        assert_eq!(fg_with("ansi:196", true), "\x1b[38;5;196m");
        assert_eq!(bg_with("ansi:196", false), "\x1b[48;5;196m");
        // 空串 = 终端默认色
        assert_eq!(t.resolve_color("warning"), Color::Reset);
        assert_eq!(color(""), Color::Reset);
        assert_eq!(color("nope"), Color::Reset);

        let _ = std::fs::remove_file(themes_dir.join("color-syntax.json"));
    }

    #[test]
    fn theme_appearance_detected_from_colors() {
        // 未声明 appearance 时从 colors 推导：背景比前景暗 → dark
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = crate::core::settings_manager::agent_dir();
        let themes_dir = dir.join("themes");
        let _ = std::fs::create_dir_all(&themes_dir);
        std::fs::write(
            themes_dir.join("detect-dark.json"),
            r##"{"name":"detect-dark","colors":{"text":"#e0e0e0","userMessageBg":"#101010"}}"##,
        )
        .unwrap();
        std::fs::write(
            themes_dir.join("detect-light.json"),
            r##"{"name":"detect-light","colors":{"text":"#202020","userMessageBg":"#fefefe"}}"##,
        )
        .unwrap();

        assert_eq!(Theme::load("detect-dark").appearance(), Appearance::Dark);
        assert_eq!(Theme::load("detect-light").appearance(), Appearance::Light);

        let _ = std::fs::remove_file(themes_dir.join("detect-dark.json"));
        let _ = std::fs::remove_file(themes_dir.join("detect-light.json"));
    }

    #[test]
    fn system_theme_uses_indexed_tier_without_terminal_colors() {
        // 测试环境不是 TTY：查不到任何配色 → 索引兜底档（ANSI 索引 + dim），
        // 不再回落到内置 dark/light（终端会用自己的主题渲染这些索引）
        let _ad = crate::test_support::AgentDirGuard::temp();
        let t = Theme::load(SYSTEM_THEME_NAME);
        assert_eq!(t.name, SYSTEM_THEME_NAME);
        assert!(!t.vars.is_empty());
        // 彩色 token 用 ANSI 索引槽（accent 属 violet 族 → 槽 5，success 属 green 族 → 槽 2）
        assert_eq!(t.get("accent", ""), "ansi:5", "accent 用色族槽位");
        assert_eq!(t.get("success", ""), "ansi:2", "success 属 green 族 → 槽 2");
        // 中性 token 与面板不带颜色（终端默认 / 透明）
        assert_eq!(t.get("text", "#fff"), "", "正文用终端默认色");
        assert_eq!(t.get("userMessageBg", "#fff"), "", "面板不设背景");
        // 中性非正文 token 走 SGR 2（faint）
        assert!(t.dim_keys.contains("muted"), "{:?}", t.dim_keys);
        assert!(t.dim_keys.contains("dim"), "{:?}", t.dim_keys);
        assert!(!t.dim_keys.contains("text"), "正文不算 dim");
    }

    #[test]
    fn indexed_tier_marks_neutral_tokens_faint() {
        // muted 走 dim（SGR 2），accent 正常着色
        let _ad = crate::test_support::AgentDirGuard::temp();
        let theme = Theme::load(SYSTEM_THEME_NAME);
        assert!(
            theme
                .style("muted", "#808080")
                .add_modifier
                .contains(Modifier::DIM),
            "muted 应叠加 DIM"
        );
        assert!(
            !theme
                .style("accent", "#8abeb7")
                .add_modifier
                .contains(Modifier::DIM),
            "accent 不应叠加 DIM"
        );
    }

    #[test]
    fn system_theme_name_is_listed() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let names = available_theme_names(
            "/nonexistent",
            &crate::core::settings_manager::agent_dir(),
            true,
        );
        assert!(names.iter().any(|n| n == SYSTEM_THEME_NAME), "{names:?}");
    }

    #[test]
    fn self_referencing_colors_resolve_to_vars_color() {
        for name in ["dark", "light"] {
            let t = Theme::load(name);
            let text = assert_resolved_color(&t, "text");
            assert_resolved_color(&t, "userMessageText");
            assert_eq!(t.get("userMessageText", ""), text);
            assert_resolved_color(&t, "accent");
            assert_resolved_color(&t, "selectedBg");
            assert_resolved_color(&t, "toolPendingBg");
            assert_resolved_color(&t, "toolSuccessBg");
            assert_resolved_color(&t, "toolErrorBg");
        }
    }

    /// 内置 dark/light 用的是 pi 的规范键名（无旧名别名层）：渲染读的键必须能解析成具体颜色。
    #[test]
    fn builtin_themes_use_canonical_token_names() {
        /// prux 渲染与扩展实际读取的键（即 pi schema 里的规范名）。
        const CONSUMED: &[&str] = &[
            "accent",
            "border",
            "success",
            "error",
            "warning",
            "muted",
            "dim",
            "text",
            "userMessageBg",
            "userMessageText",
            "customMessageBg",
            "customMessageText",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
            "toolTitle",
            "toolOutput",
            "mdCode",
            "mdCodeBlock",
            "mdListBullet",
            "scrollbarThumb",
        ];
        for name in ["dark", "light"] {
            let t = Theme::load(name);
            for key in CONSUMED {
                assert_resolved_color(&t, key);
            }
        }

        // 旧键名不得再出现在主题里（消费端已改用规范名）
        const RETIRED: &[&str] = &[
            "userMsgBg",
            "customMsgBg",
            "mdMath",
            "mdMathBlock",
            "systemInfo",
            "systemSuccess",
            "systemWarning",
            "systemError",
            "dimGray",
            "darkGray",
        ];
        for name in ["dark", "light"] {
            let t = Theme::load(name);
            for key in RETIRED {
                assert!(!t.vars.contains_key(*key), "{name}: 不应再定义旧键 {key}");
            }
        }
    }
}

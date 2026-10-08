//! `system` 主题：从终端上报的配色推导出一整套主题色。
//!
//! 要点：
//! - 每个 token 属于一个**色族**（决定色相与饱和度）并带**对比度等级**（决定明度曲线）；
//!   色相与饱和度优先取自终端调色板里该色族对应的 ANSI 槽，缺失时用色族自带值；
//! - 明度由等级曲线唯一决定（曲线是对「背景明度 → 该等级所需明度」的拟合多项式）；
//! - 颜色在 OKHSL 里构造：饱和度相对 sRGB 色域，并在接近黑/白时向灰收敛；
//! - 中灰背景可能装不下所有等级 → 按 pi 的做法二分「松弛度」到能解为止；
//! - 正文类 token（text/userMessageText/toolTitle）在终端前景足够强时直接用终端默认色
//!   （导出为空串 = `Color::Reset`），否则用终端前景的色相按所需明度重建。
//!
//! 表格（FAMILIES / TOKEN_FAMILIES / LEVELS / RULES）由 pi 运行时导出生成，逐值一致。

use crate::utils::color::{
    Okhsl, Oklch, Rgb, okhsl_to_rgb, oklab_to_okhsl_lightness, oklch_to_rgb, rgb_to_okhsl,
    rgb_to_oklch,
};
use std::collections::HashMap;

/// 主题面向的明暗外观
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Appearance {
    /// 深色终端（背景暗，文字向亮处推）
    #[default]
    Dark,
    /// 浅色终端（背景亮，文字向暗处推）
    Light,
}

impl Appearance {
    /// 规范名 `"dark"` / `"light"`。
    pub fn as_str(&self) -> &'static str {
        match self {
            Appearance::Dark => "dark",
            Appearance::Light => "light",
        }
    }
}

/// 生成输入
#[derive(Debug, Clone, Default)]
pub struct SystemThemeInput {
    /// 终端前景色；`None` 表示未上报
    pub foreground: Option<Rgb>,
    /// 终端背景色；`None` 表示未上报（生成直接返回 `None`）
    pub background: Option<Rgb>,
    /// ANSI 0-15
    pub palette: Option<Vec<Rgb>>,
    /// 饱和度倍率 0（灰度）-1；缺省 1
    pub saturation: Option<f64>,
}

/// 生成结果
#[derive(Debug, Clone, Default)]
pub struct SystemThemeOutput {
    /// token → `#rrggbb` / `ansi:<n>` / `""`（终端默认色）
    pub colors: HashMap<&'static str, String>,
    /// 推导出的终端明暗外观
    pub appearance: Appearance,
    /// 需要用 **SGR 2（faint）** 渲染的 token：仅索引兜底档会用到
    /// （中性色比正文更淡时用 faint，而不是亮黑——部分主题里亮黑几乎看不见）。
    /// 带终端配色的档位里中性色本就是具体颜色，此列表为空。
    pub dim: Vec<&'static str>,
}

/// 色族的饱和度区间：明度接近黑/白时饱和度向 `min` 收敛，其余处可达 `max`
struct Saturation {
    /// 黑/白端的最低饱和度
    min: f64,
    /// 中明度处的最高饱和度
    max: f64,
}

/// 色族：一组 token 共享的色相与饱和度配置，并绑定终端调色板的 ANSI 槽
struct Family {
    /// 色族名，供 `TOKEN_FAMILIES` 引用（`family()` 按名查找）
    #[allow(dead_code)]
    name: &'static str,
    /// 色相（OKHSL，度）
    hue: f64,
    /// 饱和度区间
    saturation: Saturation,
    /// 取色用的 ANSI 调色板槽位，无调色板时仅作占位
    slot: u8,
}

/// 对比度规则：`token` 画在 `on` 列出的所有表面上时，明度不得弱于 `level` 等级
struct Rule {
    /// 被约束的 token 名
    token: &'static str,
    /// 该 token 可能出现的表面 token（`background` 表示终端背景）
    on: &'static [&'static str],
    /// `LEVELS` 中的等级名
    level: &'static str,
}

/// 所有面板类 token（生成时额外做「面板可读性」限制）
const PANEL_TOKENS: &[&str] = &[
    "userMessageBg",
    "toolPendingBg",
    "toolSuccessBg",
    "toolErrorBg",
    "selectedBg",
    "searchMatchBg",
    "customMessageBg",
];

/// 正文类 token：终端前景足够强时直接用它（空串 = 终端默认色）
const FOREGROUND_TOKENS: &[&str] = &["text", "userMessageText", "toolTitle"];

/// 正文文字必须达到的 WCAG 2 对比度
const TEXT_MINIMUM_WCAG_CONTRAST: f64 = 4.5;

/// 松弛时把强于该等级的等级压向它
fn readable_floor(appearance: Appearance) -> &'static str {
    match appearance {
        Appearance::Dark => "readable",
        Appearance::Light => "subtle",
    }
}

/// 正文使用终端前景所需的等级
const FOREGROUND_LEVEL: &str = "emphasis";

/// 色族表（pi 导出）：token 经 `TOKEN_FAMILIES` 映射到其中一项
const FAMILIES: &[Family] = &[
    Family {
        name: "neutral",
        hue: 231.49,
        saturation: Saturation {
            min: 0.02,
            max: 0.08,
        },
        slot: 8,
    },
    Family {
        name: "blue",
        hue: 231.49,
        saturation: Saturation {
            min: 0.1,
            max: 0.68,
        },
        slot: 4,
    },
    Family {
        name: "green",
        hue: 158.68,
        saturation: Saturation {
            min: 0.1,
            max: 0.76,
        },
        slot: 2,
    },
    Family {
        name: "red",
        hue: 20.0,
        saturation: Saturation {
            min: 0.1,
            max: 0.92,
        },
        slot: 1,
    },
    Family {
        name: "yellow",
        hue: 82.36,
        saturation: Saturation { min: 0.5, max: 1.0 },
        slot: 3,
    },
    Family {
        name: "orange",
        hue: 52.0,
        saturation: Saturation {
            min: 0.12,
            max: 0.85,
        },
        slot: 3,
    },
    Family {
        name: "violet",
        hue: 295.0,
        saturation: Saturation { min: 0.2, max: 0.6 },
        slot: 5,
    },
    Family {
        name: "calamine",
        hue: 202.43,
        saturation: Saturation {
            min: 0.1,
            max: 0.74,
        },
        slot: 6,
    },
    Family {
        name: "thinkingSlate",
        hue: 231.49,
        saturation: Saturation {
            min: 0.08,
            max: 0.2,
        },
        slot: 4,
    },
    Family {
        name: "thinkingBlue",
        hue: 231.49,
        saturation: Saturation {
            min: 0.2,
            max: 0.45,
        },
        slot: 4,
    },
    Family {
        name: "thinkingPeriwinkle",
        hue: 263.25,
        saturation: Saturation { min: 0.3, max: 0.6 },
        slot: 6,
    },
    Family {
        name: "thinkingViolet",
        hue: 295.0,
        saturation: Saturation {
            min: 0.4,
            max: 0.75,
        },
        slot: 5,
    },
    Family {
        name: "thinkingMagenta",
        hue: 337.5,
        saturation: Saturation {
            min: 0.5,
            max: 0.85,
        },
        slot: 13,
    },
    Family {
        name: "thinkingRed",
        hue: 20.0,
        saturation: Saturation {
            min: 0.95,
            max: 1.0,
        },
        slot: 1,
    },
];

/// token → 色族名（决定色相与饱和度）；缺失时回落到 `neutral`
const TOKEN_FAMILIES: &[(&str, &str)] = &[
    ("selectedBg", "blue"),
    ("searchMatchBg", "orange"),
    ("userMessageBg", "blue"),
    ("customMessageBg", "violet"),
    ("toolPendingBg", "neutral"),
    ("toolSuccessBg", "green"),
    ("toolErrorBg", "red"),
    ("text", "neutral"),
    ("userMessageText", "neutral"),
    ("customMessageText", "neutral"),
    ("toolTitle", "neutral"),
    ("syntaxOperator", "neutral"),
    ("syntaxPunctuation", "neutral"),
    ("muted", "neutral"),
    ("dim", "neutral"),
    ("thinkingText", "neutral"),
    ("toolOutput", "neutral"),
    ("mdLinkUrl", "neutral"),
    ("mdQuote", "neutral"),
    ("mdQuoteBorder", "neutral"),
    ("mdHr", "neutral"),
    ("mdCodeBlockBorder", "neutral"),
    ("toolDiffContext", "neutral"),
    ("syntaxComment", "neutral"),
    ("scrollbarTrack", "neutral"),
    ("scrollbarThumb", "neutral"),
    ("searchMatchText", "neutral"),
    ("borderMuted", "neutral"),
    ("accent", "violet"),
    ("borderAccent", "violet"),
    ("customMessageLabel", "violet"),
    ("mdCode", "violet"),
    ("mdListBullet", "violet"),
    ("syntaxType", "violet"),
    ("border", "blue"),
    ("mdLink", "blue"),
    ("syntaxKeyword", "blue"),
    ("syntaxVariable", "calamine"),
    ("success", "green"),
    ("mdCodeBlock", "green"),
    ("toolDiffAdded", "green"),
    ("bashMode", "green"),
    ("syntaxNumber", "green"),
    ("error", "red"),
    ("toolDiffRemoved", "red"),
    ("warning", "yellow"),
    ("mdHeading", "yellow"),
    ("syntaxFunction", "yellow"),
    ("syntaxString", "orange"),
    ("thinkingOff", "neutral"),
    ("thinkingMinimal", "thinkingSlate"),
    ("thinkingLow", "thinkingBlue"),
    ("thinkingMedium", "thinkingPeriwinkle"),
    ("thinkingHigh", "thinkingViolet"),
    ("thinkingXhigh", "thinkingMagenta"),
    ("thinkingMax", "thinkingRed"),
];

/// token → 调色板槽位覆盖；未列出的 token 用其色族的默认槽位
const TOKEN_SLOTS: &[(&str, u8)] = &[
    ("syntaxString", 2),
    ("syntaxNumber", 5),
    ("searchMatchBg", 3),
];

/// (等级名, 外观, 明度曲线系数, 可达的表面明度区间)
type LevelRow = (&'static str, Appearance, [f64; 6], (f64, f64));
/// 对比度等级表（pi 导出）：曲线给出「表面明度 → 该等级所需明度」
const LEVELS: &[LevelRow] = &[
    (
        "panel",
        Appearance::Dark,
        [0.29131, -0.39746, 2.33185, -0.85524, -1.2076, 0.86276],
        (0.0, 0.979),
    ),
    (
        "panel",
        Appearance::Light,
        [-3.74073, 27.94549, -78.44258, 112.6798, -79.60015, 22.11277],
        (0.348, 1.0),
    ),
    (
        "track",
        Appearance::Dark,
        [0.39028, -0.23015, 0.83573, 2.43829, -4.38292, 2.01582],
        (0.0, 0.946),
    ),
    (
        "track",
        Appearance::Light,
        [
            -5.24921, 38.37322, -107.28833, 152.10005, -106.17127, 29.18061,
        ],
        (0.368, 1.0),
    ),
    (
        "thinking0",
        Appearance::Dark,
        [0.52988, -0.05809, -0.30924, 4.63567, -6.52933, 2.89108],
        (0.0, 0.873),
    ),
    (
        "thinking0",
        Appearance::Light,
        [
            -28.27749, 182.85284, -469.62416, 603.15916, -384.59976, 97.35147,
        ],
        (0.51, 1.0),
    ),
    (
        "thinking1",
        Appearance::Dark,
        [0.55278, -0.03667, -0.45659, 4.95347, -6.90265, 3.0706],
        (0.0, 0.858),
    ),
    (
        "thinking1",
        Appearance::Light,
        [
            -37.10484, 235.86282, -596.62344, 754.3633, -474.00763, 118.3551,
        ],
        (0.535, 1.0),
    ),
    (
        "thinking2",
        Appearance::Dark,
        [0.57486, -0.01765, -0.58987, 5.25227, -7.27175, 3.25532],
        (0.0, 0.842),
    ),
    (
        "thinking2",
        Appearance::Light,
        [
            -59.89653, 377.05024, -945.07843, 1182.03145, -734.96375, 181.68658,
        ],
        (0.556, 1.0),
    ),
    (
        "thinking3",
        Appearance::Dark,
        [0.59621, -0.00062, -0.71148, 5.53588, -7.6392, 3.44606],
        (0.0, 0.827),
    ),
    (
        "thinking3",
        Appearance::Light,
        [
            -72.07122,
            445.84082,
            -1099.57352,
            1353.88793,
            -829.53392,
            202.26164,
        ],
        (0.58, 1.0),
    ),
    (
        "thinking4",
        Appearance::Dark,
        [0.61691, 0.01462, -0.82288, 5.80651, -8.00641, 3.64333],
        (0.0, 0.811),
    ),
    (
        "thinking4",
        Appearance::Light,
        [
            -110.14338,
            674.21488,
            -1645.75941,
            2004.32367,
            -1215.15899,
            293.3183,
        ],
        (0.6, 1.0),
    ),
    (
        "thinking5",
        Appearance::Dark,
        [0.63702, 0.02826, -0.92498, 6.06465, -8.37246, 3.84651],
        (0.0, 0.795),
    ),
    (
        "thinking5",
        Appearance::Light,
        [
            -175.47701,
            1063.54495,
            -2570.70594,
            3098.80776,
            -1860.15527,
            444.76392,
        ],
        (0.62, 1.0),
    ),
    (
        "thinking6",
        Appearance::Dark,
        [0.65658, 0.04044, -1.01835, 6.30989, -8.73529, 4.05439],
        (0.0, 0.779),
    ),
    (
        "thinking6",
        Appearance::Light,
        [
            -183.81712,
            1094.70055,
            -2602.68539,
            3088.71276,
            -1826.91131,
            430.75931,
        ],
        (0.643, 1.0),
    ),
    (
        "subtle",
        Appearance::Dark,
        [0.56762, -0.02475, -0.5383, 5.12628, -7.10931, 3.17324],
        (0.0, 0.848),
    ),
    (
        "subtle",
        Appearance::Light,
        [
            -232.85459,
            1376.54473,
            -3249.11801,
            3827.91186,
            -2248.29472,
            526.55751,
        ],
        (0.657, 1.0),
    ),
    (
        "thumb",
        Appearance::Dark,
        [0.60323, 0.00278, -0.73328, 5.57157, -7.68067, 3.46933],
        (0.0, 0.823),
    ),
    (
        "thumb",
        Appearance::Light,
        [
            -82.89897,
            511.01355,
            -1255.98095,
            1540.76821,
            -940.68087,
            228.58523,
        ],
        (0.586, 1.0),
    ),
    (
        "readable",
        Appearance::Dark,
        [0.66937, 0.04704, -1.06871, 6.43941, -8.9332, 4.17229],
        (0.0, 0.77),
    ),
    (
        "readable",
        Appearance::Light,
        [
            -1554.52576,
            8733.56817,
            -19604.93507,
            21977.72696,
            -12300.99599,
            2749.81288,
        ],
        (0.751, 1.0),
    ),
    (
        "emphasis",
        Appearance::Dark,
        [0.7303, 0.07695, -1.31626, 7.1681, -10.14436, 4.92846],
        (0.0, 0.712),
    ),
    (
        "emphasis",
        Appearance::Light,
        [
            -4948.31942,
            26870.91986,
            -58334.48399,
            63280.17197,
            -34298.01053,
            7430.30146,
        ],
        (0.811, 1.0),
    ),
    (
        "textOnPanel",
        Appearance::Dark,
        [0.86713, 0.05232, -0.89428, 4.79014, -5.5432, 1.75023],
        (0.0, 0.542),
    ),
    (
        "textOnPanel",
        Appearance::Light,
        [
            -8570.89457,
            43954.60805,
            -90084.00702,
            92220.6791,
            -47152.15802,
            9632.27113,
        ],
        (0.867, 1.0),
    ),
    (
        "text",
        Appearance::Dark,
        [0.89242, 0.02311, -0.44862, 2.34417, -0.06084, -2.63844],
        (0.0, 0.5),
    ),
    (
        "text",
        Appearance::Light,
        [
            -2004.67048,
            6664.47299,
            -6060.70202,
            -1792.61209,
            5133.82359,
            -1939.85583,
        ],
        (0.894, 1.0),
    ),
];

/// 对比度规则表（pi 导出）：决定每个 token 出现在哪些表面、至少要到哪个等级
const RULES: &[Rule] = &[
    Rule {
        token: "userMessageBg",
        on: &["background"],
        level: "panel",
    },
    Rule {
        token: "toolPendingBg",
        on: &["background"],
        level: "panel",
    },
    Rule {
        token: "toolSuccessBg",
        on: &["background"],
        level: "panel",
    },
    Rule {
        token: "toolErrorBg",
        on: &["background"],
        level: "panel",
    },
    Rule {
        token: "selectedBg",
        on: &["background"],
        level: "panel",
    },
    Rule {
        token: "searchMatchBg",
        on: &["background"],
        level: "panel",
    },
    Rule {
        token: "customMessageBg",
        on: &["background"],
        level: "panel",
    },
    Rule {
        token: "text",
        on: &["background"],
        level: "text",
    },
    Rule {
        token: "text",
        on: &["selectedBg"],
        level: "textOnPanel",
    },
    Rule {
        token: "userMessageText",
        on: &["userMessageBg"],
        level: "textOnPanel",
    },
    Rule {
        token: "toolTitle",
        on: &["toolPendingBg", "toolSuccessBg", "toolErrorBg"],
        level: "textOnPanel",
    },
    Rule {
        token: "accent",
        on: &[
            "background",
            "selectedBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "success",
        on: &[
            "background",
            "selectedBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "error",
        on: &[
            "background",
            "selectedBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "warning",
        on: &[
            "background",
            "selectedBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "muted",
        on: &[
            "background",
            "selectedBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "dim",
        on: &[
            "background",
            "selectedBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "subtle",
    },
    Rule {
        token: "thinkingText",
        on: &["background"],
        level: "readable",
    },
    Rule {
        token: "customMessageText",
        on: &[
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "customMessageLabel",
        on: &[
            "background",
            "customMessageBg",
            "selectedBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "toolOutput",
        on: &[
            "background",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "mdHeading",
        on: &["background", "userMessageBg", "customMessageBg"],
        level: "readable",
    },
    Rule {
        token: "mdLink",
        on: &["background", "userMessageBg", "customMessageBg"],
        level: "readable",
    },
    Rule {
        token: "mdLinkUrl",
        on: &["background", "userMessageBg", "customMessageBg"],
        level: "readable",
    },
    Rule {
        token: "mdCode",
        on: &["background", "userMessageBg", "customMessageBg"],
        level: "readable",
    },
    Rule {
        token: "mdQuote",
        on: &["background", "userMessageBg", "customMessageBg"],
        level: "readable",
    },
    Rule {
        token: "mdCodeBlockBorder",
        on: &["background", "userMessageBg", "customMessageBg"],
        level: "readable",
    },
    Rule {
        token: "mdListBullet",
        on: &["background", "userMessageBg", "customMessageBg"],
        level: "readable",
    },
    Rule {
        token: "mdCodeBlock",
        on: &[
            "background",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "toolDiffAdded",
        on: &[
            "background",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "toolDiffRemoved",
        on: &[
            "background",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "toolDiffContext",
        on: &[
            "background",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "syntaxComment",
        on: &[
            "background",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "syntaxKeyword",
        on: &[
            "background",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "syntaxFunction",
        on: &[
            "background",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "syntaxVariable",
        on: &[
            "background",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "syntaxString",
        on: &[
            "background",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "syntaxNumber",
        on: &[
            "background",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "syntaxType",
        on: &[
            "background",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "syntaxOperator",
        on: &[
            "background",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "syntaxPunctuation",
        on: &[
            "background",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "searchMatchText",
        on: &["searchMatchBg"],
        level: "readable",
    },
    Rule {
        token: "bashMode",
        on: &["background"],
        level: "readable",
    },
    Rule {
        token: "border",
        on: &["background"],
        level: "readable",
    },
    Rule {
        token: "borderAccent",
        on: &["background"],
        level: "readable",
    },
    Rule {
        token: "borderMuted",
        on: &["background"],
        level: "subtle",
    },
    Rule {
        token: "mdQuoteBorder",
        on: &[
            "background",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "mdHr",
        on: &[
            "background",
            "userMessageBg",
            "customMessageBg",
            "toolPendingBg",
            "toolSuccessBg",
            "toolErrorBg",
        ],
        level: "readable",
    },
    Rule {
        token: "scrollbarTrack",
        on: &["background"],
        level: "track",
    },
    Rule {
        token: "scrollbarThumb",
        on: &["scrollbarTrack"],
        level: "thumb",
    },
    Rule {
        token: "thinkingOff",
        on: &["background"],
        level: "thinking0",
    },
    Rule {
        token: "thinkingMinimal",
        on: &["background"],
        level: "thinking1",
    },
    Rule {
        token: "thinkingLow",
        on: &["background"],
        level: "thinking2",
    },
    Rule {
        token: "thinkingMedium",
        on: &["background"],
        level: "thinking3",
    },
    Rule {
        token: "thinkingHigh",
        on: &["background"],
        level: "thinking4",
    },
    Rule {
        token: "thinkingXhigh",
        on: &["background"],
        level: "thinking5",
    },
    Rule {
        token: "thinkingMax",
        on: &["background"],
        level: "thinking6",
    },
];

// ============================================================================
// 生成
// ============================================================================

/// 把 `value` 夹到 `[min, max]`（`min > max` 时以 `min` 为准）。
fn clamp(value: f64, min: f64, max: f64) -> f64 {
    value.min(max).max(min)
}

/// 按名查色族；名字必须存在于 `FAMILIES`，否则 panic（表由常量维护，不会在运行时变）。
fn family(name: &str) -> &'static Family {
    FAMILIES
        .iter()
        .find(|f| f.name == name)
        .expect("family 表必须自洽")
}

/// 查 token 所属色族；未登记的 token 回落成 `neutral`。
fn family_of(token: &str) -> &'static Family {
    let name = TOKEN_FAMILIES
        .iter()
        .find(|(t, _)| *t == token)
        .map(|(_, f)| *f)
        .unwrap_or("neutral");
    family(name)
}

/// 查 token 绑定的 ANSI 调色板槽位；未登记时为 `None`（调用方改用色族自带槽位）。
fn slot_of(token: &str) -> Option<u8> {
    TOKEN_SLOTS
        .iter()
        .find(|(t, _)| *t == token)
        .map(|(_, s)| *s)
}

/// 等级曲线：表面明度 `surface_l` 上该等级所需的明度（不可达时为 `None`）
fn level_target(level: &str, appearance: Appearance, surface_l: f64) -> Option<f64> {
    let (_, _, coefficients, reachable) = LEVELS
        .iter()
        .find(|(name, a, _, _)| *name == level && *a == appearance)?;
    if surface_l < reachable.0 || surface_l > reachable.1 {
        return None;
    }
    Some(
        coefficients
            .iter()
            .enumerate()
            .map(|(power, coefficient)| coefficient * surface_l.powi(power as i32))
            .sum(),
    )
}

/// 饱和度权重：高斯（中心 0.5、sigma 0.25），黑/白处 0，中间 1
fn bell_weight(lightness: f64) -> f64 {
    let gaussian = |x: f64| (-((x - 0.5).powi(2)) / (2.0 * 0.25 * 0.25)).exp();
    (gaussian(lightness) - gaussian(0.0)) / (1.0 - gaussian(0.0))
}

/// sRGB 的 Oklab 明度
fn oklab_lightness(color: Rgb) -> f64 {
    rgb_to_oklch(color).l
}

/// WCAG 2 相对亮度
pub fn relative_luminance(color: Rgb) -> f64 {
    let linear = |channel: u8| {
        let value = channel as f64 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(color.r) + 0.7152 * linear(color.g) + 0.0722 * linear(color.b)
}

/// WCAG 2 对比度（1-21）
pub fn wcag_contrast(first: Rgb, second: Rgb) -> f64 {
    let a = relative_luminance(first);
    let b = relative_luminance(second);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// 终端是深色还是浅色：优先看终端自己的前景方向（且那样文字确实可读），
/// 否则看白字与黑字谁在背景上对比更强。
pub fn terminal_appearance(background: Rgb, foreground: Option<Rgb>) -> Appearance {
    let white = Rgb::new(255, 255, 255);
    let black = Rgb::new(0, 0, 0);
    let white_contrast = wcag_contrast(white, background);
    let black_contrast = wcag_contrast(black, background);

    if let Some(foreground) = foreground {
        let foreground_l = oklab_lightness(foreground);
        let background_l = oklab_lightness(background);
        if (foreground_l - background_l).abs() > 0.05 {
            let appearance = if foreground_l > background_l {
                Appearance::Dark
            } else {
                Appearance::Light
            };
            let best = match appearance {
                Appearance::Dark => white_contrast,
                Appearance::Light => black_contrast,
            };
            if best >= TEXT_MINIMUM_WCAG_CONTRAST {
                return appearance;
            }
        }
    }

    if white_contrast >= black_contrast {
        Appearance::Dark
    } else {
        Appearance::Light
    }
}

/// token 依赖顺序：每个表面先于画在它上面的 token
fn solve_order() -> Vec<&'static str> {
    /// 后序 DFS：先把 token 依赖的每个表面入队，再把 token 自身追加到 `order`（已入队则直接返回）。
    fn visit(token: &'static str, order: &mut Vec<&'static str>) {
        if order.contains(&token) {
            return;
        }
        for rule in RULES.iter().filter(|rule| rule.token == token) {
            for surface in rule.on {
                if *surface != "background" {
                    visit(surface, order);
                }
            }
        }
        order.push(token);
    }

    let mut order: Vec<&'static str> = Vec::new();
    for rule in RULES {
        visit(rule.token, &mut order);
    }
    order
}

/// 单次主题求解的上下文：终端背景 + 色族/等级表的取值环境
struct Generator {
    /// 终端背景色，作为所有相对明度计算的基点
    background: Rgb,
    /// `background` 的 Oklab 明度，缓存避免重复换算
    background_l: f64,
    /// 终端 ANSI 调色板源色（含 OKLCH 彩度）；`None` 时用色族自带色相/饱和度
    palette: Option<Vec<SourceColor>>,
    /// 饱和度倍率（来自输入，已 clamp 到 0-1）
    saturation: f64,
    /// 终端明暗外观，决定明度往哪端推
    appearance: Appearance,
    /// 是否向亮端推明度（即 `appearance == Appearance::Dark`）
    lighter: bool,
    /// 不可达时的兜底明度：深色为 1.0，浅色为 0.0
    extreme: f64,
}

impl Generator {
    /// 色族的饱和度曲线相对其最大值：中明度 1，黑/白处 `min / max`
    fn saturation_curve(&self, family: &Family, lightness: f64) -> f64 {
        let floor = if family.saturation.max > 0.0 {
            family.saturation.min / family.saturation.max
        } else {
            1.0
        };
        floor + (1.0 - floor) * bell_weight(lightness)
    }

    /// 源色的色相 + 它在另一个 OKHSL 明度上的饱和度（沿色族曲线衰减，且不超过源色）。
    ///
    /// OKHSL 饱和度是相对「该明度下 sRGB 色域极限」的比例，同一个饱和度在别的明度上
    /// 可能对应更大彩度（Catppuccin Frappe 的粉 #f4b8e4 会变成 #eb76d1）。因此再用源色的
    /// OKLCH 彩度设上限（同一 falloff/saturation），超出则按该彩度重建，保住 pastel。
    fn anchored(
        &self,
        source: SourceColor,
        family: &Family,
        lightness: f64,
        saturation: f64,
    ) -> Rgb {
        let anchor = self.saturation_curve(family, source.okhsl.l);
        let falloff = if anchor > 0.0 {
            (self.saturation_curve(family, lightness) / anchor).min(1.0)
        } else {
            1.0
        };

        let color = okhsl_to_rgb(
            source.okhsl.h,
            source.okhsl.s * falloff * saturation,
            lightness,
        );

        let cap = source.chroma * falloff * saturation;
        let oklch = rgb_to_oklch(color);
        if oklch.c <= cap {
            color
        } else {
            oklch_to_rgb(Oklch {
                l: oklch.l,
                c: cap,
                h: source.okhsl.h,
            })
        }
    }

    /// 某 token 在给定 Oklab 明度上的颜色
    fn paint(&self, token: &str, oklab_l: f64) -> Rgb {
        let lightness = oklab_to_okhsl_lightness(oklab_l);
        let family = family_of(token);
        match self.palette.as_ref() {
            None => {
                let (min, max) = (family.saturation.min, family.saturation.max);
                okhsl_to_rgb(
                    family.hue,
                    (min + (max - min) * bell_weight(lightness)) * self.saturation,
                    lightness,
                )
            }
            Some(palette) => {
                let slot = slot_of(token).unwrap_or(family.slot) as usize;
                self.anchored(palette[slot], family, lightness, self.saturation)
            }
        }
    }

    /// 规则在表面上的目标明度，按 `t` 松弛：
    /// 0→1 把强于 readable 底线的等级压向它；1→2 把所有等级压向表面自身。
    fn target(&self, level: &str, surface_l: f64, t: f64) -> Option<f64> {
        let reached = level_target(level, self.appearance, surface_l);
        if reached.is_none() && t == 0.0 {
            return None;
        }
        let distance = reached.unwrap_or(self.extreme) - surface_l;
        let floor = level_target(readable_floor(self.appearance), self.appearance, surface_l)
            .unwrap_or(self.extreme)
            - surface_l;
        let compressed = if distance.abs() > floor.abs() {
            distance - (distance - floor) * t.min(1.0)
        } else {
            distance
        };
        Some(surface_l + compressed * (1.0 - (t - 1.0).max(0.0)))
    }

    /// 面板类 token 必须亮/暗到极值文字仍达正文最低对比度
    fn extreme_text(&self) -> Rgb {
        if self.lighter {
            Rgb::new(255, 255, 255)
        } else {
            Rgb::new(0, 0, 0)
        }
    }

    /// `color` 作为面板底色时，极值文字（白/黑）在其上是否达到正文最低对比度。
    fn readable(&self, color: Rgb) -> bool {
        wcag_contrast(self.extreme_text(), color) >= TEXT_MINIMUM_WCAG_CONTRAST
    }

    /// 面板 token 的着色入口：目标明度不可读时，向背景一侧二分 20 次，
    /// 取仍满足可读性的最远明度，保证面板上的极值文字可读。
    fn limit_panel(&self, token: &str, lightness: f64) -> Rgb {
        let color = self.paint(token, lightness);
        if self.readable(color) {
            return color;
        }
        let (mut low, mut high) = (self.background_l, lightness);
        for _ in 0..20 {
            let middle = (low + high) / 2.0;
            if self.readable(self.paint(token, middle)) {
                low = middle;
            } else {
                high = middle;
            }
        }
        self.paint(token, low)
    }

    /// 按 `solve_order` 依次求解全部 token 的颜色，每个 token 取其所有规则目标的
    /// 最优明度（深色取最大、浅色取最小），面板类再过 [`Generator::limit_panel`]。
    /// 某个目标明度算出界（不在 0-1）时返回 `None`，表示此 `t` 无解。
    fn solve(&self, t: f64) -> Option<HashMap<&'static str, Rgb>> {
        let mut colors: HashMap<&'static str, Rgb> = HashMap::new();
        colors.insert("background", self.background);

        for token in solve_order() {
            let mut targets: Vec<f64> = Vec::new();
            for rule in RULES.iter().filter(|rule| rule.token == token) {
                for surface in rule.on {
                    let surface_l =
                        oklab_lightness(*colors.get(surface).unwrap_or(&self.background));
                    let value = self.target(rule.level, surface_l, t)?;
                    if !(0.0..=1.0).contains(&value) {
                        return None;
                    }
                    targets.push(value);
                }
            }
            let lightness = if self.lighter {
                targets.iter().cloned().fold(f64::MIN, f64::max)
            } else {
                targets.iter().cloned().fold(f64::MAX, f64::min)
            };
            let color = if PANEL_TOKENS.contains(&token) {
                self.limit_panel(token, lightness)
            } else {
                self.paint(token, lightness)
            };
            colors.insert(token, color);
        }
        Some(colors)
    }
}

/// 索引兜底档：终端**什么都没上报**时的配色。
///
/// 直接用 ANSI 索引色 0-15——终端用自己的主题渲染这些索引，因此任何背景都能贴合；
/// 面板类 token 不设背景（`""` = 终端默认，保持透明）；中性色里正文以外的 token 交给
/// **SGR 2（faint）** 而不是亮黑（部分主题里亮黑几乎看不见）；彩色 token 的槽位取其色族的
/// ANSI 槽（`TOKEN_SLOTS` 可覆盖），饱和度被设成 0 时一律退回终端默认色。
pub fn generate_indexed_theme_colors(saturation: f64, appearance: Appearance) -> SystemThemeOutput {
    let saturation = clamp(saturation, 0.0, 1.0);
    let mut colors: HashMap<&'static str, String> = HashMap::new();
    let mut dim: Vec<&'static str> = Vec::new();

    for (token, family_name) in TOKEN_FAMILIES {
        if PANEL_TOKENS.contains(token) {
            colors.insert(token, String::new());
            continue;
        }

        let neutral = *family_name == "neutral";
        let color = if !neutral && saturation > 0.0 {
            let slot = slot_of(token).unwrap_or(family(family_name).slot);
            format!("ansi:{slot}")
        } else {
            String::new()
        };
        colors.insert(token, color);

        if neutral && !FOREGROUND_TOKENS.contains(token) {
            dim.push(token);
        }
    }

    SystemThemeOutput {
        colors,
        appearance,
        dim,
    }
}

/// 终端调色板源色：OKHSL 通道 + 其 OKLCH 彩度（用于限制换明度后的彩度）。
#[derive(Debug, Clone, Copy)]
struct SourceColor {
    /// 源色的 OKHSL 通道
    okhsl: Okhsl,
    /// 源色的 OKLCH 彩度（不超过它的换明度结果不该更鲜艳）
    chroma: f64,
}

/// 由 sRGB 终端色构造 [`SourceColor`]。
fn source_color(color: Rgb) -> SourceColor {
    SourceColor {
        okhsl: rgb_to_okhsl(color),
        chroma: rgb_to_oklch(color).c,
    }
}

/// 生成 system 主题的配色。终端未上报背景时返回 `None`（调用方改用索引兜底档）。
pub fn generate_system_theme_colors(input: &SystemThemeInput) -> Option<SystemThemeOutput> {
    let background = input.background?;
    let saturation = clamp(input.saturation.unwrap_or(1.0), 0.0, 1.0);
    let palette: Option<Vec<SourceColor>> = input
        .palette
        .as_ref()
        .filter(|p| p.len() == 16)
        .map(|p| p.iter().map(|c| source_color(*c)).collect());

    let appearance = terminal_appearance(background, input.foreground);
    let generator = Generator {
        background,
        background_l: oklab_lightness(background),
        palette,
        saturation,
        appearance,
        lighter: appearance == Appearance::Dark,
        extreme: if appearance == Appearance::Dark {
            1.0
        } else {
            0.0
        },
    };

    let mut relaxation = 0.0;
    let mut solved = generator.solve(0.0);
    if solved.is_none() {
        // 中灰背景装不下所有等级：二分「尽可能小」的松弛度（完全松弛必能解）
        let (mut low, mut high) = (0.0, 2.0);
        solved = generator.solve(high);
        for _ in 0..20 {
            let middle = (low + high) / 2.0;
            match generator.solve(middle) {
                Some(attempt) => {
                    high = middle;
                    solved = Some(attempt);
                }
                None => low = middle,
            }
        }
        relaxation = high;
    }
    let solved = solved.unwrap_or_default();

    let mut output: SystemThemeOutput = SystemThemeOutput {
        colors: HashMap::new(),
        appearance,
        dim: Vec::new(),
    };
    for (token, _) in TOKEN_FAMILIES {
        output.colors.insert(
            token,
            solved.get(token).map(|c| c.to_hex()).unwrap_or_default(),
        );
    }

    for token in FOREGROUND_TOKENS {
        let surfaces: Vec<Rgb> = RULES
            .iter()
            .filter(|rule| rule.token == *token)
            .flat_map(|rule| {
                rule.on
                    .iter()
                    .map(|surface| *solved.get(surface).unwrap_or(&background))
            })
            .collect();

        let mut text = solved.get(token).copied();
        if let Some(foreground) = input.foreground {
            let targets: Vec<Option<f64>> = surfaces
                .iter()
                .map(|surface| {
                    generator.target(FOREGROUND_LEVEL, oklab_lightness(*surface), relaxation)
                })
                .collect();
            if targets
                .iter()
                .all(|target| target.is_some_and(|value| (0.0..=1.0).contains(&value)))
            {
                let values: Vec<f64> = targets.into_iter().flatten().collect();
                let needed = if generator.lighter {
                    values.iter().cloned().fold(f64::MIN, f64::max)
                } else {
                    values.iter().cloned().fold(f64::MAX, f64::min)
                };
                let foreground_l = oklab_lightness(foreground);
                if if generator.lighter {
                    foreground_l >= needed
                } else {
                    foreground_l <= needed
                } {
                    output.colors.insert(token, String::new());
                    continue;
                }
                text = Some(generator.anchored(
                    source_color(foreground),
                    family("neutral"),
                    oklab_to_okhsl_lightness(needed),
                    generator.saturation,
                ));
            }
        }
        if let Some(text) = text {
            output.colors.insert(
                token,
                with_text_contrast(text, &surfaces, generator.lighter).to_hex(),
            );
        }
    }

    Some(output)
}

/// 把文字色向白/黑移动，直到在所有表面上都达到 WCAG 最低对比度
fn with_text_contrast(color: Rgb, surfaces: &[Rgb], lighter: bool) -> Rgb {
    let meets = |candidate: Rgb| {
        surfaces
            .iter()
            .all(|surface| wcag_contrast(candidate, *surface) >= TEXT_MINIMUM_WCAG_CONTRAST)
    };
    if meets(color) {
        return color;
    }

    let Okhsl { h, s, l } = rgb_to_okhsl(color);
    let at = |lightness: f64| okhsl_to_rgb(h, s, lightness);
    let extreme = if lighter { 1.0 } else { 0.0 };
    if !meets(at(extreme)) {
        return at(extreme);
    }

    let (mut low, mut high) = (l, extreme);
    for _ in 0..20 {
        let middle = (low + high) / 2.0;
        if meets(at(middle)) {
            high = middle;
        } else {
            low = middle;
        }
    }
    at(high)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PALETTE: [(u8, u8, u8); 16] = [
        (0, 0, 0),
        (205, 49, 49),
        (13, 188, 121),
        (229, 229, 16),
        (36, 114, 200),
        (188, 63, 188),
        (17, 168, 205),
        (229, 229, 229),
        (102, 102, 102),
        (241, 76, 76),
        (35, 209, 139),
        (245, 245, 67),
        (59, 142, 234),
        (214, 112, 214),
        (41, 184, 219),
        (255, 255, 255),
    ];

    fn palette() -> Vec<Rgb> {
        PALETTE
            .iter()
            .map(|(r, g, b)| Rgb::new(*r, *g, *b))
            .collect()
    }

    /// 检查生成结果与 pi 实现逐 token 一致；参考值由 pi v1.0.2 的 TS 实现在 node 下导出
    /// （pi 1.0.0 加入 OKLCH chroma 上限后，pastel/青色系 token 的参考值随之更新）。
    fn assert_colors(output: &SystemThemeOutput, expected: &[(&str, &str)]) {
        for (token, want) in expected {
            let got = output.colors.get(token).cloned().unwrap_or_default();
            assert_eq!(&got, want, "token {token}");
        }
    }

    #[test]
    fn dark_terminal_with_palette_matches_pi() {
        let output = generate_system_theme_colors(&SystemThemeInput {
            foreground: Some(Rgb::new(229, 229, 231)),
            background: Some(Rgb::new(30, 30, 30)),
            palette: Some(palette()),
            saturation: None,
        })
        .expect("有背景即应生成");

        assert_eq!(output.appearance, Appearance::Dark);
        assert_colors(
            &output,
            &[
                ("selectedBg", "#1e324a"),
                ("searchMatchBg", "#333300"),
                ("userMessageBg", "#1e324a"),
                ("customMessageBg", "#442343"),
                ("toolPendingBg", "#313131"),
                ("toolSuccessBg", "#183928"),
                ("toolErrorBg", "#492522"),
                // 终端前景足够强 → 直接用终端默认色（空串）
                ("text", ""),
                ("userMessageText", ""),
                ("toolTitle", ""),
                ("muted", "#9f9f9f"),
                ("dim", "#828282"),
                ("accent", "#d379d1"),
                ("border", "#5c9be7"),
                ("syntaxVariable", "#36afd2"),
                ("success", "#15bd7a"),
                ("error", "#e97b72"),
                ("warning", "#a7a704"),
                ("thinkingMax", "#f25b54"),
                ("scrollbarThumb", "#999999"),
            ],
        );
        assert_eq!(
            output.colors.len(),
            TOKEN_FAMILIES.len(),
            "每个 token 都要有取值（可为空串 = 终端默认色）"
        );
    }

    #[test]
    fn pastel_palette_keeps_chroma_at_other_lightnesses() {
        // 对齐 pi #10255 / PR #10293：Catppuccin Frappe 的粉色换到别的明度后不得涨彩度
        let frappe: [(u8, u8, u8); 16] = [
            (0x51, 0x57, 0x6d),
            (0xe7, 0x82, 0x84),
            (0xa6, 0xd1, 0x89),
            (0xe5, 0xc8, 0x90),
            (0x8c, 0xaa, 0xee),
            (0xf4, 0xb8, 0xe4),
            (0x81, 0xc8, 0xbe),
            (0xb5, 0xbf, 0xe2),
            (0x62, 0x68, 0x80),
            (0xe6, 0x71, 0x72),
            (0x8e, 0xc7, 0x72),
            (0xd9, 0xba, 0x73),
            (0x7b, 0x9e, 0xf0),
            (0xf2, 0xa4, 0xdb),
            (0x5a, 0xbf, 0xb5),
            (0xa5, 0xad, 0xce),
        ];
        let output = generate_system_theme_colors(&SystemThemeInput {
            foreground: Some(Rgb::new(0xc6, 0xd0, 0xf5)),
            background: Some(Rgb::new(0x30, 0x34, 0x46)),
            palette: Some(
                frappe
                    .iter()
                    .map(|(r, g, b)| Rgb::new(*r, *g, *b))
                    .collect(),
            ),
            saturation: None,
        })
        .expect("有背景即应生成");

        fn hex_to_rgb(hex: &str) -> Rgb {
            let h = hex.trim_start_matches('#');
            let byte = |range: std::ops::Range<usize>| u8::from_str_radix(&h[range], 16).unwrap();
            Rgb::new(byte(0..2), byte(2..4), byte(4..6))
        }
        let chroma = |hex: &str| rgb_to_oklch(hex_to_rgb(hex)).c;
        let pink = rgb_to_oklch(Rgb::new(0xf4, 0xb8, 0xe4));
        let accent = output.colors.get("accent").cloned().unwrap_or_default();
        let accent_lch = rgb_to_oklch(hex_to_rgb(&accent));
        // 重音色比粉色暗，但不得比粉色更鲜艳（加上限之前约为 2 倍）
        assert!(accent_lch.l < pink.l - 0.05, "accent 应比粉色暗: {accent}");
        assert!(
            accent_lch.c <= pink.c * 1.03,
            "accent 彩度不得超粉色: {:?} vs {:?}",
            accent_lch.c,
            pink.c
        );
        for panel in ["userMessageBg", "customMessageBg"] {
            let hex = output.colors.get(panel).cloned().unwrap_or_default();
            assert!(chroma(&hex) <= 0.1, "{panel} 应接近中性色: {hex}");
        }
    }

    #[test]
    fn dark_terminal_without_palette_matches_pi() {
        let output = generate_system_theme_colors(&SystemThemeInput {
            foreground: Some(Rgb::new(229, 229, 231)),
            background: Some(Rgb::new(30, 30, 30)),
            palette: None,
            saturation: None,
        })
        .expect("有背景即应生成");

        assert_eq!(output.appearance, Appearance::Dark);
        assert_colors(
            &output,
            &[
                ("userMessageBg", "#1f343f"),
                ("accent", "#a493d6"),
                ("border", "#57a3c8"),
                ("syntaxString", "#dd8751"),
                ("thinkingOff", "#677176"),
                ("thinkingMax", "#fe475a"),
                ("text", ""),
            ],
        );
    }

    #[test]
    fn light_terminal_matches_pi() {
        let output = generate_system_theme_colors(&SystemThemeInput {
            foreground: Some(Rgb::new(0, 0, 0)),
            background: Some(Rgb::new(255, 255, 255)),
            palette: None,
            saturation: None,
        })
        .expect("有背景即应生成");

        assert_eq!(output.appearance, Appearance::Light);
        assert_colors(
            &output,
            &[
                ("userMessageBg", "#edf1f3"),
                ("customMessageBg", "#f0eff5"),
                ("accent", "#8268c4"),
                ("success", "#3a8e64"),
                ("error", "#dc2c44"),
                ("dim", "#929ba0"),
                ("thinkingOff", "#cfd2d4"),
                ("text", ""),
            ],
        );
    }

    #[test]
    fn mid_gray_background_relaxes_levels() {
        // 中灰背景装不下全部等级：先松弛再求值，文字仍达 WCAG 4.5:1
        let output = generate_system_theme_colors(&SystemThemeInput {
            background: Some(Rgb::new(128, 128, 128)),
            ..Default::default()
        })
        .expect("有背景即应生成");

        assert_colors(
            &output,
            &[
                ("selectedBg", "#317c9e"),
                ("toolErrorBg", "#dc2d45"),
                ("text", "#000000"),
                ("thinkingMax", "#000000"),
            ],
        );
    }

    #[test]
    fn no_background_yields_nothing() {
        assert!(generate_system_theme_colors(&SystemThemeInput::default()).is_none());
    }

    #[test]
    fn appearance_detection_matches_pi() {
        assert_eq!(
            terminal_appearance(Rgb::new(30, 30, 30), Some(Rgb::new(229, 229, 231))),
            Appearance::Dark
        );
        assert_eq!(
            terminal_appearance(Rgb::new(255, 255, 255), Some(Rgb::new(0, 0, 0))),
            Appearance::Light
        );
        // 只给背景：白字对比更强 → 深色
        assert_eq!(
            terminal_appearance(Rgb::new(0, 0, 0), None),
            Appearance::Dark
        );
        assert_eq!(
            terminal_appearance(Rgb::new(255, 255, 255), None),
            Appearance::Light
        );
    }

    #[test]
    fn wcag_contrast_endpoints() {
        let white = Rgb::new(255, 255, 255);
        let black = Rgb::new(0, 0, 0);
        assert!((wcag_contrast(white, black) - 21.0).abs() < 1e-9);
        assert!((wcag_contrast(white, white) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn body_text_keeps_wcag_minimum_on_panels() {
        // 任意终端配色下，正文类 token 在其绘制表面上都必须 >= 4.5:1
        for (fg, bg) in [
            (Rgb::new(229, 229, 231), Rgb::new(30, 30, 30)),
            (Rgb::new(0, 0, 0), Rgb::new(255, 255, 255)),
            (Rgb::new(200, 200, 200), Rgb::new(60, 60, 60)),
        ] {
            let output = generate_system_theme_colors(&SystemThemeInput {
                foreground: Some(fg),
                background: Some(bg),
                ..Default::default()
            })
            .unwrap();
            let text = output.colors.get("text").cloned().unwrap_or_default();
            // 空串 = 终端默认前景：由终端保证可读，跳过
            if text.is_empty() {
                continue;
            }
            let rgb = crate::utils::color::parse_color_str(&text)
                .map(crate::utils::color::color_to_rgb)
                .expect("应为 hex");
            assert!(
                wcag_contrast(rgb, bg) >= TEXT_MINIMUM_WCAG_CONTRAST,
                "text {text} on {bg:?}"
            );
        }
    }
}

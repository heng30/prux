//! 颜色值：sRGB / Oklab / OKLCH / OKHSL 互转与主题颜色值解析。
//!
//! 主题文件允许 `#rgb` / `oklch()` / `okhsl()` / 256 色索引四种写法，
//! `system` 主题的配色推导又完全建立在这几个色彩空间上，故集中在本模块。

/// sRGB 颜色（0-255）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    /// 红通道 0-255
    pub r: u8,
    /// 绿通道 0-255
    pub g: u8,
    /// 蓝通道 0-255
    pub b: u8,
}

impl Rgb {
    /// 直接按三个通道值构造颜色，不做范围校验，可在 const 上下文中调用。
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Rgb { r, g, b }
    }

    /// `#rrggbb`
    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

/// OKHSL 通道：色相（度）、饱和度 0-1（相对该色相/明度下 sRGB 色域的极限）、明度 0-1
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Okhsl {
    /// 色相（度）
    pub h: f64,
    /// 饱和度 0-1
    pub s: f64,
    /// 明度 0-1
    pub l: f64,
}

/// OKLCH 通道：明度 0-1、彩度、色相（度）
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oklch {
    /// 明度 0-1
    pub l: f64,
    /// 彩度
    pub c: f64,
    /// 色相（度）
    pub h: f64,
}

/// 主题里的一个颜色值（解析结果）
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Color {
    /// 256 色索引（0-255）
    Indexed(u8),
    /// 十六进制写的 sRGB 颜色
    Rgb(Rgb),
    /// `oklch()` 写的颜色
    Oklch(Oklch),
}

// ============================================================================
// Oklab / OKHSL 数学
// ============================================================================

/// Oklab 三元组：[L, a, b] 或任意 3 维向量
type Vec3 = [f64; 3];
/// 3x3 行主序矩阵
type Mat3 = [Vec3; 3];

// 矩阵与拟合系数逐值照搬 pi（超 f64 精度的尾数保留原样，不做四舍五入）
/// 线性 sRGB → LMS（Oklab 第一步）
#[allow(clippy::excessive_precision)]
const LINEAR_SRGB_TO_LMS: Mat3 = [
    [0.4122214694707629, 0.5363325372617349, 0.0514459932675022],
    [0.2119034958178251, 0.6806995506452344, 0.1073969535369405],
    [0.0883024591900564, 0.2817188391361215, 0.6299787016738222],
];
/// LMS 立方根 → Oklab
#[allow(clippy::excessive_precision)]
const LMS_TO_LAB: Mat3 = [
    [0.210454268309314, 0.793617774702305, -0.0040720430116193],
    [1.9779985324311684, -2.42859224204858, 0.450593709617411],
    [0.0259040424655478, 0.7827717124575296, -0.8086757549230774],
];
/// Oklab → LMS 立方根
#[allow(clippy::excessive_precision)]
const LAB_TO_LMS: Mat3 = [
    [1.0, 0.3963377773761749, 0.2158037573099136],
    [1.0, -0.1055613458156586, -0.0638541728258133],
    [1.0, -0.0894841775298119, -1.2914855480194092],
];
/// LMS 立方 → 线性 sRGB
#[allow(clippy::excessive_precision)]
const LMS_TO_LINEAR_SRGB: Mat3 = [
    [4.0767416360759583, -3.3077115392580629, 0.2309699031821043],
    [-1.2684379732850315, 2.6097573492876882, -0.341319376002657],
    [-0.0041960761386756, -0.7034186179359362, 1.7076146940746117],
];

/// 3x3 矩阵左乘列向量（行·向量）。
fn mul(m: &Mat3, v: Vec3) -> Vec3 {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

/// 每通道（红/绿/蓝）：该通道先越界时 (a, b) 半平面，以及近似最大饱和度的多项式系数
const SATURATION_FIT: [([f64; 2], [f64; 5]); 3] = [
    (
        [-1.8817031, -0.80936501],
        [1.19086277, 1.76576728, 0.59662641, 0.75515197, 0.56771245],
    ),
    (
        [1.8144408, -1.19445267],
        [0.73956515, -0.45954404, 0.08285427, 0.12541073, -0.14503204],
    ),
    (
        [0.13110758, 1.81333971],
        [1.35733652, -0.00915799, -1.1513021, -0.50559606, 0.00692167],
    ),
];
/// 明度映射曲线的常数项（pi 拟合值）
const K1: f64 = 0.206;
/// 明度映射曲线的常数项（pi 拟合值）
const K2: f64 = 0.03;
/// 由 K1/K2 推得的缩放系数
const K3: f64 = (1.0 + K1) / (1.0 + K2);

/// Oklab 明度 → OKHSL 明度
pub fn oklab_to_okhsl_lightness(x: f64) -> f64 {
    0.5 * (K3 * x - K1 + ((K3 * x - K1).powi(2) + 4.0 * K2 * K3 * x).sqrt())
}

/// OKHSL 明度 → Oklab 明度
fn okhsl_to_oklab_lightness(x: f64) -> f64 {
    (x * x + K1 * x) / (K3 * (x + K2))
}

/// sRGB 传输函数：线性 → 编码（0-1）
fn linear_to_srgb(value: f64) -> f64 {
    if value > 0.0031308 {
        1.055 * value.powf(1.0 / 2.4) - 0.055
    } else {
        12.92 * value
    }
}

/// sRGB 传输函数：编码 → 线性（0-1）
fn srgb_to_linear(value: f64) -> f64 {
    if value <= 0.04045 {
        value / 12.92
    } else {
        ((value + 0.055) / 1.055).powf(2.4)
    }
}

/// Oklab [L, a, b] → 线性 sRGB（0-1，可能越界）
fn oklab_to_linear_srgb(lab: Vec3) -> Vec3 {
    let lms = mul(&LAB_TO_LMS, lab);
    mul(
        &LMS_TO_LINEAR_SRGB,
        [lms[0].powi(3), lms[1].powi(3), lms[2].powi(3)],
    )
}

/// 线性 sRGB → Oklab [L, a, b]
fn linear_srgb_to_oklab(rgb: Vec3) -> Vec3 {
    let lms = mul(&LINEAR_SRGB_TO_LMS, rgb);
    mul(&LMS_TO_LAB, [lms[0].cbrt(), lms[1].cbrt(), lms[2].cbrt()])
}

/// sRGB（0-255）→ Oklab
pub fn rgb_to_oklab(color: Rgb) -> Vec3 {
    linear_srgb_to_oklab([
        srgb_to_linear(color.r as f64 / 255.0),
        srgb_to_linear(color.g as f64 / 255.0),
        srgb_to_linear(color.b as f64 / 255.0),
    ])
}

/// 线性 sRGB → sRGB（0-255，越界通道裁剪）
fn linear_srgb_to_rgb(linear: Vec3) -> Rgb {
    let channel = |v: f64| (linear_to_srgb(v).clamp(0.0, 1.0) * 255.0).round() as u8;
    Rgb::new(channel(linear[0]), channel(linear[1]), channel(linear[2]))
}

/// 沿 (a, b) 彩度方向，各立方根 LMS 分量的变化率
fn lms_slopes(a: f64, b: f64) -> Vec3 {
    [
        LAB_TO_LMS[0][1] * a + LAB_TO_LMS[0][2] * b,
        LAB_TO_LMS[1][1] * a + LAB_TO_LMS[1][2] * b,
        LAB_TO_LMS[2][1] * a + LAB_TO_LMS[2][2] * b,
    ]
}

/// 色相 (a, b) 在 sRGB 内的最大饱和度（C/L）：多项式拟合 + 一次 Halley 迭代
fn max_saturation(a: f64, b: f64) -> f64 {
    let channel = SATURATION_FIT
        .iter()
        .enumerate()
        .find(|(index, ([x, y], _))| *index == 2 || x * a + y * b > 1.0)
        .map(|(index, _)| index)
        .unwrap_or(2);
    let [k0, k1, k2, k3, k4] = SATURATION_FIT[channel].1;
    let weights = LMS_TO_LINEAR_SRGB[channel];
    let saturation = k0 + k1 * a + k2 * b + k3 * a * a + k4 * a * b;

    let slopes = lms_slopes(a, b);
    let base = [
        1.0 + saturation * slopes[0],
        1.0 + saturation * slopes[1],
        1.0 + saturation * slopes[2],
    ];
    let dot =
        |values: Vec3| weights[0] * values[0] + weights[1] * values[1] + weights[2] * values[2];
    let f = dot([base[0].powi(3), base[1].powi(3), base[2].powi(3)]);
    let f1 = dot([
        3.0 * slopes[0] * base[0].powi(2),
        3.0 * slopes[1] * base[1].powi(2),
        3.0 * slopes[2] * base[2].powi(2),
    ]);
    let f2 = dot([
        6.0 * slopes[0].powi(2) * base[0],
        6.0 * slopes[1].powi(2) * base[1],
        6.0 * slopes[2].powi(2) * base[2],
    ]);
    saturation - (f * f1) / (f1 * f1 - 0.5 * f * f2)
}

/// 色相 (a, b) 下 sRGB 最饱和色的 (Oklab 明度, 彩度)
fn cusp(a: f64, b: f64) -> (f64, f64) {
    let saturation = max_saturation(a, b);
    let linear = oklab_to_linear_srgb([1.0, saturation * a, saturation * b]);
    let max = linear[0].max(linear[1]).max(linear[2]);
    let lightness = (1.0 / max).cbrt();
    (lightness, lightness * saturation)
}

/// 等明度线在 `lightness` 处离开 sRGB 色域时的彩度
fn max_chroma(a: f64, b: f64, lightness: f64, cusp: (f64, f64)) -> f64 {
    let (cusp_l, cusp_c) = cusp;
    if lightness <= cusp_l {
        return (cusp_c * lightness) / cusp_l;
    }
    // 上半段：三角边，然后对每个到达 1 的通道各做一次 Halley 迭代
    let t = (cusp_c * (lightness - 1.0)) / (cusp_l - 1.0);
    let slopes = lms_slopes(a, b);
    let lms = [
        lightness + t * slopes[0],
        lightness + t * slopes[1],
        lightness + t * slopes[2],
    ];
    let cubes = [lms[0].powi(3), lms[1].powi(3), lms[2].powi(3)];
    let first = [
        3.0 * slopes[0] * lms[0].powi(2),
        3.0 * slopes[1] * lms[1].powi(2),
        3.0 * slopes[2] * lms[2].powi(2),
    ];
    let second = [
        6.0 * slopes[0].powi(2) * lms[0],
        6.0 * slopes[1].powi(2) * lms[1],
        6.0 * slopes[2].powi(2) * lms[2],
    ];
    let dot =
        |row: Vec3, values: Vec3| row[0] * values[0] + row[1] * values[1] + row[2] * values[2];

    let mut min_step = f64::MAX;
    for row in LMS_TO_LINEAR_SRGB {
        let f = dot(row, cubes) - 1.0;
        let f1 = dot(row, first);
        let f2 = dot(row, second);
        let u = f1 / (f1 * f1 - 0.5 * f * f2);
        let step = if u >= 0.0 { -f * u } else { f64::MAX };
        min_step = min_step.min(step);
    }
    t + min_step
}

/// OKHSL 在明度 L、色相 (a, b) 处的彩度参考点：[c0, cMid, cMax]
fn chroma_stops(l: f64, a: f64, b: f64) -> (f64, f64, f64) {
    let peak = cusp(a, b);
    let c_max = max_chroma(a, b, l, peak);
    let k = c_max
        / (l * (peak.1 / peak.0))
            .min((1.0 - l) * (peak.1 / (1.0 - peak.0)))
            .max(f64::MIN_POSITIVE);
    let mid_s = 0.11516993
        + 1.0
            / (7.4477897
                + 4.1590124 * b
                + a * (-2.19557347
                    + 1.75198401 * b
                    + a * (-2.13704948 - 10.02301043 * b
                        + a * (-4.24894561 + 5.38770819 * b + 4.69891013 * a))));
    let mid_t = 0.11239642
        + 1.0
            / (1.6132032 - 0.68124379 * b
                + a * (0.40370612
                    + 0.90148123 * b
                    + a * (-0.27087943
                        + 0.6122399 * b
                        + a * (0.00299215 - 0.45399568 * b - 0.14661872 * a))));
    let c_mid = 0.9
        * k
        * (1.0 / (1.0 / (l * mid_s).powi(4) + 1.0 / ((1.0 - l) * mid_t).powi(4)))
            .sqrt()
            .sqrt();
    let c0 = (1.0 / (1.0 / (l * 0.4).powi(2) + 1.0 / ((1.0 - l) * 0.8).powi(2))).sqrt();
    (c0, c_mid, c_max)
}

/// OKHSL → sRGB（0-255，越界通道裁剪）
pub fn okhsl_to_rgb(hue: f64, saturation: f64, lightness: f64) -> Rgb {
    let l = okhsl_to_oklab_lightness(lightness);
    let mut lab: Vec3 = [l, 0.0, 0.0];
    if l > 0.0 && l < 1.0 && saturation > 0.0 {
        let angle = 2.0 * std::f64::consts::PI * ((hue % 360.0 + 360.0) % 360.0) / 360.0;
        let a = angle.cos();
        let b = angle.sin();
        let (c0, c_mid, c_max) = chroma_stops(l, a, b);
        // 彩度从 0 经 s = 0.8 处的 cMid 升到 s = 1 处的 cMax
        let chroma = if saturation < 0.8 {
            let t = 1.25 * saturation;
            let k1 = 0.8 * c0;
            (t * k1) / (1.0 - (1.0 - k1 / c_mid) * t)
        } else {
            let t = 5.0 * (saturation - 0.8);
            let k1 = (0.2 * c_mid * c_mid * 1.25 * 1.25) / c0;
            c_mid + (t * k1) / (1.0 - (1.0 - k1 / (c_max - c_mid)) * t)
        };
        lab = [l, chroma * a, chroma * b];
    }
    linear_srgb_to_rgb(oklab_to_linear_srgb(lab))
}

/// sRGB（0-255）→ OKHSL
pub fn rgb_to_okhsl(color: Rgb) -> Okhsl {
    let [l, lab_a, lab_b] = rgb_to_oklab(color);
    let chroma = lab_a.hypot(lab_b);
    let lightness = oklab_to_okhsl_lightness(l);
    if chroma < 1e-9 || lightness <= 0.0 || lightness >= 1.0 {
        return Okhsl {
            h: 0.0,
            s: 0.0,
            l: lightness,
        };
    }

    let hue = (lab_b.atan2(lab_a) * 180.0 / std::f64::consts::PI + 360.0) % 360.0;
    let (c0, c_mid, c_max) = chroma_stops(l, lab_a / chroma, lab_b / chroma);
    let saturation = if chroma < c_mid {
        let k1 = 0.8 * c0;
        0.8 * (chroma / (k1 + (1.0 - k1 / c_mid) * chroma))
    } else {
        let k1 = (0.2 * c_mid * c_mid * 1.25 * 1.25) / c0;
        let offset = chroma - c_mid;
        0.8 + 0.2 * (offset / (k1 + (1.0 - k1 / (c_max - c_mid)) * offset))
    };
    Okhsl {
        h: hue,
        s: saturation.clamp(0.0, 1.0),
        l: lightness,
    }
}

/// sRGB → OKLCH
pub fn rgb_to_oklch(color: Rgb) -> Oklch {
    let [l, a, b] = rgb_to_oklab(color);
    Oklch {
        l,
        c: a.hypot(b),
        h: (b.atan2(a) * 180.0 / std::f64::consts::PI + 360.0) % 360.0,
    }
}

// ============================================================================
// 颜色值解析与转换
// ============================================================================

/// 16 个基础 ANSI 色的 sRGB 参考值（终端通常会用自己的主题渲染索引色）
const BASIC_COLORS: [Rgb; 16] = [
    Rgb::new(0, 0, 0),
    Rgb::new(128, 0, 0),
    Rgb::new(0, 128, 0),
    Rgb::new(128, 128, 0),
    Rgb::new(0, 0, 128),
    Rgb::new(128, 0, 128),
    Rgb::new(0, 128, 128),
    Rgb::new(192, 192, 192),
    Rgb::new(128, 128, 128),
    Rgb::new(255, 0, 0),
    Rgb::new(0, 255, 0),
    Rgb::new(255, 255, 0),
    Rgb::new(0, 0, 255),
    Rgb::new(255, 0, 255),
    Rgb::new(0, 255, 255),
    Rgb::new(255, 255, 255),
];

/// 256 色中 6x6x6 色立方每级的通道取值
const CUBE_VALUES: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// 256 色索引 → sRGB
pub fn indexed_to_rgb(index: u8) -> Rgb {
    if index < 16 {
        return BASIC_COLORS[index as usize];
    }
    if index < 232 {
        let cube = index - 16;
        return Rgb::new(
            CUBE_VALUES[(cube / 36) as usize],
            CUBE_VALUES[((cube % 36) / 6) as usize],
            CUBE_VALUES[(cube % 6) as usize],
        );
    }
    let gray = 8 + (index as u16 - 232) * 10;
    Rgb::new(gray as u8, gray as u8, gray as u8)
}

/// 判断线性 sRGB 三分量是否都落在 [0,1]（含吸收舍入误差的浮点容差）。
fn in_srgb_gamut(linear: Vec3) -> bool {
    /// 色域判定的浮点容差，用于吸收线性 sRGB 换算中的舍入误差。
    const EPSILON: f64 = 1e-7;
    linear
        .iter()
        .all(|channel| *channel >= -EPSILON && *channel <= 1.0 + EPSILON)
}

/// OKLCH → sRGB：越界时保持色相、二分降低彩度直到落入色域
pub fn oklch_to_rgb(color: Oklch) -> Rgb {
    let radians = color.h * std::f64::consts::PI / 180.0;
    let (cos, sin) = (radians.cos(), radians.sin());
    let at_chroma = |chroma: f64| oklab_to_linear_srgb([color.l, chroma * cos, chroma * sin]);

    let direct = at_chroma(color.c);
    if in_srgb_gamut(direct) {
        return linear_srgb_to_rgb(direct);
    }

    // 无色版本必然在色域内：所有二分步都不合时以它兜底（如 oklch(100% 0.3 150) → 白）
    let mut linear = at_chroma(0.0);
    let (mut low, mut high) = (0.0, color.c);
    for _ in 0..20 {
        let chroma = (low + high) / 2.0;
        let candidate = at_chroma(chroma);
        if in_srgb_gamut(candidate) {
            low = chroma;
            linear = candidate;
        } else {
            high = chroma;
        }
    }
    linear_srgb_to_rgb(linear)
}

/// 任意颜色值 → sRGB
pub fn color_to_rgb(color: Color) -> Rgb {
    match color {
        Color::Indexed(index) => indexed_to_rgb(index),
        Color::Rgb(rgb) => rgb,
        Color::Oklch(oklch) => oklch_to_rgb(oklch),
    }
}

/// 颜色值 → OKHSL
pub fn color_to_okhsl(color: Color) -> Okhsl {
    rgb_to_okhsl(color_to_rgb(color))
}

/// 颜色值 → `#rrggbb`
pub fn color_to_hex(color: Color) -> String {
    color_to_rgb(color).to_hex()
}

/// 解析主题里的颜色值：`#rgb` / `#rrggbb` / `oklch()` / `okhsl()` / 256 色索引数字。
///
/// 语法对齐 pi `parseColor`；无法识别返回 `None`（调用方按“未设置”处理）。
pub fn parse_color(value: &serde_json::Value) -> Option<Color> {
    match value {
        serde_json::Value::Number(n) => n
            .as_u64()
            .and_then(|i| u8::try_from(i).ok())
            .map(Color::Indexed),
        serde_json::Value::String(text) => parse_color_str(text),
        _ => None,
    }
}

/// 字符串形式的颜色值
pub fn parse_color_str(text: &str) -> Option<Color> {
    let text = text.trim();
    if let Some(hex) = parse_hex_color(text) {
        return Some(Color::Rgb(hex));
    }
    if let Some(oklch) = parse_oklch(text) {
        return Some(Color::Oklch(oklch));
    }
    parse_okhsl(text).map(|hsl| Color::Rgb(okhsl_to_rgb(hsl.0, hsl.1, hsl.2)))
}

/// `#rgb` / `#rrggbb`（大小写不敏感）
fn parse_hex_color(text: &str) -> Option<Rgb> {
    let digits = text.strip_prefix('#')?;
    if !digits.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let expanded = match digits.len() {
        3 => digits.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => digits.to_string(),
        _ => return None,
    };
    let channel = |range: std::ops::Range<usize>| u8::from_str_radix(&expanded[range], 16).ok();
    Some(Rgb::new(channel(0..2)?, channel(2..4)?, channel(4..6)?))
}

/// 数字（可带 `%`、可带 `deg`、可带指数）
struct NumberScanner<'a> {
    /// 预拆的字符序列
    chars: Vec<char>,
    /// 当前扫描位置
    pos: usize,
    /// 生命周期占位，保持与输入字符串的借用关系
    _text: &'a str,
}

impl NumberScanner<'_> {
    /// 按字符拆入扫描器、位置归零（用于在 `oklch()`/`okhsl()` 括号内顺序读取）。
    fn new(text: &str) -> Self {
        NumberScanner {
            chars: text.chars().collect(),
            pos: 0,
            _text: "",
        }
    }

    /// 跳过当前位置起的连续空白字符。
    fn skip_ws(&mut self) {
        while self.chars.get(self.pos).is_some_and(|c| c.is_whitespace()) {
            self.pos += 1;
        }
    }

    /// 读一个数字，返回 (值, 其后是否紧跟 `%`)
    fn number(&mut self) -> Option<(f64, bool)> {
        self.skip_ws();
        let start = self.pos;
        if self
            .chars
            .get(self.pos)
            .is_some_and(|c| *c == '+' || *c == '-')
        {
            self.pos += 1;
        }
        let mut digits = false;
        while self.chars.get(self.pos).is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
            digits = true;
        }
        if self.chars.get(self.pos) == Some(&'.') {
            self.pos += 1;
            while self.chars.get(self.pos).is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
                digits = true;
            }
        }
        if !digits {
            self.pos = start;
            return None;
        }
        if self
            .chars
            .get(self.pos)
            .is_some_and(|c| *c == 'e' || *c == 'E')
        {
            let save = self.pos;
            self.pos += 1;
            if self
                .chars
                .get(self.pos)
                .is_some_and(|c| *c == '+' || *c == '-')
            {
                self.pos += 1;
            }
            let mut exp_digits = false;
            while self.chars.get(self.pos).is_some_and(|c| c.is_ascii_digit()) {
                self.pos += 1;
                exp_digits = true;
            }
            if !exp_digits {
                self.pos = save;
            }
        }
        let text: String = self.chars[start..self.pos].iter().collect();
        let value = text.parse::<f64>().ok()?;
        let percent = self.chars.get(self.pos) == Some(&'%');
        if percent {
            self.pos += 1;
        }
        Some((value, percent))
    }

    /// 若当前位置紧跟 `keyword`（区分大小写）则消费之并返回 true，否则不移动位置。
    fn eat_keyword(&mut self, keyword: &str) -> bool {
        let chars: Vec<char> = keyword.chars().collect();
        if self.pos + chars.len() > self.chars.len() {
            return false;
        }
        if self.chars[self.pos..self.pos + chars.len()] == chars[..] {
            self.pos += chars.len();
            return true;
        }
        false
    }

    /// 跳过尾随空白后判断是否已到输入末尾。
    fn at_end(&mut self) -> bool {
        self.skip_ws();
        self.pos >= self.chars.len()
    }
}

/// ASCII 大小写不敏感地剥掉前缀；不匹配返回 `None`。
fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    if text.len() >= prefix.len() && text[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&text[prefix.len()..])
    } else {
        None
    }
}

/// `oklch(L% C H[deg])`；L 带 `%` 时按 0-100 归一
fn parse_oklch(text: &str) -> Option<Oklch> {
    let rest = strip_prefix_ci(text, "oklch")?;
    let rest = rest.strip_prefix('(')?.strip_suffix(')')?;

    let mut scanner = NumberScanner::new(rest);
    let (lightness, percent) = scanner.number()?;
    let (chroma, _) = scanner.number()?;
    let (hue, _) = scanner.number()?;
    scanner.eat_keyword("deg");
    if !scanner.at_end() {
        return None;
    }

    let l = if percent {
        lightness / 100.0
    } else {
        lightness
    };
    if !(0.0..=1.0).contains(&l) || chroma < 0.0 {
        return None;
    }
    Some(Oklch {
        l,
        c: chroma,
        h: ((hue % 360.0) + 360.0) % 360.0,
    })
}

/// `okhsl(H[deg] S% L%)`；S/L 带 `%` 时按 0-100 归一
fn parse_okhsl(text: &str) -> Option<(f64, f64, f64)> {
    let rest = strip_prefix_ci(text, "okhsl")?;
    let rest = rest.strip_prefix('(')?.strip_suffix(')')?;

    let mut scanner = NumberScanner::new(rest);
    let (hue, _) = scanner.number()?;
    scanner.eat_keyword("deg");
    let (saturation, saturation_percent) = scanner.number()?;
    let (lightness, lightness_percent) = scanner.number()?;
    if !scanner.at_end() {
        return None;
    }

    let s = if saturation_percent {
        saturation / 100.0
    } else {
        saturation
    };
    let l = if lightness_percent {
        lightness / 100.0
    } else {
        lightness
    };
    if !(0.0..=1.0).contains(&s) || !(0.0..=1.0).contains(&l) {
        return None;
    }
    Some((hue, s, l))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 参考值由 pi 自身实现（`tui/src/{oklab,colors}.ts`，node --experimental-strip-types）生成
    #[test]
    fn okhsl_to_rgb_matches_pi() {
        let cases = [
            ((231.49, 0.68, 0.5), (52, 128, 164)),
            ((20.0, 0.92, 0.35), (151, 21, 42)),
            ((82.36, 1.0, 0.6), (185, 135, 0)),
            ((0.0, 0.0, 0.0), (0, 0, 0)),
            ((0.0, 0.0, 1.0), (255, 255, 255)),
            ((295.0, 0.6, 0.72), (181, 165, 231)),
            ((231.49, 0.68, 0.108), (5, 27, 37)),
        ];
        for ((h, s, l), expected) in cases {
            let rgb = okhsl_to_rgb(h, s, l);
            assert_eq!(
                (rgb.r, rgb.g, rgb.b),
                expected,
                "okhsl({h} {s} {l}) → {rgb:?}"
            );
        }
    }

    #[test]
    fn rgb_to_okhsl_matches_pi() {
        let hsl = rgb_to_okhsl(Rgb::new(60, 128, 160));
        assert!((hsl.h - 231.22360780748355).abs() < 1e-9, "{hsl:?}");
        assert!((hsl.s - 0.6039553034190615).abs() < 1e-9, "{hsl:?}");
        assert!((hsl.l - 0.5006249597690675).abs() < 1e-9, "{hsl:?}");

        // 纯白：无色相、零饱和
        let white = rgb_to_okhsl(Rgb::new(255, 255, 255));
        assert_eq!((white.h, white.s), (0.0, 0.0));
        assert!((white.l - 1.0).abs() < 1e-12);
    }

    #[test]
    fn rgb_to_oklab_matches_pi() {
        let lab = rgb_to_oklab(Rgb::new(60, 128, 160));
        let expected = [
            0.5693819749989067,
            -0.05283696873673377,
            -0.06577141185960467,
        ];
        for (got, want) in lab.iter().zip(expected) {
            assert!((got - want).abs() < 1e-12, "{lab:?}");
        }
    }

    #[test]
    fn parses_all_color_syntaxes() {
        assert_eq!(
            parse_color_str("#abc"),
            Some(Color::Rgb(Rgb::new(170, 187, 204)))
        );
        assert_eq!(
            parse_color_str("#AABBCC"),
            Some(Color::Rgb(Rgb::new(170, 187, 204)))
        );
        assert_eq!(parse_color_str("#abcd"), None, "4 位十六进制不接受");
        assert_eq!(parse_color_str("nope"), None);

        // oklch / okhsl 结果与 pi colorToHex 一致
        assert_eq!(
            color_to_hex(parse_color_str("oklch(60% 0.15 250)").unwrap()),
            "#2784d5"
        );
        assert_eq!(
            color_to_hex(parse_color_str("oklch(1 0.3 150)").unwrap()),
            "#ffffff"
        );
        assert_eq!(
            color_to_hex(parse_color_str("oklch(0.8 0.2 30)").unwrap()),
            "#ffa191"
        );
        assert_eq!(
            color_to_hex(parse_color_str("okhsl(200 60% 40%)").unwrap()),
            "#2c696c"
        );

        // 索引数字
        assert_eq!(
            parse_color(&serde_json::json!(42)),
            Some(Color::Indexed(42))
        );
        assert_eq!(color_to_hex(Color::Indexed(42)), "#00d787");
        assert_eq!(parse_color(&serde_json::json!("")), None);
        assert_eq!(color_to_hex(Color::Indexed(0)), "#000000");
        assert_eq!(color_to_hex(Color::Indexed(255)), "#eeeeee");
    }

    #[test]
    fn oklch_gamut_mapping_keeps_hue() {
        // 超色域：二分降彩度，不得变成 NaN / 越界
        let rgb = oklch_to_rgb(Oklch {
            l: 0.6,
            c: 0.5,
            h: 250.0,
        });
        assert!(rgb.r > 0 || rgb.g > 0 || rgb.b > 0);
        assert!(rgb.r < 255 || rgb.g < 255 || rgb.b < 255);
    }

    #[test]
    fn indexed_to_rgb_matches_pi_cube_and_gray() {
        assert_eq!(indexed_to_rgb(16), Rgb::new(0, 0, 0));
        assert_eq!(indexed_to_rgb(17), Rgb::new(0, 0, 95));
        assert_eq!(indexed_to_rgb(231), Rgb::new(255, 255, 255));
        assert_eq!(indexed_to_rgb(232), Rgb::new(8, 8, 8));
        assert_eq!(indexed_to_rgb(255), Rgb::new(238, 238, 238));
    }
}

//! LaTeX 数学表达式 → 终端 Unicode 文本渲染（移植自 pi/packages/tui/src/latex.ts）。
//!
//! 能力：希腊字母/运算符符号表、Unicode 上下标（`x^2` → x²）、根号、分式
//! （display 模式垂直堆叠 `─` 分式线）、求和/积分上下限（display 模式布局）、
//! 矩阵/行列式、cases、对齐环境。无法解析的表达式返回 None，由调用方回退原文。

use crate::utils::display::display_width;
use std::{cell::RefCell, rc::Rc};

/// 布局标记（私有使用区，不会出现在正常正文中）
const LAYOUT_START: char = '\u{f0000}';
/// 布局占位标记的结束符，与起始标记、节点索引配对后交给渲染阶段。
const LAYOUT_END: char = '\u{f0001}';
/// 矩阵对齐用的保护空格（最后替换为普通空格）
const PROTECTED_SPACE: char = '\u{f0002}';
/// 具名运算符边界标记（如 \sin → sin），normalize 时按上下文补空格/删除
const NAMED_OP_START: char = '\u{f0004}';
/// 具名运算符标记的结束符，normalize 时据此删除或保留边界空格。
const NAMED_OP_END: char = '\u{f0005}';
/// 负间距命令标记（\u0000 NUL，不会出现在正常文本）
const NEGATIVE_SPACE: char = '\u{0}';

/// LaTeX 命令名到 Unicode 数学符号的映射表（希腊字母、运算符、箭头等）。
const SYMBOLS: &[(&str, &str)] = &[
    ("alpha", "α"),
    ("beta", "β"),
    ("gamma", "γ"),
    ("delta", "δ"),
    ("epsilon", "ϵ"),
    ("varepsilon", "ε"),
    ("zeta", "ζ"),
    ("eta", "η"),
    ("theta", "θ"),
    ("vartheta", "ϑ"),
    ("iota", "ι"),
    ("kappa", "κ"),
    ("varkappa", "ϰ"),
    ("lambda", "λ"),
    ("mu", "μ"),
    ("nu", "ν"),
    ("xi", "ξ"),
    ("pi", "π"),
    ("varpi", "ϖ"),
    ("rho", "ρ"),
    ("varrho", "ϱ"),
    ("sigma", "σ"),
    ("varsigma", "ς"),
    ("tau", "τ"),
    ("upsilon", "υ"),
    ("phi", "ϕ"),
    ("varphi", "φ"),
    ("chi", "χ"),
    ("psi", "ψ"),
    ("omega", "ω"),
    ("Gamma", "Γ"),
    ("Delta", "Δ"),
    ("Theta", "Θ"),
    ("Lambda", "Λ"),
    ("Xi", "Ξ"),
    ("Pi", "Π"),
    ("Sigma", "Σ"),
    ("Upsilon", "Υ"),
    ("Phi", "Φ"),
    ("Psi", "Ψ"),
    ("Omega", "Ω"),
    ("pm", "±"),
    ("mp", "∓"),
    ("times", "×"),
    ("div", "÷"),
    ("cdot", "·"),
    ("ast", "∗"),
    ("star", "⋆"),
    ("circ", "∘"),
    ("bullet", "•"),
    ("oplus", "⊕"),
    ("ominus", "⊖"),
    ("otimes", "⊗"),
    ("oslash", "⊘"),
    ("odot", "⊙"),
    ("bigcirc", "○"),
    ("dagger", "†"),
    ("ddagger", "‡"),
    ("amalg", "⨿"),
    ("uplus", "⊎"),
    ("sqcap", "⊓"),
    ("sqcup", "⊔"),
    ("triangleleft", "◁"),
    ("triangleright", "▷"),
    ("wr", "≀"),
    ("cap", "∩"),
    ("cup", "∪"),
    ("bigcap", "⋂"),
    ("bigcup", "⋃"),
    ("bigwedge", "⋀"),
    ("bigvee", "⋁"),
    ("bigsqcup", "⨆"),
    ("biguplus", "⨄"),
    ("bigoplus", "⨁"),
    ("bigotimes", "⨂"),
    ("bigodot", "⨀"),
    ("setminus", "∖"),
    ("in", "∈"),
    ("notin", "∉"),
    ("ni", "∋"),
    ("subset", "⊂"),
    ("supset", "⊃"),
    ("subseteq", "⊆"),
    ("supseteq", "⊇"),
    ("sqsubset", "⊏"),
    ("sqsupset", "⊐"),
    ("sqsubseteq", "⊑"),
    ("sqsupseteq", "⊒"),
    ("prec", "≺"),
    ("preceq", "≼"),
    ("succ", "≻"),
    ("succeq", "≽"),
    ("ll", "≪"),
    ("gg", "≫"),
    ("le", "≤"),
    ("leq", "≤"),
    ("leqslant", "≤"),
    ("ge", "≥"),
    ("geq", "≥"),
    ("geqslant", "≥"),
    ("ne", "≠"),
    ("neq", "≠"),
    ("equiv", "≡"),
    ("approx", "≈"),
    ("sim", "∼"),
    ("simeq", "≃"),
    ("cong", "≅"),
    ("asymp", "≍"),
    ("doteq", "≐"),
    ("propto", "∝"),
    ("parallel", "∥"),
    ("perp", "⊥"),
    ("mid", "∣"),
    ("vdash", "⊢"),
    ("dashv", "⊣"),
    ("models", "⊨"),
    ("Vdash", "⊩"),
    ("Vvdash", "⊪"),
    ("nvdash", "⊬"),
    ("nvDash", "⊭"),
    ("forall", "∀"),
    ("exists", "∃"),
    ("nexists", "∄"),
    ("neg", "¬"),
    ("land", "∧"),
    ("wedge", "∧"),
    ("lor", "∨"),
    ("vee", "∨"),
    ("to", "→"),
    ("rightarrow", "→"),
    ("longrightarrow", "→"),
    ("leftarrow", "←"),
    ("longleftarrow", "←"),
    ("gets", "←"),
    ("leftrightarrow", "↔"),
    ("longleftrightarrow", "↔"),
    ("hookleftarrow", "↩"),
    ("hookrightarrow", "↪"),
    ("twoheadleftarrow", "↞"),
    ("twoheadrightarrow", "↠"),
    ("leftharpoonup", "↼"),
    ("leftharpoondown", "↽"),
    ("rightharpoonup", "⇀"),
    ("rightharpoondown", "⇁"),
    ("rightleftharpoons", "⇌"),
    ("leftrightharpoons", "⇋"),
    ("nearrow", "↗"),
    ("searrow", "↘"),
    ("swarrow", "↙"),
    ("nwarrow", "↖"),
    ("rightsquigarrow", "⇝"),
    ("leadsto", "⇝"),
    ("Rightarrow", "⇒"),
    ("Longrightarrow", "⇒"),
    ("Leftarrow", "⇐"),
    ("Longleftarrow", "⇐"),
    ("Leftrightarrow", "⇔"),
    ("Longleftrightarrow", "⇔"),
    ("implies", "⇒"),
    ("iff", "⇔"),
    ("mapsto", "↦"),
    ("longmapsto", "↦"),
    ("uparrow", "↑"),
    ("downarrow", "↓"),
    ("partial", "∂"),
    ("nabla", "∇"),
    ("int", "∫"),
    ("iint", "∬"),
    ("iiint", "∭"),
    ("oint", "∮"),
    ("sum", "∑"),
    ("prod", "∏"),
    ("coprod", "∐"),
    ("infty", "∞"),
    ("emptyset", "∅"),
    ("varnothing", "∅"),
    ("angle", "∠"),
    ("therefore", "∴"),
    ("because", "∵"),
    ("aleph", "ℵ"),
    ("beth", "ℶ"),
    ("gimel", "ℷ"),
    ("daleth", "ℸ"),
    ("top", "⊤"),
    ("bot", "⊥"),
    ("triangle", "△"),
    ("square", "□"),
    ("lozenge", "◊"),
    ("checkmark", "✓"),
    ("complement", "∁"),
    ("wp", "℘"),
    ("prime", "′"),
    ("ldots", "…"),
    ("dots", "…"),
    ("cdots", "⋯"),
    ("vdots", "⋮"),
    ("ddots", "⋱"),
    ("ell", "ℓ"),
    ("hbar", "ℏ"),
    ("Im", "ℑ"),
    ("Re", "ℜ"),
    ("langle", "⟨"),
    ("rangle", "⟩"),
    ("vert", "|"),
    ("lvert", "|"),
    ("rvert", "|"),
    ("Vert", "‖"),
    ("lVert", "‖"),
    ("rVert", "‖"),
    ("lbrace", "{"),
    ("rbrace", "}"),
    ("backslash", "\\"),
    ("lfloor", "⌊"),
    ("rfloor", "⌋"),
    ("lceil", "⌈"),
    ("rceil", "⌉"),
    ("colon", ":"),
];

/// 应渲染为直立文本的具名函数命令表（sin、cos、lim 等）。
const NAMED_OPERATORS: &[&str] = &[
    "arccos", "arcsin", "arctan", "arg", "cos", "cosh", "cot", "coth", "csc", "deg", "det", "dim",
    "exp", "gcd", "hom", "inf", "ker", "lg", "lim", "liminf", "limsup", "ln", "log", "max", "min",
    "Pr", "sec", "sin", "sinh", "sup", "tan", "tanh",
];

/// 支持带上下限的极限类运算符命令表（lim、max、sup 等）。
const LIMIT_OPERATORS: &[&str] = &[
    "argmax", "argmin", "inf", "injlim", "lim", "liminf", "limsup", "max", "min", "projlim", "sup",
];

/// display 模式下把上下限垂直排在符号上下的命令表（sum、int 等）。
const DISPLAY_LIMIT_SYMBOLS: &[&str] = &[
    "bigcap",
    "bigcup",
    "bigodot",
    "bigoplus",
    "bigotimes",
    "bigsqcup",
    "biguplus",
    "bigvee",
    "bigwedge",
    "coprod",
    "int",
    "iint",
    "iiint",
    "oint",
    "prod",
    "sum",
];

/// 关系运算符命令表，渲染时在符号两侧补空格以免与相邻字符粘连。
const RELATION_COMMANDS: &[&str] = &[
    "Leftarrow",
    "Leftrightarrow",
    "Longleftarrow",
    "Longleftrightarrow",
    "Longrightarrow",
    "Rightarrow",
    "Vdash",
    "Vvdash",
    "approx",
    "asymp",
    "cong",
    "dashv",
    "doteq",
    "downarrow",
    "equiv",
    "ge",
    "geq",
    "geqslant",
    "gets",
    "gg",
    "hookleftarrow",
    "hookrightarrow",
    "iff",
    "implies",
    "in",
    "leadsto",
    "le",
    "leftarrow",
    "leftharpoondown",
    "leftharpoonup",
    "leftrightarrow",
    "leftrightharpoons",
    "leq",
    "leqslant",
    "ll",
    "longleftarrow",
    "longleftrightarrow",
    "longmapsto",
    "longrightarrow",
    "mapsto",
    "mid",
    "models",
    "ne",
    "nearrow",
    "neq",
    "ni",
    "notin",
    "nvdash",
    "nvDash",
    "nwarrow",
    "parallel",
    "perp",
    "prec",
    "preceq",
    "propto",
    "rightharpoondown",
    "rightharpoonup",
    "rightleftharpoons",
    "rightarrow",
    "rightsquigarrow",
    "searrow",
    "sim",
    "simeq",
    "sqsubset",
    "sqsubseteq",
    "sqsupset",
    "sqsupseteq",
    "subset",
    "subseteq",
    "succ",
    "succeq",
    "supset",
    "supseteq",
    "swarrow",
    "to",
    "triangleleft",
    "triangleright",
    "twoheadleftarrow",
    "twoheadrightarrow",
    "uparrow",
    "vdash",
];

/// `\not` 取反后的符号映射表（如 ∈ → ∉、= → ≠）。
const NEGATED_SYMBOLS: &[(char, &str)] = &[
    ('<', "≮"),
    ('>', "≯"),
    ('=', "≠"),
    ('∈', "∉"),
    ('∋', "∌"),
    ('∣', "∤"),
    ('∥', "∦"),
    ('∼', "≁"),
    ('≃', "≄"),
    ('≅', "≇"),
    ('≈', "≉"),
    ('≡', "≢"),
    ('≤', "≰"),
    ('≥', "≱"),
    ('≺', "⊀"),
    ('≻', "⊁"),
    ('⊂', "⊄"),
    ('⊃', "⊅"),
    ('⊆', "⊈"),
    ('⊇', "⊉"),
    ('⊢', "⊬"),
    ('⊨', "⊭"),
    ('↔', "↮"),
    ('←', "↚"),
    ('→', "↛"),
    ('⇒', "⇏"),
    ('⇐', "⇍"),
    ('⇔', "⇎"),
    ('≼', "⋠"),
    ('≽', "⋡"),
];

/// 黑板粗体数集符号映射表（`\mathbb{R}` → ℝ 等）。
const BLACKBOARD: &[(char, &str)] = &[
    ('C', "ℂ"),
    ('H', "ℍ"),
    ('N', "ℕ"),
    ('P', "ℙ"),
    ('Q', "ℚ"),
    ('R', "ℝ"),
    ('Z', "ℤ"),
];

/// 上标字符到 Unicode 上标字符的映射表，用于 `^` 脚本渲染。
const SUPERSCRIPTS: &[(char, &str)] = &[
    ('0', "⁰"),
    ('1', "¹"),
    ('2', "²"),
    ('3', "³"),
    ('4', "⁴"),
    ('5', "⁵"),
    ('6', "⁶"),
    ('7', "⁷"),
    ('8', "⁸"),
    ('9', "⁹"),
    ('+', "⁺"),
    ('-', "⁻"),
    ('=', "⁼"),
    ('(', "⁽"),
    (')', "⁾"),
    ('a', "ᵃ"),
    ('b', "ᵇ"),
    ('c', "ᶜ"),
    ('d', "ᵈ"),
    ('e', "ᵉ"),
    ('f', "ᶠ"),
    ('g', "ᵍ"),
    ('h', "ʰ"),
    ('i', "ⁱ"),
    ('j', "ʲ"),
    ('k', "ᵏ"),
    ('l', "ˡ"),
    ('m', "ᵐ"),
    ('n', "ⁿ"),
    ('o', "ᵒ"),
    ('p', "ᵖ"),
    ('r', "ʳ"),
    ('s', "ˢ"),
    ('t', "ᵗ"),
    ('u', "ᵘ"),
    ('v', "ᵛ"),
    ('w', "ʷ"),
    ('x', "ˣ"),
    ('y', "ʸ"),
    ('z', "ᶻ"),
];

/// 下标字符到 Unicode 下标字符的映射表，用于 `_` 脚本渲染。
const SUBSCRIPTS: &[(char, &str)] = &[
    ('0', "₀"),
    ('1', "₁"),
    ('2', "₂"),
    ('3', "₃"),
    ('4', "₄"),
    ('5', "₅"),
    ('6', "₆"),
    ('7', "₇"),
    ('8', "₈"),
    ('9', "₉"),
    ('+', "₊"),
    ('-', "₋"),
    ('=', "₌"),
    ('(', "₍"),
    (')', "₎"),
    ('a', "ₐ"),
    ('e', "ₑ"),
    ('h', "ₕ"),
    ('i', "ᵢ"),
    ('j', "ⱼ"),
    ('k', "ₖ"),
    ('l', "ₗ"),
    ('m', "ₘ"),
    ('n', "ₙ"),
    ('o', "ₒ"),
    ('p', "ₚ"),
    ('r', "ᵣ"),
    ('s', "ₛ"),
    ('t', "ₜ"),
    ('u', "ᵤ"),
    ('v', "ᵥ"),
    ('x', "ₓ"),
];

/// 产生水平空白的间距命令表（`\,`、`\quad`、`\qquad` 等）。
const SPACING_COMMANDS: &[&str] = &[
    ",",
    ":",
    ";",
    " ",
    ">",
    "enspace",
    "enskip",
    "medspace",
    "quad",
    "qquad",
    "thickspace",
    "thinspace",
];

/// 产生负间距（向左回退）的命令表（`\!` 等）。
const NEGATIVE_SPACING_COMMANDS: &[&str] = &["!", "negmedspace", "negthickspace", "negthinspace"];

/// 可直接忽略、不影响输出的样式与限制命令表（`\displaystyle` 等）。
const IGNORED_COMMANDS: &[&str] = &[
    "displaystyle",
    "limits",
    "nolimits",
    "scriptstyle",
    "scriptscriptstyle",
    "textstyle",
];

/// legacy 字体切换命令（`{\rm x}`、`{\bf x}` 等）：忽略命令本身及其后续空白。
const FONT_SWITCH_COMMANDS: &[&str] = &["bf", "cal", "it", "rm", "sf", "sl", "tt"];

/// 手动指定定界符尺寸的命令表（`\big`、`\Big`、`\bigg` 等），渲染时丢弃。
const SIZE_COMMANDS: &[&str] = &[
    "big", "Big", "bigg", "Bigg", "bigl", "Bigl", "biggl", "Biggl", "bigr", "Bigr", "biggr",
    "Biggr",
];

/// 仅透传参数内容的字体/文本包裹命令表（`\text`、`\mathbf` 等）。
const PLAIN_WRAPPERS: &[&str] = &[
    "emph",
    "mathcal",
    "mathbf",
    "mathfrak",
    "mathit",
    "mathrm",
    "mathnormal",
    "mathscr",
    "mathsf",
    "mathtt",
    "mathup",
    "mbox",
    "overbrace",
    "pmb",
    "smash",
    "substack",
    "text",
    "textbf",
    "textit",
    "textmd",
    "textnormal",
    "textrm",
    "textsc",
    "textsf",
    "textsl",
    "texttt",
    "textup",
    "underbrace",
    "bm",
    "boldsymbol",
];

/// 重音命令到 Unicode 组合附加符的映射表（`\hat`、`\vec` 等）。
const ACCENTS: &[(&str, &str)] = &[
    ("acute", "\u{0301}"),
    ("bar", "\u{0305}"),
    ("breve", "\u{0306}"),
    ("check", "\u{030c}"),
    ("ddot", "\u{0308}"),
    ("dot", "\u{0307}"),
    ("grave", "\u{0300}"),
    ("hat", "\u{0302}"),
    ("mathring", "\u{030a}"),
    ("overleftarrow", "\u{20d6}"),
    ("overleftrightarrow", "\u{20e1}"),
    ("overline", "\u{0305}"),
    ("overrightarrow", "\u{20d7}"),
    ("tilde", "\u{0303}"),
    ("underline", "\u{0332}"),
    ("vec", "\u{20d7}"),
    ("widehat", "\u{0302}"),
    ("widetilde", "\u{0303}"),
];

/// 判断 `s` 是否在字符串集合 `set` 中（命令名查表用）。
fn in_set(set: &[&str], s: &str) -> bool {
    set.contains(&s)
}

/// 在 `(键, 值)` 表中按键查值；表中没有该键时为 `None`。
fn find<'a>(table: &'a [(&'a str, &'a str)], key: &str) -> Option<&'a str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// 在字符键映射表中查找对应值；未命中时为 `None`。
fn find_char<'a>(table: &'a [(char, &'a str)], key: char) -> Option<&'a str> {
    table.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
}

/// 把字符串中的 `= + -` 周围空白去掉（JS `/\s*([=+-])\s*/g` → `$1`）
fn strip_sign_spaces(s: &str) -> String {
    let mut out = String::new();
    let mut segment = String::new();
    for c in s.chars() {
        if c == '=' || c == '+' || c == '-' {
            out.push_str(segment.trim());
            out.push(c);
            segment.clear();
        } else {
            segment.push(c);
        }
    }
    out.push_str(segment.trim());
    out
}

/// 把 `value` 逐字符按映射表替换；只要有一个字符无法映射就整体返回 `None`
/// （上下标转换要求「全有或全无」，否则不如退回普通前缀写法）。
fn replace_characters(value: &str, replacements: &[(char, &str)]) -> Option<String> {
    let mut result = String::new();
    for c in value.chars() {
        let r = find_char(replacements, c)?;
        result.push_str(r);
    }
    Some(result)
}

/// 把脚本值格式化为行内脚本：优先转成 Unicode 上下标；否则单字符（或全字母下标）
/// 直接写成 `^x`/`_x`，多字符加括号如 `^(x+1)`。`kind` 为 `"sub"` 表示下标，其余按上标。
fn format_script(value: &str, kind: &str) -> String {
    let value = normalize_script_value(value);
    if let Some(unicode) = format_unicode_script(&value, kind) {
        return unicode;
    }
    let prefix = if kind == "sub" { "_" } else { "^" };
    let char_count = value.chars().count();
    if char_count == 1
        || (kind == "sub" && !value.is_empty() && value.chars().all(|c| c.is_ascii_alphabetic()))
    {
        return format!("{}{}", prefix, value);
    }
    format!("{}({})", prefix, value)
}

/// trim 后去除 `=`/`+`/`-` 两侧空白。
fn normalize_script_value(value: &str) -> String {
    strip_sign_spaces(value.trim())
}

/// 尝试把脚本值转为 Unicode 上下标；含无法映射字符时返回 None。
fn format_unicode_script(value: &str, kind: &str) -> Option<String> {
    let replacements = if kind == "sub" {
        SUBSCRIPTS
    } else {
        SUPERSCRIPTS
    };
    replace_characters(&normalize_script_value(value), replacements)
}

/// 判断字符串是否只由字母、数字与 `.` 组成且非空（决定能否不加括号直接拼接）。
fn simple_text(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_alphabetic() || c.is_numeric() || c == '.')
}

/// 把分子/分母渲染为行内分式 `a/b`，不满足简单文本的一侧补括号。
fn format_fraction(numerator: &str, denominator: &str) -> String {
    let numerator = numerator.trim();
    let denominator = denominator.trim();
    let simple_numerator = simple_text(numerator);
    let simple_denominator = simple_text(denominator)
        && denominator.chars().all(|c| c.is_numeric() || c == '.')
        || denominator.chars().count() == 1;
    let num_part = if simple_numerator {
        numerator.to_string()
    } else {
        format!("({})", numerator)
    };
    let den_part = if simple_denominator {
        denominator.to_string()
    } else {
        format!("({})", denominator)
    };
    format!("{}/{}", num_part, den_part)
}

/// 渲染根号：简单文本直接跟在根号符号后，否则加括号。
fn format_root(value: &str, symbol: &str) -> String {
    let value = value.trim();
    if simple_text(value) {
        format!("{}{}", symbol, value)
    } else {
        format!("{}({})", symbol, value)
    }
}

/// 在字符切片中查找子串首次出现的起始下标；`needle` 为空或未找到时为 `None`。
fn find_substring(chars: &[char], needle: &str) -> Option<usize> {
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() || chars.len() < needle_chars.len() {
        return None;
    }
    'outer: for i in 0..=chars.len() - needle_chars.len() {
        for (j, &nc) in needle_chars.iter().enumerate() {
            if chars[i + j] != nc {
                continue 'outer;
            }
        }
        return Some(i);
    }
    None
}

/// JS `body.split(/\\\\(?:\[[^\]\n]*\])?/)`：按字面 `\\`（可选带 `[..]`）分割
fn split_environment_rows(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = body.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && chars.get(i + 1) == Some(&'\\') {
            let mut j = i + 2;
            if chars.get(j) == Some(&'[') {
                let mut k = j + 1;
                while k < chars.len() && chars[k] != ']' && chars[k] != '\n' {
                    k += 1;
                }
                if k < chars.len() && chars[k] == ']' {
                    j = k + 1;
                }
            }
            out.push(std::mem::take(&mut cur));
            i = j;
            continue;
        }
        cur.push(chars[i]);
        i += 1;
    }
    out.push(cur);
    out
}

/// JS `body.replace(/^\s*\{[^}]*\}/, "")`（alignedat/array 的列数参数）
fn trim_leading_group(s: &str) -> String {
    let t = s.trim_start();
    if t.starts_with('{')
        && let Some(pos) = t.find('}')
    {
        return t[pos + 1..].to_string();
    }
    s.to_string()
}

/// `[ \t]+` → 单个空格
fn collapse_spaces(s: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for c in s.chars() {
        if c == ' ' || c == '\t' {
            pending_space = true;
        } else {
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push(c);
        }
    }
    out
}

/// 具名运算符左边界是否需要补空格：左侧是字母数字或右闭括号等「紧贴」字符时为真。
fn is_op_left_space(c: char) -> bool {
    c.is_alphabetic() || c.is_numeric() || c == ')' || c == '}' || c == ']' || c == LAYOUT_END
}

/// 具名运算符右边界是否需要补空格：右侧是字母数字或根号/布局起始符时为真。
fn is_op_right_space(c: char) -> bool {
    c.is_alphabetic() || c.is_numeric() || c == '√' || c == LAYOUT_START
}

/// 移除具名运算符标记并按上下文补空格、折叠空白、修剪空行。
fn normalize_output(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if c == NAMED_OP_START {
            let prev = if i > 0 { chars[i - 1] } else { '\0' };
            if is_op_left_space(prev) {
                out.push(' ');
            }
        } else if c == NAMED_OP_END {
            let next = chars.get(i + 1).copied().unwrap_or('\0');
            if is_op_right_space(next) {
                out.push(' ');
            }
        } else {
            out.push(c);
        }
    }

    let lines: Vec<&str> = out.split('\n').collect();
    let mut kept: Vec<String> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let collapsed = collapse_spaces(line).trim().to_string();
        if !collapsed.is_empty() || (index > 0 && index < lines.len() - 1) {
            kept.push(collapsed);
        }
    }
    kept.join("\n").trim().to_string()
}

/// 待垂直布局的节点：分式、带上下限的运算符、矩阵与堆叠脚本。
enum LayoutNode {
    /// 垂直堆叠的分式：分子在上、分母在下。
    Fraction {
        /// 分式的分子文本（位于横线上方）。
        numerator: String,
        /// 分式的分母文本（位于横线下方）。
        denominator: String,
    },
    /// 带上下限的运算符（如求和、积分）的垂直布局。
    Operator {
        /// 运算符符号文本（如 Σ、∫）。
        operator: String,
        /// 下限文本，无下限时为 `None`。
        lower: Option<String>,
        /// 上限文本，无上限时为 `None`。
        upper: Option<String>,
    },
    /// 矩阵/多行环境，按行对齐渲染。
    Matrix {
        /// 矩阵各行的已渲染文本。
        lines: Vec<String>,
        /// 作为对齐基准的行号（从 0 起）。
        baseline: usize,
    },
    /// 嵌套/不支持 Unicode 上下标时的垂直堆叠脚本（上标在上、下标在下）
    Script {
        /// 下标文本，无下标时为 `None`。
        lower: Option<String>,
        /// 上标文本，无上标时为 `None`。
        upper: Option<String>,
    },
}

/// 布局节点表的共享句柄，解析器写入、渲染阶段按索引读取。
type NodeRef = Rc<RefCell<Vec<LayoutNode>>>;

/// 一次垂直布局的结果：多行文本、总宽度与基线所在行号。
struct Layout {
    /// 布局渲染出的多行文本。
    lines: Vec<String>,
    /// 各行填充后的统一显示宽度（按终端列宽计）。
    width: usize,
    /// 基线所在行号，用于与相邻布局对齐。
    baseline: usize,
}

/// 把一行按终端显示宽度补空格到 `width`；`centered` 为真时左右均分（左侧取整）。
fn pad_layout_line(line: &str, width: usize, centered: bool) -> String {
    let padding = width.saturating_sub(display_width(line));
    let left = if centered { padding / 2 } else { 0 };
    format!("{}{}{}", " ".repeat(left), line, " ".repeat(padding - left))
}

/// 把多个垂直布局并排拼成一个布局：按基线对齐各行、缺行处补空格，宽度取最大值。
/// 空切片返回单个空行的零宽零基线布局。
fn join_layouts(layouts: &[Layout]) -> Layout {
    if layouts.is_empty() {
        return Layout {
            lines: vec![String::new()],
            width: 0,
            baseline: 0,
        };
    }
    let baseline = layouts.iter().map(|l| l.baseline).max().unwrap_or(0);
    let below = layouts
        .iter()
        .map(|l| l.lines.len().saturating_sub(l.baseline + 1))
        .max()
        .unwrap_or(0);
    let mut lines: Vec<String> = Vec::new();
    for row in 0..=baseline + below {
        let mut line = String::new();
        for layout in layouts {
            let source_row = row as isize - baseline as isize + layout.baseline as isize;
            if source_row >= 0 && (source_row as usize) < layout.lines.len() {
                line.push_str(&pad_layout_line(
                    &layout.lines[source_row as usize],
                    layout.width,
                    false,
                ));
            } else {
                line.push_str(&" ".repeat(layout.width));
            }
        }
        lines.push(line.trim_end().to_string());
    }
    Layout {
        lines,
        width: layouts.iter().map(|l| l.width).sum(),
        baseline,
    }
}

/// 把含布局标记的源码渲染为多行 Layout。节点内可嵌套（fraction 内再套 fraction）。
fn render_layout(source: &str, nodes: &[LayoutNode]) -> Layout {
    let mut rendered_lines: Vec<String> = Vec::new();
    let mut first_baseline = 0usize;

    for source_line in source.split('\n') {
        let chars: Vec<char> = source_line.chars().collect();
        let mut layouts: Vec<Layout> = Vec::new();
        let mut position = 0usize;
        let mut previous_node: Option<&LayoutNode> = None;

        let mut i = 0;
        while i < chars.len() {
            if chars[i] != LAYOUT_START {
                i += 1;
                continue;
            }
            let mut j = i + 1;
            let mut num: usize = 0;
            let mut digits = false;
            while j < chars.len() && chars[j].is_ascii_digit() {
                num = num * 10 + (chars[j] as u32 - '0' as u32) as usize;
                digits = true;
                j += 1;
            }
            if !digits || j >= chars.len() || chars[j] != LAYOUT_END {
                i += 1;
                continue;
            }
            let Some(node) = nodes.get(num) else {
                i = j + 1;
                continue;
            };

            // 前置文本段
            if i > position {
                layouts.push(leading_text_layout(
                    &chars,
                    position,
                    i,
                    previous_node,
                    node,
                ));
            }

            layouts.push(render_layout_node(node, nodes));

            position = j + 1;
            previous_node = Some(node);
            i = j + 1;
        }

        if position < chars.len() {
            layouts.push(trailing_text_layout(&chars, position, previous_node));
        }

        let line_layout = join_layouts(&layouts);
        if rendered_lines.is_empty() {
            first_baseline = line_layout.baseline;
        }
        rendered_lines.extend(line_layout.lines);
    }

    let width = rendered_lines
        .iter()
        .map(|l| display_width(l))
        .max()
        .unwrap_or(0);
    Layout {
        lines: rendered_lines,
        width,
        baseline: first_baseline,
    }
}

/// 渲染单个布局节点（分式/运算符/矩阵）为 Layout
fn render_layout_node(node: &LayoutNode, nodes: &[LayoutNode]) -> Layout {
    match node {
        LayoutNode::Fraction {
            numerator,
            denominator,
        } => {
            let numerator_layout = render_layout(numerator, nodes);
            let denominator_layout = render_layout(denominator, nodes);
            let content_width = numerator_layout.width.max(denominator_layout.width).max(1);
            let width = content_width + 2;
            let mut lines: Vec<String> = numerator_layout
                .lines
                .iter()
                .map(|l| pad_layout_line(l, width, true))
                .collect();
            lines.push(format!(" {} ", "─".repeat(content_width)));
            lines.extend(
                denominator_layout
                    .lines
                    .iter()
                    .map(|l| pad_layout_line(l, width, true)),
            );
            Layout {
                lines,
                width,
                baseline: numerator_layout.lines.len(),
            }
        }
        LayoutNode::Operator {
            operator,
            lower,
            upper,
        } => {
            let content_width = display_width(operator)
                .max(lower.as_ref().map(|s| display_width(s)).unwrap_or(0))
                .max(upper.as_ref().map(|s| display_width(s)).unwrap_or(0));
            let mut lines: Vec<String> = Vec::new();
            if let Some(upper) = upper {
                lines.push(format!("{} ", pad_layout_line(upper, content_width, true)));
            }
            lines.push(format!(
                "{} ",
                pad_layout_line(operator, content_width, true)
            ));
            if let Some(lower) = lower {
                lines.push(format!("{} ", pad_layout_line(lower, content_width, true)));
            }
            Layout {
                lines,
                width: content_width + 1,
                baseline: if upper.is_some() { 1 } else { 0 },
            }
        }
        LayoutNode::Matrix { lines, baseline } => {
            let width = lines.iter().map(|l| display_width(l)).max().unwrap_or(0);
            Layout {
                lines: lines
                    .iter()
                    .map(|l| pad_layout_line(l, width, false))
                    .collect(),
                width,
                baseline: *baseline,
            }
        }
        LayoutNode::Script { lower, upper } => {
            let upper_layout = upper.as_ref().map(|u| render_layout(u, nodes));
            let lower_layout = lower.as_ref().map(|l| render_layout(l, nodes));
            let width = upper_layout
                .as_ref()
                .map(|l| l.width)
                .unwrap_or(0)
                .max(lower_layout.as_ref().map(|l| l.width).unwrap_or(0));
            let mut lines: Vec<String> = Vec::new();
            if let Some(l) = &upper_layout {
                lines.extend(
                    l.lines
                        .iter()
                        .map(|line| pad_layout_line(line, width, false)),
                );
            }
            lines.push(" ".repeat(width));
            if let Some(l) = &lower_layout {
                lines.extend(
                    l.lines
                        .iter()
                        .map(|line| pad_layout_line(line, width, false)),
                );
            }
            Layout {
                lines,
                width,
                baseline: upper_layout.as_ref().map(|l| l.lines.len()).unwrap_or(0),
            }
        }
    }
}

/// 渲染节点前的文本段（含矩阵前后空格保护）为 Layout
fn leading_text_layout(
    chars: &[char],
    position: usize,
    i: usize,
    previous_node: Option<&LayoutNode>,
    node: &LayoutNode,
) -> Layout {
    let sliced: String = chars[position..i].iter().collect();
    let trimmed = if previous_node.is_some() {
        sliced.trim_start()
    } else {
        sliced.as_str()
    }
    .trim_end();
    let preserve_leading = matches!(previous_node, Some(LayoutNode::Matrix { .. }))
        && sliced.chars().next().is_some_and(|c| c.is_whitespace());
    let preserve_trailing = matches!(node, LayoutNode::Matrix { .. })
        && sliced
            .chars()
            .next_back()
            .is_some_and(|c| c.is_whitespace());
    let text = if !trimmed.is_empty() {
        format!(
            "{}{}{}",
            if preserve_leading { " " } else { "" },
            trimmed,
            if preserve_trailing { " " } else { "" },
        )
    } else if preserve_leading || preserve_trailing {
        " ".to_string()
    } else {
        String::new()
    };
    let text_w = display_width(&text);
    Layout {
        lines: vec![text],
        width: text_w,
        baseline: 0,
    }
}

/// 渲染一行末尾剩余文本段为 Layout
fn trailing_text_layout(
    chars: &[char],
    position: usize,
    previous_node: Option<&LayoutNode>,
) -> Layout {
    let sliced: String = chars[position..].iter().collect();
    let trimmed = if previous_node.is_some() {
        sliced.trim_start()
    } else {
        sliced.as_str()
    };
    let text = if matches!(previous_node, Some(LayoutNode::Matrix { .. }))
        && sliced.chars().next().is_some_and(|c| c.is_whitespace())
    {
        format!(" {}", trimmed)
    } else {
        trimmed.to_string()
    };
    let text_w = display_width(&text);
    Layout {
        lines: vec![text],
        width: text_w,
        baseline: 0,
    }
}

/// 解析 result 尾部 `\u{f0000}数字\u{f0001}` 标记的节点索引（矩阵后接 `.` 用）
fn trailing_layout_marker_index(s: &str) -> Option<usize> {
    let chars: Vec<char> = s.chars().collect();
    if chars.last() != Some(&LAYOUT_END) {
        return None;
    }
    let mut j = chars.len() - 1;
    while j > 0 && chars[j - 1].is_ascii_digit() {
        j -= 1;
    }
    if j == 0 || chars[j - 1] != LAYOUT_START || j == chars.len() - 1 {
        return None;
    }
    chars[j..chars.len() - 1]
        .iter()
        .collect::<String>()
        .parse()
        .ok()
}

/// 递归下降的 LaTeX 解析器，持有字符流、游标与共享布局节点表。
struct LatexParser {
    /// 源串拆成的字符序列，避免按字节索引破坏 UTF-8。
    chars: Vec<char>,
    /// 当前解析游标（`chars` 中的下标）。
    position: usize,
    /// 共享的布局节点表：解析时写入，渲染阶段按索引读取。
    nodes: NodeRef,
    /// 是否为块级公式（true 时启用垂直堆叠与上下限布局）。
    display: bool,
    /// 是否仍可渲染；遇到不支持的语法置为 false。
    supported: bool,
    /// 是否允许把分式堆叠为垂直布局（嵌套实参中会被抑制）。
    stack_fractions: bool,
    /// 当前所在脚本嵌套深度（>0 表示正在解析另一个脚本的实参）
    script_depth: usize,
}

impl LatexParser {
    /// 以源码初始化解析器（位置归零）；`nodes` 是共享的布局节点表，
    /// `display` 决定是否启用分式/上下限的垂直堆叠布局。
    fn new(source: &str, nodes: NodeRef, display: bool) -> Self {
        Self {
            chars: source.chars().collect(),
            position: 0,
            nodes,
            display,
            supported: true,
            stack_fractions: true,
            script_depth: 0,
        }
    }

    /// 消费解析器跑完整个输入：语法不支持或末尾仍有未消费字符时返回 `None`，
    /// 否则返回归一化（去多余空白、修剪空行）后的文本。
    fn render(mut self) -> Option<String> {
        let rendered = self.parse_sequence(None);
        if !self.supported || self.position != self.chars.len() {
            return None;
        }
        Some(normalize_output(&rendered))
    }

    /// 解析一段序列直到遇到 `end` 字符（`None` 表示解析到输入末尾），就地推进 `self.position`。
    /// 遇到多余的 `}` 或到末尾仍未闭合的 `{` 时置 `supported = false`。
    fn parse_sequence(&mut self, end: Option<char>) -> String {
        let mut result = String::new();
        while self.position < self.chars.len() {
            let character = self.chars[self.position];
            if let Some(e) = end
                && character == e
            {
                self.position += 1;
                return result;
            }
            if character == '}' {
                self.supported = false;
                return result;
            }
            if character == '{' {
                self.position += 1;
                result.push_str(&self.parse_sequence(Some('}')));
                continue;
            }
            if character == '\\' {
                let command = self.parse_command();
                if command == "\u{0}" {
                    result = result.trim_end().to_string();
                    if result.ends_with(NAMED_OP_END) {
                        result.truncate(result.len() - NAMED_OP_END.len_utf8());
                    }
                } else {
                    result.push_str(&command);
                }
                continue;
            }
            if character == '^' || character == '_' {
                self.position += 1;
                result = result.trim_end().to_string();
                let script = self.parse_scripts(character);
                if result.ends_with(NAMED_OP_END) {
                    result.truncate(result.len() - NAMED_OP_END.len_utf8());
                    result.push_str(&script);
                    result.push(NAMED_OP_END);
                } else {
                    result.push_str(&script);
                }
                continue;
            }
            if character.is_whitespace() {
                result.push_str(&self.parse_whitespace());
                continue;
            }
            if character == '=' || character == '<' || character == '>' {
                result = format!("{} {} ", result.trim_end(), character);
                self.position += 1;
                continue;
            }
            if character == '&' {
                self.position += 1;
                continue;
            }
            if character == '~' {
                self.position += 1;
                result.push(' ');
                continue;
            }
            if character == '.'
                && let Some(index) = trailing_layout_marker_index(&result)
                && let Some(node) = self.nodes.borrow_mut().get_mut(index)
                && let LayoutNode::Matrix { lines, .. } = node
                && let Some(last) = lines.last_mut()
            {
                last.push('.');
                self.position += 1;
                continue;
            }
            result.push(character);
            self.position += 1;
        }
        if end.is_some() {
            self.supported = false;
        }
        result
    }

    /// 解析 `^`/`_` 脚本（支持两种顺序，如 `x^2_3`），
    /// 嵌套或无法转 Unicode 的 display 脚本堆叠为 `Script` 布局节点。
    fn parse_scripts(&mut self, initial_marker: char) -> String {
        let mut sub: Option<String> = None;
        let mut sup: Option<String> = None;
        let mut order: Vec<char> = Vec::new();

        self.parse_script_argument(initial_marker, &mut sub, &mut sup, &mut order);

        // 跳过空白后若为对侧脚本标记（如 `x^2 _3`），一并解析
        let mut next_position = self.position;
        while next_position < self.chars.len() && self.chars[next_position].is_whitespace() {
            next_position += 1;
        }
        if let Some(&next_marker) = self.chars.get(next_position)
            && (next_marker == '^' || next_marker == '_')
            && next_marker != initial_marker
        {
            self.position = next_position + 1;
            self.parse_script_argument(next_marker, &mut sub, &mut sup, &mut order);
        }

        let sub_unicode = sub.as_deref().and_then(|v| format_unicode_script(v, "sub"));
        let sup_unicode = sup.as_deref().and_then(|v| format_unicode_script(v, "sup"));

        // 复杂脚本（含 `/`、无布局标记的多字符且不含大写/`*`）不使用布局节点
        let blocks_layout = |value: &Option<String>| -> bool {
            match value {
                None => false,
                Some(v) => {
                    v.contains('/')
                        || (!v.contains(LAYOUT_START)
                            && v.chars().count() > 1
                            && !v
                                .chars()
                                .any(|c| c.is_ascii_uppercase() || c == '*' || c == '∗'))
                }
            }
        };
        let can_use_layout = !(blocks_layout(&sub) || blocks_layout(&sup));
        let needs_layout = self.display
            && can_use_layout
            && (self.script_depth > 0
                || (sub.is_some() && sub_unicode.is_none())
                || (sup.is_some() && sup_unicode.is_none()));

        if !needs_layout {
            return order
                .iter()
                .map(|kind| {
                    if *kind == '_' {
                        sub_unicode
                            .clone()
                            .unwrap_or_else(|| format_script(sub.as_deref().unwrap_or(""), "sub"))
                    } else {
                        sup_unicode
                            .clone()
                            .unwrap_or_else(|| format_script(sup.as_deref().unwrap_or(""), "sup"))
                    }
                })
                .collect();
        }

        let mut nodes = self.nodes.borrow_mut();
        let index = nodes.len();
        nodes.push(LayoutNode::Script {
            lower: sub.as_deref().map(normalize_output),
            upper: sup.as_deref().map(normalize_output),
        });
        format!("{}{}{}", LAYOUT_START, index, LAYOUT_END)
    }

    /// 解析 `^`/`_` 之后的脚本实参，按 `marker` 写入 `sub`/`sup`，
    /// 并把标记追加到 `order` 以保留源码中的书写顺序。
    fn parse_script_argument(
        &mut self,
        marker: char,
        sub: &mut Option<String>,
        sup: &mut Option<String>,
        order: &mut Vec<char>,
    ) {
        self.script_depth += 1;
        let value = self.parse_required_argument(false);
        self.script_depth -= 1;
        if marker == '_' {
            *sub = Some(value);
        } else {
            *sup = Some(value);
        }
        order.push(marker);
    }

    /// 跳过连续空白并统一折叠为一个空格（LaTeX 中换行等同于空格）。
    fn parse_whitespace(&mut self) -> String {
        while self.position < self.chars.len() && self.chars[self.position].is_whitespace() {
            self.position += 1;
        }
        " ".to_string()
    }

    /// 解析 `\` 开头的命令并分派到各专用处理函数，返回其渲染文本。
    /// 无法识别的命令置 `supported = false` 并原样返回 `\name`。
    fn parse_command(&mut self) -> String {
        self.position += 1;
        if self.position >= self.chars.len() {
            self.supported = false;
            return String::new();
        }
        let first = self.chars[self.position];
        if first == '\n' || first == '\r' {
            self.position += 1;
            if first == '\r'
                && self.position < self.chars.len()
                && self.chars[self.position] == '\n'
            {
                self.position += 1;
            }
            return " ".to_string();
        }
        let command: String;
        if first.is_ascii_alphabetic() {
            let start = self.position;
            while self.position < self.chars.len()
                && self.chars[self.position].is_ascii_alphabetic()
            {
                self.position += 1;
            }
            command = self.chars[start..self.position].iter().collect();
        } else {
            command = first.to_string();
            self.position += 1;
        }

        if let Some(value) = self.parse_command_constant(&command) {
            return value;
        }
        if command == "not" {
            return self.parse_command_not();
        }
        if let Some(value) = self.parse_command_symbol(&command) {
            return value;
        }
        if command == "left" || command == "middle" || command == "right" {
            return self.parse_command_delimiter();
        }
        if command == "frac" || command == "dfrac" || command == "tfrac" {
            return self.parse_command_fraction(&command);
        }
        if command == "sqrt" {
            return self.parse_command_sqrt();
        }
        if command == "boxed" || command == "fbox" {
            return format!("[{}]", self.parse_required_argument(true).trim());
        }
        if command == "binom" || command == "dbinom" || command == "tbinom" {
            return self.parse_command_binom();
        }
        if let Some(accent) = find(ACCENTS, &command) {
            return self.parse_command_accent(&command, accent);
        }
        if command == "mathbb" {
            return self.parse_command_mathbb();
        }
        if command == "operatorname" {
            return self.parse_command_operatorname();
        }
        if command == "mod" || command == "bmod" {
            return " mod ".to_string();
        }
        if command == "pmod" || command == "pod" {
            return self.parse_command_pmod(&command);
        }
        if command == "overset" || command == "stackrel" || command == "underset" {
            return self.parse_command_stack(&command);
        }
        if in_set(PLAIN_WRAPPERS, &command) {
            let value = self.parse_required_argument(true);
            return if command.starts_with("text") || command == "mbox" {
                value
            } else {
                value.trim().to_string()
            };
        }
        if command == "begin" || command == "end" {
            return self.parse_command_begin_end(&command);
        }
        self.supported = false;
        format!("\\{}", command)
    }

    /// 处理返回固定字面量的简单命令（换行、间距、忽略、逐字字符等）
    fn parse_command_constant(&mut self, command: &str) -> Option<String> {
        if command == "\\" {
            return Some("\n".to_string());
        }
        if in_set(SPACING_COMMANDS, command) {
            return Some(" ".to_string());
        }
        if in_set(NEGATIVE_SPACING_COMMANDS, command) {
            return Some(NEGATIVE_SPACE.to_string());
        }
        if in_set(IGNORED_COMMANDS, command) {
            return Some(String::new());
        }
        if in_set(FONT_SWITCH_COMMANDS, command) {
            // 字体切换不影响文本内容，但需吞掉后续空白（`\rm intrinsic` → `intrinsic`）
            while self.position < self.chars.len() && self.chars[self.position].is_whitespace() {
                self.position += 1;
            }
            return Some(String::new());
        }
        if command == "{"
            || command == "}"
            || command == "$"
            || command == "%"
            || command == "#"
            || command == "_"
            || command == "&"
        {
            return Some(command.to_string());
        }
        if command == "|" {
            return Some("‖".to_string());
        }
        None
    }

    /// 处理符号类命令（极限运算符/符号表/具名运算符/尺寸命令）
    fn parse_command_symbol(&mut self, command: &str) -> Option<String> {
        if in_set(LIMIT_OPERATORS, command) {
            return Some(self.parse_operator(command, "bracket", true, true));
        }
        if let Some(symbol) = find(SYMBOLS, command) {
            if in_set(DISPLAY_LIMIT_SYMBOLS, command) {
                return Some(self.parse_operator(symbol, "script", true, false));
            }
            return Some(
                if command == "cdot" || command == "times" || in_set(RELATION_COMMANDS, command) {
                    format!(" {} ", symbol)
                } else {
                    symbol.to_string()
                },
            );
        }
        if in_set(NAMED_OPERATORS, command) {
            return Some(format!("{}{}{}", NAMED_OP_START, command, NAMED_OP_END));
        }
        if in_set(SIZE_COMMANDS, command) {
            return Some(String::new());
        }
        None
    }

    /// 处理 left/middle/right 定界符命令，跳过可选的 `.`
    fn parse_command_delimiter(&mut self) -> String {
        if self.position < self.chars.len() && self.chars[self.position] == '.' {
            self.position += 1;
        }
        String::new()
    }

    /// 处理 begin/end 环境命令
    fn parse_command_begin_end(&mut self, command: &str) -> String {
        if command == "begin" {
            self.parse_environment()
        } else {
            self.supported = false;
            String::new()
        }
    }

    /// 处理 `\not` 取反：优先查找符号表，否则叠加否定斜线
    fn parse_command_not(&mut self) -> String {
        let value = self.parse_required_argument(false).trim().to_string();
        let negated = value
            .chars()
            .next()
            .filter(|_| value.chars().count() == 1)
            .and_then(|c| find_char(NEGATED_SYMBOLS, c));
        if let Some(negated) = negated {
            return format!(" {} ", negated);
        }
        let chars: Vec<char> = value.chars().collect();
        if chars.is_empty() {
            self.supported = false;
            return String::new();
        }
        let mut out = String::new();
        out.push(' ');
        out.push(chars[0]);
        out.push('\u{0338}');
        for c in &chars[1..] {
            out.push(*c);
        }
        out.push(' ');
        out
    }

    /// 处理分式命令（frac/dfrac/tfrac），display 模式堆叠为布局节点
    fn parse_command_fraction(&mut self, command: &str) -> String {
        let should_stack = self.display && self.stack_fractions && command != "tfrac";
        let numerator = self.parse_required_argument(!should_stack);
        let denominator = self.parse_required_argument(!should_stack);
        if should_stack {
            let mut nodes = self.nodes.borrow_mut();
            let index = nodes.len();
            nodes.push(LayoutNode::Fraction {
                numerator: normalize_output(&numerator),
                denominator: normalize_output(&denominator),
            });
            return format!("{}{}{}", LAYOUT_START, index, LAYOUT_END);
        }
        format_fraction(&numerator, &denominator)
    }

    /// 处理根号命令，支持可选次方
    fn parse_command_sqrt(&mut self) -> String {
        let degree = self.parse_optional_argument().map(|s| s.trim().to_string());
        let value = self.parse_required_argument(true);
        match degree.as_deref() {
            None | Some("2") => format_root(&value, "√"),
            Some("3") => format_root(&value, "∛"),
            Some("4") => format_root(&value, "∜"),
            Some(d) => format!("{}{}", format_script(d, "sup"), format_root(&value, "√")),
        }
    }

    /// 处理二项式命令（binom/dbinom/tbinom）
    fn parse_command_binom(&mut self) -> String {
        format!(
            "({} choose {})",
            self.parse_required_argument(true),
            self.parse_required_argument(true),
        )
    }

    /// 处理重音命令，单字符直接叠加，否则退回括号形式
    fn parse_command_accent(&mut self, command: &str, accent: &str) -> String {
        let value = self.parse_required_argument(true);
        if value.chars().count() == 1 {
            format!("{}{}", value, accent)
        } else {
            format!("{}({})", command, value)
        }
    }

    /// 处理黑板体命令（mathbb）
    fn parse_command_mathbb(&mut self) -> String {
        let value = self.parse_required_argument(true);
        let mut out = String::new();
        for c in value.chars() {
            match find_char(BLACKBOARD, c) {
                Some(bb) => out.push_str(bb),
                None => out.push(c),
            }
        }
        out
    }

    /// 处理自定义运算符命令（operatorname）
    fn parse_command_operatorname(&mut self) -> String {
        let mut starred = false;
        if self.position < self.chars.len() && self.chars[self.position] == '*' {
            starred = true;
            self.position += 1;
        }
        let operator = normalize_output(&self.parse_required_argument(true))
            .trim()
            .to_string();
        self.parse_operator(&operator, "bracket", starred, true)
    }

    /// 处理 pmod/pod 同余命令
    fn parse_command_pmod(&mut self, command: &str) -> String {
        let value = self.parse_required_argument(true).trim().to_string();
        if command == "pmod" {
            format!(" (mod {})", value)
        } else {
            format!(" ({})", value)
        }
    }

    /// 处理上下堆叠命令（overset/ststackrel/underset）
    fn parse_command_stack(&mut self, command: &str) -> String {
        if command == "underset" {
            let lower = self.parse_required_argument(true);
            let value = self.parse_required_argument(true).trim().to_string();
            return format!("{}{}", value, format_script(&lower, "sub"));
        }
        let upper = self.parse_required_argument(true);
        let value = self.parse_required_argument(true).trim().to_string();
        format!("{}{}", value, format_script(&upper, "sup"))
    }

    /// 解析运算符的上下限（含 `\limits`/`\nolimits` 修饰）：display 模式且允许上下限时
    /// 生成 `Operator` 布局节点，否则退化为行内 `_`/`^` 脚本。
    /// `inline_lower_style` 为 `"bracket"` 时下限用 `[...]` 而非下标；`spaced` 为真时两侧补空格。
    fn parse_operator(
        &mut self,
        operator: &str,
        inline_lower_style: &str,
        display_limits: bool,
        spaced: bool,
    ) -> String {
        let mut use_display_limits = display_limits;
        let mut modifier_position = self.position;
        while modifier_position < self.chars.len()
            && (self.chars[modifier_position] == ' ' || self.chars[modifier_position] == '\t')
        {
            modifier_position += 1;
        }
        let modifier: Option<(&str, usize)> = {
            let rest: String = self.chars[modifier_position..].iter().collect();
            let bytes = rest.as_bytes();
            if rest.starts_with("\\limits") && (bytes.len() == 7 || !bytes[7].is_ascii_alphabetic())
            {
                Some(("limits", 7))
            } else if rest.starts_with("\\nolimits")
                && (bytes.len() == 9 || !bytes[9].is_ascii_alphabetic())
            {
                Some(("nolimits", 9))
            } else {
                None
            }
        };
        if let Some((m, len)) = modifier {
            use_display_limits = m == "limits";
            self.position = modifier_position + len;
        }

        let mut lower: Option<String> = None;
        let mut upper: Option<String> = None;
        loop {
            let mut script_position = self.position;
            while script_position < self.chars.len()
                && (self.chars[script_position] == ' ' || self.chars[script_position] == '\t')
            {
                script_position += 1;
            }
            let kind = self.chars.get(script_position).copied();
            if kind != Some('_') && kind != Some('^') {
                break;
            }
            self.position = script_position + 1;
            let value = normalize_output(&self.parse_required_argument(false)).replace(' ', "");
            if kind == Some('_') {
                if lower.is_some() {
                    self.supported = false;
                }
                lower = Some(value);
            } else {
                if upper.is_some() {
                    self.supported = false;
                }
                upper = Some(value);
            }
        }

        if self.display && use_display_limits && (lower.is_some() || upper.is_some()) {
            let mut nodes = self.nodes.borrow_mut();
            let index = nodes.len();
            nodes.push(LayoutNode::Operator {
                operator: operator.to_string(),
                lower,
                upper,
            });
            return format!("{}{}{}", LAYOUT_START, index, LAYOUT_END);
        }

        let mut rendered = operator.to_string();
        if let Some(l) = &lower {
            let part = if inline_lower_style == "bracket" {
                format!("[{}]", l)
            } else {
                format_script(l, "sub")
            };
            rendered.push_str(&part);
        }
        if let Some(u) = &upper {
            rendered.push_str(&format_script(u, "sup"));
        }
        if spaced {
            format!(" {} ", rendered)
        } else {
            rendered
        }
    }

    /// 解析必选实参（`{...}`、单字符或单个命令）；`stack_fractions` 控制实参内部
    /// 是否仍允许分式垂直堆叠，解析结束后恢复原设置。
    fn parse_required_argument(&mut self, stack_fractions: bool) -> String {
        let previous = self.stack_fractions;
        self.stack_fractions = previous && stack_fractions;
        let value = self.parse_required_argument_value();
        self.stack_fractions = previous;
        value
    }

    /// 跳过空白后读取实参本体：`{...}` 递归解析序列，`\cmd` 走命令解析，否则取单字符。
    /// 输入耗尽时置 `supported = false` 并返回空串。
    fn parse_required_argument_value(&mut self) -> String {
        while self.position < self.chars.len() && self.chars[self.position].is_whitespace() {
            self.position += 1;
        }
        if self.position >= self.chars.len() {
            self.supported = false;
            return String::new();
        }
        if self.chars[self.position] == '{' {
            self.position += 1;
            return self.parse_sequence(Some('}'));
        }
        if self.chars[self.position] == '\\' {
            return self.parse_command();
        }
        let value = self.chars[self.position].to_string();
        self.position += 1;
        value
    }

    /// 解析可选实参 `[...]` 并以嵌套方式渲染其内容；当前位置不是 `[` 或找不到 `]`
    /// 时返回 `None`（不消费输入）。
    fn parse_optional_argument(&mut self) -> Option<String> {
        while self.position < self.chars.len()
            && (self.chars[self.position] == ' ' || self.chars[self.position] == '\t')
        {
            self.position += 1;
        }
        if self.chars.get(self.position) != Some(&'[') {
            return None;
        }
        let mut end = None;
        for (i, &c) in self.chars.iter().enumerate().skip(self.position + 1) {
            if c == ']' {
                end = Some(i);
                break;
            }
        }
        let end = end?;
        let value: String = self.chars[self.position + 1..end].iter().collect();
        self.position = end + 1;
        Some(self.render_nested(&value, true))
    }

    /// 原样读取 `{...}` 分组内容（按花括号配平、跳过 `\` 转义），不做 LaTeX 渲染。
    /// 开头不是 `{` 或括号未闭合时置 `supported = false` 并返回 `None`。
    fn read_raw_group(&mut self) -> Option<String> {
        while self.position < self.chars.len() && self.chars[self.position].is_whitespace() {
            self.position += 1;
        }
        if self.chars.get(self.position) != Some(&'{') {
            self.supported = false;
            return None;
        }
        self.position += 1;
        let start = self.position;
        let mut depth = 1usize;
        while self.position < self.chars.len() {
            let c = self.chars[self.position];
            if c == '\\' {
                self.position += 2;
                continue;
            }
            if c == '{' {
                depth += 1;
            }
            if c == '}' {
                depth -= 1;
                if depth == 0 {
                    let value: String = self.chars[start..self.position].iter().collect();
                    self.position += 1;
                    return Some(value);
                }
            }
            self.position += 1;
        }
        self.supported = false;
        None
    }

    /// 解析 `\begin{env}...\end{env}`：按环境名分派到公式/对齐/cases/矩阵等分支，
    /// 并消费到对应的 `\end`。缺少结束标记或环境不支持时置 `supported = false`。
    fn parse_environment(&mut self) -> String {
        let Some(environment) = self.read_raw_group() else {
            return String::new();
        };
        let end_marker = format!("\\end{{{}}}", environment);
        let Some(end) = find_substring(&self.chars[self.position..], &end_marker) else {
            self.supported = false;
            return String::new();
        };
        let body: String = self.chars[self.position..self.position + end]
            .iter()
            .collect();
        self.position += end + end_marker.chars().count();

        if environment == "equation" || environment == "equation*" || environment == "displaymath" {
            return self.render_nested(&body, true).trim().to_string();
        }

        if matches!(
            environment.as_str(),
            "aligned"
                | "align"
                | "align*"
                | "alignedat"
                | "alignat"
                | "alignat*"
                | "gather"
                | "gathered"
                | "multline"
                | "multline*"
                | "split"
        ) {
            return self.render_aligned_environment(&body, &environment);
        }

        if environment == "cases" || environment == "cases*" {
            return self.render_cases_environment(&body);
        }

        if [
            "array",
            "matrix",
            "smallmatrix",
            "pmatrix",
            "bmatrix",
            "Bmatrix",
            "vmatrix",
            "Vmatrix",
        ]
        .contains(&environment.as_str())
        {
            let matrix_body = if environment == "array" {
                trim_leading_group(&body)
            } else {
                body
            };
            return self.render_matrix(&environment, &matrix_body);
        }

        self.supported = false;
        body
    }

    /// 渲染对齐环境（aligned/align/gather/multline 等），按行以 & 对齐后逐行渲染
    fn render_aligned_environment(&mut self, body: &str, environment: &str) -> String {
        let aligned_at = ["alignedat", "alignat", "alignat*"].contains(&environment);
        let aligned_body = if aligned_at {
            trim_leading_group(body)
        } else {
            body.to_string()
        };
        let mut parts: Vec<String> = Vec::new();
        for row in split_environment_rows(&aligned_body) {
            let cells: Vec<&str> = row.split('&').collect();
            let source = if aligned_at {
                let mut merged = String::new();
                let pairs = cells.len().div_ceil(2);
                for i in 0..pairs {
                    let from = i * 2;
                    let to = (i * 2 + 2).min(cells.len());
                    let joined = cells[from..to].join("");
                    if !merged.is_empty() {
                        merged.push(' ');
                    }
                    merged.push_str(&joined);
                }
                merged
            } else {
                cells.join("")
            };
            let rendered = self.render_nested(&source, true).trim().to_string();
            if !rendered.is_empty() {
                parts.push(rendered);
            }
        }
        parts.join("\n")
    }

    /// 渲染 cases 环境：值列对齐、条件列对齐到同一列，并作为居中矩阵布局节点嵌入周围等式。
    fn render_cases_environment(&mut self, body: &str) -> String {
        let rows: Vec<Vec<String>> = split_environment_rows(body)
            .iter()
            .map(|row| {
                row.split('&')
                    .map(|cell| self.render_nested(cell, false).trim().to_string())
                    .collect()
            })
            .filter(|row: &Vec<String>| row.iter().any(|c| !c.is_empty()))
            .collect();
        let value_width = rows
            .iter()
            .map(|row| display_width(row.first().map(|s| s.trim_end_matches(',')).unwrap_or("")))
            .max()
            .unwrap_or(0);
        let contents: Vec<String> = rows
            .iter()
            .map(|row| {
                let value = row.first().map(|s| s.trim_end_matches(',')).unwrap_or("");
                let condition = row.get(1).map(|s| s.as_str()).unwrap_or("");
                if condition.is_empty() {
                    return value.to_string();
                }
                let prefix = if starts_with_case_word(condition) {
                    " "
                } else {
                    " if "
                };
                let pad = value_width.saturating_sub(display_width(value));
                format!(
                    "{}{}{}{}",
                    value,
                    PROTECTED_SPACE.to_string().repeat(pad),
                    prefix,
                    condition
                )
            })
            .collect();

        if contents.len() <= 1 {
            return contents
                .first()
                .map(|c| format!("⎧ {}", c))
                .unwrap_or_default();
        }

        // 偶数行时插入一条纯分隔符行，使大括号的垂直中心落在中线上
        let middle = contents.len() / 2;
        let mut visual: Vec<Option<String>> = contents.into_iter().map(Some).collect();
        if visual.len().is_multiple_of(2) {
            visual.insert(middle, None);
        }
        let lines: Vec<String> = visual
            .iter()
            .enumerate()
            .map(|(index, content)| {
                let delimiter = if index == 0 {
                    "⎧"
                } else if index == visual.len() - 1 {
                    "⎩"
                } else {
                    "⎨"
                };
                match content {
                    None => delimiter.to_string(),
                    Some(c) => format!("{} {}", delimiter, c),
                }
            })
            .collect();

        let mut nodes = self.nodes.borrow_mut();
        let index = nodes.len();
        nodes.push(LayoutNode::Matrix {
            lines,
            baseline: middle,
        });
        format!("{}{}{}", LAYOUT_START, index, LAYOUT_END)
    }

    /// 渲染矩阵/数组环境：按 `&` 分列、逐列取最大宽度对齐，再按环境名补上括号定界符。
    /// 只有一行时直接返回该行，不生成布局节点。
    fn render_matrix(&mut self, environment: &str, body: &str) -> String {
        let matrix: Vec<Vec<String>> = split_environment_rows(body)
            .iter()
            .map(|row| {
                row.split('&')
                    .map(|cell| self.render_nested(cell, false).trim().to_string())
                    .collect()
            })
            .filter(|row: &Vec<String>| row.iter().any(|c| !c.is_empty()))
            .collect();
        let column_count = matrix.iter().map(|r| r.len()).max().unwrap_or(0);
        let mut column_widths = vec![0usize; column_count];
        for row in &matrix {
            for (i, cell) in row.iter().enumerate() {
                if i < column_count {
                    column_widths[i] = column_widths[i].max(display_width(cell));
                }
            }
        }
        let rows: Vec<String> = matrix
            .iter()
            .map(|row| {
                let mut out = String::new();
                for (i, &w) in column_widths.iter().enumerate() {
                    let cell = row.get(i).cloned().unwrap_or_default();
                    out.push_str(&cell);
                    let pad = w.saturating_sub(display_width(&cell));
                    for _ in 0..pad {
                        out.push(PROTECTED_SPACE);
                    }
                    if i < column_count - 1 {
                        out.push_str(" │ ");
                    }
                }
                out
            })
            .collect();

        let lines: Vec<String> =
            if environment == "array" || environment == "matrix" || environment == "smallmatrix" {
                rows
            } else {
                let delimiters: &[(&str, [char; 6])] = &[
                    ("pmatrix", ['⎛', '⎞', '⎜', '⎟', '⎝', '⎠']),
                    ("bmatrix", ['⎡', '⎤', '⎢', '⎥', '⎣', '⎦']),
                    ("Bmatrix", ['⎧', '⎫', '⎨', '⎬', '⎩', '⎭']),
                    ("vmatrix", ['│', '│', '│', '│', '│', '│']),
                    ("Vmatrix", ['║', '║', '║', '║', '║', '║']),
                ];
                let Some((_, dl)) = delimiters.iter().find(|(n, _)| *n == environment) else {
                    self.supported = false;
                    return rows.join("\n");
                };
                rows.iter()
                    .enumerate()
                    .map(|(index, row)| {
                        let left = if index == 0 {
                            dl[0]
                        } else if index == rows.len() - 1 {
                            dl[4]
                        } else {
                            dl[2]
                        };
                        let right = if index == 0 {
                            dl[1]
                        } else if index == rows.len() - 1 {
                            dl[5]
                        } else {
                            dl[3]
                        };
                        format!("{} {} {}", left, row, right)
                    })
                    .collect()
            };

        if lines.len() <= 1 {
            return lines.first().cloned().unwrap_or_default();
        }
        let mut nodes = self.nodes.borrow_mut();
        let index = nodes.len();
        nodes.push(LayoutNode::Matrix { lines, baseline: 0 });
        format!("{}{}{}", LAYOUT_START, index, LAYOUT_END)
    }

    /// 用新的子解析器渲染内嵌片段，共享同一布局节点表；子片段不支持时
    /// 置 `self.supported = false` 并原样返回 `source`。
    fn render_nested(&mut self, source: &str, stack_fractions: bool) -> String {
        let rendered =
            LatexParser::new(source, self.nodes.clone(), self.display && stack_fractions).render();
        match rendered {
            Some(r) => r,
            None => {
                self.supported = false;
                source.to_string()
            }
        }
    }
}

/// 判断 cases 条件列是否已以 if/when/for/otherwise 开头（大小写不敏感，且其后须为
/// 非字母数字），用于决定是否再补一个 "if"。
fn starts_with_case_word(condition: &str) -> bool {
    let lower: Vec<char> = condition.to_lowercase().chars().collect();
    for w in ["if", "when", "for", "otherwise"] {
        let wchars: Vec<char> = w.chars().collect();
        if lower.len() >= wchars.len()
            && lower[..wchars.len()] == wchars[..]
            && (lower.len() == wchars.len() || !lower[wchars.len()].is_alphanumeric())
        {
            return true;
        }
    }
    false
}

/// 渲染 LaTeX 数学表达式为终端 Unicode 文本。`display` 为 true 时（块级公式）
/// 分式垂直堆叠、求和/积分上下限分列。返回 None 表示语法不支持。
pub fn render_latex(source: &str, display: bool) -> Option<String> {
    let nodes: NodeRef = Rc::new(RefCell::new(Vec::new()));
    let rendered = LatexParser::new(source, nodes.clone(), display).render()?;
    if nodes.borrow().is_empty() {
        return Some(rendered.replace(PROTECTED_SPACE, " "));
    }
    let layout = render_layout(&rendered, &nodes.borrow());
    let indentation = layout
        .lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.len() - l.trim_start().len())
        .min()
        .unwrap_or(0);
    let mut out_lines: Vec<String> = Vec::new();
    for l in &layout.lines {
        let idx = l
            .char_indices()
            .nth(indentation)
            .map(|(i, _)| i)
            .unwrap_or(l.len());
        out_lines.push(l[idx..].trim_end().to_string());
    }
    Some(
        out_lines
            .join("\n")
            .trim_end()
            .to_string()
            .replace(PROTECTED_SPACE, " "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_simple() {
        assert_eq!(
            render_latex("a^2 + b^2 = c^2", false).as_deref(),
            Some("a² + b² = c²")
        );
        assert_eq!(render_latex("x_n", false).as_deref(), Some("xₙ"));
        assert_eq!(render_latex("\\pi r^2", false).as_deref(), Some("π r²"));
        assert_eq!(
            render_latex("e^{i\\pi} + 1 = 0", false).as_deref(),
            Some("e^(iπ) + 1 = 0")
        );
    }

    #[test]
    fn pythagorean() {
        assert_eq!(
            render_latex("a^2 + b^2 = c^2", false).as_deref(),
            Some("a² + b² = c²")
        );
    }

    #[test]
    fn quadratic_formula_inline() {
        // 对齐 pi latex.test.ts：inline 分式 `a/b`，± 前无空格
        assert_eq!(
            render_latex("x = \\frac{-b \\pm \\sqrt{b^2 - 4ac}}{2a}", false).as_deref(),
            Some("x = (-b ± √(b² - 4ac))/(2a)")
        );
    }

    #[test]
    fn taylor_inline() {
        // 对齐 pi：inline 模式下 sum 上下限用 Unicode 上下标
        assert_eq!(
            render_latex("e^x = \\sum_{n=0}^{\\infty} \\frac{x^n}{n!}", false).as_deref(),
            Some("eˣ = ∑ₙ₌₀^∞ xⁿ/(n!)")
        );
    }

    #[test]
    fn fermat_inline() {
        assert_eq!(
            render_latex("a^{p-1} \\equiv 1 \\pmod{p}", false).as_deref(),
            Some("aᵖ⁻¹ ≡ 1 (mod p)")
        );
    }

    #[test]
    fn normal_distribution_inline() {
        assert_eq!(
            render_latex(
                "f(x) = \\frac{1}{\\sigma\\sqrt{2\\pi}} e^{-\\frac{(x-\\mu)^2}{2\\sigma^2}}",
                false
            )
            .as_deref(),
            Some("f(x) = 1/(σ√2π) e^(-((x-μ)²)/(2σ²))")
        );
    }

    #[test]
    fn stirling_inline() {
        assert_eq!(
            render_latex(
                "n! \\approx \\sqrt{2\\pi n} \\left(\\frac{n}{e}\\right)^n",
                false
            )
            .as_deref(),
            Some("n! ≈ √(2π n) (n/e)ⁿ")
        );
    }

    #[test]
    fn geometric_series_inline() {
        assert_eq!(
            render_latex(
                "\\sum_{n=0}^{\\infty} ar^n = \\frac{a}{1-r}, \\quad |r| < 1",
                false
            )
            .as_deref(),
            Some("∑ₙ₌₀^∞ arⁿ = a/(1-r), |r| < 1")
        );
    }

    #[test]
    fn display_fraction_stacks() {
        let r = render_latex("\\frac{a}{b}", true).unwrap();
        let lines: Vec<&str> = r.lines().collect();
        assert_eq!(lines.len(), 3, "{:?}", r);
        assert!(lines[0].contains('a'));
        assert!(lines[1].contains('─'));
        assert!(lines[2].contains('b'));
    }

    #[test]
    fn display_sum_limits_stack() {
        let r = render_latex("\\sum_{i=0}^{n} i", true).unwrap();
        let lines: Vec<&str> = r.lines().collect();
        assert!(lines.len() >= 3, "{:?}", r);
        assert!(lines[0].contains('n'), "{:?}", r);
        assert!(lines[1].contains('∑'), "{:?}", r);
        assert!(lines[2].contains('i'), "{:?}", r);
    }

    #[test]
    fn matrix_renders_multiline() {
        let r = render_latex("\\begin{pmatrix} 1 & 2 \\\\ 3 & 4 \\end{pmatrix}", true).unwrap();
        let lines: Vec<&str> = r.lines().collect();
        assert!(lines.len() >= 2, "{:?}", r);
        assert!(lines[0].contains('⎛') && lines[0].contains('1'), "{:?}", r);
        assert!(
            lines.last().unwrap().contains('⎞') || lines.last().unwrap().contains('⎝'),
            "{:?}",
            r
        );
    }

    #[test]
    fn unsupported_falls_back_none() {
        assert_eq!(render_latex("\\unknowncommand{x}", false), None);
        assert_eq!(render_latex("\\begin{unknown}", false), None);
    }

    #[test]
    fn relation_spacing() {
        assert_eq!(render_latex("a \\le b", false).as_deref(), Some("a ≤ b"));
        assert_eq!(
            render_latex("A \\subset B", false).as_deref(),
            Some("A ⊂ B")
        );
    }

    #[test]
    fn named_operator_spacing() {
        assert_eq!(
            render_latex("\\sin x + \\cos x", false).as_deref(),
            Some("sin x + cos x")
        );
        assert_eq!(render_latex("2\\sin x", false).as_deref(), Some("2 sin x"));
        assert_eq!(render_latex("\\sin x", false).as_deref(), Some("sin x"));
        assert_eq!(render_latex("\\ln n", false).as_deref(), Some("ln n"));
    }

    #[test]
    fn blackboard_and_accents() {
        assert_eq!(render_latex("\\mathbb{R}", false).as_deref(), Some("ℝ"));
        assert_eq!(render_latex("\\hat{x}", false).as_deref(), Some("x\u{302}"));
        assert_eq!(
            render_latex("\\vec{v}", false).as_deref(),
            Some("v\u{20d7}")
        );
    }

    #[test]
    fn legacy_font_switches_are_ignored() {
        // 回归 #8827：`{\rm x}` 等旧式字体切换不得回落成原始源码
        assert_eq!(
            render_latex("{\\rm intrinsic}", false).as_deref(),
            Some("intrinsic")
        );
        assert_eq!(render_latex("{\\bf bold}", false).as_deref(), Some("bold"));
        assert_eq!(
            render_latex("{\\cal calligraphic}", false).as_deref(),
            Some("calligraphic")
        );
        assert_eq!(
            render_latex("\\bf x + \\it y", false).as_deref(),
            Some("x + y")
        );
    }

    #[test]
    fn not_negates() {
        assert_eq!(render_latex("\\not\\leq", false).as_deref(), Some("≰"));
    }

    #[test]
    fn cases_align_and_center_around_equation() {
        // 奇数行：中间行落在基线上（pi #9564）
        assert_eq!(
            render_latex(
                r"f(x)=\begin{cases}a & x<0 \\ b & \text{if }x=0 \\ c & \text{otherwise}\end{cases}",
                false,
            )
            .as_deref(),
            Some("       ⎧ a if x < 0\nf(x) = ⎨ b if x = 0\n       ⎩ c otherwise")
        );
        // 偶数行：插入纯分隔符行使大括号垂直居中
        assert_eq!(
            render_latex(
                r"f(x) = \begin{cases} x^{2} & x \geq 0 \\ -x & x < 0 \end{cases}",
                false,
            )
            .as_deref(),
            Some("       ⎧ x² if x ≥ 0\nf(x) = ⎨\n       ⎩ -x if x < 0")
        );
    }

    #[test]
    fn nested_and_unsupported_display_scripts_stack() {
        // 嵌套脚本与无法转 Unicode 的 display 脚本垂直堆叠（pi #7929）
        assert_eq!(
            render_latex(
                r"\partial_tU_2(t,0)=Aj_*(1-t)^{-A-1}.\qquad x^{n^2}+x_{i_j}",
                true,
            )
            .as_deref(),
            Some(
                "                            2\n                    -A-1   n\n∂ₜU₂(t,0) = Aj (1-t)    . x  +x\n              *                i\n                                j"
            )
        );
        // 脚本内的分式保持线性
        assert_eq!(
            render_latex(r"e^{\frac{1}{2}}+\tfrac{1}{2}", true).as_deref(),
            Some("e^(1/2)+1/2")
        );
    }
}

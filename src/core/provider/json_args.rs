//! 工具调用参数（模型给出的 JSON 文本）的解析与修复。
//!
//! 模型给出的参数串经常不严格：字符串里带 `\usepackage` 这类非法转义、原始控制字符、或者流式传输到一半被截断。
//! 直接交给 `serde_json` 会整串失败，于是只能退回「最长可解析前缀」——字段与它后面的内容会被静默丢掉。
//!
//! 这里分两条路：
//! - [`parse_streaming_json`]：流式期间用，宽容（修复 → 前缀兜底 → 空对象）；
//! - [`parse_tool_call_arguments`]：调用已定稿时用，严格——绝不接受部分前缀，
//!   失败时返回错误说明（含模型发来的原文），由校验阶段回给模型。

use serde_json::{Value, json};

/// JSON 字符串字面量里的合法转义字符。
///
/// 刻意**不含** `u`：`\u` 只有后接 4 位十六进制才算合法，`\usepackage` 之类按字面文本处理。
const VALID_ESCAPES: [char; 8] = ['"', '\\', '/', 'b', 'f', 'n', 'r', 't'];

/// 增量解析产出一次快照的最小新增字符数。
///
/// 快照要克隆整棵参数树，逐 delta 克隆在大参数上同样是 O(n²)，按实参展示的人眼精度
/// 每 4 KB 更新一次完全够用。
const STREAMING_SNAPSHOT_STEP: usize = 4 * 1024;

/// 增量解析失败后，回退到「整串重解析」的最小新增长度。
///
/// 回退路径每步都是全量扫描，步长太小会退化成 O(n²)（90 KB 的参数能吃掉上百 MB 堆）。
const STREAMING_FALLBACK_STEP: usize = 8 * 1024;

/// 把字符串里的一个原始控制字符转义成合法 JSON 转义。
fn escape_control(c: char) -> String {
    match c {
        '\u{08}' => "\\b".to_string(),
        '\u{0c}' => "\\f".to_string(),
        '\n' => "\\n".to_string(),
        '\r' => "\\r".to_string(),
        '\t' => "\\t".to_string(),
        other => format!("\\u{:04x}", other as u32),
    }
}

/// 修复模型给出的畸形 JSON 字符串字面量：
/// - 字符串内的原始控制字符转义成 `\n` / `\uXXXX` 等；
/// - 非法转义（含后接不足 4 位十六进制的 `\u`）前补一个反斜杠，使其按字面文本保留。
///
/// 字符串之外的内容原样保留。拿不到任何需要修复的地方时返回值与输入相同。
pub(crate) fn repair_json(json: &str) -> String {
    let chars: Vec<char> = json.chars().collect();
    let mut out = String::with_capacity(json.len());
    let mut in_string = false;
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i];

        if !in_string {
            out.push(c);
            if c == '"' {
                in_string = true;
            }
            i += 1;
            continue;
        }

        if c == '"' {
            out.push(c);
            in_string = false;
            i += 1;
            continue;
        }

        if c == '\\' {
            let Some(&next) = chars.get(i + 1) else {
                // 结尾单个反斜杠：补成合法的 `\\`
                out.push_str("\\\\");
                i += 1;
                continue;
            };
            if next == 'u' {
                // 流式截断：剩不到 4 位就无法判定，整段丢掉（pi partial-json 同款）
                if chars.len() - (i + 2) < 4 {
                    break;
                }
                let digits: String = chars[i + 2..i + 6].iter().collect();
                if digits.chars().all(|d| d.is_ascii_hexdigit()) {
                    out.push_str("\\u");
                    out.push_str(&digits);
                    i += 6;
                    continue;
                }
            }
            if VALID_ESCAPES.contains(&next) {
                out.push('\\');
                out.push(next);
                i += 2;
                continue;
            }
            out.push_str("\\\\");
            i += 1;
            continue;
        }

        if (c as u32) <= 0x1f {
            out.push_str(&escape_control(c));
        } else {
            out.push(c);
        }
        i += 1;
    }

    out
}

/// 先直接解析，失败时修复转义再解析；两次都失败返回最后一次的错误文案。
fn parse_json_with_repair(json: &str) -> Result<Value, String> {
    match serde_json::from_str::<Value>(json) {
        Ok(v) => Ok(v),
        Err(err) => {
            let repaired = repair_json(json);
            if repaired != json
                && let Ok(v) = serde_json::from_str::<Value>(&repaired)
            {
                return Ok(v);
            }
            Err(err.to_string())
        }
    }
}

/// 从后往前逐步截断，返回最长的可完整解析 JSON 前缀；没有任何前缀能解析时为 `None`。
fn partial_parse(s: &str) -> Option<Value> {
    let chars: Vec<char> = s.chars().collect();
    for cut in (1..=chars.len()).rev() {
        let candidate: String = chars[..cut].iter().collect();
        if let Ok(v) = serde_json::from_str::<Value>(&candidate) {
            return Some(v);
        }
    }
    None
}

/// 尽量补全不完整的 JSON（流式中途）：先结束未闭合的字符串，再补上缺失的 `}` / `]`。
///
/// 补出来的文本还要能解析才算数（比如 `{"a":` 补成 `{"a":}` 仍非法）——调用方解析失败
/// 就退回空对象。
fn close_incomplete_json(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    let mut closers: Vec<char> = Vec::new();
    let mut in_string = false;
    let mut escaped = false;

    for c in s.chars() {
        out.push(c);
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => closers.push('}'),
            '[' => closers.push(']'),
            '}' | ']' => {
                closers.pop();
            }
            _ => {}
        }
    }

    if in_string {
        // 结尾是孤立的反斜杠时先去掉，避免补出非法转义
        if escaped {
            out.pop();
        }
        out.push('"');
    }
    while let Some(closer) = closers.pop() {
        out.push(closer);
    }
    out
}

/// 流式工具参数的增量解析器：原文缓冲 + 实时参数快照。
///
/// 正常 JSON 走 [`IncrementalJson`] 状态机，每个 delta 只扫描新增字符；
/// 内容无法增量处理（例如顶层不是对象/数组）时退化为按步长整串重解析，
/// 保证展示不会卡死在 O(n²) 上。定稿仍由 [`parse_tool_call_arguments`] 严格解析。
#[derive(Debug, Clone)]
pub(crate) struct StreamingArgs {
    /// 累积的参数原文。
    text: String,
    /// 增量解析状态机。
    parser: IncrementalJson,
    /// 当前可展示的参数快照。
    value: Value,
    /// 回退模式下上一次整串解析时的原文长度。
    fallback_len: usize,
}

impl Default for StreamingArgs {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingArgs {
    /// 建一个空参数的解析器（模型还没给出任何参数）。
    pub(crate) fn new() -> Self {
        Self {
            text: String::new(),
            parser: IncrementalJson::new(),
            value: json!({}),
            fallback_len: 0,
        }
    }

    /// 追加一段参数增量，返回新的参数快照（可能比实际进度滞后不到一个快照步长）。
    pub(crate) fn push(&mut self, delta: &str) -> Value {
        self.text.push_str(delta);
        if self.parser.broken() {
            // 小参数直接每次都解析（与增量路径同等规模），大参数按步长收敛
            let step = if self.text.len() < STREAMING_FALLBACK_STEP {
                0
            } else {
                STREAMING_FALLBACK_STEP
            };
            if self.text.len() - self.fallback_len >= step {
                self.fallback_len = self.text.len();
                self.value = parse_streaming_json(&self.text);
            }
        } else {
            // 参数还小的时候每轮都出快照（成本可忽略），跨过步长后才按步长收敛
            let force = self.text.len() < STREAMING_SNAPSHOT_STEP;
            if let Some(snapshot) = self.parser.feed(delta, force) {
                self.value = snapshot;
            }
        }
        self.value.clone()
    }

    /// 用完整原文重置（`arguments.done`、grammar 包装这类整串覆盖），返回快照。
    pub(crate) fn reset(&mut self, full: &str) -> Value {
        self.text.clear();
        self.text.push_str(full);
        self.parser = IncrementalJson::new();
        self.fallback_len = self.text.len();
        self.value = match self.parser.feed(full, true) {
            Some(snapshot) => snapshot,
            None => parse_streaming_json(full),
        };
        self.value.clone()
    }

    /// 建一个已经喂过 `text` 的解析器（`function_call` 首事件里带的初始参数）。
    pub(crate) fn with_text(text: &str) -> Self {
        let mut args = Self::new();
        args.reset(text);
        args
    }

    /// 直接给定参数值（`content_block_start` 里已经是对象的 `input`）。
    pub(crate) fn set_value(&mut self, value: Value) {
        self.text.clear();
        self.parser = IncrementalJson::new();
        self.fallback_len = 0;
        self.value = value;
    }

    /// 已累积的参数原文（定稿时交给 [`parse_tool_call_arguments`]）。
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
}

/// 增量 JSON 解析器的写入位置：对象键或数组下标。
#[derive(Debug, Clone)]
enum PathStep {
    /// 对象成员，值为键名。
    Key(String),
    /// 数组元素，值为下标。
    Index(usize),
}

/// 增量 JSON 解析器的状态。
#[derive(Debug, Clone, Copy, PartialEq)]
enum JState {
    /// 期待一个值（文档开头、冒号后、数组元素前）。
    Value,
    /// 对象内：期待键或 `}`。
    Key,
    /// 键已读，期待 `:`。
    Colon,
    /// 值已读完，期待 `,` 或闭合。
    AfterValue,
    /// 字符串内，`key` 为真表示在读对象键。
    Str {
        /// 是否正在读取对象键。
        key: bool,
    },
    /// 数字 / `true` / `false` / `null` 等标量内。
    Scalar,
    /// 顶层值已结束，后续字符忽略。
    Done,
    /// 无法增量处理：此后由 [`StreamingArgs`] 回退到整串重解析。
    Broken,
}

/// 栈式增量 JSON 状态机：只消费新增字符，维护「当前已可用的参数树」。
///
/// 只追求流式展示够用：不校验语义、遇到无法处理的输入直接 `Broken`，
/// 由调用方回退到宽容的整串解析，因此这里任何路径都不 panic。
#[derive(Debug, Clone)]
struct IncrementalJson {
    /// 当前状态。
    state: JState,
    /// 参数树根。
    root: Value,
    /// 从根到当前写入槽位的路径。
    path: Vec<PathStep>,
    /// 容器栈：`o` 对象、`a` 数组，与 `path` 一一对应。
    stack: Vec<char>,
    /// 当前对象待写的键（键字符串读完后落在这里）。
    pending_key: Option<String>,
    /// 字符串 / 标量的累积缓冲。
    buf: String,
    /// 字符串内刚读到反斜杠。
    escaped: bool,
    /// 未收尾的 `\uXXXX` 已收到的十六进制字符。
    unicode: Option<String>,
    /// 距上次产出快照新增的字符数。
    since_snapshot: usize,
}

impl IncrementalJson {
    /// 建一个空状态机。
    fn new() -> Self {
        Self {
            state: JState::Value,
            root: Value::Null,
            path: Vec::new(),
            stack: Vec::new(),
            pending_key: None,
            buf: String::new(),
            escaped: false,
            unicode: None,
            since_snapshot: 0,
        }
    }

    /// 是否已放弃增量解析。
    fn broken(&self) -> bool {
        self.state == JState::Broken
    }

    /// 消费一段增量；`force` 为真时必定产出快照。
    ///
    /// 返回 `None` 表示这一轮没有新快照（未到步长）或已 [`broken`](Self::broken)。
    fn feed(&mut self, delta: &str, force: bool) -> Option<Value> {
        for c in delta.chars() {
            self.since_snapshot += 1;
            self.step(c);
            if self.state == JState::Broken {
                return None;
            }
        }
        if force || self.since_snapshot >= STREAMING_SNAPSHOT_STEP {
            self.since_snapshot = 0;
            return Some(self.snapshot());
        }
        None
    }

    /// 当前可展示的参数快照（含未读完的字符串 / 标量）。
    fn snapshot(&mut self) -> Value {
        match self.state {
            JState::Str { key: false } => {
                let text = self.buf.clone();
                self.set_here(Value::String(text));
            }
            JState::Scalar => {
                let value = scalar_value(&self.buf);
                self.set_here(value);
            }
            _ => {}
        }
        self.root.clone()
    }

    /// 处理一个字符。
    fn step(&mut self, c: char) {
        match self.state {
            JState::Value => self.begin_value(c),
            JState::Key => match c {
                '}' => self.close_container('o'),
                '"' => {
                    self.buf.clear();
                    self.unicode = None;
                    self.state = JState::Str { key: true };
                }
                _ if c.is_whitespace() => {}
                _ => self.state = JState::Broken,
            },
            JState::Colon => match c {
                ':' => self.state = JState::Value,
                _ if c.is_whitespace() => {}
                _ => self.state = JState::Broken,
            },
            JState::AfterValue => match c {
                ',' => {
                    self.state = if self.stack.last() == Some(&'o') {
                        JState::Key
                    } else {
                        JState::Value
                    }
                }
                '}' => self.close_container('o'),
                ']' => self.close_container('a'),
                _ if c.is_whitespace() => {}
                _ => self.state = JState::Broken,
            },
            JState::Str { key } => self.string_char(c, key),
            JState::Scalar => match c {
                ',' | '}' | ']' => {
                    self.finish_scalar();
                    self.step(c);
                }
                _ if c.is_whitespace() => self.finish_scalar(),
                _ => self.buf.push(c),
            },
            JState::Done | JState::Broken => {}
        }
    }

    /// 开始一个值：开槽位（容器还会压栈），或进入字符串 / 标量累积。
    fn begin_value(&mut self, c: char) {
        if c.is_whitespace() {
            return;
        }
        match c {
            '{' | '[' => {
                self.open_slot();
                let kind = if c == '{' { 'o' } else { 'a' };
                self.set_here(if kind == 'o' { json!({}) } else { json!([]) });
                self.stack.push(kind);
                self.state = if kind == 'o' {
                    JState::Key
                } else {
                    JState::Value
                };
            }
            '"' => {
                self.open_slot();
                self.buf.clear();
                self.unicode = None;
                self.state = JState::Str { key: false };
            }
            // 标量只可能是数字或 `true` / `false` / `null`，其它起始字符直接放弃增量解析
            '-' | '0'..='9' | 't' | 'f' | 'n' => {
                self.open_slot();
                self.buf.clear();
                self.buf.push(c);
                self.state = JState::Scalar;
            }
            _ => self.state = JState::Broken,
        }
    }

    /// 字符串内的字符：转义、`\uXXXX` 与闭合。
    fn string_char(&mut self, c: char, key: bool) {
        if let Some(hex) = self.unicode.as_mut() {
            if c.is_ascii_hexdigit() {
                hex.push(c);
                if hex.len() == 4 {
                    let digits = self.unicode.take().unwrap_or_default();
                    match u32::from_str_radix(&digits, 16)
                        .ok()
                        .and_then(char::from_u32)
                    {
                        Some(decoded) => self.buf.push(decoded),
                        // 非法码点按字面保留（与 repair_json 的处理一致）
                        None => {
                            self.buf.push_str("\\u");
                            self.buf.push_str(&digits);
                        }
                    }
                }
                return;
            }
            // `\u` 后不是十六进制：整段按字面保留，当前字符重新按普通字符处理
            let digits = self.unicode.take().unwrap_or_default();
            self.buf.push_str("\\u");
            self.buf.push_str(&digits);
            self.string_char(c, key);
            return;
        }

        if self.escaped {
            self.escaped = false;
            match c {
                '"' => self.buf.push('"'),
                '\\' => self.buf.push('\\'),
                '/' => self.buf.push('/'),
                'b' => self.buf.push('\u{8}'),
                'f' => self.buf.push('\u{c}'),
                'n' => self.buf.push('\n'),
                'r' => self.buf.push('\r'),
                't' => self.buf.push('\t'),
                'u' => self.unicode = Some(String::new()),
                // 非法转义按字面保留：`\usepackage` 这类内容在写文件的工具参数里很常见
                other => {
                    self.buf.push('\\');
                    self.buf.push(other);
                }
            }
            return;
        }

        match c {
            '\\' => self.escaped = true,
            '"' if key => {
                self.pending_key = Some(std::mem::take(&mut self.buf));
                self.state = JState::Colon;
            }
            '"' => self.finish_string(),
            _ => self.buf.push(c),
        }
    }

    /// 字符串值读完：写进槽位并回到「期待逗号或闭合」。
    fn finish_string(&mut self) {
        let text = std::mem::take(&mut self.buf);
        self.set_here(Value::String(text));
        self.path.pop();
        self.state = JState::AfterValue;
    }

    /// 标量读完：按 JSON 标量写入槽位（解析不了写 `null`）。
    fn finish_scalar(&mut self) {
        let raw = std::mem::take(&mut self.buf);
        self.set_here(scalar_value(&raw));
        self.path.pop();
        self.state = JState::AfterValue;
    }

    /// 闭合当前容器；类型不匹配即放弃增量解析。
    fn close_container(&mut self, kind: char) {
        if self.stack.last() != Some(&kind) {
            self.state = JState::Broken;
            return;
        }
        self.stack.pop();
        if self.stack.is_empty() {
            self.path.clear();
            self.state = JState::Done;
        } else {
            self.path.pop();
            self.state = JState::AfterValue;
        }
    }

    /// 在当前位置占一个槽位并把 `path` 指向它（对象按 `pending_key` 插入，数组追加元素）。
    fn open_slot(&mut self) {
        if self.stack.is_empty() {
            self.root = Value::Null;
            return;
        }
        if self.stack.last() == Some(&'o') {
            let key = self.pending_key.take().unwrap_or_default();
            if let Some(map) = value_at_mut(&mut self.root, &self.path).as_object_mut() {
                map.insert(key.clone(), Value::Null);
            }
            self.path.push(PathStep::Key(key));
        } else {
            let index = match value_at_mut(&mut self.root, &self.path).as_array_mut() {
                Some(list) => {
                    list.push(Value::Null);
                    list.len() - 1
                }
                None => 0,
            };
            self.path.push(PathStep::Index(index));
        }
    }

    /// 把值写到当前槽位。
    fn set_here(&mut self, value: Value) {
        *value_at_mut(&mut self.root, &self.path) = value;
    }
}

/// 标量原文转 JSON 值：数字 / `true` / `false` / `null`，其余（含流式中的半截数字）当 `null`。
fn scalar_value(raw: &str) -> Value {
    serde_json::from_str::<Value>(raw.trim()).unwrap_or(Value::Null)
}

/// 沿路径取可变引用；路径与树不一致时停在最近的可用节点（不会 panic）。
fn value_at_mut<'v>(cur: &'v mut Value, path: &[PathStep]) -> &'v mut Value {
    let Some((step, rest)) = path.split_first() else {
        return cur;
    };
    match step {
        PathStep::Key(key) => {
            if cur.get(key.as_str()).is_none() {
                return cur;
            }
            value_at_mut(cur.get_mut(key.as_str()).expect("键已确认存在"), rest)
        }
        PathStep::Index(index) => {
            if cur.get(*index).is_none() {
                return cur;
            }
            value_at_mut(cur.get_mut(*index).expect("下标已确认存在"), rest)
        }
    }
}

/// 流式（可能不完整）JSON 解析：修复后解析 → 最长可解析前缀 → 补全未闭合结构 → 空对象。
///
/// 任何输入都返回一个 JSON 值（对象），调用方无需处理错误。
pub(crate) fn parse_streaming_json(s: &str) -> Value {
    if s.trim().is_empty() {
        return json!({});
    }
    if let Ok(v) = parse_json_with_repair(s) {
        return v;
    }
    if let Some(v) = partial_parse(s) {
        return v;
    }
    let repaired = repair_json(s);
    if let Some(v) = partial_parse(&repaired) {
        return v;
    }
    if let Ok(v) = serde_json::from_str::<Value>(&close_incomplete_json(&repaired)) {
        return v;
    }
    json!({})
}

/// 已定稿的工具调用参数解析：**绝不**保留部分解析结果。
///
/// 返回 `(arguments, arguments_error)`：
/// - 解析成功且是对象 → `(对象, None)`；
/// - 参数为 `None` / 空串 → `({}, None)`（模型没给参数）；
/// - 非法 JSON，或结果不是对象（数组 / `null` / 标量）→ `({}, Some(说明))`，
///   说明里带上模型发来的原文，供校验阶段回给模型。
pub(crate) fn parse_tool_call_arguments(json: Option<&str>) -> (Value, Option<String>) {
    let Some(json) = json else {
        return (json!({}), None);
    };

    if json.trim().is_empty() {
        return (json!({}), None);
    }

    match parse_json_with_repair(json) {
        Ok(v @ Value::Object(_)) => (v, None),
        Ok(_) => (
            json!({}),
            Some(format!(
                "Tool arguments must be a JSON object\n\nReceived arguments:\n{json}"
            )),
        ),
        Err(err) => (
            json!({}),
            Some(format!("{err}\n\nReceived arguments:\n{json}")),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 非法 `\u` 转义按字面保留，字段不再被丢掉（pi #10868）。
    #[test]
    fn repairs_invalid_unicode_escapes_without_dropping_fields() {
        assert_eq!(
            parse_streaming_json(r#"{"text":"\url{x}"}"#),
            json!({ "text": "\\url{x}" })
        );
        assert_eq!(
            parse_streaming_json(r#"{"path":"a.tex","content":"\usepackage{x}","mode":"w"}"#),
            json!({ "path": "a.tex", "content": "\\usepackage{x}", "mode": "w" })
        );
        // 合法的 \u 转义仍然解码
        assert_eq!(
            parse_streaming_json(r#"{"text":"caf\u00e9 \url"}"#),
            json!({ "text": "café \\url" })
        );
    }

    /// 流式中途被截断的 `\u` 转义被丢弃，前面的字段仍然拿得到（对齐 pi partial-json）。
    #[test]
    fn truncated_unicode_escape_keeps_earlier_fields() {
        assert_eq!(
            parse_streaming_json(r#"{"text":"ab\u00"#),
            json!({ "text": "ab" })
        );
        assert_eq!(
            parse_streaming_json(r#"{"text":"ok","tail":"ab\u00"#),
            json!({ "text": "ok", "tail": "ab" })
        );
    }

    /// 原始控制字符被转义，而不是让整串解析失败。
    #[test]
    fn escapes_raw_control_characters() {
        assert_eq!(
            parse_streaming_json("{\"text\":\"a\nb\"}"),
            json!({ "text": "a\nb" })
        );
    }

    /// 定稿参数：非法 JSON → 空对象 + 错误说明（含原文），不再吞成部分前缀。
    #[test]
    fn finalized_arguments_never_keep_a_partial_prefix() {
        let (args, err) = parse_tool_call_arguments(Some(r#"{"path":"a.rs","limit":}"#));
        assert_eq!(args, json!({}));
        let err = err.expect("malformed arguments must report an error");
        assert!(err.contains("Received arguments:"), "{err}");
        assert!(err.contains(r#""path":"a.rs""#), "{err}");
    }

    /// 定稿参数：结果不是对象时同样报错（数组 / null / 标量）。
    #[test]
    fn finalized_arguments_require_an_object() {
        for raw in ["[1,2]", "null", "42", r#""text""#] {
            let (args, err) = parse_tool_call_arguments(Some(raw));
            assert_eq!(args, json!({}), "{raw}");
            assert!(err.is_some(), "{raw}");
        }
        // 非法 `\u` 修复后仍是合法对象 → 不报错
        let (args, err) = parse_tool_call_arguments(Some(r#"{"content":"\usepackage{x}"}"#));
        assert_eq!(err, None);
        assert_eq!(args, json!({ "content": "\\usepackage{x}" }));
        // 没给参数 → 空对象、无错误
        assert_eq!(parse_tool_call_arguments(None), (json!({}), None));
        assert_eq!(parse_tool_call_arguments(Some("  ")), (json!({}), None));
    }

    /// 增量解析：逐 delta 喂入，快照与最后整串解析一致，且未到快照步长时沿用上次结果。
    #[test]
    fn streaming_args_matches_full_parse_across_deltas() {
        let text = r#"{"path":"a.rs","content":"caf\u00e9 \url{x}","mode":"w","n":-12,"ok":true,"tags":["a","b"],"nested":{"k":null}}"#;
        let mut args = StreamingArgs::new();
        let mut snapshot = json!({});
        for chunk in text.as_bytes().chunks(7) {
            snapshot = args.push(std::str::from_utf8(chunk).expect("测试文本按字节切成 UTF-8"));
        }
        assert_eq!(snapshot, parse_streaming_json(text));
        assert_eq!(args.text(), text);
        assert_eq!(
            parse_tool_call_arguments(Some(args.text())).0,
            parse_streaming_json(text)
        );
    }

    /// 未到快照步长的中间 delta 不再重解析：快照保持上一次的值（不是空对象）。
    #[test]
    fn streaming_args_keeps_the_last_snapshot_between_steps() {
        let mut args = StreamingArgs::new();
        let first = args.push(r#"{"path":"a.rs"}"#);
        assert_eq!(first, json!({ "path": "a.rs" }));
        // 只要还没跨过步长，快照就不变
        let second = args.push(r#","#);
        assert_eq!(second, json!({ "path": "a.rs" }));
    }

    /// 超大参数不再逐 delta 全量重解析：喂 200 KB 的单字段只有增量扫描。
    #[test]
    fn streaming_args_handle_large_arguments_incrementally() {
        let body = "x".repeat(200 * 1024);
        let mut args = StreamingArgs::new();
        args.push(r#"{"content":""#);
        let mut snapshot = json!({});
        for chunk in body.as_bytes().chunks(4096) {
            snapshot = args.push(std::str::from_utf8(chunk).expect("ASCII 切片总是合法 UTF-8"));
        }
        // 快照最多落后一个步长，最终定稿仍是完整内容
        let partial = snapshot["content"].as_str().unwrap_or("");
        assert!(body.starts_with(partial), "快照应是前缀而非空");
        assert!(body.len() - partial.len() <= STREAMING_SNAPSHOT_STEP);
        snapshot = args.push(r#""}"#);
        assert_eq!(snapshot["content"].as_str().unwrap_or(""), body);
    }

    /// 增量状态机处理不了的输入退回整串重解析（顶层是数组、尾随垃圾等）。
    #[test]
    fn streaming_args_fall_back_when_the_stream_is_not_plain_json() {
        let mut args = StreamingArgs::new();
        args.push("[1,2");
        let snapshot = args.push("]");
        assert_eq!(snapshot, json!([1, 2]));

        // 无法解析的内容不 panic，最差是空对象
        let mut broken = StreamingArgs::new();
        broken.push("<<<");
        assert_eq!(broken.push(">>>"), json!({}));
    }
}

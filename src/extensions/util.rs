use crate::{
    core::{
        extensions::{
            self, DockSpan, ExtensionSetting, ExtensionUiRequest, RichSpan, UiNotifyLevel,
        },
        provider::AgentMessage,
        tools::ToolError,
    },
    modes::interactive::app::MsgLevel,
    utils::{
        glyphs::{DEF_SPINNER, DEF_SPINNER_BRAILLE},
        time::now_ms,
    },
};
use serde_json::Value;
use std::{
    path::{PathBuf, absolute},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// 动画 tick 间隔（毫秒）：spinner 每 120ms 走一帧
pub const SPINNER_TICK_MS: u64 = 120;

/// 默认 UA：接近浏览器，避免被简单 UA 拦截
pub fn default_user_agent() -> String {
    "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36 prux-web-access/1.0"
        .to_string()
}

/// 代表一个工作区的目录名。
///
/// 镜像自身会话日志用的编码（`<agent-dir>/sessions/--Users-me-work-repo--/<timestamp>_<id>.jsonl`），
/// 因此一个工作区的任务文件与它的转写文件同名，肉眼可查。
pub fn project_key(cwd: &str) -> String {
    let resolved = absolute(cwd).unwrap_or_else(|_| PathBuf::from(cwd));
    let s = resolved.to_string_lossy();
    let stripped = s.trim_start_matches(['/', '\\']);
    let encoded: String = stripped
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == ':' {
                '-'
            } else {
                c
            }
        })
        .collect();
    format!("--{encoded}--")
}

/// 通用一次性 id：`{毫秒时间戳}-{十六进制尾巴}`。
///
/// 面向「能直接进文件名 / 日志 / 协议字段」的场景（如 `chatcmpl-…`、目标 id），
/// 长度不限、人眼可读、按时间大致有序。熵源只有 `亚秒纳秒 ^ pid`，没有计数器，
/// 时钟粒度粗的平台上同一进程连续两次调用**理论上可能**取到同一个值；
/// 需要更强唯一性时用 [`generate_id`]，需要短且可手敲的 id 用 [`next_id`]。
pub fn make_id() -> String {
    let rand: u64 = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64);

    format!("{}-{:x}", now_ms(), rand)
}

/// 定长 id：16 位十六进制；`时间戳 ^ 计数器*常量`（splitmix 风格混合）。
///
/// 三次中唯一性最强的一个：计数器保证同进程内**永不重复**（即便时钟不动），
/// 定长无分隔符便于做外部系统的 key / responseId。代价是比 [`next_id`] 长一倍。
pub fn generate_id() -> String {
    /// 进程内自增计数器，保证定长 id 即便时钟不动也不重复。
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let mixed = nanos ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    format!("{mixed:016x}")
}

/// 短 id：8 位十六进制（32 bit 随机）。
///
/// 面向「要频繁展示、还会被模型和用户手敲」的场景（`resume <id>`、mention、`/agents` 面板），
/// 故刻意短。随机源不可用时退化为 `时间戳 ^ 单调计数器`，同进程内仍不重复。
pub fn next_id() -> String {
    /// 随机源不可用时的退化计数器，与时间戳异或生成短 id。
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut b = [0u8; 4];
    if getrandom::fill(&mut b).is_err() {
        let n = SEQ.fetch_add(1, Ordering::Relaxed) ^ now_ms();
        b.copy_from_slice(&(n as u32).to_be_bytes());
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// token 数量的紧凑展示：<1000 原样，否则 `1.2K` / `3.4M`（1 位小数）。
pub fn format_tokens(value: u64) -> String {
    format_tokens_precise::<1>(value)
}

/// 指定小数位数的 token 格式化（`format_tokens` 默认 1 位小数）。
/// 函数 const 泛型不能带默认值，故通过该函数显式指定。
pub fn format_tokens_precise<const PRECISION: u8>(value: u64) -> String {
    if value >= 1_000_000 {
        format!(
            "{:.p$}M",
            value as f64 / 1_000_000.0,
            p = PRECISION as usize
        )
    } else if value >= 1_000 {
        format!("{:.p$}K", value as f64 / 1_000.0, p = PRECISION as usize)
    } else {
        value.to_string()
    }
}

/// 把（通常形如 `1`、`12` 的）id 解析成数字用于排序；无法解析时返回 [`f64::NAN`]，
/// 交给调用方的 `partial_cmp(...).unwrap_or(...)` 决定回退顺序。
pub fn numeric_id(id: &str) -> f64 {
    id.parse::<f64>().unwrap_or(f64::NAN)
}

/// 目标文本压成单行（空白折叠为一个空格）并按字符数截断，超出时以 `…` 收尾。
pub fn truncate_objective(objective: &str, max: usize) -> String {
    let single = objective.split_whitespace().collect::<Vec<_>>().join(" ");
    if single.chars().count() > max {
        let truncated: String = single.chars().take(max.saturating_sub(1)).collect();
        format!("{}…", truncated)
    } else {
        single
    }
}

/// 构造一个「循环选择」型扩展设置项：面板里按 Space/Enter 在 `choices` 间循环。
///
/// `value` 是当前值（应出现在 `choices` 中）。各扩展的面板表（tasks / subagent /
/// loop-detect / hang-detect）都用它逐项构造，不再各自维护一份中间规格表。
pub fn cycle_setting(
    key: &str,
    label: &str,
    description: &str,
    value: String,
    choices: &[&str],
) -> ExtensionSetting {
    ExtensionSetting {
        key: key.to_string(),
        label: label.to_string(),
        description: description.to_string(),
        value,
        choices: choices.iter().map(|c| c.to_string()).collect(),
    }
}

/// 面板档位表里「面板标签 → 内部数值」的反查，用于配对 [`cycle_setting`] 构造的选择项。
///
/// 表即各扩展的 `(内部数值, 面板标签)` 档位表；标签匹配不上时报错（含原始标签，便于定位面板文案改动）。
pub fn choice_num(table: &[(f64, &str)], value: &str) -> Result<f64, String> {
    table
        .iter()
        .find(|(_, l)| *l == value)
        .map(|(v, _)| *v)
        .ok_or_else(|| format!("invalid option: {value}"))
}

/// 主题色 dock span：`key` 查主题表，查不到时用 `fallback`（十六进制或颜色名）。
///
/// dock 行里九成 span 都是这个形状，所以放在 util 里共享，免得每个扩展都写一遍 [`DockSpan::new`] 的结构体字面量。
pub fn dock_span(key: &'static str, fallback: &'static str, text: impl Into<String>) -> DockSpan {
    DockSpan::new(key, fallback, text)
}

/// 无主题色的 dock span（终端默认前景）。
pub fn dock_plain(text: impl Into<String>) -> DockSpan {
    DockSpan::plain(text)
}

/// 遮蔽展示机密值（API key / token）：首尾各留 [`MASK_HEAD`] / [`MASK_TAIL`] 个字符，
/// 中间固定替换成 [`MASK_STARS`] 个 `*`（如 `sk-p*****mnop`）。
///
/// 只按字符切（不切坏 UTF-8）。空串返回空串，由调用方决定展示 `(none)` 之类；
/// 短于 `MASK_HEAD + MASK_TAIL + MASK_MIN_HIDDEN`（即 12）个字符则**整体打码**：
/// 首尾留的比中间遮住的还多，那种「遮蔽」等于把密钥摆回屏幕上。
pub fn mask_secret(secret: &str) -> String {
    /// 遮蔽机密值时保留的首部字符数。
    const MASK_HEAD: usize = 4; // 首部保留的字符数
    /// 遮蔽机密值时保留的尾部字符数。
    const MASK_TAIL: usize = 4; // 尾部保留的字符数
    /// 遮蔽机密值时中间固定输出的星号个数。
    const MASK_STARS: usize = 5; // 中间固定的星号个数
    /// 至少需要遮住的字符数，不足则整体打码。
    const MASK_MIN_HIDDEN: usize = 4; // 至少遮住多少个字符（不足则整体打码）

    let chars: Vec<char> = secret.chars().collect();
    if chars.is_empty() {
        return String::new();
    }

    if chars.len() < MASK_HEAD + MASK_TAIL + MASK_MIN_HIDDEN {
        return "*".repeat(MASK_STARS);
    }

    let head: String = chars[..MASK_HEAD].iter().collect();
    let tail: String = chars[chars.len() - MASK_TAIL..].iter().collect();
    format!("{head}{}{tail}", "*".repeat(MASK_STARS))
}

/// 纯文本通知（`Notify` 变体）：不经富文本通道，收发双方都明确「这里只有一段文本」。
pub fn notify_plain(text: impl Into<String>, level: UiNotifyLevel) {
    extensions::request_ui(ExtensionUiRequest::Notify {
        text: text.into(),
        level,
    });
}

/// 富文本通知：把带样式的 span 交给 UI 通知通道（跨线程请求，由 UI 侧消费）。
pub fn notify(spans: Vec<RichSpan>, level: UiNotifyLevel) {
    extensions::request_ui(ExtensionUiRequest::NotifyRich { spans, level });
}

/// 纯文本通知：把 `text` 包成单个无样式 span 走 [`notify`]。
pub fn notify_text(text: &str, level: UiNotifyLevel) {
    notify(vec![RichSpan::plain(text.to_string())], level);
}

/// 命令输出通知：把聊天区消息级别映射成 UI 通知级别后发纯文本通知。
pub fn notify_cmd(text: &str, level: MsgLevel) {
    notify_text(
        text,
        match level {
            MsgLevel::Info => UiNotifyLevel::Info,
            MsgLevel::Warning => UiNotifyLevel::Warning,
            MsgLevel::Error => UiNotifyLevel::Error,
            MsgLevel::Success => UiNotifyLevel::Success,
        },
    );
}

/// 往消息列表尾部追加一条 user 文本消息（作为注入的上下文）。
pub fn push_context(messages: &mut Vec<AgentMessage>, text: &str) {
    messages.push(AgentMessage::user_text(text));
}

/// 选择面板选项串的 key：约定为 `<key>  <展示文本>`，key 即首个空白分隔 token。
///
/// 面板回传的是整条展示串，所以分发只看 key——展示文案与字形怎么改都不会让
/// `on_ui_choice` 的匹配失效（`/agents`、`/download`、`/tasks` 共用这一约定）。
pub fn choice_key(choice: &str) -> &str {
    choice.split_whitespace().next().unwrap_or("")
}

/// 必填字符串参数。
pub fn req_str(args: &Value, key: &str) -> Result<String, ToolError> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .ok_or_else(|| ToolError(format!("missing required argument: {key}")))
}

/// 可选字符串参数（空串视为未提供）。
pub fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// 从 `v` 的 `key` 取字符串；缺键 / 非字符串回落空串。
pub fn str_field(v: Option<&Value>, key: &str) -> String {
    v.and_then(|obj| obj.get(key))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

pub fn value_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// 可选字符串数组参数：非数组视为未提供；元素中非字符串项与空串丢弃。
pub fn opt_string_array(args: &Value, key: &str) -> Option<Vec<String>> {
    let arr = args.get(key)?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
    )
}

/// 按字符数截断（不切坏 UTF-8），超出时追加省略号。
pub fn truncate_chars(s: &str, max: usize) -> String {
    truncate_chars_with_postfix(s, max, "…")
}

/// 按字符截断（不切坏 UTF-8），超出时在尾部标注。
pub fn truncate_chars_with_postfix(text: &str, max: usize, postfix: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max).collect();
    format!("{kept}{postfix}")
}

/// 按字符数硬截断（不切坏 UTF-8），超出部分直接丢弃、不追加省略号。
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }

    s.chars().take(max).collect()
}

/// 当前 spinner 帧号（墙钟驱动）：`now_ms / tick_ms` 对 `glyph_count` 取模。
///
/// 节奏是**调用方的策略**，所以 `tick_ms` 是参数而不是常量：subagent 用 120ms，
/// tasks dock 用 150ms（`SPINNER_TICK_MS`）。`glyph_count` 为 0（空字形集）或
/// `tick_ms` 为 0 时返回 0，不会除零 panic。
pub fn spinner_frame_index(glyph_count: usize, tick_ms: u64) -> usize {
    if glyph_count == 0 || tick_ms == 0 {
        return 0;
    }
    (now_ms() / tick_ms) as usize % glyph_count
}

/// 运行中 spinner 帧（按墙钟轮转；Braille 10 帧，120ms/帧）。
pub fn spinner_frame() -> &'static str {
    DEF_SPINNER_BRAILLE[spinner_frame_index(DEF_SPINNER_BRAILLE.len(), SPINNER_TICK_MS)]
}

/// 下载中的 spinner 帧（星形 spinner，120ms/帧）。
///
/// 与 tasks dock 同源（同一个 [`DEF_SPINNER`] 字形集），只是节奏各自定义：
/// 字形集里的帧号算法都走 [`spinner_frame_index`]。
pub fn download_spinner() -> &'static str {
    DEF_SPINNER[spinner_frame_index(DEF_SPINNER.len(), SPINNER_TICK_MS)]
}

/// 在 `cwd` 下执行一次 git 命令并取回 stdout。
///
/// 超过 `timeout` 会杀掉子进程并返回超时错误；进程无法启动或退出码非 0 时返回错误
/// （优先用 stderr 文本）。成功时返回去除首尾空白的 stdout。
pub async fn git(cwd: &str, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args).current_dir(cwd).kill_on_drop(true);

    let out = match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(format!("git {} could not run: {e}", args.join(" "))),
        Err(_) => {
            return Err(format!(
                "git {} timed out after {}s",
                args.join(" "),
                timeout.as_secs()
            ));
        }
    };

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("git {} failed ({})", args.join(" "), out.status)
        } else {
            stderr
        });
    }

    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// 类型名参数允许带 `@` 前缀（对齐 `@handle message` 提及语法），返回去掉前缀的展示串。
///
/// 仅用于错误提示：`/agents disable @foo` 报 `unknown agent type: foo`，而不是 `@foo`。
pub fn type_arg_label(name: &str) -> &str {
    let trimmed = name.trim();
    trimmed.strip_prefix('@').unwrap_or(trimmed)
}

/// 去掉前导 `v`/`V`（`v1.2.3` → `1.2.3`）。
pub fn normalize_version(tag: &str) -> String {
    tag.trim().trim_start_matches(['v', 'V']).to_string()
}

/// 逐段比较版本；`latest` 严格新于 `current` 才为真。
pub fn version_greater(latest: &str, current: &str) -> bool {
    parse_version(latest) > parse_version(current)
}

/// 版本号 → 数字段。每段取前导数字（`3-rc1` 这种预发布后缀只取 `3`），
/// 无法解析的段记 0；整体解析失败（如 `garbage`）得到 `[0]`，不会误判为更新。
pub fn parse_version(v: &str) -> Vec<u64> {
    normalize_version(v)
        .split('.')
        .map(|part| {
            let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
            digits.parse::<u64>().unwrap_or(0)
        })
        .collect()
}

/// 取两个数值中更严的一个；`0` 表示未知 / 不限，不参与取小。
pub fn tighter_u32(a: u32, b: u32) -> u32 {
    match (a, b) {
        (0, other) | (other, 0) => other,
        (a, b) => a.min(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spinner_frame_index_stays_in_range_and_survives_degenerate_input() {
        // 空字形集 / 零间隔：返回 0 而不是除零
        assert_eq!(spinner_frame_index(0, 120), 0);
        assert_eq!(spinner_frame_index(10, 0), 0);
        // 正常路径：帧号始终落在 [0, glyph_count)
        for _ in 0..50 {
            assert!(spinner_frame_index(10, 120) < 10);
            assert_eq!(spinner_frame_index(1, 1), 0);
        }
    }

    #[test]
    fn numeric_id_parses_numbers_and_returns_nan_for_garbage() {
        assert_eq!(numeric_id("12"), 12.0);
        assert_eq!(numeric_id("1.5"), 1.5);
        assert!(numeric_id("abc").is_nan());
        assert!(numeric_id("").is_nan());
    }

    #[test]
    fn project_key_matches_pi_encoding() {
        assert_eq!(project_key("/Users/me/work/repo"), "--Users-me-work-repo--");
    }

    #[test]
    fn opt_string_array_skips_non_strings_and_blanks() {
        assert_eq!(opt_string_array(&serde_json::json!({}), "ids"), None);
        assert_eq!(
            opt_string_array(&serde_json::json!({ "ids": 1 }), "ids"),
            None
        );
        assert_eq!(
            opt_string_array(
                &serde_json::json!({ "ids": [" 1 ", "", 2, null, "2"] }),
                "ids"
            ),
            Some(vec!["1".to_string(), "2".to_string()])
        );
    }

    #[test]
    fn mask_secret_keeps_head_tail_and_hides_middle() {
        // 典型密钥：首 4 + 5 个 `*` + 尾 4
        assert_eq!(mask_secret("sk-proj-abcdefghijklmnop"), "sk-p*****mnop");
        assert_eq!(mask_secret("abcdefghijkl"), "abcd*****ijkl");
        assert_eq!(mask_secret("abcdefghijklm"), "abcd*****jklm");
    }

    #[test]
    fn mask_secret_fully_masks_empty_and_short() {
        // 空串交回调用方
        assert_eq!(mask_secret(""), "");
        // 短值整体打码：不泄露任何字符（否则等于半明文）
        assert_eq!(mask_secret("short"), "*****");
        assert_eq!(mask_secret("12345678901"), "*****");
    }

    #[test]
    fn mask_secret_is_utf8_safe() {
        // 按字符切：不 panic、不切坏码位
        assert_eq!(
            mask_secret("密钥一二三四五六七八九十"),
            "密钥一二*****七八九十"
        );
        assert_eq!(mask_secret("密钥"), "*****");
    }

    #[test]
    fn tighter_ignores_zero() {
        assert_eq!(tighter_u32(100, 50), 50);
        assert_eq!(tighter_u32(0, 50), 50);
        assert_eq!(tighter_u32(50, 0), 50);
        assert_eq!(tighter_u32(0, 0), 0);
    }
}

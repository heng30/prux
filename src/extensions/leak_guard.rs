//! `leak-guard` 扩展：外发内容里的凭据与高熵串检测。
//!
//! | 时机 | 钩子 | 动作 |
//! |---|---|---|
//! | 每次请求前 | `transform_context` | 扫每条消息的 `Text` 块；`warn` 只提示，`block` 额外把命中打码 |
//! | 每次工具调用前 | `before_tool_call_ext` | 扫入参 JSON；`warn` 只提示，`block` 拒绝执行 |
//!
//! - **默认只 warn 不拦**：误报会打断正常排障（用户可能就是要传一个测试用假 token），
//!   拦截需在 `/extension` 面板的扩展设置里把 `Mode` 切到 `block`。
//! - **打码只改本次请求的副本**：`transform_context` 的改写不进会话历史，也不动磁盘。
//! - **不改 thinking 块**：思考块带供应商签名，改文本会让签名失效（请求直接报错），
//!   因此打码只作用于 `Text` 块。
//! - **去重**：同一类别 + 同一片段只告警一次（`transform_context` 每轮都会重扫历史消息），
//!   最多记 64 条，满了挤掉最早一条；这一点同时避免了刷屏。
//! - **提示带片段预览**：UI 文案里附上命中片段的首尾几个字符（`q8Zr2NvT…Zx4Q`），
//!   否则用户看不出是路径、base64 还是哈希触发的；预览只进 UI，不进模型上下文。
//!
//! 规则表覆盖 `sk-` / `sk-ant-` / `ghp_` / `AKIA` / `xox?-` / `AIza` / `glpat-` / `sk_live_` /
//! JWT / 私钥块，以及长度 ≥32、大小写与数字齐全、Shannon 熵 ≥4 bits/char 的高熵串；
//! 机器生成的标识（UUID、哈希、带 ISO 日期时间的会话文件名）与路径不算凭据。
//!
//! 配置持久化在 `agent_dir()/extensions/leak-guard.json`（`mode`：`warn` / `block` / `off`）：
//! `/leak-guard [warn|block|off]` 直接设档，裸 `/leak-guard` 打开扩展设置面板。
//! 默认不启用（`Dev` / `Creator`）。

use crate::{
    core::{
        config_cache::ConfigCache,
        extensions::{
            BeforeToolCallOutcome, CommandHost, Extension, ExtensionCommand, ExtensionHook,
            ExtensionMode, ExtensionSetting, ExtensionTool, ForkProjectInfo, SubcommandDef,
            UiNotifyLevel,
        },
        provider::{AgentMessage, ContentBlock},
        settings_manager::agent_dir,
        tools::ToolError,
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_LEAK_GUARD, command_arg,
        util::{cycle_setting, notify_plain},
    },
    modes::tui::handlers::register_host_command,
    utils::paths::{read_json_file, write_json_file},
};
use regex::Regex;
use serde_json::Value;
use std::{
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, LazyLock, Mutex, MutexGuard, RwLock},
};
use strum::{EnumString, IntoStaticStr};

/// 扩展名（也是 `/extension` 面板中的标识与配置文件前缀）。
const EXT: &str = "leak-guard";

/// 斜杠命令名（`/leak-guard`）。
const CMD: &str = EXT;

/// 配置文件相对 agent 目录的路径。
const CONFIG_REL: &str = "extensions/leak-guard.json";

/// 配置文件里的键：检测模式。
const CONFIG_MODE: &str = "mode";

/// 已告警记录的上限：满了就挤掉最早的一条，避免无界增长（整体清空会让同一批命中
/// 在下一轮重新算「新命中」，历史里噪声一多就会每轮重复告警）。
const MAX_REMEMBERED: usize = 64;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static LEAK_GUARD_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_LEAK_GUARD,
    make: || -> Arc<dyn Extension> { Arc::new(LeakGuard) },
};

/// 会话文件名里的 ISO 日期时间（`2026-11-02T10-25-54`，含文件名用的 `-` 分隔变体）。
static TIMESTAMP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}[:-]\d{2}").expect("datetime's format invalid")
});

/// 检测模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
enum Mode {
    /// 关闭：不扫描、不提示、不改写。
    Off,
    /// 默认档：只在 UI 上提示，请求与工具参数原样放行。
    Warn,
    /// 拦截档：请求前的命中被打码，工具调用的命中直接拒绝执行。
    Block,
}

impl Mode {
    /// 面板候选的展示顺序：warn → block → off。
    const CHOICES: [Mode; 3] = [Mode::Warn, Mode::Block, Mode::Off];

    /// 配置与候选列表里用的稳定标识（小写，与 [`Mode::from_str`] 互逆）。
    fn label(self) -> &'static str {
        self.into()
    }

    /// 子命令候选里的一句话说明。
    fn description(self) -> &'static str {
        match self {
            Mode::Off => "Stop scanning prompts and tool arguments",
            Mode::Warn => "Notify only; requests and tool calls go out unchanged",
            Mode::Block => "Redact prompt hits and refuse tool calls that carry a secret",
        }
    }
}

/// 一条命中：文本里的字节区间 + 类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Hit {
    /// 命中类别（`openai-key` 等），用于文案与打码标记。
    category: &'static str,
    /// 命中在文本里的起始字节偏移。
    start: usize,
    /// 命中在文本里的结束字节偏移（不含）。
    end: usize,
}

/// 扩展的进程内状态。
#[derive(Default)]
struct State {
    /// 配置里的模式；None = 还没读过（按需从磁盘加载）。
    mode: Option<Mode>,
    /// 已告警过的 `(类别, 片段)` 哈希，用于去重。
    warned: Vec<u64>,
}

/// 扩展状态的进程级单例。
static STATE: Mutex<State> = Mutex::new(State {
    mode: None,
    warned: Vec::new(),
});

/// 配置文件的读写锁（面板写盘与按需读盘可能并发）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// 配置文件的读缓存（锁序恒为「RwLock → 缓存内层 Mutex」）。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// 合并后的凭据正则：命名分组即类别，一次扫描拿到全部命中。
///
/// 交替顺序即优先级（`regex` 取 leftmost-first，故 `sk-ant-` 必须排在 `sk-` 之前）。
static SECRET_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(concat!(
        r"(?P<private_key>-----BEGIN[A-Z ]*PRIVATE KEY-----)",
        r"|(?P<anthropic_key>sk-ant-[A-Za-z0-9_\-]{20,})",
        r"|(?P<openai_key>sk-(?:proj-)?[A-Za-z0-9_\-]{20,})",
        r"|(?P<github_token>gh[pousr]_[A-Za-z0-9]{20,})",
        r"|(?P<gitlab_token>glpat-[A-Za-z0-9_\-]{20,})",
        r"|(?P<aws_access_key>(?:AKIA|ASIA)[0-9A-Z]{16})",
        r"|(?P<slack_token>xox[abposr]-[A-Za-z0-9\-]{10,})",
        r"|(?P<google_api_key>AIza[0-9A-Za-z_\-]{35})",
        r"|(?P<stripe_key>[sr]k_(?:live|test)_[A-Za-z0-9]{20,})",
        r"|(?P<jwt>eyJ[A-Za-z0-9_\-]{8,}\.eyJ[A-Za-z0-9_\-]{8,}\.[A-Za-z0-9_\-]{8,})",
    ))
    .expect("leak-guard 的规则表必须是合法正则")
});

/// 取状态锁；锁被毒化时取回内部值。
fn lock() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 配置文件路径：`agent_dir()/extensions/leak-guard.json`。
fn config_path() -> PathBuf {
    agent_dir().join(CONFIG_REL)
}

/// 当前模式：内存里有就用，没有就从磁盘加载并缓存。
fn mode() -> Mode {
    if let Some(m) = lock().mode {
        return m;
    }
    let loaded = load_mode(&config_path());
    lock().mode = Some(loaded);
    loaded
}

/// 从指定路径读模式（便于测试直接给路径）；缺失/非法/无法识别时回落 [`Mode::Warn`]。
fn load_mode(path: &Path) -> Mode {
    let _guard = CONFIG_LOCK.read().unwrap_or_else(|e| e.into_inner());
    let value = CONFIG_CACHE.read(path, || read_json_file(path));
    value
        .get(CONFIG_MODE)
        .and_then(Value::as_str)
        .and_then(|s| Mode::from_str(s.trim().to_lowercase().as_str()).ok())
        .unwrap_or(Mode::Warn)
}

/// 把模式写进指定路径的配置文件：保留其它键，缺失时新建目录。
fn store_mode(path: &Path, mode: Mode) -> std::io::Result<()> {
    let _guard = CONFIG_LOCK.write().unwrap_or_else(|e| e.into_inner());

    let mut root = CONFIG_CACHE.read(path, || read_json_file(path));
    if !root.is_object() {
        root = serde_json::json!({});
    }
    if let Some(map) = root.as_object_mut() {
        map.insert(
            CONFIG_MODE.to_string(),
            Value::String(mode.label().to_string()),
        );
    }

    write_json_file(path, &root)?;
    CONFIG_CACHE.store(path, root);
    Ok(())
}

/// 命名分组 → 对外展示的类别名。
fn category(group: &str) -> &'static str {
    match group {
        "private_key" => "private-key",
        "anthropic_key" => "anthropic-key",
        "openai_key" => "openai-key",
        "github_token" => "github-token",
        "gitlab_token" => "gitlab-token",
        "aws_access_key" => "aws-access-key",
        "slack_token" => "slack-token",
        "google_api_key" => "google-api-key",
        "stripe_key" => "stripe-key",
        "jwt" => "jwt",
        _ => "secret",
    }
}

/// 扫描一段文本，返回按位置排序的命中（规则表 + 高熵串，重叠的只留先出现的）。
fn scan(text: &str) -> Vec<Hit> {
    let mut hits: Vec<Hit> = Vec::new();

    for caps in SECRET_RE.captures_iter(text) {
        let Some(whole) = caps.get(0) else {
            continue;
        };
        let Some(index) = (1..caps.len()).find(|i| caps.get(*i).is_some()) else {
            continue;
        };
        let name = SECRET_RE.capture_names().nth(index).flatten().unwrap_or("");
        hits.push(Hit {
            category: category(name),
            start: whole.start(),
            end: whole.end(),
        });
    }

    for (start, end) in high_entropy_runs(text) {
        if hits.iter().any(|h| start < h.end && h.start < end) {
            continue;
        }
        hits.push(Hit {
            category: "high-entropy",
            start,
            end,
        });
    }

    hits.sort_by_key(|h| h.start);
    hits
}

/// 扫描文本里的高熵串，返回 `(start, end)` 字节区间。
///
/// 候选字符集是 base64 的字母表（含 URL 变体的 `-` `_`、填充 `=`）加 `+`，**不含 `/`**：
/// 斜杠会把整条路径连成一个「高熵串」，路径本身永远不是凭据。
fn high_entropy_runs(text: &str) -> Vec<(usize, usize)> {
    let mut runs = Vec::new();
    let mut start = None;

    for (i, c) in text.char_indices() {
        let part = c.is_ascii_alphanumeric() || matches!(c, '+' | '=' | '-' | '_');
        match (part, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                if is_high_entropy(&text[s..i]) {
                    runs.push((s, i));
                }
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start
        && is_high_entropy(&text[s..])
    {
        runs.push((s, text.len()));
    }
    runs
}

/// 一段候选串是否像随机凭据：够长、大小写与数字齐全、熵足够高。
///
/// shortcut: 机器生成的标识一律跳过（见 [`looks_like_identifier`]），那是本仓最常见的高熵噪声；
/// 真要抓 hex 编码的密钥，得另加白名单再放开这一条。
fn is_high_entropy(run: &str) -> bool {
    /// 高熵串的最小长度：短于此的随机串碰撞概率太低，不值得报。
    const MIN_LEN: usize = 32;
    /// 判定为随机串的 Shannon 熵下限（bits/char）。
    const MIN_ENTROPY: f64 = 4.0;

    if run.len() < MIN_LEN || looks_like_identifier(run) {
        return false;
    }

    let (mut lower, mut upper, mut digit) = (false, false, false);
    for c in run.chars() {
        match c {
            'a'..='z' => lower = true,
            'A'..='Z' => upper = true,
            '0'..='9' => digit = true,
            _ => {}
        }
    }

    lower && upper && digit && shannon(run) >= MIN_ENTROPY
}

/// 候选串是否是机器生成的标识而非凭据：去掉 `-` `_` 后只剩 hex（UUID / git sha / 内容哈希），
/// 或者体内带 ISO 日期时间（`2026-11-02T10-25-54-666Z_<uuid>` 这类会话文件名）。
fn looks_like_identifier(run: &str) -> bool {
    run.chars()
        .filter(|c| !matches!(c, '-' | '_'))
        .all(|c| c.is_ascii_hexdigit())
        || TIMESTAMP_RE.is_match(run)
}

/// 字节级 Shannon 熵（bits/char）。
fn shannon(s: &str) -> f64 {
    let mut counts = [0u32; 256];
    for b in s.bytes() {
        counts[b as usize] += 1;
    }
    let n = s.len() as f64;
    counts
        .iter()
        .filter(|c| **c > 0)
        .map(|c| {
            let p = f64::from(*c) / n;
            -p * p.log2()
        })
        .sum()
}

/// 记录一次命中；返回 `true` 表示这条 `(类别, 片段)` 之前没告警过。
fn remember(category: &str, snippet: &str) -> bool {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    category.hash(&mut hasher);
    snippet.hash(&mut hasher);
    let key = hasher.finish();

    let mut st = lock();
    if st.warned.contains(&key) {
        return false;
    }
    if st.warned.len() >= MAX_REMEMBERED {
        st.warned.remove(0);
    }
    st.warned.push(key);
    true
}

/// 把命中替换成 `[REDACTED:<category>]`（`hits` 须按位置升序且互不重叠）。
fn redacted(text: &str, hits: &[Hit]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut cursor = 0;
    for hit in hits {
        out.push_str(&text[cursor..hit.start]);
        out.push_str(&format!("[REDACTED:{}]", hit.category));
        cursor = hit.end;
    }
    out.push_str(&text[cursor..]);
    out
}

/// 把命中的类别去重成一份清单（如 `["openai-key", "jwt"]`）。
fn categories(hits: &[Hit]) -> Vec<&'static str> {
    let mut list: Vec<&'static str> = Vec::new();
    for hit in hits {
        if !list.contains(&hit.category) {
            list.push(hit.category);
        }
    }
    list
}

/// 把命中归类，每类取首条命中做片段预览，供 UI 文案指出到底哪一处触发了告警。
fn describe_hits(hits: &[Hit], text: &str) -> Vec<(&'static str, String)> {
    let mut list: Vec<(&'static str, String)> = Vec::new();
    for hit in hits {
        if !list.iter().any(|(category, _)| *category == hit.category) {
            list.push((hit.category, preview(&text[hit.start..hit.end])));
        }
    }
    list
}

/// 命中片段的预览：只露首尾几个字符，够定位又不再把整条凭据贴一遍。
fn preview(snippet: &str) -> String {
    /// 预览保留的开头字符数。
    const HEAD: usize = 8;
    /// 预览保留的结尾字符数。
    const TAIL: usize = 4;

    let chars: Vec<char> = snippet.chars().collect();
    if chars.len() <= HEAD + TAIL + 1 {
        return snippet.to_string();
    }

    let head: String = chars[..HEAD].iter().collect();
    let tail: String = chars[chars.len() - TAIL..].iter().collect();
    format!("{head}…{tail}")
}

/// 文案里的「像什么」：单类与多类分开写，避免出现 `a openai-key` 这种硬拼。
fn describe(list: &[&str]) -> String {
    if list.len() == 1 {
        format!("what looks like a credential ({})", list[0])
    } else {
        format!("what looks like credentials ({})", list.join(", "))
    }
}

/// 同 [`describe`]，但每类附一条片段预览（只用于 UI 提示，不进模型上下文）。
fn describe_samples(list: &[(&str, String)]) -> String {
    let items: Vec<String> = list
        .iter()
        .map(|(category, sample)| format!("{category}: `{sample}`"))
        .collect();
    if items.len() == 1 {
        format!("what looks like a credential ({})", items[0])
    } else {
        format!("what looks like credentials ({})", items.join(", "))
    }
}

/// 扫一条消息的文本块；`block` 为 true 时把命中打码。
/// 返回本条消息里**新**的命中（类别 + 片段预览）。
fn scan_message(message: &mut AgentMessage, block: bool) -> Vec<(&'static str, String)> {
    let mut fresh = Vec::new();

    for content in message.content.iter_mut() {
        let ContentBlock::Text { text, .. } = content else {
            continue;
        };

        let hits = scan(text);
        if hits.is_empty() {
            continue;
        }
        for hit in &hits {
            if remember(hit.category, &text[hit.start..hit.end]) {
                fresh.push((hit.category, preview(&text[hit.start..hit.end])));
            }
        }
        if block {
            *text = redacted(text, &hits);
        }
    }

    fresh
}

/// 请求前扫描：warn 只提示，block 额外打码。返回本次要提示的命中（类别 + 片段预览）。
fn handle_prompt(messages: &mut [AgentMessage], mode: Mode) -> Vec<(&'static str, String)> {
    let mut reported: Vec<(&'static str, String)> = Vec::new();
    for message in messages.iter_mut() {
        for (category, sample) in scan_message(message, mode == Mode::Block) {
            if !reported.iter().any(|(seen, _)| *seen == category) {
                reported.push((category, sample));
            }
        }
    }
    reported
}

/// 提示文案：请求侧。
fn prompt_warning(reported: &[(&str, String)], mode: Mode) -> String {
    let what = describe_samples(reported);
    if mode == Mode::Block {
        format!(
            "leak-guard: the prompt contains {what} — replaced with [REDACTED:*] in this request. \
             Send a placeholder or read the value from an environment variable instead."
        )
    } else {
        format!(
            "leak-guard: the prompt contains {what}. Prefer a placeholder or an environment \
             variable reference; mode is warn, so the request went out unchanged."
        )
    }
}

/// 工具调用前的检测：命中且模式为 block 时拒绝执行。
fn guard_tool_call(name: &str, args: &Value) -> BeforeToolCallOutcome {
    let mode = mode();
    if mode == Mode::Off {
        return BeforeToolCallOutcome::allow();
    }

    let rendered = args.to_string();
    let hits = scan(&rendered);
    if hits.is_empty() {
        return BeforeToolCallOutcome::allow();
    }

    let fresh: Vec<&'static str> = hits
        .iter()
        .filter(|hit| remember(hit.category, &rendered[hit.start..hit.end]))
        .map(|hit| hit.category)
        .collect();

    if mode == Mode::Block {
        // 原因会回到模型上下文：只报类别，不带片段
        let what = describe(&categories(&hits));
        return BeforeToolCallOutcome {
            block: true,
            reason: Some(format!(
                "leak-guard blocked this call: `{name}` arguments contain {what}. \
                 Read the value from the environment (or a file) inside the command instead of \
                 pasting it into tool arguments."
            )),
            terminate: false,
            args: None,
        };
    }

    if !fresh.is_empty() {
        let what = describe_samples(&describe_hits(&hits, &rendered));
        notify_plain(
            format!("leak-guard: `{name}` arguments contain {what}."),
            UiNotifyLevel::Warning,
        );
    }
    BeforeToolCallOutcome::allow()
}

/// `/leak-guard` 命令族入口：裸命令开面板，带档位直接落盘。
fn command_leak_guard(host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    if arg.is_empty() {
        host.open_extension_settings(EXT);
        return false;
    }

    match Mode::from_str(arg.to_lowercase().as_str()) {
        Ok(mode) => match LeakGuard.apply_setting("mode", mode.label()) {
            Ok(()) => notify_plain(
                format!("leak-guard mode set to {}.", mode.label()),
                UiNotifyLevel::Info,
            ),
            Err(e) => notify_plain(format!("leak-guard: {e}"), UiNotifyLevel::Error),
        },
        Err(_) => notify_plain(
            format!(
                "Usage: /leak-guard [{}]",
                Mode::CHOICES
                    .iter()
                    .map(|m| m.label())
                    .collect::<Vec<_>>()
                    .join("|")
            ),
            UiNotifyLevel::Warning,
        ),
    }
    false
}

/// `leak-guard` 扩展主体。
pub struct LeakGuard;

impl Extension for LeakGuard {
    /// 返回固定扩展名 `leak-guard`。
    fn name(&self) -> &str {
        EXT
    }

    /// `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Flag credential-like strings before they leave in a prompt or tool arguments (leak-guard)"
    }

    /// 声明本扩展移植自 zeroclaw 0.8.7，供 `/extension` 面板展示来源链接。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "zeroclaw".to_string(),
            plugin_version: "0.8.7".to_string(),
            url: "https://github.com/zeroclaw-labs/zeroclaw".to_string(),
        })
    }

    /// 默认关闭：误报会进聊天区，只能由用户显式打开。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` / `Creator` 扩展模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 不提供模型工具（纯检测）。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/leak-guard` 命令：裸命令打开设置面板，带参数直接设档。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Set the leak-guard mode (warn|block|off); no argument opens its \
                          settings panel"
                .to_string(),
            busy_safe: true,
            subcommands: Mode::CHOICES
                .iter()
                .map(|m| SubcommandDef {
                    name: m.label(),
                    description: m.description(),
                })
                .collect(),
        }]
    }

    /// 把 `/leak-guard` 的执行入口接线到 TUI 命令注册表。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_leak_guard);
    }

    /// 订阅请求前变换与工具调用前拦截。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![
            ExtensionHook::TransformContext,
            ExtensionHook::BeforeToolCall,
        ]
    }

    /// 每次 LLM 请求前扫一遍上下文；`warn` 只提示，`block` 把命中打码（只影响本次请求的副本）。
    fn transform_context(&self, messages: &mut Vec<AgentMessage>) -> crate::error::Result<()> {
        let mode = mode();
        if mode == Mode::Off {
            return Ok(());
        }

        let reported = handle_prompt(messages, mode);
        if !reported.is_empty() {
            notify_plain(prompt_warning(&reported, mode), UiNotifyLevel::Warning);
        }
        Ok(())
    }

    /// 工具调用前的检测：`warn` 放行并提示，`block` 拒绝执行。
    fn before_tool_call_ext(
        &self,
        name: &str,
        args: &Value,
        _assistant_message: Option<&AgentMessage>,
        _context: &[AgentMessage],
        _all_args: &Value,
    ) -> std::result::Result<BeforeToolCallOutcome, ToolError> {
        Ok(guard_tool_call(name, args))
    }

    /// 扩展设置面板：只有一个 `mode` 档位。
    fn settings(&self) -> Vec<ExtensionSetting> {
        vec![cycle_setting(
            "mode",
            "Mode",
            "warn flags credential-like strings in the UI only; block also redacts them from the prompt and refuses tool calls that carry them",
            mode().label().to_string(),
            &Mode::CHOICES.map(Mode::label),
        )]
    }

    /// 应用面板选择并立即落盘。
    fn apply_setting(&self, key: &str, value: &str) -> std::result::Result<(), String> {
        if key != "mode" {
            return Err(format!("unknown setting: {key}"));
        }
        let mode = Mode::from_str(value.trim().to_lowercase().as_str())
            .map_err(|_| format!("invalid option: {value}"))?;

        store_mode(&config_path(), mode).map_err(|e| e.to_string())?;
        lock().mode = Some(mode);
        Ok(())
    }

    /// 启用时重读配置；禁用时丢掉模式缓存（告警去重记录保留，避免重新打开就刷屏）。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            lock().mode = Some(load_mode(&config_path()));
        } else {
            lock().mode = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{AUTH_TEST_LOCK, AgentDirGuard};

    /// 串行锁 + 临时 agent_dir：扩展模式与告警去重记录都是进程级全局态。
    fn guard() -> (MutexGuard<'static, ()>, AgentDirGuard) {
        let lock = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        (lock, AgentDirGuard::temp())
    }

    /// 一段像 OpenAI key 的字符串（长度足够、类别明确）。
    const OPENAI: &str = "sk-proj-AbCdEf0123456789GhIjKlMn";

    #[test]
    fn rule_table_matches_the_usual_provider_keys() {
        for (text, expected) in [
            (OPENAI, "openai-key"),
            ("sk-ant-api03-0123456789abcdefghij", "anthropic-key"),
            ("ghp_0123456789abcdefghijABCDEFGHIJ", "github-token"),
            ("AKIAIOSFODNN7EXAMPLE", "aws-access-key"),
            ("AIzaSyA1234567890abcdefghijklmnopqrstuv", "google-api-key"),
            ("-----BEGIN OPENSSH PRIVATE KEY-----", "private-key"),
        ] {
            let hits = scan(text);
            assert_eq!(hits.len(), 1, "{text} 应只命中一条: {hits:?}");
            assert_eq!(hits[0].category, expected, "{text}");
        }
    }

    #[test]
    fn sha_and_uuid_are_not_reported() {
        for text in [
            "da39a3ee5e6b4b0d3255bfef95601890afd80709",
            "d41d8cd98f00b204e9800998ecf8427ed41d8cd98f00b204e9800998ecf8427e",
            "550e8400-e29b-41d4-a716-446655440000",
        ] {
            assert!(
                scan(text).is_empty(),
                "{text} 不该被当成凭据: {:?}",
                scan(text)
            );
        }
    }

    #[test]
    fn paths_and_session_names_are_not_reported() {
        for text in [
            // 本仓会话文件名的完整形态：路径 + 时间戳 + UUID
            "/home/blue/.prux/sessions/--data-Videos-videos-draft-artical--/\
             2026-11-02T10-25-54-666Z_01a19bcb-4dea-7518-9176-439af2a4b1f3.jsonl",
            "/tmp/prux-subagents-1000/data-Videos-videos-draft-artical/tasks/wf_c719a5cd2c67",
            "2026-11-02T10-25-54-666Z_01a19bcb-4dea-7518-9176-439af2a4b1f3",
        ] {
            assert!(
                scan(text).is_empty(),
                "{text} 不该被当成凭据: {:?}",
                scan(text)
            );
        }
    }

    #[test]
    fn random_base64_is_high_entropy_but_prose_is_not() {
        let blob = "q8Zr2NvT5xLw9KpY3mHb7CfJ1sDg6RtA0eU+Zx4Q";
        assert!(is_high_entropy(blob));
        assert!(!is_high_entropy("src_extensions_leak_guard_config_path"));
        assert!(!is_high_entropy("short"));
        assert_eq!(scan(blob).len(), 1);
        assert_eq!(scan(blob)[0].category, "high-entropy");
    }

    #[test]
    fn the_warning_names_the_offending_snippet() {
        let (_g, _ad) = guard();
        LeakGuard.apply_setting("mode", "warn").unwrap();

        let mut messages = vec![AgentMessage::user_text(
            "snippet: q8Zr2NvT5xLw9KpY3mHb7CfJ1sDg6RtA0eU+Zx4Q",
        )];
        let reported = handle_prompt(&mut messages, Mode::Warn);
        let text = prompt_warning(&reported, Mode::Warn);
        assert!(text.contains("high-entropy: `q8Zr2NvT…Zx4Q`"), "{text}");
        assert!(
            !text.contains("q8Zr2NvT5xLw9KpY"),
            "预览不得回显整条串: {text}"
        );
    }

    #[test]
    fn redaction_keeps_the_surrounding_text() {
        let text = format!("export OPENAI_API_KEY={OPENAI}\nnext line");
        let hits = scan(&text);
        let out = redacted(&text, &hits);
        assert_eq!(
            out,
            "export OPENAI_API_KEY=[REDACTED:openai-key]\nnext line"
        );
        assert!(!out.contains("AbCdEf0123456789"), "原文必须被替换掉");
    }

    #[test]
    fn overlapping_high_entropy_run_does_not_duplicate_a_rule_hit() {
        let text = format!("token={OPENAI}");
        let hits = scan(&text);
        assert_eq!(hits.len(), 1, "{hits:?}");
    }

    #[test]
    fn remember_evicts_the_oldest_entry_instead_of_wiping_the_list() {
        let (_g, _ad) = guard();
        for i in 0..MAX_REMEMBERED {
            assert!(
                remember("high-entropy", &format!("snippet-{i}")),
                "第 {i} 条应是新的"
            );
        }
        assert!(
            remember("high-entropy", "snippet-new"),
            "满额后新条目仍要告警"
        );
        assert!(
            remember("high-entropy", "snippet-0"),
            "最早的条目被挤掉后应重新算新命中"
        );
        assert!(
            !remember("high-entropy", "snippet-63"),
            "挤掉一条后其余记录必须保留，否则每轮都会重新告警"
        );
    }

    #[test]
    fn mode_labels_roundtrip_and_the_command_declares_them() {
        for mode in Mode::CHOICES {
            assert_eq!(Mode::from_str(mode.label()).unwrap(), mode);
        }
        let command = LeakGuard.commands().pop().expect("应声明 /leak-guard 命令");
        assert_eq!(command.name, CMD);
        assert_eq!(command.subcommands.len(), Mode::CHOICES.len());
    }

    #[test]
    fn mode_roundtrips_through_the_config_file() {
        let (_g, _ad) = guard();
        let path = config_path();
        assert_eq!(load_mode(&path), Mode::Warn, "缺文件时回落 warn");

        store_mode(&path, Mode::Block).unwrap();
        assert_eq!(load_mode(&path), Mode::Block);

        store_mode(&path, Mode::Off).unwrap();
        assert_eq!(load_mode(&path), Mode::Off);
    }

    #[test]
    fn apply_setting_persists_and_rejects_unknown_keys() {
        let (_g, _ad) = guard();
        LeakGuard.apply_setting("mode", "block").unwrap();
        assert_eq!(load_mode(&config_path()), Mode::Block);
        assert_eq!(mode(), Mode::Block);
        assert!(LeakGuard.apply_setting("nope", "x").is_err());
        assert!(LeakGuard.apply_setting("mode", "turbo").is_err());
    }

    #[test]
    fn warn_mode_reports_but_lets_the_tool_call_through() {
        let (_g, _ad) = guard();
        LeakGuard.apply_setting("mode", "warn").unwrap();

        let outcome = guard_tool_call(
            "bash",
            &serde_json::json!({ "command": format!("curl -H 'Authorization: Bearer {OPENAI}'") }),
        );
        assert!(!outcome.block);
        assert!(outcome.reason.is_none());
    }

    #[test]
    fn block_mode_refuses_a_tool_call_carrying_a_secret() {
        let (_g, _ad) = guard();
        LeakGuard.apply_setting("mode", "block").unwrap();

        let outcome = guard_tool_call(
            "bash",
            &serde_json::json!({ "command": format!("echo {OPENAI}") }),
        );
        assert!(outcome.block);
        let reason = outcome.reason.expect("block 必须给出原因");
        assert!(reason.contains("openai-key"), "{reason}");
        assert!(!reason.contains("AbCdEf0123456789"), "原因里不得回显密钥");
    }

    #[test]
    fn off_mode_scans_nothing() {
        let (_g, _ad) = guard();
        LeakGuard.apply_setting("mode", "off").unwrap();

        let mut messages = vec![AgentMessage::user_text(&format!("key: {OPENAI}"))];
        LeakGuard.transform_context(&mut messages).unwrap();
        assert!(messages[0].text().contains(OPENAI), "off 档不该改写");
        assert!(!guard_tool_call("bash", &serde_json::json!({ "command": OPENAI })).block);
    }

    #[test]
    fn block_mode_redacts_the_text_block_only() {
        let (_g, _ad) = guard();
        LeakGuard.apply_setting("mode", "block").unwrap();

        let mut messages = vec![AgentMessage::user_text(&format!("key: {OPENAI}"))];
        LeakGuard.transform_context(&mut messages).unwrap();
        assert_eq!(messages[0].text(), "key: [REDACTED:openai-key]");
    }

    #[test]
    fn thinking_blocks_keep_their_signature_text() {
        let (_g, _ad) = guard();
        LeakGuard.apply_setting("mode", "block").unwrap();

        let mut message = AgentMessage::user_text("");
        message.content = vec![ContentBlock::Thinking {
            thinking: format!("the key is {OPENAI}"),
            thinking_signature: Some("sig".to_string()),
            redacted: None,
        }];
        let mut messages = vec![message];
        LeakGuard.transform_context(&mut messages).unwrap();
        assert!(
            messages[0].thinking().contains(OPENAI),
            "thinking 块带签名，改文本会让签名失效"
        );
    }
}

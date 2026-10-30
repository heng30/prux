//! `shake` 扩展：手动把上下文里的重内容抖掉。
//!
//! 三档一次性丢弃，都在**请求侧投影**里生效（改写 `agent.messages`，不碰会话文件，
//! 因此 `/tree` 与历史记录不受影响）：
//!
//! | 模式 | 丢什么 |
//! |---|---|
//! | `elide`（默认） | 工具结果的正文换成短占位符；文本里超大的围栏代码块 / XML 块换成一行说明 |
//! | `images` | 所有图片块（消息因此变空时补一行占位文本） |
//! | `thinking` | assistant 的思考块（**保留最后一条 assistant**） |
//!
//! **为什么是标记 + `transform_context`**：命令 handler 只拿得到 TUI 状态（`&mut App`），
//! 模型上下文在 worker 线程的 agent 里，够不到。因此 `/shake` 只落一个一次性标记，
//! 真正动手在下一轮请求前的 [`Extension::transform_context`]；真实条数也在那里回执
//! （经 `Notify` 进聊天区），所以用户看到的是「实际丢了多少」，不是预估值。
//!
//! **最近内容保护**：`elide` 放过末尾约 4000 token（越近越可能是当前正在用的东西）；
//! `thinking` 放过最后一条 assistant——开启 thinking 的 Anthropic 要求当前轮 tool_use
//! 重放时带上思考块，抖掉它会换来 400。
//!
//! **已知边界**：`transform_context` 不区分主 / 子 agent，排队标记会被**下一个**发出的请求
//! 消费。空闲时排队时那必然是主 agent 的下一条 prompt；但若在 run 进行中排队、且下一发请求
//! 恰是某个子代理的，就会抖到子代理的上下文上（一次性标记随之作废，主上下文保持原样）。
//!
//! 默认**不启用**（`Dev` / `Creator` 模式）。

use crate::{
    core::{
        extensions::{
            CommandHost, Extension, ExtensionCommand, ExtensionHook, ExtensionMode, ExtensionTool,
            SubcommandDef, UiNotifyLevel,
        },
        provider::{AgentMessage, ContentBlock, estimate_tokens},
    },
    error::Result,
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_SHAKE, command_arg, util::notify_plain,
    },
    modes::interactive::handlers::register_host_command,
    utils::{display::plural, tokens},
};
use std::sync::{Arc, Mutex, OnceLock};
use strum_macros::{EnumString, IntoStaticStr};

/// 扩展名（也是 `/extension` 面板中的标识）。
const EXT: &str = "shake";

/// 斜杠命令名（`/shake`）。
const CMD: &str = "shake";

/// `elide` 放过的末尾 token 预算：越近的内容越可能是当前正在使用的上下文。
const PROTECT_TOKENS: u32 = 4_000;

/// 围栏代码块 / XML 块够大（token）才值得换成占位符。
const FENCE_MIN_TOKENS: u64 = 400;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static SHAKE_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_SHAKE,
    make: || -> Arc<dyn Extension> { Arc::new(Shake) },
};

/// 丢弃模式（`/shake elide|images|thinking`）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
enum ShakeMode {
    /// 工具结果正文 + 超大块 → 短占位符（默认）。
    #[default]
    Elide,
    /// 图片块。
    Images,
    /// assistant 思考块。
    Thinking,
}

/// 一次丢弃的统计（回执文案的唯一来源）。
#[derive(Debug, Default, Clone, Copy)]
struct ShakeReport {
    /// 本次使用的模式。
    mode: ShakeMode,
    /// 被换成占位符的工具结果条数。
    tool_results: usize,
    /// 被换成占位符的围栏代码块 / XML 块个数。
    blocks: usize,
    /// 被移除的图片块个数。
    images: usize,
    /// 被移除的思考块个数。
    thinking: usize,
    /// 估算释放的 token 数。
    tokens_freed: u32,
}

impl ShakeReport {
    /// 按模式构造一份空统计。
    fn new(mode: ShakeMode) -> ShakeReport {
        ShakeReport {
            mode,
            ..Default::default()
        }
    }

    /// 一条回执文案：图片 / 思考模式各自点名，`elide` 合并两类计数；无改动时明确说没有。
    fn summary(&self) -> String {
        match self.mode {
            ShakeMode::Images => match self.images {
                0 => "No images found in this session.".to_string(),
                n => format!("Dropped {n} image{} from this session.", plural(n)),
            },
            ShakeMode::Thinking => match self.thinking {
                0 => "No thinking blocks found in this session.".to_string(),
                n => format!(
                    "Dropped {n} thinking block{} from this session (~{} tokens freed).",
                    plural(n),
                    crate::utils::display::format_tokens(self.tokens_freed as u64)
                ),
            },
            ShakeMode::Elide => {
                let mut parts: Vec<String> = Vec::new();
                if self.tool_results > 0 {
                    parts.push(format!(
                        "{} tool result{}",
                        self.tool_results,
                        plural(self.tool_results)
                    ));
                }
                if self.blocks > 0 {
                    parts.push(format!("{} block{}", self.blocks, plural(self.blocks)));
                }
                if parts.is_empty() {
                    "Nothing to shake.".to_string()
                } else {
                    format!(
                        "Shook {} (~{} tokens freed).",
                        parts.join(" + "),
                        crate::utils::display::format_tokens(self.tokens_freed as u64)
                    )
                }
            }
        }
    }
}

/// 待执行的一次性模式（进程级单例）：命令写入，`transform_context` 取走。
fn pending() -> &'static Mutex<Option<ShakeMode>> {
    /// 待执行的一次性 shake 模式；None = 当前没有排队的 shake。
    static P: OnceLock<Mutex<Option<ShakeMode>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// 取走待执行模式（一次性）：没有排队时返回 `None`。
fn take_pending() -> Option<ShakeMode> {
    pending().lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// 清空排队中的模式（禁用扩展 / 换会话时调用）。
fn clear_pending() {
    *pending().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// `shake` 扩展主体。
pub struct Shake;

impl Extension for Shake {
    /// 返回固定扩展名 `shake`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Drop heavy content from the context on demand: /shake [elide|images|thinking] \
         replaces tool results and oversized blocks with short placeholders, or strips \
         image / thinking blocks. It applies before the next request; session files and the \
         transcript are never rewritten."
    }

    /// 声明 `Dev` / `Creator` 两个并排模式：`Minimal` 下不可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认不启用：会改写模型上下文，属用户显式开启的能力。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供工具，只提供斜杠命令。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/shake` 及其三个模式子命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Drop heavy content from the context (tool results, images, thinking)"
                .to_string(),
            // handler 只落一次性标记，不锁 agent：忙碌时也能排队（在下一轮请求前生效）。
            busy_safe: true,
            subcommands: vec![
                SubcommandDef {
                    name: "elide",
                    description: "Strip tool results and oversized blocks (default)",
                },
                SubcommandDef {
                    name: "images",
                    description: "Strip image blocks",
                },
                SubcommandDef {
                    name: "thinking",
                    description: "Drop thinking blocks",
                },
            ],
        }]
    }

    /// 接线 `/shake` 执行入口。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_shake);
    }

    /// 只在有排队的模式时声明 `TransformContext`：空闲时框架直接跳过本扩展。
    fn hooks(&self) -> Vec<ExtensionHook> {
        if pending()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
        {
            vec![ExtensionHook::TransformContext]
        } else {
            Vec::new()
        }
    }

    /// 禁用时清掉排队模式，免得下次请求莫名其妙掉内容。
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            clear_pending();
        }
    }

    /// 换会话时清掉排队模式，避免抖到另一个会话上。
    fn on_session_switched(&self, _session_path: Option<&str>, _messages: &[AgentMessage]) {
        clear_pending();
    }

    /// 请求前投影：取走一次性模式，按模式改写 `messages`，并把**实际**丢掉的数量回执给用户。
    fn transform_context(&self, messages: &mut Vec<AgentMessage>) -> Result<()> {
        let Some(mode) = take_pending() else {
            return Ok(());
        };

        let report = match mode {
            ShakeMode::Elide => elide(messages),
            ShakeMode::Images => drop_images(messages),
            ShakeMode::Thinking => drop_thinking(messages),
        };
        notify_plain(report.summary(), UiNotifyLevel::Info);
        Ok(())
    }
}

/// `/shake [elide|images|thinking]` 命令分发：解析模式 → 落一次性标记（真正丢弃在下一轮请求前）。
fn command_shake(host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw).trim().to_lowercase();
    let mode = if arg.is_empty() {
        ShakeMode::Elide
    } else {
        match arg.parse::<ShakeMode>() {
            Ok(mode) => mode,
            Err(_) => {
                host.push_message(
                    &format!("Unknown /shake mode \"{arg}\". Use elide, images, or thinking."),
                    UiNotifyLevel::Warning,
                );
                return false;
            }
        }
    };

    let name: &'static str = mode.into();
    *pending().lock().unwrap_or_else(|e| e.into_inner()) = Some(mode);
    host.push_message(
        &format!("Shake ({name}) queued: heavy content drops before the next request."),
        UiNotifyLevel::Info,
    );
    false
}

/// `elide`：工具结果正文与超大块换成占位符，放过末尾 [`PROTECT_TOKENS`] 个 token。
fn elide(messages: &mut [AgentMessage]) -> ShakeReport {
    let protect_from = protect_boundary(messages, PROTECT_TOKENS);
    let mut report = ShakeReport::new(ShakeMode::Elide);

    for message in messages.iter_mut().take(protect_from) {
        let before = estimate_tokens(message);
        let changed = match message.role.as_str() {
            "toolResult" => elide_tool_result(message, &mut report),
            "user" | "assistant" | "custom" => elide_message_blocks(message, &mut report),
            _ => false,
        };
        if changed {
            report.tokens_freed += before.saturating_sub(estimate_tokens(message));
        }
    }
    report
}

/// 把一条工具结果的正文换成占位符（保留图片等非文本块）；返回是否改动。
fn elide_tool_result(message: &mut AgentMessage, report: &mut ShakeReport) -> bool {
    let has_text = message
        .content
        .iter()
        .any(|b| matches!(b, ContentBlock::Text { text, .. } if !text.trim().is_empty()));
    if !has_text {
        return false;
    }

    let tool = message
        .tool_name
        .as_deref()
        .filter(|n| !n.is_empty())
        .unwrap_or("unknown");
    let mut kept: Vec<ContentBlock> = Vec::with_capacity(message.content.len());
    kept.push(placeholder(&format!(
        "[tool result elided by /shake: {tool} output dropped]"
    )));
    kept.extend(
        message
            .content
            .drain(..)
            .filter(|b| !matches!(b, ContentBlock::Text { .. })),
    );
    message.content = kept;
    report.tool_results += 1;
    true
}

/// 把一条消息文本块里超大的围栏代码块 / XML 块换成占位行；返回是否改动。
fn elide_message_blocks(message: &mut AgentMessage, report: &mut ShakeReport) -> bool {
    let mut changed = false;
    for block in message.content.iter_mut() {
        let ContentBlock::Text { text, .. } = block else {
            continue;
        };
        let (replaced, count) = elide_large_blocks(text);
        if count > 0 {
            *text = replaced;
            report.blocks += count;
            changed = true;
        }
    }
    changed
}

/// `images`：移除所有图片块；消息因此变空时补一行占位文本（空消息会被上游拒）。
fn drop_images(messages: &mut [AgentMessage]) -> ShakeReport {
    let mut report = ShakeReport::new(ShakeMode::Images);
    for message in messages.iter_mut() {
        let before = estimate_tokens(message);
        let mut removed = 0usize;
        message.content.retain(|block| {
            if matches!(block, ContentBlock::Image { .. }) {
                removed += 1;
                false
            } else {
                true
            }
        });
        if removed == 0 {
            continue;
        }
        if message.content.is_empty() {
            message
                .content
                .push(placeholder("[image removed by /shake]"));
        }
        report.images += removed;
        report.tokens_freed += before.saturating_sub(estimate_tokens(message));
    }
    report
}

/// `thinking`：移除 assistant 的思考块，**保留最后一条 assistant**（当前轮 tool_use 重放需要它）。
fn drop_thinking(messages: &mut [AgentMessage]) -> ShakeReport {
    let mut report = ShakeReport::new(ShakeMode::Thinking);
    let protected = messages.iter().rposition(|m| m.role == "assistant");
    for (index, message) in messages.iter_mut().enumerate() {
        if message.role != "assistant" || Some(index) == protected {
            continue;
        }
        let before = estimate_tokens(message);
        let mut removed = 0usize;
        message.content.retain(|block| {
            if matches!(block, ContentBlock::Thinking { .. }) {
                removed += 1;
                false
            } else {
                true
            }
        });
        if removed == 0 {
            continue;
        }
        report.thinking += removed;
        report.tokens_freed += before.saturating_sub(estimate_tokens(message));
    }
    report
}

/// 末尾 token 预算的保护区起点：`messages[返回下标..]` 不被 `elide` 改动。
///
/// 从末尾往前累加估算 token，累计达到 `budget` 时返回该消息的下标；
/// 整段上下文都不足 `budget` 时返回 `0`（等于"全都受保护"）。
fn protect_boundary(messages: &[AgentMessage], budget: u32) -> usize {
    let mut total = 0u32;
    for (index, message) in messages.iter().enumerate().rev() {
        total = total.saturating_add(estimate_tokens(message));
        if total >= budget {
            return index;
        }
    }
    0
}

/// 一条短占位文本块。
fn placeholder(text: &str) -> ContentBlock {
    ContentBlock::Text {
        text: text.to_string(),
        text_signature: None,
    }
}

/// 把一段文本里超大的围栏代码块与 XML 块换成占位行；返回（新文本，替换块数）。
fn elide_large_blocks(text: &str) -> (String, usize) {
    let lines: Vec<&str> = text.split('\n').collect();
    let enc = tokens::encoding_for(None);
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut count = 0usize;
    let mut index = 0usize;

    while index < lines.len() {
        let span = block_span(&lines, index).filter(|(end, _)| {
            let body = lines[index..=*end].join("\n");
            tokens::count(enc, &body) >= FENCE_MIN_TOKENS
        });

        match span {
            Some((end, kind)) => {
                let body = lines[index..=end].join("\n");
                let tokens = tokens::count(enc, &body);
                out.push(format!(
                    "[... /shake elided a {}-line {kind} block, about {tokens} tokens ...]",
                    end - index + 1
                ));
                count += 1;
                index = end + 1;
            }
            None => {
                out.push(lines[index].to_string());
                index += 1;
            }
        }
    }

    if count == 0 {
        return (text.to_string(), 0);
    }
    (out.join("\n"), count)
}

/// 从第 `start` 行开始的围栏代码块 / XML 块跨度；返回（结束行下标，种类名）。
fn block_span(lines: &[&str], start: usize) -> Option<(usize, &'static str)> {
    let line = lines[start].trim();
    if let Some((fence, len)) = fence_open(line) {
        let end = (start + 1..lines.len()).find(|&i| fence_close(lines[i].trim(), fence, len))?;
        return Some((end, "code"));
    }
    if let Some(tag) = xml_open(line) {
        let closing = format!("</{tag}>");
        let end = (start + 1..lines.len()).find(|&i| lines[i].trim() == closing)?;
        return Some((end, "xml"));
    }
    None
}

/// 围栏代码块的起始行：3 个以上的 ``` 或 ~~~；返回（字符，长度）。
fn fence_open(line: &str) -> Option<(char, usize)> {
    let first = line.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let len = line.chars().take_while(|c| *c == first).count();
    if len < 3 {
        return None;
    }
    Some((first, len))
}

/// 围栏代码块的结束行：同一个字符、长度不短于起始围栏、且没有其它内容。
fn fence_close(line: &str, fence: char, len: usize) -> bool {
    let count = line.chars().count();
    count >= len && line.chars().all(|c| c == fence)
}

/// 顶层 XML 元素的起始行（**只认小写标签名**，与提示词里的块约定一致）；返回标签名。
fn xml_open(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('<')?;
    if rest.starts_with('/') || rest.starts_with('!') || rest.starts_with('?') {
        return None;
    }
    if !line.ends_with('>') || line.ends_with("/>") {
        return None;
    }
    let name_end = rest.find(|c: char| c.is_whitespace() || c == '>')?;
    let name = &rest[..name_end];
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c == '_' || c == '-')
    {
        return None;
    }
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::AgentMessage;
    use crate::modes::interactive::app::App;

    /// 造一条足够长的文本（保证 token 估算远超保护预算）。
    fn filler(tag: &str, repeats: usize) -> String {
        (0..repeats)
            .map(|i| format!("{tag} line {i} with some words to count"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 造一条工具结果消息。
    fn tool_result(name: &str, text: &str) -> AgentMessage {
        let mut message = AgentMessage::user_text(text);
        message.role = "toolResult".into();
        message.tool_call_id = Some("call_1".into());
        message.tool_name = Some(name.into());
        message
    }

    /// 造一条 assistant 消息（可带思考块）。
    fn assistant(text: &str, thinking: Option<&str>) -> AgentMessage {
        let mut message = AgentMessage::user_text(text);
        message.role = "assistant".into();
        if let Some(thinking) = thinking {
            message.content.push(ContentBlock::Thinking {
                thinking: thinking.into(),
                thinking_signature: Some("sig".into()),
                redacted: None,
            });
        }
        message
    }

    #[test]
    fn mode_parsing_is_lowercase_only() {
        assert_eq!("elide".parse::<ShakeMode>(), Ok(ShakeMode::Elide));
        assert_eq!("images".parse::<ShakeMode>(), Ok(ShakeMode::Images));
        assert_eq!("thinking".parse::<ShakeMode>(), Ok(ShakeMode::Thinking));
        assert!("Elide".parse::<ShakeMode>().is_err());
        assert!("everything".parse::<ShakeMode>().is_err());
    }

    #[test]
    fn command_declares_three_subcommands() {
        let commands = Shake.commands();
        assert_eq!(commands.len(), 1);
        assert_eq!(commands[0].name, "shake");
        assert!(commands[0].busy_safe, "handler 只落标记，不锁 agent");
        let names: Vec<&str> = commands[0].subcommands.iter().map(|s| s.name).collect();
        assert_eq!(names, vec!["elide", "images", "thinking"]);
    }

    #[test]
    fn defaults_are_off_and_dev_only() {
        assert!(!Shake.default_enabled(), "shake 应默认不启用");
        assert_eq!(
            Shake.modes(),
            vec![ExtensionMode::Dev, ExtensionMode::Creator]
        );
        assert!(Shake.tools().is_empty(), "shake 不提供工具");
    }

    #[test]
    fn hooks_only_declared_when_armed() {
        clear_pending();
        assert!(Shake.hooks().is_empty(), "空闲时不声明钩子");
        *pending().lock().unwrap() = Some(ShakeMode::Images);
        assert_eq!(Shake.hooks(), vec![ExtensionHook::TransformContext]);
        clear_pending();
    }

    #[test]
    fn transform_is_one_shot() {
        clear_pending();
        let mut messages = vec![
            AgentMessage::user_text("go"),
            assistant("done", Some("thinking hard")),
            AgentMessage::user_text("next"),
            assistant("next done", Some("more thinking")),
        ];
        *pending().lock().unwrap() = Some(ShakeMode::Thinking);
        Shake.transform_context(&mut messages).unwrap();
        assert!(pending().lock().unwrap().is_none(), "一次性标记应被取走");
        let thinking = messages
            .iter()
            .filter(|m| {
                m.content
                    .iter()
                    .any(|b| matches!(b, ContentBlock::Thinking { .. }))
            })
            .count();
        assert_eq!(thinking, 1, "只有最后一条 assistant 保留思考");

        // 第二次调用不再改动
        let before = messages.len();
        Shake.transform_context(&mut messages).unwrap();
        assert_eq!(messages.len(), before);
    }

    #[test]
    fn elide_replaces_tool_results_and_keeps_images() {
        clear_pending();
        let mut message = tool_result("bash", "a very long tool output");
        message.content.push(ContentBlock::Image {
            data: "AAA".into(),
            mime_type: "image/png".into(),
        });
        // 末尾塞一大段正文把工具结果挤出保护区（保护预算 4000 token）
        let mut tail = AgentMessage::user_text(&filler("tail", 900));
        tail.role = "user".into();
        let mut messages = vec![AgentMessage::user_text("go"), message, tail];

        let report = elide(&mut messages);
        assert_eq!(report.tool_results, 1);
        assert_eq!(messages[1].role, "toolResult");
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("call_1"));
        let text = messages[1].text();
        assert!(text.contains("elided by /shake"), "got: {text}");
        assert!(text.contains("bash"), "占位符应点名工具: {text}");
        assert!(
            messages[1]
                .content
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. })),
            "图片应保留"
        );
    }

    #[test]
    fn elide_protects_recent_tokens() {
        // 末尾长期内容受保护：只有 4 条小消息时整段都在 4000 token 预算内。
        let mut messages = vec![
            AgentMessage::user_text("one"),
            tool_result("bash", "small"),
            assistant("two", None),
            AgentMessage::user_text("three"),
        ];
        let report = elide(&mut messages);
        assert_eq!(report.tool_results, 0, "预算内不应改动");
        assert!(messages[1].text().contains("small"));
        assert_eq!(protect_boundary(&messages, PROTECT_TOKENS), 0);
    }

    #[test]
    fn elide_skips_small_blocks_and_shakes_big_ones() {
        let big_body = filler("code", FENCE_MIN_TOKENS as usize / 4);
        let small_body = "y".repeat(16);
        let text = format!("intro\n```rust\n{big_body}\n```\nmiddle\n```\n{small_body}\n```\nend");
        let (rebuilt, count) = elide_large_blocks(&text);
        assert_eq!(count, 1, "只有大块被替换: {rebuilt}");
        assert!(
            rebuilt.contains("/shake elided a") && rebuilt.contains("code block"),
            "got: {rebuilt}"
        );
        assert!(rebuilt.contains(&small_body), "小块应原样保留");
        assert!(rebuilt.contains("intro") && rebuilt.contains("end"));
    }

    #[test]
    fn elide_handles_xml_blocks() {
        let body = "z".repeat(FENCE_MIN_TOKENS as usize * 4);
        let text = format!("<file path=\"a.rs\">\n{body}\n</file>");
        let (rebuilt, count) = elide_large_blocks(&text);
        assert_eq!(count, 1, "got: {rebuilt}");
        assert!(rebuilt.contains("3-line xml block"), "got: {rebuilt}");
        assert!(!rebuilt.contains("a.rs"), "整块被换掉: {rebuilt}");
    }

    #[test]
    fn elide_leaves_unterminated_blocks_alone() {
        let body = "z".repeat(FENCE_MIN_TOKENS as usize * 4);
        let text = format!("```\n{body}\n");
        let (rebuilt, count) = elide_large_blocks(&text);
        assert_eq!(count, 0, "没有闭合围栏不算块");
        assert_eq!(rebuilt, text);
    }

    #[test]
    fn drop_images_counts_and_keeps_message_non_empty() {
        let mut message = AgentMessage::user_text("");
        message.content = vec![ContentBlock::Image {
            data: "AAA".into(),
            mime_type: "image/png".into(),
        }];
        let mut messages = vec![AgentMessage::user_text("look"), message];

        let report = drop_images(&mut messages);
        assert_eq!(report.images, 1);
        assert!(
            messages[1]
                .content
                .iter()
                .all(|b| !matches!(b, ContentBlock::Image { .. })),
            "图片应被移除"
        );
        assert!(
            messages[1].text().contains("image removed by /shake"),
            "空消息需补占位文本: {:?}",
            messages[1].text()
        );
        assert!(
            report
                .summary()
                .contains("Dropped 1 image from this session.")
        );
    }

    #[test]
    fn drop_thinking_keeps_last_assistant() {
        let mut messages = vec![
            assistant("a1", Some("t1")),
            AgentMessage::user_text("u"),
            assistant("a2", Some("t2")),
        ];
        let report = drop_thinking(&mut messages);
        assert_eq!(report.thinking, 1, "只掉非末尾 assistant 的思考");
        assert!(messages[0].text().contains("a1"));
        assert_eq!(messages[2].content.len(), 2, "末尾 assistant 不动");
    }

    #[test]
    fn summaries_cover_empty_and_hit_cases() {
        assert_eq!(
            ShakeReport::new(ShakeMode::Elide).summary(),
            "Nothing to shake."
        );
        assert_eq!(
            ShakeReport::new(ShakeMode::Images).summary(),
            "No images found in this session."
        );
        assert_eq!(
            ShakeReport::new(ShakeMode::Thinking).summary(),
            "No thinking blocks found in this session."
        );

        let report = ShakeReport {
            tool_results: 2,
            blocks: 1,
            tokens_freed: 1200,
            ..ShakeReport::new(ShakeMode::Elide)
        };
        assert_eq!(
            report.summary(),
            "Shook 2 tool results + 1 block (~1.2K tokens freed)."
        );

        let report = ShakeReport {
            thinking: 1,
            tokens_freed: 900,
            ..ShakeReport::new(ShakeMode::Thinking)
        };
        assert_eq!(
            report.summary(),
            "Dropped 1 thinking block from this session (~900 tokens freed)."
        );
    }

    #[test]
    fn command_queues_mode_and_rejects_unknown() {
        clear_pending();
        let mut st = App::new();

        // 默认 elide
        assert!(!command_shake(&mut st, "shake"));
        assert_eq!(take_pending(), Some(ShakeMode::Elide));

        // 显式模式（大小写不敏感）
        assert!(!command_shake(&mut st, "shake IMAGES"));
        assert_eq!(take_pending(), Some(ShakeMode::Images));

        // 未知模式：不排队
        assert!(!command_shake(&mut st, "shake everything"));
        assert_eq!(take_pending(), None);
    }
}

//! `shrink` 扩展：工具结果进上下文之前的**内容类型压缩**。
//!
//! 与 [`crate::utils::truncate`] 的分工：那是「按行数/字节数砍掉尾巴」，不管内容是什么；
//! 这里是「认出这是搜索结果 / 日志 / diff / JSON 数组，按各类型的信息密度重新排版」。
//! 两者互补——砍完还大的结果才值得按类型重排。
//!
//! 三件事：
//!
//! 1. **压缩**：[`ExtensionHook::AfterToolCall`] 上按 [`pipeline::detect`] 认出类型，
//!    交给对应压缩器（[`pipeline`]）。压完不比原文短、或类型没开、或识别不出，
//!    都原样放行——**绝不放大、绝不猜**。
//! 2. **保原文**：压缩丢掉的字节写进 `utils::output_files` 的临时文件（0600、独占创建），
//!    尾部给一行路径，模型要原文就 `read` 它。`keepFull = false` 时压完即丢。
//! 3. **可观测量**：`/shrink` 打印配置与本进程累计节省，`/shrink reset` 归零。
//!
//! 失败语义：本扩展的任何内部问题都**原样放行工具结果**。`apply_after_tool_call` 的
//! `Err` 分支会把结果整体替换成错误文本并把调用标记为失败，压缩器绝不能走到那里。
//!
//! 配置见 `agent_dir()/extensions/shrink.json`（[`config`]）；默认关闭——
//! 它会改变模型看到的内容，属于「用户自己决定开」的那一类。
//!
//! ## 与其它 `AfterToolCall` 扩展的顺序
//!
//! 注册优先级 [`PRIORITY_SHRINK`] 低于 `injection_guard`（让它扫到未压缩的原文）、
//! 高于 `hooks`（`PostToolUse` 追加的上下文不要被当成工具输出重排）。

mod config;
mod pipeline;

use crate::{
    core::{
        extensions::{
            AfterToolCallOutcome, CommandHost, Extension, ExtensionCommand, ExtensionHook,
            ExtensionMode, ExtensionTool, SubcommandDef, UiNotifyLevel, register_host_command,
        },
        provider::AgentMessage,
        tools::ToolError,
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_SHRINK, command_arg},
    utils::{display::format_bytes, output_files::write_output_file},
};
use serde_json::Value;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

/// 扩展名（`/extension` 面板里的标识，也是 `/shrink` 的命令归属）。
const EXT: &str = "shrink";

/// 斜杠命令名（`/shrink`）。
const CMD: &str = "shrink";

/// `/shrink` 子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "settings",
        description: "Print the shrink settings and the savings of this process (default)",
    },
    SubcommandDef {
        name: "reset",
        description: "Reset the savings counters of this process",
    },
];

/// 压缩结果尾部标记的起始片段：既给模型看，也用来识别「这份结果已经压过了」。
const MARKER: &str = "[shrunk ";

/// 本进程已压缩的工具结果数。
static SHRUNK: AtomicU64 = AtomicU64::new(0);
/// 本进程压缩前的累计字节数。
static BYTES_IN: AtomicU64 = AtomicU64::new(0);
/// 本进程压缩后的累计字节数。
static BYTES_OUT: AtomicU64 = AtomicU64::new(0);

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static SHRINK_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_SHRINK,
    make: || -> Arc<dyn Extension> { Arc::new(Shrink) },
};

/// `shrink` 扩展：只有一个 `AfterToolCall` 钩子和一条 `/shrink` 命令。
pub struct Shrink;

impl Shrink {
    /// 就地压缩一份工具结果：返回改写后的文本，或 `None` 表示原样放行。
    fn shrink_result(&self, tool: &str, text: &str) -> Option<String> {
        let cfg = config::load();
        if text.len() < cfg.min_bytes || text.contains(MARKER) {
            return None;
        }

        let kind = pipeline::detect(tool, text);
        if !cfg.allows(kind) {
            return None;
        }
        let compressed = pipeline::compress(kind, text)?;
        if compressed.len() >= text.len() {
            return None;
        }

        let full = cfg
            .keep_full
            .then(|| write_output_file("tool-output", "txt", text.as_bytes()).ok())
            .flatten();

        SHRUNK.fetch_add(1, Ordering::Relaxed);
        BYTES_IN.fetch_add(text.len() as u64, Ordering::Relaxed);
        BYTES_OUT.fetch_add(compressed.len() as u64, Ordering::Relaxed);

        let kind: &str = kind.into();
        let mut out = compressed;
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!(
            "\n{}{}: {} → {}",
            MARKER,
            kind,
            format_bytes(text.len() as u64),
            format_bytes(out.len() as u64)
        ));
        match full {
            Some(path) => out.push_str(&format!("; full output: {}", path.display())),
            None => out.push_str("; original discarded"),
        }
        out.push_str("]\n");
        Some(out)
    }
}

/// `/shrink` 命令入口：打印配置与累计节省。
fn command_shrink(host: &mut dyn CommandHost, raw: &str) -> bool {
    if command_arg(raw).trim() == "reset" {
        SHRUNK.store(0, Ordering::Relaxed);
        BYTES_IN.store(0, Ordering::Relaxed);
        BYTES_OUT.store(0, Ordering::Relaxed);
        host.push_message("shrink: savings counters reset", UiNotifyLevel::Info);
        return false;
    }

    let cfg = config::load();
    let count = SHRUNK.load(Ordering::Relaxed);
    let bytes_in = BYTES_IN.load(Ordering::Relaxed);
    let bytes_out = BYTES_OUT.load(Ordering::Relaxed);

    let mut lines = vec![format!(
        "shrink: minBytes {} · search {} · log {} · diff {} · json {} · keepFull {}",
        cfg.min_bytes,
        on_off(cfg.search),
        on_off(cfg.log),
        on_off(cfg.diff),
        on_off(cfg.json),
        on_off(cfg.keep_full)
    )];
    lines.push(if count == 0 {
        "no tool result shrunk in this process yet".to_string()
    } else {
        format!(
            "{count} tool results shrunk: {} → {} ({} saved)",
            format_bytes(bytes_in),
            format_bytes(bytes_out),
            format_bytes(bytes_in.saturating_sub(bytes_out))
        )
    });
    if !cfg.errors.is_empty() {
        lines.push(format!(
            "config errors in {}:",
            config::config_path().display()
        ));
        lines.extend(cfg.errors.iter().map(|e| format!("  {e}")));
    }
    lines.push(format!("config: {}", config::config_path().display()));

    host.push_message(&lines.join("\n"), UiNotifyLevel::Info);
    false
}

/// `true` 显示 `on`，`false` 显示 `off`（只用于 `/shrink` 的单行概览）。
fn on_off(flag: bool) -> &'static str {
    if flag { "on" } else { "off" }
}

impl Extension for Shrink {
    /// 扩展名 `shrink`。
    fn name(&self) -> &str {
        EXT
    }

    /// 面板说明：按内容类型压缩工具结果。
    fn description(&self) -> &str {
        "Shrink tool results by content kind (search hits, logs, diffs, JSON arrays) before \
         they enter the context. The full original is kept in a temp file. \
         /shrink shows settings and savings"
    }

    /// 仅 `Dev` / `Creator` 模式可用：它会改变模型看到的内容。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认关闭：压缩会丢信息，由用户显式打开。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供工具：全部行为由工具结果钩子驱动。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/shrink`（查看配置与累计节省）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Show shrink settings and savings; /shrink reset clears the counters"
                .to_string(),
            busy_safe: true,
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 接线 `/shrink` 执行入口。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_shrink);
    }

    /// 只关心工具结果。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::AfterToolCall]
    }

    /// 换会话时归零统计：`/shrink` 报的是「本会话看到的节省」。
    fn on_session_switched(&self, _session_path: Option<&str>, _messages: &[AgentMessage]) {
        SHRUNK.store(0, Ordering::Relaxed);
        BYTES_IN.store(0, Ordering::Relaxed);
        BYTES_OUT.store(0, Ordering::Relaxed);
    }

    /// 工具结果压缩：任何内部问题都回到原文，绝不返回 `Err`。
    fn after_tool_call_ext(
        &self,
        name: &str,
        _args: &Value,
        result_text: &str,
        is_error: bool,
    ) -> std::result::Result<AfterToolCallOutcome, ToolError> {
        let pass = || AfterToolCallOutcome {
            text: result_text.to_string(),
            content: None,
            details: None,
            is_error: None,
            usage: None,
            terminate: None,
        };
        // 失败结果通常是短小且要紧的报错，压它只会藏起原因。
        if is_error {
            return Ok(pass());
        }
        match self.shrink_result(name, result_text) {
            Some(text) => Ok(AfterToolCallOutcome {
                text,
                content: None,
                details: None,
                is_error: None,
                usage: None,
                terminate: None,
            }),
            None => Ok(pass()),
        }
    }

    /// 被禁用时把统计归零（用户关掉后不必再看到旧数字）。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            return;
        }
        SHRUNK.store(0, Ordering::Relaxed);
        BYTES_IN.store(0, Ordering::Relaxed);
        BYTES_OUT.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;
    use serde_json::json;

    /// 压缩后带 `[shrunk <kind>` 标记与原文路径，且比原文短。
    #[test]
    fn shrinks_search_results_and_keeps_original() {
        let mut text = String::new();
        for i in 0..200 {
            text.push_str(&format!(
                "src/some/long/path/file{}.rs:{i}: a fairly long body line with content {i}\n",
                i % 30
            ));
        }
        let _dir = AgentDirGuard::temp();
        let ext = Shrink;
        let out = ext
            .shrink_result("grep", &text)
            .expect("200 行命中应当被压缩");
        assert!(out.contains("[shrunk search:"), "{out}");
        assert!(out.contains("full output: "), "{out}");
        assert!(out.len() < text.len());

        let path = out
            .split("full output: ")
            .nth(1)
            .and_then(|s| s.split(']').next())
            .expect("路径")
            .trim()
            .to_string();
        let saved = std::fs::read_to_string(&path).expect("原文已落盘");
        assert_eq!(saved, text, "落盘的必须是完整原文");
    }

    /// 短结果、已压过的结果、Plain 内容都原样放行。
    #[test]
    fn leaves_small_and_already_shrunk_results_alone() {
        let _dir = AgentDirGuard::temp();
        let ext = Shrink;
        assert!(
            ext.shrink_result("grep", "a.rs:1:x\n").is_none(),
            "低于 minBytes"
        );
        let once = format!("{MARKER}search: 1B → 1B]\n");
        assert!(ext.shrink_result("grep", &once).is_none(), "带标记的不再压");
        let prose = "一句话。\n".repeat(400);
        assert!(
            ext.shrink_result("read", &prose).is_none(),
            "读不出类型就不压"
        );
    }

    /// 结果别改动：错误标记、details 等一律保持原样，只换 text。
    #[test]
    fn keeps_error_results_untouched() {
        let ext = Shrink;
        let text = "src/a.rs:1:x\n".repeat(400);
        let out = ext
            .after_tool_call_ext("grep", &json!({}), &text, true)
            .unwrap();
        assert_eq!(out.text, text);
        assert!(out.content.is_none() && out.details.is_none() && out.is_error.is_none());
    }
}

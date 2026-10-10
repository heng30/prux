//! Program Status Protocol（OSC 7501）：程序告诉终端自己是空闲、工作中、在等用户、完成还是失败。
//!
//! 协议规范见 <https://www.superlogical.com/rex/docs/build/program-status>，仅支持根记录。
//! 上报内容只有会话名、对话框标题与错误首行，**绝不包含 prompt 或模型输出**。
//! 终端只有在应答过支持查询后才会上报；`PRUX_PROGRAM_STATUS=1|0` 可跳过查询强制开/关。

use crate::utils::terminal_colors;
use base64::Engine as _;
use serde_json::Value;
use std::time::Duration;
use strum_macros::IntoStaticStr;

/// 支持查询序列：支持该协议的终端会原样回一个同样的序列。
pub(crate) const PROGRAM_STATUS_QUERY: &str = "\x1b]7501;?\x1b\\";

/// 支持查询的等待超时（毫秒）；超时按「不支持」处理，不阻塞启动。
const QUERY_TIMEOUT_MS: u64 = 100;

/// `PRUX_PROGRAM_STATUS`：`1` 强制上报（不查询）、`0` 关闭上报，其余（含未设置）按终端应答决定。
const ENV_OVERRIDE: &str = "PRUX_PROGRAM_STATUS";

/// `app` 字段的合法形状：`[A-Za-z0-9_.+-]{1,32}`，不合法时整个字段省略。
const MAX_APP_LEN: usize = 32;

/// `msg` 解码后的字节上限；其 base64 编码仍在规范的 2732 字节编码上限内。
const MAX_MESSAGE_BYTES: usize = 2048;

/// 退出时撤销已上报状态的序列（终端据此清掉状态栏）。
pub const PROGRAM_STATUS_CLEAR: &str = "\x1b]7501;state=clear\x1b\\";

/// OSC 7501 报告的状态取值；与协议字符串的互转由 `strum` 派生（全小写，见 [`ProgramState::as_str`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum ProgramState {
    /// 没有任何运行在进行。
    Idle,
    /// agent 回合或上下文压缩进行中。
    Working,
    /// 有对话框或登录在等用户操作。
    Blocked,
    /// 一个回合正常结束。
    Done,
    /// 一个回合以不再重试的错误结束。
    Error,
}

impl ProgramState {
    /// 协议里的状态字段取值。
    fn as_str(self) -> &'static str {
        self.into()
    }
}

/// `blocked` 状态在等什么；其它状态不带该字段；与协议字符串的互转由 `strum` 派生（全小写）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum BlockedKind {
    /// 等用户授权（权限确认）。
    Permission,
    /// 等用户回答问题（选择 / 输入对话框）。
    Question,
    /// 等用户完成登录。
    Auth,
}

impl BlockedKind {
    /// 协议里的 `kind` 字段取值。
    fn as_str(self) -> &'static str {
        self.into()
    }
}

/// 一条待上报的程序状态；`app` / `kind` / `message` 缺失时对应字段不出现在序列里。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramStatus {
    /// 要上报的状态。
    pub state: ProgramState,
    /// 稳定的程序名（`[A-Za-z0-9_.+-]{1,32}`）；不合法时字段被省略。
    pub app: Option<String>,
    /// 仅 `blocked` 状态携带的等待原因。
    pub kind: Option<BlockedKind>,
    /// 一行人类可读说明；控制字符替换为空格，超长按字节截断。
    pub message: Option<String>,
}

/// 某个对话框占用的阻塞状态（pi 的 `BlockedStatus` 等价物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedStatus {
    /// 在等什么。
    pub kind: BlockedKind,
    /// 对话框标题（上报为一行说明）。
    pub message: String,
}

impl BlockedStatus {
    /// 组装一条阻塞状态。
    pub fn new(kind: BlockedKind, message: impl Into<String>) -> Self {
        BlockedStatus {
            kind,
            message: message.into(),
        }
    }
}

/// `app` 字段是否满足协议规定的字符集与长度。
fn is_valid_app(app: &str) -> bool {
    !app.is_empty()
        && app.len() <= MAX_APP_LEN
        && app
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '+' | '-'))
}

/// 把连续控制字符折成一个空格（终端会丢弃含控制字符的上报，所以必须替换掉）。
fn replace_control_characters(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_run = false;
    for ch in text.chars() {
        if ch.is_control() {
            if !in_run {
                out.push(' ');
            }
            in_run = true;
        } else {
            out.push(ch);
            in_run = false;
        }
    }
    out
}

/// 按 UTF-8 字节数截断，不切断字符；已在限制内时原样返回。
fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = 0;
    for (idx, ch) in text.char_indices() {
        if idx + ch.len_utf8() > max_bytes {
            break;
        }
        end = idx + ch.len_utf8();
    }
    &text[..end]
}

/// 把一条状态编码成 OSC 7501 序列；`msg` 以 base64 承载。
pub fn format_program_status(status: &ProgramStatus) -> String {
    let mut pairs = vec![format!("state={}", status.state.as_str())];
    if let Some(app) = status.app.as_deref()
        && is_valid_app(app)
    {
        pairs.push(format!("app={app}"));
    }

    if status.state == ProgramState::Blocked
        && let Some(kind) = status.kind
    {
        pairs.push(format!("kind={}", kind.as_str()));
    }

    let sanitized = replace_control_characters(status.message.as_deref().unwrap_or(""));
    let message = truncate_utf8(sanitized.trim(), MAX_MESSAGE_BYTES);
    if !message.is_empty() {
        pairs.push(format!(
            "msg={}",
            base64::engine::general_purpose::STANDARD.encode(message)
        ));
    }
    format!("\x1b]7501;{}\x1b\\", pairs.join(":"))
}

/// 一条 OSC 回复体（`ESC ]` 与结束符之间）是否为 OSC 7501 支持应答。
///
/// 协议后续修订可能在 `?` 后追加键值对，所以只校验前缀。
pub fn is_program_status_reply_body(body: &str) -> bool {
    body.starts_with("7501;?")
}

/// 本终端是否上报 OSC 7501：`PRUX_PROGRAM_STATUS=1` 直接为真、`0` 直接为假，否则向终端查询一次。
///
/// 查询会短暂读写 stdin（开启 raw 模式），因此必须在 TUI 事件流建立**之前**调用；
/// 非 TTY / 非 Unix / 终端无应答一律按不支持处理。
pub fn detect_program_status_support() -> bool {
    match std::env::var(ENV_OVERRIDE).ok().as_deref() {
        Some("1") => return true,
        Some("0") => return false,
        _ => {}
    }

    // 支持应答排在 DA1 哨兵之前，因此写完两个序列后读到 DA1 即结束
    let query = format!("{PROGRAM_STATUS_QUERY}\x1b[c");
    let bytes =
        terminal_colors::query_terminal_replies(&query, Duration::from_millis(QUERY_TIMEOUT_MS));
    let mut buf = bytes;
    while let Some(reply) = terminal_colors::take_reply(&mut buf) {
        if let terminal_colors::Reply::Osc(body) = reply
            && is_program_status_reply_body(&body)
        {
            return true;
        }
    }
    false
}

/// 把 agent 会话事件与对话框状态翻译成 OSC 7501 上报。
///
/// `run_active` / `compacting` 决定是否 `working`，
/// 最近一条 assistant 响应决定回合结果，`agent_settled` 把回合结果落到静止状态。
pub struct ProgramStatusReporter {
    /// 终端是否上报（无支持时 `report()` 只做去重，不产生待写序列）。
    supported: bool,
    /// 是否处于一个正在运行的回合中。
    run_active: bool,
    /// 是否正在压缩上下文。
    compacting: bool,
    /// 当前回合的结果，回合结束后落到 `resting_status`。
    run_result: ProgramStatus,
    /// 没有回合在跑时的状态。
    resting_status: ProgramStatus,
    /// 当前是否有对话框在等用户。
    blocked: Option<BlockedStatus>,
    /// 会话名（`working` / `done` 上报的说明文字）；未命名为 None。
    session_name: Option<String>,
    /// 上次真正写出的状态键，用于去重（相同状态不重复写终端）。
    last_report: Option<String>,
    /// 待写入终端的序列；由调用方取走并写出。
    pending: Option<String>,
}

impl ProgramStatusReporter {
    /// 按支持情况新建翻译器；`supported` 为假时所有上报都被丢弃。
    pub fn new(supported: bool) -> Self {
        ProgramStatusReporter {
            supported,
            run_active: false,
            compacting: false,
            run_result: ProgramStatus {
                state: ProgramState::Done,
                app: None,
                kind: None,
                message: None,
            },
            resting_status: ProgramStatus {
                state: ProgramState::Idle,
                app: None,
                kind: None,
                message: None,
            },
            blocked: None,
            session_name: None,
            last_report: None,
            pending: None,
        }
    }

    /// 本终端是否上报 OSC 7501。
    pub fn supported(&self) -> bool {
        self.supported
    }

    /// 记录会话名（会话切换 / `session_info_changed` 时更新）。
    pub fn set_session_name(&mut self, name: Option<String>) {
        self.session_name = name.filter(|n| !n.is_empty());
    }

    /// 当前是否有对话框在等用户；由调用方从当前面板派生。
    pub fn set_blocked(&mut self, status: Option<BlockedStatus>) {
        self.blocked = status;
    }

    /// 消费一个 agent 会话事件并重算状态。
    pub fn handle_event(&mut self, event: &Value) {
        match event.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "agent_start" => {
                self.run_active = true;
                self.run_result = plain_status(ProgramState::Done);
            }
            "message_end" => {
                // 最新一条响应决定结果：被重试的错误会被随后成功的响应覆盖
                let Some(message) = event.get("message") else {
                    return;
                };

                if message.get("role").and_then(|v| v.as_str()) != Some("assistant") {
                    return;
                }

                self.run_result =
                    if message.get("stopReason").and_then(|v| v.as_str()) == Some("error") {
                        ProgramStatus {
                            state: ProgramState::Error,
                            app: None,
                            kind: None,
                            message: Some(first_line(
                                message.get("errorMessage").and_then(|v| v.as_str()),
                            )),
                        }
                    } else {
                        plain_status(ProgramState::Done)
                    };
            }
            "compaction_start" => self.compacting = true,
            "compaction_end" => {
                self.compacting = false;
                let aborted = event
                    .get("aborted")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let error = event.get("errorMessage").and_then(|v| v.as_str());
                let manual = event.get("reason").and_then(|v| v.as_str()) == Some("manual");
                if self.run_active {
                    // 恢复性压缩失败会结束回合，除非随后的响应成功
                    if aborted {
                        self.run_result = plain_status(ProgramState::Idle);
                    } else if let Some(error) = error {
                        self.run_result = error_status(error);
                    }
                } else if aborted {
                    self.resting_status = plain_status(ProgramState::Idle);
                } else if manual {
                    self.resting_status = match error {
                        Some(error) => error_status(error),
                        None => plain_status(ProgramState::Done),
                    };
                }
            }
            "agent_settled" => {
                self.run_active = false;
                let aborted = event
                    .get("aborted")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                self.resting_status = if aborted {
                    plain_status(ProgramState::Idle)
                } else {
                    self.run_result.clone()
                };
            }
            "session_info_changed" => {
                self.set_session_name(
                    event
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                );
            }
            _ => return,
        }
        self.report();
    }

    /// 忘掉上一个会话的回合状态（会话切换后调用）。
    pub fn reset(&mut self) {
        self.run_active = false;
        self.compacting = false;
        self.run_result = plain_status(ProgramState::Done);
        self.resting_status = plain_status(ProgramState::Idle);
        self.blocked = None;
        self.report();
    }

    /// 重算并（必要时）排入一条待写序列；与上次相同则什么都不做。
    pub fn report(&mut self) {
        let status = self.current_status();
        let key = format!("{status:?}");
        if self.last_report.as_deref() == Some(key.as_str()) {
            return;
        }

        self.last_report = Some(key);
        if self.supported {
            self.pending = Some(format_program_status(&status));
        }
    }

    /// 取走待写入终端的最新序列（同一帧内多次上报只保留最后一条）。
    pub fn take_pending(&mut self) -> Option<String> {
        self.pending.take()
    }

    /// 退出时的撤销序列；从未上报过（或终端不支持）时为 `None`。
    pub fn exit_sequence(&self) -> Option<&'static str> {
        (self.supported && self.last_report.is_some()).then_some(PROGRAM_STATUS_CLEAR)
    }

    /// 当前状态：按「对话框 > 压缩 > 回合中 > 静止」的优先级合成说明文字。
    fn current_status(&self) -> ProgramStatus {
        if let Some(blocked) = &self.blocked {
            return ProgramStatus {
                state: ProgramState::Blocked,
                app: None,
                kind: Some(blocked.kind),
                message: Some(blocked.message.clone()),
            };
        }

        if self.compacting {
            return ProgramStatus {
                state: ProgramState::Working,
                app: None,
                kind: None,
                message: Some("Compacting context".to_string()),
            };
        }

        let status = if self.run_active {
            plain_status(ProgramState::Working)
        } else {
            self.resting_status.clone()
        };

        // 工作中与完成都把会话名作为说明
        if matches!(status.state, ProgramState::Working | ProgramState::Done) {
            return ProgramStatus {
                message: self.session_name.clone(),
                ..status
            };
        }
        status
    }
}

/// 一个不带说明的状态。
fn plain_status(state: ProgramState) -> ProgramStatus {
    ProgramStatus {
        state,
        app: None,
        kind: None,
        message: None,
    }
}

/// 带错误首行说明的 `error` 状态。
fn error_status(message: &str) -> ProgramStatus {
    ProgramStatus {
        state: ProgramState::Error,
        app: None,
        kind: None,
        message: Some(first_line(Some(message))),
    }
}

/// 取文本首行并去空白；空或缺失时回落到 `Error`。
fn first_line(text: Option<&str>) -> String {
    let line = text.unwrap_or("").lines().next().unwrap_or("").trim();
    if line.is_empty() {
        "Error".to_string()
    } else {
        line.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 解码 OSC 7501 序列里的 `msg=` 字段（测试断言用）。
    fn decoded_message(sequence: &str) -> Option<String> {
        let body = sequence
            .strip_prefix("\x1b]7501;")?
            .strip_suffix("\x1b\\")?;
        for pair in body.split(':') {
            if let Some(encoded) = pair.strip_prefix("msg=") {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(encoded)
                    .ok()?;
                return String::from_utf8(bytes).ok();
            }
        }
        None
    }

    #[test]
    fn formats_state_and_message_fields() {
        let status = ProgramStatus {
            state: ProgramState::Working,
            app: Some("prux".to_string()),
            kind: None,
            message: Some("my session".to_string()),
        };
        let wire = format_program_status(&status);
        assert!(wire.starts_with("\x1b]7501;state=working:app=prux:msg="));
        assert!(wire.ends_with("\x1b\\"));
        assert_eq!(decoded_message(&wire).as_deref(), Some("my session"));
    }

    #[test]
    fn blocked_carries_kind_and_omits_it_elsewhere() {
        let blocked = ProgramStatus {
            state: ProgramState::Blocked,
            app: Some("prux".to_string()),
            kind: Some(BlockedKind::Auth),
            message: Some("Log in to Anthropic".to_string()),
        };
        assert!(format_program_status(&blocked).contains("state=blocked:app=prux:kind=auth:msg="));

        let done = ProgramStatus {
            state: ProgramState::Done,
            app: None,
            kind: Some(BlockedKind::Auth),
            message: None,
        };
        assert_eq!(format_program_status(&done), "\x1b]7501;state=done\x1b\\");
    }

    #[test]
    fn invalid_app_names_are_omitted() {
        for app in ["", "has space", "way-too-long-name-for-the-protocol-field"] {
            let status = ProgramStatus {
                state: ProgramState::Idle,
                app: Some(app.to_string()),
                kind: None,
                message: None,
            };
            assert_eq!(
                format_program_status(&status),
                "\x1b]7501;state=idle\x1b\\",
                "app={app:?} should be dropped"
            );
        }
        let ok = ProgramStatus {
            state: ProgramState::Idle,
            app: Some("prux-cli.dev+1".to_string()),
            kind: None,
            message: None,
        };
        assert!(format_program_status(&ok).contains(":app=prux-cli.dev+1"));
    }

    #[test]
    fn control_characters_become_one_space_and_text_is_trimmed() {
        let status = ProgramStatus {
            state: ProgramState::Error,
            app: None,
            kind: None,
            message: Some("  bad\u{7}\u{1b}[0m\nthing  ".to_string()),
        };
        assert_eq!(
            decoded_message(&format_program_status(&status)).as_deref(),
            Some("bad [0m thing")
        );
    }

    #[test]
    fn long_messages_are_truncated_on_a_character_boundary() {
        // 每个汉字 3 字节：2048 字节能放下 682 个整字
        let status = ProgramStatus {
            state: ProgramState::Error,
            app: None,
            kind: None,
            message: Some("汉".repeat(1000)),
        };
        let decoded = decoded_message(&format_program_status(&status)).unwrap();
        assert!(decoded.len() <= MAX_MESSAGE_BYTES);
        assert_eq!(decoded.chars().count(), 682);
    }

    #[test]
    fn reply_detection_accepts_spec_revisions() {
        assert!(is_program_status_reply_body("7501;?"));
        assert!(is_program_status_reply_body("7501;?version=2"));
        assert!(!is_program_status_reply_body("7501;state=idle"));
        assert!(!is_program_status_reply_body("10;#ffffff"));
    }

    #[test]
    fn reporter_tracks_run_outcome_and_dedups() {
        let mut reporter = ProgramStatusReporter::new(true);
        reporter.set_session_name(Some("demo".to_string()));

        reporter.handle_event(&json!({ "type": "agent_start" }));
        let write = reporter.take_pending().unwrap();
        assert_eq!(decoded_message(&write).as_deref(), Some("demo"));
        assert!(write.contains("state=working"));

        // 同状态重复上报不产生新的写入
        reporter.report();
        assert!(reporter.take_pending().is_none());

        reporter.handle_event(&json!({
            "type": "message_end",
            "message": { "role": "assistant", "stopReason": "error", "errorMessage": "boom\nsecond" }
        }));
        reporter.handle_event(&json!({ "type": "agent_settled", "aborted": false }));
        let write = reporter.take_pending().unwrap();
        assert!(write.contains("state=error"));
        assert_eq!(decoded_message(&write).as_deref(), Some("boom"));
    }

    #[test]
    fn aborted_run_rests_at_idle() {
        let mut reporter = ProgramStatusReporter::new(true);
        reporter.handle_event(&json!({ "type": "agent_start" }));
        _ = reporter.take_pending();
        reporter.handle_event(&json!({ "type": "agent_settled", "aborted": true }));
        let write = reporter.take_pending().unwrap();
        assert!(write.contains("state=idle"), "{write}");
    }

    #[test]
    fn blocked_dialog_wins_over_working() {
        let mut reporter = ProgramStatusReporter::new(true);
        reporter.handle_event(&json!({ "type": "agent_start" }));
        _ = reporter.take_pending();
        reporter.set_blocked(Some(BlockedStatus::new(BlockedKind::Question, "Pick one")));
        reporter.report();
        let write = reporter.take_pending().unwrap();
        assert!(write.contains("state=blocked:kind=question"), "{write}");
        assert_eq!(decoded_message(&write).as_deref(), Some("Pick one"));

        reporter.set_blocked(None);
        reporter.report();
        let write = reporter.take_pending().unwrap();
        assert!(write.contains("state=working"), "{write}");
    }

    #[test]
    fn exit_sequence_only_after_a_report() {
        let mut reporter = ProgramStatusReporter::new(true);
        assert!(reporter.exit_sequence().is_none());
        reporter.handle_event(&json!({ "type": "agent_start" }));
        assert_eq!(reporter.exit_sequence(), Some(PROGRAM_STATUS_CLEAR));

        let mut unsupported = ProgramStatusReporter::new(false);
        unsupported.handle_event(&json!({ "type": "agent_start" }));
        assert!(unsupported.exit_sequence().is_none());
    }

    #[test]
    fn unsupported_terminal_never_writes() {
        let mut reporter = ProgramStatusReporter::new(false);
        reporter.handle_event(&json!({ "type": "agent_start" }));
        assert!(reporter.take_pending().is_none());
        assert!(!reporter.supported());
    }

    #[test]
    fn compaction_reports_working_and_manual_result() {
        let mut reporter = ProgramStatusReporter::new(true);
        reporter.set_session_name(Some("demo".to_string()));
        reporter.handle_event(&json!({ "type": "compaction_start" }));
        let write = reporter.take_pending().unwrap();
        assert!(write.contains("state=working"));
        assert_eq!(
            decoded_message(&write).as_deref(),
            Some("Compacting context")
        );

        reporter.handle_event(&json!({
            "type": "compaction_end",
            "reason": "manual",
            "errorMessage": null
        }));
        let write = reporter.take_pending().unwrap();
        assert!(write.contains("state=done"), "{write}");
        assert_eq!(decoded_message(&write).as_deref(), Some("demo"));
    }
}

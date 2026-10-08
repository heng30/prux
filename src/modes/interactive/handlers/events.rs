//! agent 事件 → `App` 状态：worker 回执（`on_*`）与流式事件分发（`apply_sink_*`）。
//!
//! worker 线程把回执/事件经 `run_in_event_loop` 投到 UI 线程，最终都落到这里：
//! - `on_*`：命令回执（模型变更、会话切换、压缩完成、导航、统计、提示…）；
//! - `apply_sink_event`：agent 事件流（message_start/update/end、tool、turn_end、压缩、错误…）
//!   的分发入口，也是扩展收到 agent 事件的转发点；
//! - `apply_sink_*`：各事件的落地实现（模块内部）。
//!
//! 回执载荷类型（`ModelChanged` / `SessionSwitched` / `AppliedKey` / `SessionStats` …）
//! 与 `App` 同处 app.rs。

use super::super::{
    app::{
        App, AppliedKey, ModelChanged, MsgLevel, NavigateDone, RewindDone, SessionStats,
        SessionSwitched, SysSpan, WorkingKind,
    },
    overlay,
};
use crate::{
    core::{self, compaction, login_registry, model_refresh, provider::AgentMessage},
    extensions::footer,
    modes::interactive::render,
    utils::{
        display::{format_cost, format_thousands},
        glyphs::DEF_DONE,
        tokens::{self, TokenMeter},
    },
};
use serde_json::Value;

impl App {
    /// /thinking 命令回执：同步当前 thinking 级别镜像（worker 类型化直达，不经 JSON sink）。
    /// `level` 为模型钳制后的当前级别；`default_level` 为 Some 时（/thinking 面板 Ctrl+S）
    /// 表示已按请求级别写盘为默认值，同步 App 缓存并在状态栏提示。输出系统提示
    /// `Thinking level: X`；循环切换（Shift+Tab）连续触发时经 push_msg_dedup 原地替换。
    pub fn on_thinking_level_changed(&mut self, level: String, default_level: Option<String>) {
        self.thinking_level = Some(level.clone());

        if let Some(level) = default_level {
            self.default_thinking_level = Some(level.clone());
            self.set_status_msg(format!("Default thinking level: {}", level));
        }

        self.push_msg_dedup(
            format!("Thinking level: {}", level),
            "Thinking level: ",
            MsgLevel::Info,
        );
    }

    /// 同步模型镜像：/model 切换与 /login 自动改选模型共用（footer 显示、/model 勾选都读这里）
    fn apply_model_mirror(&mut self, m: &ModelChanged) {
        self.current_model = Some(m.model.clone());
        self.no_model_available = false;
        self.current_provider = Some(m.provider.clone());
        self.model_reasoning = m.reasoning;
        self.thinking_level = m.thinking_level.clone();
        self.thinking_levels = m.thinking_levels.clone();
        // 切换模型后旧的路由结果不再适用（footer 的 `→ ...` 立即清空）
        self.clear_routed_model();
    }

    /// 清空虚拟模型的路由显示（agent_start / 模型切换时）。
    pub fn clear_routed_model(&mut self) {
        self.routed_model = None;
        self.routed_thinking_level = None;
        self.dirty = true;
    }

    /// `model_routed`：虚拟模型本次请求路由到的物理模型（footer 显示）。
    fn apply_sink_model_routed(&mut self, event: &Value) {
        self.routed_model = event
            .get("model")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        self.routed_thinking_level = event
            .get("thinkingLevel")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        self.dirty = true;
    }

    /// /model 回执：成功更新 App 模型镜像并提示；失败错误提示。
    pub fn on_model_changed(&mut self, result: Result<ModelChanged, String>) {
        self.busy = false;
        match result {
            Ok(m) => {
                self.apply_model_mirror(&m);

                // Ctrl+S 写盘默认模型：更新 App 缓存并状态栏提示（与切换提示并存）
                if m.persisted {
                    self.default_provider = Some(m.provider.clone());
                    self.default_model = Some(m.model.clone());
                    self.set_status_msg(format!("Default model: {}/{}", m.provider, m.model));
                }

                self.push_msg_dedup(
                    format!("Switched to {} ({})", m.name, m.provider),
                    "Switched to ",
                    MsgLevel::Success,
                );
            }
            Err(e) => self.push_msg(format!("switch failed: {}", e), MsgLevel::Error),
        }
    }

    /// 会话切换：（/new /resume /import /fork /clone 均汇合于此）
    /// 回执：替换消息流与会话路径；恢复会话时清旧提示。
    pub fn on_session_switched(&mut self, result: Result<SessionSwitched, String>) {
        self.busy = false;
        match result {
            Ok(s) => {
                self.dock_visible = false;
                self.dock_offset = 0;
                self.dock_area = None;
                overlay::close(self); // 覆盖层属于某个会话的子代理：切会话先告知扩展关闭
                self.ext_settings.close();
                self.banner_hidden = true;
                self.messages = s.messages;
                self.expand_overrides.clear(); // 切会话：逐块展开态不跨 transcript
                self.context_tokens_override = None;
                self.current_session_path = s.file.clone();
                self.refresh_context_percent();

                core::extensions::dispatch_session_switched(s.file.as_deref(), &self.messages);

                let has_file = s.file.as_deref().map(|f| !f.is_empty()).unwrap_or(false);
                if has_file {
                    self.system_messages.clear();
                    self.set_status_msg("Resumed session");
                }
                self.invalidate_render();
            }
            Err(e) => self.push_msg(format!("failed to open session: {}", e), MsgLevel::Error),
        }
    }

    /// /compact 回执：`Ok(Some(summary))` = 成功（输出成功提示，携带压缩前后 token 数），
    /// `Ok(None)` = 跳过（Nothing to compact），`Err` = 失败。
    pub fn on_compact_done(&mut self, result: Result<Option<Value>, String>) {
        self.busy = false;
        match result {
            Ok(Some(summary)) => {
                let before = summary
                    .get("tokensBefore")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let after = summary
                    .get("estimatedTokensAfter")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                self.push_msg(
                    format!(
                        "{DEF_DONE} Context compacted ({} → {} tokens)",
                        before, after
                    ),
                    MsgLevel::Success,
                );
            }
            Ok(None) => self.push_msg(
                "Nothing to compact (session too small)".to_string(),
                MsgLevel::Info,
            ),
            Err(e) => self.push_msg(format!("compaction failed: {}", e), MsgLevel::Error),
        }
    }

    /// 会话树导航回执：命中分支时回填编辑器与消息流。
    pub fn on_navigate_done(&mut self, result: Result<NavigateDone, String>) {
        self.busy = false;
        match result {
            Ok(n) => {
                if let Some(text) = n.editor_text.filter(|t| !t.is_empty()) {
                    self.editor.clear();
                    self.editor.insert_text(&text);
                }
                if !n.messages.is_empty() {
                    self.messages = n.messages;
                    self.expand_overrides.clear(); // 树导航：逐块展开态不跨分支
                    self.context_tokens_override = None;
                    self.refresh_context_percent();
                    self.invalidate_render();
                }
            }
            Err(e) => self.push_msg(format!("navigate failed: {}", e), MsgLevel::Error),
        }
    }

    /// `/rewind` 回执：回退成功时用会话投影重建消息流（可清空），并按请求回填/丢弃输入。
    pub fn on_rewind_done(&mut self, result: Result<RewindDone, String>) {
        let restore = std::mem::take(&mut self.pending_rewind_restore);
        match result {
            Ok(done) => match done.removed_input {
                Some(text) => {
                    // 回退可能清空整条分支（回退第一条 user 消息）：无条件替换消息流。
                    self.messages = done.messages;
                    self.expand_overrides.clear(); // 逐块展开态不跨分支
                    self.context_tokens_override = None;
                    self.editor.clear();

                    if restore && !text.is_empty() {
                        self.editor.insert_text(&text);
                    }

                    self.scroll = 0;
                    self.max_scroll = 0;
                    self.last_render_total = 0;
                    self.refresh_context_percent();
                    self.invalidate_render();
                    self.push_msg(
                        "Rewound last user input: it and all later output were removed from the context (history remains in the session tree)."
                            .to_string(),
                        MsgLevel::Success,
                    );
                }
                None => self.push_msg(
                    "Nothing to rewind: no user input in the current session.".to_string(),
                    MsgLevel::Info,
                ),
            },
            Err(e) => self.push_msg(format!("rewind failed: {e}"), MsgLevel::Error),
        }
    }

    /// /login 应用凭据回执：自动改选模型时同步镜像并提示。
    pub fn on_applied_key(&mut self, k: AppliedKey) {
        if let Some(m) = k.applied {
            self.apply_model_mirror(&m);

            // 登录兜底选模型会写默认值（persist=true）：同步 App 默认缓存（不弹状态栏）
            if m.persisted {
                self.default_provider = Some(m.provider.clone());
                self.default_model = Some(m.model.clone());
            }

            self.push_msg(
                format!(
                    "model selected for the signed-in provider: {} ({})",
                    m.model, m.provider
                ),
                MsgLevel::Info,
            );
        }
    }

    /// 扩展工具重建回执
    pub fn on_tools_rebuilt(&mut self) {
        self.push_msg(
            "extensions updated: tools rebuilt".to_string(),
            MsgLevel::Success,
        );
    }

    /// /session 统计渲染（worker 传原始数值，格式化在此进行）
    pub fn on_session_stats(&mut self, stats: SessionStats) {
        let mut spans: Vec<SysSpan> = Vec::new();
        let head = |spans: &mut Vec<SysSpan>, t: &str| {
            spans.push(SysSpan {
                fg: Some("text".to_string()),
                bold: true,
                text: format!("{}\n", t),
            });
        };
        let kv = |spans: &mut Vec<SysSpan>, label: &str, value: String, trail: &str| {
            spans.push(SysSpan::fg("dim", &format!("{}:", label)));
            spans.push(SysSpan::fg("text", &format!(" {}{}", value, trail)));
        };

        head(&mut spans, "Session Info");
        spans.push(SysSpan::plain("\n"));
        if let Some(name) = stats.name.as_deref().filter(|n| !n.is_empty()) {
            kv(&mut spans, "Name", name.to_string(), "\n");
        }
        kv(&mut spans, "File", stats.file.clone(), "\n");
        kv(&mut spans, "ID", stats.id.clone(), "\n\n");
        head(&mut spans, "Messages");
        kv(
            &mut spans,
            "Total",
            format_thousands(stats.messages_total),
            "\n",
        );
        kv(
            &mut spans,
            "Tool calls",
            format!("{}", stats.tool_calls),
            "\n\n",
        );
        head(&mut spans, "Tokens");
        kv(
            &mut spans,
            "Input",
            format_thousands(stats.prompt_tokens),
            "\n",
        );
        kv(&mut spans, "Output", format!("{}", stats.output), "\n");
        kv(
            &mut spans,
            "Total",
            format_thousands(stats.total_tokens),
            "\n",
        );
        spans.push(SysSpan {
            fg: Some("text".to_string()),
            bold: true,
            text: "\nCost\n".to_string(),
        });

        // Cost 块：合计 +（可选）按 provider/model 分列。
        // 只有一条且正好是当前选中的模型时省略分列（否则它只是把合计重写一遍）。
        let selected = match (&self.current_provider, &self.current_model) {
            (Some(p), Some(m)) => Some(format!("{p}/{m}")),
            _ => None,
        };
        let show_breakdown = stats.cost_breakdown.len() > 1
            || stats
                .cost_breakdown
                .first()
                .is_some_and(|e| Some(&e.key) != selected.as_ref());

        kv(
            &mut spans,
            "Total",
            format!("${:.3}", stats.cost_total),
            if show_breakdown { "\n" } else { "\n\n" },
        );

        if show_breakdown {
            let last = stats.cost_breakdown.len() - 1;
            for (i, entry) in stats.cost_breakdown.iter().enumerate() {
                spans.push(SysSpan::fg("dim", &format!("  {}:", entry.key)));
                spans.push(SysSpan::fg("text", &format!(" ${:.3}", entry.cost)));
                spans.push(SysSpan::fg(
                    "dim",
                    &format!(
                        " ({} tokens){}",
                        footer::format_tokens(entry.tokens),
                        if i == last { "\n\n" } else { "\n" }
                    ),
                ));
            }
        }

        // 提示缓存保温（`/session` 的 `Cache Warming` 块）；块前留空行（靠上一条的 `\n\n`）
        let cw = &stats.cache_warming;
        head(&mut spans, "Cache Warming");
        kv(&mut spans, "Mode", cw.mode.clone(), "\n");
        kv(&mut spans, "Status", cw.detail.clone(), "\n");
        if let Some(e) = &cw.economics
            && e.economics_available
        {
            kv(
                &mut spans,
                "Cache miss penalty",
                format!("${:.3}", e.miss_cost),
                "\n",
            );
            kv(
                &mut spans,
                "Refresh cost",
                format!("${:.3}", e.warm_cost),
                "\n",
            );
        }
        if cw.warmed_count > 0 {
            kv(
                &mut spans,
                "Warmed",
                format!(
                    "{} time(s), ${}",
                    cw.warmed_count,
                    format_cost(cw.total_warmed_cost)
                ),
                "\n",
            );
        }
        self.push_msg_rich(spans, MsgLevel::Info);
    }

    /// 缓存保温回执（worker `drain_cache_warm` 发的 `cache_warm` 事件）。
    /// 仅当 settings `showCacheMissNotices` 开启时提示。
    pub fn apply_sink_cache_warm(&mut self, event: &Value) {
        if !core::settings_manager::read_settings_show_cache_miss_notices() {
            return;
        }

        let cost = event.get("cost").and_then(|v| v.as_f64()).unwrap_or(0.0);
        let note = if event
            .get("extensionOverride")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            " (extension override)"
        } else {
            ""
        };

        self.push_msg(
            format!("Cache warmed{}: ${}", note, format_cost(cost)),
            MsgLevel::Info,
        );
    }

    /// 通用命令提示回执（worker 已组装文本，UI 按 ok 决定级别）
    pub fn on_command_notice(&mut self, ok: bool, message: String) {
        if message.is_empty() {
            return;
        }
        let level = if ok { MsgLevel::Info } else { MsgLevel::Error };
        self.push_msg(message, level);
    }

    /// /share 回执：先复位忙碌态（spinner 退出），再展示结果提示。
    pub fn on_share_done(&mut self, ok: bool, message: String) {
        self.busy = false;
        self.working_kind = WorkingKind::Working;
        self.clear_status();
        self.on_command_notice(ok, message);
    }

    /// /bug 回执（worker 写完归档/失败）：先复位忙碌态，再展示结果提示。
    pub fn on_bug_report_done(&mut self, ok: bool, message: String) {
        self.busy = false;
        self.working_kind = WorkingKind::Working;
        self.clear_status();
        self.pending_bug_report = None;
        self.on_command_notice(ok, message);
    }

    /// /login 成功后后台刷新模型目录回传（RefreshModels 后台任务投闭包调用）：
    /// 更新面板状态消息（成功刷新静默、失败/无目录警告）。
    pub fn on_catalog_refresh(
        &mut self,
        provider: String,
        result: std::result::Result<model_refresh::RefreshStatus, String>,
    ) {
        let name = login_registry::provider_name(&provider);
        match result {
            Err(err) => {
                // 刷新失败：更新状态消息为错误信息（不关闭面板）
                self.refresh_status_message = format!(
                    "Could not refresh {}: {}; showing cached models.",
                    name, err
                );
                self.refresh_status_success = false;
            }
            Ok(
                model_refresh::RefreshStatus::Updated
                | model_refresh::RefreshStatus::Unchanged
                | model_refresh::RefreshStatus::Skipped,
            ) => {
                // 内容已更新/未变化/缓存窗内：均视为成功提示
                self.refresh_status_message = "Model catalogs refreshed.".to_string();
                self.refresh_status_success = true;
            }
            Ok(model_refresh::RefreshStatus::NotAvailable) => {
                self.refresh_status_message = format!(
                    "No remote model catalog for {}; using built-in models.",
                    name
                );
                self.refresh_status_success = false;
            }
        }
        self.dirty = true;
    }

    /// agent 流式事件统一消化入口（`attach_json_sink` 闭包子 → `apply_sink_event`）。
    /// 只处理 agent 核心 emit 的流式增量（message_* / tool_* / turn_end / error /
    /// auto_retry_* / agent_settled / thinking_level_changed）；worker 命令回执已类型化，经
    /// `run_in_event_loop` 闭包直接调用 `on_*` 处理函数，不再经过这里。
    /// `agent_end` 每次 `run_loop()` 尝试都发（重试/溢出恢复会多次），仅供扩展消费，
    /// 不驱动 UI 状态；整轮收尾以 `agent_settled` 为准。
    /// `thinking_level_changed` 例外：模型切换/迭代快照恢复等 agent 内部 emit 仍走 sink，
    /// 仅 SetThinking 命令回执经 `on_thinking_level_changed` 类型化直达。
    pub fn apply_sink_event(&mut self, event: &Value) {
        let ty = event.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // 把 agent 内核事件转发给扩展注册表。footer 扩展 rich 依赖 agent_start /
        // tool_execution_end / agent_end 驱动交互计数、工具计数与任务计时；
        // agent_settled 是扩展判定「真正空闲」的边界，先补上 UI 侧排队状态再转发。
        if ty == "agent_settled" {
            let enriched = self.enrich_agent_settled(event);
            core::extensions::dispatch_agent_event(&enriched);
        } else {
            core::extensions::dispatch_agent_event(event);
        }

        match ty {
            "agent_start" => self.clear_routed_model(),
            "message_start" => self.apply_sink_message_start(event),
            "message_update" => self.apply_sink_message_update(event),
            "message_end" => self.apply_sink_message_end(event),
            "tool_execution_start" => self.apply_sink_tool_execution_start(event),
            "tool_execution_end" => self.apply_sink_tool_execution_end(event),
            "turn_end" => self.apply_sink_turn_end(event),
            "compaction_end" => self.apply_sink_compaction_end(event),
            "error" => self.apply_sink_error(event),
            "auto_retry_start" => self.apply_sink_auto_retry_start(event),
            "auto_retry_end" => self.apply_sink_auto_retry_end(event),
            "thinking_level_changed" => self.apply_sink_thinking_level_changed(event),
            "model_routed" => self.apply_sink_model_routed(event),
            "agent_settled" => self.apply_sink_agent_settled(),
            "cache_warm" => self.apply_sink_cache_warm(event),
            _ => {}
        }
    }

    /// 同步模型镜像：/model 切换与 /login 自动改选模型共用（footer 显示、/model 勾选都读这里）
    fn apply_sink_message_start(&mut self, event: &Value) {
        if let Some(msg) = event.get("message") {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
            if role == "assistant" {
                self.hide_retry_notice();

                // 新 assistant 消息：清缓冲 + 按当前模型重选词表并归零计数
                let enc = tokens::encoding_for(self.current_model.as_deref());
                match &mut self.stream_meter {
                    Some(meter) => meter.set_encoding(enc),
                    None => self.stream_meter = Some(TokenMeter::new(enc)),
                }
                self.clear_streaming();
                self.tool_started_at.clear();
                // 记录流式锚点：本回合 assistant 输出应显示在此时消息列表之后
                self.stream_anchor_index = Some(self.messages.len());
            }
        }
    }

    /// message_update：按 assistantMessageEvent 子类型追加流式文本/思考/工具调用
    fn apply_sink_message_update(&mut self, event: &Value) {
        if let Some(ev) = event.get("assistantMessageEvent") {
            let et = ev.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match et {
                "text_delta" => {
                    if let Some(d) = ev.get("delta").and_then(|v| v.as_str()) {
                        self.streaming_text.push_str(d);
                        self.stream_meter_mut().feed(d);
                    }
                }
                "thinking_delta" => {
                    if let Some(d) = ev.get("delta").and_then(|v| v.as_str()) {
                        self.streaming_thinking.push_str(d);
                        self.stream_meter_mut().feed(d);
                    }
                }
                "toolcall_start" => {
                    if let Some(ci) = ev.get("contentIndex").and_then(|v| v.as_u64()) {
                        while self.streaming_tools.len() <= ci as usize {
                            self.streaming_tools.push(String::new());
                        }
                    }
                }
                "toolcall_delta" => {
                    if let Some(d) = ev.get("delta").and_then(|v| v.as_str())
                        && let Some(ci) = ev.get("contentIndex").and_then(|v| v.as_u64())
                    {
                        let idx = ci as usize;
                        if idx < self.streaming_tools.len() {
                            self.streaming_tools[idx].push_str(d);
                            self.stream_meter_mut().feed(d);
                        }
                    }
                }
                "toolcall_end" => {
                    if let Some(tc) = ev.get("toolCall") {
                        let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let args = tc
                            .get("arguments")
                            .map(|a| serde_json::to_string(a).unwrap_or_default())
                            .unwrap_or_default();
                        let summary = format!("{} {}", name, args);
                        self.streaming_tools.push(summary);
                    }
                }
                _ => {}
            }
        }
    }

    /// message_end：追加非 toolResult 消息（与末尾同角色同文本去重），清缓冲并刷新 context
    fn apply_sink_message_end(&mut self, event: &Value) {
        if let Some(msg) = event.get("message")
            && let Ok(m) = serde_json::from_value::<AgentMessage>(msg.clone())
        {
            // 下一条带可用 usage 的 assistant：其 usage 已是压缩后的真实值，
            // 压缩时的覆盖值失效，回到 usage 锚点估算（须在推入前判定——m 会被 move）
            let fresh_usage = compaction::usable_usage(&m).is_some();

            // toolResult 消息由 turn_end 的 toolResults 渲染，message_end 跳过避免重复
            // run_loop 会对用户提交的消息再次 emit message_end；
            // 若与 messages 末尾同角色同文本则跳过，避免流式期间重复显示
            if m.role != "toolResult" {
                let is_dup = m.role == "user" && {
                    self.messages
                        .last()
                        .map(|last| last.role == "user" && last.text() == m.text())
                        .unwrap_or(false)
                };

                if !is_dup {
                    self.messages.push(m);
                }
            }

            if fresh_usage {
                self.context_tokens_override = None;
            }

            self.clear_streaming();
            self.refresh_context_percent();
        }
    }

    /// tool_execution_start：记录工具启动时间（渲染 `Elapsed` 实时计时用）；
    /// 失效渲染缓存使工具块重建（注入 started_at）
    ///
    /// 带 `parentToolCallId` 的事件是嵌套调用（`codemode` 脚本调起的工具）：
    /// 不单独成块，追加到父工具块的嵌套列表（实时进度）。
    fn apply_sink_tool_execution_start(&mut self, event: &Value) {
        if let Some(parent) = event
            .get("parentToolCallId")
            .and_then(|v| v.as_str())
            .filter(|p| !p.is_empty())
        {
            let nested = self.nested_calls.entry(parent.to_string()).or_default();
            let name = event
                .get("toolName")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let summary = event
                .get("args")
                .map(render::messages::nested_arg_summary)
                .unwrap_or_default();
            nested.push(render::messages::NestedCallView {
                name,
                summary,
                is_error: None,
                duration_ms: None,
                started_at_ms: event.get("startedAtEpochMs").and_then(|v| v.as_u64()),
                cost: None,
            });
            self.invalidate_render();
            return;
        }

        if let Some(id) = event.get("toolCallId").and_then(|v| v.as_str())
            && let Some(started) = event.get("startedAtEpochMs").and_then(|v| v.as_u64())
        {
            self.tool_started_at.insert(id.to_string(), started);
            self.invalidate_render();
        }
    }

    /// tool_execution_end：工具完成，移除进行中计时（Took 由 turn_end 携带的 duration_ms 渲染）
    ///
    /// 嵌套调用结束时把对应行标记为完成（结果态由 `details.nestedCalls` 接管，
    /// 这里只在结果消息到达前给出即时反馈）。
    fn apply_sink_tool_execution_end(&mut self, event: &Value) {
        if let Some(parent) = event
            .get("parentToolCallId")
            .and_then(|v| v.as_str())
            .filter(|p| !p.is_empty())
            && event.get("toolCallId").and_then(|v| v.as_str()).is_some()
            && let Some(nested) = self.nested_calls.get_mut(parent)
            && let Some(call) = nested
                .iter_mut()
                .rev()
                .find(|c| c.is_error.is_none() && c.duration_ms.is_none())
        {
            call.is_error = Some(
                event
                    .get("isError")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
            );
            call.duration_ms = event.get("durationMs").and_then(|v| v.as_u64());
            call.started_at_ms = None;
            self.invalidate_render();
            return;
        }
        if let Some(id) = event.get("toolCallId").and_then(|v| v.as_str()) {
            self.tool_started_at.remove(id);
        }
    }

    /// turn_end：工具结果推入渲染历史、整体失效渲染缓存并刷新 context
    fn apply_sink_turn_end(&mut self, event: &Value) {
        // 状态区保持 Working... 直到 busy 结束（busy 由 prompt future 完成清除），
        // turn 完成后 Working 指示才停止，不得提前消失
        // 工具结果推入渲染历史（agent.rs 事件流中 turn_end 携带 toolResults）
        if let Some(tools) = event.get("toolResults").and_then(|v| v.as_array()) {
            for t in tools {
                if let Ok(m) = serde_json::from_value::<AgentMessage>(t.clone()) {
                    self.messages.push(m);
                }
            }
            // 工具结果可能填充任意历史 assistant 的工具块（按 toolCallId 匹配），
            // 必须整体失效渲染缓存，否则已缓存的 pending 状工具块不会更新为结果态
            self.invalidate_render();
            // 本轮工具全部落地，清空进行中计时表与嵌套进度（防御：tool_execution_end 已逐个移除）
            self.tool_started_at.clear();
            self.nested_calls.clear();
        }
        self.refresh_context_percent();
    }

    /// compaction_end：压缩完成（`result == "completed"`）后，上下文规模以事件携带的
    /// `estimatedTokensAfter`（摘要 + 保留尾部的估算）为准，覆盖 footer 的窗口使用量：
    /// 压缩摘要消息本身不带 usage，若不覆盖，估算会一直锚在压缩前那条 assistant 的
    /// 陈旧 usage 上，footer（rich/normal）的数字要等到下一条 assistant 落地才变。
    /// 覆盖值在下一条带可用 usage 的 assistant 落地、或消息流被整体替换时清除
    /// （见 [`App::context_tokens_override`]），之后自动回到 usage 锚点。
    /// 手动 `/compact`、阈值自动压缩、溢出恢复三条路径都经此事件。
    fn apply_sink_compaction_end(&mut self, event: &Value) {
        if event.get("result").and_then(|v| v.as_str()) != Some("completed") {
            return;
        }
        let Some(after) = event.get("estimatedTokensAfter").and_then(|v| v.as_u64()) else {
            return;
        };
        self.context_tokens_override = Some(after);
        self.refresh_context_percent();
    }

    /// error：错误文本转状态栏临时提示
    fn apply_sink_error(&mut self, event: &Value) {
        if let Some(e) = event.get("error").and_then(|v| v.as_str()) {
            self.set_status_msg(format!("error: {}", e));
        }
    }

    /// thinking_level_changed：同步当前 thinking 级别镜像
    fn apply_sink_thinking_level_changed(&mut self, event: &Value) {
        self.thinking_level = event["level"].as_str().map(|x| x.to_string());
    }

    /// 给 `agent_settled` 事件补上 `hasQueuedMessages`：结算时 steer 注入箱或
    /// follow-up 本地队列是否仍有内容。有内容 = 结算后会立刻起下一轮，并非真正空闲，
    /// 扩展（如 notify）据此判断是否值得发「已完成」通知。
    fn enrich_agent_settled(&self, event: &Value) -> Value {
        let has_queued = !self.runtime_followup_inbox.is_empty()
            || !self.runtime_steer_inbox.lock().unwrap().is_empty();
        let mut enriched = event.clone();
        if let Some(obj) = enriched.as_object_mut() {
            obj.insert("hasQueuedMessages".to_string(), Value::Bool(has_queued));
        }
        enriched
    }

    /// agent_settled：整轮（含重试/溢出恢复的所有 run_loop 尝试）真正结束的收尾边界——
    /// 复位忙碌态并取下一批排队 steer 继续下一轮。
    /// 不能用 agent_end：它每次 run_loop 尝试都会发，重试期间会被误判为整轮结束——
    /// 提前清 busy 会让 Esc/Ctrl+C 无法中断重试，并提前 drain 队列。
    fn apply_sink_agent_settled(&mut self) {
        self.busy = false;
        self.clear_streaming();
        self.clear_status();
        // 当前轮结束后：取下一批排队 steer（all=整批/one=一条），继续下一轮
        if let Some(batch) = super::super::drain_next_batch(self) {
            self.worker.prompt_batch(batch);
            self.busy = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::provider::{AgentMessage, ContentBlock},
        modes::interactive::app::{App, CostBreakdownEntry, sys_spans_text},
    };

    fn make_assistant(text: &str) -> AgentMessage {
        let mut m = AgentMessage::user_text(text);
        m.role = "assistant".to_string();
        m
    }

    /// /session 回执载荷（只关心成本相关的两个字段）
    fn base_stats(cost: f64, breakdown: Vec<CostBreakdownEntry>) -> SessionStats {
        SessionStats {
            name: None,
            file: "/tmp/s.json".into(),
            id: "s1".into(),
            messages_total: 2,
            tool_calls: 1,
            prompt_tokens: 1_000,
            total_tokens: 1_062,
            input: 1_000,
            output: 62,
            cache_read: 0,
            cache_write: 0,
            cost_total: cost,
            cost_breakdown: breakdown,
            cache_warming: crate::core::cache_warmer::WarmStatus {
                mode: "streaming".into(),
                detail: "Inactive (cache lifetime unavailable)".into(),
                ..Default::default()
            },
        }
    }

    /// 构造一条带 usage 的 assistant（usage 决定估算锚点值）
    fn assistant_with_usage(text: &str, ts: u64, total: u32) -> AgentMessage {
        let mut m = AgentMessage::user_text(text);
        m.role = "assistant".into();
        m.timestamp = ts;
        m.stop_reason = Some("stop".into());
        m.usage = Some(crate::core::provider::Usage {
            total_tokens: total,
            ..Default::default()
        });
        m
    }

    /// /session 成本分列：合计 + 每条 `provider/model: $c (N tokens)`（对齐 pi）
    #[test]
    fn session_stats_lists_cost_breakdown_by_model() {
        let mut app = App::new();
        app.current_provider = Some("openrouter".into());
        app.current_model = Some("auto".into());
        app.on_session_stats(base_stats(
            0.42,
            vec![
                CostBreakdownEntry {
                    key: "anthropic/claude-opus-5-5".into(),
                    cost: 0.30,
                    tokens: 1_200,
                },
                CostBreakdownEntry {
                    key: "Tools/summaries".into(),
                    cost: 0.12,
                    tokens: 3_000,
                },
            ],
        ));
        let text = sys_spans_text(&app.system_messages.last().unwrap().1.spans);
        assert!(
            text.contains(
                "Total: $0.420\n  anthropic/claude-opus-5-5: $0.300 (1.2k tokens)\n  \
                 Tools/summaries: $0.120 (3.0k tokens)\n\nCache Warming\n"
            ),
            "成本分列与 Cache Warming 块: {text:?}"
        );
    }

    /// 只有一条且正好是当前选中的模型时省略分列（它只是把合计重写一遍）——与 pi 同口径
    #[test]
    fn session_stats_omits_single_breakdown_for_selected_model() {
        let mut app = App::new();
        app.current_provider = Some("anthropic".into());
        app.current_model = Some("claude-sonnet-5-5".into());
        let entry = || CostBreakdownEntry {
            key: "anthropic/claude-sonnet-5-5".into(),
            cost: 0.42,
            tokens: 1_200,
        };
        app.on_session_stats(base_stats(0.42, vec![entry()]));
        let text = sys_spans_text(&app.system_messages.last().unwrap().1.spans);
        assert!(
            text.contains("Total: $0.420\n\nCache Warming\n"),
            "单条且命中当前模型：不分列: {text:?}"
        );

        // 同一份分列，但当前模型不是它（如网关路由到别的模型）：仍要分列
        app.current_model = Some("auto".into());
        app.on_session_stats(base_stats(0.42, vec![entry()]));
        let text = sys_spans_text(&app.system_messages.last().unwrap().1.spans);
        assert!(
            text.contains("Total: $0.420\n  anthropic/claude-sonnet-5-5: $0.420"),
            "当前模型不匹配时仍分列: {text:?}"
        );
    }

    #[test]
    fn session_stats_puts_blank_line_before_cache_warming_block() {
        // /session：`Cache Warming` 是一个独立块，前面必须留空行（与 Messages/Tokens/Cost 一致）
        let mut app = App::new();
        app.on_session_stats(base_stats(0.0, Vec::new()));

        let text = sys_spans_text(&app.system_messages.last().unwrap().1.spans);
        assert!(
            text.contains("Total: $0.000\n\nCache Warming\n"),
            "Cost 与 Cache Warming 之间应空行: {text:?}"
        );
        assert!(text.contains("Mode: streaming\n"), "{text:?}");
    }

    #[test]
    fn session_switch_hides_banner() {
        // `on_session_switched` 会经扩展注册表分发（subagent → manager::reset_all），
        // 必须与触碰 subagent 全局态的测试串行。
        let _sub = crate::extensions::subagent::test_lock();
        // banner 只在程序启动时的干净会话显示一次：/new（无提交）等会话切换不得让横幅重现
        let mut app = App::new();
        assert!(!app.banner_hidden);
        app.on_session_switched(Ok(SessionSwitched {
            session_id: "s1".into(),
            name: None,
            file: None,
            messages: Vec::new(),
        }));
        assert!(
            app.banner_hidden,
            "切换会话（含 /new 清空消息）后 banner 应永久隐藏"
        );
    }

    #[test]
    fn message_update_event_pushes_streaming_text() {
        let mut st = App::new();
        let ev: Value = serde_json::json!({
            "type": "message_update",
            "assistantMessageEvent": { "type": "text_delta", "delta": "hello" }
        });
        st.apply_sink_event(&ev);
        assert_eq!(st.streaming_text, "hello");
    }

    #[test]
    fn streaming_tokens_count_thinking_and_reset_on_message_end() {
        let mut st = App::new();
        // reasoning 输出也要计数（思考 delta 同样喂给 token 计数）
        st.apply_sink_event(&serde_json::json!({
                "type": "message_update",
                "assistantMessageEvent": { "type": "thinking_delta", "delta": "thinking about the problem " }
            }),
        );
        st.apply_sink_event(&serde_json::json!({
            "type": "message_update",
            "assistantMessageEvent": { "type": "text_delta", "delta": "answer text " }
        }));
        assert!(
            st.streaming_output_tokens() > 0,
            "thinking/text 增量应计入在途 token: {}",
            st.streaming_output_tokens()
        );
        // 消息落地：usage 已入 messages，在途计数清零（footer 用漂移修正衔接）
        let mut msg = AgentMessage::user_text("answer text");
        msg.role = "assistant".into();
        st.apply_sink_event(&serde_json::json!({ "type": "message_end", "message": msg }));
        assert_eq!(st.streaming_output_tokens(), 0);
    }

    #[test]
    fn message_end_dedupes_user_messages() {
        let mut st = App::new();
        st.messages.push(AgentMessage::user_text("same"));
        let ev: Value = serde_json::json!({
            "type": "message_end",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": "same" }],
                "timestamp": 0
            }
        });
        st.apply_sink_event(&ev);
        assert_eq!(
            st.messages.len(),
            1,
            "same role and same text should be deduped"
        );
    }

    #[test]
    fn turn_end_appends_tool_results() {
        let mut st = App::new();
        let ev: Value = serde_json::json!({
            "type": "turn_end",
            "toolResults": [{
                "role": "toolResult",
                "content": [{ "type": "text", "text": "output" }],
                "timestamp": 0
            }]
        });
        st.apply_sink_event(&ev);
        assert_eq!(st.messages.len(), 1);
        assert_eq!(st.messages[0].role, "toolResult");
    }

    /// 带 `parentToolCallId` 的事件是嵌套调用：累积到父工具块的实时进度，结束后标记完成。
    #[test]
    fn nested_tool_events_accumulate_under_parent() {
        let mut st = App::new();
        st.apply_sink_event(&serde_json::json!({
            "type": "tool_execution_start",
            "toolCallId": "n1",
            "toolName": "bash",
            "parentToolCallId": "call_1",
            "args": { "command": "ls -la" },
            "startedAtEpochMs": 1000
        }));
        let nested = st.nested_calls.get("call_1").expect("父工具应有嵌套行");
        assert_eq!(nested.len(), 1);
        assert_eq!(nested[0].name, "bash");
        assert_eq!(nested[0].summary, "ls -la");
        assert_eq!(nested[0].started_at_ms, Some(1000));
        assert!(nested[0].duration_ms.is_none(), "进行中不应有耗时");
        assert!(st.tool_started_at.is_empty(), "嵌套调用不得进入顶层计时表");

        st.apply_sink_event(&serde_json::json!({
            "type": "tool_execution_end",
            "toolCallId": "n1",
            "toolName": "bash",
            "parentToolCallId": "call_1",
            "isError": false,
            "durationMs": 250
        }));
        let nested = st.nested_calls.get("call_1").expect("父工具行仍在");
        assert_eq!(nested[0].duration_ms, Some(250));
        assert_eq!(nested[0].is_error, Some(false));
        assert_eq!(nested[0].started_at_ms, None, "完成后不再显示实时计时");
    }

    #[test]
    fn tool_execution_start_tracks_elapsed_timer_and_end_clears() {
        // 工具启动事件记录 startedAtEpochMs（渲染 `Elapsed` 实时计时），结束后移除（切回 Took）
        let mut st = App::new();
        st.apply_sink_event(&serde_json::json!({
            "type": "tool_execution_start",
            "toolCallId": "t1",
            "toolName": "bash",
            "args": { "command": "sleep 5" },
            "startedAtEpochMs": 12345
        }));
        assert_eq!(st.tool_started_at.get("t1"), Some(&12345));
        assert!(
            !st.tool_started_at.contains_key("t2"),
            "未启动的工具不应记录"
        );

        st.apply_sink_event(&serde_json::json!({
            "type": "tool_execution_end",
            "toolCallId": "t1",
            "toolName": "bash",
            "result": { "content": [{ "type": "text", "text": "ok" }] },
            "isError": false
        }));
        assert!(
            !st.tool_started_at.contains_key("t1"),
            "工具完成后应移除进行中计时"
        );
    }

    #[test]
    fn model_routed_event_updates_footer_mirror_and_agent_start_clears() {
        let mut app = App::new();
        app.apply_sink_event(&serde_json::json!({
            "type": "model_routed",
            "provider": "anthropic",
            "model": "claude-real",
            "thinkingLevel": "medium"
        }));
        assert_eq!(app.routed_model.as_deref(), Some("claude-real"));
        assert_eq!(app.routed_thinking_level.as_deref(), Some("medium"));

        // agent_start（每次 run_loop 尝试）清空路由镜像
        app.apply_sink_event(&serde_json::json!({ "type": "agent_start" }));
        assert!(app.routed_model.is_none());
        assert!(app.routed_thinking_level.is_none());
    }

    #[test]
    fn thinking_level_changed_shows_dedup_system_notice() {
        // 切换 thinking 级别输出系统提示 `Thinking level: X`；连续切换（Shift+Tab 循环）
        // 原地替换同一条提示，不堆积多条；镜像同步更新。
        let mut app = App::new();
        app.on_thinking_level_changed("off".to_string(), None);
        assert_eq!(app.thinking_level.as_deref(), Some("off"));
        assert_eq!(
            sys_spans_text(&app.system_messages[0].1.spans),
            "Thinking level: off"
        );

        // 连续循环切换：原地替换，仍是同一条
        app.on_thinking_level_changed("high".to_string(), None);
        app.on_thinking_level_changed("max".to_string(), None);
        assert_eq!(
            app.system_messages.len(),
            1,
            "连续切换应原地替换: {:?}",
            app.system_messages
        );
        assert_eq!(
            sys_spans_text(&app.system_messages[0].1.spans),
            "Thinking level: max"
        );
        assert_eq!(app.thinking_level.as_deref(), Some("max"));

        // 中间插入其他提示后（非连续）再切换：追加新一条
        app.push_msg("other notice".to_string(), MsgLevel::Info);
        app.on_thinking_level_changed("low".to_string(), None);
        assert_eq!(app.system_messages.len(), 3);
        assert_eq!(
            sys_spans_text(&app.system_messages.last().unwrap().1.spans),
            "Thinking level: low"
        );
    }

    #[test]
    fn on_share_done_resets_busy_and_reports() {
        // /share 忙碌态：回执后 spinner 必须退出（busy/working_kind 复位），
        // 同时结果提示正常进入消息流。
        let mut app = App::new();
        app.busy = true;
        app.working_kind = WorkingKind::Sharing;
        app.status = "Sharing session...".to_string();

        app.on_share_done(
            true,
            "shared via gist: https://gist.github.com/abc".to_string(),
        );
        assert!(!app.busy, "share 回执后应退出忙碌态");
        assert_eq!(app.working_kind, WorkingKind::Working);
        assert!(app.status.is_empty(), "share 状态文本应清除");
        let has = app
            .system_messages
            .iter()
            .any(|(_, s)| sys_spans_text(&s.spans).contains("shared via gist"));
        assert!(has, "share 结果应进入提示流");

        // 失败路径同样复位
        app.busy = true;
        app.working_kind = WorkingKind::Sharing;
        app.on_share_done(false, "share failed: gh not found".to_string());
        assert!(!app.busy);
        let has_err = app
            .system_messages
            .iter()
            .any(|(_, s)| sys_spans_text(&s.spans).contains("share failed"));
        assert!(has_err, "share 失败提示应进入提示流");
    }

    #[test]
    fn on_bug_report_done_resets_busy_and_reports() {
        // /bug 导出忙碌态：回执后 spinner 必须退出（busy/working_kind 复位 + pending 清空）
        let mut app = App::new();
        app.busy = true;
        app.working_kind = WorkingKind::BugReport;
        app.status = "Building bug report...".to_string();
        app.pending_bug_report = Some(crate::modes::interactive::app::PendingBugReport {
            hint: "x".to_string(),
            include_session: true,
            include_summary: false,
        });

        app.on_bug_report_done(
            true,
            "Bug report exported to: /tmp/prux-bug-report-bug-1.zip\nReport ID: bug-1".to_string(),
        );
        assert!(!app.busy, "回执后应退出忙碌态");
        assert_eq!(app.working_kind, WorkingKind::Working);
        assert!(app.status.is_empty(), "状态文本应清除");
        assert!(app.pending_bug_report.is_none(), "流程状态应清空");
        assert!(
            app.system_messages
                .iter()
                .any(|(_, s)| sys_spans_text(&s.spans).contains("Bug report exported to")),
            "导出路径应进入提示流"
        );

        // 失败路径同样复位
        app.busy = true;
        app.working_kind = WorkingKind::BugReport;
        app.on_bug_report_done(false, "Failed to write bug report: disk full".to_string());
        assert!(!app.busy);
        assert!(
            app.system_messages
                .iter()
                .any(|(_, s)| sys_spans_text(&s.spans).contains("Failed to write bug report"))
        );
    }

    #[test]
    fn retry_window_keeps_busy_until_agent_settled() {
        // 回归：重试期间每次 run_loop 尝试都会发 agent_end，不得被当作整轮结束清 busy——
        // 否则 Esc/Ctrl+C 无法中断重试，且队列会被提前 drain。
        let mut app = App::new();
        app.busy = true;
        app.apply_sink_event(&serde_json::json!({ "type": "agent_end" }));
        assert!(app.busy, "重试期间的 agent_end 不得复位 busy");
        app.apply_sink_event(&serde_json::json!({
            "type": "auto_retry_start",
            "attempt": 1,
            "maxAttempts": 3,
            "delayMs": 2000,
        }));
        assert!(app.busy, "重试进行中 busy 保持 true（可中断）");
        // 只有整轮结束信号才复位忙碌态
        app.apply_sink_event(&serde_json::json!({ "type": "agent_settled" }));
        assert!(!app.busy, "agent_settled 才复位 busy");
    }

    #[test]
    fn agent_settled_drains_queued_steer_and_reopens_busy() {
        // agent_settled 才是取下一批排队消息的时机：drain 到 steer 后重开 busy 进入下一轮。
        let mut app = App::new();
        app.busy = true;
        app.runtime_steer_inbox
            .lock()
            .unwrap()
            .push_back(AgentMessage::user_text("steer me"));
        app.apply_sink_event(&serde_json::json!({ "type": "agent_settled" }));
        assert!(
            app.runtime_steer_inbox.lock().unwrap().is_empty(),
            "agent_settled 应 drain 排队 steer"
        );
        assert!(app.busy, "drain 到待发消息后应重开 busy 进入下一轮");
    }

    #[test]
    fn agent_settled_event_carries_queue_state_to_extensions() {
        //
        // notify 等扩展靠 `hasQueuedMessages` 区分「真正空闲」与「还有活要干」：
        // steer 注入箱与 follow-up 本地队列任一非空都要如实上报。
        let mut app = App::new();
        let idle = app.enrich_agent_settled(&serde_json::json!({ "type": "agent_settled" }));
        assert_eq!(idle["hasQueuedMessages"], false, "空队列 = 真正空闲");
        assert_eq!(idle["type"], "agent_settled", "原事件字段应保留");

        app.runtime_followup_inbox.push_back("fup".to_string());
        let fup = app.enrich_agent_settled(&serde_json::json!({ "type": "agent_settled" }));
        assert_eq!(fup["hasQueuedMessages"], true, "follow-up 非空应上报");

        app.runtime_followup_inbox.clear();
        app.runtime_steer_inbox
            .lock()
            .unwrap()
            .push_back(AgentMessage::user_text("steer"));
        let steer = app.enrich_agent_settled(&serde_json::json!({ "type": "agent_settled" }));
        assert_eq!(steer["hasQueuedMessages"], true, "steer 非空应上报");
    }

    #[test]
    fn on_compact_done_success_pushes_tokens_message() {
        // 压缩成功：输出带前后 token 数的成功提示；跳过/失败保持原有提示。
        let mut app = App::new();
        let summary = serde_json::json!({
            "tokensBefore": 12000,
            "estimatedTokensAfter": 3000,
        });
        app.on_compact_done(Ok(Some(summary)));
        assert!(!app.busy, "回执后 busy 复位");
        assert_eq!(app.system_messages.len(), 1);
        let text = sys_spans_text(&app.system_messages[0].1.spans);
        assert!(text.contains("✓ Context compacted"), "成功提示: {:?}", text);
        assert!(
            text.contains("12000") && text.contains("3000"),
            "token 数: {:?}",
            text
        );

        // 跳过：Nothing to compact。
        let mut app = App::new();
        app.on_compact_done(Ok(None));
        assert!(sys_spans_text(&app.system_messages[0].1.spans).contains("Nothing to compact"));
        assert!(app.system_messages[0].1.level == MsgLevel::Info);

        // 失败：错误提示。
        let mut app = App::new();
        app.on_compact_done(Err("model api down".to_string()));
        assert!(sys_spans_text(&app.system_messages[0].1.spans).contains("compaction failed"));
    }

    #[test]
    fn turn_end_tool_results_invalidate_cache() {
        // toolResult 到达可能填充任意历史 assistant 的工具块 → 必须整体失效缓存
        let mut app = App::new();
        let mut ai = make_assistant("");
        ai.content = vec![ContentBlock::ToolCall {
            id: "call_1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "command": "ls" }),
            thought_signature: None,
            namespace: None,
        }];
        app.messages.push(ai);
        let _ = app.render_history_lines(60);
        assert_eq!(app.msg_cache.len(), 1);
        let ev: Value = serde_json::json!({
            "type": "turn_end",
            "toolResults": [{
                "role": "toolResult",
                "content": [{ "type": "text", "text": "out" }],
                "timestamp": 1,
                "tool_call_id": "call_1"
            }]
        });
        app.apply_sink_event(&ev);
        assert!(
            app.msg_cache.is_empty(),
            "turn_end 携带 toolResults 后缓存必须清空（工具块由 pending 态更新为结果态）"
        );
        // 重建后缓存条目包含工具结果
        let r = app.render_history_lines(60);
        assert_eq!(r.segments.len(), 2, "assistant 块 + toolResult 块");
    }

    #[test]
    fn compaction_end_refreshes_context_percent_from_estimated_after() {
        // /compact 成功后 footer 的窗口占用必须立即下降：压缩摘要不带 usage，
        // 旧 assistant 的 usage 是压缩前的陈旧值，需用 estimatedTokensAfter 覆盖。
        let mut app = App::new();
        app.context_window = 200_000;
        let mut old = AgentMessage::user_text("old question");
        old.timestamp = 1;
        let mut usage_msg = assistant_with_usage("old answer", 2, 100_000);
        usage_msg.entry_id = Some("a1".into());
        app.messages = vec![old, usage_msg];
        app.refresh_context_percent();
        assert!((app.context_percent - 50.0).abs() < 0.01);

        app.apply_sink_event(&serde_json::json!({
            "type": "compaction_end",
            "reason": "manual",
            "result": "completed",
            "tokensBefore": 100_000,
            "estimatedTokensAfter": 2_000,
        }));

        assert_eq!(app.context_tokens_override, Some(2_000));
        assert!(
            (app.context_percent - 1.0).abs() < 0.01,
            "压缩后占用立即按摘要 + 尾部刷新: {}",
            app.context_percent
        );
        // 渲染历史保留（footer 的累计 in/out/费用依赖 ctx.messages，不能截断）
        assert_eq!(app.messages.len(), 2);
    }

    #[test]
    fn fresh_assistant_usage_clears_compaction_override() {
        // 压缩后下一条带 usage 的 assistant 落地：usage 已是压缩后的真实值，
        // 覆盖值清除，占用回到 usage 锚点；无 usage 的消息（toolResult/用户）不清除。
        let mut app = App::new();
        app.context_window = 200_000;
        app.messages
            .push(assistant_with_usage("old answer", 2, 100_000));
        app.context_percent = 50.0;
        app.context_tokens_override = Some(2_000);

        // toolResult / 用户消息不影响覆盖值
        app.apply_sink_event(&serde_json::json!({
                "type": "message_end",
                "message": { "role": "user", "content": [{ "type": "text", "text": "next" }], "timestamp": 3 },
            }),
        );
        assert_eq!(app.context_tokens_override, Some(2_000));
        assert!((app.context_percent - 1.0).abs() < 0.01, "覆盖值仍在生效");

        // 带 usage 的 assistant：清除覆盖值并按其 usage 重算
        let fresh = assistant_with_usage("new answer", 4, 3_000);
        app.apply_sink_event(&serde_json::json!({
            "type": "message_end",
            "message": serde_json::to_value(&fresh).unwrap(),
        }));
        assert_eq!(app.context_tokens_override, None);
        assert!(
            (app.context_percent - 1.5).abs() < 0.01,
            "回到 usage 锚点: {}",
            app.context_percent
        );
    }

    #[test]
    fn session_switch_and_navigate_drop_compaction_override() {
        // `on_session_switched` 会经扩展注册表分发（subagent → manager::reset_all），
        // 必须与触碰 subagent 全局态的测试串行。
        let _sub = crate::extensions::subagent::test_lock();
        // 消息流被整体替换（切会话 / 树导航）后，压缩时的覆盖值对新上下文无效。
        let mut app = App::new();
        app.context_window = 200_000;
        app.context_tokens_override = Some(2_000);
        app.on_session_switched(Ok(SessionSwitched {
            session_id: "s1".into(),
            name: None,
            file: Some("/tmp/s.jsonl".into()),
            messages: vec![assistant_with_usage("resumed", 5, 40_000)],
        }));
        assert_eq!(app.context_tokens_override, None);
        assert!((app.context_percent - 20.0).abs() < 0.01);

        app.context_tokens_override = Some(2_000);
        app.on_navigate_done(Ok(NavigateDone {
            editor_text: None,
            messages: vec![assistant_with_usage("branch", 6, 10_000)],
        }));
        assert_eq!(app.context_tokens_override, None);
        assert!((app.context_percent - 5.0).abs() < 0.01);
    }

    #[test]
    fn compaction_end_ignores_unfinished_or_payload_less_events() {
        // 仅 result=completed 且带 estimatedTokensAfter 的事件才覆盖：失败/中断事件不动占用。
        let mut app = App::new();
        app.context_window = 200_000;
        app.messages
            .push(assistant_with_usage("old answer", 2, 100_000));
        app.refresh_context_percent();
        let before = app.context_percent;

        for ev in [
            serde_json::json!({ "type": "compaction_end", "result": "completed" }),
            serde_json::json!({ "type": "compaction_end", "result": "failed" }),
            serde_json::json!({
                "type": "compaction_end",
                "reason": "overflow",
                "result": "failed",
                "willRetry": false,
                "estimatedTokensAfter": 1,
            }),
        ] {
            app.apply_sink_event(&ev);
            assert_eq!(app.context_tokens_override, None, "{ev}");
            assert!((app.context_percent - before).abs() < 0.01, "{ev}");
        }
    }

    /// pi `showCacheMissNotices`：默认不提示保温；开启后提示成本（含扩展覆盖标注）。
    #[test]
    fn cache_warm_notice_is_gated_by_show_cache_miss_notices() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let mut app = App::new();
        let event = serde_json::json!({ "type": "cache_warm", "cost": 0.030015 });

        app.apply_sink_cache_warm(&event);
        assert!(app.system_messages.is_empty(), "默认不提示保温");

        crate::core::settings_manager::write_settings_show_cache_miss_notices(true).unwrap();
        app.apply_sink_cache_warm(&event);
        assert_eq!(app.system_messages.len(), 1);
        let text = crate::modes::interactive::app::sys_msg_text(&app.system_messages[0].1);
        assert!(text.contains("Cache warmed: $0.030015"), "{text}");

        let override_event =
            serde_json::json!({ "type": "cache_warm", "cost": 0.1, "extensionOverride": true });
        app.apply_sink_cache_warm(&override_event);
        let text = crate::modes::interactive::app::sys_msg_text(&app.system_messages[1].1);
        assert!(text.contains("(extension override)"), "{text}");
    }
}

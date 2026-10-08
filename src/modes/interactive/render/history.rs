//! 消息历史 → 渲染行：时间线组装、分段缓存、单条消息行的便捷入口。
//!
//! `App::msg_cache` / `cache_*` / `edit_previews` / `highlight_*` 等字段所有权仍在 app.rs；
//! 本模块负责「哪些参数变了要整体失效」「缓存怎么命中」「时间线怎么分段」这些渲染侧逻辑，
//! 由 `render.rs` 主循环与 `render/status.rs` 调用。
//!
//! - [`App::render_history_lines`]：组装 `HistoryRender`（分段 + 总行数 + 进行中工具 + 锚点行）
//! - [`App::invalidate_render`]：主题 / 宽度 / 展开态变化时的整体失效
//! - [`App::build_highlight_snapshot`]：后台高亮补全用的只读快照
//! - [`message_lines`]：不经过缓存的单条消息渲染（预览与测试用）

use super::{
    super::{
        app::{App, MsgLine, SysMsg},
        theme::Theme,
    },
    messages::{
        self, CachedMsg, EditRenderData, HighlightSnapshot, HistoryRender, ImageLayout, ImageSlot,
        ToolResultView,
    },
};
use crate::{
    core::{
        self,
        provider::{AgentMessage, ContentBlock},
    },
    utils::{terminal_image, time::now_ms},
};
use serde_json::Value;
use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::Arc,
};

impl App {
    /// 派生渲染输入（工具结果表 + edit 表）：两者都需要克隆全量工具输出 / diff，
    /// 而渲染每帧都会用到。按消息集指纹缓存，避免长会话里每帧重复克隆。
    fn ensure_derived_inputs(&mut self) {
        let fp = messages_fingerprint(&self.messages, &self.system_messages);
        if self.derived_fp == Some(fp) {
            return;
        }

        let tool_results = messages::scan_tool_results(&self.messages);
        let edit_map = self.build_edit_map(&tool_results);
        self.derived_tool_results = tool_results;
        self.derived_edit_map = edit_map;
        self.derived_fp = Some(fp);
    }

    /// 渲染参数（宽度 / 展开 / thinking 显隐 / 主题）与缓存不符 → 整体失效重建
    fn ensure_cache_params(&mut self, width: usize, expand_all: bool, show_thinking: bool) {
        let theme_fp = self.theme.render_fingerprint();
        // 图片布局参与缓存键：开关切换或单元格尺寸变化会改变预留行数，旧行必须重画
        let images_fp = (self.show_images, terminal_image::image_font_size());
        if self.cache_width != width
            || self.cache_expand_all != expand_all
            || self.cache_show_thinking != show_thinking
            || self.cache_theme_fp != theme_fp
            || self.cache_images_fp != images_fp
        {
            self.msg_cache.clear();
            self.cache_width = width;
            self.cache_expand_all = expand_all;
            self.cache_show_thinking = show_thinking;
            self.cache_theme_fp = theme_fp;
            self.cache_images_fp = images_fp;
        }
    }

    /// 当前生效的内联图片渲染参数；未开启或终端不支持图形协议时为 `None`
    /// （此时图片块整块不渲染，布局与历史行为一致）。
    pub fn image_layout(&self) -> Option<ImageLayout> {
        if !self.show_images {
            return None;
        }

        let font = terminal_image::image_font_size()?;
        Some(ImageLayout {
            enabled: true,
            font,
        })
    }

    /// 构建 edit 工具渲染表：result_diff 来自 toolResult.details.diff；
    /// 对尚无结果（pending）的 edit 调用，
    /// 用 tools::preview_edit_diff 算预览 diff 并按 toolCallId memo（同步一次性 + 缓存）。
    fn build_edit_map(
        &mut self,
        tool_results: &HashMap<String, ToolResultView>,
    ) -> HashMap<String, EditRenderData> {
        let mut map = messages::edit_map_from_results(&self.messages);

        for m in &self.messages {
            for block in &m.content {
                if let ContentBlock::ToolCall {
                    id,
                    name,
                    arguments,
                    ..
                } = block
                    && name == "edit"
                    && !tool_results.contains_key(id.as_str())
                {
                    let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
                    let edits = arguments.get("edits").unwrap_or(&Value::Null);
                    let preview = self
                        .edit_previews
                        .entry(id.clone())
                        .or_insert_with(|| core::tools::preview_edit_diff(path, edits, &self.cwd));

                    if let Some(d) = preview {
                        map.entry(id.clone()).or_default().preview = Some(d.clone());
                    }
                }
            }
        }
        map
    }

    /// 渲染全部历史（消息 + 系统提示时间线），逐条命中消息级缓存；
    /// 未命中条目用当前主题即时渲染并填充缓存。
    /// 已渲染过的消息（滚动/拖拽/流式 tick）零重渲染——只做指针拼接与裁剪定位。
    pub fn render_history_lines(&mut self, width: usize) -> HistoryRender {
        let expand_all = self.expand_all;
        let show_thinking = self.show_thinking;
        self.ensure_cache_params(width, expand_all, show_thinking);
        self.ensure_derived_inputs();
        let layout = self.image_layout().unwrap_or_else(ImageLayout::disabled);

        let tool_results = &self.derived_tool_results;
        let edit_map = &self.derived_edit_map;
        let items = messages::timeline_items(&self.messages, &self.system_messages);
        let mut segments: Vec<messages::HistorySegment> = Vec::new();
        let mut click_targets: Vec<messages::ClickTarget> = Vec::new();
        let mut links: Vec<messages::LinkSpan> = Vec::new();
        let mut images: Vec<ImageSlot> = Vec::new();
        let mut accum_pending: Vec<messages::PendingTool> = Vec::new();
        let mut acc: usize = 0;
        let mut anchor_end: Option<usize> = None;

        // 锚点消息序号 = assistant 开始时的消息数 - 1（即它开始前最后一条消息）
        let anchor_msg_index = self.stream_anchor_index.map(|a| a.saturating_sub(1));

        // Elapsed 实时计时：含进行中工具块的缓存条目每秒强制重渲染一次；
        // 其余帧仍命中缓存显示上一秒的值，避免工具执行期间整块历史全量重建
        let now_rebuild = now_ms();
        let allow_elapsed_rebuild = self
            .last_elapsed_rebuild
            .map(|t| now_rebuild.saturating_sub(t) >= 1000)
            .unwrap_or(true);

        for (ts, seq, iref) in items {
            let key = (ts, seq);
            let base = accum_pending.len();

            // 逐块折叠态：覆盖优先、缺失回退全局（thinking→`show_thinking`，其余→`expand_all`）
            let expansions: Vec<bool> = match iref {
                messages::TimelineRef::Msg(m) => messages::resolve_block_expansions(
                    m,
                    ts,
                    seq,
                    &self.expand_overrides,
                    expand_all,
                    show_thinking,
                ),
                messages::TimelineRef::Sys(_) => Vec::new(),
            };

            // 命中缓存 = 直接用缓存；未命中或计时块过期 = 重渲染。
            // 计时块（进行中工具）按 1s 节流强制重渲染以刷新 `Elapsed`；
            // 重渲染同样写缓存——工具结束后 turn_end 整体失效，缓存重建，无残留风险
            let hit = self.msg_cache.get(&key);
            let force = hit
                .map(|c| {
                    allow_elapsed_rebuild && c.pending.iter().any(|p| p.started_at_ms.is_some())
                })
                .unwrap_or(false);
            if force {
                self.last_elapsed_rebuild = Some(now_rebuild);
            }

            let cached = if let (Some(c), false) = (hit, force) {
                accum_pending.extend(c.pending.iter().cloned());
                c.clone()
            } else {
                let mut blocks: Vec<messages::BlockSpan> = Vec::new();
                let mut entry_links: Vec<messages::LinkSpan> = Vec::new();
                let mut entry_images: Vec<ImageSlot> = Vec::new();
                let lines = messages::render_timeline_item(
                    iref,
                    width,
                    &self.theme,
                    &expansions,
                    &mut blocks,
                    tool_results,
                    &self.tool_started_at,
                    &self.nested_calls,
                    edit_map,
                    &mut accum_pending,
                    &mut entry_links,
                    &mut entry_images,
                    layout,
                );
                let delta: Vec<messages::PendingTool> = accum_pending[base..].to_vec();
                let c = CachedMsg {
                    lines: Arc::new(lines),
                    pending: Arc::new(delta),
                    blocks: Arc::new(blocks),
                    links: Arc::new(entry_links),
                    images: Arc::new(entry_images),
                };
                self.msg_cache.insert(key, c.clone());
                c
            };

            if cached.lines.is_empty() {
                continue;
            }

            segments.push((acc, cached.lines.clone(), cached.pending.clone()));

            // 链接 → 内容全局行（相对条目起点的行偏移）
            for link in cached.links.iter() {
                let mut link = link.clone();
                link.line += acc;
                links.push(link);
            }

            // 图片槽位 → 内容全局行（同上）
            for slot in cached.images.iter() {
                let mut slot = slot.clone();
                slot.line += acc;
                images.push(slot);
            }

            // 可折叠块 → 内容全局行命中区（相对条目起点的块偏移）
            for b in cached.blocks.iter() {
                click_targets.push(messages::ClickTarget {
                    start: acc + b.start,
                    end: acc + b.end,
                    id: messages::BlockId {
                        ts,
                        seq,
                        block: b.index,
                    },
                    default_expanded: expansions.get(b.index).copied().unwrap_or(false),
                });
            }
            acc += cached.lines.len();

            // 块后补纯空行间隔（每块渲染后判断末尾，非纯间隔行则补 1 行，包含末尾块）
            let ends_blank = cached
                .lines
                .last()
                .map(messages::is_pure_gap)
                .unwrap_or(true);
            if !ends_blank {
                acc += 1;
            }

            // 锚点消息块完成后记录行号（含块后间隔）
            if let Some(anchor) = anchor_msg_index
                && anchor == seq
            {
                anchor_end = Some(acc);
            }
        }

        HistoryRender {
            segments,
            total: acc,
            pending: accum_pending,
            anchor_end,
            click_targets,
            links,
            images,
        }
    }
    /// 消息集变化（toolResult 到达 / 会话切换 / 导航）或宽度重建：清空渲染缓存，
    /// 下一帧对未命中条目重新渲染填充。edit 预览按 toolCallId 缓存，随消息集一起失效。
    pub fn invalidate_render(&mut self) {
        self.msg_cache.clear();
        self.edit_previews.clear();
    }

    /// Ctrl+O：全局展开开关。新值会**覆盖**到所有非 thinking 折叠块上，
    /// 因此丢弃这些块的逐块覆盖（thinking 由 `set_show_thinking` 单独管）。
    pub fn toggle_expand_all(&mut self) {
        self.set_expand_all(!self.expand_all);
    }

    /// 设置全局展开开关（值不变时早退，也不清逐块覆盖）
    pub fn set_expand_all(&mut self, expanded: bool) {
        if self.expand_all == expanded {
            return;
        }
        self.expand_all = expanded;
        self.drop_block_overrides(|k| matches!(k, messages::BlockKind::Thinking));
        self.dirty = true;
    }

    /// Ctrl+T：thinking 块全局显隐。清空 thinking 的逐块覆盖，非 thinking 块不受影响。
    pub fn toggle_show_thinking(&mut self) {
        self.set_show_thinking(!self.show_thinking);
    }

    /// 设置 thinking 显隐（值不变时早退）
    pub fn set_show_thinking(&mut self, visible: bool) {
        if self.show_thinking == visible {
            return;
        }
        self.show_thinking = visible;
        self.drop_block_overrides(|k| !matches!(k, messages::BlockKind::Thinking));
        self.dirty = true;
    }

    /// /settings Show images：内联图片开关。
    ///
    /// 开关改变会改变图片块占用的行数（预留行 ↔ 整块不渲染），所以必须连同渲染缓存与
    /// 图片协议缓存一起丢弃；终端不支持图形协议时只是不画图，不报错。
    pub fn set_show_images(&mut self, enabled: bool) {
        if self.show_images == enabled {
            return;
        }

        self.show_images = enabled;
        self.invalidate_render();
        super::image::reset_cache();
        self.dirty = true;
    }

    /// 丢弃逐块折叠覆盖：`keep` 返回 true 的块保留（无法判定类型的也丢弃）。
    fn drop_block_overrides(&mut self, keep: impl Fn(messages::BlockKind) -> bool) {
        let victims: Vec<messages::BlockId> = self
            .expand_overrides
            .keys()
            .filter(|id| {
                let kind = self
                    .messages
                    .get(id.seq)
                    .and_then(|m| messages::collapsible_blocks(m).get(id.block).copied());
                !kind.map(&keep).unwrap_or(false)
            })
            .copied()
            .collect();
        for id in victims {
            self.expand_overrides.remove(&id);
        }
    }

    /// 恢复启动后台补高亮任务收集快照（当前渲染参数 + Full 高亮主题）
    pub fn build_highlight_snapshot(&self) -> HighlightSnapshot {
        HighlightSnapshot {
            msgs: self.messages.clone(),
            sys: self.system_messages.clone(),
            width: self.cache_width,
            expand_all: self.expand_all,
            show_thinking: self.show_thinking,
            images: self.image_layout().unwrap_or_else(ImageLayout::disabled),
            overrides: self.expand_overrides.clone(),
            theme: {
                let mut t = self.theme.clone();
                t.syntax_highlight = true;
                t
            },
        }
    }
}

/// 消息集指纹（消息 + 系统提示）：宽度 / 主题 / 展开态变化不改变它。
/// 用于缓存与渲染参数无关、但只能全量扫描得到的派生输入（工具结果表 / edit 表）。
fn messages_fingerprint(messages: &[AgentMessage], system: &[(u64, SysMsg)]) -> u64 {
    let mut h = DefaultHasher::new();
    messages.len().hash(&mut h);
    for m in messages {
        m.timestamp.hash(&mut h);
        m.role.hash(&mut h);
        m.content.len().hash(&mut h);
        m.tool_call_id.hash(&mut h);
        m.is_error.hash(&mut h);
        m.duration_ms.hash(&mut h);
    }

    system.len().hash(&mut h);

    for (ts, msg) in system {
        ts.hash(&mut h);
        msg.spans.len().hash(&mut h);
    }

    h.finish()
}

/// 消息 → 渲染行
pub fn message_lines(
    msg: &AgentMessage,
    width: usize,
    theme: &Theme,
    expand_all: bool,
) -> Vec<MsgLine> {
    message_lines_full(msg, width, theme, expand_all, true)
}
/// 带 thinking 显隐的消息行生成
pub fn message_lines_full(
    msg: &AgentMessage,
    width: usize,
    theme: &Theme,
    expand_all: bool,
    show_thinking: bool,
) -> Vec<MsgLine> {
    let mut pending = Vec::new();
    let edit_map = HashMap::new();

    messages::render_message(
        msg,
        width,
        theme,
        expand_all,
        show_thinking,
        &mut pending,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &edit_map,
    )
}
/// 供外部测试使用的消息行生成入口
#[cfg(test)]
pub fn message_lines_for_test(
    msg: &AgentMessage,
    width: usize,
    theme: &Theme,
    expand_all: bool,
) -> Vec<MsgLine> {
    message_lines(msg, width, theme, expand_all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::provider::AgentMessage,
        modes::interactive::{app::MsgLine, theme::Theme},
    };

    fn line_text(line: &MsgLine) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn make_assistant(text: &str) -> AgentMessage {
        let mut m = AgentMessage::user_text(text);
        m.role = "assistant".to_string();
        m
    }

    #[test]
    fn message_lines_render_user_and_assistant() {
        let theme = Theme::default();
        let lines =
            message_lines_for_test(&AgentMessage::user_text("hello-user"), 60, &theme, false);
        assert!(lines.iter().any(|l| line_text(l).contains("hello-user")));

        let lines2 = message_lines_for_test(&make_assistant("hello-ai"), 60, &theme, false);
        assert!(lines2.iter().any(|l| line_text(l).contains("hello-ai")));
    }

    #[test]
    fn thinking_block_folds_and_expands() {
        let theme = Theme::default();
        let mut msg = make_assistant("");
        msg.content.push(ContentBlock::Thinking {
            thinking: "x".repeat(200),
            thinking_signature: None,
            redacted: None,
        });
        // 折叠（show_thinking=false）：斜体 "Thinking..." 标签（对齐 pi 隐藏 thinking 块）
        let folded = message_lines_full(&msg, 60, &theme, true, false);
        assert!(folded.iter().any(|l| line_text(l).contains("Thinking...")));
        // 展开：完整内容
        let expanded = message_lines_for_test(&msg, 60, &theme, true);
        let total: String = expanded.iter().map(line_text).collect();
        assert!(total.contains("xxxxx"));
    }

    #[test]
    fn tool_call_block_summarizes() {
        let theme = Theme::default();
        let mut msg = make_assistant("");
        msg.content.push(ContentBlock::ToolCall {
            id: "1".into(),
            name: "read".into(),
            arguments: serde_json::json!({ "path": "a.rs" }),
            thought_signature: None,
            namespace: None,
        });
        // 工具块：粗体工具名 + 参数（对齐 ToolExecutionComponent fallback）
        let lines = message_lines_for_test(&msg, 60, &theme, true);
        let total: String = lines.iter().map(line_text).collect();
        assert!(total.contains("read"), "tool title: {}", total);
        assert!(total.contains("a.rs"), "args: {}", total);
    }

    /// 派生输入（工具结果表 / edit 表）按消息集指纹缓存：
    /// 宽度变化不得重建，消息变化必须重建。
    #[test]
    fn derived_render_inputs_cached_until_messages_change() {
        let mut app = App::new();
        let mut asst = AgentMessage::user_text("");
        asst.role = "assistant".to_string();
        asst.content = vec![ContentBlock::ToolCall {
            id: "t1".into(),
            name: "bash".into(),
            arguments: serde_json::json!({ "cmd": "ls" }),
            thought_signature: None,
            namespace: None,
        }];
        app.messages.push(asst);
        let mut res = AgentMessage::user_text("output");
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("t1".into());
        res.timestamp = app.messages[0].timestamp + 1;
        app.messages.push(res);

        let _ = app.render_history_lines(60);
        assert!(app.derived_tool_results.contains_key("t1"));

        // 伪造哨兵值：消息集未变时后续渲染必须复用缓存（哨兵存活）
        app.derived_tool_results.insert(
            "t1".into(),
            ToolResultView {
                text: "SENTINEL".into(),
                ..Default::default()
            },
        );
        let _ = app.render_history_lines(40); // 宽度变化：渲染缓存失效，派生输入不该失效
        assert_eq!(
            app.derived_tool_results.get("t1").map(|v| v.text.as_str()),
            Some("SENTINEL")
        );

        // 新增消息 → 指纹变化 → 重建（哨兵被真实结果覆盖）
        let mut res2 = AgentMessage::user_text("second");
        res2.role = "toolResult".to_string();
        res2.tool_call_id = Some("t1".into());
        res2.timestamp = app.messages[1].timestamp + 1;
        app.messages.push(res2);
        let _ = app.render_history_lines(40);
        assert_eq!(
            app.derived_tool_results.get("t1").map(|v| v.text.as_str()),
            Some("second")
        );
    }

    /// 主题变化丢弃渲染缓存（已有用例覆盖）但不得抛掉派生输入——
    /// 否则主题预览/宽度抖动会把全量工具输出重新克隆一遂。
    #[test]
    fn derived_render_inputs_survive_theme_change() {
        let mut app = App::new();
        let mut res = AgentMessage::user_text("output");
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("t9".into());
        app.messages.push(res);

        let _ = app.render_history_lines(60);
        app.derived_tool_results.insert(
            "t9".into(),
            ToolResultView {
                text: "SENTINEL".into(),
                ..Default::default()
            },
        );

        let fp_before = app.theme.render_fingerprint();
        let mut other = app.theme.clone();
        other
            .vars
            .insert("accent".to_string(), "#ff0000".to_string());
        app.theme = other;
        assert_ne!(
            app.theme.render_fingerprint(),
            fp_before,
            "主题指纹应变化（下一帧 ensure_cache_params 据此清空渲染缓存）"
        );
        let _ = app.render_history_lines(60);
        assert_eq!(
            app.derived_tool_results.get("t9").map(|v| v.text.as_str()),
            Some("SENTINEL"),
            "派生输入与渲染参数无关，不该被主题变化抛弃"
        );
    }

    #[test]
    fn render_history_cache_hits_and_reuses_arc() {
        // 同一渲染参数：第二次渲染必须命中缓存（条目 Arc 指针相同 → 零重渲染）
        let mut app = App::new();
        for i in 0..5 {
            app.messages
                .push(AgentMessage::user_text(&format!("msg {}", i)));
        }
        let r1 = app.render_history_lines(60);
        assert_eq!(r1.segments.len(), 5, "每条消息一段");
        let first = r1.segments[0].1.clone();
        let r2 = app.render_history_lines(60);
        assert!(
            std::sync::Arc::ptr_eq(&first, &r2.segments[0].1),
            "同参数命中缓存，不得重渲染"
        );
        assert_eq!(r1.total, r2.total, "总行数稳定");

        // 追加消息：新消息走渲染，旧消息缓存仍命中
        app.messages
            .push(AgentMessage::user_text("appended message"));
        let r3 = app.render_history_lines(60);
        assert!(std::sync::Arc::ptr_eq(&first, &r3.segments[0].1));
        assert_eq!(r3.segments.len(), 6);
        assert!(r3.total > r1.total);
    }

    #[test]
    fn render_history_blocks_end_with_gap_row_and_total_matches() {
        // 每个渲染块（消息）尾部必须带一行纯空行（is_pure_gap），且 total 与实际可见行数一致
        // （此前的 bug：间隔行只计入 total 不进 segments，导致块间无空行且 total 虚高）
        let mut app = App::new();
        app.messages.push(AgentMessage::user_text("first user"));
        let mut asst = AgentMessage::user_text("");
        asst.role = "assistant".to_string();
        asst.content = vec![crate::core::provider::ContentBlock::Text {
            text: "assistant text".into(),
            text_signature: None,
        }];
        app.messages.push(asst);
        let r = app.render_history_lines(60);
        assert_eq!(r.segments.len(), 2);
        // 每段：首行是块内容，末尾是纯空行（渲染层间隔）
        for (_, lines, _) in &r.segments {
            assert!(!lines.is_empty(), "块不得为空");
            assert!(
                crate::modes::interactive::render::messages::is_pure_gap(lines.last().unwrap()),
                "块尾应带渲染间隔空行"
            );
        }
        // total 必须等于全部可见行之和（间隔行已含在段内，不再虚高）
        let visible_total: usize = r.segments.iter().map(|(_, lines, _)| lines.len()).sum();
        assert_eq!(r.total, visible_total, "total 与实际渲染行一致");
        // 相邻块间恰有一行空行（前一节尾空行 + 后一节首行内容，无双重空行）
        let last_of_prev = r.segments[0].1.last().unwrap();
        let first_of_next = &r.segments[1].1[0];
        assert!(crate::modes::interactive::render::messages::is_pure_gap(
            last_of_prev
        ));
        assert!(!crate::modes::interactive::render::messages::is_pure_gap(
            first_of_next
        ));
    }

    #[test]
    fn render_history_cache_invalidates_on_param_change() {
        let mut app = App::new();
        for i in 0..3 {
            app.messages
                .push(AgentMessage::user_text(&format!("msg {}", i)));
        }
        let r1 = app.render_history_lines(60);
        let ptr60 = r1.segments[0].1.clone();
        // 宽度变化 → 整体失效重建
        let r2 = app.render_history_lines(80);
        assert!(
            !std::sync::Arc::ptr_eq(&ptr60, &r2.segments[0].1),
            "宽度变化必须重建缓存"
        );
        // 展开状态变化 → 整体失效重建
        let ptr80 = r2.segments[0].1.clone();
        app.expand_all = !app.expand_all;
        let r3 = app.render_history_lines(80);
        assert!(
            !std::sync::Arc::ptr_eq(&ptr80, &r3.segments[0].1),
            "expand_all 变化必须重建缓存"
        );
        // thinking 显隐变化 → 整体失效重建
        let ptr_e = r3.segments[0].1.clone();
        app.show_thinking = !app.show_thinking;
        let r4 = app.render_history_lines(80);
        assert!(
            !std::sync::Arc::ptr_eq(&ptr_e, &r4.segments[0].1),
            "show_thinking 变化必须重建缓存"
        );
    }

    #[test]
    fn render_history_cache_invalidates_on_theme_change() {
        // /theme 切换后渲染缓存必须整体失效，否则旧主题颜色的块残留（不同主题混显）
        let mut app = App::new();
        for i in 0..3 {
            app.messages
                .push(AgentMessage::user_text(&format!("msg {}", i)));
        }
        let r1 = app.render_history_lines(60);
        let ptr = r1.segments[0].1.clone();
        assert_eq!(app.msg_cache.len(), 3);

        // 主题颜色变量变化（等价 /theme 切到另一套 vars）→ 整体失效重建
        let mut other = app.theme.clone();
        other
            .vars
            .insert("accent".to_string(), "#ff0000".to_string());
        app.theme = other;
        let r2 = app.render_history_lines(60);
        assert!(
            !std::sync::Arc::ptr_eq(&ptr, &r2.segments[0].1),
            "主题变化必须重建缓存"
        );
        assert_eq!(app.msg_cache.len(), 3, "重建后缓存应重新填充");

        // 同主题重渲染 → 再次命中缓存
        let r3 = app.render_history_lines(60);
        assert!(
            std::sync::Arc::ptr_eq(&r2.segments[0].1, &r3.segments[0].1),
            "主题未变时同参数应命中缓存"
        );
    }

    #[test]
    fn message_end_append_keeps_history_cache() {
        // 普通追加（message_end 用户消息/助手消息）不影响已渲染历史 → 不清缓存。
        // 时间戳显式指定：first=1 < later=2，保证时间线顺序稳定。
        let mut app = App::new();
        let mut first = AgentMessage::user_text("first");
        first.timestamp = 1;
        app.messages.push(first);
        let r1 = app.render_history_lines(60);
        let ptr = r1.segments[0].1.clone();
        let ev: Value = serde_json::json!({
            "type": "message_end",
            "message": {
                "role": "user",
                "content": [{ "type": "text", "text": "later" }],
                "timestamp": 2
            }
        });
        app.apply_sink_event(&ev);
        assert_eq!(app.msg_cache.len(), 1, "追加消息不触发整体失效");
        let r2 = app.render_history_lines(60);
        assert!(
            std::sync::Arc::ptr_eq(&ptr, &r2.segments[0].1),
            "旧消息缓存仍命中"
        );
        assert_eq!(r2.segments.len(), 2);
    }

    #[test]
    fn build_highlight_snapshot_forces_full_highlight() {
        // 恢复启动快照：主题强制 syntax_highlight=true（后台补全用完整高亮重渲染）
        let mut app = App::new();
        app.messages.push(AgentMessage::user_text("x"));
        app.theme.syntax_highlight = false;
        app.cache_width = 60;
        let snap = app.build_highlight_snapshot();
        assert!(snap.theme.syntax_highlight, "快照主题必须开启高亮");
        assert_eq!(snap.msgs.len(), 1);
        assert_eq!(snap.width, 60);
    }

    fn segment_text(r: &messages::HistoryRender) -> String {
        r.segments
            .iter()
            .flat_map(|(_, lines, _)| lines.iter())
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn collapsible_summary_registers_click_target_and_expands_per_item() {
        // branch/compaction 摘要：默认折叠，登记为可点击条目；单条展开只影响自己
        let mut app = App::new();
        let mut summary = AgentMessage::user_text("SUMMARYBODY");
        summary.role = "compactionSummary".to_string();
        app.messages.push(summary);
        app.messages.push(AgentMessage::user_text("plain user"));

        let r1 = app.render_history_lines(60);
        assert_eq!(r1.click_targets.len(), 1, "只有摘要条目可点击");
        let id = r1.click_targets[0].id;
        let body = r1.click_targets[0].end - r1.click_targets[0].start;
        assert!(body > 0, "命中区非空");
        assert!(!segment_text(&r1).contains("SUMMARYBODY"), "默认折叠");
        assert!(segment_text(&r1).contains("Ctrl+O/Alt+click to expand"));

        // 普通用户消息不得登记为可点击块
        assert_eq!(messages::collapsible_blocks(&app.messages[1]).len(), 0);
        assert_eq!(id.block, 0, "摘要块序号为 0");

        app.expand_overrides.insert(id, true);
        app.msg_cache.remove(&(id.ts, id.seq));
        let r2 = app.render_history_lines(60);
        assert!(segment_text(&r2).contains("SUMMARYBODY"), "展开后显示正文");
        assert!(r2.total > r1.total, "展开增加总行数");

        // 仅该条目失效：普通消息不得重渲染（命中缓存）
        assert!(std::sync::Arc::ptr_eq(&r1.segments[1].1, &r2.segments[1].1));
    }

    /// assistant 消息：thinking run 与工具块各自独立可折叠（块序号 0 / 1）
    #[test]
    fn assistant_thinking_and_tool_blocks_toggle_independently() {
        let mut app = App::new();
        app.show_thinking = false; // 缺省折叠 thinking，断言确定
        let mut m = AgentMessage::user_text("");
        m.role = "assistant".to_string();
        m.content = vec![
            ContentBlock::Thinking {
                thinking: "THINKBODY one\n\nTHINKBODY two".to_string(),
                thinking_signature: None,
                redacted: None,
            },
            ContentBlock::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({ "command": "echo hi" }),
                thought_signature: None,
                namespace: None,
            },
        ];
        app.messages.push(m);
        let mut res = AgentMessage::user_text(
            &(0..40)
                .map(|i| format!("outline {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("t1".into());
        res.timestamp = u64::MAX;
        app.messages.push(res);

        let r1 = app.render_history_lines(60);
        assert_eq!(r1.click_targets.len(), 2, "thinking run + 工具块各一个");
        let think = r1.click_targets[0].id;
        let tool = r1.click_targets[1].id;
        assert_eq!((think.block, tool.block), (0, 1), "块序号按渲染顺序");
        assert_eq!((think.ts, think.seq), (tool.ts, tool.seq), "同属一条消息");
        let text1 = segment_text(&r1);
        assert!(
            !text1.contains("THINKBODY"),
            "thinking 默认折叠（show_thinking=false）"
        );
        assert!(
            text1.contains("ctrl+o/alt+click to expand"),
            "工具输出默认折叠"
        );

        // 只展开 thinking：工具块仍折叠
        app.expand_overrides.insert(think, true);
        app.msg_cache.remove(&(think.ts, think.seq));
        let r2 = app.render_history_lines(60);
        let text2 = segment_text(&r2);
        assert!(text2.contains("THINKBODY"), "thinking 展开");
        assert!(
            text2.contains("ctrl+o/alt+click to expand"),
            "工具块未受影响"
        );
        assert!(r2.total > r1.total);

        // 再展开工具块：两者都开
        app.expand_overrides.insert(tool, true);
        app.msg_cache.remove(&(tool.ts, tool.seq));
        let r3 = app.render_history_lines(60);
        let text3 = segment_text(&r3);
        assert!(text3.contains("THINKBODY"));
        assert!(text3.contains("outline 39"), "工具输出全量展开");
        assert!(!text3.contains("ctrl+o/alt+click to expand"));
        assert!(r3.total > r2.total);
    }

    /// 全局开关（Ctrl+O / Ctrl+T）覆盖逐块状态：与 pi `setToolsExpanded` /
    /// `setHideThinkingBlock` 同语义——各自只重置自己那一类块。
    #[test]
    fn global_toggles_overwrite_per_block_overrides_like_pi() {
        let mut app = App::new();
        let mut m = AgentMessage::user_text("");
        m.role = "assistant".to_string();
        m.content = vec![
            ContentBlock::Thinking {
                thinking: "THINKBODY one\n\nTHINKBODY two".to_string(),
                thinking_signature: None,
                redacted: None,
            },
            ContentBlock::ToolCall {
                id: "t1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({ "command": "echo hi" }),
                thought_signature: None,
                namespace: None,
            },
        ];
        app.messages.push(m);
        let mut res = AgentMessage::user_text(
            &(0..40)
                .map(|i| format!("outline {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        res.role = "toolResult".to_string();
        res.tool_call_id = Some("t1".into());
        res.timestamp = u64::MAX;
        app.messages.push(res);

        let r = app.render_history_lines(60);
        let think = r.click_targets[0].id;
        let tool = r.click_targets[1].id;

        // 逐块反向覆盖：thinking 收起、工具块展开
        app.expand_overrides.insert(think, false);
        app.expand_overrides.insert(tool, true);
        app.msg_cache.clear();
        let r2 = app.render_history_lines(60);
        assert!(!segment_text(&r2).contains("THINKBODY"));
        assert!(segment_text(&r2).contains("outline 39"));

        // Ctrl+O：覆盖所有非 thinking 块 → 工具块逐块覆盖被丢弃，thinking 覆盖保留
        app.toggle_expand_all();
        assert!(app.expand_all);
        assert_eq!(
            app.expand_overrides.get(&think),
            Some(&false),
            "thinking 覆盖保留"
        );
        assert_eq!(
            app.expand_overrides.get(&tool),
            None,
            "工具块覆盖被全局值覆盖"
        );
        let t3 = segment_text(&app.render_history_lines(60));
        assert!(!t3.contains("THINKBODY"), "thinking 仍按自己的覆盖（收起）");
        assert!(t3.contains("outline 39"), "工具块随全局展开");

        // Ctrl+T：清空 thinking 覆盖，不影响非 thinking
        app.toggle_show_thinking();
        assert!(!app.show_thinking);
        assert_eq!(
            app.expand_overrides.get(&think),
            None,
            "thinking 覆盖被清空"
        );
        let t4 = segment_text(&app.render_history_lines(60));
        assert!(!t4.contains("THINKBODY"), "thinking 跟随全局显隐");
    }

    #[test]
    fn skill_user_message_is_collapsible() {
        let mut app = App::new();
        app.messages.push(AgentMessage::user_text(
            "<skill name=\"demo\" location=\"/tmp/demo\">\nBODYTEXT\n</skill>",
        ));
        let r = app.render_history_lines(60);
        assert_eq!(r.click_targets.len(), 1, "skill 调用块可点击");
        assert!(!segment_text(&r).contains("BODYTEXT"), "默认折叠");
        let id = r.click_targets[0].id;
        app.expand_overrides.insert(id, true);
        app.msg_cache.remove(&(id.ts, id.seq));
        let r2 = app.render_history_lines(60);
        assert!(
            segment_text(&r2).contains("BODYTEXT"),
            "展开后显示 skill 正文"
        );
    }
}

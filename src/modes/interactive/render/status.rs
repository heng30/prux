//! 状态栏与底栏渲染

use super::super::app::App;
use crate::{
    core::extensions::{self, BannerCtx, BannerLine, FooterCtx, FooterLine},
    utils::display::truncate_display,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// 状态区（消息区与输入框之间的 1 行间隔；
/// 忙碌：braille spinner 帧（accent）+ 状态文本（muted）；帧推进 80ms，
/// 帧序列的 DEFAULT_FRAMES；区域 ≥2 行时保留 Loader 的首行空位；
/// 空闲：整区空行
pub fn render_status_bar(frame: &mut Frame, area: Rect, app: &App) {
    let width = area.width as usize;
    let mut lines = Vec::new();

    if app.busy {
        // 忙碌：braille spinner 帧（accent）+ 状态文本（muted）
        lines.extend(render_busy_status(app, area, width));
    } else if !app.status.is_empty() && app.status_deadline.is_some_and(|dl| Instant::now() < dl) {
        // 空闲临时提示（Ctrl+X 复制、OAuth 等一次性反馈）：muted 单行
        lines.extend(render_temporary_status(app, width));
    }

    // 填充剩余行（空闲时整区为空）
    while lines.len() < area.height as usize {
        lines.push(Line::from(Span::raw(" ".repeat(width.saturating_sub(1)))));
    }

    let para = Paragraph::new(lines).style(Style::default());
    frame.render_widget(para, area);
}

/// 渲染忙碌态：spinner 帧（accent）+ 状态文本（muted）；含 2s 临时提示到点回落
fn render_busy_status(app: &App, area: Rect, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let (frames, base_text) = (app.working_kind.spinner(), app.working_kind.label());
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let frame_ch = frames[(ms / 80) as usize % frames.len()];
    let spinner = app.theme.style("accent", "#8abeb7");
    let text = app.theme.style("muted", "#808080");

    // 2s 临时提示（set_status_msg 带 deadline）到点立即隐藏：渲染侧直接按
    // deadline 判断，不依赖事件循环 tick 的过期清理（流式高频事件下 select!
    // 的 sleep 分支会被推迟，提示会残留超过 2s）。隐藏后回落忙碌基态文案
    // Working...，状态区不得整体空白、spinner 不得消失。
    let toast_expired = app.status_deadline.is_some_and(|dl| Instant::now() >= dl);
    // 压缩/branch/retry 状态不用默认 Working 兜底（保留具体文案）
    let base_shown = if app.status.is_empty() {
        base_text
    } else {
        app.status.as_str()
    };
    let shown = if toast_expired { base_text } else { base_shown };
    let (status, _) = truncate_display(shown, width.saturating_sub(4));

    // 输出 ["", spinner+msg]：首行空 + 状态行；
    // 状态区压缩为 1 行时省略首行空位，直接渲染 spinner+状态
    if area.height >= 2 {
        lines.push(Line::from(""));
    }

    for l in status.split('\n') {
        if lines.len() >= area.height as usize {
            break;
        }

        lines.push(Line::from(vec![
            Span::styled(frame_ch.to_string(), spinner),
            Span::styled(format!(" {}", l.trim_start()), text),
        ]));
    }
    lines
}

/// 渲染空闲临时提示（Ctrl+X 复制、OAuth 等一次性反馈）：muted 单行
fn render_temporary_status(app: &App, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let text = app.theme.style("muted", "#808080");
    let (line, _) = truncate_display(&app.status, width.saturating_sub(1));
    lines.push(Line::from(Span::styled(line, text)));
    lines
}

/// 收集底栏渲染行：遍历已注册 footer 扩展，取第一个提供渲染的扩展
/// 无扩展注册（或全部被 /extension 禁用）时返回空——底栏不渲染、不留空行。
pub fn collect_footer_lines(app: &App, width: usize) -> Vec<FooterLine> {
    let footers = extensions::registered_footers();
    let Some(ext) = footers.first() else {
        return Vec::new();
    };

    let ctx = FooterCtx {
        width,
        cwd: &app.cwd,
        model: if app.no_model_available {
            None
        } else {
            app.current_model.as_deref()
        },
        model_reasoning: app.model_reasoning,
        thinking_level: app.thinking_level.as_deref(),
        routed_model: app.routed_model.as_deref(),
        routed_thinking_level: app.routed_thinking_level.as_deref(),
        git_branch: app.git_branch.as_deref(),
        context_percent: app.context_percent,
        context_window: app.context_window,
        busy: app.busy,
        streaming_output_tokens: app.streaming_output_tokens(),
        messages: &app.messages,
    };
    ext.render(&ctx)
}

/// 收集横幅渲染行：仅“干净启动”时返回 banner 扩展的渲染行——
/// 用户尚未提交过输入（`banner_hidden` 未置位）、会话无历史消息、无启动初始消息，
/// 且存在已启用（当前模式可用）的 banner 扩展。
/// 其余情况返回空——banner 不渲染、不留空行（布局不为其保留高度）。
pub fn collect_banner_lines(app: &App, width: usize, height: usize) -> Vec<BannerLine> {
    if app.banner_hidden || !app.messages.is_empty() || !app.initial_queue.is_empty() {
        return Vec::new();
    }
    let banners = extensions::registered_banners();
    let Some(ext) = banners.first() else {
        return Vec::new();
    };

    ext.render(&BannerCtx { width, height })
}

/// 横幅：由 banner 扩展渲染（窗口顶部，消息区之上）。
/// 无扩展注册或非干净启动时 lines 为空，直接跳过、不占布局。
pub fn render_top_bar(frame: &mut Frame, area: Rect, app: &App, lines: &[BannerLine]) {
    if lines.is_empty() {
        return;
    }

    let width = area.width as usize;
    let mut ratatui_lines: Vec<Line<'static>> = Vec::new();
    for raw in lines.iter().take(area.height as usize) {
        let spans: Vec<Span<'static>> = raw
            .iter()
            .map(|s| {
                let mut style = app.theme.style(s.key, s.fallback);
                if s.bold {
                    style = style.add_modifier(ratatui::style::Modifier::BOLD);
                }
                Span::styled(s.text.clone(), style)
            })
            .collect();
        ratatui_lines.push(Line::from(spans));
    }

    // 填充剩余行，避免残留
    while ratatui_lines.len() < area.height as usize {
        ratatui_lines.push(Line::from(Span::raw(" ".repeat(width.saturating_sub(1)))));
    }

    let para = Paragraph::new(ratatui_lines).style(Style::default());
    frame.render_widget(para, area);
}

/// 底栏：由 footer 扩展渲染（不再显示内置快捷键提示）。
/// 无扩展注册时 lines 为空，直接跳过。
pub fn render_bottom_bar(frame: &mut Frame, area: Rect, app: &App, lines: &[FooterLine]) {
    if lines.is_empty() {
        return;
    }

    let width = area.width as usize;
    let mut ratatui_lines: Vec<Line<'static>> = Vec::new();
    for raw in lines.iter().take(area.height as usize) {
        let spans: Vec<Span<'static>> = raw
            .iter()
            .map(|s| Span::styled(s.text.clone(), app.theme.style(s.key, s.fallback)))
            .collect();
        ratatui_lines.push(Line::from(spans));
    }

    // 填充剩余行，避免残留
    while ratatui_lines.len() < area.height as usize {
        ratatui_lines.push(Line::from(Span::raw(" ".repeat(width.saturating_sub(1)))));
    }
    let para = Paragraph::new(ratatui_lines).style(Style::default());
    frame.render_widget(para, area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::app::App;
    use crate::modes::interactive::app::WorkingKind;

    #[test]
    fn busy_status_matches_pi_loader_shape() {
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        app.busy = true;
        app.status = "Working...".to_string();
        let backend = TestBackend::new(40, 2);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 2);
                render_status_bar(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        // 首行空（对齐 pi Loader render 输出 ["", spinner+msg]）
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), " ");
        // 第二行：braille 帧 + 状态文本
        let first = buf.cell((0, 1)).unwrap().symbol().to_string();
        let frames = crate::utils::glyphs::DEF_SPINNER_BRAILLE;
        assert!(
            frames.contains(&first.as_str()),
            "braille spinner: {:?}",
            first
        );
        let row1: String = (0..40)
            .map(|c| buf.cell((c, 1)).unwrap().symbol().to_string())
            .collect();
        assert!(row1.contains("Working..."), "row: {:?}", row1);
    }

    #[test]
    fn busy_status_msg_hidden_after_deadline() {
        // 回归：busy 时 2s 临时提示（set_status_msg）到点必须隐藏，回落 Working...；
        // 渲染侧直接按 deadline 判断，不依赖事件循环 tick 过期清理。
        use ratatui::backend::TestBackend;
        use std::time::Duration;
        let mut app = App::new();
        app.busy = true;
        app.status = "error: tool failed".to_string();
        app.status_deadline = Some(Instant::now() - Duration::from_secs(1));
        let backend = TestBackend::new(40, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 1);
                render_status_bar(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let row: String = (0..40)
            .map(|c| buf.cell((c, 0)).unwrap().symbol().to_string())
            .collect();
        assert!(!row.contains("error"), "过期提示应隐藏: {:?}", row);
        assert!(row.contains("Working..."), "回落忙碌基态: {:?}", row);
    }

    #[test]
    fn busy_status_msg_stays_until_deadline() {
        // busy 时未过期的临时提示正常显示（spinner + 提示文本）
        use ratatui::backend::TestBackend;
        use std::time::Duration;
        let mut app = App::new();
        app.busy = true;
        app.status = "thinking blocks hidden (Ctrl+T to show)".to_string();
        app.status_deadline = Some(Instant::now() + Duration::from_secs(2));
        let backend = TestBackend::new(60, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 60, 1);
                render_status_bar(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let row: String = (0..60)
            .map(|c| buf.cell((c, 0)).unwrap().symbol().to_string())
            .collect();
        assert!(
            row.contains("thinking blocks hidden"),
            "未过期提示应显示: {:?}",
            row
        );
    }

    #[test]
    fn idle_status_msg_shown_until_deadline() {
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        app.set_status_msg("Copied last agent message to clipboard");
        let backend = TestBackend::new(50, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 50, 1);
                render_status_bar(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let row: String = (0..50)
            .map(|c| buf.cell((c, 0)).unwrap().symbol().to_string())
            .collect();
        assert!(
            row.contains("Copied last agent message to clipboard"),
            "row: {:?}",
            row
        );
    }

    #[test]
    fn rich_footer_event_flow_and_cost_via_apply_sink() {
        // 回归：rich footer 的交互计数/工具计数/计时依赖 agent_start /
        // tool_execution_end / agent_end 经 apply_sink_event → dispatch_agent_event
        // 到达扩展；花费数据源来自 app.messages 里 assistant 消息的 usage。
        // 此前 apply_sink_event 只更新 App 内部状态、从不转发事件到扩展，
        // 导致交互数/工具计数/计时/token 速率恒为 0、第二行工具计数空白。
        // 用 render 返回空的 spy footer 验证事件链路（返回空则不占用底栏布局，
        // 避免影响并行的 render_frame 测试）；RichFooter 聚合逻辑由单测覆盖。
        use crate::core::extensions::{
            ExtensionMode, FooterCtx, FooterLine, register_footer_extension, set_extension_enabled,
            set_extension_mode,
        };
        use crate::core::provider::{AgentMessage, Cost, Usage};
        use crate::modes::interactive::app::App;
        use crate::test_support::AUTH_TEST_LOCK;
        use serde_json::json;

        struct SpyFooter {
            seen: std::sync::mpsc::Sender<String>,
        }
        impl crate::core::extensions::FooterExtension for SpyFooter {
            fn name(&self) -> &str {
                "rich-flow-spy"
            }
            fn on_agent_event(&self, event: &serde_json::Value) {
                if let Some(t) = event.get("type").and_then(|v| v.as_str()) {
                    let _ = self.seen.send(t.to_string());
                }
            }
            fn render(&self, _ctx: &FooterCtx) -> Vec<FooterLine> {
                Vec::new()
            }
        }

        let _g = AUTH_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<String>();
        register_footer_extension(SpyFooter { seen: seen_tx });

        let mut app = App::new();
        app.apply_sink_event(&json!({ "type": "agent_start" }));
        app.apply_sink_event(&json!({ "type": "tool_execution_end", "toolName": "bash" }));
        let mut m = AgentMessage::user_text("");
        m.role = "assistant".to_string();
        m.usage = Some(Usage {
            input: 1000,
            output: 500,
            cache_read: 300,
            cost: Cost {
                total: 0.0123,
                ..Cost::default()
            },
            ..Usage::default()
        });
        app.apply_sink_event(
            &json!({ "type": "message_end", "message": serde_json::to_value(&m).unwrap() }),
        );
        app.apply_sink_event(&json!({ "type": "agent_end" }));

        // 事件已转发到 footer 扩展（交互计数/工具计数/计时的数据源）
        let types: Vec<String> = seen_rx.try_iter().collect();
        for want in ["agent_start", "tool_execution_end", "agent_end"] {
            assert!(types.iter().any(|t| t == want), "缺事件 {want}: {types:?}");
        }

        // 花费数据源：assistant 消息 usage 落地到 app.messages
        let last = app.messages.last().expect("assistant message pushed");
        assert_eq!(last.role, "assistant");
        let u = last.usage.as_ref().expect("assistant usage recorded");
        assert!(u.cost.total > 0.0, "cost from messages usage");

        // 清理全局注册表，避免污染并行测试
        set_extension_enabled("rich-flow-spy", false);
    }

    #[test]
    fn busy_status_shows_sharing_kind() {
        use ratatui::backend::TestBackend;
        let mut app = App::new();
        app.busy = true;
        app.working_kind = WorkingKind::Sharing;
        let backend = TestBackend::new(40, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 1);
                render_status_bar(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let first = buf.cell((0, 0)).unwrap().symbol().to_string();
        let frames = crate::utils::glyphs::DEF_SPINNER_BRAILLE;
        assert!(frames.contains(&first.as_str()), "spinner: {:?}", first);
        let row: String = (0..40)
            .map(|c| buf.cell((c, 0)).unwrap().symbol().to_string())
            .collect();
        assert!(row.contains("Sharing session..."), "row: {:?}", row);
    }

    #[test]
    fn idle_status_msg_hidden_after_deadline() {
        use ratatui::backend::TestBackend;
        use std::time::Duration;
        let mut app = App::new();
        app.status = "stale hint".to_string();
        app.status_deadline = Some(Instant::now() - Duration::from_secs(1));
        let backend = TestBackend::new(30, 1);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 30, 1);
                render_status_bar(f, area, &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let row: String = (0..30)
            .map(|c| buf.cell((c, 0)).unwrap().symbol().to_string())
            .collect();
        assert!(!row.contains("stale"), "row: {:?}", row);
    }

    #[test]
    fn banner_clean_startup_lifecycle() {
        // banner 可见性：仅干净启动（未提交输入 + 无历史 + 无启动初始消息）且扩展启用时
        // 才返回渲染行；其余状态一律为空（不占布局）。spy 用超宽门控避免干扰并行的
        // 整帧渲染测试（它们在 <900 列宽下跑，永不命中 spy）。
        use crate::core::extensions::{
            BannerCtx, BannerExtension, BannerLine, BannerSpan, ExtensionMode,
            register_banner_extension, set_extension_enabled, set_extension_mode,
        };
        use crate::core::provider::AgentMessage;
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        struct SpyBanner;
        impl BannerExtension for SpyBanner {
            fn name(&self) -> &str {
                "banner-clean-spy"
            }
            fn render(&self, ctx: &BannerCtx) -> Vec<BannerLine> {
                if ctx.width < 900 {
                    return Vec::new();
                }
                vec![
                    vec![BannerSpan::new("accent", "#8abeb7", "TOP-CLEAN-1")],
                    vec![BannerSpan::new("accent", "#8abeb7", "TOP-CLEAN-2")],
                ]
            }
        }
        register_banner_extension(SpyBanner);

        let mut app = App::new();
        // 干净启动：显示
        let lines = collect_banner_lines(&app, 1000, 30);
        assert_eq!(lines.len(), 2, "干净启动应渲染 banner");

        // 提交输入后（banner_hidden）：隐藏
        app.banner_hidden = true;
        assert!(collect_banner_lines(&app, 1000, 30).is_empty());
        app.banner_hidden = false;

        // 恢复历史（resume/continue）：隐藏
        app.messages.push(AgentMessage::user_text("history"));
        assert!(collect_banner_lines(&app, 1000, 30).is_empty());
        app.messages.clear();

        // CLI 启动初始消息：隐藏
        app.initial_queue.push_back(AgentMessage::user_text("hi"));
        assert!(collect_banner_lines(&app, 1000, 30).is_empty());
        app.initial_queue.clear();

        // 扩展被禁用：隐藏
        assert!(set_extension_enabled("banner-clean-spy", false));
        assert!(collect_banner_lines(&app, 1000, 30).is_empty());
        // 恢复并清盘
        assert!(set_extension_enabled("banner-clean-spy", true));
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }

    #[test]
    fn banner_renders_at_top_of_frame_until_submit() {
        // 整帧渲染：干净启动时 banner 占据消息区之上的顶部行；首次提交（banner_hidden）
        // 后消失且布局让位（消息区回到顶部）。spy 宽门控隔离并行整帧测试。
        use crate::core::extensions::{
            BannerCtx, BannerExtension, BannerLine, BannerSpan, ExtensionMode,
            register_banner_extension, set_extension_enabled, set_extension_mode,
        };
        use crate::modes::interactive::render_frame;
        use ratatui::backend::TestBackend;
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        struct SpyBanner;
        impl BannerExtension for SpyBanner {
            fn name(&self) -> &str {
                "banner-frame-spy"
            }
            fn render(&self, ctx: &BannerCtx) -> Vec<BannerLine> {
                if ctx.width < 900 {
                    return Vec::new();
                }
                vec![
                    vec![BannerSpan::new("accent", "#8abeb7", "TOP-FRAME-1")],
                    vec![BannerSpan::new("accent", "#8abeb7", "TOP-FRAME-2")],
                ]
            }
        }
        register_banner_extension(SpyBanner);

        let mut app = App::new();
        let mut terminal = ratatui::Terminal::new(TestBackend::new(1000, 30)).unwrap();
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..1000)
                .map(|c| buf.cell((c, y)).unwrap().symbol().to_string())
                .collect()
        };
        assert!(
            row(0).trim().contains("TOP-FRAME-1"),
            "banner 首行在窗口顶部: {:?}",
            &row(0)[..80]
        );
        assert!(
            row(1).trim().contains("TOP-FRAME-2"),
            "banner 第二行紧随其后: {:?}",
            &row(1)[..80]
        );

        // 首次提交后：banner 消失，消息区回到顶部（单元格重新渲染，顶部不再有 art）
        app.banner_hidden = true;
        terminal.draw(|f| render_frame(f, &mut app)).unwrap();
        let buf = terminal.backend().buffer();
        let top: String = (0..3)
            .flat_map(|y| (0..1000).map(move |c| buf.cell((c, y)).unwrap().symbol().to_string()))
            .collect();
        assert!(
            !top.contains("TOP-FRAME"),
            "提交后 banner 应隐藏: {:?}",
            &top[..200]
        );
        // 清盘
        assert!(set_extension_enabled("banner-frame-spy", false));
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
    }
}

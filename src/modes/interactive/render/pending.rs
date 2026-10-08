//! 待发送消息显示区
//!
//! busy 时 Enter 排队的 steer 在此列出（每行一条 `Steering: xxx`，dim），
//! 底部附 `↳ Alt+Up to edit all queued messages` 提示；
//! 无排队时整区高度为 0，不占布局。区位于消息区与状态区之间

use super::super::app::App;
use crate::utils::{display::truncate_display, glyphs::DEF_NESTED};
use ratatui::{
    Frame,
    layout::Rect,
    text::{Line, Span},
    widgets::Paragraph,
};

/// 待发送区行数：空队列为 0（不占布局）；有内容 = 空行 + N 条消息 + 提示行
pub fn pending_lines(app: &App) -> usize {
    let steer_count = app.runtime_steer_inbox.lock().unwrap().len();
    let follow_up_count = app.runtime_followup_inbox.len();
    let total = steer_count + follow_up_count;
    if total == 0 {
        return 0;
    }

    // 每条消息截断为单行，行数不随宽度变化
    1 + total + 1
}

/// 渲染待发送区（布局位于消息区与状态区之间）
pub fn render_pending(frame: &mut Frame, area: Rect, app: &App) {
    // steer = 运行中注入箱尚未注入的；follow-up = 本地队列（settle 后发送）
    let steers: Vec<String> = app
        .runtime_steer_inbox
        .lock()
        .unwrap()
        .iter()
        .map(|m| m.text())
        .collect();
    if steers.is_empty() && app.runtime_followup_inbox.is_empty() {
        return;
    }
    let width = area.width as usize;

    // 用 "dim" 主题键，自定义主题（如 synthwave 定义 dim=comment）才能正确命中
    let dim = app.theme.style("dim", "#666666");
    let mut lines: Vec<Line<'static>> = Vec::new();

    // 空行分隔
    lines.push(Line::from(Span::raw(" ".repeat(width.saturating_sub(1)))));

    // steer 显示 "Steering:"，follow-up 显示 "Follow-up:"，
    // 先全部 steer 再 follow-up。每条取首行并截断为一行。
    for text in &steers {
        let first = text.split('\n').next().unwrap_or("");
        let (line, _) = truncate_display(&format!("Steering: {}", first), width.saturating_sub(1));
        lines.push(Line::from(Span::styled(line, dim)));
    }

    for text in &app.runtime_followup_inbox {
        let first = text.split('\n').next().unwrap_or("");
        let (line, _) = truncate_display(&format!("Follow-up: {}", first), width.saturating_sub(1));
        lines.push(Line::from(Span::styled(line, dim)));
    }

    // 提示行：Alt+Up 取回全部排队消息
    let (hint, _) = truncate_display(
        &format!("{DEF_NESTED} Alt+Up to edit all queued messages"),
        width.saturating_sub(1),
    );

    lines.push(Line::from(Span::styled(hint, dim)));
    frame.render_widget(Paragraph::new(lines), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::interactive::app::App;

    #[test]
    fn empty_queue_renders_nothing() {
        let app = App::new();
        assert_eq!(pending_lines(&app), 0);
    }

    #[test]
    fn queued_steers_render_one_line_each_plus_hint() {
        let app = App::new();
        app.runtime_steer_inbox.lock().unwrap().push_back(
            crate::core::provider::AgentMessage::user_text("continue faster"),
        );
        app.runtime_steer_inbox.lock().unwrap().push_back(
            crate::core::provider::AgentMessage::user_text("and fix the leak"),
        );
        assert_eq!(pending_lines(&app), 4, "空行 + 2 条 steer + 提示行");

        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(50, 4);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_pending(f, Rect::new(0, 0, 50, 4), &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..50)
                .map(|c| buf.cell((c, y)).unwrap().symbol().to_string())
                .collect()
        };
        assert_eq!(row(0).trim(), "", "首行空（对齐 pi Spacer(1)）");
        assert!(
            row(1).contains("Steering: continue faster"),
            "row1: {:?}",
            row(1)
        );
        assert!(
            row(2).contains("Steering: and fix the leak"),
            "row2: {:?}",
            row(2)
        );
        assert!(
            row(3).contains("↳ Alt+Up to edit all queued messages"),
            "row3: {:?}",
            row(3)
        );
    }

    #[test]
    fn follow_up_renders_with_follow_up_label_after_steers() {
        // 对齐 pi：follow-up 显示 "Follow-up:"，且排在全部 steer 之后
        let mut app = App::new();
        app.runtime_steer_inbox
            .lock()
            .unwrap()
            .push_back(crate::core::provider::AgentMessage::user_text("steer one"));
        app.runtime_followup_inbox =
            std::collections::VecDeque::from(vec!["fup first".to_string(), "fup last".to_string()]);
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(50, 5);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_pending(f, Rect::new(0, 0, 50, 5), &app);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let row = |y: u16| -> String {
            (0..50)
                .map(|c| buf.cell((c, y)).unwrap().symbol().to_string())
                .collect()
        };
        assert!(row(1).contains("Steering: steer one"), "row1: {:?}", row(1));
        assert!(
            row(2).contains("Follow-up: fup first"),
            "row2: {:?}",
            row(2)
        );
        assert!(row(3).contains("Follow-up: fup last"), "row3: {:?}", row(3));
    }

    #[test]
    fn multiline_steer_collapses_to_first_line() {
        let app = App::new();
        app.runtime_steer_inbox.lock().unwrap().push_back(
            crate::core::provider::AgentMessage::user_text("first line\nsecond line"),
        );
        use ratatui::backend::TestBackend;
        let backend = TestBackend::new(30, 3);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| render_pending(f, Rect::new(0, 0, 30, 3), &app))
            .unwrap();
        let buf = terminal.backend().buffer();
        let row1: String = (0..30)
            .map(|c| buf.cell((c, 1)).unwrap().symbol().to_string())
            .collect();
        assert!(row1.contains("Steering: first line"), "row1: {:?}", row1);
        assert!(!row1.contains("second line"), "只取首行: {:?}", row1);
    }
}

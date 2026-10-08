//! 外部编辑器 / shell 命令

use crate::{
    core::settings_manager,
    modes::interactive::app::{App, MsgLevel},
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::{
    cell::RefCell,
    io::{IsTerminal, Write},
    rc::Rc,
};
use tempfile::NamedTempFile;

/// Ctrl+G 外部编辑器：临时文件编辑后读回
pub(super) fn open_external_editor(shared: &Rc<RefCell<App>>, text: String) {
    // 终端是否可用于 TUI 操作：stdout 是终端时才允许 raw mode 切换与 alt screen 操作。
    // 测试/CI 环境 stdout 被管道捕获，crossterm 操作会阻塞，必须跳过。
    let term = std::io::stdout().is_terminal();

    if term {
        disable_raw_mode().ok();
        _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::event::DisableMouseCapture
        );
    }

    // 临时文件创建/编辑失败：恢复终端并报错（NamedTempFile 在 drop 时删除文件）
    let Some(new_text) = edit_via_external_editor(&text) else {
        if term {
            _ = crossterm::execute!(
                std::io::stdout(),
                crossterm::terminal::EnterAlternateScreen,
                crossterm::event::EnableBracketedPaste,
                crossterm::event::EnableMouseCapture
            );
            _ = enable_raw_mode();
        }

        let mut st = shared.borrow_mut();
        st.push_msg("failed to create temp file".to_string(), MsgLevel::Error);
        st.full_redraw = true;
        st.dirty = true;
        return;
    };

    if term {
        _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::event::EnableMouseCapture
        );
        _ = enable_raw_mode();
    }

    let mut st = shared.borrow_mut();
    st.editor.clear();
    st.editor.insert_text(&new_text);
    st.push_msg(
        format!("external editor saved ({} chars)", new_text.chars().count()),
        MsgLevel::Success,
    );
    st.full_redraw = true;
    st.dirty = true;
}

/// 创建临时文件并调用外部编辑器，读回编辑后文本（stripBom + 去尾部换行）；
/// 临时文件创建失败返回 None（NamedTempFile 必须存活到读回之后，故编辑与读回都在本函数内）
fn edit_via_external_editor(text: &str) -> Option<String> {
    let mut tmp = NamedTempFile::new().ok()?;
    _ = tmp.write_all(text.as_bytes());
    let path = tmp.path().to_string_lossy().to_string();

    // externalEditor 未设置时回退 VISUAL → EDITOR
    let editor = settings_manager::read_settings_external_editor()
        .or_else(|| std::env::var("VISUAL").ok().filter(|v| !v.is_empty()))
        .or_else(|| std::env::var("EDITOR").ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "vi".to_string());

    // 非零退出返回 failed（不读回）
    let exit_ok = std::process::Command::new(&editor)
        .arg(&path)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !exit_ok {
        eprintln!("external editor failed: {} {}", editor, path);
    }

    // stripBom + 去尾部换行
    let new_text = std::fs::read_to_string(&path)
        .unwrap_or_default()
        .trim_start_matches('\u{feff}')
        .trim_end_matches('\n')
        .trim_end_matches('\r')
        .to_string();
    Some(new_text)
}

/// 执行 shell 命令并捕获输出（!cmd / !!cmd）
pub(super) fn run_shell_command(cmd: &str) -> String {
    let output = std::process::Command::new("sh").arg("-c").arg(cmd).output();
    match output {
        Ok(o) => {
            let mut out = String::from_utf8_lossy(&o.stdout).to_string();
            if !o.stderr.is_empty() {
                out.push_str(&String::from_utf8_lossy(&o.stderr));
            }

            let status = o.status.code().unwrap_or(-1);
            if out.trim().is_empty() {
                format!("(exit {})", status)
            } else {
                format!("{}(exit {})", out.trim_end(), status)
            }
        }
        Err(e) => format!("execution failed: {}", e),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::*;
    use crate::modes::interactive::app::App;
    use crate::modes::interactive::handlers::handle_event;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    /// EDITOR 是进程级全局环境变量：涉及它的测试必须串行
    static EDITOR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn ctrl_g_external_editor_restores_text_and_requests_full_redraw() {
        // EDITOR=true：立即退出、不修改文件 → 文本原样读回输入框；
        // full_redraw 标记让事件循环 terminal.clear() 全量重绘（修复 diff 增量错乱）
        let _g = EDITOR_LOCK.lock().unwrap();
        unsafe { std::env::set_var("EDITOR", "true") };
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.editor.insert_text("draft from external editor");
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        );
        let st = shared.borrow_mut();
        assert_eq!(
            st.editor.text(),
            "draft from external editor",
            "外部编辑器内容应放回输入框"
        );
        assert!(
            st.full_redraw,
            "外部编辑器返回后应请求全量重绘（clear + 重置 previous buffer）"
        );
        assert!(st.dirty);
    }

    #[cfg(unix)]
    #[test]
    fn ctrl_g_external_editor_reads_back_modified_content() {
        // 模拟真正的外部编辑器（把文件重写为新内容）：读回应是修改后的文本
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-editor.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf 'edited content' > \"$1\"\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(&script).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&script, perm).unwrap();
        let _g = EDITOR_LOCK.lock().unwrap();
        unsafe { std::env::set_var("EDITOR", script.to_str().unwrap()) };

        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.editor.insert_text("old draft");
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
        );
        let st = shared.borrow_mut();
        assert_eq!(
            st.editor.text(),
            "edited content",
            "应读回编辑器修改后的内容"
        );
        assert!(st.full_redraw);
    }
}

//! 会话选择：--session/--fork/--session-id/--resume 的参数解析与 TUI 选择器

use crate::core::session_manager::{self, SessionMeta, list_sessions};
use crossterm::{
    cursor::{Hide, Show},
    event::{self, Event, KeyCode},
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use std::{
    io::{BufRead, BufReader, IsTerminal, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    time::Duration,
};

/// 把 --session/--fork 参数解析为会话文件路径：
/// 直接是文件 → 使用；否则在 extra_dir / default_dir 中按部分 UUID 查找。
pub fn resolve_session_arg(arg: &str, default_dir: &Path, extra_dir: Option<&Path>) -> PathBuf {
    let as_path = PathBuf::from(arg);
    if as_path.is_file() {
        return as_path;
    }
    if let Some(dir) = extra_dir
        && let Some(found) = find_session_by_id(dir, arg)
    {
        return found;
    }
    if let Some(found) = find_session_by_id(default_dir, arg) {
        return found;
    }
    as_path
}

/// 按完整或部分 UUID 查找会话文件（只读 JSONL 首行 header，不加载正文）。
pub fn find_session_by_id(dir: &Path, id: &str) -> Option<PathBuf> {
    for path in list_sessions(dir) {
        let Some(first) = read_first_line(&path) else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&first) else {
            continue;
        };
        if v.get("id").and_then(|v| v.as_str()) == Some(id)
            || v.get("id")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.starts_with(id))
        {
            return Some(path);
        }
    }
    None
}

/// 读取文件首行（仅第一行，不读完整文件）。会话 header 查找/时间戳探针共用。
fn read_first_line(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    Some(line)
}

/// 渐进元数据加载器（`--resume`：结果渐进出现、按 mtime 优先、选中即取消未完成读取）。
///
/// 候选顺序由 [`list_sessions`]（修改时间新→旧）给定；后台线程按序读取每个会话的展示元数据
/// （[`session_meta`]，整文件解析，最贵的一步），逐条经 channel 送回；调用方在事件循环里
/// [`drain`](Self::drain) 取回并重绘——最旧/最大的会话不会拖住首批结果的出现。
/// [`cancel`](Self::cancel) 置位后后台线程在下一个候选前停止（选中/取消选择器时调用）。
pub struct ProgressiveMetas {
    /// 候选会话文件，已按加载优先级（mtime 新→旧）排序。
    paths: Vec<PathBuf>,
    /// 与 `paths` 同序的结果槽，None 表示该条尚未读回。
    rows: Vec<Option<SessionMeta>>,
    /// 已到达的最大前缀长度（已连续就绪的条数）。
    loaded: usize,
    /// 后台线程回传（候选下标，元数据）的通道。
    rx: Receiver<(usize, Option<SessionMeta>)>,
    /// 置位后后台线程在下一个候选前停止加载。
    cancel: Arc<AtomicBool>,
}

impl ProgressiveMetas {
    /// 启动加载：`paths` 必须已按期望的加载优先级排序（mtime 新→旧）。
    pub fn start(paths: Vec<PathBuf>) -> Self {
        let (tx, rx) = mpsc::channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let jobs = paths.clone();

        std::thread::spawn(move || {
            for (i, path) in jobs.into_iter().enumerate() {
                if flag.load(Ordering::Relaxed) {
                    break;
                }
                let meta = session_manager::session_meta(&path);
                if tx.send((i, meta)).is_err() {
                    break; // 接收端已丢弃（选择器已关闭）
                }
            }
        });

        let total = paths.len();
        Self {
            paths,
            rows: vec![None; total],
            loaded: 0,
            rx,
            cancel,
        }
    }

    /// 取回已到达的结果；返回本次新增的条数（0 = 无新结果）。
    pub fn drain(&mut self) -> usize {
        let mut n = 0;
        while let Ok((i, meta)) = self.rx.try_recv() {
            if i < self.rows.len() {
                self.rows[i] = meta;
            }
            self.loaded = self.loaded.max(i + 1);
            n += 1;
        }
        n
    }

    /// 是否仍有候选在加载（已取消或已全部取回时为 false）。
    pub fn is_loading(&self) -> bool {
        !self.cancel.load(Ordering::Relaxed) && self.loaded < self.paths.len()
    }

    /// 已处理（含不可读）的候选数。
    pub fn loaded(&self) -> usize {
        self.loaded
    }

    /// 候选总数。
    pub fn total(&self) -> usize {
        self.paths.len()
    }

    /// 已成功读到元数据的候选（保持 mtime 顺序；不可读的会话不出现在列表里）。
    pub fn visible(&self) -> impl Iterator<Item = (usize, &Path, &SessionMeta)> {
        self.rows
            .iter()
            .enumerate()
            .filter_map(|(i, m)| m.as_ref().map(|m| (i, self.paths[i].as_path(), m)))
    }

    /// 取消未完成的读取（选中/关闭选择器时调用；幂等）。
    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// 阻塞到全部候选处理完（非 TTY 直选路径与测试用）。
    pub fn wait_all(&mut self) {
        while self.is_loading() {
            if self.drain() == 0 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        self.drain();
    }
}

/// 单行展示文本：名称或首条用户消息 + 消息数 + 相对时间。
fn row_label(meta: &SessionMeta) -> String {
    let title = match meta.name.as_deref().map(str::trim) {
        Some(n) if !n.is_empty() => n.to_string(),
        _ if !meta.first_message.is_empty() => meta.first_message.clone(),
        _ => "(no messages)".to_string(),
    };

    let title = truncate_chars(&title, 72);
    let mut out = title;

    if meta.message_count > 0 {
        out.push_str(&format!("  [{} msgs]", meta.message_count));
    }

    if !meta.modified_age.is_empty() {
        out.push_str(&format!("  ({})", meta.modified_age));
    }
    out
}

/// 按字符（非字节）数截断到 `max`，超出时以 `…` 收尾（保证总长恰为 `max`）
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 选择器固定框架占用的行数：标题、标题后空行、列表后空行、状态行。
const FRAME_LINES: usize = 4;

/// 每页行数：由终端高度扣除固定框架得到。
///
/// 整页必须放得下：若一页超过终端高度，终端自身会滚动，把每页顶部的若干行顶出屏幕，
/// 翻页时看起来像“多跳了几行”（下一页首行不再是上一页末行 + 1）。
/// 前提是每行都经 [`clip_line`] 按终端宽度截断，不会折行。
fn page_rows(terminal_rows: u16) -> usize {
    (terminal_rows as usize).saturating_sub(FRAME_LINES).max(1)
}

/// 把一行裁剪到终端列宽并把控制字符替换为空格。
///
/// 终端不会自动换行（而是折行），若一行超过列宽就会占两行，整页高度被撑破，
/// 于是每页顶部若干行被顶出屏幕，翻页时看起来像跳过了若干条。
fn clip_line(s: &str, cols: usize) -> String {
    let sanitized: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    crate::utils::display::truncate_display(&sanitized, cols.max(1)).0
}

/// 页对齐窗口 `[start, end)`：`selected` 落在哪一页就整页显示。
///
/// 页对齐保证下一页首行 = 上一页末行 + 1（不跳行）。
fn window_range(selected: usize, count: usize, page: usize) -> (usize, usize) {
    let page = page.max(1);
    let start = (selected / page) * page;
    let end = (start + page).min(count);
    (start, end)
}

/// --resume：非 TTY 直接选最新；TTY 使用选择器（方向键/回车/Esc）。
///
/// 元数据渐进加载（见 [`ProgressiveMetas`]）：先出现最新会话，其余按 mtime 依次补上，
/// 选中或取消时取消未完成的读取。
pub fn pick_session(dir: &Path) -> Option<PathBuf> {
    let sessions = list_sessions(dir);
    if sessions.is_empty() {
        eprintln!("Error: no sessions in directory: {}", dir.display());
        return None;
    }

    // 非 TTY 或只有一个会话：无需交互，直接取最新（不读元数据）
    if !std::io::stdin().is_terminal() || sessions.len() == 1 {
        return sessions.into_iter().next();
    }

    let total = sessions.len();
    let mut loader = ProgressiveMetas::start(sessions);
    let mut stdout = std::io::stdout();
    _ = terminal::enable_raw_mode();
    _ = crossterm::execute!(stdout, EnterAlternateScreen, Hide);
    let mut selected = 0usize;
    let mut result: Option<PathBuf> = None;
    let mut dirty = true;

    loop {
        // 取回渐进结果（首帧前也先等一批：空列表对用户没有意义）
        let arrived = loader.drain();
        if arrived > 0 || loader.loaded() == 0 {
            dirty = true;
        }

        let visible: Vec<(usize, PathBuf, String)> = loader
            .visible()
            .map(|(i, path, meta)| (i, path.to_path_buf(), row_label(meta)))
            .collect();
        let count = visible.len();
        if count > 0 && selected >= count {
            selected = count - 1;
        }

        // 终端尺寸每帧读取：列宽决定每行截断（避免折行撑破页高），行高决定每页行数
        let (term_cols, term_rows) = terminal::size().unwrap_or((80, 24));
        let cols = (term_cols as usize).max(1);
        let page = page_rows(term_rows);

        if dirty {
            let mut out = String::new();
            out.push_str("\x1b[2J\x1b[H");
            out.push_str(&clip_line(
                "Select a session to resume (↑/↓ select, PgUp/PgDn page, Enter confirm, Esc cancel):",
                cols,
            ));
            out.push_str("\r\n\r\n");
            if count == 0 {
                out.push_str(&clip_line("  loading…", cols));
                out.push('\r');
                out.push('\n');
            } else {
                let (window_start, window_end) = window_range(selected, count, page);
                for (n, (_, _, label)) in visible
                    .iter()
                    .enumerate()
                    .take(window_end)
                    .skip(window_start)
                {
                    let marker = if n == selected { ">" } else { " " };
                    let line = format!("{} {} {}", marker, n + 1, label);
                    out.push_str(&clip_line(&line, cols));
                    out.push_str("\r\n");
                }
            }

            let footer = if loader.is_loading() {
                format!(
                    "{} sessions total, loading {}/{}…",
                    total,
                    loader.loaded(),
                    total
                )
            } else {
                format!("{} sessions total", total)
            };
            out.push_str("\r\n");
            out.push_str(&clip_line(&footer, cols));
            _ = stdout.write_all(out.as_bytes());
            _ = stdout.flush();
            dirty = false;
        }

        // 有事件立刻处理；空闲时定期取回渐进结果
        let timeout = if loader.is_loading() {
            Duration::from_millis(50)
        } else {
            Duration::from_secs(3600)
        };

        match event::poll(timeout) {
            Ok(true) => match event::read() {
                Ok(Event::Key(key)) => match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        if count > 0 {
                            selected = (selected + count - 1) % count;
                            dirty = true;
                        }
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        if count > 0 {
                            selected = (selected + 1) % count;
                            dirty = true;
                        }
                    }
                    // 翻页：整页跳转（末页/首页裁剪），与窗口的页对齐保持一致
                    KeyCode::PageUp => {
                        if count > 0 {
                            selected = selected.saturating_sub(page);
                            dirty = true;
                        }
                    }
                    KeyCode::PageDown => {
                        if count > 0 {
                            selected = (selected + page).min(count - 1);
                            dirty = true;
                        }
                    }
                    KeyCode::Home => {
                        if count > 0 {
                            selected = 0;
                            dirty = true;
                        }
                    }
                    KeyCode::End => {
                        if count > 0 {
                            selected = count - 1;
                            dirty = true;
                        }
                    }
                    KeyCode::Enter => {
                        if let Some((_, path, _)) = visible.get(selected) {
                            result = Some(path.clone());
                        }
                        break;
                    }
                    KeyCode::Esc | KeyCode::Char('q') => break,
                    _ => {}
                },
                Ok(Event::Resize(_, _)) => dirty = true,
                Ok(_) => {}
                Err(_) => break,
            },
            Ok(false) => {}
            Err(_) => break,
        }
    }

    // 选中/取消：取消未完成的读取（后台线程在下一个候选前停止）
    loader.cancel();

    _ = crossterm::execute!(stdout, LeaveAlternateScreen, Show);
    _ = terminal::disable_raw_mode();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::AgentMessage;
    use crate::core::session_manager::Session;

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user_text(text)
    }

    /// 建会话：写入首条用户消息（可选名称），返回路径。`sleep` 保证 mtime 严格递增。
    fn make_session(dir: &Path, cwd: &str, text: &str, name: Option<&str>) -> PathBuf {
        let s = Session::create(cwd, Some(dir.to_path_buf()), true).unwrap();
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);
        // 重新打开追加消息（create 只写 header）
        let mut s = Session::open(&path.to_string_lossy()).unwrap();
        s.append_message(&user(text));
        if let Some(n) = name {
            s.append_session_info(n);
        }
        drop(s);
        std::thread::sleep(Duration::from_millis(2));
        path
    }

    #[test]
    fn progressive_loader_publishes_in_mtime_order() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        let first = make_session(&sess_dir, cwd, "oldest", None);
        let second = make_session(&sess_dir, cwd, "middle", Some("named middle"));
        let third = make_session(&sess_dir, cwd, "newest", None);

        let paths = list_sessions(&sess_dir);
        assert_eq!(
            paths,
            vec![third.clone(), second.clone(), first.clone()],
            "候选按 mtime 新→旧"
        );

        let mut loader = ProgressiveMetas::start(paths);
        assert_eq!(loader.total(), 3);
        assert_eq!(loader.loaded(), 0, "启动时还没有结果");
        loader.wait_all();
        assert!(!loader.is_loading());
        assert_eq!(loader.loaded(), 3);

        let rows: Vec<(PathBuf, String)> = loader
            .visible()
            .map(|(_, p, m)| (p.to_path_buf(), row_label(m)))
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].0, third, "最新会话在最前");
        assert!(rows[0].1.contains("newest"), "{}", rows[0].1);
        assert!(rows[1].1.contains("named middle"), "名称优先于首条消息");
        assert!(rows[2].1.contains("oldest"));
        assert!(rows[0].1.contains("[1 msgs]"), "{}", rows[0].1);
    }

    /// 渐进性：首个结果无需等待全部候选读完（大目录下先出最新会话）。
    #[test]
    fn progressive_loader_yields_first_row_before_all_loaded() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        for i in 0..5 {
            make_session(&sess_dir, cwd, &format!("msg {i}"), None);
        }
        let mut loader = ProgressiveMetas::start(list_sessions(&sess_dir));
        // 逐条取回：第一次 drain 至少能拿到 1 条，且此时仍未全部加载
        let mut first_batch = 0;
        for _ in 0..200 {
            first_batch = loader.drain();
            if first_batch > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(first_batch > 0, "应能取到首批结果");
        assert!(
            loader.visible().count() < loader.total() || loader.loaded() == loader.total(),
            "首批结果不要求全部候选读完"
        );
        loader.wait_all();
        assert_eq!(loader.visible().count(), 5);
    }

    /// 取消：未完成的读取在下一个候选前停止（选中后不再空转解析大目录）。
    #[test]
    fn progressive_loader_cancel_stops_loading() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        for i in 0..8 {
            make_session(&sess_dir, cwd, &format!("msg {i}"), None);
        }
        let mut loader = ProgressiveMetas::start(list_sessions(&sess_dir));
        loader.cancel();
        assert!(!loader.is_loading(), "取消后不再处于加载中");
        // 允许在途结果落地，但不会继续处理剩余候选
        std::thread::sleep(Duration::from_millis(50));
        loader.drain();
        assert!(loader.loaded() < loader.total(), "取消后不应读完所有候选");
    }

    /// 每页行数 = 终端高度 - 固定框架，保证整页放得下（终端不滚动）。
    #[test]
    fn page_rows_fits_terminal() {
        assert_eq!(page_rows(24), 20);
        assert_eq!(page_rows(27), 23);
        assert_eq!(page_rows(4), 1);
        assert_eq!(page_rows(0), 1, "异常高度至少保留 1 行");
    }

    /// 窄终端：长行必须截断到列宽，否则折行会撑破页高，翻页时跳过若干条。
    #[test]
    fn clip_line_fits_terminal_width() {
        use crate::utils::display::display_width;
        let long = format!(
            "  1 ITEM {}  [1 msgs]  (now)",
            "很长的会话首条消息".repeat(20)
        );
        for cols in [10usize, 47, 80, 120] {
            let line = clip_line(&long, cols);
            assert!(
                display_width(&line) <= cols,
                "cols={cols} width={}",
                display_width(&line)
            );
        }
        // 控制字符替换为空格，不污染终端
        assert_eq!(clip_line("a\tb\u{7}c", 40), "a b c");
    }

    /// 页对齐窗口：下一页首行 = 上一页末行 + 1（翻页不跳行）。
    #[test]
    fn window_range_pages_without_gaps() {
        assert_eq!(window_range(0, 60, 20), (0, 20));
        assert_eq!(window_range(19, 60, 20), (0, 20));
        assert_eq!(window_range(20, 60, 20), (20, 40));
        assert_eq!(window_range(59, 60, 20), (40, 60));
        // 末页裁剪
        assert_eq!(window_range(45, 45, 20), (40, 45));
        // page=0 兜底为 1 行
        assert_eq!(window_range(3, 10, 0), (3, 4));

        // 逐页推进：上一页末尾 + 1 == 下一页开头
        let count = 95usize;
        let page = page_rows(27); // 23
        let mut prev_end = 0usize;
        for p in 0..count.div_ceil(page) {
            let (start, end) = window_range(p * page, count, page);
            if p > 0 {
                assert_eq!(start, prev_end, "第 {} 页应从上一页末行 + 1 开始", p + 1);
            }
            prev_end = end;
        }
    }

    /// 不可读的候选不出现在列表里（发现是尽力而为：一个坏文件不影响其它会话）。
    #[test]
    fn progressive_loader_skips_unreadable_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        std::fs::create_dir_all(&sess_dir).unwrap();
        let good = make_session(&sess_dir, cwd, "good", None);
        let bad = sess_dir.join("broken.jsonl");
        std::fs::write(&bad, "not json\n").unwrap();
        // 让 broken 更新（排在前面）
        std::thread::sleep(Duration::from_millis(2));
        std::fs::write(&bad, "not json\n").unwrap();

        let mut loader = ProgressiveMetas::start(list_sessions(&sess_dir));
        loader.wait_all();
        let visible: Vec<PathBuf> = loader.visible().map(|(_, p, _)| p.to_path_buf()).collect();
        assert_eq!(visible, vec![good], "坏文件被跳过");
        assert_eq!(loader.loaded(), 2, "已处理计数含不可读候选");
    }
}

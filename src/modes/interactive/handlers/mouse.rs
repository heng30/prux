//! 鼠标事件（滚轮 / 拖选 / 滚动条 / 中键复制粘贴）

use crate::{
    modes::interactive::{
        self,
        app::{App, MsgLevel},
        overlay,
        panel::PanelKind,
    },
    utils::clipboard::{
        read_clipboard, read_clipboard_file_paths, read_clipboard_image, write_clipboard,
    },
};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use std::{cell::RefCell, rc::Rc};

/// Alt+滚轮加速倍数：按住 Alt 滚轮，每次滚动行数 ×5（消息区 3→15，停靠面板 1→5），用于快速跳过长回复。
const ALT_WHEEL_MULTIPLIER: usize = 5;
/// 边缘自动滚动每 tick 滚动行数（80ms tick）
const DRAG_EDGE_STEP: usize = 2;
/// 指针停留在视口边缘的 tick 数（满阈值才持续滚动，避免选择边缘行时的误触）
const EDGE_DWELL_TICKS: u8 = 3;

/// 本次滚轮事件应滚动的行数：由 [`crate::utils::wheel::WheelScrollAccelerator`] 按
/// 手势速度换算（固定值设置则恒为该值），再乘以 Alt 加速倍数。
/// 每次滚轮事件只调用一次（加速器有跨事件状态）。
///
/// 处理鼠标事件：滚轮滚动消息区、左键拖选文本
pub(super) fn handle_mouse_event(shared: &Rc<RefCell<App>>, mouse: MouseEvent) {
    let mut st = shared.borrow_mut();
    match mouse.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => st.handle_mouse_wheel(&mouse),
        MouseEventKind::Down(MouseButton::Left) => st.handle_mouse_left_down(&mouse),
        MouseEventKind::Drag(MouseButton::Left) => st.handle_mouse_left_drag(&mouse),
        MouseEventKind::Down(MouseButton::Middle) => st.handle_mouse_middle_down(&mouse),
        MouseEventKind::Up(MouseButton::Left) => st.handle_mouse_left_up(),
        _ => {}
    }
}

impl App {
    /// 本次滚轮事件应滚动的行数：由 [`crate::utils::wheel::WheelScrollAccelerator`] 按
    /// 手势速度换算（固定值设置则恒为该值），再乘以 Alt 加速倍数。
    /// 每次滚轮事件只调用一次（加速器有跨事件状态）。
    fn wheel_lines(&mut self, mouse: &MouseEvent) -> usize {
        let direction = if mouse.kind == MouseEventKind::ScrollUp {
            -1
        } else {
            1
        };

        let now_ms = self.wheel_epoch.elapsed().as_millis() as u64;
        let lines = self.wheel_accel.next(direction, now_ms);

        if mouse.modifiers.contains(KeyModifiers::ALT) {
            lines.saturating_mul(ALT_WHEEL_MULTIPLIER)
        } else {
            lines
        }
    }

    /// 指针是否落在消息区（最近一帧记录的位置）内；消息区没渲染时为假。
    fn over_log_area(&self, row: u16) -> bool {
        self.mouse.log_area_h > 0
            && row >= self.mouse.log_area
            && row < self.mouse.log_area.saturating_add(self.mouse.log_area_h)
    }

    /// 滚轮：指针在停靠面板内滚面板，在消息区内滚消息区，**其余区域不滚**；
    /// 行数由 `fullscreenWheelScrollLines` 设置决定（auto 按手势速度加速）。
    /// 按住 Alt 时行数 ×[`ALT_WHEEL_MULTIPLIER`]。
    fn handle_mouse_wheel(&mut self, mouse: &MouseEvent) {
        // 每次事件只取一次行数（加速器有跨事件状态，重复调用会错乱速度推算）
        let step = self.wheel_lines(mouse);

        // 覆盖层优先：指针在覆盖层应独占的区域内时滚覆盖层（顶/底不接力）。
        // 全屏覆盖层接管整屏（主页消息区根本没渲染）。
        if overlay::captures_mouse(self, mouse.column, mouse.row) {
            let step = step as isize;
            let delta = if mouse.kind == MouseEventKind::ScrollUp {
                -step
            } else {
                step
            };
            overlay::scroll_by(self, delta);
            return;
        }

        if let Some(area) = self.dock_area
            && mouse.row >= area.y
            && mouse.row < area.y + area.height
        {
            if mouse.kind == MouseEventKind::ScrollUp {
                self.dock_offset = self.dock_offset.saturating_sub(step);
            } else {
                self.dock_offset = self.dock_offset.saturating_add(step);
            }
            self.dirty = true;
            return;
        }

        // 只有指针在消息区内才滚消息区：输入框、状态栏、底栏上滚轮不该带动输出区
        if !self.over_log_area(mouse.row) {
            return;
        }

        if mouse.kind == MouseEventKind::ScrollUp {
            self.scroll = self.scroll.saturating_add(step);
        } else {
            self.scroll = self.scroll.saturating_sub(step);
        }
        self.dirty = true;
    }

    /// 左键松开：结束拖选与滚动条抓取，复位边缘滞留计数
    fn handle_mouse_left_up(&mut self) {
        // 覆盖层拖选/滚动条抓取一并结束（无拖拽时是 no-op）
        overlay::left_up(self);
        self.mouse.dragging = false;
        self.mouse.scroll_dragging = false;
        self.mouse.scroll_press = None;
        self.mouse.drag_row = None;
        self.mouse.edge_dwell = 0;
        self.dirty = true;
    }

    /// 左键按下：面板点击（Ctrl+OAuth URL 打开浏览器）、滚动条按下、消息区拖选起点
    fn handle_mouse_left_down(&mut self, mouse: &MouseEvent) {
        // 覆盖层优先：滚动条抓取 / 内容拖选起点都归覆盖层
        if overlay::captures_mouse(self, mouse.column, mouse.row) {
            overlay::left_down(self, mouse.column, mouse.row);
            return;
        }

        if self.panel.active {
            self.mouse.sel = None;
            self.mouse.dragging = false;
            self.mouse.scroll_dragging = false;
            self.mouse.scroll_press = None;
            self.mouse.drag_row = None;
            self.mouse.edge_dwell = 0;

            // Ctrl+click：LoginOauth 面板的授权 URL / device code 验证 URI 行 → 系统浏览器打开
            if self.handle_panel_oauth_ctrl_click(mouse) {
                return;
            }

            // Ctrl+click：ExtensionDetail 面板的 fork URL 行 → 系统浏览器打开
            if self.handle_panel_extension_detail_ctrl_click(mouse) {
                return;
            }

            self.dirty = true;
            return;
        }

        // 滚动条列按下：track 空白处点击跳转（thumb 顶对齐点击行）；thumb 上按下仅抓取不跳转
        if self.mouse.scroll_x == Some(mouse.column) {
            self.mouse.sel = None;
            self.mouse.dragging = false;
            self.mouse.scroll_dragging = true;
            self.mouse.drag_row = None;
            self.mouse.edge_dwell = 0;

            if self.scrollbar_press_on_thumb(mouse.row) {
                // 抓住 thumb：固定按下点相对 thumb 顶的偏移
                // （拖动期间保持；不能在拖动中按最新 thumb_start 重算，否则向上拖会漂移加速）
                let off = mouse
                    .row
                    .saturating_sub(self.mouse.log_area)
                    .saturating_sub(self.mouse.scroll_thumb.map(|(s, _)| s as u16).unwrap_or(0));
                self.mouse.scroll_press = Some((mouse.row, off as usize));
            } else {
                self.scrollbar_click_jump(mouse.row);
            }
            self.dirty = true;
            return;
        }

        let area_y = self.message_area_y();
        let area_h = self.message_area_h();

        // Ctrl+click 消息区链接 → 系统浏览器打开（与面板 OAuth 的 Ctrl+click 同一手势）。
        // 必须带 Ctrl：裸左键留给文本选择，两者不能抢同一个手势。
        if mouse.modifiers.contains(KeyModifiers::CONTROL)
            && self.open_message_link(mouse.column, mouse.row)
        {
            self.mouse.sel = None;
            self.mouse.dragging = false;
            self.mouse.drag_row = None;
            return;
        }

        // Alt+点击可折叠块（branch/compaction 摘要、skill 调用、thinking run、工具块）→
        // 单独折叠/展开。必须带 Alt：裸左键留给文本选择，两者不能抢同一个手势。
        if mouse.modifiers.contains(KeyModifiers::ALT)
            && let Some(t) = self.mouse.click_target_at(mouse.row)
        {
            let cur = self
                .expand_overrides
                .get(&t.id)
                .copied()
                .unwrap_or(t.default_expanded);

            self.expand_overrides.insert(t.id, !cur);
            self.msg_cache.remove(&(t.id.ts, t.id.seq)); // 只失效该条目（消息）的渲染缓存，其它块零重渲染
            self.mouse.sel = None;
            self.mouse.dragging = false;
            self.mouse.drag_row = None;
            self.dirty = true;
            return;
        }

        if let Some((line, ch)) = self.mouse.hit(mouse.column, mouse.row, area_y, area_h) {
            self.mouse.sel = Some((line, ch, line, ch));
            self.mouse.dragging = true;
            self.dirty = true;
        } else {
            // 消息区外点击：清除选择
            self.mouse.sel = None;
            self.mouse.dragging = false;
            self.dirty = true;
        }
    }

    /// 中键按下：有选中 → 把选中文本复制进系统剪贴板，并把选中文本直接粘贴进主页输入框
    /// （复制+粘贴在同一流程内原子完成；粘贴用内存文本、不读回剪贴板，杜绝写读之间的
    /// 进程交接竞态导致粘贴到旧剪贴板内容）；无选中 → 从系统剪贴板粘贴（连续中键 = 连续粘贴）。
    fn handle_mouse_middle_down(&mut self, mouse: &MouseEvent) {
        // 覆盖层优先：复制覆盖层选中 + 粘进已聚焦的输入行（未聚焦只复制）
        if overlay::captures_mouse(self, mouse.column, mouse.row) {
            self.handle_overlay_middle_down();
            return;
        }

        if let Some(text) = self.selected_text() {
            // 复制失败仅警告，不阻断粘贴（粘贴不依赖剪贴板）
            if let Err(e) = write_clipboard(&text) {
                self.push_msg(format!("copy failed: {e}"), MsgLevel::Warning);
            }

            self.mouse.sel = None;
            self.mouse.dragging = false;
            self.mouse.drag_row = None;
            self.editor.paste_text(&text);
            self.dirty = true;
            return;
        }

        // 无选中：粘贴剪贴板
        if let Some(text) = read_clipboard() {
            self.editor.paste_text(&text);
            self.dirty = true;
        }
    }

    /// 中键（覆盖层内）：有选中 → 复制到系统剪贴板并粘进**已聚焦**的输入行（未聚焦只复制）；
    /// 无选中 → 从剪贴板粘贴（同样只在输入行聚焦时写入）。
    fn handle_overlay_middle_down(&mut self) {
        if let Some(text) = overlay::selected_text(self) {
            if let Err(e) = write_clipboard(&text) {
                self.push_msg(format!("copy failed: {e}"), MsgLevel::Warning);
            }

            overlay::clear_selection(self);
            if overlay::input_focused(self) {
                overlay::paste_into_input(self, &text);
            }
            self.dirty = true;
            return;
        }

        if overlay::input_focused(self)
            && let Some(text) = read_clipboard()
        {
            overlay::paste_into_input(self, &text);
            self.dirty = true;
        }
    }

    /// `app.message.copy` 在 OAuth 登录面板里的动作：复制授权 URL 到剪贴板。
    ///
    /// 登录面板里的 URL 是展示用文本，长 URL 折行后无法整段选中；没有登录流程或 URL
    /// 为空时只提示，不写剪贴板。
    pub(super) fn copy_oauth_authorization_url(&mut self) {
        let url = self
            .oauth_login
            .as_ref()
            .map(|oauth| oauth.auth_url())
            .unwrap_or_default();

        if url.is_empty() {
            self.push_msg("No sign-in URL to copy yet.".to_string(), MsgLevel::Info);
            return;
        }

        // 成功提示走状态栏，不进消息流
        if let Err(e) = write_clipboard(&url) {
            self.push_msg(format!("copy failed: {e}"), MsgLevel::Warning);
        } else {
            self.set_status_msg("Copied sign-in URL to clipboard");
        }
    }

    /// Ctrl+click 命中消息区链接 → 打开系统浏览器；命中返回 true，未命中不改变任何状态。
    fn open_message_link(&mut self, col: u16, row: u16) -> bool {
        let Some(link) = self.mouse.link_at(col, row) else {
            return false;
        };

        let url = link.url.clone();
        if webbrowser::open(&url).is_ok() {
            self.push_msg(format!("Opening {url}"), MsgLevel::Info);
        } else {
            self.push_msg(
                "Failed to open browser. Copy the URL and open it manually.".to_string(),
                MsgLevel::Warning,
            );
        }
        self.dirty = true;
        true
    }

    /// Ctrl+click 面板 OAuth 区域：点击授权 URL / device code 验证 URI 行 → 系统浏览器打开；命中返回 true
    fn handle_panel_oauth_ctrl_click(&mut self, mouse: &MouseEvent) -> bool {
        if mouse.modifiers.contains(KeyModifiers::CONTROL)
            && self
                .panel
                .top()
                .is_some_and(|l| l.kind == PanelKind::LoginOauth)
            && let Some(oauth) = self.oauth_login.as_ref()
            && let Some(area) = self.panel_area
        {
            // device-code 流：点击 Code / URI 区域 → 打开 verification_uri
            // 布局：空(0) 标题(1) 空(2) Code(3) 空(4) URI(5..5+n) Ctrl(5+n)…
            if let Some((_, verification_uri)) = oauth.device_code_info() {
                let n = interactive::render::oauth_url_lines(&verification_uri, area.width as usize)
                    as u16;

                if mouse.row >= area.y + 6 && mouse.row < area.y + 6 + n {
                    if webbrowser::open(&verification_uri).is_ok() {
                        self.push_msg(
                            "Opening login page in your browser...".to_string(),
                            MsgLevel::Info,
                        );
                    } else {
                        self.push_msg(
                            "Failed to open browser. Copy the URL and open it manually."
                                .to_string(),
                            MsgLevel::Warning,
                        );
                    }
                    self.dirty = true;
                    return true;
                }
            }

            let n =
                interactive::render::oauth_url_lines(&oauth.auth_url(), area.width as usize) as u16;

            // 内容行：空(0) 标题(1) 空(2) URL(3..3+n)；边框在 area 内部 → 屏幕行 +1
            if mouse.row >= area.y + 4 && mouse.row < area.y + 4 + n {
                if webbrowser::open(&oauth.auth_url()).is_ok() {
                    self.push_msg(
                        "Opening login page in your browser...".to_string(),
                        MsgLevel::Info,
                    );
                } else {
                    self.push_msg(
                        "Failed to open browser. Copy the URL and open it manually.".to_string(),
                        MsgLevel::Warning,
                    );
                }
                self.dirty = true;
                return true;
            }
        }
        false
    }

    /// Ctrl+click 面板 ExtensionDetail 区域：点击 fork URL 行 → 系统浏览器打开；命中返回 true
    fn handle_panel_extension_detail_ctrl_click(&mut self, mouse: &MouseEvent) -> bool {
        if mouse.modifiers.contains(KeyModifiers::CONTROL)
            && self
                .panel
                .top()
                .is_some_and(|l| l.kind == PanelKind::ExtensionDetail)
            && let Some(url) = self.extension_detail_url.as_deref()
            && !url.is_empty()
            && let Some(area) = self.panel_area
        {
            // 找到 fork url 行在 extension_detail 中的位置，计算其在面板内的屏幕行
            // 面板布局：边框顶(0) 空(1) 标题(2) 空(3) 描述行... fork url 行 ... Ctrl+click提示 ...
            // 每行描述可能被 wrap_text 换行，需要逐行累加
            let mut fork_url_screen_row: Option<u16> = None;
            let mut row_offset: u16 = 4; // 边框内空+标题+空 = 行 1..3（0-indexed: 1,2,3）
            for l in &self.extension_detail {
                if l.starts_with("  fork url: ") {
                    fork_url_screen_row = Some(row_offset);
                    break;
                }
                let wrapped = interactive::render::wrap_text(l, area.width as usize - 4);
                row_offset += wrapped.len() as u16;
            }

            if let Some(fork_row) = fork_url_screen_row {
                let screen_row = area.y + fork_row;
                if mouse.row == screen_row {
                    if webbrowser::open(url).is_ok() {
                        self.push_msg(
                            "Opening fork project page in your browser...".to_string(),
                            MsgLevel::Info,
                        );
                    } else {
                        self.push_msg(
                            "Failed to open browser. Copy the URL and open it manually."
                                .to_string(),
                            MsgLevel::Warning,
                        );
                    }
                    self.dirty = true;
                    return true;
                }
            }
        }
        false
    }

    /// 左键拖动：滚动条抓取拖动优先，否则扩展文本拖选（视口边缘自动滚动）
    fn handle_mouse_left_drag(&mut self, mouse: &MouseEvent) {
        // 覆盖层优先：指针在覆盖层内，或覆盖层正在拖拽（滚动条/选择）
        if overlay::captures_mouse(self, mouse.column, mouse.row) || overlay::is_dragging(self) {
            overlay::left_drag(self, mouse.column, mouse.row);
            return;
        }

        // 滚动条抓取拖动（增量跟随）优先于文本拖选；按下在滚动条列时无法两者兼得
        if self.mouse.scroll_dragging {
            if self.mouse.scroll_press.is_some() {
                self.scrollbar_drag_to(mouse.row);
            }
            self.dirty = true;
            return;
        }

        if !self.mouse.dragging {
            return;
        }

        // 记录最近拖拽行（80ms tick 边缘持续自动滚动用）
        self.mouse.drag_row = Some(mouse.row);

        let area_y = self.message_area_y();
        let area_h = self.message_area_h();
        // 只在指针离开视口边缘时清零滞留计数：边缘内小幅移动（触发新的 Drag 事件）
        // 不打断自动滚动的累积，否则持续滚动可能永远起不来
        let at_edge = mouse.row <= area_y || mouse.row.saturating_add(1) >= area_y + area_h;
        if !at_edge {
            self.mouse.edge_dwell = 0;
        }
        if let Some((line, ch)) = self.mouse.hit(mouse.column, mouse.row, area_y, area_h) {
            self.mouse.set_drag_point(line, ch);
            self.dirty = true;
            return;
        }

        // 指针在消息区外：立即滚一步（上方越界上滚、下方越界下滚），
        // 选中终点钉在视口边缘内容行；持续滚动由 auto_scroll_tick 驱动
        if (mouse.row < area_y && self.scroll_drag_edge(area_h as usize, true))
            || (mouse.row >= area_y + area_h && self.scroll_drag_edge(area_h as usize, false))
        {
            self.dirty = true;
        }
    }

    /// 拖拽边缘滚动一步：up=true 上滚（终点钉视口顶行开头），up=false 下滚（终点钉底行行尾）。
    /// 顶行/底行的字符端点：上滚钉 (新 view_start, 0)；下滚钉 (新 view_start+area_h-1, usize::MAX)，
    /// 行尾长度要等滚动后的下一帧渲染才可知，用 MAX 由高亮/提取的 min 钳制。
    /// 返回是否实际滚动（内容已到边缘返回 false）。
    fn scroll_drag_edge(&mut self, area_h: usize, up: bool) -> bool {
        let max_scroll = self.mouse.log_total.saturating_sub(area_h);
        if up {
            if self.scroll >= max_scroll {
                return false;
            }
            self.scroll = (self.scroll + DRAG_EDGE_STEP).min(max_scroll);
            let start = self
                .mouse
                .log_total
                .saturating_sub(area_h)
                .saturating_sub(self.scroll);
            self.mouse.set_drag_point(start, 0);
        } else {
            if self.scroll == 0 {
                return false;
            }
            self.scroll = self.scroll.saturating_sub(DRAG_EDGE_STEP);
            let start = self
                .mouse
                .log_total
                .saturating_sub(area_h)
                .saturating_sub(self.scroll);
            let end_line = (start + area_h)
                .saturating_sub(1)
                .min(self.mouse.log_total.saturating_sub(1));
            self.mouse.set_drag_point(end_line, usize::MAX);
        }
        true
    }

    /// 事件循环 80ms tick 驱动：拖拽中且指针停在视口边缘时持续自动滚动（浏览器式跨屏选择）。
    /// 上边缘：指针停在最顶行（row == area_y；终端把越界上方点击钳到顶行，
    /// 这是"拖出视口上缘"在终端里唯一可表达的手势）；
    /// 下边缘：指针在消息区下方（row >= area_y + area_h）。
    /// 指针需在边缘滞留 EDGE_DWELL_TICKS 个 tick 才开始，之后每 tick 滚一步；
    /// 一旦离开边缘或松开即复位。返回是否发生了滚动（调用方据此置 dirty）。
    pub(crate) fn auto_scroll_tick(&mut self) -> bool {
        if !self.mouse.dragging {
            self.mouse.edge_dwell = 0;
            return false;
        }
        let Some(row) = self.mouse.drag_row else {
            self.mouse.edge_dwell = 0;
            return false;
        };
        let area_y = self.mouse.log_area;
        let area_h = self.mouse.log_area_h.max(1) as usize;
        let at_top = row <= area_y;
        // 底行也算下边缘（与顶行对称：row == area_y+area_h-1 即触发下滚）
        let at_bottom = row.saturating_add(1) >= area_y + area_h as u16;
        if !at_top && !at_bottom {
            self.mouse.edge_dwell = 0;
            return false;
        }
        if self.mouse.edge_dwell < EDGE_DWELL_TICKS {
            self.mouse.edge_dwell += 1;
            return false;
        }
        self.scroll_drag_edge(area_h, at_top)
    }

    /// 按下位置是否落在 thumb 区间内（抓取而非跳转）
    fn scrollbar_press_on_thumb(&self, row: u16) -> bool {
        let Some((start, len)) = self.mouse.scroll_thumb else {
            return false;
        };
        let rel = row
            .saturating_sub(self.mouse.log_area)
            .min(self.mouse.log_area_h.saturating_sub(1)) as usize;
        rel >= start && rel < start + len
    }

    /// 点击 track 空白：thumb 顶对齐点击行（scroll = max_scroll * (1 - rel/(track-1))）。
    /// scroll_press 第二槽记录 grab_offset=0：跳转后 thumb 顶 = 点击行，拖动从该行跟手。
    fn scrollbar_click_jump(&mut self, row: u16) {
        let track = self.mouse.log_area_h.max(1) as usize;
        let view = track;
        let max_scroll = self.mouse.log_total.saturating_sub(view);
        let rel = row
            .saturating_sub(self.mouse.log_area)
            .min(track.saturating_sub(1) as u16) as usize;
        let ratio = if track <= 1 {
            0.0
        } else {
            rel as f64 / (track - 1) as f64
        };

        self.scroll = ((max_scroll as f64 * (1.0 - ratio)).round() as usize).min(max_scroll);
        // 跳转后以新位置为抓取基准（thumb 顶已在点击行，偏移 0）
        self.mouse.scroll_press = Some((row, 0));
    }

    /// 抓取拖动映射。
    /// thumb 顶目标 = 鼠标行 - grabOffset（按下时固定，保持跟手不跳变），
    /// 再按与点击跳转同一把尺子反算 scroll：track 顶 → scroll=max_scroll（开头），
    /// track 底 → scroll=0（结尾）。
    /// 注意 grabOffset 必须在按下时固定：若拖动中用最新 thumb_start 重算，向上拖时
    /// 偏差累积（thumb 比鼠标快），向下拖被 saturating_sub 钳 0 反而正常。
    fn scrollbar_drag_to(&mut self, row: u16) {
        let Some((_, grab_offset)) = self.mouse.scroll_press else {
            return;
        };

        let track = self.mouse.log_area_h.max(1) as usize;
        let max_scroll = self.mouse.log_total.saturating_sub(track);

        // thumb 顶目标相对位置（track 内），贴底上限 track-1
        let rel = row
            .saturating_sub(self.mouse.log_area)
            .saturating_sub(grab_offset as u16)
            .min(track.saturating_sub(1) as u16) as usize;

        let ratio = if track <= 1 {
            0.0
        } else {
            rel as f64 / (track - 1) as f64
        };
        self.scroll = ((max_scroll as f64 * (1.0 - ratio)).round() as usize).min(max_scroll);
    }

    /// 消息区屏幕 y（最近一帧渲染记录；无记录时假设消息区在顶部）
    fn message_area_y(&self) -> u16 {
        self.mouse.log_area
    }

    /// 消息区高度（由可见行推断）
    fn message_area_h(&self) -> u16 {
        self.mouse.visible.len() as u16
    }

    /// Ctrl+Shift+C：复制选中文本（OSC52 剪贴板，fallback 系统命令）
    pub(super) fn copy_selection(&mut self) {
        let Some(text) = self.selected_text() else {
            self.set_status_msg("No text selected to copy");
            return;
        };

        // 成功提示走状态栏，不进消息流
        if let Err(e) = write_clipboard(&text) {
            self.push_msg(format!("copy failed: {e}"), MsgLevel::Warning);
        } else {
            self.set_status_msg("Copied selection to clipboard");
        }
        self.mouse.sel = None;
    }

    /// Ctrl+Shift+C（覆盖层）：复制覆盖层选中文本
    pub(super) fn copy_overlay_selection(&mut self) {
        let Some(text) = overlay::selected_text(self) else {
            self.set_status_msg("No text selected to copy");
            return;
        };

        if let Err(e) = write_clipboard(&text) {
            self.push_msg(format!("copy failed: {e}"), MsgLevel::Warning);
        } else {
            self.set_status_msg("Copied selection to clipboard");
        }
        overlay::clear_selection(self);
    }

    /// 复制最后一条助手消息到剪贴板（/copy、Ctrl+X、Ctrl+Shift+C）
    pub(super) fn copy_last_assistant_message(&mut self) {
        let Some(msg) = self.messages.iter().rev().find(|m| m.role == "assistant") else {
            self.push_msg("No agent messages to copy yet.".to_string(), MsgLevel::Info);
            return;
        };
        let text = msg.text();

        // 成功提示走状态栏，不进消息流
        if let Err(e) = write_clipboard(&text) {
            self.push_msg(format!("copy failed: {e}"), MsgLevel::Warning);
        } else {
            self.set_status_msg("Copied last agent message to clipboard");
        }
    }

    /// Alt+C：复制主页输入框全部内容到系统剪贴板（大粘贴 marker 展开为完整文本）。
    /// 空输入框仅提示，不写剪贴板。
    pub(super) fn copy_editor_content(&mut self) {
        let text = self.editor.expanded_text();
        if text.is_empty() {
            self.push_msg(
                "Editor is empty. Nothing to copy.".to_string(),
                MsgLevel::Info,
            );
            return;
        }
        if let Err(e) = write_clipboard(&text) {
            self.push_msg(format!("copy failed: {e}"), MsgLevel::Warning);
        } else {
            self.set_status_msg("Copied editor content to clipboard");
        }
    }

    /// 粘贴剪贴板内容到输入框。优先级：**文件路径 → 图片 → 文本**（与 pi 一致）：
    /// 复制文件（macOS Finder / Linux 文件管理器）插入原路径，截图类只带图片的内容落成临时 PNG
    /// 路径，纯文本最后兜底。bash 模式下路径按 shell 规则加引号并以空格相连，
    /// 其余模式每行一个路径。
    pub(super) fn paste_clipboard(&mut self) {
        let file_paths = read_clipboard_file_paths();
        if !file_paths.is_empty() {
            // 路径里的控制字符会注入转义序列/换行，直接拒绝整次粘贴
            if file_paths.iter().any(|p| p.chars().any(char::is_control)) {
                self.push_msg(
                    "paste failed: clipboard file path contains control characters".to_string(),
                    MsgLevel::Warning,
                );
                return;
            }
            let bash_mode = self.is_bash_mode();
            self.editor.insert_text(&format_clipboard_paths(
                &file_paths,
                bash_mode,
                self.editor.cursor_neighbours(),
            ));
            self.dirty = true;
            return;
        }

        let tmp_for_image = std::env::temp_dir();
        if let Some(path) = read_clipboard_image(&tmp_for_image) {
            self.editor.insert_text(&path);
            self.dirty = true;
            return;
        }
        if let Some(text) = read_clipboard() {
            self.editor.paste_text(&text);
            self.dirty = true;
        } else {
            self.push_msg(
                "paste failed: no clipboard access".to_string(),
                MsgLevel::Warning,
            );
        }
    }

    /// 编辑器当前是否处于 bash 模式（`!` 开头的输入行）。
    fn is_bash_mode(&self) -> bool {
        self.editor
            .lines
            .first()
            .map(|l| l.trim_start().starts_with('!'))
            .unwrap_or(false)
    }
}

/// 把剪贴板里的文件路径拼成插入文本：bash 模式逐条加 shell 引号并用空格连接，
/// 其余模式每行一条；`neighbours` 为光标两侧字符（前后都是非空白时各补一个空格，
/// 避免路径和已有输入粘连）。
fn format_clipboard_paths(
    paths: &[String],
    bash_mode: bool,
    neighbours: (Option<char>, Option<char>),
) -> String {
    let body = if bash_mode {
        paths
            .iter()
            .map(|p| shell_quote_if_needed(p))
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        paths.join("\n")
    };

    let (before, after) = neighbours;
    let leading = before.filter(|c| !c.is_whitespace()).map_or("", |_| " ");
    let trailing = after.filter(|c| !c.is_whitespace()).map_or("", |_| " ");
    format!("{leading}{body}{trailing}")
}

/// bash 模式下的路径引号：仅含安全字符（`A-Za-z0-9_-./~:@`）时原样返回，
/// 否则用单引号包裹并把内部的 `'` 转义为 `'\''`。
fn shell_quote_if_needed(value: &str) -> String {
    let safe = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-./~:@".contains(c));

    if safe {
        value.to_string()
    } else {
        format!("'{}'", value.replace('\'', "'\\''"))
    }
}

/// 处理终端事件（键盘 / resize / 粘贴）
#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::*;
    use crate::modes::interactive::app::App;

    /// 测试用固定滚轮步长为 3 行（auto 加速依赖真实时序，单测里不可复现）
    const WHEEL_STEP: usize = 3;

    /// 构造测试 App：滚轮步长固定 3 行，行为与引入 `fullscreenWheelScrollLines` 之前一致
    fn test_app() -> App {
        let mut app = App::new();
        app.wheel_accel
            .set_lines(crate::utils::wheel::WheelScrollLines::Lines(WHEEL_STEP));
        // 默认给一个覆盖整屏的消息区（渲染时由 render_message_area 记录），
        // 滚轮只有落在消息区内才生效——需要「消息区之外」的用例自行改写这两个字段
        app.mouse.log_area = 0;
        app.mouse.log_area_h = 40;
        app
    }

    /// 带修饰键的左键按下（Alt 折叠切换用）
    fn alt_left_down(st: &mut App, row: u16) {
        st.handle_mouse_left_down(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row,
            modifiers: KeyModifiers::ALT,
        });
    }

    #[test]
    fn alt_left_click_toggles_collapsible_summary() {
        // Alt+点击折叠块（branch/compaction 摘要）→ 单独折叠/展开，且不启动文本选择
        use crate::core::provider::AgentMessage;
        use ratatui::backend::TestBackend;

        let mut st = test_app();
        let mut summary = AgentMessage::user_text("SUMMARYBODY");
        summary.role = "compactionSummary".to_string();
        st.messages.push(summary);

        let backend = TestBackend::new(60, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| crate::modes::interactive::render::render_frame(f, &mut st))
            .unwrap();
        let id = st.mouse.click_targets[0].id;
        let row = st.mouse.click_targets[0].start;
        assert!(st.expand_overrides.is_empty());

        alt_left_down(&mut st, row);
        assert!(st.mouse.sel.is_none(), "Alt+点击折叠块不启动文本选择");
        assert!(!st.mouse.dragging);
        assert_eq!(st.expand_overrides.get(&id), Some(&true), "首次点击展开");

        // 重绘后再点一次：折叠回默认态
        terminal
            .draw(|f| crate::modes::interactive::render::render_frame(f, &mut st))
            .unwrap();
        let row2 = st.mouse.click_targets[0].start;
        alt_left_down(&mut st, row2);
        assert_eq!(st.expand_overrides.get(&id), Some(&false), "再次点击折叠");
    }

    #[test]
    fn plain_left_click_on_collapsible_keeps_selection_behaviour() {
        // 裸左键必须仍然用作文本选择：落在折叠块上也不得切换展开态
        use crate::core::provider::AgentMessage;
        use ratatui::backend::TestBackend;

        let mut st = test_app();
        let mut summary = AgentMessage::user_text("SUMMARYBODY");
        summary.role = "compactionSummary".to_string();
        st.messages.push(summary);

        let backend = TestBackend::new(60, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| crate::modes::interactive::render::render_frame(f, &mut st))
            .unwrap();
        let row = st.mouse.click_targets[0].start;

        st.handle_mouse_left_down(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 2,
            row,
            modifiers: KeyModifiers::NONE,
        });
        assert!(st.expand_overrides.is_empty(), "裸左键不切换展开态");
        assert!(st.mouse.sel.is_some(), "裸左键启动文本选择");
        assert!(st.mouse.dragging);
    }

    #[test]
    fn copy_editor_content_empty_editor_pushes_hint_only() {
        // 空输入框：不触系统剪贴板，仅追加一条提示（写剪贴板依赖真实环境，不在单测中断言）
        let mut st = test_app();
        assert_eq!(st.system_messages.len(), 0);
        st.copy_editor_content();
        assert_eq!(st.system_messages.len(), 1, "空输入框应只追加一条提示");
    }

    #[test]
    fn middle_click_copies_selection_to_clipboard_and_clears_sel() {
        // 中键：有选中时复制到系统剪贴板并取消选中（写入剪贴板为真实系统调用，
        // 这里只断言选择状态；剪贴板粘贴结果依赖环境，不在单测中断言输入框内容）
        use ratatui::backend::TestBackend;
        let mut st = test_app();
        for i in 0..5 {
            st.messages
                .push(crate::core::provider::AgentMessage::user_text(&format!(
                    "line {:02}",
                    i
                )));
        }
        let backend = TestBackend::new(50, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| crate::modes::interactive::render::render_frame(f, &mut st))
            .unwrap();
        let start = st.mouse.view_start;
        st.mouse.sel = Some((start, 0, start, usize::MAX));
        let expected = st.selected_text().unwrap();
        assert!(!expected.is_empty());

        st.handle_mouse_middle_down(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Middle),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });

        assert_eq!(st.mouse.sel, None, "中键复制后取消选中");
        assert!(!st.mouse.dragging);

        // 无选中时中键不 panic、不改变选择状态；若剪贴板可用则连续粘贴
        st.handle_mouse_middle_down(&MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Middle),
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(st.mouse.sel, None, "无选中时中键保持无选择");
    }

    #[test]
    fn wheel_over_overlay_scrolls_overlay_only() {
        // 覆盖层与消息区同时存在：指针在覆盖层内 → 只滚覆盖层
        use ratatui::layout::Rect;
        let st = Rc::new(RefCell::new(test_app()));
        {
            let mut s = st.borrow_mut();
            let mut view = crate::core::extensions::OverlayView::inline(
                "Fleet",
                (0..30)
                    .map(|i| vec![crate::core::extensions::DockSpan::plain(format!("l{i}"))])
                    .collect(),
                6,
            );
            view.footer.clear();
            crate::modes::interactive::overlay::open(&mut s, 1, view);
            s.overlay.as_mut().unwrap().content_rows = 5;
            s.overlay.as_mut().unwrap().area = Some(Rect::new(0, 5, 40, 5));
            s.scroll = 17;
        }
        let wheel = |kind| MouseEvent {
            kind,
            column: 10,
            row: 6,
            modifiers: KeyModifiers::NONE,
        };
        handle_mouse_event(&st, wheel(MouseEventKind::ScrollDown));
        assert_eq!(st.borrow().overlay.as_ref().unwrap().scroll, WHEEL_STEP);
        assert_eq!(st.borrow().scroll, 17, "消息区不动");
        handle_mouse_event(&st, wheel(MouseEventKind::ScrollUp));
        assert_eq!(st.borrow().overlay.as_ref().unwrap().scroll, 0);
        handle_mouse_event(&st, wheel(MouseEventKind::ScrollUp));
        assert_eq!(st.borrow().overlay.as_ref().unwrap().scroll, 0, "顶部夹紧");
        assert_eq!(st.borrow().scroll, 17);
    }

    #[test]
    fn alt_wheel_accelerates_overlay_scroll() {
        // Alt 加速对覆盖层（inline / Panel / Fullscreen）同样生效：行数 ×ALT_WHEEL_MULTIPLIER。
        use crate::core::extensions::{DockSpan, OverlaySize, OverlayView};
        use ratatui::layout::Rect;

        let long_view = |size| {
            let mut view = OverlayView::inline(
                "Fleet",
                (0..40)
                    .map(|i| vec![DockSpan::plain(format!("l{i}"))])
                    .collect(),
                6,
            );
            view.footer.clear();
            view.size = size;
            view
        };

        // inline 覆盖层：指针在矩形内
        let st = Rc::new(RefCell::new(test_app()));
        {
            let mut s = st.borrow_mut();
            crate::modes::interactive::overlay::open(
                &mut s,
                1,
                long_view(OverlaySize::Inline { max_rows: 6 }),
            );
            s.overlay.as_mut().unwrap().content_rows = 5;
            s.overlay.as_mut().unwrap().area = Some(Rect::new(0, 5, 40, 5));
        }
        handle_mouse_event(
            &st,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 6,
                modifiers: KeyModifiers::ALT,
            },
        );
        assert_eq!(
            st.borrow().overlay.as_ref().unwrap().scroll,
            WHEEL_STEP * ALT_WHEEL_MULTIPLIER,
            "inline 覆盖层 Alt 加速"
        );

        // Panel 覆盖层（占据输入区，如 FleetView）：矩形内同样加速
        let st = Rc::new(RefCell::new(test_app()));
        {
            let mut s = st.borrow_mut();
            crate::modes::interactive::overlay::open(
                &mut s,
                1,
                long_view(OverlaySize::Panel { max_rows: 10 }),
            );
            s.overlay.as_mut().unwrap().content_rows = 5;
            s.overlay.as_mut().unwrap().area = Some(Rect::new(0, 5, 40, 10));
        }
        handle_mouse_event(
            &st,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 6,
                modifiers: KeyModifiers::ALT,
            },
        );
        assert_eq!(
            st.borrow().overlay.as_ref().unwrap().scroll,
            WHEEL_STEP * ALT_WHEEL_MULTIPLIER,
            "Panel 覆盖层 Alt 加速"
        );

        // Fullscreen 覆盖层：接管整屏，任意位置 Alt 滚轮都加速
        let st = Rc::new(RefCell::new(test_app()));
        {
            let mut s = st.borrow_mut();
            crate::modes::interactive::overlay::open(&mut s, 1, long_view(OverlaySize::Fullscreen));
            s.overlay.as_mut().unwrap().content_rows = 5;
        }
        handle_mouse_event(
            &st,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 1,
                row: 1,
                modifiers: KeyModifiers::ALT,
            },
        );
        assert_eq!(
            st.borrow().overlay.as_ref().unwrap().scroll,
            WHEEL_STEP * ALT_WHEEL_MULTIPLIER,
            "Fullscreen 覆盖层 Alt 加速"
        );
    }

    #[test]
    fn alt_wheel_over_rendered_panel_overlay_accelerates() {
        // 走真实渲染路径：Panel 覆盖层渲染后记录 `area`，此时指针在覆盖层内 Alt 滚轮应加速。
        // （防止只靠手工设置 `area` 的测试掩盖“渲染未记录区域→ wheel 落到消息区”的回归）
        use crate::core::extensions::{DockSpan, OverlaySize, OverlayView};
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;

        let mut app = test_app();
        let mut view = OverlayView::inline(
            "Fleet",
            (0..40)
                .map(|i| vec![DockSpan::plain(format!("l{i}"))])
                .collect(),
            6,
        );
        view.footer.clear();
        view.size = OverlaySize::Panel { max_rows: 10 };
        crate::modes::interactive::overlay::open(&mut app, 1, view);

        let area = Rect::new(0, 20, 60, 10);
        let backend = TestBackend::new(60, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                crate::modes::interactive::render::overlay::render(
                    f,
                    area,
                    &mut app,
                    OverlaySize::Panel { max_rows: 10 },
                );
            })
            .unwrap();
        assert!(
            app.overlay.as_ref().unwrap().area.is_some(),
            "渲染应记录覆盖层区域（否则滚轮命中失败）"
        );
        app.scroll = 17;

        let st = Rc::new(RefCell::new(app));
        handle_mouse_event(
            &st,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 5,
                row: 21,
                modifiers: KeyModifiers::ALT,
            },
        );
        assert_eq!(
            st.borrow().overlay.as_ref().unwrap().scroll,
            WHEEL_STEP * ALT_WHEEL_MULTIPLIER,
            "Panel 覆盖层（真实渲染路径）Alt 加速"
        );
        assert_eq!(st.borrow().scroll, 17, "消息区不动");
    }

    #[test]
    fn wheel_over_dock_scrolls_dock_only() {
        // 指针在 dock_area 内：滚轮只滚面板（与消息区同一行数），不动消息区；
        // 顶/底不接力（吞掉），不会把剩余滚动透传给消息区
        use ratatui::layout::Rect;
        let st = Rc::new(RefCell::new(test_app()));
        {
            let mut s = st.borrow_mut();
            s.dock_area = Some(Rect::new(0, 5, 40, 11));
            s.dock_offset = 2;
            s.scroll = 17;
        }
        let wheel = |kind| MouseEvent {
            kind,
            column: 10,
            row: 6,
            modifiers: KeyModifiers::NONE,
        };
        // 面板内往下滚：dock_offset +WHEEL_STEP，消息区不动
        handle_mouse_event(&st, wheel(MouseEventKind::ScrollDown));
        assert_eq!(st.borrow().dock_offset, 2 + WHEEL_STEP);
        assert_eq!(st.borrow().scroll, 17, "面板内滚轮不得滚动消息区");
        // 面板内往上滚：dock_offset -1
        handle_mouse_event(&st, wheel(MouseEventKind::ScrollUp));
        assert_eq!(st.borrow().dock_offset, 2);
        // 顶部不再减（吞掉，不接力）
        handle_mouse_event(&st, wheel(MouseEventKind::ScrollUp));
        handle_mouse_event(&st, wheel(MouseEventKind::ScrollUp));
        handle_mouse_event(&st, wheel(MouseEventKind::ScrollUp));
        assert_eq!(st.borrow().dock_offset, 0, "顶部夹紧且不接力消息区");
        assert_eq!(st.borrow().scroll, 17);
    }

    #[test]
    fn wheel_outside_dock_scrolls_messages() {
        // 指针不在 dock_area：保持原有 3 行/格滚消息区
        use ratatui::layout::Rect;
        let st = Rc::new(RefCell::new(test_app()));
        {
            let mut s = st.borrow_mut();
            s.dock_area = Some(Rect::new(0, 5, 40, 11));
            s.dock_offset = 2;
            s.scroll = 17;
        }
        handle_mouse_event(
            &st,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 10,
                row: 30,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(
            st.borrow().scroll,
            17 + WHEEL_STEP,
            "消息区按 WHEEL_STEP 滚动"
        );
        assert_eq!(st.borrow().dock_offset, 2, "面板偏移不变");
    }

    /// 滚轮只作用于指针所在的区域：输入框 / 状态栏 / 底栏等消息区之外的地方不该带动输出区
    /// （回归：在输入框与底栏上方滚动会把输出区滚走）。
    #[test]
    fn wheel_outside_log_area_does_not_scroll_messages() {
        use ratatui::layout::Rect;
        let st = Rc::new(RefCell::new(test_app()));
        {
            let mut s = st.borrow_mut();
            // 消息区占 0..20 行，下面 20..40 行是输入框/状态栏/底栏
            s.mouse.log_area = 0;
            s.mouse.log_area_h = 20;
            s.scroll = 17;
            s.dock_area = Some(Rect::new(0, 41, 40, 5));
        }

        for row in [20, 25, 33, 39] {
            for kind in [MouseEventKind::ScrollUp, MouseEventKind::ScrollDown] {
                handle_mouse_event(
                    &st,
                    MouseEvent {
                        kind,
                        column: 10,
                        row,
                        modifiers: KeyModifiers::NONE,
                    },
                );
                assert_eq!(st.borrow().scroll, 17, "第 {row} 行的滚轮不该滚输出区");
            }
        }

        // 消息区没渲染（高度 0）时同样不滚
        st.borrow_mut().mouse.log_area_h = 0;
        handle_mouse_event(
            &st,
            MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 10,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(st.borrow().scroll, 17);
    }

    #[test]
    fn alt_wheel_accelerates_scroll() {
        // 对齐 pi：按住 Alt 滚轮时滚动行数 ×ALT_WHEEL_MULTIPLIER（消息区 / 停靠面板均加速）。
        use ratatui::layout::Rect;
        let st = Rc::new(RefCell::new(test_app()));
        let wheel = |kind, mods| MouseEvent {
            kind,
            column: 10,
            row: 30,
            modifiers: mods,
        };

        // 普通滚轮：WHEEL_STEP 行/格
        handle_mouse_event(&st, wheel(MouseEventKind::ScrollUp, KeyModifiers::NONE));
        assert_eq!(st.borrow().scroll, WHEEL_STEP);

        // Alt+滚轮：WHEEL_STEP × 5
        st.borrow_mut().scroll = 0;
        handle_mouse_event(&st, wheel(MouseEventKind::ScrollUp, KeyModifiers::ALT));
        assert_eq!(st.borrow().scroll, WHEEL_STEP * ALT_WHEEL_MULTIPLIER);

        // 向下同样加速，且不会下溢
        st.borrow_mut().scroll = 100;
        handle_mouse_event(&st, wheel(MouseEventKind::ScrollDown, KeyModifiers::ALT));
        assert_eq!(st.borrow().scroll, 100 - WHEEL_STEP * ALT_WHEEL_MULTIPLIER);
        st.borrow_mut().scroll = 1;
        handle_mouse_event(&st, wheel(MouseEventKind::ScrollDown, KeyModifiers::ALT));
        assert_eq!(st.borrow().scroll, 0, "向下夹紧");

        // 停靠面板内：与消息区同一行数，Alt 后 ×5
        {
            let mut s = st.borrow_mut();
            s.scroll = 0;
            s.dock_area = Some(Rect::new(0, 5, 40, 11));
            s.dock_offset = 0;
        }
        handle_mouse_event(
            &st,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 6,
                modifiers: KeyModifiers::ALT,
            },
        );
        assert_eq!(
            st.borrow().dock_offset,
            WHEEL_STEP * ALT_WHEEL_MULTIPLIER,
            "停靠面板 Alt 加速"
        );
        assert_eq!(st.borrow().scroll, 0, "面板内不滚消息区");
    }

    #[test]
    fn drag_to_track_top_reaches_content_start_from_bottom() {
        // 回归：从底部附近（scroll 小）按住 thumb 向上拖到 track 顶，必须能看到开头内容。
        // 旧增量映射以 press_scroll 为基准递推，thumb 顶永远到不了 track 顶 → 顶部内容丢失。
        let mut st = test_app();
        st.mouse.log_area = 10;
        st.mouse.log_area_h = 10;
        st.mouse.log_total = 120;
        let max_scroll = 120 - 10;
        st.scroll = 5; // 内容底部附近
        st.mouse.scroll_thumb = Some((6, 2)); // thumb 相对 rel 6..8
        st.mouse.scroll_press = Some((16, 0)); // 按下在屏幕行 16 = rel 6（thumb 顶部，偏移 0）
        st.scrollbar_drag_to(10); // 拖到 track 顶（rel 0）
        assert_eq!(
            st.scroll, max_scroll,
            "拖到 track 顶必须看到开头, scroll={}",
            st.scroll
        );
    }

    #[test]
    fn drag_from_top_press_keeps_grab_offset() {
        // 按下在 thumb 中部（rel 2 偏移）拖到 track 顶：thumb 顶 = 鼠标行 - 偏移，
        // 同样必须到达内容顶部（端点一致性，与点击跳转同一把尺子）
        let mut st = test_app();
        st.mouse.log_area = 10;
        st.mouse.log_area_h = 10;
        st.mouse.log_total = 120;
        let max_scroll = 120 - 10;
        st.scroll = 55;
        st.mouse.scroll_thumb = Some((4, 3)); // thumb 相对 rel 4..7
        st.mouse.scroll_press = Some((16, 2)); // 按下 rel 6（thumb 中部，偏移 2）
        st.scrollbar_drag_to(10); // track 顶
        assert_eq!(st.scroll, max_scroll, "offset 跟手拖到顶也必须到开头");
        // 拖向 track 底：thumb 顶相对最终贴底（rel track-1），scroll=0
        st.scrollbar_drag_to(10 + (10 - 1) as u16); // 直接到 rel 9 位置（含偏移→thumb 顶 rel 7）
        let rel = (10 - 1) as usize;
        let _ratio = rel as f64 / 9.0;
        // 由于偏移 2，thumb 顶无法指导 rel 9，只能到 rel 7：期望 scroll = max*(1-7/9)
        let expected = ((max_scroll as f64 * (1.0 - 7.0 / 9.0)).round() as usize).min(max_scroll);
        assert_eq!(st.scroll, expected, "跟手偏移下拖到底的滚动位置");
    }

    #[test]
    fn drag_does_not_accumulate_thumb_offset_drift() {
        // 回归：grab_offset 必须按下时固定。旧实现每次拖动用最新 thumb_start 重算，
        // 向上拖时偏差累积（thumb 比鼠标快）；向下拖因 saturating_sub 钳 0 反而正常。
        let mut st = test_app();
        st.mouse.log_area = 0;
        st.mouse.log_area_h = 40;
        st.mouse.log_total = 1000;
        let _track = 40usize;
        let max_scroll = 1000 - 40;
        // 模拟点击 track 空白跳转后：thumb 顶 rel 10，按下偏移 0
        st.scroll = 714;
        st.mouse.scroll_thumb = Some((10, 2));
        st.mouse.scroll_press = Some((10, 0));
        // 向上拖 2 行 → thumb 顶应 = 鼠标行（rel 8）
        st.scrollbar_drag_to(8);
        let expected = ((max_scroll as f64 * (1.0 - 8.0 / 39.0)).round() as usize).min(max_scroll);
        assert_eq!(st.scroll, expected, "第一段拖动跟手: {}", st.scroll);
        // 渲染帧更新 thumb_start（模拟 render_frame 写入 mouse.scroll_thumb）
        st.mouse.scroll_thumb = Some((8, 2));
        // 再向上拖 2 行 → thumb 顶仍应 = 鼠标行（rel 6），不因 thumb_start 漂移
        st.scrollbar_drag_to(6);
        let expected = ((max_scroll as f64 * (1.0 - 6.0 / 39.0)).round() as usize).min(max_scroll);
        assert_eq!(
            st.scroll, expected,
            "拖动中 thumb_start 更新后仍跟手: {}",
            st.scroll
        );
    }

    #[test]
    fn scrollbar_click_jump_and_grab_drag() {
        let mut st = test_app();
        st.mouse.log_area = 10; // 消息区从 y=10 开始
        st.mouse.log_area_h = 10;
        st.mouse.log_total = 120;
        let track = 10usize;
        let max_scroll = 120 - 10;
        // track 顶端 = 内容顶部、底端 = 内容底部（点击 track 空白跳转）
        st.scroll = 0;
        st.mouse.scroll_thumb = Some((5, 2));
        st.scrollbar_click_jump(10);
        assert_eq!(st.scroll, max_scroll, "track 顶端点击 = 内容顶部");
        st.scrollbar_click_jump(19);
        assert_eq!(st.scroll, 0, "track 底端点击 = 内容底部");
        // 点击 thumb 区间：不跳转（仅抓取）
        st.scroll = 60;
        st.mouse.scroll_thumb = Some((5, 2)); // thumb 相对区间在 track 行 5..7（消息区 y10..20 → rel 5）
        st.mouse.scroll_press = Some((15, 0)); // 抓取：偏移 0（按下在 thumb 顶）
        assert!(st.scrollbar_press_on_thumb(15), "落在 thumb 上进入抓取");
        assert_eq!(st.scroll, 60, "thumb 上按下不跳转");
        // 抓取拖动：按住 thumb 顶（rel 5）拖向 track 底 → thumb 顶贴底 = 内容底部；
        // thumb 高 1（120 行内容/10 行 track），贴底上限 rel=9，拖出消息区底 clamp 贴底 = 内容底部
        st.mouse.scroll_press = Some((15, 0));
        st.scrollbar_drag_to((15 + (track - 1 - 5)) as u16); // rel 到 track-1
        assert_eq!(
            st.scroll, 0,
            "thumb 顶拖到 track 底 = 内容底部, scroll={}",
            st.scroll
        );
        st.scrollbar_drag_to(20);
        assert_eq!(st.scroll, 0, "thumb 贴底（clamp）= 内容底部");
        // 拖回 track 顶 → 内容顶部
        st.scrollbar_drag_to(10);
        assert_eq!(st.scroll, max_scroll, "thumb 拖到 track 顶 = 内容顶部");
        // 继续拖出消息区上下方：clamp 不越界
        st.scrollbar_drag_to(3);
        assert_eq!(st.scroll, max_scroll, "上方越界 clamp 到内容顶部");
        st.scrollbar_drag_to(99);
        assert_eq!(st.scroll, 0, "下方越界 clamp 到内容底部");
        // 内容不足一屏：无可滚动 → 0
        st.mouse.log_total = 5;
        st.mouse.log_area_h = 10;
        st.scrollbar_click_jump(0);
        assert_eq!(st.scroll, 0, "内容不足一屏时保持 0");
        // 渲染侧一致性：scroll=0 时 thumb 应贴 track 底（thumb_start + len == track）
        st.mouse.log_total = 120;
        st.mouse.log_area_h = 10;
        st.scroll = 0;
        let denom = (120 - 1 + track) as f64;
        let pos = (120 - 1) as f64 * (1.0 - 0.0 / max_scroll as f64);
        let tl = ((track as f64 * track as f64 / denom).round() as usize).clamp(1, track);
        let ts = ((pos * track as f64 / denom).round() as usize).min(track.saturating_sub(tl));
        assert_eq!(ts + tl, track, "scroll=0 时 thumb 贴 track 底");
    }

    #[test]
    fn drag_edge_auto_scroll_up_after_dwell() {
        // 拖拽中指针停在最顶行：滞留 EDGE_DWELL_TICKS 个 tick 才开始上滚，
        // 之后每 tick 滚一步，终点钉新视口顶行开头（选中向更早内容生长）
        let mut st = test_app();
        st.mouse.log_area = 0;
        st.mouse.log_area_h = 10;
        st.mouse.log_total = 120;
        st.scroll = 50;
        st.mouse.dragging = true;
        st.mouse.drag_row = Some(0); // 顶行（row == area_y）
        st.mouse.sel = Some((60, 2, 60, 2));
        // 滞留不足阈值：dwell 计数 1..3，均不滚
        for _ in 0..EDGE_DWELL_TICKS {
            assert!(!st.auto_scroll_tick(), "滞留不足不滚动");
        }
        assert_eq!(st.scroll, 50);
        // 满阈值后的下一次 tick 开始滚
        assert!(st.auto_scroll_tick());
        assert_eq!(st.scroll, 50 + DRAG_EDGE_STEP, "上滚一步");
        let start = st
            .mouse
            .log_total
            .saturating_sub(st.mouse.log_area_h as usize)
            .saturating_sub(st.scroll);
        assert_eq!(st.mouse.sel, Some((60, 2, start, 0)), "终点钉视口顶行开头");
        // 持续滚到内容顶部后自动停止
        let mut guard = 0;
        while st.auto_scroll_tick() && guard < 1000 {
            guard += 1;
        }
        assert_eq!(
            st.scroll,
            st.mouse.log_total - st.mouse.log_area_h as usize,
            "滚到内容顶部"
        );
        // 松开：复位且不再滚
        st.mouse.dragging = false;
        assert!(!st.auto_scroll_tick());
        assert_eq!(st.mouse.edge_dwell, 0);
    }

    #[test]
    fn drag_edge_auto_scroll_down_pins_bottom_line() {
        // 拖拽中指针在消息区下方：滞留满阈值后下滚，终点钉新视口底行行尾
        let mut st = test_app();
        st.mouse.log_area = 0;
        st.mouse.log_area_h = 10;
        st.mouse.log_total = 120;
        st.scroll = 60;
        st.mouse.dragging = true;
        st.mouse.drag_row = Some(10); // 消息区下方（row >= area_y+area_h）
        st.mouse.sel = Some((45, 0, 45, 0));
        for _ in 0..EDGE_DWELL_TICKS {
            st.auto_scroll_tick();
        }
        assert!(st.auto_scroll_tick(), "满阈值后下滚");
        assert_eq!(st.scroll, 60 - DRAG_EDGE_STEP);
        let start = st
            .mouse
            .log_total
            .saturating_sub(st.mouse.log_area_h as usize)
            .saturating_sub(st.scroll);
        let end_line = (start + 9).min(st.mouse.log_total - 1);
        assert_eq!(
            st.mouse.sel,
            Some((45, 0, end_line, usize::MAX)),
            "终点钉底行行尾"
        );
        // 贴底后不再滚
        st.scroll = 2;
        st.mouse.edge_dwell = EDGE_DWELL_TICKS;
        assert!(st.auto_scroll_tick());
        assert_eq!(st.scroll, 0);
        assert!(!st.auto_scroll_tick(), "贴底后不再滚");
    }

    #[test]
    fn drag_edge_ignores_pointer_inside_area() {
        // 指针在消息区内部：即使之前在边缘滞留过也不滚，并复位滞留计数
        let mut st = test_app();
        st.mouse.log_area = 0;
        st.mouse.log_area_h = 10;
        st.mouse.log_total = 120;
        st.scroll = 50;
        st.mouse.dragging = true;
        st.mouse.drag_row = Some(5); // 区域内
        st.mouse.edge_dwell = EDGE_DWELL_TICKS;
        assert!(!st.auto_scroll_tick());
        assert_eq!(st.scroll, 50, "区域内不滚动");
        assert_eq!(st.mouse.edge_dwell, 0, "离开边缘复位滞留计数");
    }

    #[test]
    fn drag_edge_bottom_row_triggers_down_scroll() {
        // 底行（row == area_y+area_h-1）停住也触发下滚，与顶行对称；
        // 消息区末行停住也触发下滚，与顶行对称
        let mut st = test_app();
        st.mouse.log_area = 0;
        st.mouse.log_area_h = 6;
        st.mouse.log_total = 120;
        st.scroll = 60;
        st.mouse.dragging = true;
        st.mouse.drag_row = Some(5); // 底行
        st.mouse.sel = Some((45, 0, 45, 0));
        for _ in 0..EDGE_DWELL_TICKS {
            assert!(!st.auto_scroll_tick());
        }
        assert!(st.auto_scroll_tick(), "底行滞留满阈值后下滚");
        assert_eq!(st.scroll, 60 - DRAG_EDGE_STEP, "底行停住触发下滚");
        let start = st
            .mouse
            .log_total
            .saturating_sub(st.mouse.log_area_h as usize)
            .saturating_sub(st.scroll);
        let end_line = (start + 5).min(st.mouse.log_total - 1);
        assert_eq!(st.mouse.sel, Some((45, 0, end_line, usize::MAX)));
    }

    #[test]
    fn drag_event_at_edge_keeps_dwell() {
        // 拖拽事件在边缘小幅移动不清零滞留计数（持续滚动不被打断），
        // 离开边缘才清零
        let mut st = test_app();
        st.mouse.visible = vec!["aaaa".to_string(); 6];
        st.mouse.log_area = 0;
        st.mouse.log_area_h = 6;
        st.mouse.dragging = true;
        st.mouse.edge_dwell = EDGE_DWELL_TICKS - 1; // 已在边缘滞留 2 tick
        st.mouse.sel = Some((10, 0, 10, 0));
        let ev = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 3,
            row: 5, // 底行（边缘内）
            modifiers: KeyModifiers::NONE,
        };
        st.handle_mouse_left_drag(&ev);
        assert_eq!(
            st.mouse.edge_dwell,
            EDGE_DWELL_TICKS - 1,
            "边缘内拖动保留滞留计数"
        );
        let ev = MouseEvent {
            kind: MouseEventKind::Drag(MouseButton::Left),
            column: 3,
            row: 3, // 区域中部
            modifiers: KeyModifiers::NONE,
        };
        st.handle_mouse_left_drag(&ev);
        assert_eq!(st.mouse.edge_dwell, 0, "离开边缘清零");
    }

    #[test]
    fn scroll_drag_edge_immediate_step() {
        // 拖拽事件本身（非 tick）越界时也立即滚一步
        let mut st = test_app();
        st.mouse.log_total = 120;
        st.mouse.sel = Some((60, 2, 60, 2));
        st.scroll = 60;
        assert!(st.scroll_drag_edge(10, false), "下滚一步");
        assert_eq!(st.scroll, 60 - DRAG_EDGE_STEP);
        assert!(st.scroll_drag_edge(10, true), "上滚一步");
        assert_eq!(st.scroll, 60);
        // 贴底时下滚失败、顶部时上滚失败
        st.scroll = 0;
        assert!(!st.scroll_drag_edge(10, false), "贴底不滚");
        st.scroll = 110; // max_scroll = 120-10
        assert!(!st.scroll_drag_edge(10, true), "顶部不滚");
    }

    /// 全屏覆盖层独占鼠标：滚轮（即使在底部区域）只滚覆盖层；
    /// 点覆盖层右侧滚动条列是抓取覆盖层 thumb，而不是主页滚动条。
    #[test]
    fn fullscreen_overlay_owns_mouse_scroll_and_scrollbar() {
        use crate::core::extensions::{DockSpan, OverlaySize, OverlayView};
        use ratatui::layout::Rect;

        let st = Rc::new(RefCell::new(test_app()));
        {
            let mut s = st.borrow_mut();
            let mut view = OverlayView::inline(
                "Viewer",
                (0..50)
                    .map(|i| vec![DockSpan::plain(format!("l{i}"))])
                    .collect(),
                6,
            );
            view.size = OverlaySize::Fullscreen;
            view.footer.clear();
            crate::modes::interactive::overlay::open(&mut s, 1, view);
            let o = s.overlay.as_mut().unwrap();
            o.content_rows = 5;
            o.area = Some(Rect::new(0, 0, 40, 10));
            crate::modes::interactive::overlay::record_content_geometry(o, 1, 5, Vec::new());
            crate::modes::interactive::overlay::record_scrollbar(o, Some(39), Some((0, 2)));
            s.scroll = 17;
        }

        // 滚轮：全屏覆盖层下任意行都归覆盖层，主页不动
        handle_mouse_event(
            &st,
            MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 10,
                row: 12,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(st.borrow().overlay.as_ref().unwrap().scroll, WHEEL_STEP);
        assert_eq!(st.borrow().scroll, 17, "主页消息区不动");

        // 点覆盖层滚动条列：抓取覆盖层 thumb，主页 scroll 不变
        handle_mouse_event(
            &st,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 39,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert!(st.borrow().overlay.as_ref().unwrap().sel.scroll_dragging);
        // 拖到 track 底 → 覆盖层滚到内容结尾（顶部偏移语义）
        handle_mouse_event(
            &st,
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 39,
                row: 5,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert_eq!(st.borrow().overlay.as_ref().unwrap().scroll, 45);
        assert_eq!(st.borrow().scroll, 17, "主页滚动位置不变");
    }

    /// 覆盖层内容拖选：按下→拖动设定选中区间，选中文本可提取。
    #[test]
    fn overlay_drag_selects_content() {
        use crate::core::extensions::{DockSpan, OverlaySize, OverlayView};
        use ratatui::layout::Rect;

        let st = Rc::new(RefCell::new(test_app()));
        {
            let mut s = st.borrow_mut();
            let mut view = OverlayView::inline(
                "Viewer",
                vec![
                    vec![DockSpan::plain("hello world")],
                    vec![DockSpan::plain("second line")],
                ],
                6,
            );
            view.size = OverlaySize::Fullscreen;
            view.footer.clear();
            crate::modes::interactive::overlay::open(&mut s, 1, view);
            let o = s.overlay.as_mut().unwrap();
            o.content_rows = 5;
            o.area = Some(Rect::new(0, 0, 40, 10));
            crate::modes::interactive::overlay::record_content_geometry(
                o,
                1,
                5,
                vec!["hello world".to_string(), "second line".to_string()],
            );
            crate::modes::interactive::overlay::record_scrollbar(o, None, None);
        }

        handle_mouse_event(
            &st,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 0,
                row: 1,
                modifiers: KeyModifiers::NONE,
            },
        );
        handle_mouse_event(
            &st,
            MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                column: 6,
                row: 2,
                modifiers: KeyModifiers::NONE,
            },
        );
        {
            let s = st.borrow();
            assert_eq!(s.overlay.as_ref().unwrap().sel.sel, Some((0, 0, 1, 6)));
            let text = crate::modes::interactive::overlay::selected_text(&s).unwrap();
            assert!(text.contains("hello world"), "{text:?}");
            assert!(text.contains("second"), "{text:?}");
        }
    }

    #[test]
    fn clipboard_paths_join_one_per_line_outside_bash_mode() {
        let paths = vec!["/tmp/photo.png".to_string(), "/tmp/b.png".to_string()];
        assert_eq!(
            format_clipboard_paths(&paths, false, (None, None)),
            "/tmp/photo.png\n/tmp/b.png"
        );
    }

    #[test]
    fn clipboard_paths_are_shell_quoted_in_bash_mode() {
        // 空格、命令替换、单引号都要转义；安全字符路径保持原样
        let paths = vec![
            "/tmp/My Photos/photo.png".to_string(),
            "/tmp/$(touch hacked).png".to_string(),
            "/tmp/it's.png".to_string(),
            "/tmp/plain.png".to_string(),
        ];
        assert_eq!(
            format_clipboard_paths(&paths, true, (None, None)),
            "'/tmp/My Photos/photo.png' '/tmp/$(touch hacked).png' '/tmp/it'\\''s.png' /tmp/plain.png"
        );
    }

    #[test]
    fn clipboard_paths_keep_space_from_adjacent_text() {
        // 光标两侧都是非空白字符时各补一个空格，避免路径与已有输入粘连
        let paths = vec!["/tmp/photo.png".to_string()];
        assert_eq!(
            format_clipboard_paths(&paths, false, (Some(':'), None)),
            " /tmp/photo.png"
        );
        assert_eq!(
            format_clipboard_paths(&paths, false, (None, Some('x'))),
            "/tmp/photo.png "
        );
        // 行首/行尾或相邻字符已是空白时不补
        assert_eq!(
            format_clipboard_paths(&paths, false, (None, None)),
            "/tmp/photo.png"
        );
        assert_eq!(
            format_clipboard_paths(&paths, false, (Some(' '), Some(' '))),
            "/tmp/photo.png"
        );
    }

    #[test]
    fn cursor_neighbours_reports_adjacent_chars() {
        let mut app = test_app();
        app.editor.replace_all("Review:");
        app.editor.cursor_col = app.editor.lines[0].chars().count();
        assert_eq!(app.editor.cursor_neighbours(), (Some(':'), None));
        app.editor.cursor_col = 0;
        assert_eq!(app.editor.cursor_neighbours(), (None, Some('R')));
    }
}

//! 键盘/事件处理
//!
//! 所有输入处理集中在 [`handle_event`]：全局快捷键、信任询问、忙碌态中断、
//! /命令、!cmd 外壳命令、@文件展开、Tab 补全、编辑器移动。
//!
//! 按功能拆分子模块：
//! - [`mouse`]    鼠标事件（滚轮/拖选/滚动条）
//! - [`commands`] /斜杠命令分发
//! - [`tools`]    外部编辑器/shell
//! - [`suggest`]  输入框候选/补全（/ 命令、@ 文件、Tab 路径补全）
//! - [`history`]  `/history` 候选（历史实时过滤、`@clear` / `@clear-all` 控制词）
//! - [`panels`]   模态选择面板（/model /session /theme /login /logout /extension）
//! - [`sessions`] 会话选择器（/resume /session）
//! - [`settings`] 设置选择器（/settings）、模型循环（Ctrl+P）
//! - [`trust`]    项目信任（启动询问面板 / `/trust` 对话框 / 未信任警告）
//!
//! 另外三块「由 worker/事件流驱动」的处理，按领域独立成模块：
//! - [`events`]        agent 事件与 worker 回执（`apply_sink_event` / `on_*`）
//! - [`messages`]      系统消息流与状态栏短提示的写入
//! - [`metrics`]       状态栏派生数据（git 分支 / context 占用 / 流式 token）
//! - [`confirms`]      确认面板族（/import、import cwd、settings 覆写、/history 删除）
//! - [`bug_report`]    `/bug` 报告流程（描述 → transcript → 摘要 → 导出 zip）
//! - [`session_repair`] 会话文件完整性与崩溃残留（修复面板 / 未完成 operation / `run_import`）

mod auth;
mod bug_report;
mod commands;
mod ext_settings;
mod extensions;
mod history;
mod messages;
mod metrics;
mod mouse;
mod overlay;
mod panels;
mod retry;
mod sessions;
mod settings;
mod skills;
mod steer;
mod suggest;
mod tools;
mod trust;

pub(crate) mod confirms;
pub(crate) mod events;
pub(crate) mod session_repair;

use self::{
    commands::{extension_keybinding_handler, is_busy_safe_command},
    mouse::handle_mouse_event,
    suggest::{complete_path, last_word},
    tools::{open_external_editor, run_shell_command},
};
use super::{
    app::{App, MsgLevel, SuggestionKind},
    line_input::printable_char,
};
use crate::{
    cli::file_processor,
    core::{
        self,
        extensions::UserPromptAction,
        keybindings, model_resolver, prompt_history,
        prompt_templates::expand_prompt_template,
        provider::{AgentMessage, ContentBlock},
        skills::expand_skill_command,
    },
    utils::image::ImageResizeLimits,
};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

pub(crate) use self::commands::{register_extension_keybinding, register_slash_command};
pub(crate) use retry::RetryUi;
pub use session_repair::{
    stage_session_repair_global, stage_zombie_operations_global, take_staged_session_repair,
    take_staged_zombie_operations,
};

/// 按键处理的去向：继续主循环、退出程序，或中断当前回合。
pub enum KeyAction {
    /// 按键已消费，继续主循环。
    Continue,
    /// 退出 TUI 主循环。
    Quit,
    /// 中断当前正在进行的回合（如 Esc / Ctrl-C）。
    CancelPrompt,
}

/// 扩展快捷键分发：遍历已启用扩展的 keybinding 声明，
/// 命中即查 TUI 注册表执行入口并调用。busy 态跳过——扩展 handler 可能锁 agent rebuild_tools，
/// prompt future 正持锁挂起会死锁（与内置需锁快捷键同语义）。返回是否已消费该按键。
fn dispatch_extension_keybinding(st: &mut App, key: &KeyEvent) -> bool {
    if st.busy {
        return false;
    }

    for ext in core::extensions::registered() {
        for binding in ext.keybindings() {
            // 扩展快捷键不进静态 KEYBINDINGS 表（KeybindingsManager 只含内置 id），这里直接解析声明键并匹配事件。
            let hit = binding.keys.iter().any(|k| {
                keybindings::parse_key_id(k)
                    .is_some_and(|kid| keybindings::matches_event(key, &kid))
            });

            if !hit {
                continue;
            }

            if let Some(handler) = extension_keybinding_handler(ext.name(), &binding.id) {
                handler(st);
                st.dirty = true;
                return true;
            }
        }
    }
    false
}

/// 扩展输入拦截分发：返回 `Some(text)` 表示继续提交（可能是改写后的文本）；
/// `None` 表示已被扩展认领（不启动回合 / 不排队）。
///
/// 先广播提交通知（[`core::extensions::dispatch_user_submit`]）再做拦截：
/// 无论输入是否被认领/改写，扩展都应能感知“用户开新回合了”（子代理据此把
/// 上一轮已完成的记录从 dock 隐去）。
fn intercept_user_prompt(text: &str, messages: &[AgentMessage], busy: bool) -> Option<String> {
    core::extensions::dispatch_user_submit(text, busy);

    match core::extensions::dispatch_user_prompt(text, messages, busy) {
        Some(UserPromptAction::Handled) => None,
        Some(UserPromptAction::Rewrite(t)) => Some(t),
        Some(UserPromptAction::Continue) | None => Some(text.to_string()),
    }
}

/// 处理终端事件（键盘 / resize / 粘贴）
pub fn handle_event(shared: &Rc<RefCell<App>>, event: Event) -> KeyAction {
    // 终端 resize：100ms 节流合并重绘；同步编辑器视觉行宽度
    if let Event::Resize(w, _) = event {
        let mut st = shared.borrow_mut();
        // 输入框减去左右边距（对齐 render_input_box 的 2px padding）
        st.editor.set_visual_width(w.saturating_sub(2) as usize);
        if st.last_resize.elapsed() > Duration::from_millis(100) {
            st.dirty = true;
            st.last_resize = Instant::now();
        }
        return KeyAction::Continue;
    }

    // bracketed paste：按当前激活的输入框分发（设置选择器 / 会话选择器 / 面板 / 编辑器）
    if let Event::Paste(data) = event {
        let mut st = shared.borrow_mut();
        if st.settings_selector.active {
            if st.settings_selector.submenu.is_none() {
                st.settings_selector.filter.insert_text(&data);
                st.settings_selector.recompute();
            }
        } else if st.session_selector.active {
            if st.session_selector.rename_mode {
                st.session_selector.rename_input.insert_text(&data);
            } else {
                st.session_selector.filter.insert_text(&data);
                st.session_selector.recompute();
            }
        } else if st.overlay.is_some() {
            st.handle_overlay_paste(&data);
        } else if st.panel.active {
            // 无输入行面板（确认框等）不把粘贴文本落入不可见 filter；
            // 有输入行的面板（LoginKey API key / 搜索框）正常接收。
            if let Some(layer) = st.panel.top_mut()
                && layer.kind.has_filter_input()
            {
                layer.filter.insert_text(&data);
                layer.selected = 0;
            }
        } else if st.ext_settings.visible {
            // 扩展设置面板无文本输入：吞掉粘贴，避免落入不可见编辑器
        } else {
            st.editor.paste_text(&data);
        }
        st.dirty = true;
        return KeyAction::Continue;
    }

    // 鼠标事件（滚动 / 拖选）
    if let Event::Mouse(mouse) = event {
        handle_mouse_event(shared, mouse);
        return KeyAction::Continue;
    }
    let Event::Key(key) = event else {
        return KeyAction::Continue;
    };

    // 只认按键按下（`Press`/`Repeat`），丢弃抬起（`Release`）：Windows 走 legacy console
    // API 时 crossterm 的 winapi reader 会为同一次按键先后上报 Press 与 Release，且两者
    // `KeyCode` 完全相同——不过滤就会「敲一个 a 输入框出现 aa」。
    // 代价：Windows 上 Alt+数字小键盘 输入码位只会在 Release 里带出字符，该输入方式失效；
    // 若保留 Release 又会与 Alt+字母 的快捷键（alt+b/alt+f/alt+d…）重复触发。
    if key.kind == KeyEventKind::Release {
        return KeyAction::Continue;
    }

    let mut st = shared.borrow_mut();

    // 扩展覆盖层（FleetView / 会话查看器）：存在时接管键盘
    if st.overlay.is_some() {
        return st.handle_overlay_key(&key);
    }

    // 模态选择面板优先于忙碌态捷径：busy 时经 busy_safe 命令（如 /goal）弹出的
    // 面板也必须能被 Enter 确认、Esc/Ctrl+C 关闭，否则这些键会被 handle_busy_key 当成 steer/中断吞掉。
    if st.panel.active {
        return st.handle_panel_key(&key);
    }

    // 扩展设置面板：显示期间接管键盘（↑/↓ 移动、Space/Enter 循环、Esc/Ctrl+C 隐藏）
    if st.ext_settings.visible {
        return st.handle_ext_settings_key(&key);
    }

    // 忙碌态：中断/退出/拦截；未消费的键放行到下方正常处理
    if st.busy
        && let Some(action) = handle_busy_key(&mut st, &key)
    {
        return action;
    }

    // 空闲时 Esc：先取消可取消的后台工作（如 MCP OAuth 登录），没有在飞工作时才放行给编辑器。
    if !st.busy
        && keybindings::get_global().matches(&key, "app.interrupt")
        && core::extensions::cancel_background_work() > 0
    {
        st.status = "cancelled background work".to_string();
        st.dirty = true;
        return KeyAction::Continue;
    }

    // /settings 设置选择器：键盘完全让位
    if st.settings_selector.active {
        return st.handle_settings_selector_key(&key);
    }

    // /resume、/session 会话选择器：键盘完全让位
    if st.session_selector.active {
        return st.handle_session_selector_key(&key);
    }

    // 编辑器按键：全部由全局 keybindings 表驱动（含刷新候选列表）
    let action = handle_editor_key(&mut st, &key);
    match action {
        EditorAction::Quit => KeyAction::Quit,
        // 外部编辑器：先释放 RefMut 再打开（避免双重可变借用）
        EditorAction::OpenExternal(text) => {
            drop(st);
            open_external_editor(shared, text);
            KeyAction::Continue
        }
        EditorAction::Continue => KeyAction::Continue,
    }
}

/// 忙碌态按键：中断 / 退出 / 拦截需要锁 agent 的快捷键；Enter 入 steer 队列。
/// 返回 Some 表示按键已消费；None 表示未消费，放行到正常处理。
fn handle_busy_key(st: &mut App, key: &KeyEvent) -> Option<KeyAction> {
    let kb = keybindings::get_global();
    // 任务运行/流式输出时中断当前任务（默认 Esc）
    if kb.matches(key, "app.interrupt") {
        st.status = "interrupting...".to_string();
        st.dirty = true;
        return Some(KeyAction::CancelPrompt);
    }

    // 普通 Ctrl+C（app.clear）：输入框有内容时先清空输入
    // （与空闲态语义一致，不清除忙碌、不打断运行——待发的 steer 文本应可随时丢弃）；
    // 输入框为空时才中断当前任务。
    // 带 SHIFT 的 Ctrl+Shift+C 放行到编辑器（复制选中文本），避免忙碌时选区复制被误判为中断
    if kb.matches(key, "app.clear") && !key.modifiers.contains(KeyModifiers::SHIFT) {
        if !st.editor.is_empty() {
            st.editor.clear();
            st.refresh_suggestions(); // 内容已变：关闭可能激活的 / 命令候选等
            st.dirty = true;
            return Some(KeyAction::Continue);
        }
        st.status = "interrupting...".to_string();
        st.dirty = true;
        return Some(KeyAction::CancelPrompt);
    }

    if kb.matches(key, "app.exit") {
        // Ctrl+D 直接退出（运行中的请求随退出丢弃，
        // 排队的 steer 随进程结束丢弃；不锁 agent——prompt future 正持锁挂起）
        st.status = "exiting...".to_string();
        st.dirty = true;
        return Some(KeyAction::Quit);
    }

    // busy 时 Shift+Tab（cycle_thinking）需要锁 agent 读模型，拦截避免死锁。
    // 用 keybinding 匹配而非 BackTab 判定：部分终端把 Shift+Tab 上报为
    // Char('\t')+SHIFT，BackTab 判定会漏拦 → cycle_thinking 锁 agent 死锁。
    if kb.matches(key, "app.thinking.cycle") {
        return Some(KeyAction::Continue);
    }

    // busy 时 Ctrl+P / Shift+Ctrl+P（cycle model）同样需要锁 agent，拦截避免死锁
    if kb.matches(key, "app.model.cycleForward") || kb.matches(key, "app.model.cycleBackward") {
        return Some(KeyAction::Continue);
    }

    // 忙碌时仍可输入——Enter（tui.input.submit）将文本加入 steer 队列
    // （当前轮结束后自动继续执行），其余编辑键（含 Up/Down 历史）放行。
    // 但声明为 busy_safe 的扩展命令（handler 不锁 agent，如 plan-mode 的 /todos）
    // 立即分发执行，避免被误当普通消息排队。
    if kb.matches(key, "tui.input.submit")
        && !key.modifiers.contains(KeyModifiers::SHIFT)
        && !key.modifiers.contains(KeyModifiers::CONTROL)
    {
        // 忙碌时回车（发送 steer / 空回车）也把消息区滚动条滚回底部
        st.scroll = 0;
        st.dirty = true;

        // 候选面板激活时先应用选中项（与空闲提交一致）：Enter 发送/执行的是面板里
        // 选中的命令，而不是输入框里未完成的前缀（如 `/agen` 选中 `/agents`）。
        // 命令/子命令应用后继续走下方 busy_safe 判定：busy_safe 命令立即执行，
        // 其余按应用后的完整命令文本入 steer 队列。
        if st.suggestion.active {
            match st.suggestion.kind {
                SuggestionKind::Commands | SuggestionKind::Subcommands => st.apply_suggestion(),
                // @ 文件候选：只补全路径，不发送（对齐空闲态）
                SuggestionKind::Files => {
                    st.apply_suggestion();
                    return Some(KeyAction::Continue);
                }
                // 历史候选：整段替换后落到下方普通文本提交（busy 时即入 steer 队列）
                SuggestionKind::History => {
                    if st.suggestion.selected >= st.suggestion.items.len() {
                        return Some(KeyAction::Continue);
                    }
                    st.apply_suggestion();
                }
                // 控制词（@clear / @clear-all）：Enter 弹确认面板，不发送内容
                SuggestionKind::HistoryAction => {
                    execute_history_action(st);
                    return Some(KeyAction::Continue);
                }
            }
        }

        let raw = st.editor.text();
        if let Some(cmd) = raw
            .strip_prefix('/')
            .map(str::trim)
            .filter(|c| is_busy_safe_command(c))
        {
            let cmd = cmd.to_string();
            submit_and_record(st); // 清空编辑器并记录（/命令不落历史），与空闲提交一致
            _ = st.handle_slash_command(&cmd);
        } else {
            // 与空闲提交同一条展开链：先 /skill:name + prompt 模板展开，再做扩展拦截，
            // 最后入 steer 队列——忙碌时 skill 也要按 skill 命令处理，不能原样当普通文本发。
            // 记录的是**用户原文**（编辑器里写的），不是展开/改写后的文本。
            let expanded = expand_user_text(st, &raw);
            if let Some(text) = intercept_user_prompt(&expanded, &st.messages, st.busy) {
                // 扩展输入拦截（@mention 等）：继续排队（可能是改写后的文本）。
                submit_and_record(st);
                st.queue_steer_text(text, false);
            } else {
                // 已被扩展认领：不排队、不启动回合（扩展自行投递）
                submit_and_record(st);
            }
        }

        return Some(KeyAction::Continue);
    }

    // 其他键（含 / 命令候选、@ 文件补全、Up/Down 历史）：放行到下方正常处理
    None
}

/// 编辑器按键处理结果：OpenExternal 需在释放 RefMut 后由调用方打开外部编辑器
enum EditorAction {
    /// 继续主循环。
    Continue,
    /// 退出程序。
    Quit,
    /// 打开外部编辑器，字符串为待编辑的当前草稿内容。
    OpenExternal(String),
}

// 四层按键分发：app 动作 > tui.input > tui.editor > 可打印字符输入。
// 各层消费后统一在下方刷新候选列表：删除/换行/撤销/清空都会改变内容，
// 必须按最新输入重算 / 与 @ 候选（否则删除触发符后面板残留）；
// 纯光标移动等内容未变时 refresh_suggestions 内部按版本号短路。
/// 四层按键分发的编辑器入口：app 动作 > tui.input > tui.editor > 可打印字符。
///
/// 除退出与打开外部编辑器外，任何路径都会重算 / 与 @ 候选并置脏标记；
/// 返回调用方接下来要执行的动作。
fn handle_editor_key(st: &mut App, key: &KeyEvent) -> EditorAction {
    let action = if let Some(action) = handle_app_action_key(st, key) {
        Some(action)
    } else if let Some(action) = handle_input_submit_key(st, key) {
        Some(action)
    } else if let Some(action) = handle_input_nav_key(st, key) {
        Some(action)
    } else if let Some(action) = handle_editor_edit_key(st, key) {
        Some(action)
    } else {
        // 可打印字符输入（Shift 层映射 / 普通字符 / CapsLock）；Esc 取消候选列表。
        // 未绑定任何动作的 Ctrl/Alt 组合不输入（只输入无修饰字符）
        match printable_char(key) {
            Some(c) => st.editor.insert_char(c),
            None => {
                if key.code == KeyCode::Esc {
                    st.suggestion.deselect();
                }
            }
        }
        None
    };

    // 退出 / 打开外部编辑器时不刷新；其余路径（含删除等编辑键、纯输入）统一刷新候选
    if !matches!(
        action,
        Some(EditorAction::Quit) | Some(EditorAction::OpenExternal(_))
    ) {
        st.refresh_suggestions();
    }

    st.dirty = true;
    action.unwrap_or(EditorAction::Continue)
}

/// app 级动作键：退出 / 外部编辑器 / 复制 / 剪贴板 / 清空 / 挂起 / 模型 / 工具 / 主题 / 滚动。
/// 命中返回 Some（已消费）；未命中返回 None，交给 tui.input 层级。
fn handle_app_action_key(st: &mut App, key: &KeyEvent) -> Option<EditorAction> {
    let kb = keybindings::get_global();

    // app.exit：编辑器空时退出；非空时向下落到 tui.editor.deleteCharForwardC
    if kb.matches(key, "app.exit") && st.editor.is_empty() {
        return Some(EditorAction::Quit);
    }

    // 外部编辑器（$EDITOR）：临时文件编辑后读回（paste marker 先展开）
    if kb.matches(key, "app.editor.external") {
        let text = st.editor.expanded_text();
        return Some(EditorAction::OpenExternal(text));
    }

    if kb.matches(key, "app.editor.copy") {
        // app.editor.copy：复制主页输入框全部内容（Alt+C）
        st.copy_editor_content();
    } else if kb.matches(key, "app.message.copy") {
        // app.message.copy：复制最后一条助手消息
        st.copy_last_assistant_message();
    } else if kb.matches(key, "tui.input.copy") {
        // 优先复制鼠标选中文本；无选中时复制最后一条助手消息
        if st.selected_text().is_some() {
            st.copy_selection();
        } else {
            st.copy_last_assistant_message();
        }
    } else if kb.matches(key, "app.clipboard.pasteImage")
        || (matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.modifiers.contains(KeyModifiers::SHIFT))
    {
        st.paste_clipboard(); // 粘贴剪贴板；Ctrl+Shift+V 为兼容键
    } else if kb.matches(key, "app.clear") {
        // handleCtrlC：清编辑器；500ms 内第二次触发退出
        let now = Instant::now();
        let double = st
            .last_ctrl_c
            .map(|t| now.duration_since(t) < Duration::from_millis(500))
            .unwrap_or(false);
        st.last_ctrl_c = Some(now);

        if double {
            return Some(EditorAction::Quit);
        }

        if !st.editor.is_empty() {
            st.editor.clear();
        }
    } else if kb.matches(key, "app.suspend") {
        // Ctrl+Z 挂起进程到后台（undo 用 Ctrl+-）；Windows 无作业控制，忽略
        #[cfg(unix)]
        {
            _ = nix::sys::signal::raise(nix::sys::signal::Signal::SIGTSTP);
        }
    } else if kb.matches(key, "app.model.select") {
        st.open_model_panel(); // 打开模型选择器
    } else if kb.matches(key, "app.tools.expand") {
        // 工具输出折叠/展开（全局值覆盖所有非 thinking 块，丢弃其逐块状态）
        st.toggle_expand_all();

        let expanded = st.expand_all;
        st.set_status_msg(if expanded {
            "expanded tool output (Ctrl+O to fold)".to_string()
        } else {
            "folded tool output (Ctrl+O to expand)".to_string()
        });
    } else if kb.matches(key, "app.thinking.toggle") {
        // Ctrl+T = 思考块显示/隐藏（丢弃 thinking 的逐块状态）
        st.toggle_show_thinking();

        let shown = st.show_thinking;
        st.set_status_msg(if shown {
            "thinking blocks visible (Ctrl+T to hide)".to_string()
        } else {
            "thinking blocks hidden (Ctrl+T to show)".to_string()
        });
    } else if kb.matches(key, "app.theme.cycle") {
        st.cycle_theme(); // Ctrl+Shift+T：循环主题
    } else if kb.matches(key, "app.model.cycleBackward")
        && !kb.matches(key, "tui.editor.historyPrevious")
    {
        st.cycle_model_dir(-1);
    } else if kb.matches(key, "app.model.cycleForward")
        && !kb.matches(key, "tui.editor.historyNext")
    {
        st.cycle_model_dir(1);
    } else if kb.matches(key, "app.thinking.cycle") {
        st.cycle_thinking(); // Shift+Tab 循环 thinking 级别
    } else if kb.matches(key, "app.message.dequeue") {
        st.dequeue_steers(); // Alt+Up：把排队的 steer 取回编辑器
    } else if kb.matches(key, "app.message.followUp") {
        st.queue_steer(true); // Alt+Enter：排队 follow-up 消息
    } else if kb.matches(key, "app.message.scrollToTop") {
        // shift+home：消息区滚动到顶部
        st.scroll = st.max_scroll;
        st.dirty = true;
    } else if kb.matches(key, "app.message.scrollToBottom") {
        // shift+end：消息区滚动到底部（跟随最新）
        st.scroll = 0;
        st.dirty = true;
    } else if kb.matches(key, "app.message.pageUp") {
        // shift+pageup：消息区向上翻一页（视口高度取自最近一帧）
        st.scroll = (st.scroll + message_page(st)).min(st.max_scroll);
        st.dirty = true;
    } else if kb.matches(key, "app.message.pageDown") {
        // shift+pagedown：消息区向下翻一页（scroll=0 即底部，跟随最新）
        st.scroll = st.scroll.saturating_sub(message_page(st));
        st.dirty = true;
    } else {
        return None;
    }
    Some(EditorAction::Continue)
}

/// 消息区翻页步长：取最近一帧渲染的消息区高度（`shift+pageup/pagedown` 用）。
/// 尚未渲染过（高度 0）时退化为 1 行，避免翻页变成 no-op。
fn message_page(st: &App) -> usize {
    (st.mouse.log_area_h as usize).max(1)
}

/// 提交输入框内容并把 prompt 追加落盘。
///
/// 所有提交路径（空闲发送、busy 时 steer、follow-up 队列）都走它，保证：
/// - **历史上不做内容过滤**：任何非空输入（含 `/` 命令、`!`/`!!` shell 行）都记，
///   不丢用户输入过的内容；`Editor::submit` 已保证拿到的 `text` 非空，空白不入历史；
/// - 落盘与内存截断同步：追加后按当前档位截断 `editor.history`，
///   使「本次会话 ↑/↓ 能翻到的」与「重启后能翻到的」一致。
///
/// `max == 0`（不落盘）时 `append` 是 no-op，`cap_history(0)` 也不截断——那个档位只关持久化，不关功能。
fn submit_and_record(st: &mut App) -> Option<String> {
    let text = st.editor.submit()?;
    prompt_history::append(&st.cwd, &text, st.history_max_entries);
    st.editor.cap_history(st.history_max_entries);
    Some(text)
}

/// 展开用户提交文本：`/skill:name [args]` → skill 块，再做 prompt 模板展开。
///
/// 空闲提交、busy 时 steer、follow-up 排队共用同一条展开链——
/// 忙碌时 skill 也必须按 skill 命令处理，而不是原样当普通文本发给模型。
pub(crate) fn expand_user_text(st: &App, text: &str) -> String {
    let expanded = expand_skill_command(text, &st.skills);
    expand_prompt_template(&expanded, &st.prompt_templates)
}

/// 执行 `/history` 控制词候选（`@clear` / `@clear-all`）：弹出确认面板，
/// 不发送任何内容，命令原文（`/history @clear`）随单行一起清掉——它是一次命令调用。
/// 空闲与忙碌提交路径共用；零匹配/空历史时面板留在原地，提交被吞掉。
fn execute_history_action(st: &mut App) {
    if st.suggestion.selected >= st.suggestion.items.len() {
        return;
    }

    let token = st.suggestion.items[st.suggestion.selected].insert.clone();
    st.suggestion.deselect();
    st.editor.clear();
    st.dirty = true;
    st.open_history_clear_confirm(token == "@clear-all");
}

/// Enter 提交：候选列表应用 / 发送消息 / !shell / /命令 / 技能展开。
/// 命中返回 Some；未命中返回 None，交给导航层级。
fn handle_input_submit_key(st: &mut App, key: &KeyEvent) -> Option<EditorAction> {
    let kb = keybindings::get_global();
    if kb.matches(key, "tui.input.submit") {
        // 字符前是 \ 时 Enter 退格+换行（终端无 Shift+Enter 回退）
        if st.editor.should_submit_on_backslash() {
            st.editor.backslash_enter();
            st.dirty = true;
            return Some(EditorAction::Continue);
        }

        // 回车（有内容发送 / 空回车）都把消息区滚动条滚回底部
        st.scroll = 0;
        st.dirty = true;
        st.banner_hidden = true;

        // 候选列表激活：先应用补全，再决定是否提交。
        // - Commands/Subcommands：补全 + 立即执行（Tab/Enter 含义对齐 / 面板）
        // - Files：只补全，不发送
        // - History：整段替换后落到下面的普通 prompt 发送路径（Enter = 直接发送；
        //   Tab 走 handle_input_nav_key，只填入不发送）
        if st.suggestion.active {
            match st.suggestion.kind {
                SuggestionKind::Commands | SuggestionKind::Subcommands => {
                    st.apply_suggestion();
                    if let Some(text) = submit_and_record(st)
                        && let Some(cmd) = text.strip_prefix('/')
                        && st.handle_slash_command(cmd.trim())
                    {
                        return Some(EditorAction::Quit);
                    }
                    return Some(EditorAction::Continue);
                }
                SuggestionKind::Files => {
                    // @ 文件候选：应用补全路径（含可能的子目录），不发送
                    st.apply_suggestion();
                    return Some(EditorAction::Continue);
                }
                SuggestionKind::History => {
                    // 零匹配/空历史：面板留在原地显示「No matching history」，
                    // Enter 被吞掉——不发送、不清行、不关面板，继续改关键词
                    if st.suggestion.selected >= st.suggestion.items.len() {
                        return Some(EditorAction::Continue);
                    }
                    st.apply_suggestion();
                }
                SuggestionKind::HistoryAction => {
                    // `@clear` / `@clear-all`：Enter 执行（弹确认面板），
                    // 不发送任何内容，也不落到下面的 prompt 提交路径。
                    execute_history_action(st);
                    return Some(EditorAction::Continue);
                }
            }
        }

        if let Some(text) = submit_and_record(st) {
            // !cmd / !!cmd：shell 命令。
            // Bash mode：两者都立即执行并 显示输出到 TUI，但都不触发新的 LLM 回合；
            // 区别在于!cmd 输出进入会话上下文，供后续对话使用，
            // !!cmd 输出被排除在上下文之外（Hidden shell command）。
            if let Some(shell_cmd) = text.strip_prefix("!!") {
                let output = run_shell_command(shell_cmd.trim());
                st.push_msg(
                    format!("$ {}\n{}", shell_cmd.trim(), output),
                    MsgLevel::Info,
                );
            } else if let Some(shell_cmd) = text.strip_prefix('!') {
                let output = run_shell_command(shell_cmd.trim());
                let final_text = format!("$ {}\n{}", shell_cmd.trim(), output);
                let msg = AgentMessage::user_text(&final_text);
                st.messages.push(msg.clone());
                st.worker.append_message(msg);
            } else if let Some(cmd) = text.strip_prefix('/') {
                if st.handle_slash_command(cmd.trim()) {
                    return Some(EditorAction::Quit);
                }
            } else {
                // @ 文件引用保持原样（@ 只做路径补全，提交为引用文本，
                // 不展开文件内容到消息，TUI 渲染显示 `@路径`）→ /skill:name → prompt template。
                // skills 启动固定 /reload 更新，App 缓存读，不锁 agent。
                let expanded = expand_user_text(st, &text);

                // 扩展输入拦截：扩展（如 plan-mode 精炼 / @mention）可改写文本或直接认领提交；
                // 认领则不启动回合、不渲染用户消息。
                let Some(final_text) = intercept_user_prompt(&expanded, &st.messages, st.busy)
                else {
                    return Some(EditorAction::Continue);
                };

                // @图片 引用展开为用户消息图片附件；非图片 @ 路径保持原样（现有引用行为不变）
                let (expanded_text, img_files) = file_processor::expand_at_image_refs(
                    &final_text,
                    &st.cwd,
                    current_image_resize_limits(st),
                );

                if img_files.is_empty() {
                    if st.busy {
                        // busy 中提交：直接排队到 worker，不立即渲染；
                        // 回合开始时 agent 重放 user 消息（message_end），顺序正确
                        st.worker.prompt(final_text.clone());
                    } else {
                        st.messages.push(AgentMessage::user_text(&final_text));
                        st.status_start = Some(final_text);
                    }
                } else {
                    let mut msg = AgentMessage::user_text(&expanded_text);
                    for (data, mime) in img_files {
                        msg.content.push(ContentBlock::Image {
                            data,
                            mime_type: mime,
                        });
                    }

                    if st.busy {
                        // busy 中提交：直接排队到 worker，回合开始时 agent 重放（含图片块）
                        st.worker.prompt_message(msg);
                    } else {
                        // 仅入队：NextAction::PromptMessage 消费时会 push 到 st.messages
                        st.initial_queue.push_back(msg);
                    }
                }
            }
        }
    } else {
        return None;
    }
    Some(EditorAction::Continue)
}

/// 输入区导航/补全键：换行（Shift+Enter / Ctrl+J）、Ctrl+Enter 兼容换行、Tab 补全/折叠。
/// 命中返回 Some；未命中返回 None，交给编辑器编辑层级。
fn handle_input_nav_key(st: &mut App, key: &KeyEvent) -> Option<EditorAction> {
    let kb = keybindings::get_global();
    if kb.matches(key, "tui.input.newLine") {
        // 候选列表激活时 Ctrl+J 下移选中项； 换行（Shift+Enter / Ctrl+J）
        if st.suggestion.active
            && key.code == KeyCode::Char('j')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            st.suggestion_next();
        } else {
            st.editor.newline();
        }
    } else if key.code == KeyCode::Enter && key.modifiers.contains(KeyModifiers::CONTROL) {
        st.editor.newline(); // Ctrl+Enter：兼容换行
    } else if kb.matches(key, "tui.input.tab") {
        // 候选列表激活时：应用当前选中项
        if st.suggestion.active {
            st.apply_suggestion();
        } else {
            // 输入非空时补全路径；空时切换折叠
            let text = st.editor.text();
            if !text.trim().is_empty() {
                // 用 App.cwd（不锁 agent：busy 时 prompt future 持锁挂起）
                let cwd = st.cwd.clone();
                if let Some((prefix, start)) = last_word(&text)
                    && let Some(completed) = complete_path(&prefix, &cwd)
                {
                    st.editor.clear();
                    let new_text = format!("{}{}", &text[..start], completed);
                    st.editor.insert_text(&new_text);
                }
            } else {
                st.toggle_expand_all();
                let expanded = st.expand_all;
                st.set_status_msg(if expanded {
                    "expanded all messages (Ctrl+O to fold)".to_string()
                } else {
                    "folded messages (Ctrl+O to expand)".to_string()
                });
            }
        }
    } else {
        return None;
    }
    Some(EditorAction::Continue)
}

/// 编辑器编辑键：kill/yank/undo、光标移动、翻页、删词、扩展快捷键兜底。
/// 命中返回 Some；未命中返回 None（落到可打印字符输入）。
/// 编辑器编辑键：kill/yank、光标移动、字符删除、扩展快捷键兜底。
/// 命中返回 Some；未命中返回 None（落到可打印字符输入）。
fn handle_editor_edit_key(st: &mut App, key: &KeyEvent) -> Option<EditorAction> {
    if let Some(action) = handle_kill_yank_key(st, key) {
        return Some(action);
    }
    if let Some(action) = handle_cursor_move_key(st, key) {
        return Some(action);
    }
    if let Some(action) = handle_char_delete_key(st, key) {
        return Some(action);
    }
    None
}

/// kill / yank / undo 键：删到行尾/行首、粘贴 kill-ring、撤销。
fn handle_kill_yank_key(st: &mut App, key: &KeyEvent) -> Option<EditorAction> {
    let kb = keybindings::get_global();
    if kb.matches(key, "tui.editor.deleteToLineEnd") {
        // 候选列表激活时 Ctrl+K 上移选中项；否则删到行尾
        if st.suggestion.active
            && key.code == KeyCode::Char('k')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            st.suggestion_prev();
        } else {
            let killed = st.editor.kill_line_end();
            if !killed.is_empty() {
                st.kill_buffer = killed.clone();
                st.kill_ring.push(killed);
                st.yank_index = st.kill_ring.len();
                st.last_kill_was_word = false;
            }
        }
    } else if kb.matches(key, "tui.editor.deleteToLineStart") {
        // Ctrl+U = 删到行首
        let killed = st.editor.kill_line_start();
        if !killed.is_empty() {
            st.kill_buffer = killed.clone();
            st.kill_ring.push(killed);
            st.yank_index = st.kill_ring.len();
            st.last_kill_was_word = false;
        }
    } else if kb.matches(key, "tui.editor.yank") {
        // Ctrl+Y = 粘贴最近删除的文本（对齐 pi KillRing yank：从最新条目开始）
        if let Some(buf) = st.kill_ring.last().cloned().or_else(|| {
            if st.kill_buffer.is_empty() {
                None
            } else {
                Some(st.kill_buffer.clone())
            }
        }) {
            st.yank_index = st.kill_ring.len();
            st.editor.insert_text(&buf);
        }
    } else if kb.matches(key, "tui.editor.yankPop") {
        // Alt+Y = yankPop：循环更旧的删除条目（对齐 pi KillRing yankPop）
        if !st.kill_ring.is_empty() {
            if st.yank_index == 0 {
                st.yank_index = st.kill_ring.len();
            }
            st.yank_index -= 1;
            if let Some(buf) = st.kill_ring.get(st.yank_index).cloned() {
                st.editor.insert_text(&buf);
            }
        }
    } else if kb.matches(key, "tui.editor.undo") {
        st.editor.undo();
    } else {
        return None;
    }
    Some(EditorAction::Continue)
}

/// 光标移动 / 历史 / 翻页键。
fn handle_cursor_move_key(st: &mut App, key: &KeyEvent) -> Option<EditorAction> {
    let kb = keybindings::get_global();
    if kb.matches(key, "tui.editor.cursorUp") {
        // 候选列表导航；否则编辑器内上移，已在首行行首时切换到上一条历史
        if st.suggestion.active {
            st.suggestion_prev();
        } else {
            st.editor.move_up_or_history();
        }
    } else if kb.matches(key, "tui.editor.cursorDown") {
        if st.suggestion.active {
            st.suggestion_next();
        } else {
            st.editor.move_down_or_history();
        }
    } else if kb.matches(key, "tui.editor.historyPrevious") {
        st.editor.history_prev();
    } else if kb.matches(key, "tui.editor.historyNext") {
        st.editor.history_next();
    } else if kb.matches(key, "tui.editor.cursorLeft") {
        st.editor.move_left();
    } else if kb.matches(key, "tui.editor.cursorRight") {
        st.editor.move_right();
    } else if kb.matches(key, "tui.editor.cursorWordLeft") {
        st.editor.move_word_left();
    } else if kb.matches(key, "tui.editor.cursorWordRight") {
        st.editor.move_word_right();
    } else if kb.matches(key, "tui.editor.cursorLineStart") {
        st.editor.home();
    } else if kb.matches(key, "tui.editor.cursorLineEnd") {
        st.editor.end();
    } else if kb.matches(key, "tui.editor.pageUp") {
        // 候选列表激活时列表翻页；否则编辑器内光标翻页
        if st.suggestion.active {
            for _ in 0..10 {
                st.suggestion_prev();
            }
        } else {
            st.editor.move_page_up();
        }
    } else if kb.matches(key, "tui.editor.pageDown") {
        if st.suggestion.active {
            for _ in 0..10 {
                st.suggestion_next();
            }
        } else {
            st.editor.move_page_down();
        }
    } else {
        return None;
    }
    Some(EditorAction::Continue)
}

/// 单字符 / 单词删除与扩展快捷键兜底。
fn handle_char_delete_key(st: &mut App, key: &KeyEvent) -> Option<EditorAction> {
    let kb = keybindings::get_global();
    if kb.matches(key, "tui.editor.deleteCharBackward") {
        st.editor.backspace();
    } else if kb.matches(key, "tui.editor.deleteCharForward") {
        st.editor.delete();
    } else if kb.matches(key, "tui.editor.deleteWordBackward") {
        let killed = st.editor.delete_word_backward();
        if !killed.is_empty() {
            // 对齐 pi：连续词 kill 累积 prepend/append；此处简化为每次 push
            st.kill_buffer = killed.clone();
            st.kill_ring.push(killed);
            st.yank_index = st.kill_ring.len();
            st.last_kill_was_word = true;
        }
    } else if kb.matches(key, "tui.editor.deleteWordForward") {
        let killed = st.editor.delete_word_forward();
        if !killed.is_empty() {
            st.kill_buffer = killed.clone();
            st.kill_ring.push(killed);
            st.yank_index = st.kill_ring.len();
            st.last_kill_was_word = true;
        }
    } else if dispatch_extension_keybinding(st, key) {
        // 扩展快捷键：内置键优先，走到这表示内置链未消费；
        // busy 态内部分发跳过（handlers 可能锁 agent rebuild_tools）
    } else {
        return None;
    }
    Some(EditorAction::Continue)
}

/// 当前模型的内联图片限制（目录 `inputLimits.images.resize`）。
fn current_image_resize_limits(st: &App) -> ImageResizeLimits {
    st.current_provider
        .as_deref()
        .zip(st.current_model.as_deref())
        .and_then(|(p, m)| model_resolver::find_model(p, m).ok())
        .map(|e| e.input_limits.images.resize)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::arc_with_non_send_sync)]
    use super::*;
    use crate::modes::interactive::agent_actor::WorkerHandle;
    use crate::modes::interactive::app::App;
    use crate::modes::interactive::line_input::shift_layer;
    use crate::modes::interactive::panel::{PanelItem, PanelKind};
    use crossterm::event::KeyEvent;

    /// auth.json read-modify-write 无锁：涉及 auth 的测试必须串行，
    /// 且与 settings_manager 测试共用同一把锁 + 同一目录（PRUX_AGENT_DIR 是进程级全局）
    /// 测试 agent 目录守卫：每测试独立临时目录（线程本地 override），
    /// 替代全局锁 + 进程级 env 劫持（并行互不干扰、无死锁）。
    fn test_agent_dir() -> crate::test_support::AgentDirGuard {
        crate::test_support::AgentDirGuard::temp()
    }

    /// 回归：Tab 补全（命令候选与路径补全）修改编辑器后必须置脏，否则主循环
    /// 不重绘，输入框画面残留旧内容（切窗口触发 resize 全量重绘才同步）。
    #[test]
    fn tab_completion_marks_input_dirty() {
        // 命令候选 Tab：/mod → /model
        let shared = Rc::new(RefCell::new(App::new()));
        for c in "/mod".chars() {
            handle_event(
                &shared,
                Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            );
        }
        shared.borrow_mut().dirty = false; // 模拟上一帧已渲染
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow();
            assert_eq!(st.editor.text(), "/model ", "命令补全生效");
            assert!(st.dirty, "Tab 命令补全后必须置脏");
        }

        // 路径补全 Tab：log → login.txt
        let shared = Rc::new(RefCell::new(App::new()));
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("login.txt"), "x").unwrap();
        shared.borrow_mut().cwd = dir.path().to_str().unwrap().to_string();
        for c in "log".chars() {
            handle_event(
                &shared,
                Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            );
        }
        shared.borrow_mut().dirty = false;
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow();
            assert_eq!(st.editor.text(), "login.txt ", "路径补全生效");
            assert!(st.dirty, "Tab 路径补全后必须置脏");
        }
    }

    /// 回归：Windows legacy console（crossterm winapi reader）对同一次按键会先后上报
    /// Press 与 Release，两者 KeyCode 相同——只过滤 Press 之外的 kind 才能避免「敲 a 出 aa」。
    #[test]
    fn release_key_events_are_ignored() {
        let shared = Rc::new(RefCell::new(App::new()));
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
        );
        handle_event(
            &shared,
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
        );
        assert_eq!(
            shared.borrow().editor.text(),
            "a",
            "Release 不应重复插入字符"
        );

        // 功能键同理：Release 不得二次触发（如 Backspace 连删两下）
        handle_event(
            &shared,
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::Backspace,
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
        );
        assert_eq!(
            shared.borrow().editor.text(),
            "a",
            "Release 的 Backspace 应被忽略"
        );
    }

    /// 回归：换行 / Ctrl+J 修改编辑器内容后也必须置脏（同 Tab 早退路径）。
    #[test]
    fn newline_marks_input_dirty() {
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.editor.insert_text("abc");
            st.dirty = false;
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
        );
        let st = shared.borrow();
        assert_eq!(st.editor.line_count(), 2, "Shift+Enter 换行");
        assert!(st.dirty, "换行后必须置脏");
    }

    /// 回归：输入 /（或 @）后删除触发符，候选面板必须隐藏。
    /// 删除键此前走 handle_editor_edit_key 早退路径不刷新候选，导致面板残留。
    #[test]
    fn deleting_trigger_char_hides_suggestion_panel() {
        // / 命令候选：输入 / 激活 → Backspace 删除 → 隐藏
        let shared = Rc::new(RefCell::new(App::new()));
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE)),
        );
        assert!(shared.borrow().suggestion.active, "输入 / 应激活命令候选");
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow();
            assert!(!st.suggestion.active, "删除 / 后面板应隐藏");
            assert_eq!(st.editor.text(), "");
        }

        // @ 文件候选：异步结果已应用后删除 @ → 隐藏
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.editor.insert_text("@");
            st.suggestion.active = true;
            st.suggestion.kind = SuggestionKind::Files;
            st.suggestion.start = 0;
            st.suggestion.query = String::new();
            st.suggestion.items = vec![crate::modes::interactive::app::SuggestionItem {
                name: "file.rs".to_string(),
                description: "file.rs".to_string(),
                insert: "file.rs".to_string(),
            }];
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow();
            assert!(!st.suggestion.active, "删除 @ 后面板应隐藏");
            assert_eq!(st.editor.text(), "");
        }
    }

    /// 带子命令的测试扩展：`/subcmd-enter pause|resume`
    struct SubcmdEnterExt;

    impl crate::core::extensions::Extension for SubcmdEnterExt {
        fn name(&self) -> &str {
            "subcmd-enter-ext"
        }
        fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
            Vec::new()
        }
        fn commands(&self) -> Vec<crate::core::extensions::ExtensionCommand> {
            vec![crate::core::extensions::ExtensionCommand {
                name: "subcmd-enter".to_string(),
                description: "demo".to_string(),
                busy_safe: true,
                subcommands: vec![
                    crate::core::extensions::SubcommandDef {
                        name: "pause",
                        description: "Pause it",
                    },
                    crate::core::extensions::SubcommandDef {
                        name: "resume",
                        description: "Resume it",
                    },
                ],
            }]
        }
        fn on_registered(&self) {
            register_slash_command("subcmd-enter-ext", "subcmd-enter", |st, raw| {
                st.push_msg(format!("EXEC {raw}"), MsgLevel::Info);
                false
            });
        }
    }

    /// 注册（仅一次）并启用测试扩展；调用方需持 AUTH_TEST_LOCK + AgentDirGuard
    fn ensure_subcmd_enter_ext() {
        static REG: std::sync::Once = std::sync::Once::new();
        REG.call_once(|| crate::core::extensions::register_extension(SubcmdEnterExt));
        crate::core::extensions::set_extension_enabled("subcmd-enter-ext", true);
    }

    fn type_text(shared: &Rc<RefCell<App>>, text: &str) {
        for c in text.chars() {
            handle_event(
                shared,
                Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)),
            );
        }
    }

    /// 子命令面板：Tab 只补全、Enter 补全并立即执行（与 / 命令面板同语义）
    #[test]
    fn subcommand_tab_completes_and_enter_executes() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        ensure_subcmd_enter_ext();

        // Tab：补全选中子命令，不执行
        let shared = Rc::new(RefCell::new(App::new()));
        type_text(&shared, "/subcmd-enter p");
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow();
            assert_eq!(st.editor.text(), "/subcmd-enter pause ", "Tab 只补全");
            assert!(st.system_messages.is_empty(), "Tab 不得执行命令");
        }

        // Enter：验完 Tab 后重新构造输入，回车应补全 + 提交 + 执行
        let shared = Rc::new(RefCell::new(App::new()));
        type_text(&shared, "/subcmd-enter p");
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow();
            assert_eq!(st.editor.text(), "", "Enter 提交后清空输入框");
            let last = crate::modes::interactive::app::sys_msg_text(
                &st.system_messages.last().expect("应执行子命令").1,
            );
            assert_eq!(last, "EXEC subcmd-enter pause", "Enter 应补全并执行首项");
        }

        // 空查询（`/subcmd-enter `）+ Enter：同样执行默认首项（对齐 / 面板）
        let shared = Rc::new(RefCell::new(App::new()));
        type_text(&shared, "/subcmd-enter ");
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow();
            let last = crate::modes::interactive::app::sys_msg_text(
                &st.system_messages.last().expect("空查询回车也应执行首项").1,
            );
            assert_eq!(last, "EXEC subcmd-enter pause");
        }
    }
    /// channels 版 worker：可断言 handler 发出的命令（分层测试 C 的 handler 层）
    fn test_agent_cmd() -> (
        WorkerHandle,
        crate::modes::interactive::agent_actor::CommandRx,
    ) {
        // env 类测试（worker 跨线程读 agent_dir）仍走进程级 pin；线程已有 guard 时 pin 自动 no-op
        crate::test_support::pin_test_agent_dir();
        let (cmd_tx, cmd_rx) = crate::modes::interactive::agent_actor::channels();
        (WorkerHandle::new(cmd_tx), cmd_rx)
    }

    #[test]
    fn panel_paste_goes_to_filter() {
        let mut st = App::new();
        st.panel.open(
            PanelKind::Model,
            "model".to_string(),
            vec![PanelItem {
                label: "model-a".to_string(),
                value: "m1".to_string(),
                desc: "[p]".to_string(),
                name: String::new(),
                ..Default::default()
            }],
        );
        let shared = Rc::new(RefCell::new(st));
        handle_event(&shared, Event::Paste("sk-abc".to_string()));
        let st = shared.borrow_mut();
        assert_eq!(
            st.panel.top().unwrap().filter.value,
            "sk-abc",
            "面板激活时 Paste 进面板过滤框"
        );
        drop(st);
        // 非面板/选择器状态：Paste 进主页编辑器
        let st = App::new();
        let shared = Rc::new(RefCell::new(st));
        handle_event(&shared, Event::Paste("ed-1".to_string()));
        let st = shared.borrow_mut();
        assert_eq!(st.editor.text(), "ed-1", "默认 Paste 进主页编辑器");
    }

    #[test]
    fn large_bracketed_paste_collapses_to_marker() {
        let st = App::new();
        let shared = Rc::new(RefCell::new(st));
        let big: String = (0..20)
            .map(|i| format!("line {}", i))
            .collect::<Vec<_>>()
            .join("\n");
        handle_event(&shared, Event::Paste(big));
        let mut st = shared.borrow_mut();
        assert_eq!(st.editor.line_count(), 1, "20 行粘贴折叠为单行 marker");
        assert_eq!(st.editor.text(), "[paste #1 +20 lines]");
        // 提交时展开为完整文本
        assert_eq!(st.editor.submit().unwrap().lines().count(), 20);
    }

    #[test]
    fn shift_layer_maps_us_symbols() {
        let pairs = [
            ('1', '!'),
            ('2', '@'),
            ('3', '#'),
            ('4', '$'),
            ('5', '%'),
            ('6', '^'),
            ('7', '&'),
            ('8', '*'),
            ('9', '('),
            ('0', ')'),
            ('`', '~'),
            ('-', '_'),
            ('=', '+'),
            ('[', '{'),
            (']', '}'),
            ('\\', '|'),
            (';', ':'),
            ('\'', '\"'),
            (',', '<'),
            ('.', '>'),
            ('/', '?'),
        ];
        for (raw, shifted) in pairs {
            assert_eq!(shift_layer(raw), shifted, "shift+{} 应为 {}", raw, shifted);
        }
    }

    #[test]
    fn shift_layer_passthrough_and_upper() {
        assert_eq!(shift_layer('a'), 'A');
        assert_eq!(shift_layer('A'), 'A');
        assert_eq!(shift_layer('@'), '@');
        assert_eq!(shift_layer('中'), '中');
    }

    #[test]
    fn ctrl_jk_navigate_suggestion_list_when_active() {
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.editor.insert_text("/o");
            st.refresh_suggestions();
            assert!(st.suggestion.active);
        }
        let n = shared.borrow_mut().suggestion.items.len();
        assert!(n >= 2);
        // Ctrl+J 下移选中项
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
        );
        {
            let st = shared.borrow_mut();
            assert_eq!(st.suggestion.selected, 1, "Ctrl+J 下移");
            assert!(st.suggestion.active, "内容未变列表仍激活");
        }
        // Ctrl+K 上移选中项
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
        );
        assert_eq!(shared.borrow_mut().suggestion.selected, 0, "Ctrl+K 上移");
        // 顶部 Ctrl+K 回绕到末尾
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
        );
        assert_eq!(
            shared.borrow_mut().suggestion.selected,
            n - 1,
            "顶部 Ctrl+K 回绕到末尾"
        );
        // 末尾 Ctrl+J 回绕到顶部
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
        );
        assert_eq!(
            shared.borrow_mut().suggestion.selected,
            0,
            "末尾 Ctrl+J 回绕到顶部"
        );
    }

    #[test]
    fn ctrl_jk_fall_back_when_no_suggestion_list() {
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.editor.insert_text("abcd");
        }
        // Ctrl+J 无候选列表时仍为换行
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL)),
        );
        assert_eq!(shared.borrow_mut().editor.line_count(), 2, "Ctrl+J 换行");
        // Ctrl+K 无候选列表时仍为删到行尾（回到第一行行首再 kill）
        {
            let mut st = shared.borrow_mut();
            st.editor.move_up();
            st.editor.home();
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL)),
        );
        {
            let st = shared.borrow_mut();
            assert_eq!(st.editor.lines[0], "", "Ctrl+K 删到行尾");
            assert_eq!(st.kill_buffer, "abcd", "删掉内容进 kill-ring");
        }
    }

    #[test]
    fn up_at_multiline_top_switches_history() {
        // 回归：多行草稿到达首行行首后 ↑ 才能切换历史；行中先归位到行首
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.editor.history.push("prev".to_string());
            st.editor.insert_text("a\nb");
        }
        let up = || Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        // ↑ → 首行行尾（不在行首）
        handle_event(&shared, up());
        {
            let st = shared.borrow();
            assert_eq!((st.editor.cursor_line, st.editor.cursor_col), (0, 1));
            assert_eq!(st.editor.text(), "a\nb", "尚未切换历史");
        }
        // ↑ → 归位行首，仍不切换
        handle_event(&shared, up());
        {
            let st = shared.borrow();
            assert_eq!((st.editor.cursor_line, st.editor.cursor_col), (0, 0));
            assert_eq!(st.editor.text(), "a\nb", "归位行首不切换历史");
        }
        // ↑ → 首行行首：切换上一条历史
        handle_event(&shared, up());
        assert_eq!(shared.borrow().editor.text(), "prev");
    }

    #[test]
    fn caps_lock_uppercases_main_editor_input() {
        use crossterm::event::{KeyEventKind, KeyEventState};
        let shared = Rc::new(RefCell::new(App::new()));
        // CapsLock（kitty 状态位）：字母上转大写
        handle_event(
            &shared,
            Event::Key(KeyEvent::new_with_kind_and_state(
                KeyCode::Char('a'),
                KeyModifiers::NONE,
                KeyEventKind::Press,
                KeyEventState::CAPS_LOCK,
            )),
        );
        // CapsLock+Shift：字母互抵保持小写
        handle_event(
            &shared,
            Event::Key(KeyEvent::new_with_kind_and_state(
                KeyCode::Char('b'),
                KeyModifiers::SHIFT,
                KeyEventKind::Press,
                KeyEventState::CAPS_LOCK,
            )),
        );
        assert_eq!(shared.borrow_mut().editor.text(), "Ab");
    }

    #[test]
    fn enter_resets_scroll_to_bottom_when_empty() {
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.scroll = 42; // 人为滚到上方
        }
        // 空内容按回车：不发送，但滚动条必须滚回底部
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        let st = shared.borrow_mut();
        assert_eq!(st.scroll, 0, "空回车也把滚动条滚回底部");
        assert!(st.status_start.is_none(), "空内容不提交消息");
    }

    #[test]
    fn enter_submit_resets_scroll_to_bottom() {
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.scroll = 42;
            st.editor.insert_text("hello");
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        let st = shared.borrow_mut();
        assert_eq!(st.scroll, 0, "发送消息后滚动条滚回底部");
        assert!(st.status_start.is_some(), "有内容应提交");
    }

    // ---- /history：实时历史搜索的面板交互 ----

    fn press(shared: &Rc<RefCell<App>>, code: KeyCode, mods: KeyModifiers) {
        handle_event(shared, Event::Key(KeyEvent::new(code, mods)));
    }

    fn history_app_with(entries: &[&str]) -> Rc<RefCell<App>> {
        let shared = Rc::new(RefCell::new(App::new()));
        shared.borrow_mut().editor.history = entries.iter().map(|s| s.to_string()).collect();
        shared
    }

    #[test]
    fn history_enter_sends_selected_entry_directly() {
        let shared = history_app_with(&["fix the bug"]);
        type_text(&shared, "/history");
        assert!(shared.borrow().suggestion.active, "打完 /history 即出面板");

        press(&shared, KeyCode::Enter, KeyModifiers::NONE);
        let st = shared.borrow();
        assert!(!st.suggestion.active, "发送后面板关闭");
        assert!(st.editor.text().is_empty(), "编辑器已清空");
        assert_eq!(
            st.status_start.as_deref(),
            Some("fix the bug"),
            "Enter 直接发送选中条目（不是重新打开搜索）"
        );
        assert_eq!(st.messages.len(), 1, "作为普通 prompt 发出一条用户消息");
    }

    #[test]
    fn history_tab_fills_input_without_sending() {
        let shared = history_app_with(&["fix the bug"]);
        type_text(&shared, "/history");

        press(&shared, KeyCode::Tab, KeyModifiers::NONE);
        let st = shared.borrow();
        assert!(!st.suggestion.active, "Tab 填入后面板关闭");
        assert_eq!(st.editor.text(), "fix the bug", "Tab 只填入输入框");
        assert!(st.status_start.is_none(), "Tab 不发送");
        assert!(st.messages.is_empty(), "Tab 不产生消息");
    }

    #[test]
    fn history_action_tab_fills_command_without_executing() {
        // `/history @` → Tab 应把控制词补全进输入框（`/history @clear`），
        // 不得弹确认面板、不得执行删除
        let _ad = test_agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.cwd = cwd.clone();
            st.editor.insert_text("remember me");
        }
        press(&shared, KeyCode::Enter, KeyModifiers::NONE); // 先落盘一条
        assert_eq!(crate::core::prompt_history::load(&cwd, 500).len(), 1);

        type_text(&shared, "/history @");
        assert_eq!(
            shared.borrow().suggestion.kind,
            SuggestionKind::HistoryAction,
            "`/history @` 应弹控制词候选"
        );

        press(&shared, KeyCode::Tab, KeyModifiers::NONE);
        {
            let st = shared.borrow();
            assert_eq!(
                st.editor.text(),
                "/history @clear",
                "Tab 应把选中控制词补进输入框"
            );
            // 补齐后刷新会重新命中 `@clear` 前缀：面板保持在控制词候选上（Enter 可继续执行）
            assert_eq!(st.suggestion.kind, SuggestionKind::HistoryAction);
            assert_eq!(st.suggestion.items[0].insert, "@clear");
            assert!(st.pending_history_clear.is_none(), "Tab 不得弹确认面板");
            assert_ne!(
                st.panel.top().map(|l| l.kind),
                Some(PanelKind::HistoryClearConfirm),
                "Tab 不得执行删除"
            );
        }
        assert_eq!(
            crate::core::prompt_history::load(&cwd, 500).len(),
            1,
            "Tab 之后文件必须还在"
        );
    }

    #[test]
    fn history_clear_confirm_ignores_typing() {
        // 确认面板无输入行：打字不得落进不可见 filter，把按钮过滤没（Yes/No 消失）
        let _ad = test_agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.cwd = cwd.clone();
            st.editor.insert_text("keep me");
        }
        press(&shared, KeyCode::Enter, KeyModifiers::NONE); // 先落盘一条

        type_text(&shared, "/history @clear");
        press(&shared, KeyCode::Enter, KeyModifiers::NONE); // 弹确认面板
        assert_eq!(
            shared.borrow().panel.top().map(|l| l.kind),
            Some(PanelKind::HistoryClearConfirm)
        );

        type_text(&shared, "xyz");
        {
            let st = shared.borrow();
            assert_eq!(
                st.panel.top().unwrap().filter.value,
                "",
                "确认面板不接受文本输入"
            );
            assert_eq!(st.panel.filtered_items().len(), 2, "Yes/No 应仍在");
        }

        // 括号粘贴同样不得落入不可见 filter
        handle_event(&shared, Event::Paste("pasted".to_string()));
        assert_eq!(shared.borrow().panel.top().unwrap().filter.value, "");

        // 导航键仍然可用
        press(&shared, KeyCode::Down, KeyModifiers::NONE);
        assert_eq!(shared.borrow().panel.top().unwrap().selected, 1);
    }

    #[test]
    fn history_zero_match_enter_is_swallowed() {
        let shared = history_app_with(&["aa"]);
        type_text(&shared, "/history zz");
        {
            let st = shared.borrow();
            assert!(st.suggestion.active, "零匹配也保持面板");
            assert!(st.suggestion.items.is_empty());
        }

        press(&shared, KeyCode::Enter, KeyModifiers::NONE);
        let st = shared.borrow();
        assert_eq!(
            st.editor.text(),
            "/history zz",
            "Enter 被吞掉：不清行、不发送"
        );
        assert!(st.status_start.is_none());
        assert!(st.messages.is_empty(), "绝不能落到 cmd_unknown 发给模型");
    }

    #[test]
    fn history_clear_confirm_yes_deletes_file_and_memory() {
        let _ad = test_agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.cwd = cwd.clone();
            st.editor.history = vec!["old prompt".to_string()];
            st.editor.insert_text("remember me");
        }
        press(&shared, KeyCode::Enter, KeyModifiers::NONE); // 先落盘一条
        assert_eq!(crate::core::prompt_history::load(&cwd, 500).len(), 1);

        type_text(&shared, "/history @clear");
        press(&shared, KeyCode::Enter, KeyModifiers::NONE); // 弹确认面板
        {
            let st = shared.borrow();
            assert_eq!(
                st.panel.top().map(|l| l.kind),
                Some(PanelKind::HistoryClearConfirm),
                "@clear 必须弹确认面板"
            );
            assert!(st.pending_history_clear.is_some());
            assert!(st.editor.text().is_empty(), "执行命令后命令行被清掉");
        }

        press(&shared, KeyCode::Enter, KeyModifiers::NONE); // Yes（默认选中第一项）
        let st = shared.borrow();
        assert!(st.pending_history_clear.is_none());
        assert!(st.editor.history.is_empty(), "内存列表同步清空");
        assert!(
            crate::core::prompt_history::load(&cwd, 500).is_empty(),
            "文件应已删除"
        );
    }

    #[test]
    fn history_clear_confirm_no_keeps_everything() {
        let _ad = test_agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.cwd = cwd.clone();
            st.editor.insert_text("keep me");
        }
        press(&shared, KeyCode::Enter, KeyModifiers::NONE);

        type_text(&shared, "/history @clear");
        press(&shared, KeyCode::Enter, KeyModifiers::NONE);
        press(&shared, KeyCode::Down, KeyModifiers::NONE); // 选到 No
        press(&shared, KeyCode::Enter, KeyModifiers::NONE);

        let st = shared.borrow();
        assert!(st.pending_history_clear.is_none());
        assert_eq!(st.editor.history.len(), 1, "取消不得动内存历史");
        assert_eq!(
            crate::core::prompt_history::load(&cwd, 500).len(),
            1,
            "取消不得删文件"
        );
    }

    #[test]
    fn history_clear_without_file_only_hints() {
        let _ad = test_agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let shared = Rc::new(RefCell::new(App::new()));
        shared.borrow_mut().cwd = dir.path().to_string_lossy().to_string();

        type_text(&shared, "/history @clear");
        press(&shared, KeyCode::Enter, KeyModifiers::NONE);

        let st = shared.borrow();
        assert!(st.pending_history_clear.is_none());
        assert_ne!(
            st.panel.top().map(|l| l.kind),
            Some(PanelKind::HistoryClearConfirm),
            "无文件可删时不弹空确认框"
        );
        assert!(
            st.system_messages
                .iter()
                .any(|(_, m)| crate::modes::interactive::app::sys_msg_text(m)
                    .contains("No prompt history")),
            "应给一句友好提示"
        );
    }

    #[test]
    fn history_clear_confirm_escape_cancels_and_keeps_file() {
        // 破坏性操作的兵家必争：Esc 必须能退出且什么都不删
        let _ad = test_agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.cwd = cwd.clone();
            st.editor.insert_text("keep me");
        }
        press(&shared, KeyCode::Enter, KeyModifiers::NONE);

        type_text(&shared, "/history @clear-all");
        press(&shared, KeyCode::Enter, KeyModifiers::NONE);
        assert!(shared.borrow().pending_history_clear.is_some());

        press(&shared, KeyCode::Esc, KeyModifiers::NONE);
        let st = shared.borrow();
        assert!(st.pending_history_clear.is_none(), "Esc 必须清掉待确认状态");
        assert!(!st.panel.active, "Esc 关闭面板");
        assert_eq!(st.editor.history.len(), 1, "内存历史保留");
        assert_eq!(
            crate::core::prompt_history::load(&cwd, 500).len(),
            1,
            "Esc 之后文件必须还在"
        );
    }

    #[test]
    fn history_esc_then_enter_reopens_panel() {
        let shared = history_app_with(&["aa"]);
        type_text(&shared, "/history");
        assert!(shared.borrow().suggestion.active);

        // Esc 关面板（现有未绑定键行为），命令原文留在输入框
        press(&shared, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!shared.borrow().suggestion.active, "Esc 关闭面板");
        assert_eq!(shared.borrow().editor.text(), "/history");

        // 再按 Enter：cmd_history 兜底把命令写回编辑器 → 刷新后重新接管面板
        // （`/history` 本身也进历史——任何非空输入都记——所以条目数是 2：`aa` + `/history`）
        press(&shared, KeyCode::Enter, KeyModifiers::NONE);
        let st = shared.borrow();
        assert_eq!(st.suggestion.kind, SuggestionKind::History);
        assert_eq!(st.suggestion.items.len(), 2, "重新打开搜索");
        assert!(st.messages.is_empty(), "兜底路径不发送任何内容");
    }

    #[test]
    fn submit_records_prompt_to_prompt_history_file() {
        // 真正的落盘路径（其它交互测试没有 AgentDirGuard，落盘自动禁用）
        let _ad = test_agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let shared = Rc::new(RefCell::new(App::new()));
        let cwd = dir.path().to_string_lossy().to_string();
        {
            let mut st = shared.borrow_mut();
            st.cwd = cwd.clone();
            st.history_max_entries = 500;
            st.editor.insert_text("remember me");
        }
        press(&shared, KeyCode::Enter, KeyModifiers::NONE);

        let loaded = crate::core::prompt_history::load(&cwd, 500);
        assert_eq!(loaded.len(), 1, "提交的 prompt 应落盘: {loaded:?}");
        assert_eq!(loaded[0].text, "remember me");
    }

    #[test]
    fn shift_home_scrolls_message_area_to_top() {
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.max_scroll = 24;
            st.scroll = 3;
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Home, KeyModifiers::SHIFT)),
        );
        let st = shared.borrow_mut();
        assert_eq!(st.scroll, 24, "shift+home 滚动到顶部");
    }

    #[test]
    fn shift_end_scrolls_message_area_to_bottom() {
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.max_scroll = 24;
            st.scroll = 24;
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::End, KeyModifiers::SHIFT)),
        );
        let st = shared.borrow_mut();
        assert_eq!(st.scroll, 0, "shift+end 滚动到底部（跟随最新）");
    }

    #[test]
    fn shift_page_up_scrolls_message_area_by_viewport() {
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.max_scroll = 100;
            st.scroll = 5;
            st.mouse.log_area_h = 20;
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT)),
        );
        let st = shared.borrow_mut();
        assert_eq!(st.scroll, 25, "shift+pageup 向上翻一页（视口高度）");
    }

    #[test]
    fn shift_page_down_scrolls_message_area_by_viewport() {
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.max_scroll = 100;
            st.scroll = 25;
            st.mouse.log_area_h = 20;
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::SHIFT)),
        );
        let st = shared.borrow_mut();
        assert_eq!(st.scroll, 5, "shift+pagedown 向下翻一页（视口高度）");
    }

    #[test]
    fn page_scroll_clamps_to_bounds() {
        // 顶部：shift+pageup 不超过 max_scroll
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.max_scroll = 24;
            st.scroll = 20;
            st.mouse.log_area_h = 20;
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::SHIFT)),
        );
        assert_eq!(shared.borrow().scroll, 24, "翻页在顶部封顶");

        // 底部：shift+pagedown 不低于 0
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.max_scroll = 24;
            st.scroll = 3;
            st.mouse.log_area_h = 20;
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::SHIFT)),
        );
        assert_eq!(shared.borrow().scroll, 0, "翻页在底部不越界");
    }

    #[test]
    fn alt_up_routes_to_dequeue() {
        use crossterm::event::KeyEvent;
        let _g = test_agent_dir();
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let st = shared.borrow_mut();
            st.runtime_steer_inbox
                .lock()
                .unwrap()
                .push_back(crate::core::provider::AgentMessage::user_text("recover me"));
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)),
        );
        let st = shared.borrow_mut();
        assert!(st.runtime_followup_inbox.is_empty(), "Alt+Up 清空队列");
        assert_eq!(st.editor.text(), "recover me", "取回编辑器");
    }

    /// 扩展输入拦截：认领的提交不启动回合 / 不排队；改写文本照常走原提交路径。
    #[test]
    fn extension_user_prompt_claim_and_rewrite() {
        let _g = test_agent_dir();
        let _reg = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        use crate::core::extensions::{
            Extension, ExtensionHook, ExtensionTool, UserPromptAction, register_extension,
            set_extension_enabled, unregister_extension,
        };
        struct MentionStub;
        impl Extension for MentionStub {
            fn name(&self) -> &str {
                "mention-stub"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn hooks(&self) -> Vec<ExtensionHook> {
                vec![ExtensionHook::UserPrompt]
            }
            fn on_user_prompt(&self, text: &str) -> Option<UserPromptAction> {
                if text.starts_with("@claim") {
                    Some(UserPromptAction::Handled)
                } else {
                    text.strip_prefix("@main ")
                        .map(|rest| UserPromptAction::Rewrite(rest.to_string()))
                }
            }
        }
        register_extension(MentionStub);
        set_extension_enabled("mention-stub", true);
        // `dispatch_user_prompt` 会遍历全局注册表（可能命中 subagent 扩展），
        // 必须与触碰 manager 全局态的 subagent 测试串行。
        let _sub = crate::extensions::subagent::test_lock();
        let shared = Rc::new(RefCell::new(App::new()));

        // 空闲 + 认领：不启动回合、编辑器清空
        shared.borrow_mut().editor.insert_text("@claim do it");
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow();
            assert!(st.messages.is_empty(), "认领的提交不得启动回合");
            assert_eq!(st.editor.text(), "");
        }

        // 忙碌 + 认领：不入主 steer 队列
        {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.editor.insert_text("@claim again");
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow();
            assert!(
                st.runtime_steer_inbox.lock().unwrap().is_empty(),
                "认领的 mention 不得入主 steer 队列"
            );
            assert_eq!(st.editor.text(), "");
        }

        // 忙碌 + 改写：排队改写后的文本
        shared.borrow_mut().editor.insert_text("@main hello there");
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow();
            let inbox = st.runtime_steer_inbox.lock().unwrap();
            assert_eq!(inbox.len(), 1);
            assert_eq!(inbox[0].text(), "hello there", "忙碌时应排队改写后的文本");
        }
        unregister_extension("mention-stub");
    }

    #[test]
    fn shift_tab_cycles_thinking_level_and_refreshes_status_text() {
        // 对齐 pi app.thinking.cycle：非 busy 时连续 Shift+Tab 应循环 thinking 级别，
        // 且每次按下 TUI 提示都更新（回归：渲染缓存不复位时提示停在第一次的内容）。
        let _g = test_agent_dir();
        let (agent, mut cmd_rx) = test_agent_cmd();
        let shared = Rc::new(RefCell::new(App::new()));
        shared.borrow_mut().worker = agent;
        shared.borrow_mut().model_reasoning = true;
        shared.borrow_mut().thinking_levels = vec!["off".to_string(), "high".to_string()];
        // 非 busy 时连续 Shift+Tab：每次应下发 SetThinking 命令（worker 回执推进镜像/提示）
        for _ in 0..3 {
            handle_event(
                &shared,
                Event::Key(KeyEvent::new(KeyCode::BackTab, KeyModifiers::NONE)),
            );
            let cmd = cmd_rx.try_recv().expect("应发 SetThinking");
            assert!(
                matches!(
                    cmd,
                    crate::modes::interactive::agent_actor::AgentCommand::SetThinking { .. }
                ),
                "应发 SetThinking: {cmd:?}"
            );
        }
        // 镜像推进由回执完成（场景测试覆盖）；handler 层保证命令连续下发不 panic
        let st = shared.borrow_mut();
        assert!(st.model_reasoning);
    }

    #[test]
    fn alt_p_toggles_plan_mode() {
        // 对齐 pi registerShortcut(Key.alt("p"))：Alt+P 切换 plan mode
        // （busy 时该键被拦截，此处验证空闲态切换与通知）
        let _g = test_agent_dir();
        let _reg = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _plan = crate::extensions::plan_mode::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::extensions::plan_mode::ensure_registered();
        crate::core::extensions::set_extension_enabled("plan-mode", true);
        crate::extensions::plan_mode::set_enabled(false);
        let dir = crate::core::settings_manager::agent_dir();
        let _ = std::fs::remove_file(dir.join("plan-mode.json"));
        let shared = Rc::new(RefCell::new(App::new()));
        let key = |mods: KeyModifiers| Event::Key(KeyEvent::new(KeyCode::Char('p'), mods));

        handle_event(&shared, key(KeyModifiers::ALT));
        assert!(
            crate::extensions::plan_mode::is_enabled(),
            "Alt+P 开启 plan mode"
        );
        let last = shared
            .borrow()
            .system_messages
            .last()
            .map(|(_, s)| crate::modes::interactive::app::sys_spans_text(&s.spans));
        assert!(
            last.as_deref().unwrap_or("").contains("Plan mode enabled"),
            "got: {last:?}"
        );

        handle_event(&shared, key(KeyModifiers::ALT));
        assert!(!crate::extensions::plan_mode::is_enabled(), "再次按下关闭");

        // busy 时拦截（避免锁 agent rebuild）：不切换
        crate::extensions::plan_mode::set_enabled(true);
        let mut st = shared.borrow_mut();
        st.busy = true;
        drop(st);
        handle_event(&shared, key(KeyModifiers::ALT));
        assert!(
            crate::extensions::plan_mode::is_enabled(),
            "busy 时不应切换"
        );
        shared.borrow_mut().busy = false;
        crate::extensions::plan_mode::set_enabled(false);
        let _ = std::fs::remove_file(dir.join("plan-mode.json"));
        // 清全局 UI 队列（persist() 入队的 PersistSessionEntry 等），避免泄漏到并行测试
        while crate::core::extensions::take_pending_ui().is_some() {}
    }

    #[test]
    fn alt_p_noop_when_extension_disabled() {
        // 扩展禁用守卫：plan-mode 禁用后 Alt+P 不切换、不弹误导提示
        // （命令侧已由动态分发拦截，快捷键走 toggle_ui 兜底）
        let _g = test_agent_dir();
        let _reg = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _plan = crate::extensions::plan_mode::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::extensions::plan_mode::ensure_registered();
        crate::extensions::plan_mode::set_enabled(false);
        crate::core::extensions::set_extension_enabled("plan-mode", false);
        let dir = crate::core::settings_manager::agent_dir();
        let _ = std::fs::remove_file(dir.join("plan-mode.json"));
        let shared = Rc::new(RefCell::new(App::new()));
        let key = |mods: KeyModifiers| Event::Key(KeyEvent::new(KeyCode::Char('p'), mods));

        handle_event(&shared, key(KeyModifiers::ALT));
        assert!(!crate::extensions::plan_mode::is_enabled(), "禁用后不切换");
        let msgs = shared.borrow_mut();
        let has_plan_msg = msgs.system_messages.iter().any(|(_, s)| {
            crate::modes::interactive::app::sys_spans_text(&s.spans).contains("Plan mode")
        });
        assert!(!has_plan_msg, "禁用后不弹 plan 提示");
        drop(msgs);

        crate::core::extensions::set_extension_enabled("plan-mode", true);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        // 清全局 UI 队列，避免泄漏到并行测试
        while crate::core::extensions::take_pending_ui().is_some() {}
    }

    #[test]
    fn busy_submit_dispatches_busy_safe_todos_cmd() {
        // 回归：忙碌时回车，声明了 busy_safe 的扩展命令（plan-mode /todos，
        // handler 不锁 agent）必须立即分发显示，而非被误当普通消息入 steer 队列；
        // 非 busy_safe 命令（/plan 锁 agent）保持原行为仍排队。
        // 另：无 todo 任务时 /todos 不再注册为命令（busy_safe=false，
        // 忙碌时按普通文本入 steer 队列），避免空计划时误操作。
        let _g = test_agent_dir();
        let _reg = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _plan = crate::extensions::plan_mode::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::extensions::plan_mode::ensure_registered();
        crate::core::extensions::set_extension_enabled("plan-mode", true);
        crate::extensions::plan_mode::reset_state_for_tests();
        crate::extensions::plan_mode::set_enabled(true);
        let dir = crate::core::settings_manager::agent_dir();
        let _ = std::fs::remove_file(dir.join("plan-mode.json"));
        let shared = Rc::new(RefCell::new(App::new()));

        // 无 todos：/todos 未注册 → busy_safe 标记为 false（plan=false 未知=false）
        assert!(!crate::core::extensions::command_busy_safe("todos"));
        assert!(!crate::core::extensions::command_busy_safe("plan"));
        assert!(!crate::core::extensions::command_busy_safe("nonexistent"));

        // busy + 空计划 /todos：非 busy_safe → 按普通文本入 steer 队列，不执行
        {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.editor.insert_text("/todos");
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow_mut();
            assert!(
                st.runtime_followup_inbox.is_empty(),
                "无 todos 时 /todos 不应进 followup 队列"
            );
            assert_eq!(st.runtime_steer_inbox.lock().unwrap().len(), 1);
            assert_eq!(st.runtime_steer_inbox.lock().unwrap()[0].text(), "/todos");
            assert!(!st.dock_visible, "空计划 /todos 不应打开面板");
        }
        // 清空 phase1 的 steer，便于 phase2 断言队列为空
        shared
            .borrow_mut()
            .runtime_steer_inbox
            .lock()
            .unwrap()
            .clear();

        // 注入 todos 后 /todos 重新注册 → busy_safe=true；busy + /todos 立即分发
        {
            let mut st = shared.borrow_mut();
            use crate::core::extensions::Extension;
            let ext = crate::extensions::plan_mode::PlanMode::new();
            ext.on_agent_event(&serde_json::json!({
                "type": "agent_end",
                "messages": [
                    { "role": "assistant", "content": [{ "type": "text", "text": "Plan:\n1. Inspect code\n2. Verify result" }] }
                ]
            }));
            assert!(
                crate::core::extensions::command_busy_safe("todos"),
                "有 todos 时 /todos 应恢复 busy_safe"
            );
            st.busy = true;
            st.editor.insert_text("/todos");
        }
        let _ = crate::core::extensions::take_pending_ui();
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow_mut();
            assert!(st.dock_visible, "busy 时 /todos 应打开面板");
            assert!(
                st.runtime_followup_inbox.is_empty(),
                "busy 时 /todos 不应入 followup 队列"
            );
            assert!(
                st.runtime_steer_inbox.lock().unwrap().is_empty(),
                "busy 时 /todos 不应入 steer 队列"
            );
            assert_eq!(st.editor.text(), "", "命令消费后编辑器应清空");
        }

        // busy + /plan（busy_safe=false）：保持原行为入 steer 队列
        {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.editor.insert_text("/plan");
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow_mut();
            assert_eq!(
                st.runtime_followup_inbox.len(),
                0,
                "/plan 非 busy_safe 应进注入箱而非本地队列"
            );
            assert_eq!(st.runtime_steer_inbox.lock().unwrap().len(), 1);
            assert_eq!(st.runtime_steer_inbox.lock().unwrap()[0].text(), "/plan");
        }

        shared.borrow_mut().busy = false;
        crate::extensions::plan_mode::set_enabled(false);
        let _ = std::fs::remove_file(dir.join("plan-mode.json"));
        // 清全局 UI 队列，避免泄漏到并行测试
        while crate::core::extensions::take_pending_ui().is_some() {}
    }

    #[test]
    fn busy_submit_dispatches_busy_safe_dock_cmd() {
        // 回归：忙碌时回车，/dock（纯 UI 切换，handler 不锁 agent）必须立即执行，
        // 而非被误当普通消息入 steer 队列（否则 `/dock` 文本会被当用户消息发给模型）。
        let _g = test_agent_dir();
        // plan mode 内存态是进程级单例：与 plan_mode.rs / commands.rs 测试串行
        // （不共享任何文件，只串行内存态）
        let _reg = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _plan = crate::extensions::plan_mode::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::extensions::plan_mode::ensure_registered();
        crate::core::extensions::set_extension_enabled("plan-mode", true);
        // plan 全局状态与其它测试共享：先复位避免遗留 todos 污染本测试，
        // 测试结束同样复位（否则后续 /todos 测试会看到本次注入的步骤）
        crate::extensions::plan_mode::reset_state_for_tests();
        crate::extensions::plan_mode::set_enabled(true);
        let dir = crate::core::settings_manager::agent_dir();
        let _ = std::fs::remove_file(dir.join("plan-mode.json"));
        let shared = Rc::new(RefCell::new(App::new()));

        // 内置 busy_safe 标记：dock=true（不依赖任何扩展声明），未知命令仍 false
        assert!(is_busy_safe_command("dock"));
        assert!(!is_busy_safe_command("go to the moon"));

        // 无 dock 内容：busy + /dock 仍立即执行（提示无内容），不入队列
        {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.editor.insert_text("/dock");
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow_mut();
            assert!(
                st.runtime_followup_inbox.is_empty(),
                "busy 时 /dock 不应入 followup 队列"
            );
            assert!(
                st.runtime_steer_inbox.lock().unwrap().is_empty(),
                "busy 时 /dock 不应入 steer 队列"
            );
            assert_eq!(st.editor.text(), "", "命令消费后编辑器应清空");
            assert!(!st.dock_visible, "无内容不应打开面板");
            let has_hint = st.system_messages.iter().any(|(_, s)| {
                crate::modes::interactive::app::sys_spans_text(&s.spans).contains("No dock content")
            });
            assert!(has_hint, "busy 时 /dock 应显示无内容提示");
        }

        // 有 dock 内容（plan 注入步骤并 /todos 显示段）：busy + /dock 打开面板
        {
            let mut st = shared.borrow_mut();
            use crate::core::extensions::Extension;
            let ext = crate::extensions::plan_mode::PlanMode::new();
            ext.on_agent_event(&serde_json::json!({
                "type": "agent_end",
                "messages": [
                    { "role": "assistant", "content": [{ "type": "text", "text": "Plan:\n1. Inspect code\n2. Verify result" }] }
                ]
            }));
            // /todos 显示 plan 停靠段并打开面板
            _ = st.handle_slash_command("todos");
            assert!(st.dock_visible, "/todos 应打开面板");
            // 空闲态 /dock 关闭面板（内容仍在，仅切开关）
            _ = st.handle_slash_command("dock");
            assert!(!st.dock_visible);
            st.busy = true;
            st.editor.insert_text("/dock");
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow_mut();
            assert!(st.dock_visible, "busy 时 /dock 应打开面板");
            assert!(
                st.runtime_steer_inbox.lock().unwrap().is_empty(),
                "busy 时 /dock 不应入 steer 队列"
            );
        }

        // busy 时再次 /dock：关闭面板
        {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.editor.insert_text("/dock");
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow_mut();
            assert!(!st.dock_visible, "busy 时 /dock 应关闭面板");
            assert!(
                st.runtime_steer_inbox.lock().unwrap().is_empty(),
                "busy 时 /dock 不应入 steer 队列"
            );
        }

        shared.borrow_mut().busy = false;
        crate::extensions::plan_mode::reset_state_for_tests();
        crate::extensions::plan_mode::set_enabled(false);
        let _ = std::fs::remove_file(dir.join("plan-mode.json"));
        // 清全局 UI 队列，避免泄漏到并行测试
        while crate::core::extensions::take_pending_ui().is_some() {}
    }

    #[test]
    fn busy_submit_applies_active_suggestion_before_busy_safe_check() {
        // 回归：busy 时输入 `/comp` 弹出命令候选（默认选中 `/compact`），按 Enter
        // 应把**面板中选中的命令**入 steer 队列，而不是未完成的前缀 `/comp`；
        // busy_safe 命令（`/doc` → `/dock`）则应立即执行，同样不入队列。
        let _g = test_agent_dir();
        let shared = Rc::new(RefCell::new(App::new()));

        // 非 busy_safe：`/comp` → 选中 compact → Enter 入 steer 的是 `/compact`
        {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.editor.insert_text("/comp");
            st.refresh_suggestions();
            assert!(st.suggestion.active, "`/comp` 应弹出命令候选");
            assert_eq!(st.suggestion.kind, SuggestionKind::Commands);
            assert_eq!(
                st.suggestion.items[st.suggestion.selected].name, "compact",
                "默认选中项应为 compact"
            );
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow_mut();
            assert_eq!(
                st.runtime_steer_inbox.lock().unwrap().len(),
                1,
                "非 busy_safe 命令应入 steer 队列"
            );
            let text = st.runtime_steer_inbox.lock().unwrap()[0].text();
            assert_eq!(
                text.trim(),
                "/compact",
                "入队文本应为面板选中的完整命令，而非前缀 /comp: {text:?}"
            );
            assert_eq!(st.editor.text(), "", "提交后编辑器应清空");
        }

        // busy_safe：`/doc` → 选中 dock → Enter 立即执行，不入队列
        shared
            .borrow_mut()
            .runtime_steer_inbox
            .lock()
            .unwrap()
            .clear();
        {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.editor.insert_text("/doc");
            st.refresh_suggestions();
            assert!(st.suggestion.active, "`/doc` 应弹出命令候选");
            assert_eq!(
                st.suggestion.items[st.suggestion.selected].name, "dock",
                "默认选中项应为 dock"
            );
        }
        handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        {
            let st = shared.borrow_mut();
            assert!(
                st.runtime_steer_inbox.lock().unwrap().is_empty(),
                "busy_safe 命令应立即执行，不入 steer 队列"
            );
            assert_eq!(st.editor.text(), "", "命令消费后编辑器应清空");
        }

        shared.borrow_mut().busy = false;
    }

    #[test]
    fn busy_ctrl_c_clears_input_instead_of_interrupting() {
        // busy 态输入框有内容时按 Ctrl+C：应清空输入（与空闲态 app.clear 一致），
        // 保持 busy 不中断；输入框为空时 Ctrl+C 才中断当前任务。
        let _g = test_agent_dir();
        let shared = Rc::new(RefCell::new(App::new()));
        let ctrl_c = |shared: &Rc<RefCell<App>>| {
            handle_event(
                shared,
                Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            )
        };

        // 有内容：清空输入，busy 保持，不中断
        {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.editor.insert_text("follow-up to send");
        }
        assert!(
            matches!(ctrl_c(&shared), KeyAction::Continue),
            "有内容时 Ctrl+C 只清空输入，不中断"
        );
        {
            let st = shared.borrow();
            assert_eq!(st.editor.text(), "", "输入框应被清空");
            assert!(st.busy, "busy 状态不应被终止");
            assert!(!st.suggestion.active, "候选面板应随清空关闭");
            assert!(st.dirty, "清空后必须置脏");
        }

        // 再次按 Ctrl+C（输入框已空）：返回中断动作（busy 复位由事件循环 process_key_action 处理）
        assert!(
            matches!(ctrl_c(&shared), KeyAction::CancelPrompt),
            "空输入框 Ctrl+C 中断 busy"
        );

        // 忙碌时输入 / 激活命令候选，Ctrl+C 清空并关闭候选
        {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.editor.insert_text("/mod");
            st.suggestion.active = true;
        }
        ctrl_c(&shared);
        {
            let st = shared.borrow();
            assert_eq!(st.editor.text(), "");
            assert!(!st.suggestion.active, "Ctrl+C 清空后候选应关闭");
            assert!(st.busy);
        }
        shared.borrow_mut().busy = false;
    }

    #[test]
    fn busy_submit_expands_skill_command() {
        // 回归：忙碌时提交 `/skill:name` 必须按 skill 命令处理（展开为 skill 块）
        // 再排进 steer 队列，不能原样当普通文本发给模型。
        let _g = test_agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let skill_md = dir.path().join("SKILL.md");
        std::fs::write(&skill_md, "---\nname: demo\ndescription: d\n---\n# Body\n").unwrap();

        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.skills = vec![crate::core::skills::Skill {
                name: "demo".to_string(),
                description: "d".to_string(),
                path: skill_md,
                base_dir: dir.path().to_path_buf(),
                disable_model_invocation: false,
                source: "test".to_string(),
            }];
            st.editor.insert_text("/skill:demo do it");
        }
        press(&shared, KeyCode::Enter, KeyModifiers::NONE);
        {
            let st = shared.borrow();
            let inbox = st.runtime_steer_inbox.lock().unwrap();
            assert_eq!(inbox.len(), 1);
            let text = inbox[0].text();
            assert!(text.contains("<skill name=\"demo\""), "got: {text}");
            assert!(text.contains("# Body"), "got: {text}");
            assert!(text.ends_with("do it"), "got: {text}");
        }
        shared.borrow_mut().busy = false;
    }

    #[test]
    fn esc_during_retry_requests_cancel() {
        // 回归：重试窗口内 busy 保持 true（由派发置位、直到 agent_settled 才复位），
        // Esc 必须路由到中断（CancelPrompt）——否则用户看到 “escape to cancel” 却按不动。
        let _g = test_agent_dir();
        let shared = Rc::new(RefCell::new(App::new()));
        {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.retry_ui = Some(RetryUi {
                attempt: 1,
                max: 3,
                deadline: std::time::Instant::now() + std::time::Duration::from_secs(5),
                last_refresh: std::time::Instant::now(),
                ts: 1,
            });
        }
        let action = handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(
            matches!(action, KeyAction::CancelPrompt),
            "重试期间 Esc 应返回 CancelPrompt"
        );
        shared.borrow_mut().busy = false;
    }

    /// 空闲时 Esc：先取消可取消的后台工作（如 MCP OAuth 登录），不再放行给编辑器。
    /// pi #10565：登录在每一步都能取消。
    #[test]
    fn idle_escape_cancels_background_work() {
        // 登记表是进程全局的：与其它碰它的用例串行
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = test_agent_dir();
        let shared = Rc::new(RefCell::new(App::new()));
        let token = tokio_util::sync::CancellationToken::new();
        let guard = core::extensions::register_background_cancel("test-login", token.clone());

        let action = handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(matches!(action, KeyAction::Continue));
        assert!(token.is_cancelled(), "空闲时 Esc 应取消后台工作");

        // 工作结束后（guard drop）Esc 不再被后台工作拦截
        drop(guard);
        let action = handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(matches!(action, KeyAction::Continue));
    }

    /// 回归：busy 时经 busy_safe 命令（如 /goal）弹出的扩展面板，键盘必须先给面板：
    /// Enter 确认选中项、Esc/Ctrl+C 关闭面板，不能被 handle_busy_key 当成
    /// steer / 中断吞掉（否则对话框无法确认、也关不掉）。
    #[test]
    fn busy_panel_takes_keys_before_busy_shortcuts() {
        let _ad = test_agent_dir();
        let shared = Rc::new(RefCell::new(App::new()));

        let open_replace_panel = |id: u64| {
            let mut st = shared.borrow_mut();
            st.busy = true;
            st.panel.open(
                PanelKind::Custom(id),
                "Replace goal?".to_string(),
                vec![PanelItem::new("Replace".to_string(), "Replace".to_string())],
            );
        };

        // Enter：确认选中项 → 面板关闭；不得落入 steer 队列，也不得复位 busy
        open_replace_panel(7);
        let action = handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        assert!(matches!(action, KeyAction::Continue));
        {
            let st = shared.borrow();
            assert!(!st.panel.active, "busy 态 Enter 应确认并关闭面板");
            assert!(st.busy, "确认面板不应复位 busy");
            assert_eq!(st.editor.text(), "", "不应把面板确认当成 steer 输入");
        }

        // Esc：先关面板，返回 Continue（而非 CancelPrompt 中断当前回合）
        open_replace_panel(8);
        let action = handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
        );
        assert!(
            matches!(action, KeyAction::Continue),
            "busy 态面板 Esc 应关面板而非中断"
        );
        assert!(!shared.borrow().panel.active, "Esc 应关闭面板");

        // Ctrl+C：同样先关面板
        open_replace_panel(9);
        let action = handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
        );
        assert!(
            matches!(action, KeyAction::Continue),
            "busy 态面板 Ctrl+C 应关面板而非中断"
        );
        assert!(!shared.borrow().panel.active, "Ctrl+C 应关闭面板");

        shared.borrow_mut().busy = false;
    }

    /// 从 `/agents` 一级菜单选择会打开覆盖层的条目（workflows / create / edit）后，
    /// 一级 Select 面板必须关闭：否则菜单与覆盖层会同屏。
    struct MenuOverlayExt;

    impl crate::core::extensions::Extension for MenuOverlayExt {
        fn name(&self) -> &str {
            "menu-overlay-test-ext"
        }
        fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
            Vec::new()
        }
        fn hooks(&self) -> Vec<crate::core::extensions::ExtensionHook> {
            vec![crate::core::extensions::ExtensionHook::Overlay]
        }
        fn overlay_view(
            &self,
            id: u64,
            _cols: u16,
        ) -> Option<crate::core::extensions::OverlayView> {
            (id == 4242)
                .then(|| crate::core::extensions::OverlayView::inline("Workflows", Vec::new(), 6))
        }
        fn on_overlay_event(&self, _id: u64, _ev: &crate::core::extensions::OverlayEvent) -> bool {
            true
        }
        fn on_ui_choice(&self, id: u64, choice: Option<String>) -> Option<String> {
            if id == 4241 && choice.is_some() {
                crate::core::extensions::request_ui(
                    crate::core::extensions::ExtensionUiRequest::ShowOverlay { id: 4242 },
                );
            }
            None
        }
    }

    #[test]
    fn overlay_opened_from_menu_choice_closes_the_menu_panel() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while crate::core::extensions::take_pending_ui().is_some() {}
        crate::core::extensions::register_extension(MenuOverlayExt);

        let shared = Rc::new(RefCell::new(App::new()));
        crate::core::extensions::request_ui(crate::core::extensions::ExtensionUiRequest::Select {
            id: 4241,
            title: "Sub-agents".to_string(),
            options: vec![crate::core::extensions::SelectOption::new(
                "workflows  Workflows (1 runs)",
            )],
        });
        crate::modes::interactive::maybe_handle_extension_ui(&shared);
        assert!(shared.borrow().panel.active, "一级菜单应打开");

        // Enter 选中该项：扩展请求 ShowOverlay
        _ = handle_event(
            &shared,
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
        );
        crate::modes::interactive::maybe_handle_extension_ui(&shared);

        assert!(
            !shared.borrow().panel.active,
            "打开覆盖层后一级菜单必须关闭"
        );
        assert!(shared.borrow().overlay.is_some(), "覆盖层应打开");

        crate::core::extensions::unregister_extension("menu-overlay-test-ext");
    }

    /// 互斥不变式：任何 ShowOverlay 都必须先收起模态面板（不论面板是如何打开的）。
    #[test]
    fn show_overlay_closes_any_active_panel() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while crate::core::extensions::take_pending_ui().is_some() {}
        crate::core::extensions::register_extension(MenuOverlayExt);

        let shared = Rc::new(RefCell::new(App::new()));
        crate::core::extensions::request_ui(crate::core::extensions::ExtensionUiRequest::Select {
            id: 4241,
            title: "Sub-agents".to_string(),
            options: vec![crate::core::extensions::SelectOption::new(
                "view  Fleet view",
            )],
        });
        crate::modes::interactive::maybe_handle_extension_ui(&shared);
        assert!(shared.borrow().panel.active, "面板应打开");

        // 面板仍开着时直接来一个 ShowOverlay（模拟扩展在面板未确认时开覆盖层）
        crate::core::extensions::request_ui(
            crate::core::extensions::ExtensionUiRequest::ShowOverlay { id: 4242 },
        );
        crate::modes::interactive::maybe_handle_extension_ui(&shared);

        assert!(!shared.borrow().panel.active, "覆盖层打开时面板必须已收起");
        assert!(shared.borrow().overlay.is_some(), "覆盖层应打开");

        crate::core::extensions::unregister_extension("menu-overlay-test-ext");
    }
}

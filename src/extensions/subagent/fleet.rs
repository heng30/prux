//! FleetView（输入框下方的可导航代理列表）与会话查看器（全屏实时视图）。
//!
//! 核心提供 [`crate::core::extensions::OverlayView`] 原语，本模块负责**内容与语义**：
//! - FleetView：↑↓ 选代理、Enter 打开查看器、`s` 停止、`r` 续跑、Esc 关闭；
//! - 查看器：transcript 以 [`crate::core::provider::AgentMessage`] 交给 TUI，用**主页同一条**
//!   消息渲染管线渲染（与聊天区一致）；Enter 打开**不**自动聚焦输入，要 steer/续跑先按 `i`，
//!   Enter 提交、Esc 只退出输入态；↑↓/PgUp/PgDn/Home/End 与滚轮/滚动条滚动，
//!   在底部才自动跟随；底部键位只 advertised 此刻真能用的键。
//!
//! 状态是进程级单例；`last_ctx` 记住最近一次工具调用拿到的 [`ToolExecCtx`] + 运行时句柄，
//! 使**UI 触发的 spawn**（`r` 续跑）成为可能——物化子代理必须有 `make_sub_agent`，而它只在工具调用里拿得到。

use super::{
    super::{subagent::agent_types, util::spinner_frame},
    manager, notify, schedule,
    types::{AgentRecord, AgentStatus, AgentType},
    util_notify, widget,
    wizard::WizardState,
    workflow::{
        self,
        inspector::InspectorState,
        task::{RunStatus, WorkflowRun},
    },
};
use crate::{
    core::{
        extensions::{
            DockLine, DockSpan, ExtensionUiRequest, OverlayEditor, OverlayEvent, OverlayInput,
            OverlayKey, OverlaySize, OverlayView, ToolExecCtx, UiNotifyLevel, next_ui_id,
            request_ui,
        },
        provider::AgentMessage,
    },
    extensions::util::truncate_chars,
    utils::glyphs::{DEF_ARROW_UP_DOWN, DEF_QUEUED, DEF_SELECTED_MARK, DEF_TOOL},
};
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, OnceLock},
};
use tokio::runtime::Handle;

/// 打开的覆盖层类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OverlayKind {
    /// 输入框下方的可导航代理列表（↑↓ 选、Enter 打开、`s` 停、`r` 续跑）。
    Fleet,
    /// 单个代理的全屏会话查看器（实时转写 + steer/续跑）。
    Viewer,
    /// 运行列表：内容由 `workflow::task` 提供，本模块只持 id 与按键分发。
    Workflows,
    /// 单次运行的检查器（两栏）：内容由 `workflow::inspector` 提供。
    Inspector,
    /// 定时任务列表（看 + 取消）：内容由 `schedule::view` 提供。
    Schedules,
    /// 创建代理向导（手动）：状态机在 `wizard`。
    Wizard,
    /// 类型编辑：核心多行编辑器改一个 agent `.md` 的完整内容。
    TypeEdit,
}

/// UI 状态（进程级单例）。
#[derive(Default)]
struct UiState {
    /// 当前覆盖层 id（None = 未打开）
    overlay_id: Option<u64>,
    /// 当前覆盖层种类（查看器/检查器/向导…）。
    kind: Option<OverlayKind>,
    /// FleetView 选中下标
    selected: usize,
    /// 查看器目标 agent id
    viewer_target: Option<String>,
    /// 运行列表选中下标
    wf_selected: usize,
    /// 检查器目标 run id 与它的 UI 状态
    ins_target: Option<String>,
    /// 检查器的选中/扁平状态。
    ins: InspectorState,
    /// 定时任务列表选中下标
    sched_selected: usize,
    /// 创建代理向导状态
    wizard: WizardState,
    /// 类型编辑：展示名 / 文件路径 / 初值 / 重新装载信号
    edit_name: String,
    /// 正在编辑的类型文件路径。
    edit_path: Option<PathBuf>,
    /// 编辑器初值（文件内容）。
    edit_content: String,
    /// 重新装载信号（递增即让编辑器重载）。
    edit_generation: u64,
    /// 查看器输入是否被扩展请求聚焦
    focus_input: bool,
    /// 最近一次查看器操作的结果提示（steer/resume/stop 后在固定区显示一行）
    notice: Option<String>,
    /// 最近一次工具调用的物化上下文（UI 触发 spawn 用）
    last_ctx: Option<ToolExecCtx>,
    /// 最近一次工具调用所在的运行时句柄（TUI 线程里 spawn 需要它）
    last_rt: Option<Handle>,
}

/// 取子代理 widget 的进程级 UI 状态单例（首次访问时初始化）。
fn state() -> &'static Mutex<UiState> {
    /// 子代理 widget 的进程级 UI 状态单例，供 TUI 线程与工具执行线程共享。
    static S: OnceLock<Mutex<UiState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(UiState::default()))
}

/// 取 UI 状态锁；持锁线程曾 panic 时取中毒锁内的数据继续使用。
fn lock() -> MutexGuard<'static, UiState> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// 记住最近一次工具调用的物化上下文（`execute_tool_async` 里调用）。
///
/// 只在**异步工具执行**内可见 `tokio::runtime::Handle::current()`，
/// 因此句柄与 ctx 一起缓存，供 TUI 线程（不在运行时内）发起续跑。
pub(crate) fn remember_ctx(ctx: &ToolExecCtx) {
    let handle = Handle::try_current().ok();

    let mut st = lock();
    st.last_ctx = Some(ctx.clone());

    if handle.is_some() {
        st.last_rt = handle;
    }
}

/// 会话切换 / 扩展禁用：关闭覆盖层并丢弃缓存的上下文。
// 保留 last_ctx/last_rt：核心在会话切换后会重新 `on_exec_ctx` 刷新它们，
// 而 `/new` 后首个回合前的 mention 也要能 spawn/resume。
pub(crate) fn reset() {
    let mut st = lock();
    st.overlay_id = None;
    st.kind = None;
    st.selected = 0;
    st.viewer_target = None;
    st.focus_input = false;
    st.notice = None;
    st.edit_name.clear();
    st.edit_path = None;
    st.edit_content.clear();
}

/// 打开创建代理向导（手动）。
pub(crate) fn open_wizard() {
    let id = next_ui_id();
    {
        let mut st = lock();
        st.overlay_id = Some(id);
        st.kind = Some(OverlayKind::Wizard);
        st.wizard = Default::default();
        st.focus_input = false;
    }
    request_ui(ExtensionUiRequest::ShowOverlay { id });
}

/// 打开运行列表（`/agents workflows`）。
pub(crate) fn open_workflows() {
    let id = next_ui_id();
    {
        let mut st = lock();
        st.overlay_id = Some(id);
        st.kind = Some(OverlayKind::Workflows);
        st.selected = 0;
        st.focus_input = false;
    }
    request_ui(ExtensionUiRequest::ShowOverlay { id });
}

/// 打开类型编辑器（一个 agent `.md` 的完整内容）。
pub(crate) fn open_type_editor(name: &str, path: PathBuf, content: String) {
    let id = next_ui_id();
    {
        let mut st = lock();
        st.overlay_id = Some(id);
        st.kind = Some(OverlayKind::TypeEdit);
        st.edit_name = name.to_string();
        st.edit_path = Some(path);
        st.edit_content = content;
        st.edit_generation = st.edit_generation.wrapping_add(1);
        st.focus_input = false;
    }
    request_ui(ExtensionUiRequest::ShowOverlay { id });
}

/// 打开 FleetView。
pub(crate) fn open_fleet() {
    let id = next_ui_id();
    {
        let mut st = lock();
        st.overlay_id = Some(id);
        st.kind = Some(OverlayKind::Fleet);
        st.selected = 0;
        st.focus_input = false;
    }
    request_ui(ExtensionUiRequest::ShowOverlay { id });
}

/// 打开某个代理的会话查看器。
///
/// 一律**不**自动聚焦底部输入行（Enter 进来就是浏览/滚动）；要 steer/续跑先按 `i`
/// 聚焦输入行（footer 会提示），Enter 提交、Esc 只退出输入态。
pub(crate) fn open_viewer(key: &str) {
    let Some(rec) = manager::record(key) else {
        super::util_notify(&format!("unknown agent id: {key}"), UiNotifyLevel::Warning);
        return;
    };

    let id = next_ui_id();
    {
        let mut st = lock();
        st.overlay_id = Some(id);
        st.kind = Some(OverlayKind::Viewer);
        st.viewer_target = Some(rec.id.clone());
        st.focus_input = false;
        st.notice = None;
    }
    request_ui(ExtensionUiRequest::ShowOverlay { id });
}

/// TUI 关闭覆盖层时的回调（清本地状态）。
pub(crate) fn on_closed(id: u64) {
    let mut st = lock();
    if st.overlay_id == Some(id) {
        st.overlay_id = None;
        st.kind = None;
        st.viewer_target = None;
        st.focus_input = false;
        st.edit_name.clear();
        st.edit_path = None;
        st.edit_content.clear();
    }
}

/// 类型编辑：把核心回传的完整文本写回文件；成功后关闭覆盖层。
fn save_type_edit(id: u64, text: &str) -> bool {
    let (path, name) = {
        let st = lock();
        (st.edit_path.clone(), st.edit_name.clone())
    };
    let Some(path) = path else {
        return false;
    };

    match std::fs::write(&path, text) {
        Ok(()) => {
            util_notify(
                &format!("Saved agent type {name:?} ({})", path.display()),
                UiNotifyLevel::Info,
            );
            on_closed(id);
            request_ui(ExtensionUiRequest::HideOverlay { id });
            true
        }
        Err(e) => {
            util_notify(
                &format!("could not save {}: {e}", path.display()),
                UiNotifyLevel::Warning,
            );
            true
        }
    }
}

/// 类型编辑器视图（全屏；编辑整个 `.md` 内容）。
fn type_edit_view(
    name: &str,
    path: &Option<PathBuf>,
    content: &str,
    generation: u64,
) -> OverlayView {
    let label = path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_default();

    OverlayView {
        title: format!("Edit agent: {name}"),
        lines: Vec::new(),
        footer: vec![
            ("Ctrl+S".to_string(), "save".to_string()),
            ("Esc".to_string(), "cancel".to_string()),
        ],
        input: None,
        editor: Some(OverlayEditor {
            label,
            value: content.to_string(),
            placeholder: String::new(),
            rows: 0, // 0 = 填满剩余区域（Fullscreen）
            generation,
        }),
        size: OverlaySize::Fullscreen,
        header: Vec::new(),
        messages: Vec::new(),
        selected: None,
        input_focus: false,
    }
}

/// 覆盖层内容（每帧由 TUI 拉取）。
pub(crate) fn overlay_view(id: u64, cols: u16) -> Option<OverlayView> {
    let (
        kind,
        selected,
        target,
        focus,
        wf_selected,
        ins_target,
        ins,
        sched_selected,
        wizard,
        edit_name,
        edit_path,
        edit_content,
        edit_generation,
        notice,
    ) = {
        let st = lock();
        if st.overlay_id != Some(id) {
            return None;
        }

        (
            st.kind?,
            st.selected,
            st.viewer_target.clone(),
            st.focus_input,
            st.wf_selected,
            st.ins_target.clone(),
            st.ins,
            st.sched_selected,
            st.wizard.clone(),
            st.edit_name.clone(),
            st.edit_path.clone(),
            st.edit_content.clone(),
            st.edit_generation,
            st.notice.clone(),
        )
    };
    match kind {
        OverlayKind::Fleet => Some(fleet_view(selected)),
        OverlayKind::Wizard => Some(wizard.view()),
        OverlayKind::TypeEdit => Some(type_edit_view(
            &edit_name,
            &edit_path,
            &edit_content,
            edit_generation,
        )),
        OverlayKind::Workflows => Some(workflow::task::overlay_view(wf_selected)),
        // 检查器：两栏（左树右详情）由扩展按终端宽度自己拼
        OverlayKind::Inspector => {
            let target = ins_target.clone()?;
            let run = workflow::task::find(&target)?;
            Some(workflow::inspector::view(&run, ins, cols))
        }
        // 定时任务列表：没有任务也认领（显示"no scheduled jobs"而不是闪退）
        OverlayKind::Schedules => Some(schedule::view(sched_selected)),
        // 目标被清理（会话切换/记录淘汰）→ 不再认领，TUI 自动关闭
        OverlayKind::Viewer => target
            .as_deref()
            .and_then(manager::record)
            .map(|rec| viewer_view(&rec, focus, notice.as_deref())),
    }
}

/// 覆盖层事件。
pub(crate) fn on_overlay_event(id: u64, ev: &OverlayEvent) -> bool {
    let kind = {
        let st = lock();
        if st.overlay_id != Some(id) {
            return false;
        }
        st.kind
    };

    match ev {
        OverlayEvent::Closed => {
            on_closed(id);
            true
        }
        OverlayEvent::Submit { text } => {
            submit(text.clone());
            true
        }
        OverlayEvent::EditorSubmit { text } => match kind {
            Some(OverlayKind::TypeEdit) => save_type_edit(id, text),
            Some(OverlayKind::Wizard) => {
                lock().wizard.editor_submit(text.clone());
                true
            }
            _ => false,
        },
        OverlayEvent::Key { key, input } => match kind {
            Some(OverlayKind::Fleet) => fleet_key(key),
            Some(OverlayKind::Viewer) => viewer_key(key, input.as_deref().unwrap_or("")),
            // 运行列表是只读的：↑↓/PgUp/PgDn/Home/End 交给 TUI 的默认滚动，
            Some(OverlayKind::Workflows) => workflows_key(key),
            // 检查器：控制键自己消费（滚动/选择）
            Some(OverlayKind::Inspector) => inspector_key(key),
            // 定时任务列表：↑↓ 选、d 取消
            Some(OverlayKind::Schedules) => schedules_key(key),
            // 创建向导：自定义状态机自持
            Some(OverlayKind::Wizard) => lock().wizard.key(id, key),
            // 类型编辑：文本编辑键由核心接管，这里只处理 Esc（交给 TUI 默认关闭）
            Some(OverlayKind::TypeEdit) => false,
            None => false,
        },
    }
}

/// 列表导航环绕：到达底部再按 ↓ 回顶部、顶部再按 ↑ 到底部。空列表返回 0（调用方不会渲染选中项）。
fn wrap_index(current: usize, delta: isize, len: usize) -> usize {
    if len == 0 {
        return 0;
    }

    (current as isize + delta).rem_euclid(len as isize) as usize
}

/// 运行列表按键：↑↓ 选（环绕）、Enter 进检查器。
fn workflows_key(key: &OverlayKey) -> bool {
    let total = workflow::task::list().len();
    match key {
        OverlayKey::Up => {
            let mut st = lock();
            st.wf_selected = wrap_index(st.wf_selected, -1, total);
            true
        }
        OverlayKey::Down => {
            let mut st = lock();
            st.wf_selected = wrap_index(st.wf_selected, 1, total);
            true
        }
        OverlayKey::Enter => {
            let runs = workflow::task::list();
            let idx = lock().wf_selected.min(runs.len().saturating_sub(1));
            if let Some(run) = runs.get(idx) {
                open_inspector(&run.id);
            }
            true
        }
        _ => false,
    }
}

/// 检查器按键：↑↓ 选行、`f` 扁平/阶段、`p` 暂停/继续、`s` 跳过、`r` 重跑、`x` 停止、
/// `c`/Enter 打开选中 agent 的会话、Esc 由 TUI 关闭（回列表由 `on_closed` 处理）。
fn inspector_key(key: &OverlayKey) -> bool {
    let (target, ins) = {
        let st = lock();
        (st.ins_target.clone(), st.ins)
    };

    let Some(target) = target else {
        return false;
    };

    let Some(run) = workflow::task::find(&target) else {
        return false; // 运行已被清掉（会话切换）：不再认领，TUI 会关掉覆盖层
    };

    let rows = workflow::inspector::rows(&run, ins);

    match key {
        OverlayKey::Up => {
            let mut st = lock();
            st.ins.selected = wrap_index(st.ins.selected, -1, rows.len());
            true
        }
        OverlayKey::Down => {
            let mut st = lock();
            st.ins.selected = wrap_index(st.ins.selected, 1, rows.len());
            true
        }
        OverlayKey::Char('f') => {
            let mut st = lock();
            st.ins.flat = !st.ins.flat;
            st.ins.selected = 0;
            true
        }
        OverlayKey::Char('p') => {
            if let Some(ctl) = run.control.as_ref() {
                let paused = ctl.is_paused();
                if paused {
                    ctl.resume();
                } else {
                    ctl.pause();
                }

                workflow::task::set_paused(&target, !paused);

                if paused {
                    say("workflow resumed");
                } else {
                    say("workflow paused (running agents finish; none new start)");
                }
            }
            true
        }
        OverlayKey::Char('s') => {
            if let Some(ctl) = run.control.as_ref()
                && let Some(index) = workflow::inspector::selected_agent(&run, ins)
                && !ctl.skip(index)
            {
                say("that agent has already settled");
            }
            true
        }
        OverlayKey::Char('r') => {
            if let Some(ctl) = run.control.as_ref()
                && let Some(index) = workflow::inspector::selected_agent(&run, ins)
                && !ctl.retry(index)
            {
                say("retry only works while that agent is running");
            }
            true
        }
        OverlayKey::Char('x') => {
            match workflow::task::stop(&target) {
                Ok(()) => say(format!("stopping workflow {target}")),
                Err(e) => report(Err(e)),
            }
            true
        }
        OverlayKey::Char('c') | OverlayKey::Enter => {
            if let Some(agent) = workflow::inspector::selected_agent(&run, ins) {
                match workflow::inspector::agent_record_id(&run, agent) {
                    Some(record) => open_viewer(&record),
                    None => say("that agent has no record yet"),
                }
            }
            true
        }
        _ => false,
    }
}

/// 定时任务列表按键：↑↓ 选（环绕）、`d` 取消选中的那个。可配 Ctrl+J/K（TUI 翻成 ↑↓）。
fn schedules_key(key: &OverlayKey) -> bool {
    let total = schedule::list().len();
    match key {
        OverlayKey::Up => {
            let mut st = lock();
            st.sched_selected = wrap_index(st.sched_selected, -1, total);
            true
        }
        OverlayKey::Down => {
            let mut st = lock();
            st.sched_selected = wrap_index(st.sched_selected, 1, total);
            true
        }
        OverlayKey::Char('d') => {
            let jobs = schedule::list();
            let idx = lock().sched_selected.min(jobs.len().saturating_sub(1));

            if let Some(job) = jobs.get(idx) {
                if schedule::cancel(&job.id) {
                    say(format!("cancelled scheduled job {:?}", job.name));

                    let mut st = lock();
                    st.sched_selected = st.sched_selected.saturating_sub(1);
                } else {
                    say("that job is already gone");
                }
            }
            true
        }
        OverlayKey::Enter => true, // Enter 只吞掉，不做事（列表是只读的；Esc 关闭）
        _ => false,
    }
}

/// 打开定时任务列表。
pub(crate) fn open_schedules() {
    let id = next_ui_id();
    {
        let mut st = lock();
        st.overlay_id = Some(id);
        st.kind = Some(OverlayKind::Schedules);
        st.sched_selected = 0;
        st.focus_input = false;
    }
    request_ui(ExtensionUiRequest::ShowOverlay { id });
}

/// 打开某次运行的检查器。
pub(crate) fn open_inspector(run_id: &str) {
    let id = next_ui_id();
    {
        let mut st = lock();
        st.overlay_id = Some(id);
        st.kind = Some(OverlayKind::Inspector);
        st.ins_target = Some(run_id.to_string());
        st.ins = workflow::inspector::InspectorState::default();
        st.focus_input = false;
    }
    request_ui(ExtensionUiRequest::ShowOverlay { id });
}

/// FleetView 按键：↑↓ 选（环绕）、Enter 打开、`s` 停、`r` 续跑。
///
/// 与其它列表一致，导航只认 ↑↓（Ctrl+J/K 由 TUI 翻成同一组键）。
fn fleet_key(key: &OverlayKey) -> bool {
    let selected = lock().selected;
    let total = fleet_row_count();

    match key {
        OverlayKey::Up => {
            lock().selected = wrap_index(selected, -1, total);
            true
        }
        OverlayKey::Down => {
            lock().selected = wrap_index(selected, 1, total);
            true
        }
        OverlayKey::Home => {
            lock().selected = 0;
            true
        }
        OverlayKey::End => {
            lock().selected = total.saturating_sub(1);
            true
        }
        OverlayKey::Enter => {
            match selected_row() {
                Some(FleetRow::Agent(id)) => open_viewer(&id),
                Some(FleetRow::Workflow(id)) => open_inspector(&id),
                None => {}
            }
            true
        }
        OverlayKey::Char('s') => {
            match selected_row() {
                Some(FleetRow::Agent(id)) => report(manager::stop(&id)),
                Some(FleetRow::Workflow(id)) => report(workflow::task::stop(&id)),
                None => {}
            }
            true
        }
        OverlayKey::Char('r') => {
            // 续跑只对 agent 有意义；工作流行走检查器里的控制键
            if let Some(id) = selected_id() {
                resume(&id, "");
            }
            true
        }
        _ => false,
    }
}

/// 查看器按键。
fn viewer_key(key: &OverlayKey, input: &str) -> bool {
    let Some(id) = lock().viewer_target.clone() else {
        return false;
    };

    match key {
        // 聚焦输入（不插入字符）
        OverlayKey::Char('i') => {
            lock().focus_input = true;
            true
        }
        OverlayKey::Char('s') => {
            // 已结算的 agent 停不掉（`manager::stop` 会报 "already completed"）；
            // footer 也不提示它，这里直接忽略，避免一颗无用的键换来一行报错。
            if manager::record(&id).is_some_and(|r| !r.status.is_terminal()) {
                match manager::stop(&id) {
                    Ok(()) => note(format!("stop requested: {id}")),
                    Err(e) => {
                        note(format!("stop failed: {e}"));
                        util_notify(&e, UiNotifyLevel::Warning);
                    }
                }
            }
            true
        }
        OverlayKey::Char('r') => {
            // 续跑只对**已结算**的 agent 有意义（运行中该 steer）。footer 同样按状态提示。
            match manager::record(&id).map(|r| r.status) {
                Some(s) if s.is_terminal() => resume(&id, input),
                Some(s) => note(format!("agent {id} is {}; steer it instead", s.as_str())),
                None => {}
            }
            true
        }
        // 退出输入态（TUI 也会本地失焦），不关覆盖层
        OverlayKey::Esc if lock().focus_input => {
            lock().focus_input = false;
            true
        }
        _ => false,
    }
}

/// 内联输入提交：运行中 → steer；已终结 → 续跑。
fn submit(text: String) {
    let text = text.trim().to_string();
    let (kind, target) = {
        let st = lock();
        (st.kind, st.viewer_target.clone())
    };

    let Some(id) = target else {
        return;
    };

    if kind != Some(OverlayKind::Viewer) {
        return;
    }

    if text.is_empty() {
        return;
    }

    let status = manager::record(&id).map(|r| r.status);
    match status {
        Some(s) if !s.is_terminal() => match manager::steer(&id, &text) {
            Ok(()) => note(format!("steer sent: {}", truncate_chars(&text, 80))),
            Err(e) => {
                note(format!("steer failed: {e}"));
                util_notify(&e, UiNotifyLevel::Warning);
            }
        },
        Some(_) => resume(&id, &text),
        None => {}
    }

    lock().focus_input = false;
}

/// 续跑（UI 触发）：需要缓存的 `ToolExecCtx` + 运行时句柄；后台执行。
///
/// 真正的 `dispatch` 放到后台任务里跑，**成功与失败都回写 `note`**：先前只在
/// `spawn_background_agent` 返回 `false`（无 ctx）时才提示，而 `dispatch` 自身的
/// `Err`（无落盘会话 / 未知 resume 目标 / 模型解析失败…）被 `_ =` 丢弃——
/// 用户按 Enter 后覆盖层停在原样，看起来就是“没反应”。
fn resume(key: &str, prompt: &str) {
    let Some((ctx, rt)) = cached_ctx() else {
        note("cannot resume yet: this session has no execution context");
        util_notify(
            "cannot resume yet: this session has no execution context",
            UiNotifyLevel::Warning,
        );
        return;
    };

    let Some(rec) = manager::record(key) else {
        note(format!("cannot resume {key}: the agent record is gone"));
        return;
    };

    let prompt = if prompt.trim().is_empty() {
        "Continue where you left off.".to_string()
    } else {
        prompt.trim().to_string()
    };

    let roster = agent_types::discover(&ctx.cwd);
    let Ok(ty) = agent_types::resolve(&roster.types, &rec.agent_type).cloned() else {
        let msg = format!(
            "cannot resume {}: the {} agent is no longer available",
            rec.id, rec.agent_type
        );
        note(&msg);
        util_notify(&msg, UiNotifyLevel::Warning);
        return;
    };

    let id = rec.id.clone();
    note(format!("resuming {id}…"));

    rt.spawn(async move {
        let req = manager::request_for_type(&ty, prompt, format!("resume {id}"), Some(id.clone()));
        match manager::dispatch(&ctx, &ty, req).await {
            Ok(_) => note(format!("resumed {id}")),
            Err(e) => {
                note(format!("resume failed: {e}"));
                util_notify(&e, UiNotifyLevel::Warning);
            }
        }
    });
}

/// 缓存的物化上下文 + 运行时句柄（TUI / 输入线程发起 spawn 用）。
pub(super) fn cached_ctx() -> Option<(ToolExecCtx, Handle)> {
    let st = lock();
    Some((st.last_ctx.clone()?, st.last_rt.clone()?))
}

/// 从缓存的 ctx/rt 后台派发一个已解析类型的代理（mention / UI 续跑共用）。
/// 无缓存 ctx 时返回 `false`（调用方自行提示）。
pub(super) fn spawn_background_agent(
    ty: AgentType,
    prompt: String,
    description: String,
    resume_id: Option<String>,
) -> bool {
    let Some((ctx, rt)) = cached_ctx() else {
        return false;
    };

    rt.spawn(async move {
        let req = manager::request_for_type(&ty, prompt, description, resume_id);
        _ = manager::dispatch(&ctx, &ty, req).await;
    });
    true
}

/// FleetView 内容。
fn fleet_view(selected: usize) -> OverlayView {
    let recs = manager::list();
    let runs = workflow::task::list();
    let total = recs.len() + runs.len();
    let mut lines: Vec<DockLine> = Vec::new();

    if total == 0 {
        lines.push(vec![DockSpan::new(
            "dim",
            "#666666",
            "no sub-agents in this session",
        )]);
    }

    for (i, rec) in recs.iter().enumerate() {
        lines.push(fleet_line(rec, i == selected));
    }

    for (k, run) in runs.iter().enumerate() {
        lines.push(workflow_fleet_line(run, recs.len() + k == selected));
    }

    let active = recs.iter().filter(|r| !r.status.is_terminal()).count();
    let runs_live = runs.iter().filter(|r| !r.status.is_terminal()).count();
    let title = if runs.is_empty() {
        format!("Sub-agents · {active} active · {} total", recs.len())
    } else {
        format!(
            "Sub-agents · {active} active · {} total · {runs_live} workflow(s)",
            recs.len()
        )
    };

    OverlayView {
        title,
        lines,
        footer: vec![
            (DEF_ARROW_UP_DOWN.to_string(), "move".to_string()),
            ("Enter".to_string(), "open".to_string()),
            ("s".to_string(), "stop".to_string()),
            ("r".to_string(), "resume".to_string()),
            ("Esc".to_string(), "close".to_string()),
        ],
        input: None,
        editor: None,
        // 面板样式：占据输入框区域（隐藏主页输入框）+ 上下分割线，对齐 `/agents list` 选择器
        size: OverlaySize::Panel { max_rows: 12 },
        header: Vec::new(),
        messages: Vec::new(),
        selected: Some(selected.min(total.saturating_sub(1))),
        input_focus: false,
    }
}

/// 工作流运行状态图标（运行中用 spinner 帧，与子代理一致，保证 busy 时有动画）。
fn run_status_icon(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Running => spinner_frame(),
        other => other.icon(),
    }
}

/// FleetView 里一行工作流运行：`▶ ⟳ workflow name id · 3/7 agents · 1m12s`。
fn workflow_fleet_line(run: &WorkflowRun, selected: bool) -> DockLine {
    let header = run.header();
    let (key, fallback) = run.status.color();

    vec![
        DockSpan::new(
            if selected { "accent" } else { "muted" },
            if selected { "#8abeb7" } else { "#999999" },
            if selected { DEF_SELECTED_MARK } else { "  " },
        ),
        DockSpan::new(key, fallback, format!("{} ", run_status_icon(run.status))),
        DockSpan::new("accent", "#8abeb7", format!("workflow {}", header.name)),
        DockSpan::new("dim", "#666666", format!(" {} · {}", run.id, header.stats)),
    ]
}

/// FleetView 一行：`▶ ✓ Explore (name) id · 描述 · 3t · 1.2k tok · 4.2s`
fn fleet_line(rec: &AgentRecord, selected: bool) -> DockLine {
    let mut spans: Vec<DockSpan> = Vec::new();
    if selected {
        spans.push(DockSpan::new("accent", "#8abeb7", DEF_SELECTED_MARK));
    } else {
        spans.push(DockSpan::plain("  "));
    }

    let (status_key, status_fallback) = rec.status.color();
    spans.push(DockSpan::new(
        status_key,
        status_fallback,
        format!("{} ", widget::status_icon(rec.status)),
    ));

    let badge = {
        let base = match &rec.name {
            Some(n) => format!("{} ({n})", rec.display_name),
            None => rec.display_name.clone(),
        };
        match rec.handle.as_deref() {
            Some(h) => format!("@{h} {base}"),
            None => base,
        }
    };

    match widget::resolve_agent_color(rec.color.as_deref()) {
        Some(hex) => spans.push(DockSpan::hex(hex, badge)),
        None => spans.push(DockSpan::new("accent", "#8abeb7", badge)),
    }

    spans.push(DockSpan::new("dim", "#666666", format!(" {}", rec.id)));

    let activity = match (&rec.activity, rec.status) {
        (Some(tool), AgentStatus::Running) => format!(" {} {tool}", DEF_TOOL),
        _ => {
            let d = truncate_chars(&rec.description, 36);
            if d.is_empty() {
                String::new()
            } else {
                format!(" {d}")
            }
        }
    };

    if !activity.is_empty() {
        spans.push(DockSpan::new("text", "#d0d0d0", activity));
    }

    let mut stats = Vec::new();
    if rec.status != AgentStatus::Completed {
        stats.push(rec.status.as_str().to_string());
    }

    stats.push(format!("{}t", rec.turns));
    stats.push(format!(
        "{} tok",
        notify::format_tokens(rec.usage.display_total())
    ));
    stats.push(format!("{:.1}s", rec.duration_ms() as f64 / 1000.0));

    spans.push(DockSpan::new(
        "muted",
        "#999999",
        format!(" · {}", stats.join(" · ")),
    ));
    spans
}

/// 查看器内容：固定区（状态头 / 路径 / 活动行 / 回显）+ 交给 TUI 渲染的 transcript。
fn viewer_view(rec: &AgentRecord, focus_input: bool, notice: Option<&str>) -> OverlayView {
    // 固定区（不随滚动）：状态头 + 路径 + 运行中活动行 + 操作回显。
    // 全屏覆盖层遮住聊天区，回显放固定区才能保证任何滚动位置都看得见。
    let mut header: Vec<DockLine> = Vec::new();
    let (key, fallback) = rec.status.color();

    header.push(vec![DockSpan::new(
        key,
        fallback,
        format!(
            "{} {}",
            widget::status_icon(rec.status),
            rec.status_header()
        ),
    )]);

    if let Some(session) = rec.session_path.as_deref() {
        // 只显示文件名：完整路径在窄终端里会被截断，且这里只需要“哪个会话”
        let name = Path::new(session)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| session.to_string());
        header.push(vec![DockSpan::new(
            "dim",
            "#666666",
            format!("session: {name}"),
        )]);
    }

    if let Some(output) = rec.output_path.as_deref() {
        header.push(vec![DockSpan::new(
            "dim",
            "#666666",
            format!("output: {output}"),
        )]);
    }

    if rec.status == AgentStatus::Running {
        let activity = rec
            .activity
            .as_deref()
            .map(|t| format!("{DEF_TOOL} {t}"))
            .unwrap_or_else(|| DEF_QUEUED.to_string());

        header.push(vec![DockSpan::new("accent", "#8abeb7", activity)]);
    }

    if let Some(text) = notice.filter(|t| !t.trim().is_empty()) {
        header.push(vec![DockSpan::new("accent", "#8abeb7", text.to_string())]);
    }

    // 滚动区：transcript 交给 TUI 用主页同一条管线渲染。
    let messages = transcript_messages(rec);
    let mut lines: Vec<DockLine> = Vec::new();
    if messages.is_empty() {
        lines.push(vec![DockSpan::new("dim", "#666666", "(no messages yet)")]);
    }

    // 底部只 advertised 此刻真能用的键（"不可用就不要显示"）：
    // - 已结算的 agent 停不掉、也 steer 不了 → 不提示 `s`/`i steer`；
    // - 没落盘会话的已结算 agent 续不了跑 → 不提示 `i`/`r`，连输入行都不给；
    // - 输入聚焦时字符都进输入框，命令键（`s`/`r`/`i`）此刻不可用 → 只提示 Enter/Esc/滚动。
    let terminal = rec.status.is_terminal();
    let can_stop = !terminal; // 排队/运行中才停得掉
    let can_steer = !terminal; // 排队/运行中才 steer 得动
    let can_resume = terminal && rec.session_path.is_some(); // 结算且有落盘会话才续得跑
    let has_input = can_steer || can_resume;
    let focused = focus_input && has_input;

    let mut footer: Vec<(String, String)> = Vec::new();
    if focused {
        footer.push(("Enter".to_string(), "send".to_string()));
    }

    footer.push((DEF_ARROW_UP_DOWN.to_string(), "scroll".to_string()));
    if !focused {
        if can_stop {
            footer.push(("s".to_string(), "stop".to_string()));
        }

        if can_resume {
            footer.push(("r".to_string(), "resume".to_string()));
        }

        if has_input {
            footer.push((
                "i".to_string(),
                if terminal {
                    "resume with message"
                } else {
                    "steer"
                }
                .to_string(),
            ));
        }
    }

    footer.push((
        "Esc".to_string(),
        if focused { "cancel input" } else { "close" }.to_string(),
    ));

    OverlayView {
        title: format!(
            "{} · {} · {}t · {} tok",
            rec.id,
            rec.display_name,
            rec.turns,
            notify::format_tokens(rec.usage.display_total())
        ),
        header,
        lines,
        messages,
        footer,
        input: has_input.then(|| OverlayInput {
            label: if terminal {
                "resume>".to_string()
            } else {
                "steer>".to_string()
            },
            value: String::new(),
            placeholder: if terminal {
                "message for the resumed run…".to_string()
            } else {
                "steer the running agent…".to_string()
            },
        }),
        editor: None,
        size: OverlaySize::Fullscreen,
        selected: None,
        input_focus: focused,
    }
}

/// transcript（`message_end` 事件的 message JSON）→ `AgentMessage` 列表。
///
/// 这是主页消息流同一套渲染管线的输入类型；反序列化失败的消息退化成一条 assistant
/// 文本消息（原始 JSON），不静默丢弃。
fn transcript_messages(rec: &AgentRecord) -> Vec<AgentMessage> {
    let mut out = Vec::new();

    for m in &rec.transcript {
        match serde_json::from_value::<AgentMessage>(m.clone()) {
            Ok(msg) => out.push(msg),
            Err(_) => {
                let raw = serde_json::to_string(m).unwrap_or_default();
                let mut fallback =
                    AgentMessage::user_text(&format!("[unrenderable message] {raw}"));
                fallback.role = "assistant".to_string();
                out.push(fallback);
            }
        }
    }

    out
}

/// 当前选中项 id。
fn selected_id() -> Option<String> {
    match selected_row()? {
        FleetRow::Agent(id) => Some(id),
        FleetRow::Workflow(_) => None,
    }
}

/// FleetView 的合并行：agent 在前、工作流运行在后。
#[derive(Debug, Clone)]
enum FleetRow {
    /// 子代理记录行，载荷为 agent id。
    Agent(String),
    /// 工作流运行行，载荷为 run id。
    Workflow(String),
}

/// FleetView 合并列表的总行数（子代理行 + 工作流运行行）。
fn fleet_row_count() -> usize {
    manager::list().len() + workflow::task::list().len()
}

/// 当前选中行的载荷；选中下标越界（如列表刚变短）时为 `None`。
fn selected_row() -> Option<FleetRow> {
    let selected = lock().selected;
    let agents = manager::list();

    if selected < agents.len() {
        return Some(FleetRow::Agent(agents[selected].id.clone()));
    }

    let idx = selected - agents.len();
    workflow::task::list()
        .get(idx)
        .map(|r| FleetRow::Workflow(r.id.clone()))
}

/// 停止/steer 的结果提示（成功不打扰，失败告警）。
fn report(result: Result<(), String>) {
    if let Err(e) = result {
        util_notify(&e, UiNotifyLevel::Warning);
    }
}

/// 直接说一句（信息级）。
fn say(text: impl Into<String>) {
    util_notify(&text.into(), UiNotifyLevel::Info);
}

/// 在查看器固定区记一行操作回显（成功/失败）。
///
/// 全屏覆盖层会遮住聊天区，`util_notify` 的提示用户看不到；这里把结果就地绑到覆盖层
/// 的**固定区**（不随滚动移动），使 steer/resume 提交后总有可见反馈。
/// 下一行会覆盖上一行；重新打开查看器时清空。
fn note(text: impl Into<String>) {
    let mut st = lock();
    st.notice = Some(text.into());
}

/// 测试用：丢弃缓存的上下文（`reset()` 有意保留它，无法用 reset 模拟"无活跃会话"）。
#[cfg(test)]
pub(super) fn clear_ctx_for_test() {
    let mut st = lock();
    st.last_ctx = None;
    st.last_rt = None;
}

/// 诊断用：当前覆盖层是否打开（测试）。
#[cfg(test)]
pub(crate) fn is_open() -> bool {
    lock().overlay_id.is_some()
}

/// 诊断用：当前选中下标（测试）。
#[cfg(test)]
pub(crate) fn selected_index() -> usize {
    lock().selected
}

/// 诊断用：当前查看目标（测试）。
#[cfg(test)]
pub(crate) fn viewer_target_id() -> Option<String> {
    lock().viewer_target.clone()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::subagent::types::UsageTotals;

    fn rec(id: &str, status: AgentStatus) -> AgentRecord {
        AgentRecord {
            id: id.to_string(),
            name: Some("probe".to_string()),
            handle: Some("explore".to_string()),
            alias: Some("probe".to_string()),
            agent_type: "Explore".to_string(),
            display_name: "Explore".to_string(),
            description: "look around".to_string(),
            status,
            background: true,
            color: Some("cyan".to_string()),
            activity: None,
            turns: 2,
            tool_uses: 1,
            usage: UsageTotals {
                input: 500,
                output: 100,
                cache_read: 0,
                cache_write: 0,
                cost: 0.0,
            },
            started_ms: 0,
            ended_ms: Some(1000),
            result: Some("done".to_string()),
            transcript: vec![
                serde_json::json!({"role":"user","content":[{"type":"text","text":"go"}]}),
                serde_json::json!({"role":"assistant","content":[{"type":"text","text":"ok"}]}),
            ],
            session_path: Some("/tmp/s.jsonl".to_string()),
            output_path: None,
            max_turns: None,
            steer: None,
            abort: None,
            stop_requested: false,
            worktree: None,
            worktree_result: None,
            parent_agent_id: None,
            depth: 0,
            workflow_owned: false,
            model: None,
            compaction_count: 0,
            consumed: false,
            done_rx: None,
        }
    }

    fn text(line: &DockLine) -> String {
        line.iter().map(|s| s.text.as_str()).collect()
    }

    #[test]
    fn fleet_line_marks_selection_and_carries_badge_color() {
        let r = rec("a1b2", AgentStatus::Running);
        let selected = fleet_line(&r, true);
        assert!(text(&selected).starts_with("▶ "), "{}", text(&selected));
        assert!(text(&selected).contains("a1b2"));
        assert!(text(&selected).contains("Explore (probe)"));
        assert!(text(&selected).contains("2t"));
        let badge = selected
            .iter()
            .find(|s| s.text.contains("Explore"))
            .expect("badge");
        assert_eq!(badge.fallback.as_ref(), "#0891B2", "cyan → hex");

        let plain = fleet_line(&r, false);
        assert!(text(&plain).starts_with("  "), "{}", text(&plain));
    }

    /// 运行中的图标必须是动画 spinner 帧（不是静态的 `AgentStatus::icon()`），
    /// 否则 overlay 里 running 的进度图标会卡住不动。
    #[test]
    fn running_icons_use_the_animated_spinner() {
        let r = rec("a1b2", AgentStatus::Running);
        let icon = widget::status_icon(AgentStatus::Running);
        assert!(
            text(&fleet_line(&r, false)).contains(icon),
            "FleetView 行：{}",
            text(&fleet_line(&r, false))
        );
        assert!(
            text(&viewer_view(&r, false, None).header[0]).contains(icon),
            "查看器状态头：{}",
            text(&viewer_view(&r, false, None).header[0])
        );
    }

    #[test]
    fn fleet_view_lists_records_and_clamps_selection() {
        // manager / workflow 都是进程级全局态：与其它触碰它们的测试串行
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        manager::reset_all();
        super::super::workflow::task::reset_all();
        // 无记录：提示行 + selected 指向 0
        let v = fleet_view(3);
        assert_eq!(v.lines.len(), 1, "空态一行提示");
        assert!(text(&v.lines[0]).contains("no sub-agents"));
        assert_eq!(v.selected, Some(0));
        assert_eq!(v.size, OverlaySize::Panel { max_rows: 12 });
        assert!(v.footer.iter().any(|(k, _)| k == "Enter"));
        assert!(v.input.is_none());
    }

    #[test]
    fn viewer_view_shows_status_transcript_and_input() {
        let running = rec("a1b2", AgentStatus::Running);
        let v = viewer_view(&running, false, Some("steer sent: go"));
        assert_eq!(v.size, OverlaySize::Fullscreen);
        // 固定区：状态头 + session 路径 + 操作回显
        assert!(
            text(&v.header[0]).contains("[subagent a1b2"),
            "{}",
            text(&v.header[0])
        );
        assert!(
            v.header
                .iter()
                .any(|l| text(l).contains("session: s.jsonl")),
            "session 只显示文件名：{:?}",
            v.header.iter().map(text).collect::<Vec<_>>()
        );
        assert!(
            !v.header.iter().any(|l| text(l).contains("/tmp/s.jsonl")),
            "不应再显示完整路径"
        );
        assert!(
            v.header.iter().any(|l| text(l).contains("steer sent: go")),
            "操作回显应在固定区：{:?}",
            v.header.iter().map(text).collect::<Vec<_>>()
        );
        // transcript 交给 TUI 用主页管线渲染
        assert_eq!(v.messages.len(), 2, "两条 transcript 消息");
        assert_eq!(v.messages[0].role, "user");
        assert!(v.messages[0].text().contains("go"));
        assert_eq!(v.messages[1].role, "assistant");
        let input = v.input.as_ref().expect("steer 输入行");
        assert_eq!(input.label, "steer>");
        assert!(v.footer.iter().any(|(_, d)| d == "steer"));
        assert!(
            !v.footer.iter().any(|(k, _)| k == "f" || k == "m"),
            "f/m 快捷键已移除：{:?}",
            v.footer
        );
        // 运行中：停得掉（`s`），但续不了跑（`r` 不提示），也不提示 Enter（未聚焦输入）
        assert!(
            v.footer.iter().any(|(k, d)| k == "s" && d == "stop"),
            "运行中应能停：{:?}",
            v.footer
        );
        assert!(
            !v.footer.iter().any(|(k, _)| k == "r"),
            "运行中的 agent 续不了跑，不该提示 r：{:?}",
            v.footer
        );
        assert!(
            !v.footer.iter().any(|(k, _)| k == "Enter"),
            "{:?}",
            v.footer
        );

        // 已结算 + 输入聚焦：只提示 Enter/Esc/滚动；字符进输入框，`s`/`r`/`i` 此刻都不是命令
        let done = rec("c3d4", AgentStatus::Completed);
        let v = viewer_view(&done, true, None);
        assert!(v.input_focus);
        assert_eq!(v.input.as_ref().unwrap().label, "resume>");
        assert!(
            v.footer.iter().any(|(k, d)| k == "Enter" && d == "send"),
            "聚焦时应提示 Enter 发送：{:?}",
            v.footer
        );
        assert!(
            v.footer
                .iter()
                .any(|(k, d)| k == "Esc" && d == "cancel input"),
            "{:?}",
            v.footer
        );
        assert!(
            !v.footer
                .iter()
                .any(|(k, _)| k == "s" || k == "r" || k == "i"),
            "聚焦打字时命令键不可用，不该提示：{:?}",
            v.footer
        );

        // 已结算 + 未聚焦：能续跑（`i`/`r`），但停不掉（不提示 `s`）
        let v = viewer_view(&done, false, None);
        assert!(
            v.footer
                .iter()
                .any(|(k, d)| k == "i" && d == "resume with message"),
            "{:?}",
            v.footer
        );
        assert!(
            v.footer.iter().any(|(k, d)| k == "r" && d == "resume"),
            "{:?}",
            v.footer
        );
        assert!(
            !v.footer.iter().any(|(k, _)| k == "s"),
            "已结算的 agent 停不掉，不该提示 s：{:?}",
            v.footer
        );
    }

    /// 结算后没有落盘会话 → 续不了跑：连输入行与 `i`/`r` 都不该出现（“不可用就不要显示”）。
    #[test]
    fn viewer_hides_resume_affordances_without_a_session() {
        let mut done = rec("nosess", AgentStatus::Completed);
        done.session_path = None;
        let v = viewer_view(&done, false, None);
        assert!(v.input.is_none(), "无会话时不该给输入行");
        assert!(!v.input_focus);
        assert!(
            !v.footer.iter().any(|(k, _)| k == "i" || k == "r"),
            "无会话时不该提示续跑：{:?}",
            v.footer
        );
        assert!(v.footer.iter().any(|(k, d)| k == "Esc" && d == "close"));
    }

    /// 反序列化失败的消息退化成一条 assistant 文本消息，不静默丢弃。
    #[test]
    fn transcript_messages_falls_back_on_bad_json() {
        let mut r = rec("badjson", AgentStatus::Completed);
        r.transcript = vec![serde_json::json!({"role": "assistant"})]; // 缺 content
        let msgs = transcript_messages(&r);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "assistant");
        assert!(msgs[0].text().contains("unrenderable"));
    }

    /// 操作回显在**固定区**（不随滚动移动），任何滚动位置都看得见。
    #[test]
    fn notice_is_in_the_fixed_header() {
        let _g = manager::test_lock();
        reset();
        note("resumed n1");

        let rec = rec("n1", AgentStatus::Completed);
        let notice = lock().notice.clone();
        let v = viewer_view(&rec, false, notice.as_deref());
        assert!(
            v.header.iter().any(|l| text(l).contains("resumed n1")),
            "回显应在固定区: {:?}",
            v.header.iter().map(text).collect::<Vec<_>>()
        );
        reset();
    }

    /// 查看器：只有 `i` 聚焦下方输入（其它字符键交回 TUI，不夺焦点）。
    #[test]
    fn viewer_only_i_focuses_the_input() {
        let _g = manager::test_lock();
        reset();
        lock().viewer_target = Some("ghost".to_string());
        lock().focus_input = false;

        assert!(!viewer_key(&OverlayKey::Char('z'), ""), "普通字符交回 TUI");
        assert!(!lock().focus_input, "普通字符不应聚焦输入");

        assert!(viewer_key(&OverlayKey::Char('i'), ""), "`i` 被扩展消费");
        assert!(lock().focus_input, "`i` 聚焦输入");

        // 聚焦后 Esc 只取消输入（不关覆盖层）
        assert!(viewer_key(&OverlayKey::Esc, ""));
        assert!(!lock().focus_input);
        reset();
    }

    /// 查看器不再消费 `f`/`m`/`End`：滚动/跟随全交给 TUI 默认语义。
    #[test]
    fn viewer_does_not_consume_f_m_or_end() {
        let _g = manager::test_lock();
        reset();
        lock().viewer_target = Some("ghost".to_string());

        for key in [
            OverlayKey::Up,
            OverlayKey::Down,
            OverlayKey::PageUp,
            OverlayKey::PageDown,
            OverlayKey::Home,
            OverlayKey::End,
            OverlayKey::Char('f'),
            OverlayKey::Char('F'),
            OverlayKey::Char('m'),
        ] {
            assert!(!viewer_key(&key, ""), "{key:?} 应交给 TUI");
        }
        reset();
    }

    /// 类型编辑器：只看标题 + 文件路径（顶部不再有提示行；与正文间的空行由渲染层补）。
    #[test]
    fn type_edit_view_is_title_plus_path_only() {
        let view = type_edit_view(
            "general-purpose",
            &Some(PathBuf::from("/tmp/agents/general-purpose.md")),
            "---\nname: general-purpose\n---\nbody",
            7,
        );
        assert_eq!(view.title, "Edit agent: general-purpose");
        assert!(view.lines.is_empty(), "不应再有顶部提示行");
        let ed = view.editor.expect("应有编辑器");
        assert_eq!(ed.label, "/tmp/agents/general-purpose.md");
        assert!(ed.value.contains("body"));
        assert_eq!(ed.rows, 0, "全屏填满剩余空间");
        assert_eq!(ed.generation, 7);
        assert_eq!(view.footer.len(), 2, "键位只在底栏给出");
    }

    #[test]
    fn schedules_enter_browses_and_only_d_cancels() {
        // 回归：Enter 曾经也会取消任务，用户只是想“打开看看”就会误删。
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        schedule::reset();
        crate::extensions::subagent::session::reset();
        crate::extensions::subagent::session::set_session_key(Some("fleet-sched-test".into()));
        schedule::sync_session();

        let job = schedule::add(schedule::NewJob {
            name: "nightly audit".to_string(),
            description: "nightly audit".to_string(),
            schedule: "5m".to_string(),
            subagent_type: "general-purpose".to_string(),
            prompt: "do the thing".to_string(),
            model: None,
            thinking: None,
            max_turns: None,
        })
        .expect("add");
        lock().sched_selected = 0;

        assert!(schedules_key(&OverlayKey::Enter), "Enter 应被吞掉");
        assert_eq!(schedule::list().len(), 1, "Enter 不应取消任务");

        assert!(schedules_key(&OverlayKey::Char('d')), "d 取消");
        assert!(schedule::list().is_empty(), "d 应取消任务");
        assert!(!schedule::cancel(&job.id), "任务已取消");

        schedule::reset();
        crate::extensions::subagent::session::reset();
    }

    /// 定时任务列表：与其它列表一样两端环绕。
    ///
    /// TUI 侧把 Ctrl+J/K 翻成 [`OverlayKey::Down`]/[`Up`]（见
    /// `modes::interactive::handlers::overlay::to_overlay_key`），所以
    /// Ctrl+J/K 自动继承这里的环绕行为。
    #[test]
    fn schedules_list_navigation_wraps() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        schedule::reset();
        crate::extensions::subagent::session::reset();
        crate::extensions::subagent::session::set_session_key(Some("fleet-sched-wrap".into()));
        schedule::sync_session();

        let job = |name: &str| schedule::NewJob {
            name: name.to_string(),
            description: name.to_string(),
            schedule: "5m".to_string(),
            subagent_type: "general-purpose".to_string(),
            prompt: "do the thing".to_string(),
            model: None,
            thinking: None,
            max_turns: None,
        };
        schedule::add(job("first")).expect("add first");
        schedule::add(job("second")).expect("add second");
        let total = schedule::list().len();
        assert_eq!(total, 2, "两个任务在列表里");

        lock().sched_selected = 0;
        assert!(schedules_key(&OverlayKey::Up), "Up 被消费");
        assert_eq!(lock().sched_selected, total - 1, "首项 ↑ 环绕到末项");
        assert!(schedules_key(&OverlayKey::Down), "Down 被消费");
        assert_eq!(lock().sched_selected, 0, "末项 ↓ 环绕到首项");
        assert!(schedules_key(&OverlayKey::Down));
        assert_eq!(lock().sched_selected, 1, "中间项正常下移");

        schedule::reset();
        crate::extensions::subagent::session::reset();
    }

    #[test]
    fn overlay_open_close_and_unknown_target_paths() {
        // 注册表是进程级全局态：与其它触碰注册表的测试串行（含本 crate 的 manager 测试）
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        manager::reset_all();
        reset();
        crate::core::extensions::register_extension(SuperExt);

        // 未打开时不认领任何 id
        assert!(overlay_view(1, 100).is_none());
        assert!(!on_overlay_event(1, &OverlayEvent::Closed));

        open_fleet();
        let id = lock().overlay_id.expect("打开后有 id");
        assert_eq!(
            crate::core::extensions::overlay_view(id, 100).map(|v| v.title),
            Some("Sub-agents · 0 active · 0 total".to_string()),
            "注册表中应有本扩展认领"
        );
        // ↑ 到顶不越界、↓ 无记录不越界
        assert!(fleet_key(&OverlayKey::Up));
        assert_eq!(selected_index(), 0);
        assert!(fleet_key(&OverlayKey::Down));
        assert_eq!(selected_index(), 0);
        // Enter 无记录：无副作用（不 panic、不打开查看器）
        assert!(fleet_key(&OverlayKey::Enter));
        assert!(viewer_target_id().is_none());

        // Closed 回调清状态
        assert!(on_overlay_event(id, &OverlayEvent::Closed));
        assert!(overlay_view(id, 100).is_none());
        assert!(!is_open());

        // 查看器目标不存在 → 只提示，不打开
        open_viewer("deadbeef");
        assert!(!is_open());
        manager::reset_all();
        crate::core::extensions::unregister_extension(super::super::EXT);
    }

    /// 注册表分发用的薄壳（把 `Subagent` 的覆盖层钩子单独暴露给本模块测试）。
    struct SuperExt;

    impl crate::core::extensions::Extension for SuperExt {
        fn name(&self) -> &str {
            super::super::EXT
        }
        fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
            Vec::new()
        }
        fn hooks(&self) -> Vec<crate::core::extensions::ExtensionHook> {
            vec![crate::core::extensions::ExtensionHook::Overlay]
        }
        fn overlay_view(&self, id: u64, _cols: u16) -> Option<OverlayView> {
            overlay_view(id, 100)
        }
        fn on_overlay_event(&self, id: u64, ev: &OverlayEvent) -> bool {
            on_overlay_event(id, ev)
        }
    }

    /// FleetView 合并 live 工作流运行，Enter 进检查器。
    #[test]
    fn fleet_includes_workflow_rows_and_opens_inspector() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        manager::reset_all();
        super::super::workflow::task::reset_all();
        reset();

        let run_id = "wf_fleet_test";
        let _abort =
            super::super::workflow::task::register(run_id, "demo", "desc", None, None, None);
        let v = fleet_view(0);
        let rendered: Vec<String> = v.lines.iter().map(text).collect();
        assert!(
            rendered.iter().any(|l| l.contains("workflow demo")),
            "{rendered:?}"
        );
        assert!(v.selected.is_some());

        // 工作流行是第 0 行（无 agent）：Enter 进检查器
        lock().selected = 0;
        assert!(fleet_key(&OverlayKey::Enter));
        assert_eq!(lock().kind, Some(OverlayKind::Inspector));
        assert_eq!(lock().ins_target.as_deref(), Some(run_id));

        super::super::workflow::task::reset_all();
        reset();
    }

    /// 列表导航环绕：两端都可以循环（不夹在边界）。
    #[test]
    fn wrap_index_cycles_at_both_ends() {
        assert_eq!(wrap_index(0, -1, 3), 2, "首项 ↑ 环绕到末项");
        assert_eq!(wrap_index(2, 1, 3), 0, "末项 ↓ 环绕到首项");
        assert_eq!(wrap_index(1, 1, 3), 2, "中间项正常移动");
        assert_eq!(wrap_index(1, -1, 3), 0);
        assert_eq!(wrap_index(0, -1, 0), 0, "空列表不 panic");
        assert_eq!(wrap_index(5, 1, 0), 0);
    }

    /// 工作流运行列表：到达底部再按 ↓ 回首项、顶部再按 ↑ 回末项。
    #[test]
    fn workflow_list_navigation_wraps() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        manager::reset_all();
        super::super::workflow::task::reset_all();
        reset();

        let _a = super::super::workflow::task::register("wf_wrap_a", "a", "d", None, None, None);
        let _b = super::super::workflow::task::register("wf_wrap_b", "b", "d", None, None, None);
        let total = super::super::workflow::task::list().len();
        assert_eq!(total, 2, "两个运行在列表里");

        lock().wf_selected = 0;
        assert!(workflows_key(&OverlayKey::Up));
        assert_eq!(lock().wf_selected, total - 1, "首项 ↑ 环绕到末项");
        assert!(workflows_key(&OverlayKey::Down));
        assert_eq!(lock().wf_selected, 0, "末项 ↓ 环绕到首项");
        assert!(workflows_key(&OverlayKey::Down));
        assert_eq!(lock().wf_selected, 1, "中间项正常下移");

        super::super::workflow::task::reset_all();
        reset();
    }

    /// 覆盖层导航只认 ↑↓：Ctrl+J/K 由 TUI 翻成 [`OverlayKey::Down`]/[`Up`] 后到达，
    /// 普通 `j`/`k` 不应也能翻页（要与 `/agents types` 等选择面板一致）。
    #[test]
    fn plain_j_k_are_not_navigation_in_overlays() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        manager::reset_all();
        super::super::workflow::task::reset_all();
        reset();

        let _a = super::super::workflow::task::register("wf_jk_a", "a", "d", None, None, None);
        let _b = super::super::workflow::task::register("wf_jk_b", "b", "d", None, None, None);
        assert_eq!(super::super::workflow::task::list().len(), 2);

        // 运行列表：普通 j/k 不被消费，也不移动选中项
        lock().wf_selected = 0;
        assert!(!workflows_key(&OverlayKey::Char('j')), "普通 j 不是导航键");
        assert!(!workflows_key(&OverlayKey::Char('k')), "普通 k 不是导航键");
        assert_eq!(lock().wf_selected, 0, "普通 j/k 不应移动选中项");
        assert!(workflows_key(&OverlayKey::Down), "↓（含 Ctrl+J）应导航");
        assert_eq!(lock().wf_selected, 1);

        // 检查器：普通 j/k 同样不被消费
        open_inspector("wf_jk_a");
        let before = lock().ins.selected;
        assert!(!inspector_key(&OverlayKey::Char('j')));
        assert!(!inspector_key(&OverlayKey::Char('k')));
        assert_eq!(lock().ins.selected, before, "普通 j/k 不应移动检查器选中行");
        assert!(inspector_key(&OverlayKey::Down), "↓ 应导航");
        assert!(inspector_key(&OverlayKey::Up), "↑ 应导航");

        super::super::workflow::task::reset_all();
        reset();
    }

    /// Fleet 列表：同样两端环绕。
    #[test]
    fn fleet_list_navigation_wraps() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _g = manager::test_lock();
        manager::reset_all();
        super::super::workflow::task::reset_all();
        reset();

        let _a = super::super::workflow::task::register("wf_wrap_c", "c", "d", None, None, None);
        let _b = super::super::workflow::task::register("wf_wrap_d", "d", "d", None, None, None);
        assert_eq!(fleet_row_count(), 2);

        lock().selected = 0;
        assert!(fleet_key(&OverlayKey::Up));
        assert_eq!(lock().selected, fleet_row_count() - 1, "首项 ↑ 环绕");
        assert!(fleet_key(&OverlayKey::Down));
        assert_eq!(lock().selected, 0, "末项 ↓ 环绕");

        super::super::workflow::task::reset_all();
        reset();
    }
}

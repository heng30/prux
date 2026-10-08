//! `hang-detect` 扩展：会话假死（长时间无活动）检测与自动恢复。
//!
//! **问题**：provider 网络卡住、流式响应中断、工具挂死等情况下，一个 run 会长时间
//! 不再产生任何 agent 事件；TUI 一直显示「Working...」，用户只能手动 Esc。
//!
//! **本扩展**：给「静默」设一个上限。run 进行中若超过 `timeoutSecs`（等待模型）或
//! `toolTimeoutSecs`（工具执行中）没有任何 agent 事件，就认定假死：
//!
//! 1. 经 [`ExtensionUiRequest::RestartRun`] 请求 TUI **中止当前回合并立即重开一轮**——
//!    新一轮的 user 批次由「恢复引导消息 + 运行中 steer 注入箱」合成（follow-up 保持
//!    「settle 后才发送」的语义，留在队列里不动），排队 steer 不因假死丢失；
//! 2. 连续恢复超过 `maxRecoveries` 次仍假死时，退化为普通 [`ExtensionUiRequest::AbortRun`]
//!    （只中止、不重启），避免对永久故障无限重启空烧 token。
//!
//! **活动信号**：任何 agent 事件（`agent_start` / `turn_start` / `message_*` /
//! `tool_execution_*` / `turn_end` …）都算活动并刷新计时；`auto_retry_start` 的退避等待
//! 也按 `delayMs` 顺延，不误判为假死。
//!
//! **计时心跳**：看门狗跑在一条**独立的后台线程**里，每 [`WATCHDOG_INTERVAL`] 唤醒一次
//! 检查静默是否超时。不依赖 TUI 事件循环，也不与其它扩展的渲染动画（`wants_redraw` 的
//! `any()` 会短路）争抢调用时机。
//!
//! **线程生死**：线程随扩展启用状态动态启停（不用 `OnceLock` 一次性启动）：
//! 注册时若扩展已启用则拉起；启用时（未启动才）首次启动，禁用时唤醒并退出线程，
//! 不留后台空转。
//!
//! **上下电**：`agent_start` 上电、`agent_end` / `agent_settled` 断电；
//! 会话开始 / 切换时整段复位。`maxRecoveries` 计数在**正常结算**
//! （`agent_settled`）时清零，因此只有「连续假死且始终没能成功结算」才会耗尽上限。
//!
//! 配置持久化在 `agent_dir()/extensions/hang-detect.json`（首次加载自动创建并回填缺省键）；
//! `/hang-detect`（等价 `/hang-detect status`）查看状态与配置，
//! `/hang-detect settings` 打开扩展设置面板，
//! `/hang-detect timeout|tool-timeout|recoveries <n>` 写入任意数值，`/hang-detect reset` 清状态。
//!
//! 实现拆分为两个子模块：
//! - [`config`]：配置缺省 / 校验、面板档位映射与落盘；
//! - 本文件：看门狗状态机、事件处理、UI 请求与扩展主体。

mod config;

use crate::{
    core::{
        extensions::{
            self, Extension, ExtensionCommand, ExtensionHook, ExtensionMode, ExtensionSetting,
            ExtensionTool, ExtensionUiRequest, SubcommandDef, UiNotifyLevel,
        },
        provider::AgentMessage,
        session_manager::Session,
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_HANG_DETECT, command_arg, util::notify_text,
    },
    modes::interactive::{app::App, handlers::register_slash_command},
};
use serde_json::Value;
use std::{
    sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use config::*;

/// 看门狗轮询间隔：每 0.5s 唤醒一次检查静默是否超时（相对超时值，至多 0.5s 过冲）。
const WATCHDOG_INTERVAL: Duration = Duration::from_millis(500);

/// `/hang-detect` 子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "status",
        description: "Print watchdog state and current config (default when no subcommand is given)",
    },
    SubcommandDef {
        name: "settings",
        description: "Open the hang-detect settings panel",
    },
    SubcommandDef {
        name: "timeout",
        description: "Set idle timeout seconds (0 disables): /hang-detect timeout 180",
    },
    SubcommandDef {
        name: "tool-timeout",
        description: "Set tool-execution timeout seconds (0 = off): /hang-detect tool-timeout 600",
    },
    SubcommandDef {
        name: "recoveries",
        description: "Set max consecutive recoveries (0 = unlimited): /hang-detect recoveries 3",
    },
    SubcommandDef {
        name: "reset",
        description: "Clear watchdog state (recovery counter and timers)",
    },
];

/// 自声明工厂：linkme 分布式切片
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static HANG_DETECT_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_HANG_DETECT,
    make: || -> Arc<dyn Extension> { Arc::new(HangDetect) },
};

/// 恢复引导消息（新一轮批次的队首），作为 user 消息进入会话历史。
fn restart_message(secs: u64) -> AgentMessage {
    AgentMessage::user_text(&format!(
        "[hang-detect] The previous turn stalled: no model or tool activity for {secs}s, \
         so it was aborted. Continue from where you left off."
    ))
}

/// 看门狗状态（进程级单例）。
struct State {
    /// 运行期配置（缺省齐全）
    cfg: Config,
    /// 当前是否有 run 在跑（`agent_start` 上电，`agent_end` / `agent_settled` 断电）
    running: bool,
    /// 最近一次活动时间（任何 agent 事件 / 上电；`auto_retry_start` 按退避顺延）
    last_activity: Instant,
    /// 是否有工具正在执行（`tool_execution_start` 未见 `end`）
    tool_running: bool,
    /// 是否已就本次静默发起过恢复（防同一停顿重复入队；上电 / 断电时复位）
    fired: bool,
    /// 连续恢复次数（正常结算时清零）
    recoveries: u32,
}

impl Default for State {
    /// 缺省状态：从配置文件（含缺省值合并）载入数值项，未运行、未触发、恢复计数为 0。
    fn default() -> Self {
        State {
            cfg: merge_config(None),
            running: false,
            last_activity: Instant::now(),
            tool_running: false,
            fired: false,
            recoveries: 0,
        }
    }
}

impl State {
    /// 当前生效的静默上限：工具执行中读 `toolTimeoutSecs`，否则读 `timeoutSecs`；
    /// 对应键为 `0` 时返回 `None`（该场景不检查）。
    fn limit(&self) -> Option<Duration> {
        let key = if self.tool_running {
            "toolTimeoutSecs"
        } else {
            "timeoutSecs"
        };
        let secs = self.cfg.int(key);
        (secs > 0).then(|| Duration::from_secs(secs as u64))
    }

    /// 上电：run 开始（`agent_start`）。
    fn arm(&mut self) {
        self.running = true;
        self.fired = false;
        self.tool_running = false;
        self.last_activity = Instant::now();
    }

    /// 断电：run 结束（`agent_end` / `agent_settled`）。
    fn disarm(&mut self) {
        self.running = false;
        self.fired = false;
        self.tool_running = false;
    }

    /// 刷新活动时间（仅在跑动时）。
    fn touch(&mut self) {
        if self.running {
            self.last_activity = Instant::now();
        }
    }
}

/// 取运行期假死检测状态单例（首次访问时按缺省配置惰性初始化）。
fn state() -> &'static Mutex<State> {
    /// 运行期假死检测状态（上电标志、上次活动时刻、恢复计数）的进程级单例。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 取状态锁；锁中毒（持锁线程 panic）时取回内部数据继续用，不向外传播 panic。
fn lock_state() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// 复位运行期状态（保留配置）。
fn reset_state() {
    let mut st = lock_state();
    st.disarm();
    st.recoveries = 0;
    st.last_activity = Instant::now();
}

/// 本次停顿是否已达上限；是则返回生效的上限时长（用于文案）。
///
/// 纯判定（不修改状态），便于测试注入 `now`。
fn stalled_for(st: &State, now: Instant) -> Option<Duration> {
    if !st.running || st.fired {
        return None;
    }
    let limit = st.limit()?;
    (now.saturating_duration_since(st.last_activity) >= limit).then_some(limit)
}

/// 恢复动作：未超上限 → 中断并重开；超上限 → 只中断。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// 恢复次数未超 maxRecoveries：中断卡住的回合并重新开跑（重发排队的 steer）
    Restart,
    /// 恢复次数已超上限：只中断回合结束，不再自动重开
    Abort,
}

/// 按已恢复次数决定动作：`max == 0`（不限次）或未超上限 → 重开，超上限 → 只中断。
fn action_for(recoveries: u32, max: usize) -> Action {
    if max == 0 || recoveries <= max as u32 {
        Action::Restart
    } else {
        Action::Abort
    }
}

/// 心跳：检查静默是否超时，命中即发恢复请求。
///
/// 由后台看门狗线程周期调用（测试也可直接调）。
/// 只在判定为真时改状态并入队（`fired` 防止同一停顿重复入队）。
fn heartbeat() {
    let mut st = lock_state();
    let Some(limit) = stalled_for(&st, Instant::now()) else {
        return;
    };

    st.fired = true;
    st.recoveries += 1;
    let recoveries = st.recoveries;
    let max = st.cfg.int("maxRecoveries");
    let tool = st.tool_running;
    drop(st);

    let secs = limit.as_secs();
    let what = if tool { "tool" } else { "model" };

    match action_for(recoveries, max) {
        Action::Restart => {
            notify_text(
                &format!(
                    "hang-detect: no {what} activity for {secs}s — aborted the stalled turn and \
                     restarting (queued steer resent)"
                ),
                UiNotifyLevel::Warning,
            );
            extensions::request_ui(ExtensionUiRequest::RestartRun {
                message: Box::new(restart_message(secs)),
            });
        }
        Action::Abort => {
            notify_text(
                &format!(
                    "hang-detect: still stalled after {recoveries} recoveries (no {what} activity \
                     for {secs}s) — aborting without restart"
                ),
                UiNotifyLevel::Warning,
            );
            extensions::request_ui(ExtensionUiRequest::AbortRun);
        }
    }
}

/// 看门狗后台线程的运行时控制块（进程级单例）。
///
/// 线程需要随扩展启用/禁用动态启停，`handle == Some` 即线程在跑，`active` 是线程的退出/继续标志。
struct Watchdog {
    /// 线程是否应继续轮询（禁用后置 `false`，线程唤醒即退出）
    active: bool,
    /// 线程句柄；`None` = 未运行（未启动 / 已退出并回收）
    handle: Option<JoinHandle<()>>,
}

/// 控制块 + 唤醒条件变量，同一把锁保护，供启停与线程休眠共用。
fn watchdog() -> &'static (Mutex<Watchdog>, Condvar) {
    /// 看门狗线程的控制块与唤醒条件变量，进程级单例，供启停与线程休眠共用。
    static W: OnceLock<(Mutex<Watchdog>, Condvar)> = OnceLock::new();
    W.get_or_init(|| {
        (
            Mutex::new(Watchdog {
                active: false,
                handle: None,
            }),
            Condvar::new(),
        )
    })
}

/// 线程体：休眠 [`WATCHDOG_INTERVAL`]，被禁用唤醒后立即退出。
fn watchdog_loop() {
    let (lock, cvar) = watchdog();
    let mut wd = lock.lock().unwrap_or_else(|e| e.into_inner());

    while wd.active {
        let (guard, _timeout) = cvar
            .wait_timeout(wd, WATCHDOG_INTERVAL)
            .unwrap_or_else(|e| e.into_inner());

        wd = guard;

        if wd.active {
            heartbeat();
        }
    }
}

/// 启动后台看门狗线程（幂等：已在跑则仅确保 `active`）。
fn start_watchdog() {
    let (lock, _cvar) = watchdog();
    let mut wd = lock.lock().unwrap_or_else(|e| e.into_inner());
    wd.active = true;

    if wd.handle.is_some() {
        return; // 线程已在跑
    }

    match std::thread::Builder::new()
        .name("hang-detect".to_string())
        .spawn(watchdog_loop)
    {
        Ok(h) => wd.handle = Some(h),
        Err(_) => wd.active = false, // 拉起失败：不留下“在跑”的假象
    }
}

/// 退出后台看门狗线程（幂等：未跑则仅确保 `active == false`）。
///
/// 禁用后调用：置位并唤醒休眠中的线程，等它退出（唤醒即退，几乎无等待）。
fn stop_watchdog() {
    let (lock, cvar) = watchdog();
    let handle = {
        let mut wd = lock.lock().unwrap_or_else(|e| e.into_inner());
        wd.active = false;
        cvar.notify_all();
        wd.handle.take()
    };

    // 锁外 join（线程退出需重新拿这把锁）
    if let Some(h) = handle {
        _ = h.join();
    }
}

/// `hang-detect` 扩展。
pub struct HangDetect;

impl Extension for HangDetect {
    /// 扩展名 `hang-detect`（诊断用）。
    fn name(&self) -> &str {
        EXT
    }

    /// /extension 面板详情里的英文用途说明：监视无 agent 活动的会话并中断重开。
    fn description(&self) -> &str {
        "Watchdog for stalled sessions: abort and restart a run that produces no agent activity \
         for longer than the configured timeout, resending queued steer/follow-up messages"
    }

    /// 声明 `Dev` / `Creator` 两个并排模式：`Minimal` 下不可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认不启用：会自动中断并重开回合，属于改变默认 agentic 行为的能力。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供模型工具：看门狗只被动监听事件、主动中断，不需要模型调用。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 只声明 `/hang-detect` 一个斜杠命令（打印状态 / 打开设置面板 / 改配置）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description:
                "Print watchdog status and config (default); /hang-detect settings opens the \
                 panel; status | timeout <s> | tool-timeout <s> | recoveries <n> | reset"
                    .to_string(),
            busy_safe: true, // handler 只读写扩展状态 / 打开设置面板，不锁 agent，忙碌安全
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 面板设置项：预设档位 + Space/Enter 循环（无自由输入）。
    fn settings(&self) -> Vec<ExtensionSetting> {
        let st = lock_state();
        panel_settings(&st.cfg)
    }

    /// 应用面板选择并立即落盘（面板是"设置"语义）。
    fn apply_setting(&self, key: &str, value: &str) -> std::result::Result<(), String> {
        apply_panel_choice(&mut lock_state().cfg, key, value)?;

        let path = config_path();
        let cfg = lock_state().cfg.clone();
        write_config(&path, &cfg);
        Ok(())
    }

    /// 把 `/hang-detect` 的执行入口接线到 TUI 命令注册表。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_hang_detect);
    }

    /// 启用时物化配置文件并拉起看门狗线程（未启动才首次启动）；
    /// 禁用时退出看门狗线程并复位运行期状态。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            lock_state().cfg = load_config();
            start_watchdog();
        } else {
            stop_watchdog();
            reset_state();
        }
    }

    /// 始终订阅 agent 事件：上电信号 `agent_start` 本身就在这个面上，
    /// 若按运行态门控则永远收不到启动信号（对齐 loop-detect 的取舍）。
    /// 空闲时没有事件流入，因此没有额外开销；跑动中每帧事件只刷新一个时间戳。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::AgentEvent]
    }

    /// 按事件类型刷新看门狗状态：上/断电、工具起止、活动时间戳；
    /// `auto_retry_start` 按退避时长顺延活动时间（退避等待不算假死）。
    fn on_agent_event(&self, event: &Value) {
        let ty = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let mut st = lock_state();
        match ty {
            "agent_start" => st.arm(),
            "agent_end" => st.disarm(),
            "agent_settled" => {
                st.disarm();
                st.recoveries = 0; // 正常结算 = 一轮完整走完，恢复计数清零（下次假死重新计数）
            }
            "tool_execution_start" => {
                // 嵌套调用（带 parentToolCallId）不改变“工具在跑”的总体状态：
                // 外层工具本来就还在执行，中间层结束也不应该把总状态清掉。
                if !extensions::is_nested_call_event(event) {
                    st.tool_running = true;
                }
                st.touch();
            }
            "tool_execution_end" => {
                if !extensions::is_nested_call_event(event) {
                    st.tool_running = false;
                }
                st.touch();
            }
            "auto_retry_start" => {
                // 退避等待不是假死：把活动时间按 delayMs 顺延到退避结束
                if st.running {
                    let delay = event.get("delayMs").and_then(|v| v.as_u64()).unwrap_or(0);
                    st.last_activity = Instant::now() + Duration::from_millis(delay);
                }
            }
            _ => st.touch(),
        }
    }

    /// 会话加载时复位运行期状态（保留配置），避免沿用上个会话的计时与恢复计数。
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], _session: Option<&Session>) {
        reset_state();
    }

    /// 会话切换时同样复位运行期状态（保留配置）。
    fn on_session_switched(&self, _session_path: Option<&str>, _messages: &[AgentMessage]) {
        reset_state();
    }
}

/// `/hang-detect` 命令分发。
fn command_hang_detect(app: &mut App, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((a, b)) => (a, b.trim()),
        None => (arg, ""),
    };

    match sub {
        // 无参数 = `status`（打印状态与配置）
        "" | "status" => {
            let st = lock_state();
            let limit = st
                .limit()
                .map(|d| format!("{}s", d.as_secs()))
                .unwrap_or_else(|| "off".to_string());
            let mut lines = vec![
                "hang-detect status".to_string(),
                format!("  running:            {}", st.running),
                format!("  tool running:       {}", st.tool_running),
                format!("  fired:              {}", st.fired),
                format!("  recoveries:         {}", st.recoveries),
                format!("  current limit:      {limit}"),
                format!(
                    "  silent for:         {}s",
                    st.last_activity.elapsed().as_secs()
                ),
                String::new(),
                "  config (edit hang-detect.json or use the /hang-detect panel):".to_string(),
            ];
            for (key, _) in NUMERIC_DEFAULTS {
                lines.push(format!("    {key}={}", st.cfg.int(key)));
            }
            drop(st);
            notify_text(&lines.join("\n"), UiNotifyLevel::Info);
        }
        "settings" => {
            if app.ext_settings.is_open_for(EXT) {
                app.ext_settings.close();
            } else {
                app.ext_settings.open(EXT);
            }
            app.dirty = true;
        }
        "timeout" | "tool-timeout" | "recoveries" => {
            let key = match sub {
                "timeout" => "timeoutSecs",
                "tool-timeout" => "toolTimeoutSecs",
                _ => "maxRecoveries",
            };
            match rest.parse::<u64>() {
                Ok(n) => {
                    {
                        let mut st = lock_state();
                        set_numeric(&mut st.cfg, key, n as f64);
                    }
                    let path = config_path();
                    let cfg = lock_state().cfg.clone();
                    write_config(&path, &cfg);
                    notify_text(&format!("hang-detect: {key} = {n}"), UiNotifyLevel::Success);
                }
                Err(_) => notify_text(
                    &format!("Usage: /hang-detect {sub} <non-negative integer>"),
                    UiNotifyLevel::Warning,
                ),
            }
        }
        "reset" => {
            reset_state();
            notify_text("hang-detect: state reset", UiNotifyLevel::Info);
        }
        other => notify_text(
            &format!(
                "Unknown /hang-detect subcommand: {other}. Use `status`, `settings`, `timeout`, \
                 `tool-timeout`, `recoveries` or `reset`."
            ),
            UiNotifyLevel::Warning,
        ),
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 与触碰全局注册表 / 配置目录的测试串行，并把进程级看门狗状态复位到缺省。
    fn guard() -> (crate::test_support::AgentDirGuard, MutexGuard<'static, ()>) {
        let g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let ad = crate::test_support::AgentDirGuard::temp();
        *lock_state() = State::default();
        (ad, g)
    }

    fn cfg_with(pairs: &[(&str, i64)]) -> Config {
        let mut cfg = merge_config(None);
        for (k, v) in pairs {
            set_numeric(&mut cfg, k, *v as f64);
        }
        cfg
    }

    #[test]
    fn declares_dev_creator_modes_and_default_disabled() {
        let ext = HangDetect;
        assert!(!ext.default_enabled(), "hang-detect 应默认不启用");
        assert_eq!(
            ext.modes(),
            vec![ExtensionMode::Dev, ExtensionMode::Creator],
            "hang-detect 应在 Dev / Creator 两个模式下可用"
        );
        assert!(ext.hooks().contains(&ExtensionHook::AgentEvent));
        assert!(ext.tools().is_empty());
    }

    /// 启动副作用必须看「有效启用态」：Minimal 模式下即便 settings.json 开着，
    /// 注册/启用也不得启动看门狗线程（否则极简模式后台空转）。
    #[test]
    fn registration_starts_watchdog_only_when_mode_allows() {
        let _g = guard();
        let running = || {
            watchdog()
                .0
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .active
        };

        extensions::set_extension_mode(ExtensionMode::All);
        extensions::unregister_extension(EXT);
        extensions::register_extension(HangDetect);
        extensions::set_extension_enabled(EXT, true);
        assert!(running(), "All 模式下启用应拉起看门狗线程");

        extensions::set_extension_mode(ExtensionMode::Minimal);
        assert!(!running(), "切到 Minimal 模式应停掉看门狗线程");

        // 回归钉死：注册路径不能用 is_extension_enabled（只看持久化开关）
        assert!(extensions::is_extension_enabled(EXT));
        assert!(!extensions::is_extension_active(EXT));
        assert!(extensions::unregister_extension(EXT));
        extensions::register_extension(HangDetect);
        extensions::set_extension_enabled(EXT, true);
        assert!(!running(), "Minimal 模式下注册 / 启用不得拉起看门狗线程");

        extensions::set_extension_mode(ExtensionMode::Dev);
        assert!(running(), "切回 Dev 模式应重新拉起看门狗线程");

        // 还原：停线程、注销扩展、模式回 All
        extensions::set_extension_mode(ExtensionMode::All);
        extensions::set_extension_enabled(EXT, false);
        assert!(extensions::unregister_extension(EXT));
        stop_watchdog();
        crate::core::settings_manager::write_extension_mode("all").ok();
    }

    #[test]
    fn subcommands_match_parser() {
        // 候选元数据与 handler 分支一一对应
        let names: Vec<&str> = SUBCOMMANDS.iter().map(|s| s.name).collect();
        assert_eq!(
            names,
            vec![
                "status",
                "settings",
                "timeout",
                "tool-timeout",
                "recoveries",
                "reset"
            ]
        );
    }

    #[test]
    fn config_merge_fills_defaults_and_rejects_invalid() {
        let cfg = merge_config(Some(&json!({
            "timeoutSecs": 60,
            "toolTimeoutSecs": -5,   // 非法 → 回落缺省
            "maxRecoveries": 1.5,    // 非法 → 回落缺省
        })));
        assert_eq!(cfg.int("timeoutSecs"), 60);
        assert_eq!(cfg.int("toolTimeoutSecs"), 0, "负数回落缺省");
        assert_eq!(cfg.int("maxRecoveries"), 3, "小数回落缺省");
    }

    #[test]
    fn panel_roundtrip_maps_labels_to_numbers() {
        let mut cfg = merge_config(None);
        assert_eq!(panel_value(&cfg, "timeout"), "3m");
        assert_eq!(panel_value(&cfg, "toolTimeout"), "off");
        assert_eq!(panel_value(&cfg, "maxRecoveries"), "3");

        apply_panel_choice(&mut cfg, "timeout", "10m").unwrap();
        assert_eq!(cfg.int("timeoutSecs"), 600);
        apply_panel_choice(&mut cfg, "toolTimeout", "30m").unwrap();
        assert_eq!(cfg.int("toolTimeoutSecs"), 1800);
        apply_panel_choice(&mut cfg, "maxRecoveries", "unlimited").unwrap();
        assert_eq!(cfg.int("maxRecoveries"), 0);

        assert!(apply_panel_choice(&mut cfg, "timeout", "bogus").is_err());
        assert!(apply_panel_choice(&mut cfg, "nope", "off").is_err());
    }

    #[test]
    fn limit_respects_zero_off_and_tool_mode() {
        let mut st = State {
            cfg: cfg_with(&[("timeoutSecs", 180), ("toolTimeoutSecs", 0)]),
            ..State::default()
        };
        assert_eq!(st.limit(), Some(Duration::from_secs(180)));

        st.tool_running = true;
        assert_eq!(st.limit(), None, "工具超时为 0 时工具执行期间不检查");

        st.cfg = cfg_with(&[("timeoutSecs", 180), ("toolTimeoutSecs", 600)]);
        assert_eq!(st.limit(), Some(Duration::from_secs(600)));

        st.tool_running = false;
        st.cfg = cfg_with(&[("timeoutSecs", 0)]);
        assert_eq!(st.limit(), None, "总超时为 0 时看门狗关闭");
    }

    #[test]
    fn stalled_for_requires_running_and_not_fired() {
        let now = Instant::now();
        let mut st = State {
            cfg: cfg_with(&[("timeoutSecs", 60)]),
            running: true,
            last_activity: now - Duration::from_secs(120),
            ..State::default()
        };
        assert_eq!(stalled_for(&st, now), Some(Duration::from_secs(60)));

        // 未跑动：不判
        st.running = false;
        assert_eq!(stalled_for(&st, now), None);

        // 已发起恢复：不重复判
        st.running = true;
        st.fired = true;
        assert_eq!(stalled_for(&st, now), None);

        // 未到阈值：不判
        st.fired = false;
        st.last_activity = now - Duration::from_secs(30);
        assert_eq!(stalled_for(&st, now), None);
    }

    #[test]
    fn action_for_caps_at_max_recoveries() {
        assert_eq!(action_for(1, 3), Action::Restart);
        assert_eq!(action_for(3, 3), Action::Restart);
        assert_eq!(action_for(4, 3), Action::Abort);
        assert_eq!(action_for(99, 0), Action::Restart, "0 = 不设上限");
    }

    #[test]
    fn arm_disarm_and_touch_track_activity() {
        let mut st = State {
            cfg: cfg_with(&[("timeoutSecs", 60)]),
            ..State::default()
        };
        assert!(!st.running);
        st.arm();
        assert!(st.running && !st.fired && !st.tool_running);
        let armed_at = st.last_activity;

        // 非跑动时 touch 无效果
        st.running = false;
        st.touch();
        assert_eq!(st.last_activity, armed_at);
        st.running = true;

        std::thread::sleep(Duration::from_millis(2));
        st.touch();
        assert!(st.last_activity > armed_at, "跑动中 touch 应刷新时间");

        st.fired = true;
        st.disarm();
        assert!(!st.running && !st.fired && !st.tool_running);
    }

    #[test]
    fn events_arm_disarm_and_track_tools() {
        let _g = guard();
        let ext = HangDetect;
        reset_state();

        ext.on_agent_event(&json!({ "type": "agent_start" }));
        {
            let st = lock_state();
            assert!(st.running);
        }

        ext.on_agent_event(&json!({ "type": "tool_execution_start" }));
        assert!(lock_state().tool_running);

        ext.on_agent_event(&json!({ "type": "tool_execution_end" }));
        assert!(!lock_state().tool_running);

        ext.on_agent_event(&json!({ "type": "agent_end" }));
        assert!(!lock_state().running);

        // 恢复计数只在正常结算时清零
        lock_state().recoveries = 2;
        ext.on_agent_event(&json!({ "type": "agent_settled" }));
        assert_eq!(lock_state().recoveries, 0);
        reset_state();
    }

    #[test]
    fn auto_retry_extends_deadline_by_delay() {
        let _g = guard();
        let ext = HangDetect;
        reset_state();

        ext.on_agent_event(&json!({ "type": "agent_start" }));
        ext.on_agent_event(&json!({
            "type": "auto_retry_start",
            "delayMs": 5000u64,
        }));
        let st = lock_state();
        assert!(
            st.last_activity > Instant::now(),
            "退避等待应把活动时间推到未来，避免误判假死"
        );
        drop(st);
        reset_state();
    }

    #[test]
    fn heartbeat_fires_restart_once_then_caps() {
        let _g = guard();
        reset_state();
        while crate::core::extensions::take_pending_ui().is_some() {}

        {
            let mut st = lock_state();
            st.cfg = cfg_with(&[("timeoutSecs", 1), ("maxRecoveries", 1)]);
            st.arm();
            st.last_activity = Instant::now() - Duration::from_secs(5);
        }

        // 第一次命中 → RestartRun
        heartbeat();
        {
            let st = lock_state();
            assert!(st.fired, "命中后应置 fired");
            assert_eq!(st.recoveries, 1);
        }
        let mut saw_restart = false;
        while let Some(req) = crate::core::extensions::take_pending_ui() {
            if matches!(req, ExtensionUiRequest::RestartRun { .. }) {
                saw_restart = true;
            }
        }
        assert!(saw_restart, "应请求 RestartRun");

        // 同一停顿不重复入队
        heartbeat();
        assert!(crate::core::extensions::take_pending_ui().is_none());

        // 重新上电后再静默 → 超过 maxRecoveries → AbortRun
        {
            let mut st = lock_state();
            st.arm();
            st.last_activity = Instant::now() - Duration::from_secs(5);
        }
        heartbeat();
        assert_eq!(lock_state().recoveries, 2);
        let mut saw_abort = false;
        while let Some(req) = crate::core::extensions::take_pending_ui() {
            if matches!(req, ExtensionUiRequest::AbortRun) {
                saw_abort = true;
            }
        }
        assert!(saw_abort, "超过上限应退化为 AbortRun");
        reset_state();
    }

    #[test]
    fn watchdog_thread_starts_and_stops_idempotently() {
        let _g = guard();
        stop_watchdog();
        assert!(!watchdog().0.lock().unwrap().active);

        start_watchdog();
        assert!(watchdog().0.lock().unwrap().handle.is_some(), "应拉起线程");
        start_watchdog(); // 幂等：已在跑不再拉起
        assert!(watchdog().0.lock().unwrap().handle.is_some());

        stop_watchdog();
        let (lock, _) = watchdog();
        let wd = lock.lock().unwrap();
        assert!(wd.handle.is_none(), "停止后句柄应被回收");
        assert!(!wd.active);
    }

    #[test]
    fn on_enabled_changed_toggles_watchdog_thread() {
        let _g = guard();
        stop_watchdog();

        HangDetect.on_enabled_changed(true);
        assert!(
            watchdog().0.lock().unwrap().handle.is_some(),
            "启用应拉起看门狗线程"
        );

        HangDetect.on_enabled_changed(false);
        let (lock, _) = watchdog();
        let wd = lock.lock().unwrap();
        assert!(wd.handle.is_none(), "禁用应退出看门狗线程");
        assert!(!wd.active);
        drop(wd);
        // 再次启用仍能重新拉起（证明用的是动态控制块而非 OnceLock）
        HangDetect.on_enabled_changed(true);
        assert!(watchdog().0.lock().unwrap().handle.is_some());
        stop_watchdog();
    }

    #[test]
    fn restart_message_mentions_timeout() {
        let msg = restart_message(180);
        assert_eq!(msg.role, "user");
        assert!(msg.text().contains("180s"), "got: {}", msg.text());
        assert!(msg.text().contains("hang-detect"));
    }

    /// 取出并拼接所有待处理通知（`Notify` / `NotifyRich`）的纯文本。
    fn drain_notify_text() -> String {
        let mut out = String::new();
        while let Some(req) = crate::core::extensions::take_pending_ui() {
            match req {
                ExtensionUiRequest::Notify { text, .. } => out.push_str(&text),
                ExtensionUiRequest::NotifyRich { spans, .. } => {
                    for s in spans {
                        out.push_str(&s.text);
                    }
                }
                _ => {}
            }
        }
        out
    }

    #[test]
    fn command_status_lists_config() {
        let _g = guard();
        reset_state();
        while crate::core::extensions::take_pending_ui().is_some() {}
        let mut st = App::new();
        assert!(!command_hang_detect(&mut st, "hang-detect status"));
        let text = drain_notify_text();
        assert!(text.contains("hang-detect status"), "got: {text}");
        assert!(text.contains("timeoutSecs=180"), "got: {text}");
    }

    #[test]
    fn command_no_arg_defaults_to_status() {
        let _g = guard();
        reset_state();
        while crate::core::extensions::take_pending_ui().is_some() {}
        let mut st = App::new();
        assert!(!command_hang_detect(&mut st, "hang-detect"));
        let text = drain_notify_text();
        assert!(text.contains("hang-detect status"), "got: {text}");
        assert!(!st.ext_settings.is_open_for(EXT), "无参不再打开面板");
    }

    #[test]
    fn command_settings_opens_panel() {
        let _g = guard();
        reset_state();
        let mut st = App::new();
        assert!(!command_hang_detect(&mut st, "hang-detect settings"));
        assert!(st.ext_settings.is_open_for(EXT), "settings 应打开面板");
    }

    #[test]
    fn command_sets_numeric_values_and_persists() {
        let _g = guard();
        reset_state();
        let mut st = App::new();

        assert!(!command_hang_detect(&mut st, "hang-detect timeout 60"));
        assert_eq!(lock_state().cfg.int("timeoutSecs"), 60);
        assert_eq!(load_config().int("timeoutSecs"), 60, "应落盘");

        assert!(!command_hang_detect(
            &mut st,
            "hang-detect tool-timeout 600"
        ));
        assert_eq!(load_config().int("toolTimeoutSecs"), 600);

        assert!(!command_hang_detect(&mut st, "hang-detect recoveries 5"));
        assert_eq!(load_config().int("maxRecoveries"), 5);
    }

    #[test]
    fn command_rejects_bad_number_and_unknown_subcommand() {
        let _g = guard();
        reset_state();
        while crate::core::extensions::take_pending_ui().is_some() {}
        let mut st = App::new();

        assert!(!command_hang_detect(&mut st, "hang-detect timeout abc"));
        let text = drain_notify_text();
        assert!(text.contains("Usage"), "got: {text}");

        assert!(!command_hang_detect(&mut st, "hang-detect bogus"));
        let text = drain_notify_text();
        assert!(text.contains("Unknown"), "got: {text}");
    }

    #[test]
    fn apply_setting_persists_to_disk() {
        let _g = guard();
        reset_state();
        HangDetect.apply_setting("timeout", "5m").unwrap();
        assert_eq!(load_config().int("timeoutSecs"), 300);
        assert!(HangDetect.apply_setting("nope", "x").is_err());
    }
}

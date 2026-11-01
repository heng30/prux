//! `processes` 扩展：后台进程监督。
//!
//! 内置 `bash` 工具只有「前台执行 + 超时杀进程」，长命令（dev server、watch）
//! 只能占满一个工具调用直到超时被杀。本扩展给模型三个工具加一个用户命令：
//!
//! - [`TOOL_START`]：把一条命令丢到后台跑，立刻返回 `job-N` + pid + 日志路径；
//! - [`TOOL_READ`]：读某个 job 的**输出快照**（不 drain，可反复读），省略 id 时列出全部 job；
//! - [`TOOL_STOP`]：杀整个进程组（Unix `killpg`，Windows `taskkill /T`）；
//! - `/ps`：打开停靠面板看运行中的 job，`/ps stop <id>` 停止，`/ps clear` 清掉已完成记录。
//!
//! **输出去处**：子进程的 stdout/stderr 全部写进 `agent_dir()` 外的临时日志文件（0600），
//! 绝不继承终端——继承会把子进程的横幅与告警糊在 TUI 上。命令 stdin 接 null。
//!
//! **进程组**：Unix 上子进程自成进程组，停止时连同它派生的子进程一起杀，不留孤儿。
//!
//! **生命周期**：job 不随回合结束消失；扩展被禁用时停掉所有运行中的 job（否则工具面消失后
//! 用户再也停不了它们）。prux 退出后仍在跑的 job 保持运行（与「拉起长命应用」同语义），
//! 日志路径留在 `/ps` 面板里可查。
//!
//! 默认**不启用**（`Dev` 模式可用）：它会长期占用系统资源，按需在 `/extension` 面板开启。

use crate::{
    core::{
        self,
        extensions::{
            CommandHost, DockLine, DockSpan, Extension, ExtensionCommand, ExtensionHook,
            ExtensionMode, ExtensionTool, SubcommandDef, ToolExecCtx, UiNotifyLevel,
        },
        tools::{self, ToolError, ToolResult},
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_PROCESSES, command_arg,
        util::{opt_i64, opt_str, req_str, truncate_chars},
    },
    modes::tui::handlers::register_host_command,
    utils::{
        glyphs::{DEF_DOT_EMPTY, DEF_DOT_FILLED},
        output_files::create_output_file_stream,
        process::{kill_process_tree, shell_command},
        time::format_duration,
    },
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::{Duration, Instant},
};

/// 扩展名（也是 `/extension` 面板中的标识）。
const EXT: &str = "processes";

/// 斜杠命令名。
const CMD: &str = "ps";

/// 启动后台命令的工具名。
const TOOL_START: &str = "background_bash";

/// 读输出快照 / 列 job 的工具名。
const TOOL_READ: &str = "read_process_output";

/// 停止 job 的工具名。
const TOOL_STOP: &str = "stop_process";

/// 同时运行的后台 job 上限（超出的启动请求直接报错，先停一个再开）。
const MAX_RUNNING: usize = 64;

/// 保留的已完成 job 记录上限，超出时从最旧的开始丢。
const MAX_FINISHED: usize = 32;

/// 输出快照默认返回的行数。
const DEFAULT_SNAPSHOT_LINES: usize = 100;

/// 输出快照允许的最大行数。
const MAX_SNAPSHOT_LINES: usize = 2000;

/// 输出快照最多从日志尾部读的字节数（更大的日志只给最后一截）。
const MAX_SNAPSHOT_BYTES: u64 = 256 * 1024;

/// `/ps` 子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "list",
        description: "Print the background jobs (default when no subcommand is given)",
    },
    SubcommandDef {
        name: "stop",
        description: "Stop a background job: /ps stop job-1",
    },
    SubcommandDef {
        name: "clear",
        description: "Drop finished jobs from the list",
    },
];

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static PROCESSES_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_PROCESSES,
    make: || -> Arc<dyn Extension> { Arc::new(Processes) },
};

/// 已结束 job 的结局。
struct Finished {
    /// 退出码；被信号杀死时为 None。
    exit_code: Option<i32>,
    /// 从启动到退出的墙钟耗时。
    elapsed: Duration,
}

/// 一个后台 job。
struct Job {
    /// job id（`job-N`，进程内自增，不复用）。
    id: String,
    /// 原样记下的命令行（面板与工具回执展示用）。
    command: String,
    /// 子进程 pid（Unix 上同时是进程组 id）。
    pid: u32,
    /// stdout/stderr 的日志文件路径。
    log: PathBuf,
    /// 启动时刻（算耗时用）。
    started: Instant,
    /// 结束信息；None = 仍在运行。
    finished: Option<Finished>,
    /// 是否已被要求停止（决定面板上显示 `stopped` 还是 `exit N`）。
    stop_requested: bool,
}

/// 进程内常驻的 job 表（进程级单例）。
#[derive(Default)]
struct State {
    /// 已分配的最大 job 序号。
    next: u64,
    /// 全部 job，按启动顺序（面板与列表都按此顺序展示）。
    jobs: Vec<Job>,
    /// 停靠面板的显隐（内存态，不持久化）。
    dock_shown: bool,
}

/// job 表。
fn state() -> &'static Mutex<State> {
    /// job 表（叶级锁：只保护本扩展状态，不外调任何其它组件）。
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

/// 取 job 表的锁；持锁线程 panic 过时取回内部数据继续用。
fn lock() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// 后台进程监督扩展。
pub struct Processes;

impl Extension for Processes {
    /// 返回固定扩展名 `processes`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Background process supervision: run commands in the background, read their output, stop them, /ps panel"
    }

    /// 默认关闭：后台 job 会长期占用系统资源，按需开启。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 仅在 Dev 及以上模式可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 提供启动 / 读输出 / 停止三个工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![start_tool(), read_tool(), stop_tool()]
    }

    /// 声明 `/ps` 斜杠命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "/ps [list|stop <id>|clear] - background jobs".to_string(),
            busy_safe: true, // handler 只碰扩展自身状态与宿主停靠面板，不锁 agent
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 只有面板显示时才订阅 Dock（否则框架每帧空转）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        if lock().dock_shown {
            vec![ExtensionHook::Dock]
        } else {
            Vec::new()
        }
    }

    /// 注册 `/ps` 的命令处理器。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_ps);
    }

    /// 面板显示且仍有 job 在跑时请求每帧重绘（耗时列要动起来）。
    fn wants_redraw(&self) -> bool {
        let st = lock();
        st.dock_shown && st.jobs.iter().any(|j| j.finished.is_none())
    }

    /// 面板内容：标题行 + 每 job 一行（状态点 / id / 状态 / 耗时 / 命令）。
    fn dock_lines(&self) -> Vec<DockLine> {
        dock_lines()
    }

    /// 禁用时停掉所有运行中的 job 并清空记录——工具面消失后再也停不了它们。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            return;
        }
        let pids = {
            let mut st = lock();
            st.dock_shown = false;
            drain_all_running(&mut st)
        };
        for pid in pids {
            kill_process_tree(pid);
        }
    }

    /// 按工具名分发；未知工具名返回 `ToolError`。
    fn execute_tool_async(
        &self,
        name: String,
        args: Value,
        ctx: ToolExecCtx,
    ) -> BoxFuture<'static, Result<ToolResult, ToolError>> {
        Box::pin(async move {
            match name.as_str() {
                TOOL_START => start(&args, &ctx.cwd),
                TOOL_READ => read(&args),
                TOOL_STOP => stop(&args),
                other => Err(ToolError(format!("extension {EXT}: unknown tool: {other}"))),
            }
        })
    }
}

/// 取出全部运行中 job 的 pid 并清空 job 表（禁用时用）。
fn drain_all_running(st: &mut State) -> Vec<u32> {
    let jobs = std::mem::take(&mut st.jobs);
    jobs.into_iter()
        .filter(|j| j.finished.is_none())
        .map(|j| j.pid)
        .collect()
}

/// 丢掉超出保留上限的最旧的已完成 job。
fn prune(st: &mut State) {
    let finished = st.jobs.iter().filter(|j| j.finished.is_some()).count();
    if finished <= MAX_FINISHED {
        return;
    }
    let mut excess = finished - MAX_FINISHED;
    st.jobs.retain(|j| {
        if excess > 0 && j.finished.is_some() {
            excess -= 1;
            false
        } else {
            true
        }
    });
}

/// `background_bash` 工具定义。
fn start_tool() -> ExtensionTool {
    ExtensionTool {
        label: Some("Background Bash".to_string()),
        snippet: "background_bash: run a command in the background and return its job id".to_string(),
        prompt_guidelines: vec![
            "Use background_bash for long-running commands (dev servers, watchers, tail -f) and return control immediately; the built-in bash tool would block the turn until it times out.".to_string(),
            "Poll a job with read_process_output instead of sleeping; each call returns an output snapshot you can read again.".to_string(),
            "Stop a job with stop_process when you are done with it — background jobs keep running after the turn ends.".to_string(),
        ],
        ..ExtensionTool::simple(
            TOOL_START,
            "Start a shell command in the background and return immediately with a job id, its pid and the log path. \
             stdin is /dev/null; stdout and stderr go to the log file. The command keeps running after the turn ends \
             until it exits or you call stop_process.",
            json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "Shell command line to run in the background."
                    },
                    "cwd": {
                        "type": "string",
                        "description": "Working directory; defaults to the session working directory."
                    }
                },
                "required": ["command"]
            }),
            "Run a command in the background (returns job id)",
        )
    }
}

/// `read_process_output` 工具定义。
fn read_tool() -> ExtensionTool {
    ExtensionTool {
        label: Some("Read Process Output".to_string()),
        snippet: "read_process_output: snapshot of a background job's output".to_string(),
        ..ExtensionTool::simple(
            TOOL_READ,
            "Return a snapshot of a background job's output: status, pid, elapsed time, log path and the \
             last N lines of its log. Reading is not destructive — call it again to see new output. \
             Omit `id` to list all jobs.",
            json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Job id (e.g. job-1). Omit to list all jobs."
                    },
                    "lines": {
                        "type": "integer",
                        "description": "How many trailing log lines to return (default 100, max 2000)."
                    }
                }
            }),
            "Read a background job's output snapshot",
        )
    }
}

/// `stop_process` 工具定义。
fn stop_tool() -> ExtensionTool {
    ExtensionTool {
        label: Some("Stop Process".to_string()),
        snippet: "stop_process: kill a background job and its children".to_string(),
        ..ExtensionTool::simple(
            TOOL_STOP,
            "Stop a background job: kill its whole process group (children included) and report the result. \
             Already finished jobs are reported as such instead of being killed again.",
            json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "Job id to stop (e.g. job-1)."
                    }
                },
                "required": ["id"]
            }),
            "Stop a background job and its children",
        )
    }
}

/// 平台 shell：优先用 bash（与内置 `bash` 工具同一份查找逻辑），找不到才退回 `sh` / `cmd`。
///
/// shortcut: 不读 `settings.shellPath`（`resolve_shell` 在核心 `bash` 模块内是私有），
/// 等到「后台 job 的 shell 与内置 bash 不一致」被报上来再打通。
fn shell_cmd(command: &str) -> Command {
    match tools::find_bash() {
        Some(bash) => {
            let mut cmd = Command::new(bash);
            cmd.arg("-c").arg(command);
            cmd
        }
        None => shell_command(command),
    }
}

/// `background_bash`：起一个后台 job，立刻返回回执。
fn start(args: &Value, default_cwd: &str) -> Result<ToolResult, ToolError> {
    let command = req_str(args, "command")?;
    let cwd = opt_str(args, "cwd").unwrap_or(default_cwd);
    Ok(ToolResult::text(start_job(&command, cwd)?))
}

/// 启动后台 job 并登记；返回给模型的回执文本。
fn start_job(command: &str, cwd: &str) -> Result<String, ToolError> {
    let command = command.trim();
    if command.is_empty() {
        return Err(ToolError("`command` must not be empty".to_string()));
    }

    let dir = if cwd.trim().is_empty() {
        std::env::current_dir()
            .map_err(|e| ToolError(format!("cannot resolve working directory: {e}")))?
    } else {
        PathBuf::from(cwd)
    };
    if !dir.is_dir() {
        return Err(ToolError(format!(
            "working directory does not exist: {}",
            dir.display()
        )));
    }

    let (id, pid, log) = {
        let mut st = lock();
        prune(&mut st);
        let running = st.jobs.iter().filter(|j| j.finished.is_none()).count();
        if running >= MAX_RUNNING {
            return Err(ToolError(format!(
                "too many background jobs running ({running}/{MAX_RUNNING}); stop one with {TOOL_STOP} first"
            )));
        }
        let (log_file, log) = create_output_file_stream("process", ".log")
            .map_err(|e| ToolError(format!("cannot create the output log file: {e}")))?;
        let id = next_id(&mut st);
        let (pid, child) = spawn(command, &dir, log_file)?;
        st.jobs.push(Job {
            id: id.clone(),
            command: command.to_string(),
            pid,
            log: log.clone(),
            started: Instant::now(),
            finished: None,
            stop_requested: false,
        });
        st.dock_shown = true;
        reap(child, pid);
        (id, pid, log)
    };

    core::extensions::request_show_dock();
    Ok(format!(
        "Started {id} (pid {pid}) in {}\nLog: {}",
        dir.display(),
        log.display()
    ))
}

/// 分配下一个 job id（调用方持锁）。
fn next_id(st: &mut State) -> String {
    st.next += 1;
    format!("job-{}", st.next)
}

/// 拉起子进程：输出重定向到日志，Unix 上另起进程组，stdin 接 null。
fn spawn(
    command: &str,
    dir: &Path,
    log_file: File,
) -> Result<(u32, std::process::Child), ToolError> {
    let stderr = log_file
        .try_clone()
        .map_err(|e| ToolError(format!("cannot create the output log file: {e}")))?;
    let mut cmd = shell_cmd(command);
    cmd.current_dir(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_file))
        .stderr(Stdio::from(stderr));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let child = cmd
        .spawn()
        .map_err(|e| ToolError(format!("failed to start the command: {e}")))?;
    let pid = child.id();
    Ok((pid, child))
}

/// 回收线程：等子进程退出后把结局写回 job 表（job 已被清掉时静默丢弃）。
///
/// 按 **pid** 定位而不是 job id：测试与「清空后重开」会让同一个 `job-N` 换个进程，
/// 只认 id 会把旧进程的结局写到新 job 上。
fn reap(mut child: std::process::Child, pid: u32) {
    std::thread::spawn(move || {
        let exit_code = child.wait().ok().and_then(|s| s.code());
        let mut st = lock();
        if let Some(job) = st.jobs.iter_mut().find(|j| j.pid == pid) {
            job.finished = Some(Finished {
                exit_code,
                elapsed: job.started.elapsed(),
            });
        }
    });
}

/// `read_process_output`：省略 id 时列全部 job，否则给该 job 的输出快照。
fn read(args: &Value) -> Result<ToolResult, ToolError> {
    let Some(id) = opt_str(args, "id").map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(ToolResult::text(list_text()));
    };
    let lines = opt_i64(args, "lines")
        .unwrap_or(DEFAULT_SNAPSHOT_LINES as i64)
        .clamp(1, MAX_SNAPSHOT_LINES as i64) as usize;

    let (header, log) = {
        let st = lock();
        let job = st
            .jobs
            .iter()
            .find(|j| j.id == id)
            .ok_or_else(|| unknown_job(id, &st))?;
        (detail_header(job), job.log.clone())
    };

    let body = tail_text(&log, lines);
    let body = if body.trim().is_empty() {
        "(no output yet)".to_string()
    } else {
        body
    };
    Ok(ToolResult::text(format!("{header}\n\n{body}")))
}

/// 未知 job id 的错误：附上现有 id 方便改正。
fn unknown_job(id: &str, st: &State) -> ToolError {
    let known: Vec<&str> = st.jobs.iter().map(|j| j.id.as_str()).collect();
    if known.is_empty() {
        return ToolError(format!("unknown job `{id}`: no background jobs"));
    }
    ToolError(format!(
        "unknown job `{id}`; known jobs: {}",
        known.join(", ")
    ))
}

/// `stop_process`：杀整个进程组并回报。
fn stop(args: &Value) -> Result<ToolResult, ToolError> {
    let id = req_str(args, "id")?;
    let id = id.trim().to_string();
    let pid = {
        let mut st = lock();
        let idx = st
            .jobs
            .iter()
            .position(|j| j.id == id)
            .ok_or_else(|| unknown_job(&id, &st))?;
        let job = &mut st.jobs[idx];
        if let Some(f) = &job.finished {
            return Err(ToolError(format!(
                "{} has already finished ({})",
                job.id,
                outcome_text(f, job.stop_requested)
            )));
        }
        job.stop_requested = true;
        job.pid
    };

    kill_process_tree(pid);
    Ok(ToolResult::text(format!(
        "Stopped {id} (pid {pid}); its process group was killed."
    )))
}

/// 读日志尾部 `max_lines` 行（最多读 [`MAX_SNAPSHOT_BYTES`] 字节），不修改日志。
fn tail_text(path: &Path, max_lines: usize) -> String {
    let Ok(mut file) = File::open(path) else {
        return "(output log is not readable)".to_string();
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    let cut = len > MAX_SNAPSHOT_BYTES;
    if cut {
        _ = file.seek(SeekFrom::Start(len - MAX_SNAPSHOT_BYTES));
    }
    let mut buf = Vec::new();
    if file.read_to_end(&mut buf).is_err() {
        return "(output log is not readable)".to_string();
    }

    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<&str> = text.lines().collect();
    // 从中间开始读时首行可能是半行
    if cut && lines.len() > 1 {
        lines.remove(0);
    }
    let start = lines.len().saturating_sub(max_lines);
    let omitted = start > 0;
    let mut out = lines[start..].join("\n");
    if omitted {
        out = format!("… ({start} earlier lines omitted)\n{out}");
    }
    out
}

/// 一行状态文本：`running` / `exit 0` / `killed`（被停止请求杀掉）。
fn outcome_text(f: &Finished, stop_requested: bool) -> String {
    match (f.exit_code, stop_requested) {
        (_, true) => "stopped".to_string(),
        (Some(code), false) => format!("exit {code}"),
        (None, false) => "killed by signal".to_string(),
    }
}

/// 单个 job 的详细抬头（工具回执与面板之外的纯文本）。
fn detail_header(job: &Job) -> String {
    let status = match &job.finished {
        Some(f) => outcome_text(f, job.stop_requested),
        None => "running".to_string(),
    };
    let elapsed = job
        .finished
        .as_ref()
        .map(|f| f.elapsed)
        .unwrap_or_else(|| job.started.elapsed());
    format!(
        "{} {} · pid {} · {} · log {}\n$ {}",
        job.id,
        status,
        job.pid,
        format_duration(elapsed.as_millis() as u64),
        job.log.display(),
        job.command
    )
}

/// 全部 job 的纯文本清单（`read_process_output` 省略 id 时返回）。
fn list_text() -> String {
    let st = lock();
    if st.jobs.is_empty() {
        return "No background jobs.".to_string();
    }
    let running = st.jobs.iter().filter(|j| j.finished.is_none()).count();
    let mut out = format!("{} job(s), {running} running\n", st.jobs.len());
    for job in &st.jobs {
        out.push_str(&detail_header(job));
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

/// 停靠面板：标题行 + 每 job 一行。
fn dock_lines() -> Vec<DockLine> {
    let st = lock();
    if !st.dock_shown {
        return Vec::new();
    }

    let running = st.jobs.iter().filter(|j| j.finished.is_none()).count();
    let finished = st.jobs.len() - running;
    let mut title = vec![
        DockSpan::new("accent", "#8abeb7", "Processes"),
        DockSpan::new("muted", "#999999", format!("  {running} running")),
    ];
    if finished > 0 {
        title.push(DockSpan::new(
            "dim",
            "#666666",
            format!("  · {finished} finished"),
        ));
    }

    let mut lines = vec![title];
    if st.jobs.is_empty() {
        lines.push(vec![DockSpan::plain("  no background jobs")]);
        return lines;
    }

    for job in &st.jobs {
        let elapsed = job
            .finished
            .as_ref()
            .map(|f| f.elapsed)
            .unwrap_or_else(|| job.started.elapsed());
        let (glyph, key, fallback, status) = match &job.finished {
            None => (DEF_DOT_FILLED, "success", "#b5bd68", "running".to_string()),
            Some(f) if f.exit_code == Some(0) && !job.stop_requested => {
                (DEF_DOT_EMPTY, "dim", "#666666", "exit 0".to_string())
            }
            Some(f) => (
                DEF_DOT_EMPTY,
                "error",
                "#cc6666",
                outcome_text(f, job.stop_requested),
            ),
        };

        lines.push(vec![
            DockSpan::new(key, fallback, format!("{glyph} ")),
            DockSpan::new("text", "#ffffff", job.id.clone()),
            DockSpan::new("muted", "#999999", format!("  {status}")),
            DockSpan::new(
                "dim",
                "#666666",
                format!("  {}", format_duration(elapsed.as_millis() as u64)),
            ),
            DockSpan::new(
                "muted",
                "#999999",
                format!("  {}", one_line(&job.command, 80)),
            ),
        ]);
    }
    lines
}

/// 把命令压成一行并截断（面板一行放不下多行命令）。
fn one_line(command: &str, max_chars: usize) -> String {
    let flat = command.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&flat, max_chars)
}

/// `/ps [list|stop <id>|clear]`：按子命令分派，未知子命令提示用法。
fn command_ps(host: &mut dyn CommandHost, raw: &str) -> bool {
    let args = command_arg(raw);
    let (sub, rest) = match args.split_once(char::is_whitespace) {
        Some((sub, rest)) => (sub, rest.trim()),
        None => (args, ""),
    };

    match sub.to_lowercase().as_str() {
        "" | "list" => {
            host.push_message(&list_text(), UiNotifyLevel::Info);
            show_dock(host);
        }
        "stop" => {
            if rest.is_empty() {
                host.push_message("Usage: /ps stop <id>", UiNotifyLevel::Warning);
                return false;
            }
            match stop(&json!({ "id": rest })) {
                Ok(result) => host.push_message(&result.text, UiNotifyLevel::Success),
                Err(e) => host.push_message(&e.0, UiNotifyLevel::Error),
            }
        }
        "clear" => {
            let dropped = clear_finished();
            host.push_message(
                &format!("Dropped {dropped} finished job(s)."),
                UiNotifyLevel::Success,
            );
        }
        other => host.push_message(&usage(other), UiNotifyLevel::Warning),
    }
    false
}

/// 打开停靠面板（面板内容的显隐本身就是「job 存在」，故这里直接置位并请求显示）。
fn show_dock(host: &mut dyn CommandHost) {
    lock().dock_shown = true;
    host.show_dock();
}

/// 丢掉已完成 job 记录，返回丢掉的数量。
fn clear_finished() -> usize {
    let mut st = lock();
    let before = st.jobs.len();
    st.jobs.retain(|j| j.finished.is_none());
    before - st.jobs.len()
}

/// 未知子命令时的用法提示；`arg` 非空时一并回显，便于定位敲错的词。
fn usage(arg: &str) -> String {
    let head = "Usage: /ps [list|stop <id>|clear]";
    if arg.is_empty() {
        head.to_string()
    } else {
        format!("{head} (got `{arg}`)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::tui::app::{App, sys_spans_text};

    /// 进程表是进程级全局态，测试串行执行。
    fn lock_test() -> MutexGuard<'static, ()> {
        /// 本模块测试的串行锁。
        static TEST_LOCK: Mutex<()> = Mutex::new(());
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 停掉并清空全部 job，测试前后各调一次。
    fn reset() {
        let pids = {
            let mut st = lock();
            st.dock_shown = false;
            st.next = 0;
            drain_all_running(&mut st)
        };
        for pid in pids {
            kill_process_tree(pid);
        }
    }

    /// 轮询等待某个 job 结束（最多 10s），超时返回 None。
    fn wait_finished(id: &str) -> Option<String> {
        for _ in 0..500 {
            let text = read(&json!({ "id": id })).unwrap().text;
            if !text.contains(" running ·") {
                return Some(text);
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        None
    }

    /// 空命令与不存在的目录都直接拒绝。
    #[test]
    fn start_rejects_empty_command_and_missing_dir() {
        let _g = lock_test();
        assert!(start_job("   ", "/tmp").is_err());
        let err = start_job("echo hi", "/definitely/not/here").unwrap_err();
        assert!(err.0.contains("working directory does not exist"));
    }

    /// 后台命令的输出写进日志，读快照能拿到内容与退出码。
    #[test]
    fn background_job_reports_output_and_exit_code() {
        let _g = lock_test();
        reset();

        let receipt = start_job("printf 'one\\ntwo\\n'", "/tmp").unwrap();
        assert!(
            receipt.contains("Started job-1"),
            "回执应带 job id: {receipt}"
        );
        assert!(receipt.contains("Log: "), "回执应带日志路径: {receipt}");

        let text = wait_finished("job-1").expect("job 应在 10s 内结束");
        assert!(text.contains("exit 0"), "应报退出码: {text}");
        assert!(
            text.contains("one") && text.contains("two"),
            "应含输出: {text}"
        );
        assert!(text.contains("printf"), "应回显命令行: {text}");

        reset();
    }

    /// stop_process 杀掉进程组，job 变成 stopped，重复停止报「已结束」。
    #[test]
    fn stop_process_kills_the_job() {
        let _g = lock_test();
        reset();

        start_job("sleep 30", "/tmp").unwrap();
        let stopped = stop(&json!({ "id": "job-1" })).unwrap().text;
        assert!(stopped.contains("Stopped job-1"), "{stopped}");

        let text = wait_finished("job-1").expect("被杀后 job 应结束");
        assert!(text.contains("stopped"), "应标记 stopped: {text}");

        let again = stop(&json!({ "id": "job-1" })).unwrap_err();
        assert!(again.0.contains("already finished"), "{}", again.0);

        reset();
    }

    /// 省略 id 时列出全部 job；未知 id 的报错列出已知 id。
    #[test]
    fn reading_without_id_lists_jobs() {
        let _g = lock_test();
        reset();

        assert_eq!(read(&json!({})).unwrap().text, "No background jobs.");

        start_job("sleep 30", "/tmp").unwrap();
        let list = read(&json!({})).unwrap().text;
        assert!(list.contains("1 job(s), 1 running"), "{list}");
        assert!(list.contains("job-1 running"), "{list}");

        let err = read(&json!({ "id": "job-9" })).unwrap_err();
        assert!(err.0.contains("known jobs: job-1"), "{}", err.0);

        reset();
    }

    /// `/ps` 打开面板并列 job；`/ps clear` 丢掉已完成记录。
    #[test]
    fn ps_command_shows_panel_and_clears_finished_jobs() {
        let _g = lock_test();
        reset();

        start_job("printf 'done\\n'", "/tmp").unwrap();
        wait_finished("job-1").expect("job 应结束");

        let mut st = App::new();
        st.dock_visible = false;
        command_ps(&mut st, "ps");
        assert!(lock().dock_shown, "/ps 应打开面板");
        assert!(
            st.system_messages
                .iter()
                .any(|(_, m)| sys_spans_text(&m.spans).contains("job-1"))
        );
        let lines = dock_lines();
        assert!(lines.len() >= 2, "面板应有标题行 + job 行: {lines:?}");

        command_ps(&mut st, "ps clear");
        assert!(lock().jobs.is_empty(), "clear 应丢掉已完成的 job");
        assert!(dock_lines().len() == 2, "无 job 时只有标题行 + 空提示");

        assert!(!command_ps(&mut st, "ps bogus"));
        assert!(
            st.system_messages
                .iter()
                .any(|(_, m)| sys_spans_text(&m.spans).contains("Usage: /ps"))
        );

        reset();
    }

    /// 空的输出日志给一句可读提示，而不是空串。
    #[test]
    fn empty_log_reads_as_placeholder() {
        let _g = lock_test();
        reset();

        let (file, log) = create_output_file_stream("process-test", ".log").unwrap();
        drop(file);
        assert_eq!(tail_text(&log, 10), "");
        _ = std::fs::remove_file(&log);

        reset();
    }
}

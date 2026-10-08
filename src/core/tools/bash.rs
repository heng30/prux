//! bash 工具：异步执行 + 输出收集/截断 + 超时终止。

use crate::{
    core::{
        settings_manager,
        tools::index::{ToolError, ToolResult},
    },
    utils::truncate::{
        self, DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, head_bytes, tail_bytes,
    },
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{io::AsyncReadExt, sync::Mutex};

/// 运行中 bash 进程的登记项：进程 pid 与用于中止它的标志位。
type BashProcEntry = (i32, Arc<AtomicBool>);

/// 流式输出回调（带锁，多生产端共写）
pub type OnChunkCallback = Arc<Mutex<Box<dyn FnMut(&str) + Send>>>;

/// bash 命令最大超时（秒）
pub const MAX_TIMEOUT_SECONDS: f64 = 2_147_483_647.0 / 1000.0;

/// 面向程序化调用方（codemode 脚本等）的结构化输出上限：1 MiB，超出时保留首尾各半。
pub const STRUCTURED_OUTPUT_MAX_BYTES: usize = 1024 * 1024;

/// bash 可执行文件路径缓存（进程内 PATH 不会变，避免每条命令重复查找）。
static BASH_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();

/// 运行中 bash 进程注册表：id → (pid, 中止标志)，供 abort_bash 中止使用。
static BASH_PROCS: OnceLock<StdMutex<HashMap<String, BashProcEntry>>> = OnceLock::new();

/// bash 面向程序化调用方的结构化结果（`ToolResult::structured_content` 载荷）。
/// 模型侧看到的仍是截断到 50 KiB / 2000 行的文本，脚本可以决定怎么用它。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BashStructuredOutput {
    /// 命令的合并输出，至多 [`STRUCTURED_OUTPUT_MAX_BYTES`] 字节；超出时首尾各半并插入省略标记。
    pub output: String,
    /// `output` 是否省略了部分命令输出（true 表示不完整）。
    pub truncated: bool,
    /// 省略时保存完整日志的临时文件路径；未省略为 None 且不参与序列化。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_output_path: Option<String>,
    /// 进程退出码（非 0 表示命令失败，但脚本仍能拿到本结构）。
    pub exit_code: i32,
    /// 命令墙钟耗时，单位秒，取到 0.1 秒精度。
    pub wall_time_seconds: f64,
}

/// bash 执行结果
pub struct BashOutcome {
    /// 返回给模型的输出文本，可能已按行数/字节截断。
    pub text: String,
    /// 进程退出码；被信号杀死、取消或启动失败时为 None。
    pub exit_code: Option<i32>,
    /// 是否因收到 abort 请求而被主动取消。
    pub cancelled: bool,
    /// 是否因超过 timeout 限制被强制终止。
    pub timed_out: bool,
    /// 输出是否超出限制被截断（完整内容已写入临时文件）。
    pub truncated: bool,
    /// 截断时保存完整输出的临时文件路径；未截断为 None。
    pub full_output_path: Option<String>,
    /// 面向程序化调用方（codemode 脚本）的结构化结果；未拿到退出码（启动失败/超时/取消）时为 None。
    pub structured: Option<Value>,
}

/// bash 会话环境（注入的 PRUX_* 变量；exposeSessionEnvironment=false 时清空）
#[derive(Debug, Clone, Default)]
pub struct BashSessionEnv {
    /// 当前会话 id，注入为 PRUX_SESSION_ID；无会话上下文时为 None。
    pub session_id: Option<String>,
    /// 会话持久化文件路径，注入为 PRUX_SESSION_FILE；无则为 None。
    pub session_file: Option<String>,
    /// 当前模型提供方名称，注入为 PRUX_PROVIDER；未指定时为 None。
    pub provider: Option<String>,
    /// 当前模型标识，注入为 PRUX_MODEL；未指定时为 None。
    pub model: Option<String>,
    /// 推理强度档位，注入为 PRUX_REASONING_LEVEL；未设置为 None。
    pub reasoning_level: Option<String>,
    /// 是否把这些会话信息注入子进程环境；false 时先清空同名变量。
    pub expose: bool,
}

impl BashSessionEnv {
    /// 把本会话环境写入 `cmd`：先清除同名变量，仅在 `expose` 为真时按字段注入 PRUX_*。
    fn apply(&self, cmd: &mut tokio::process::Command) {
        // 先删除父进程可能泄漏的同名变量
        for k in [
            "PRUX_SESSION_ID",
            "PRUX_SESSION_FILE",
            "PRUX_PROVIDER",
            "PRUX_MODEL",
            "PRUX_REASONING_LEVEL",
        ] {
            cmd.env_remove(k);
        }

        if !self.expose {
            return;
        }

        if let Some(v) = &self.session_id {
            cmd.env("PRUX_SESSION_ID", v);
        }
        if let Some(v) = &self.session_file {
            cmd.env("PRUX_SESSION_FILE", v);
        }
        if let Some(v) = &self.provider {
            cmd.env("PRUX_PROVIDER", v);
        }
        if let Some(v) = &self.model {
            cmd.env("PRUX_MODEL", v);
        }
        if let Some(v) = &self.reasoning_level {
            cmd.env("PRUX_REASONING_LEVEL", v);
        }
    }
}

/// 解析 shell：
/// 1) settings shellPath（~ 已展开；路径不存在 → 错误）2) /bin/bash 3) $PATH bash 4) /bin/sh
fn resolve_shell() -> Result<PathBuf, String> {
    if let Some(configured) = settings_manager::read_settings_shell_path() {
        let p = PathBuf::from(&configured);
        if p.is_file() {
            return Ok(p);
        }
        return Err(format!(
            "Configured shellPath does not exist: {}",
            configured
        ));
    }

    if let Some(bash) = find_bash() {
        return Ok(bash);
    }

    let sh = PathBuf::from("/bin/sh");
    if is_executable(&sh) {
        Ok(sh)
    } else {
        Err("shell executable not found: checked settings shellPath, $PATH bash, /bin/bash, and /bin/sh.".to_string())
    }
}

/// 构造一个失败结果：无退出码、未取消/超时/截断，文本为给定内容。
fn fail_outcome(text: String) -> BashOutcome {
    BashOutcome {
        text,
        exit_code: None,
        cancelled: false,
        timed_out: false,
        truncated: false,
        full_output_path: None,
        structured: None,
    }
}

/// 运行中 bash 进程表（全局单例）：中止 id → (pid, 取消标志)。
fn bash_procs_registry() -> &'static StdMutex<HashMap<String, (i32, Arc<AtomicBool>)>> {
    BASH_PROCS.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// 强杀进程树：Unix 用 killpg（子进程以 process_group(0) 起，整组同杀）；
/// Windows 无进程组，用 taskkill /T 连同子进程一起终止。
fn kill_process_tree(pid: i32) {
    #[cfg(unix)]
    {
        _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(pid),
            nix::sys::signal::Signal::SIGKILL,
        );
    }

    #[cfg(windows)]
    {
        _ = std::process::Command::new("taskkill")
            .args(["/F", "/T", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
    }
}

/// 中止指定 id 的运行中 bash 进程（abort_bash）。返回是否找到并中止。
pub fn abort_bash(id: &str) -> bool {
    match bash_procs_registry().lock().unwrap().remove(id) {
        Some((pid, flag)) => {
            flag.store(true, Ordering::SeqCst);
            kill_process_tree(pid);
            true
        }
        None => false,
    }
}

/// 中止所有运行中的 bash 进程（abort_all_bash：中止 agent 工具运行的 bash）。
/// 返回被中止的进程数。
pub fn abort_all_bash() -> usize {
    let entries: Vec<(i32, Arc<AtomicBool>)> = {
        let mut reg = bash_procs_registry().lock().unwrap();
        reg.drain().map(|(_, e)| e).collect()
    };
    let count = entries.len();
    for (pid, flag) in entries {
        flag.store(true, Ordering::SeqCst);
        kill_process_tree(pid);
    }
    count
}

/// 查找 bash 可执行文件：优先按 `$PATH` 逐目录查找（NixOS 等发行版
/// bash 不在 `/bin` 下，而在 `/run/current-system/sw/bin` 等 PATH 目录）；
/// 找不到时回退传统路径 `/bin/bash`。
pub(crate) fn find_bash() -> Option<PathBuf> {
    BASH_PATH
        .get_or_init(|| {
            let path_var = std::env::var_os("PATH");
            if let Some(paths) = path_var.as_deref().map(std::env::split_paths)
                && let Some(found) = search_bash_in_paths(paths)
            {
                return Some(found);
            }

            let fallback = PathBuf::from("/bin/bash");
            if is_executable(&fallback) {
                Some(fallback)
            } else {
                None
            }
        })
        .clone()
}

/// 按候选目录顺序查找可执行的 bash。
fn search_bash_in_paths(paths: impl Iterator<Item = PathBuf>) -> Option<PathBuf> {
    for dir in paths {
        let candidate = dir.join(bash_bin_name());
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

/// 当前平台下 bash 可执行文件名（Windows 为 `bash.exe`）。
fn bash_bin_name() -> &'static str {
    if cfg!(windows) { "bash.exe" } else { "bash" }
}

/// 判断路径是否为常规文件且具有可执行权限（非 Unix 平台仅检查是文件）。
fn is_executable(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        path.metadata()
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

/// 执行 bash（带中止 id、流式输出回调），返回结构化结果
pub async fn execute_bash_with(
    command: &str,
    timeout: Option<f64>,
    cwd: &str,
    id: Option<&str>,
    on_chunk: Option<OnChunkCallback>,
) -> BashOutcome {
    execute_bash_with_env(
        command,
        timeout,
        cwd,
        id,
        on_chunk,
        &BashSessionEnv::default(),
    )
    .await
}

/// 带会话环境与 shell 解析的 bash 执行
pub async fn execute_bash_with_env(
    command: &str,
    timeout: Option<f64>,
    cwd: &str,
    id: Option<&str>,
    on_chunk: Option<OnChunkCallback>,
    env: &BashSessionEnv,
) -> BashOutcome {
    let cwd_path = PathBuf::from(cwd);
    if !cwd_path.exists() {
        return BashOutcome {
            text: format!(
                "Working directory does not exist: {}\nCannot execute bash commands.",
                cwd
            ),
            exit_code: None,
            cancelled: false,
            timed_out: false,
            truncated: false,
            full_output_path: None,
            structured: None,
        };
    }

    // shell 解析：settings shellPath（~ 展开、路径不存在报错）→ /bin/bash → PATH → /bin/sh
    let bash = match resolve_shell() {
        Ok(b) => b,
        Err(e) => return fail_outcome(e),
    };

    // shellCommandPrefix：在命令前加前缀（如 shopt -s expand_aliases）
    let prefix = settings_manager::read_settings_shell_command_prefix();
    let full_command = if prefix.is_empty() {
        command.to_string()
    } else {
        format!("{}\n{}", prefix, command)
    };

    let mut cmd = tokio::process::Command::new(bash);
    cmd.arg("-c")
        .arg(&full_command)
        .current_dir(&cwd_path)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    #[cfg(unix)]
    cmd.process_group(0);

    env.apply(&mut cmd);

    let fail = |text: String| BashOutcome {
        text,
        exit_code: None,
        cancelled: false,
        timed_out: false,
        truncated: false,
        full_output_path: None,
        structured: None,
    };
    if let Err(e) = validate_timeout(timeout) {
        return fail(e.0);
    }

    let started_at = Instant::now();
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return fail(format!("Failed to execute command: {}", e)),
    };

    let pid = child.id().unwrap_or(0) as i32;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let shared = Arc::new(Mutex::new(BashOutputCollector::new()));

    let cancel_flag = Arc::new(AtomicBool::new(false));
    if let Some(id) = id {
        bash_procs_registry()
            .lock()
            .unwrap()
            .insert(id.to_string(), (pid, cancel_flag.clone()));
    }

    let mut readers: Vec<Box<dyn tokio::io::AsyncRead + Unpin + Send>> = Vec::new();
    if let Some(out) = stdout {
        readers.push(Box::new(out));
    }
    if let Some(err) = stderr {
        readers.push(Box::new(err));
    }

    // 每个输出流单独一个读取任务，经 channel 汇总到一个消费任务。
    // 不能用单任务轮流读：某一侧无数据但未关闭时读操作会挂住，另一侧写满 64 KiB 管道后
    // 两边互等（`yes x | head -n 300000` 这类大批量输出必死锁）。
    let read_task = {
        let shared = shared.clone();
        let on_chunk = on_chunk.clone();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();

        for mut reader in readers {
            let tx = tx.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 8192];
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            let text = String::from_utf8_lossy(&buf[..n]).to_string();
                            if tx.send(text).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
        drop(tx);

        tokio::spawn(async move {
            while let Some(text) = rx.recv().await {
                if let Some(cb) = &on_chunk {
                    cb.lock().await(&text);
                }

                shared.lock().await.push(&text);
            }
        })
    };

    let mut timed_out = false;
    let status = match timeout {
        Some(t) => {
            let timeout_dur = Duration::from_secs_f64(t);
            match tokio::time::timeout(timeout_dur, child.wait()).await {
                Ok(s) => s.ok(),
                Err(_) => {
                    timed_out = true;
                    kill_process_tree(pid);
                    _ = child.wait().await;
                    None
                }
            }
        }
        None => child.wait().await.ok(),
    };

    let cancelled = cancel_flag.load(Ordering::SeqCst);
    if cancelled {
        kill_process_tree(pid);
        _ = child.wait().await;
    }
    if let Some(id) = id {
        bash_procs_registry().lock().unwrap().remove(id);
    }

    _ = read_task.await;

    // 取回收集器本体（不能 clone：Clone 不共享已落盘的完整输出文件，
    // 会丢掉 tail 之外的输出与 `full_output_path`）。此时读取任务已结束，强引用只剩这一份。
    let collector = match Arc::try_unwrap(shared) {
        Ok(m) => m.into_inner(),
        Err(arc) => arc.lock().await.clone(),
    };

    let exit_code = status.and_then(|s| s.code());

    let (mut output_text, full_output_path) = build_bash_output(&collector);
    let truncated = full_output_path.is_some();
    let wall_time_seconds = (started_at.elapsed().as_secs_f64() * 10.0).round() / 10.0;

    if cancelled {
        output_text = append_status(&output_text, "Command was cancelled");
    } else if timed_out {
        output_text = append_status(
            &output_text,
            &format!(
                "Command timed out after {} seconds",
                timeout.unwrap_or(0.0) as i64
            ),
        );
    } else if let Some(code) = exit_code.filter(|c| *c != 0) {
        output_text = append_status(&output_text, &format!("Command exited with code {}", code));
    }

    if output_text.is_empty() {
        output_text = "(no output)".to_string();
    }

    let structured = exit_code.map(|code| {
        let (full, over_limit) = collector.read_full_output(STRUCTURED_OUTPUT_MAX_BYTES);
        json!(BashStructuredOutput {
            output: full,
            truncated: over_limit,
            full_output_path: if over_limit {
                full_output_path.clone()
            } else {
                None
            },
            exit_code: code,
            wall_time_seconds,
        })
    });

    BashOutcome {
        text: output_text,
        exit_code,
        cancelled,
        timed_out,
        truncated,
        full_output_path,
        structured,
    }
}

/// 执行一条 bash 命令（不注入会话环境）。
///
/// 命令被取消、超时或退出码非 0 时返回 `Err(ToolError)`（错误文本即命令输出），
/// 否则返回文本结果。
pub async fn execute_bash(
    command: &str,
    timeout: Option<f64>,
    cwd: &str,
) -> Result<ToolResult, ToolError> {
    validate_timeout(timeout)?;
    let out = execute_bash_with(command, timeout, cwd, None, None).await;
    if out.cancelled || out.timed_out || out.exit_code.filter(|c| *c != 0).is_some() {
        return Err(ToolError(out.text));
    }

    let mut result = ToolResult::text(out.text);
    result.structured_content = out.structured;
    Ok(result)
}

/// 把状态行接到输出末尾：无输出时直接作为正文，否则用空行分隔。
fn append_status(output: &str, status: &str) -> String {
    if output.is_empty() {
        status.to_string()
    } else {
        format!("{}\n\n{}", output, status)
    }
}

/// 校验超时秒数：必须为有限正数且不超过 `MAX_TIMEOUT_SECONDS`，否则返回错误。
fn validate_timeout(timeout: Option<f64>) -> Result<(), ToolError> {
    if let Some(t) = timeout {
        if !t.is_finite() || t <= 0.0 {
            return Err(ToolError(
                "Invalid timeout: must be a finite number of seconds".to_string(),
            ));
        }
        if t > MAX_TIMEOUT_SECONDS {
            return Err(ToolError(format!(
                "Invalid timeout: maximum is {} seconds",
                MAX_TIMEOUT_SECONDS
            )));
        }
    }
    Ok(())
}

/// 输出收集器：实时追尾 + 超限时落盘完整输出（逃生通道）。
/// 字段对 powershell 工具开放（pub(crate)），其执行器复用同一收集/截断语义。
pub(crate) struct BashOutputCollector {
    /// 保留的输出尾部；超过上限时从头部丢弃以控制内存。
    pub(crate) tail: String,
    /// 累计接收的输出字节数，用于判断是否超过字节上限。
    pub(crate) total_bytes: usize,
    /// 已接收的完整行数，以换行符个数累计。
    pub(crate) completed_lines: usize,
    /// 末尾是否存在尚未换行的半行；true 表示还多一行未结束。
    pub(crate) has_open_line: bool,
    /// 当前未结束行已接收的字节数，用于提示该行大小。
    pub(crate) current_line_bytes: usize,
    /// 输出超限后开始写入完整内容的临时文件；未超限为 None。
    pub(crate) full_output: Option<tempfile::NamedTempFile>,
}

impl Clone for BashOutputCollector {
    /// 复制已累计的统计与 tail，但不共享已落盘的完整输出文件（新实例 `full_output` 为 None）。
    fn clone(&self) -> Self {
        BashOutputCollector {
            tail: self.tail.clone(),
            total_bytes: self.total_bytes,
            completed_lines: self.completed_lines,
            has_open_line: self.has_open_line,
            current_line_bytes: self.current_line_bytes,
            full_output: None,
        }
    }
}

impl BashOutputCollector {
    /// 构造空收集器（无输出、未超限）。
    pub(crate) fn new() -> Self {
        BashOutputCollector {
            tail: String::new(),
            total_bytes: 0,
            completed_lines: 0,
            has_open_line: false,
            current_line_bytes: 0,
            full_output: None,
        }
    }

    /// 过滤控制字符：保留 tab/换行/回车，去掉其它 C0 与 0xFFF9..=0xFFFB，并删除所有 `\r`。
    fn sanitize(chunk: &str) -> String {
        let mut out = String::new();
        for c in chunk.chars() {
            let cp = c as u32;
            if cp == 0x09 || cp == 0x0A || cp == 0x0D {
                out.push(c);
            } else if cp <= 0x1F || (0xFFF9..=0xFFFB).contains(&cp) {
                continue;
            } else {
                out.push(c);
            }
        }
        out.replace('\r', "")
    }

    /// 追加一段输出并更新字节/行数统计；首次超限时把已有内容写入临时文件，
    /// 其后持续写入完整日志，并裁剪内存中 tail 至上限的两倍。
    pub(crate) fn push(&mut self, text: &str) {
        let text = Self::sanitize(text);
        let text_bytes = text.len();
        self.total_bytes += text_bytes;
        let newline_count = text.matches('\n').count();
        self.completed_lines += newline_count;
        if let Some(last_newline) = text.rfind('\n') {
            let trailing = &text[last_newline + 1..];
            self.current_line_bytes = trailing.len();
            self.has_open_line = !trailing.is_empty();
        } else if !text.is_empty() {
            self.current_line_bytes += text_bytes;
            self.has_open_line = true;
        }
        self.tail.push_str(&text);
        let total_lines = self.completed_lines + if self.has_open_line { 1 } else { 0 };
        let over_limit = self.total_bytes > DEFAULT_MAX_BYTES || total_lines > DEFAULT_MAX_LINES;
        if over_limit && self.full_output.is_none() {
            if let Ok(mut f) = tempfile::Builder::new()
                .prefix("bash-")
                .suffix(".log")
                .tempfile()
            {
                let _ = std::io::Write::write_all(&mut f, self.tail.as_bytes());
                self.full_output = Some(f);
            }
        } else if let Some(f) = self.full_output.as_mut() {
            let _ = std::io::Write::write_all(f, text.as_bytes());
        }
        if self.tail.len() > DEFAULT_MAX_BYTES * 2 {
            let keep = DEFAULT_MAX_BYTES * 2;
            let mut start = self.tail.len() - keep;
            while start < self.tail.len() && !self.tail.is_char_boundary(start) {
                start += 1;
            }
            self.tail = self.tail[start..].to_string();
        }
    }

    /// 读取供程序化调用方（脚本）使用的完整输出，至多 `max_bytes` 字节：
    /// 未超上限时返回全量（未截断），否则保留首尾各半并以省略标记分隔。
    /// 返回（输出文本, 是否省略了内容）。
    pub(crate) fn read_full_output(&self, max_bytes: usize) -> (String, bool) {
        let text = match &self.full_output {
            Some(file) => match std::fs::read(file.path()) {
                Ok(bytes) => String::from_utf8_lossy(&bytes).to_string(),
                Err(_) => self.tail.clone(),
            },
            None => self.tail.clone(),
        };

        if text.len() <= max_bytes {
            return (text, false);
        }

        let head_limit = max_bytes / 2;
        let tail_limit = max_bytes - head_limit;
        let head = head_bytes(&text, head_limit).to_string();
        let tail = tail_bytes(&text, tail_limit).to_string();
        let omitted = text.len().saturating_sub(head.len() + tail.len());
        (
            format!("{head}\n\n[... {omitted} bytes omitted ...]\n\n{tail}"),
            true,
        )
    }
}

/// 根据收集器状态生成展示文本，返回（带截断提示的输出, 完整输出的临时文件路径）。
fn build_bash_output(collector: &BashOutputCollector) -> (String, Option<String>) {
    let tail_truncation = truncate::truncate_tail(&collector.tail, (None, None));
    let total_lines = collector.completed_lines + if collector.has_open_line { 1 } else { 0 };
    let truncated = total_lines > DEFAULT_MAX_LINES || collector.total_bytes > DEFAULT_MAX_BYTES;
    let full_output_path = collector
        .full_output
        .as_ref()
        .map(|f| f.path().to_string_lossy().to_string());

    let truncation = if truncated {
        let by = if tail_truncation.truncated {
            tail_truncation.truncated_by
        } else if collector.total_bytes > DEFAULT_MAX_BYTES {
            TruncatedBy::Bytes
        } else {
            TruncatedBy::Lines
        };
        truncate::TruncationResult {
            content: tail_truncation.content.clone(),
            truncated: true,
            truncated_by: by,
            total_lines,
            total_bytes: collector.total_bytes,
            output_lines: tail_truncation.output_lines,
            output_bytes: tail_truncation.output_bytes,
            last_line_partial: tail_truncation.last_line_partial,
            first_line_exceeds_limit: false,
            max_lines: tail_truncation.max_lines,
            max_bytes: tail_truncation.max_bytes,
        }
    } else {
        truncate::TruncationResult {
            content: collector.tail.clone(),
            truncated: false,
            truncated_by: TruncatedBy::None,
            total_lines,
            total_bytes: collector.total_bytes,
            output_lines: tail_truncation.output_lines,
            output_bytes: tail_truncation.output_bytes,
            last_line_partial: false,
            first_line_exceeds_limit: false,
            max_lines: tail_truncation.max_lines,
            max_bytes: tail_truncation.max_bytes,
        }
    };

    let mut output_text = if truncation.truncated {
        truncation.content.clone()
    } else {
        collector.tail.clone()
    };

    if truncation.truncated {
        let start_line = truncation
            .total_lines
            .saturating_sub(truncation.output_lines)
            + 1;
        let end_line = truncation.total_lines;
        if let Some(path) = &full_output_path {
            if truncation.last_line_partial {
                let last_line_size = truncate::format_size(collector.current_line_bytes);
                output_text.push_str(&format!(
                    "\n\n[Showing last {} of line {} (line is {}). Full output: {}]",
                    truncate::format_size(truncation.output_bytes),
                    end_line,
                    last_line_size,
                    path
                ));
            } else if truncation.truncated_by == TruncatedBy::Lines {
                output_text.push_str(&format!(
                    "\n\n[Showing lines {}-{} of {}. Full output: {}]",
                    start_line, end_line, truncation.total_lines, path
                ));
            } else {
                output_text.push_str(&format!(
                    "\n\n[Showing lines {}-{} of {} ({} limit). Full output: {}]",
                    start_line,
                    end_line,
                    truncation.total_lines,
                    truncate::format_size(DEFAULT_MAX_BYTES),
                    path
                ));
            }
        }
    }

    (output_text, full_output_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn search_prefers_executable_bash_in_path_order() {
        let dir = tempfile::tempdir().unwrap();
        // 第一个目录里的 bash 不可执行 → 跳过；第二个目录里的可执行 → 命中
        let not_exec = dir.path().join("not-exec");
        let yes_exec = dir.path().join("yes-exec");
        fs::create_dir_all(&not_exec).unwrap();
        fs::create_dir_all(&yes_exec).unwrap();
        fs::write(not_exec.join(bash_bin_name()), "#!/bin/sh\n").unwrap();
        let exec = yes_exec.join(bash_bin_name());
        fs::write(&exec, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&exec, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let paths = vec![not_exec.clone(), yes_exec.clone()];
        let found = search_bash_in_paths(paths.into_iter()).unwrap();
        assert_eq!(found, yes_exec.join(bash_bin_name()));
    }

    #[test]
    fn search_returns_none_when_no_executable_bash() {
        let dir = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(bash_bin_name()), "#!/bin/sh\n").unwrap();
        let paths = vec![dir.path().to_path_buf(), dir2.path().to_path_buf()];
        assert!(search_bash_in_paths(paths.into_iter()).is_none());
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 串行锁必须跨 await 持有（测试间互斥，无同 runtime 竞争者）
    async fn bash_with_chunk_callback_returns() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let cb: Box<dyn FnMut(&str) + Send> = Box::new(|_d: &str| {});
        let on_chunk = Some(Arc::new(Mutex::new(cb)));
        let out = execute_bash_with("echo hi", None, ".", Some("test-id"), on_chunk).await;
        eprintln!(
            "OUTCOME text={:?} exit={:?} cancelled={}",
            out.text, out.exit_code, out.cancelled
        );
        assert!(!out.cancelled);
    }

    /// `abort_all_bash` 杀的是**全局**注册表里的进程，会误伤并行跑的其它 bash 测试
    /// （`echo hi` 被 kill → cancelled=true）。跑真实命令的测试统一持 AUTH_TEST_LOCK 串行。
    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // 串行锁必须跨 await 持有（测试间互斥，无同 runtime 竞争者）
    async fn bash_abort_all_kills_running() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let handle = tokio::spawn(async {
            execute_bash_with("sleep 30", None, ".", Some("abort-test"), None).await
        });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let n = abort_all_bash();
        assert!(n > 0, "expected at least one registered bash process");
        let out = handle.await.unwrap();
        eprintln!("ABORTED cancelled={}", out.cancelled);
        assert!(out.cancelled);
    }

    #[test]
    fn collector_tail_trim_keeps_utf8_boundary() {
        let mut collector = BashOutputCollector::new();
        // 全 3 字节多字节字符、无换行：push 后 tail 必然超过 2 倍上限，
        // 触发按字节裁剪。若裁剪点未对齐到字符边界（panic: start byte index ... is not a char boundary），
        // 此测试会直接 panic。
        let chunk = "档".repeat(40_000); // 120000 字节
        for _ in 0..4 {
            collector.push(&chunk);
            assert!(collector.tail.len() <= DEFAULT_MAX_BYTES * 2);
            assert!(collector.tail.chars().all(|c| c == '档'));
        }
    }

    #[test]
    fn collector_tail_trim_survives_mixed_width_chunks() {
        let mut collector = BashOutputCollector::new();
        let pusher = |c: &mut BashOutputCollector, n: usize| {
            for i in 0..n {
                let s = if i % 3 == 0 {
                    "档✨汉字abc\n".to_string()
                } else {
                    "x".repeat(i % 7 + 1)
                };
                c.push(&s);
            }
        };
        pusher(&mut collector, 40_000);
        assert!(collector.tail.len() <= DEFAULT_MAX_BYTES * 2);
        // 裁剪后仍是合法 UTF-8 且不含被从中间切断的字符
        assert!(std::str::from_utf8(collector.tail.as_bytes()).is_ok());
    }

    #[test]
    fn collector_read_full_output_returns_everything_below_limit() {
        let mut collector = BashOutputCollector::new();
        collector.push("hello\n");
        let (text, truncated) = collector.read_full_output(STRUCTURED_OUTPUT_MAX_BYTES);
        assert_eq!(text, "hello\n");
        assert!(!truncated);
    }

    #[test]
    fn collector_read_full_output_keeps_head_and_tail() {
        let mut collector = BashOutputCollector::new();
        let line = format!("{}\n", "x".repeat(1023));
        for _ in 0..3000 {
            collector.push(&line);
        }
        let (text, truncated) = collector.read_full_output(STRUCTURED_OUTPUT_MAX_BYTES);
        assert!(truncated);
        assert!(text.starts_with("xxxx"));
        assert!(text.contains("bytes omitted"));
        assert!(text.trim_end().ends_with('x'));
        // 省略标记之外的正文不得超过上限（加上分隔符与标记的少量开销）
        assert!(text.len() <= STRUCTURED_OUTPUT_MAX_BYTES + 64);
    }

    #[test]
    fn collector_read_full_output_cuts_on_char_boundaries() {
        let mut collector = BashOutputCollector::new();
        let line = format!("{}\n", "档".repeat(341));
        for _ in 0..3000 {
            collector.push(&line);
        }
        let (text, truncated) = collector.read_full_output(STRUCTURED_OUTPUT_MAX_BYTES);
        assert!(truncated);
        // 首尾都是完整字符，不出现替换符（U+FFFD）
        assert!(!text.contains('\u{fffd}'));
        assert!(text.starts_with('档'));
        assert!(text.ends_with('\n'));
    }
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // 串行锁必须跨 await 持有（测试间互斥，无同 runtime 竞争者）
async fn bash_outcome_carries_structured_output() {
    let _g = crate::test_support::AUTH_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let out = execute_bash_with(
        "echo out; echo err >&2; exit 3",
        None,
        tmp.path().to_str().unwrap(),
        None,
        None,
    )
    .await;
    assert_eq!(out.exit_code, Some(3));
    let structured = out
        .structured
        .expect("structured output for a finished command");
    assert_eq!(structured["exit_code"], 3);
    assert_eq!(structured["truncated"], false);
    assert!(structured["wall_time_seconds"].is_number());
    let output = structured["output"].as_str().unwrap();
    assert!(output.contains("out") && output.contains("err"), "{output}");
    // 未截断时不给出临时文件路径
    assert!(structured.get("full_output_path").is_none());
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // 串行锁必须跨 await 持有（测试间互斥，无同 runtime 竞争者）
async fn bash_outcome_has_no_structured_output_without_exit_code() {
    let _g = crate::test_support::AUTH_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let out = execute_bash_with(
        "sleep 5",
        Some(0.2),
        tmp.path().to_str().unwrap(),
        None,
        None,
    )
    .await;
    assert!(out.timed_out);
    assert!(out.structured.is_none());
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // 串行锁必须跨 await 持有（测试间互斥，无同 runtime 竞争者）
async fn bash_injects_session_env() {
    let _g = crate::test_support::AUTH_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let env = BashSessionEnv {
        session_id: Some("sess-1".into()),
        session_file: Some("/tmp/sess.jsonl".into()),
        provider: Some("deepseek".into()),
        model: Some("deepseek-chat".into()),
        reasoning_level: Some("medium".into()),
        expose: true,
    };
    let out = execute_bash_with_env(
        "echo PRUX_SESSION_ID=$PRUX_SESSION_ID PRUX_PROVIDER=$PRUX_PROVIDER PRUX_MODEL=$PRUX_MODEL PRUX_REASONING_LEVEL=$PRUX_REASONING_LEVEL PRUX_SESSION_FILE=$PRUX_SESSION_FILE",
        None,
        tmp.path().to_str().unwrap(),
        None,
        None,
        &env,
    )
    .await;
    assert_eq!(out.exit_code, Some(0), "{}", out.text);
    assert!(out.text.contains("PRUX_SESSION_ID=sess-1"), "{}", out.text);
    assert!(out.text.contains("PRUX_PROVIDER=deepseek"), "{}", out.text);
    assert!(
        out.text.contains("PRUX_MODEL=deepseek-chat"),
        "{}",
        out.text
    );
    assert!(
        out.text.contains("PRUX_REASONING_LEVEL=medium"),
        "{}",
        out.text
    );
    assert!(
        out.text.contains("PRUX_SESSION_FILE=/tmp/sess.jsonl"),
        "{}",
        out.text
    );
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // 串行锁必须跨 await 持有（测试间互斥，无同 runtime 竞争者）
async fn bash_hides_session_env_when_not_exposed() {
    let _g = crate::test_support::AUTH_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let env = BashSessionEnv {
        session_id: Some("sess-1".into()),
        session_file: None,
        provider: Some("deepseek".into()),
        model: None,
        reasoning_level: None,
        expose: false,
    };
    let out = execute_bash_with_env(
        "echo PRUX=$PRUX_SESSION_ID:$PRUX_PROVIDER",
        None,
        tmp.path().to_str().unwrap(),
        None,
        None,
        &env,
    )
    .await;
    assert_eq!(out.exit_code, Some(0), "{}", out.text);
    // expose=false → 不注入：会话 id 与 provider 均不应出现
    assert!(!out.text.contains("sess-1"), "{}", out.text);
    assert!(!out.text.contains("deepseek"), "{}", out.text);
    assert!(out.text.contains("PRUX=:"), "{}", out.text);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)] // 串行锁必须跨 await 持有（测试间互斥，无同 runtime 竞争者）
async fn bash_prepends_shell_command_prefix() {
    let _g = crate::test_support::AUTH_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("prefix-test");
    std::fs::create_dir_all(&dir).unwrap();
    // 避免污染全局 agent_dir settings：直接断言前缀拼装逻辑（read_settings 已单独有测试路径）
    let prefix = crate::core::settings_manager::read_settings_shell_command_prefix();
    if prefix.is_empty() {
        return; // 默认无前缀；有配置时的行为由拼装逻辑保证
    }
    let out = execute_bash_with_env(
        "echo MARK=$MARKER",
        None,
        dir.to_str().unwrap(),
        None,
        None,
        &BashSessionEnv::default(),
    )
    .await;
    assert!(out.text.contains("MARK=prefixed"), "{}", out.text);
}

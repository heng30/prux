//! 外部工具管理：解析/探测 fd / rg，并按需（`/download`）安装
//!
//! - 优先使用 `agent_dir/bin` 下已安装的二进制，其次 PATH 中的系统命令。
//! - 启动时（[`ensure_bin_in_path`]）把 `agent_dir/bin` 追加到 PATH 末尾：子进程能按命令名
//!   找到 prux 装好的工具，同时不抢占系统里已有的同名命令。
//! - **不做后台自动下载**：启动只探测并提示（[`missing_tool_hints`]）；安装走
//!   `downloader` 扩展的 `/download <tool>`（[`download_now`]）。
//! - 状态（下载中/完成/失败）通过回调上报，由调用方展示到 TUI。
//! - 同一份状态另存为进程内快照（[`with_snapshots`]），供 dock 面板之类只读视图
//!   展示「当前工具情况」；快照由本模块独占写入，读取方不需要自己探盘/起进程。

use crate::{
    core::{runtime, settings_manager::agent_dir},
    error::{Error, Result},
    utils::http,
};
use std::{
    ffi::{OsStr, OsString},
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, MutexGuard, OnceLock},
    time::Duration,
};

/// 支持自动安装的工具
const TOOL_REPOS: &[(&str, &str)] = &[("fd", "sharkdp/fd"), ("rg", "BurntSushi/ripgrep")];

/// 工具安装状态回调
#[derive(Debug, Clone)]
pub enum ToolStatus {
    /// 普通进度/成功信息，UI 按普通样式展示。
    Info(String),
    /// 失败或异常提示，UI 按警告样式展示。
    Warning(String),
}

impl ToolStatus {
    /// 状态携带的提示文本（Info 与 Warning 都适用）。
    pub fn message(&self) -> &str {
        match self {
            ToolStatus::Info(m) | ToolStatus::Warning(m) => m,
        }
    }
    /// 是否为警告状态（UI 按警告样式展示）。
    pub fn is_warning(&self) -> bool {
        matches!(self, ToolStatus::Warning(_))
    }
}

/// 工具当前状态（dock 段展示与 `/download` 分支判定共用）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolState {
    /// 本地与 PATH 都没有
    Missing,
    /// 正在下载（同一工具不会并发）
    Downloading,
    /// 可用
    Present,
    /// 最近一次下载失败（旧副本仍在时 `path`/`version` 会被保留）
    Failed,
}

/// 可用副本的来源
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    /// `agent_dir/bin` 下由 prux 安装的副本
    AgentBin,
    /// PATH 中的系统副本
    Path,
}

/// 工具状态快照（只读视图用；字段都描述「当下能执行什么」）
#[derive(Debug, Clone)]
pub struct ToolSnapshot {
    /// 工具名（`fd` / `rg`）
    pub tool: String,
    /// 当前状态
    pub state: ToolState,
    /// 可执行文件路径；PATH 命中时为命令名
    pub path: Option<PathBuf>,
    /// 来源；无可用副本时为 `None`
    pub source: Option<ToolSource>,
    /// `--version` 探测到的版本号
    pub version: Option<String>,
    /// 最近一次下载失败原因（开始下载时清空）
    pub last_error: Option<String>,
}

/// 存放已安装二进制的目录：`agent_dir/bin`
pub fn bin_dir() -> PathBuf {
    agent_dir().join("bin")
}

/// 本地安装的二进制完整路径（不一定存在）
pub fn tool_local_name(name: &str) -> String {
    if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    }
}

/// 本地安装的二进制完整路径（不一定存在）
pub fn tool_local_path(name: &str) -> PathBuf {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    bin_dir().join(exe)
}

/// 在 PATH 中查找可执行文件（存在即返回命令名）
fn which_in_path(name: &str) -> Option<String> {
    let path = std::env::var_os("PATH")?;
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(&exe);
        if candidate.is_file() {
            return Some(name.to_string());
        }
    }
    None
}

/// 解析工具位置：本地 bin 优先，其次 PATH（fd 兼容 Debian 的 `fdfind`）。
/// 本地命中返回完整路径，PATH 命中返回命令名。
fn resolve(tool: &str) -> Option<(String, ToolSource)> {
    let local = tool_local_path(tool);
    if local.is_file() {
        return Some((local.to_string_lossy().to_string(), ToolSource::AgentBin));
    }

    let mut names = vec![tool];
    if tool == "fd" {
        names.push("fdfind");
    }

    for name in names {
        if which_in_path(name).is_some() {
            return Some((name.to_string(), ToolSource::Path));
        }
    }

    None
}

/// 解析 fd 路径：本地 bin 优先，其次 PATH 中的 fd / fdfind
pub fn fd_path() -> Option<String> {
    resolve("fd").map(|(cmd, _)| cmd)
}

/// 解析 rg 路径：本地 bin 优先，其次 PATH 中的 rg
pub fn rg_path() -> Option<String> {
    resolve("rg").map(|(cmd, _)| cmd)
}

/// 本模块管理的工具名清单（`TOOL_REPOS` 的顺序）
pub fn tool_names() -> Vec<&'static str> {
    TOOL_REPOS.iter().map(|(tool, _)| *tool).collect()
}

/// 该工具是否由本模块管理
pub fn is_known_tool(tool: &str) -> bool {
    TOOL_REPOS.iter().any(|(name, _)| *name == tool)
}

/// 工具显示名（`rg` → `ripgrep`；其余返回原名）
pub fn display_name(tool: &str) -> String {
    match tool {
        "rg" => "ripgrep".to_string(),
        _ => tool.to_string(),
    }
}

/// 未知工具的提示（附可用清单）
pub fn unknown_tool_message(tool: &str) -> String {
    format!(
        "Unknown tool \"{tool}\". Available: {}",
        tool_names().join(", ")
    )
}

/// 工具依赖的内置工具名（`fd` → `find`，`rg` → `grep`）
pub fn builtin_tool_for(tool: &str) -> Option<&'static str> {
    match tool {
        "fd" => Some("find"),
        "rg" => Some("grep"),
        _ => None,
    }
}

/// 工具的 GitHub releases 页面（人工下载地址）
pub fn releases_url(tool: &str) -> Option<String> {
    TOOL_REPOS
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, repo)| format!("https://github.com/{repo}/releases"))
}

/// 缺失工具的安装指引：下载地址 + `downloader` 扩展的 `/download <tool>`。
/// 内置 find/grep 依赖这些外部程序，缺失时对应工具直接报错（不做后台自动下载）。
pub fn install_hint(tool: &str) -> String {
    let display = display_name(tool);
    let builtin = builtin_tool_for(tool).unwrap_or("builtin");
    match releases_url(tool) {
        Some(url) => format!(
            "{display} is not installed; the builtin {builtin} tool needs it. \
             Install it from {url} or run `/download {tool}` (downloader extension)."
        ),
        None => format!("{display} is not installed"),
    }
}

/// 启动检查（**不下载**）：对已启用的内置 `find` / `grep`，若其后端程序缺失，
/// 返回一条安装指引；同时刷新快照（供 dock 展示当前状态）。无缺失时返回空。
pub fn missing_tool_hints(active_tools: &[String]) -> Vec<String> {
    refresh_all();

    let mut hints = Vec::new();
    for (tool, _) in TOOL_REPOS {
        let Some(builtin) = builtin_tool_for(tool) else {
            continue;
        };
        if !active_tools.iter().any(|t| t == builtin) {
            continue;
        }
        if snapshot(tool).is_some_and(|s| s.path.is_some()) {
            continue;
        }
        hints.push(install_hint(tool));
    }
    hints
}

/// 显式请求下载（可能已存在）时的离线提示。
pub fn offline_skip_message(tool: &str) -> String {
    format!(
        "Offline mode enabled: skipping download of {}.",
        display_name(tool)
    )
}

/// 把 bin 目录追加到 PATH **末尾**（子进程按命令名即可找到装好的工具）。
/// 幂等：已在 PATH 中就什么都不做，避免每次安装都把同一目录重复堆进 PATH。
///
/// 追加而非前置：系统里已有的同名命令（用户自装的 `rg`/`fd`）保持优先级；
/// 自己的 grep/find 工具不依赖 PATH 顺序，走 [`resolve`] 解析出的完整路径。
/// 目录尚未创建（首次启动、还没装任何工具）时也会写入——条目指向不存在的目录无害，
/// 且后续下载的二进制无需再改 PATH。
pub fn ensure_bin_in_path() {
    let dir = bin_dir();
    if let Some(new_path) = path_with_bin_appended(std::env::var_os("PATH").as_deref(), &dir) {
        unsafe { std::env::set_var("PATH", new_path) };
    }
}

/// 计算「把 dir 追加到 PATH 末尾」的结果；`dir` 已在其中（或无法拼接）时返回 `None`。
/// 与 env 无关的纯函数，便于测试。
fn path_with_bin_appended(current: Option<&OsStr>, dir: &Path) -> Option<OsString> {
    if let Some(p) = current
        && std::env::split_paths(p).any(|d| same_dir(&d, dir))
    {
        return None;
    }

    let mut paths: Vec<PathBuf> = current
        .into_iter()
        .flat_map(std::env::split_paths)
        .collect();
    paths.push(dir.to_path_buf());
    std::env::join_paths(paths).ok()
}

/// 目录相等判定：先按字面比较，再（两侧都存在时）按规范化路径比较，
/// 以容忍 `~`/相对路径/尾部分隔符/符号链接/Windows 大小写差异。
fn same_dir(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }

    matches!((a.canonicalize(), b.canonicalize()), (Ok(a), Ok(b)) if a == b)
}

/// 状态快照表（顺序 = [`TOOL_REPOS`]）；只由本模块写入。
fn state() -> &'static Mutex<Vec<ToolSnapshot>> {
    /// 全工具状态快照表的惰性存储，首次访问时按工具清单初始化。
    static S: OnceLock<Mutex<Vec<ToolSnapshot>>> = OnceLock::new();
    S.get_or_init(|| {
        Mutex::new(
            TOOL_REPOS
                .iter()
                .map(|(tool, _)| ToolSnapshot {
                    tool: (*tool).to_string(),
                    state: ToolState::Missing,
                    path: None,
                    source: None,
                    version: None,
                    last_error: None,
                })
                .collect(),
        )
    })
}

/// 加锁：中毒不级联（快照只用于展示，读旧值好过 panic）
fn lock_state() -> MutexGuard<'static, Vec<ToolSnapshot>> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// 只读访问全部快照（不克隆；回调内**不得**再调本模块其它会加锁的函数）
pub fn with_snapshots<R>(f: impl FnOnce(&[ToolSnapshot]) -> R) -> R {
    f(&lock_state())
}

/// 单个工具的状态快照；未知工具返回 `None`
pub fn snapshot(tool: &str) -> Option<ToolSnapshot> {
    lock_state().iter().find(|s| s.tool == tool).cloned()
}

/// 是否有工具正在下载（dock 动画判定；每帧调用，必须便宜）
pub fn any_downloading() -> bool {
    with_snapshots(|snaps| snaps.iter().any(|s| s.state == ToolState::Downloading))
}

/// 解析工具位置 + 版本号（会起一次 `<tool> --version`）
fn probe(tool: &str) -> (Option<PathBuf>, Option<ToolSource>, Option<String>) {
    match resolve(tool) {
        Some((cmd, source)) => (Some(PathBuf::from(&cmd)), Some(source), probe_version(&cmd)),
        None => (None, None, None),
    }
}

/// `--version` 输出的版本号：取首行中第一个「像版本号」的 token。
///
/// 不能用「最后一个 token」：ripgrep 15.x 打印 `ripgrep 15.2.0 (rev e89fff89ac)`，
/// 最后一个 token 是 commit id（`e89fff89ac)`），dock 里就会显示成当时提交的哈希片段。
/// 判据：去掉可能的 `v` 前缀与括号后，以数字开头且含 `.`（哈希不含点）。
fn parse_version_output(stdout: &str) -> Option<String> {
    stdout.lines().next()?.split_whitespace().find_map(|tok| {
        let token = tok
            .trim_start_matches('v')
            .trim_matches(|c| c == '(' || c == ')');
        (token.starts_with(|c: char| c.is_ascii_digit()) && token.contains('.'))
            .then(|| token.to_string())
    })
}

/// 起一次 `--version` 探测版本；命令缺失/非零退出/输出为空都返回 None
fn probe_version(cmd: &str) -> Option<String> {
    let out = Command::new(cmd).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    parse_version_output(&String::from_utf8_lossy(&out.stdout))
}

/// 重新探测并覆盖快照（启动检测、安装成功后调用）。
/// **不要**在下载进行中调用：它会把 `Downloading` 覆盖成 `Present`/`Missing`。
pub fn refresh_tool(tool: &str) {
    let (path, source, version) = probe(tool);
    let mut guard = lock_state();
    let Some(slot) = guard.iter_mut().find(|s| s.tool == tool) else {
        return;
    };

    slot.path = path;
    slot.source = source;
    slot.version = version;
    slot.last_error = None;
    slot.state = if slot.path.is_some() {
        ToolState::Present
    } else {
        ToolState::Missing
    };
}

/// 刷新全部工具的状态（启动检测用）
pub fn refresh_all() {
    for (tool, _) in TOOL_REPOS {
        refresh_tool(tool);
    }
}

/// 在飞守卫：同一把锁内「检查 + 置位」，避免并发双下载。
/// 返回 `false` = 该工具已在下载中。
fn begin_download(tool: &str) -> bool {
    let mut guard = lock_state();
    match guard.iter_mut().find(|s| s.tool == tool) {
        Some(slot) if slot.state != ToolState::Downloading => {
            slot.state = ToolState::Downloading;
            slot.last_error = None;
            true
        }
        _ => false,
    }
}

/// 记录下载失败：状态置 `Failed` 并重新探测位置——重装失败时旧副本可能仍然可用，
/// 那种情况下路径/版本要如实留在快照里（`Failed` 说的是「最近一次尝试」）。
fn fail_download(tool: &str, error: &str) {
    let (path, source, version) = probe(tool);
    let mut guard = lock_state();
    let Some(slot) = guard.iter_mut().find(|s| s.tool == tool) else {
        return;
    };

    slot.state = ToolState::Failed;
    slot.last_error = Some(error.to_string());
    slot.path = path;
    slot.source = source;
    slot.version = version;
}

/// 下载期间的 RAII 守卫：任何提前返回都不会把状态永久留在 `Downloading`
/// （那会让后续所有 `/download` 被「已在下载中」挡掉）。
struct DownloadGuard {
    /// 正在下载的工具名（Drop 兜底置失败时用）。
    tool: String,
    /// true = 调用方已写好终态，Drop 不再兜底标记失败。
    done: bool,
}

impl DownloadGuard {
    /// 构造守卫；`done` 初始为 false，表示调用方尚未写终态。
    fn new(tool: &str) -> Self {
        Self {
            tool: tool.to_string(),
            done: false,
        }
    }

    /// 收尾（状态已由调用方写好）：解除 Drop 兜底
    fn finish(mut self) {
        self.done = true;
    }
}

impl Drop for DownloadGuard {
    /// 未调用 `finish` 就析构（提前返回/panic）时，把工具状态兜底标记为下载失败。
    fn drop(&mut self) {
        if !self.done {
            fail_download(&self.tool, "download aborted");
        }
    }
}

/// 执行一次真正的下载安装（调用方负责「开始下载」提示与离线判定）。
/// 已在下载中 → 只发一条 Info 并返回 `None`。
async fn install(tool: &str, on_status: &(dyn Fn(ToolStatus) + Send + Sync)) -> Option<String> {
    let display = display_name(tool);
    if !begin_download(tool) {
        on_status(ToolStatus::Info(format!(
            "{display} is already downloading"
        )));
        return None;
    }

    let guard = DownloadGuard::new(tool);

    match download_tool(tool).await {
        Ok(path) => {
            ensure_bin_in_path();
            refresh_tool(tool);
            guard.finish();
            on_status(ToolStatus::Info(format!(
                "{display} installed to {}",
                path.display()
            )));
            Some(path.to_string_lossy().to_string())
        }
        Err(e) => {
            fail_download(tool, &e.to_string());
            guard.finish();
            on_status(ToolStatus::Warning(format!(
                "Failed to download {display}: {e}"
            )));
            None
        }
    }
}

/// 平台/架构对应的 GitHub asset 名
fn asset_name_for(tool: &str, version: &str) -> Option<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x86_64",
        "aarch64" => "aarch64",
        _ => return None,
    };
    let (os, triple) = match std::env::consts::OS {
        "macos" => ("darwin", format!("{arch}-apple-darwin")),
        "linux" => ("linux", format!("{arch}-unknown-linux-gnu")),
        "windows" => ("windows", format!("{arch}-pc-windows-msvc")),
        _ => return None,
    };
    let ext = if os == "windows" { "zip" } else { "tar.gz" };
    let asset = match tool {
        "fd" => format!("fd-v{version}-{triple}.{ext}"),
        // linux x86_64 用 musl 资产（静态链接可移植），aarch64 用 gnu
        "rg" if os == "linux" && arch == "x86_64" => {
            format!("ripgrep-{version}-x86_64-unknown-linux-musl.{ext}")
        }
        "rg" => format!("ripgrep-{version}-{triple}.{ext}"),
        _ => return None,
    };
    Some(asset)
}

/// release tag 前缀。ripgrep 的 tag 不带 `v`（`15.2.0`），fd 带 `v`（`v10.5.0`）。
fn tag_prefix(repo: &str) -> &'static str {
    match repo {
        "BurntSushi/ripgrep" => "",
        _ => "v",
    }
}

/// 拼出 release 下载 URL
fn release_url(repo: &str, version: &str, asset: &str) -> String {
    format!(
        "https://github.com/{repo}/releases/download/{}{version}/{asset}",
        tag_prefix(repo)
    )
}

/// 查询 GitHub 最新 release 版本号
async fn latest_version(repo: &str) -> Result<String> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let client = http::client_builder(None)
        .timeout(Duration::from_secs(15))
        .build()?;
    let resp = client
        .get(&url)
        .header("User-Agent", "prux-coding-agent")
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(Error::ProviderStatus { status, body });
    }
    let v: serde_json::Value = resp.json().await?;
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .unwrap_or_default();
    Ok(tag.trim_start_matches('v').to_string())
}

/// 下载文件到目标路径
async fn download_file(url: &str, dest: &Path) -> Result<()> {
    let client = http::client_builder(None)
        .timeout(Duration::from_secs(120))
        .build()?;
    let resp = client.get(url).send().await?;
    if !resp.status().is_success() {
        return Err(Error::ProviderStatus {
            status: resp.status(),
            body: format!("failed to download {url}"),
        });
    }
    let bytes = resp.bytes().await?;
    std::fs::write(dest, &bytes)?;
    Ok(())
}

/// 运行系统命令（解压用）；成功返回 None，失败返回错误信息
fn run_extraction(command: &str, args: &[&str]) -> Option<String> {
    match Command::new(command).args(args).output() {
        Ok(o) if o.status.success() => None,
        Ok(o) => Some(
            String::from_utf8_lossy(&o.stderr).trim().to_string()
                + String::from_utf8_lossy(&o.stdout).trim(),
        ),
        Err(e) => Some(format!("{command}: {e}")),
    }
}

/// 定位 Windows 自带的 `tar.exe`（优先 `%SystemRoot%\System32`）；
/// 找不到则回退到 PATH 中的 `tar.exe`。
fn windows_tar() -> String {
    if let Ok(root) = std::env::var("SystemRoot") {
        let p = PathBuf::from(root).join("System32").join("tar.exe");
        if p.is_file() {
            return p.to_string_lossy().to_string();
        }
    }
    "tar.exe".to_string()
}

/// 解压归档到 extract_dir；tar.gz 用 tar，zip 按平台用 tar/unzip/powershell
fn extract_archive(archive: &Path, extract_dir: &Path, asset: &str) -> Result<()> {
    let mut failures: Vec<String> = Vec::new();
    if asset.ends_with(".tar.gz") {
        if let Some(e) = run_extraction(
            "tar",
            &[
                "xzf",
                archive.to_str().unwrap(),
                "-C",
                extract_dir.to_str().unwrap(),
            ],
        ) {
            failures.push(format!("tar: {e}"));
        } else {
            return Ok(());
        }
    } else if asset.ends_with(".zip") {
        if cfg!(windows) {
            if let Some(e) = run_extraction(
                &windows_tar(),
                &[
                    "xf",
                    archive.to_str().unwrap(),
                    "-C",
                    extract_dir.to_str().unwrap(),
                ],
            ) {
                failures.push(format!("tar: {e}"));
            } else {
                return Ok(());
            }
            let script = "& { param($archive, $destination) $ErrorActionPreference = 'Stop'; Expand-Archive -LiteralPath $archive -DestinationPath $destination -Force }";
            if let Some(e) = run_extraction(
                "powershell.exe",
                &[
                    "-NoLogo",
                    "-NoProfile",
                    "-NonInteractive",
                    "-ExecutionPolicy",
                    "Bypass",
                    "-Command",
                    script,
                    archive.to_str().unwrap(),
                    extract_dir.to_str().unwrap(),
                ],
            ) {
                failures.push(format!("powershell: {e}"));
            } else {
                return Ok(());
            }
        } else {
            if let Some(e) = run_extraction(
                "unzip",
                &[
                    "-q",
                    archive.to_str().unwrap(),
                    "-d",
                    extract_dir.to_str().unwrap(),
                ],
            ) {
                failures.push(format!("unzip: {e}"));
            } else {
                return Ok(());
            }

            // tar 兜底尝试
            if let Some(e) = run_extraction(
                "tar",
                &[
                    "xf",
                    archive.to_str().unwrap(),
                    "-C",
                    extract_dir.to_str().unwrap(),
                ],
            ) {
                failures.push(format!("tar: {e}"));
            } else {
                return Ok(());
            }
        }
    } else {
        return Err(Error::msg(format!("unsupported archive format: {asset}")));
    }
    Err(Error::msg(format!(
        "failed to extract {asset}: {}",
        failures.join("; ")
    )))
}

/// 在解压目录中递归查找二进制
pub fn find_binary_recursively(root: &Path, binary_name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_file() && e.file_name() == binary_name {
                return Some(path);
            }
            if path.is_dir() {
                stack.push(path);
            }
        }
    }
    None
}

/// Unix 下设置可执行权限（Windows 无此概念）
#[cfg(unix)]
fn make_executable(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// 下载并安装单个工具；返回安装后的二进制路径
async fn download_tool(tool: &str) -> Result<PathBuf> {
    let repo = TOOL_REPOS
        .iter()
        .find(|(name, _)| *name == tool)
        .map(|(_, r)| *r)
        .ok_or_else(|| Error::msg(format!("unknown tool: {tool}")))?;

    let mut version = latest_version(repo).await?;
    // fd 新版不再发布 macOS x86_64 构建，钉住最后一个支持的版本
    if tool == "fd" && std::env::consts::OS == "macos" && std::env::consts::ARCH == "x86_64" {
        version = "10.3.0".to_string();
    }

    let asset = asset_name_for(tool, &version).ok_or_else(|| {
        Error::msg(format!(
            "unsupported platform: {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        ))
    })?;

    let dir = bin_dir();
    std::fs::create_dir_all(&dir)?;
    let archive_path = dir.join(&asset);
    let exe = tool_local_name(tool);
    let binary_path = dir.join(&exe);

    let url = release_url(repo, &version, &asset);
    download_file(&url, &archive_path).await?;

    // 解压到唯一临时目录（fd/rg 可能并发下载，避免共享目录竞争）
    let extract_dir = dir.join(format!("extract_tmp_{}_{}", tool, std::process::id()));
    // 并发同工具下载不存在（每个工具只装一次），但避免上次残留
    std::fs::remove_dir_all(&extract_dir).ok();
    std::fs::create_dir_all(&extract_dir)?;

    let result = (|| -> Result<()> {
        extract_archive(&archive_path, &extract_dir, &asset)?;
        // 归档内二进制可能直接在根目录或嵌套版本目录下
        let stripped = asset.trim_end_matches(".tar.gz").trim_end_matches(".zip");
        let candidates = [
            extract_dir.join(stripped).join(&exe),
            extract_dir.join(&exe),
        ];
        let found = candidates
            .into_iter()
            .find(|p| p.is_file())
            .or_else(|| find_binary_recursively(&extract_dir, &exe))
            .ok_or_else(|| Error::msg(format!("binary {exe} not found in archive {asset}")))?;

        // 目标可能已存在（`/download` 重装）：先落到临时名，删旧文件再改名——
        // Windows 的 rename 不覆盖已存在文件，直接 rename 会失败。
        let staging = dir.join(format!("{exe}.staging"));
        std::fs::remove_file(&staging).ok();
        std::fs::rename(&found, &staging)?;
        std::fs::remove_file(&binary_path).ok();
        std::fs::rename(&staging, &binary_path)?;

        #[cfg(unix)]
        make_executable(&binary_path)?;

        Ok(())
    })();

    // 清理归档与临时目录
    std::fs::remove_file(&archive_path).ok();
    std::fs::remove_dir_all(&extract_dir).ok();
    result?;

    Ok(binary_path)
}

/// 强制下载（跳过「已存在」短路）：供 `/download <tool>` 在用户确认后调用。
/// 这是本模块唯一的下载入口——启动路径只探测提示，不自动下载。
/// 并发由 [`install`] 里的在飞守卫兜住。
pub async fn download_now(
    tool: &str,
    on_status: &(dyn Fn(ToolStatus) + Send + Sync),
) -> Option<String> {
    if !is_known_tool(tool) {
        on_status(ToolStatus::Warning(unknown_tool_message(tool)));
        return None;
    }

    if runtime::offline() {
        on_status(ToolStatus::Warning(offline_skip_message(tool)));
        return None;
    }

    let display = display_name(tool);
    on_status(ToolStatus::Info(format!(
        "{display} re-download requested. Downloading..."
    )));

    install(tool, on_status).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bin_dir 追加在 PATH 末尾（不抢占系统同名命令），且已存在时不重复追加。
    #[test]
    fn bin_dir_appended_to_path_end() {
        let dir = PathBuf::from("/opt/prux/bin");

        // 不在 PATH → 追加到末尾（原有顺序不变）
        let current = std::env::join_paths(["/usr/bin", "/bin"]).unwrap();
        let out = path_with_bin_appended(Some(&current), &dir).unwrap();
        assert_eq!(
            std::env::split_paths(&out).collect::<Vec<_>>(),
            vec![
                PathBuf::from("/usr/bin"),
                PathBuf::from("/bin"),
                dir.clone()
            ]
        );

        // 已在 PATH（无论位置）→ 不改动
        let with_dir = std::env::join_paths(["/usr/bin", "/opt/prux/bin", "/bin"]).unwrap();
        assert!(path_with_bin_appended(Some(&with_dir), &dir).is_none());

        // 无 PATH → 只含 bin_dir
        let out = path_with_bin_appended(None, &dir).unwrap();
        assert_eq!(std::env::split_paths(&out).collect::<Vec<_>>(), vec![dir]);
    }

    #[test]
    fn asset_names_match_pi() {
        let fd = asset_name_for("fd", "10.3.0").unwrap();
        if cfg!(target_os = "macos") && cfg!(target_arch = "x86_64") {
            assert_eq!(fd, "fd-v10.3.0-x86_64-apple-darwin.tar.gz");
        }
        if cfg!(target_os = "linux") && cfg!(target_arch = "x86_64") {
            let fd = asset_name_for("fd", "10.3.0").unwrap();
            assert_eq!(fd, "fd-v10.3.0-x86_64-unknown-linux-gnu.tar.gz");
            let rg = asset_name_for("rg", "14.1.1").unwrap();
            assert_eq!(rg, "ripgrep-14.1.1-x86_64-unknown-linux-musl.tar.gz");
        }
    }

    #[test]
    fn release_url_matches_each_repo_tag_convention() {
        // ripgrep 的 tag 无 `v` 前缀；带 v 会 404
        assert_eq!(
            release_url(
                "BurntSushi/ripgrep",
                "15.2.0",
                "ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz"
            ),
            "https://github.com/BurntSushi/ripgrep/releases/download/15.2.0/\
             ripgrep-15.2.0-x86_64-unknown-linux-musl.tar.gz"
        );
        // fd 的 tag 带 `v` 前缀
        assert_eq!(
            release_url(
                "sharkdp/fd",
                "10.5.0",
                "fd-v10.5.0-x86_64-unknown-linux-gnu.tar.gz"
            ),
            "https://github.com/sharkdp/fd/releases/download/v10.5.0/\
             fd-v10.5.0-x86_64-unknown-linux-gnu.tar.gz"
        );
    }

    /// 本地 `<agent_dir>/bin` 副本优先于 PATH，且返回完整路径（grep/find 必须据此调起）
    #[test]
    fn local_bin_wins_and_resolves_to_absolute_path() {
        let _g = crate::test_support::AgentDirGuard::temp();
        let bin = bin_dir();
        std::fs::create_dir_all(&bin).unwrap();

        for tool in ["fd", "rg"] {
            let local = bin.join(tool_local_name(tool));
            std::fs::write(&local, b"stub").unwrap();

            let (cmd, source) = resolve(tool).expect("本地副本应当解析成功");
            assert_eq!(cmd, local.to_string_lossy(), "{tool}");
            assert_eq!(source, ToolSource::AgentBin, "{tool}");
            assert!(Path::new(&cmd).is_absolute(), "{tool}: {cmd}");
        }

        let expected_rg = bin
            .join(tool_local_name("rg"))
            .to_string_lossy()
            .to_string();
        assert_eq!(rg_path().as_deref(), Some(expected_rg.as_str()));
    }

    /// 测试用：把快照复位为初始态（全局表进程内共享，测试结束要还原）
    fn reset_snapshot(tool: &str) {
        let mut guard = lock_state();
        if let Some(slot) = guard.iter_mut().find(|s| s.tool == tool) {
            *slot = ToolSnapshot {
                tool: tool.to_string(),
                state: ToolState::Missing,
                path: None,
                source: None,
                version: None,
                last_error: None,
            };
        }
    }

    #[test]
    fn version_output_parses_both_formats() {
        assert_eq!(
            parse_version_output("fd 10.2.0\n").as_deref(),
            Some("10.2.0")
        );
        assert_eq!(
            parse_version_output("ripgrep 14.1.1\n\nfeatures:+pcre2\n").as_deref(),
            Some("14.1.1")
        );
        // ripgrep 15.x 带 `(rev <hash>)`：不能把 commit id 当成版本
        assert_eq!(
            parse_version_output("ripgrep 15.2.0 (rev e89fff89ac)\n\nfeatures:+pcre2\n").as_deref(),
            Some("15.2.0")
        );
        // 从哈希开头的提交也能排除（无 `.`）；带 `v` 前缀的能识别
        assert_eq!(parse_version_output("tool 8b2b1e1ac\n").as_deref(), None);
        assert_eq!(
            parse_version_output("tool v1.2.3\n").as_deref(),
            Some("1.2.3")
        );
        assert_eq!(parse_version_output("").as_deref(), None);
        assert_eq!(parse_version_output("\n").as_deref(), None);
    }

    /// 安装指引：内置依赖映射 + 下载地址 + `/download` 命令
    #[test]
    fn install_hint_names_repo_and_download_command() {
        assert_eq!(builtin_tool_for("fd"), Some("find"));
        assert_eq!(builtin_tool_for("rg"), Some("grep"));
        assert_eq!(builtin_tool_for("git"), None);

        assert_eq!(
            releases_url("fd").as_deref(),
            Some("https://github.com/sharkdp/fd/releases")
        );
        assert_eq!(
            releases_url("rg").as_deref(),
            Some("https://github.com/BurntSushi/ripgrep/releases")
        );

        let fd = install_hint("fd");
        assert!(
            fd.contains("https://github.com/sharkdp/fd/releases"),
            "{fd}"
        );
        assert!(fd.contains("/download fd"), "{fd}");
        assert!(fd.contains("find"), "{fd}");

        let rg = install_hint("rg");
        assert!(
            rg.contains("https://github.com/BurntSushi/ripgrep/releases"),
            "{rg}"
        );
        assert!(rg.contains("/download rg"), "{rg}");
        assert!(rg.contains("grep"), "{rg}");
    }

    #[test]
    fn tool_roster_is_tool_repos() {
        assert_eq!(tool_names(), vec!["fd", "rg"]);
        assert!(is_known_tool("fd") && is_known_tool("rg"));
        assert!(!is_known_tool("git"));
        assert_eq!(display_name("rg"), "ripgrep");
        assert_eq!(display_name("fd"), "fd");
        assert_eq!(
            unknown_tool_message("git"),
            "Unknown tool \"git\". Available: fd, rg"
        );
    }

    /// 状态机：在飞守卫拒绝并发 + 失败落态并保留最近错误。
    /// 全局快照表进程内共享，故只碰能确定性断言的字段，结束复位。
    #[test]
    fn download_guard_rejects_concurrent_and_failure_keeps_error() {
        assert!(!begin_download("nope"), "未知工具没有快照槽位");

        assert!(begin_download("fd"), "首次置位应成功");
        assert!(!begin_download("fd"), "下载中必须拒绝第二次");
        assert!(any_downloading());

        fail_download("fd", "boom");
        let snap = snapshot("fd").unwrap();
        assert_eq!(snap.state, ToolState::Failed);
        assert_eq!(snap.last_error.as_deref(), Some("boom"));

        reset_snapshot("fd");
        let snap = snapshot("fd").unwrap();
        assert_eq!(snap.state, ToolState::Missing);
        assert_eq!(snap.last_error, None);
    }
}

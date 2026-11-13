//! `backup` 扩展：把 prux 自己的用户资产打成一个可移植 zip 归档，并能就地恢复。
//!
//! **收什么**：只收 `agent_dir()` 下的 `settings.json`、`APPEND_SYSTEM.md`、`trust.json`、
//! `themes/`、`skills/` 与 `extensions/`（扩展配置、记忆、MCP 配置、workflows）。
//! 当前项目里的 `.prux/` 与 `AGENTS.md` **不进归档**：它们属于那个仓库，跟着仓库走。
//!
//! **不收什么**：`auth.json`（凭据）、`sessions/`、`history/`、`logs/`、`bin/`、`docs/`、
//! `models-store.json` 与 `recent-cwds.json`（可重建的缓存）、`backups/`（归档自己），
//! 以及 `extensions/` 下的 `worktrees/`（git 副本）、`permissions/logs/`（审计日志）、
//! `agent/`（运行态）、`memory/projects/`（按项目分文件的记忆）与 `schedules/`（定时任务）。符号链接与超过 [`MAX_FILE_BYTES`] 的单个文件跳过并记在报告里。
//!
//! 归档的第一个条目是 `manifest.json`：prux 版本、时间、来源目录，以及每个文件的
//! size 与 sha256。恢复先整份校验（路径合法 + 条目齐全 + 校验和一致）再写盘——
//! 任何一条对不上就一个字节都不写。
//!
//! 恢复**不覆盖**已存在的文件，除非显式 `--force`；`--force` 也先把原文件挪进
//! `agent_dir()/backups/recovery/<时间戳>/`（保留相对路径），手滑能再回来——
//! 放在 `backups/` 下是为了下一次打包不会把这份副本又收进归档。归档里的路径必须落在两份根目录之内
//! （绝对路径、`..`、盘符一律拒绝）。
//!
//! `/backup rm-recovery` 与 `/backup rm-all` 删掉 `backups/recovery/` 与 `backups/` 的内容，
//! 不二次确认（敲命令即是意图），只报告删了多少文件、释放多少字节。
//!
//! 路径参数以 `@` 开头时去掉那个 `@`：TUI 的路径补全带它，用户复制粘贴过来就是 `@/tmp/x`。

use crate::{
    core::{
        changelog::VERSION as PRUX_VERSION,
        extensions::{
            CommandHost, Extension, ExtensionCommand, ExtensionMode, ExtensionTool,
            ForkProjectInfo, SubcommandDef, UiNotifyLevel, register_host_command,
        },
        settings_manager::agent_dir,
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_BACKUP, command_arg},
    utils::{
        display::format_bytes,
        paths::{remove_children, remove_tree, resolve_path},
        text::hex_lower,
        time::{file_stamp, format_local, now_ms},
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    sync::Arc,
};

/// 扩展名（`/extension` 面板里的标识，也是 `/backup` 的命令归属）。
const EXT: &str = "backup";

/// 斜杠命令名（`/backup`）。
const CMD: &str = "backup";

/// 归档文件名前缀；`/backup list` 与「取最新一份」都按它认。
const ARCHIVE_PREFIX: &str = "prux-backup-";

/// 归档内的清单文件名（恢复时据此判断这是不是 prux 的备份）。
const MANIFEST: &str = "manifest.json";

/// 单个文件的大小上限：超过就跳过，避免一个误放的大二进制把归档撑爆。
const MAX_FILE_BYTES: u64 = 32 * 1024 * 1024;

/// `agent_dir()` 下整块不收的顶层条目（相对 `agent_dir()`）。
const SKIP_TOP: &[&str] = &[
    "auth.json",
    "backups",
    "bin",
    "docs",
    "history",
    "logs",
    "models-store.json",
    "recent-cwds.json",
    "sessions",
    "settings.json.lock",
];

/// 递归时不收的子树（以归档内路径判定，`/` 分隔）。
const SKIP_TREES: &[&str] = &[
    "extensions/agent",
    "extensions/memory/projects",
    "extensions/permissions/logs",
    "extensions/schedules",
    "extensions/worktrees",
];

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static BACKUP_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_BACKUP,
    make: || -> Arc<dyn Extension> { Arc::new(Backup) },
};

/// `/backup` 子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "create",
        description: "Write a zip of prux assets (default); /backup create <dir> picks the folder",
    },
    SubcommandDef {
        name: "list",
        description: "List the archives in a folder (default: agent_dir/backups)",
    },
    SubcommandDef {
        name: "restore",
        description: "Restore a zip (file or folder, `--force` overwrites into backups/recovery)",
    },
    SubcommandDef {
        name: "rm-recovery",
        description: "Delete the recovery copies in agent_dir/backups/recovery (no prompt)",
    },
    SubcommandDef {
        name: "rm-all",
        description: "Delete every archive and every recovery copy in agent_dir/backups (no prompt)",
    },
];

/// 归档内一个文件：归档路径（`/` 分隔）+ 来源绝对路径。
#[derive(Debug, Clone)]
struct Source {
    /// 归档内路径，如 `settings.json` / `extensions/mcp/mcp.json`。
    archive: String,
    /// 磁盘上的来源路径。
    path: PathBuf,
}

/// `manifest.json` 里的一条文件记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestEntry {
    /// 归档内路径（同时是恢复时的目标相对路径）。
    path: String,
    /// 字节数（恢复时与实际内容比对，抓截断）。
    size: u64,
    /// 内容的 sha256（小写 hex），恢复时校验。
    sha256: String,
}

/// `manifest.json` 的内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    /// 打包时的 prux 版本。
    prux_version: String,
    /// 打包时间（本地时间，`YYYY-MM-DD HH:MM:SS`；只给人看）。
    created_local: String,
    /// 打包时的 `agent_dir()`（恢复时只作展示，不参与定位）。
    agent_dir: String,
    /// 全部文件记录，顺序与归档内条目一致。
    entries: Vec<ManifestEntry>,
}

/// 一次打包的结果（命令层转成聊天区报告）。
#[derive(Debug)]
struct Created {
    /// 写进归档的文件数。
    entries: usize,
    /// 归档文件大小。
    bytes: u64,
    /// 被跳过的路径与原因。
    skipped: Vec<String>,
}

/// 一次恢复的结果。
#[derive(Debug)]
struct Restored {
    /// 写回的文件数。
    written: usize,
    /// 其中覆盖了已有文件的数量。
    replaced: usize,
    /// 覆盖前挪进 `backups/recovery/<时间戳>/` 的原文件路径。
    backups: Vec<PathBuf>,
    /// 本次恢复副本的根目录（`backups/recovery/<时间戳>`），报告里给用户看的落点。
    recovery: PathBuf,
}

/// `backup` 扩展：只有一条 `/backup` 命令，无工具、无钩子。
pub struct Backup;

impl Extension for Backup {
    /// 扩展名 `backup`。
    fn name(&self) -> &str {
        EXT
    }

    /// 来源：移植自 ai-memory 的 `backup-agents`。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "ai-memory".to_string(),
            plugin_version: "3.3.1".to_string(),
            url: "https://github.com/akitaonrails/ai-memory".to_string(),
        })
    }

    /// 面板说明：打包/恢复 prux 自己的资产。
    fn description(&self) -> &str {
        "Snapshot prux assets (settings, extension configs, memory, skills) into a zip, or restore \
         one. Project files are not included. /backup [create|list|restore] [dir|@dir] [--force]"
    }

    /// 仅 `Dev` / `Creator` 模式可用：它会写盘（restore 会覆盖配置）。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认关闭：写盘与覆盖配置由用户显式决定。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供工具：备份是用户的动作，不是模型该自己发起的。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/backup`。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description:
                "Snapshot prux assets to a zip, list archives, restore one, or clean backups"
                    .to_string(),
            busy_safe: true,
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 接线 `/backup` 执行入口。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_backup);
    }
}

/// `/backup` 的子命令（解析结果；空参数按 `Create`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// `/backup` 或 `/backup create [dir]`：写一份新归档。
    Create,
    /// `/backup list [dir]`：列出归档。
    List,
    /// `/backup restore [file|dir] [--force]`：恢复归档。
    Restore,
    /// `/backup rm-recovery`：清空 `backups/recovery/` 下的恢复副本。
    RmRecovery,
    /// `/backup rm-all`：清空 `backups/` 下的全部内容（归档 + 恢复副本）。
    RmAll,
    /// 认不出的子命令（原样带给错误提示）。
    Unknown,
}

/// 子命令字符串 → [`Action`]；与 [`SUBCOMMANDS`] 的一致性由测试守住。
fn parse_action(sub: &str) -> Action {
    match sub {
        "" | "create" => Action::Create,
        "list" => Action::List,
        "restore" => Action::Restore,
        "rm-recovery" => Action::RmRecovery,
        "rm-all" => Action::RmAll,
        _ => Action::Unknown,
    }
}

/// `/backup` 命令入口：`create`（默认）/ `list` / `restore` / `rm-recovery` / `rm-all`。
fn command_backup(host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw);
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((sub, rest)) => (sub.trim(), rest.trim()),
        None => (arg.trim(), ""),
    };

    let agent = agent_dir();

    match parse_action(sub) {
        Action::Create => run_create(host, rest, &agent),
        Action::List => run_list(host, rest, &agent),
        Action::Restore => run_restore(host, rest, &agent),
        Action::RmRecovery => run_rm(host, &default_dir(&agent).join("recovery"), true),
        Action::RmAll => run_rm(host, &default_dir(&agent), false),
        Action::Unknown => host.push_message(
            &format!(
                "backup: unknown subcommand `{sub}` — use /backup create|list|restore|rm-recovery|rm-all"
            ),
            UiNotifyLevel::Error,
        ),
    }
    false
}

/// `/backup create [dir]`：默认写到 `agent_dir()/backups`。
fn run_create(host: &mut dyn CommandHost, rest: &str, agent: &Path) {
    let dir = match resolve_target(rest, host.cwd().as_path(), &default_dir(agent)) {
        Ok(dir) => dir,
        Err(e) => {
            host.push_message(&format!("backup: {e}"), UiNotifyLevel::Error);
            return;
        }
    };
    let name = format!("{ARCHIVE_PREFIX}{}.zip", file_stamp());
    let dest = dir.join(name);

    match create_archive(&dest, agent) {
        Ok(created) => {
            let mut lines = vec![format!(
                "backup: {} files → {} ({})",
                created.entries,
                dest.display(),
                format_bytes(created.bytes)
            )];
            if !created.skipped.is_empty() {
                lines.push(format!("{} skipped:", created.skipped.len()));
                lines.extend(created.skipped.iter().take(20).map(|s| format!("  {s}")));
                if created.skipped.len() > 20 {
                    lines.push(format!("  … {} more", created.skipped.len() - 20));
                }
            }
            lines.push(
                "may contain credentials from extension configs (web-search, proxy, ssh-remote); \
                 restore with /backup restore <file>"
                    .to_string(),
            );
            host.push_message(&lines.join("\n"), UiNotifyLevel::Success);
        }
        Err(e) => host.push_message(&format!("backup: {e}"), UiNotifyLevel::Error),
    }
}

/// `/backup list [dir]`：按修改时间倒序列出该目录里的归档。
fn run_list(host: &mut dyn CommandHost, rest: &str, agent: &Path) {
    let dir = match resolve_target(rest, host.cwd().as_path(), &default_dir(agent)) {
        Ok(dir) => dir,
        Err(e) => {
            host.push_message(&format!("backup: {e}"), UiNotifyLevel::Error);
            return;
        }
    };

    let archives = match list_archives(&dir) {
        Ok(list) => list,
        Err(e) => {
            host.push_message(&format!("backup: {e}"), UiNotifyLevel::Error);
            return;
        }
    };
    if archives.is_empty() {
        host.push_message(
            &format!("backup: no archives in {}", dir.display()),
            UiNotifyLevel::Info,
        );
        return;
    }

    let mut lines = vec![format!(
        "backup: {} archives in {}",
        archives.len(),
        dir.display()
    )];
    for path in archives {
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(crate::utils::time::system_to_ms)
            .unwrap_or(0) as i64;
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unnamed>");
        lines.push(format!(
            "  {name}  {}  {}",
            format_bytes(size),
            format_local(modified, "%Y-%m-%d %H:%M")
        ));
    }
    host.push_message(&lines.join("\n"), UiNotifyLevel::Info);
}

/// `/backup restore <file|dir|@path> [--force]`：先整份校验，再写盘。
fn run_restore(host: &mut dyn CommandHost, rest: &str, agent: &Path) {
    let (arg, force) = split_force(rest);
    let archive = match pick_archive(&arg, host.cwd().as_path(), &default_dir(agent)) {
        Ok(path) => path,
        Err(e) => {
            host.push_message(&format!("backup: {e}"), UiNotifyLevel::Error);
            return;
        }
    };

    match restore_archive(&archive, agent, force) {
        Ok(done) => {
            let mut lines = vec![format!(
                "backup: restored {} files from {} ({} replaced)",
                done.written,
                archive.display(),
                done.replaced
            )];
            if !done.backups.is_empty() {
                lines.push(format!(
                    "previous copies moved to {}:",
                    done.recovery.display()
                ));
                lines.extend(
                    done.backups
                        .iter()
                        .take(10)
                        .map(|p| format!("  {}", p.display())),
                );
                if done.backups.len() > 10 {
                    lines.push(format!("  … {} more", done.backups.len() - 10));
                }
            }
            lines.push("restart prux to pick up restored settings".to_string());
            host.push_message(&lines.join("\n"), UiNotifyLevel::Success);
        }
        Err(e) => host.push_message(&format!("backup: {e}"), UiNotifyLevel::Error),
    }
}

/// `/backup rm-recovery` 与 `/backup rm-all`：删掉 `dir` 这一棵（`keep` 时保留目录本身），
/// 不二次确认——报告删掉的文件数与释放的字节数。
fn run_rm(host: &mut dyn CommandHost, dir: &Path, keep: bool) {
    if !dir.exists() {
        host.push_message(
            &format!(
                "backup: nothing to remove, {} does not exist",
                dir.display()
            ),
            UiNotifyLevel::Info,
        );
        return;
    }
    let result = if keep {
        remove_tree(dir)
    } else {
        remove_children(dir)
    };
    match result {
        Ok((files, bytes)) => {
            let level = if files == 0 {
                UiNotifyLevel::Info
            } else {
                UiNotifyLevel::Success
            };
            host.push_message(
                &format!(
                    "backup: removed {files} files ({} freed) from {}",
                    format_bytes(bytes),
                    dir.display()
                ),
                level,
            );
        }
        Err(e) => host.push_message(&format!("backup: {e}"), UiNotifyLevel::Error),
    }
}

/// 默认归档目录：`agent_dir()/backups`。
fn default_dir(agent: &Path) -> PathBuf {
    agent.join("backups")
}

/// 去掉 TUI `@` 补全留下的前导 `@`（只去掉一个），返回裸路径。
fn clean_path(raw: &str) -> &str {
    raw.strip_prefix('@').unwrap_or(raw)
}

/// 把用户给的参数解析成绝对路径（`~`/`~/` 展开，相对路径按 `cwd` 展开）；空参数取 `fallback`。
fn resolve_target(raw: &str, cwd: &Path, fallback: &Path) -> Result<PathBuf, String> {
    let raw = clean_path(raw.trim());
    if raw.is_empty() {
        return Ok(fallback.to_path_buf());
    }
    Ok(resolve_path(raw, &cwd.to_string_lossy()))
}

/// 把 `restore` 的参数拆成「路径」与 `--force`（`force` 也认，位置不限）。
fn split_force(raw: &str) -> (String, bool) {
    let mut force = false;
    let mut words: Vec<&str> = Vec::new();
    for word in raw.split_whitespace() {
        if word == "--force" || word.eq_ignore_ascii_case("force") {
            force = true;
        } else {
            words.push(word);
        }
    }
    (words.join(" "), force)
}

/// 解析要恢复的归档：给文件就用它；给目录（或空）就用里面最新的一份。
fn pick_archive(raw: &str, cwd: &Path, fallback: &Path) -> Result<PathBuf, String> {
    let target = resolve_target(raw, cwd, fallback)?;
    if target.is_file() {
        return Ok(target);
    }
    if target.is_dir() {
        return match list_archives(&target)?.into_iter().next() {
            Some(path) => Ok(path),
            None => Err(format!("no archives in {}", target.display())),
        };
    }
    // 路径还不存在：明确说清是「没这个文件」还是「没这个目录」，别猜。
    Err(format!(
        "no such archive or folder: {} (use /backup create first)",
        target.display()
    ))
}

/// 列出目录里名字以 [`ARCHIVE_PREFIX`] 开头的 `.zip`，按修改时间从新到旧。
fn list_archives(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("read {}: {e}", dir.display())),
    };

    let mut found: Vec<(u64, PathBuf)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(name) => name,
            None => continue,
        };
        if !name.starts_with(ARCHIVE_PREFIX) || !name.ends_with(".zip") {
            continue;
        }
        let modified = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .map(crate::utils::time::system_to_ms)
            .unwrap_or(0);
        found.push((modified, path));
    }
    // 时间戳一样（同一秒内连打两次）时按文件名倒序，保证顺序确定。
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    Ok(found.into_iter().map(|(_, path)| path).collect())
}

/// 收集要打包的文件：`agent_dir()` 的顶层条目（跳过 [`SKIP_TOP`]），递归收子树。
fn collect_sources(agent: &Path) -> (Vec<Source>, Vec<String>) {
    let mut sources = Vec::new();
    let mut skipped = Vec::new();

    if let Ok(read) = std::fs::read_dir(agent) {
        let mut top: Vec<PathBuf> = read.flatten().map(|e| e.path()).collect();
        top.sort();
        for path in top {
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if SKIP_TOP.contains(&name) {
                continue;
            }
            push_tree(&path, name, &mut sources, &mut skipped);
        }
    }

    sources.sort_by(|a, b| a.archive.cmp(&b.archive));
    (sources, skipped)
}

/// 递归收 `path` 为归档内 `archive`：符号链接、超限文件、[`SKIP_TREES`] 子树跳过。
fn push_tree(path: &Path, archive: &str, sources: &mut Vec<Source>, skipped: &mut Vec<String>) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    let kind = meta.file_type();
    if kind.is_symlink() {
        skipped.push(format!("{archive} (symlink)"));
        return;
    }
    if kind.is_dir() {
        if SKIP_TREES.contains(&archive) {
            return;
        }
        let Ok(read) = std::fs::read_dir(path) else {
            skipped.push(format!("{archive}/ (unreadable)"));
            return;
        };
        let mut children: Vec<PathBuf> = read.flatten().map(|e| e.path()).collect();
        children.sort();
        for child in children {
            let Some(name) = child.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            push_tree(&child, &format!("{archive}/{name}"), sources, skipped);
        }
        return;
    }
    if !kind.is_file() {
        return;
    }
    if meta.len() > MAX_FILE_BYTES {
        skipped.push(format!(
            "{archive} ({} > {})",
            format_bytes(meta.len()),
            format_bytes(MAX_FILE_BYTES)
        ));
        return;
    }
    sources.push(Source {
        archive: archive.to_string(),
        path: path.to_path_buf(),
    });
}

/// 写归档：先读全部内容算清单，再按「manifest 在前」写入，最后原子改名到位。
fn create_archive(dest: &Path, agent: &Path) -> Result<Created, String> {
    let (sources, skipped) = collect_sources(agent);
    if sources.is_empty() {
        return Err(format!("no assets found under {}", agent.display()));
    }

    let mut payload: Vec<(String, Vec<u8>, ManifestEntry)> = Vec::with_capacity(sources.len());
    for source in &sources {
        let bytes = std::fs::read(&source.path)
            .map_err(|e| format!("read {}: {e}", source.path.display()))?;
        let entry = ManifestEntry {
            path: source.archive.clone(),
            size: bytes.len() as u64,
            sha256: hex_lower(&Sha256::digest(&bytes)),
        };
        payload.push((source.archive.clone(), bytes, entry));
    }

    let manifest = Manifest {
        prux_version: PRUX_VERSION.to_string(),
        created_local: format_local(now_ms() as i64, "%Y-%m-%d %H:%M:%S"),
        agent_dir: agent.display().to_string(),
        entries: payload.iter().map(|(_, _, entry)| entry.clone()).collect(),
    };
    let manifest_json =
        serde_json::to_vec_pretty(&manifest).map_err(|e| format!("serialize manifest: {e}"))?;

    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;

    // 先写同目录临时文件，成功后再 rename：中途失败不会留下半份归档。
    let tmp = dest.with_file_name(format!(
        ".{}.tmp{}",
        dest.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("backup"),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let file = std::fs::File::create(&tmp).map_err(|e| format!("create {}: {e}", tmp.display()))?;
    restrict(&file);

    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .unix_permissions(0o600);
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file(MANIFEST, options)
        .map_err(|e| format!("write {MANIFEST}: {e}"))?;
    zip.write_all(&manifest_json)
        .map_err(|e| format!("write {MANIFEST}: {e}"))?;
    for (name, bytes, _) in &payload {
        zip.start_file(name.as_str(), options)
            .map_err(|e| format!("write {name}: {e}"))?;
        zip.write_all(bytes)
            .map_err(|e| format!("write {name}: {e}"))?;
    }
    let file = zip.finish().map_err(|e| format!("finish archive: {e}"))?;
    file.sync_all()
        .map_err(|e| format!("sync {}: {e}", tmp.display()))?;
    drop(file);

    std::fs::rename(&tmp, dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("rename {} → {}: {e}", tmp.display(), dest.display())
    })?;

    let bytes = std::fs::metadata(dest).map(|m| m.len()).unwrap_or(0);
    Ok(Created {
        entries: payload.len(),
        bytes,
        skipped,
    })
}

/// 归档在磁盘上只给本人读写（Unix）；其它平台靠调用方自保。
fn restrict(file: &File) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = file;
}

/// 归档内路径 → 相对路径：拒绝绝对路径、`..`、盘符前缀与空路径。
fn safe_relative(name: &str) -> Result<PathBuf, String> {
    // Windows 上 `\` 也是分隔符，统一成 `/` 再按组件检查。
    let normalized = name.replace('\\', "/");
    // Unix 上 `C:/x` 会被当成两级普通路径，恢复后凭空多出一个 `C:` 目录；
    // 盘符在两种平台上都按不安全处理。
    let bytes = normalized.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() {
        return Err(format!("unsafe path in archive: {name}"));
    }
    let path = Path::new(&normalized);
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Normal(seg) => out.push(seg),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(format!("unsafe path in archive: {name}"));
            }
        }
    }
    if out.as_os_str().is_empty() {
        return Err(format!("empty path in archive: {name}"));
    }
    Ok(out)
}

/// 归档内路径 → 恢复目标（一律落在 `agent_dir()` 之内）。
fn target_for(archive: &str, agent: &Path) -> Result<PathBuf, String> {
    Ok(agent.join(safe_relative(archive)?))
}

/// 恢复归档：整份校验（清单存在、条目齐全、校验和一致）通过后才开始写盘。
fn restore_archive(archive: &Path, agent: &Path, force: bool) -> Result<Restored, String> {
    let file = File::open(archive).map_err(|e| format!("open {}: {e}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file)
        .map_err(|e| format!("{} is not a zip: {e}", archive.display()))?;

    let manifest: Manifest = {
        let mut entry = zip.by_name(MANIFEST).map_err(|_| {
            format!(
                "{} has no {MANIFEST} — not a prux backup",
                archive.display()
            )
        })?;
        let mut text = String::new();
        entry
            .read_to_string(&mut text)
            .map_err(|e| format!("read {MANIFEST}: {e}"))?;
        serde_json::from_str(&text).map_err(|e| format!("parse {MANIFEST}: {e}"))?
    };

    let mut plan: Vec<(PathBuf, Vec<u8>)> = Vec::with_capacity(manifest.entries.len());
    for expected in &manifest.entries {
        let target = target_for(&expected.path, agent)?;
        let mut entry = zip
            .by_name(&expected.path)
            .map_err(|_| format!("archive is missing {}", expected.path))?;
        let mut bytes = Vec::with_capacity(expected.size as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| format!("read {}: {e}", expected.path))?;
        if bytes.len() as u64 != expected.size {
            return Err(format!(
                "{} is {} bytes, manifest says {} (truncated archive?)",
                expected.path,
                bytes.len(),
                expected.size
            ));
        }
        let actual = hex_lower(&Sha256::digest(&bytes));
        if actual != expected.sha256 {
            return Err(format!(
                "{} failed its checksum (corrupted archive)",
                expected.path
            ));
        }
        plan.push((target, bytes));
    }

    let conflicts: Vec<&PathBuf> = plan
        .iter()
        .map(|(target, _)| target)
        .filter(|target| target.exists())
        .collect();
    if !conflicts.is_empty() && !force {
        let mut lines = vec![format!(
            "{} of {} files already exist — nothing was written:",
            conflicts.len(),
            plan.len()
        )];
        lines.extend(
            conflicts
                .iter()
                .take(8)
                .map(|p| format!("  {}", p.display())),
        );
        if conflicts.len() > 8 {
            lines.push(format!("  … {} more", conflicts.len() - 8));
        }
        lines.push(
            "re-run with --force to overwrite (originals are moved to backups/recovery/<stamp>/)"
                .to_string(),
        );
        return Err(lines.join("\n"));
    }

    let stamp = file_stamp();
    let recovery = default_dir(agent).join("recovery").join(&stamp);
    let mut restored = Restored {
        written: 0,
        replaced: 0,
        backups: Vec::new(),
        recovery: recovery.clone(),
    };
    for (target, bytes) in plan {
        if target.exists() {
            let rel = target
                .strip_prefix(agent)
                .map_err(|_| format!("{} is outside {}", target.display(), agent.display()))?;
            let backup = free_path(&recovery.join(rel))?;
            if let Some(parent) = backup.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("create {}: {e}", parent.display()))?;
            }
            std::fs::rename(&target, &backup)
                .map_err(|e| format!("move {} aside: {e}", target.display()))?;
            restored.backups.push(backup);
            restored.replaced += 1;
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }
        std::fs::write(&target, &bytes).map_err(|e| format!("write {}: {e}", target.display()))?;
        restored.written += 1;
    }
    Ok(restored)
}

/// 恢复副本的落地路径：`path` 已存在时追加 `-1`、`-2`…（同一秒恢复两次也不会互相覆盖）。
fn free_path(path: &Path) -> Result<PathBuf, String> {
    if !path.exists() {
        return Ok(path.to_path_buf());
    }
    for n in 1..1000 {
        let mut candidate = path.as_os_str().to_os_string();
        candidate.push(format!("-{n}"));
        let candidate = PathBuf::from(candidate);
        if !candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(format!("cannot find a free name for {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    /// 声明给输入框的子命令都必须被解析器认识（文档与 handler 不许走散）。
    #[test]
    fn declared_subcommands_match_the_parser() {
        for def in SUBCOMMANDS {
            assert_ne!(parse_action(def.name), Action::Unknown, "{}", def.name);
        }
        assert_eq!(parse_action(""), Action::Create);
        assert_eq!(parse_action("nope"), Action::Unknown);
    }

    /// `rm-recovery` 只清 recovery 树、`rm-all` 连归档一起清，两者都保留 `backups/` 本身。
    #[test]
    fn remove_trees_only_touch_what_they_should() {
        let _guard = AgentDirGuard::temp();
        let agent = agent_dir();
        let backups = agent.join("backups");
        let recovery = backups.join("recovery/20260101-000000");
        std::fs::create_dir_all(recovery.join("skills/demo")).unwrap();
        std::fs::write(recovery.join("settings.json"), b"old").unwrap();
        std::fs::write(recovery.join("skills/demo/SKILL.md"), b"old").unwrap();
        std::fs::write(backups.join("one.zip"), b"zip").unwrap();

        let (files, bytes) = remove_tree(&backups.join("recovery")).unwrap();
        assert_eq!((files, bytes), (2, 6));
        assert!(!backups.join("recovery").exists());
        assert!(backups.join("one.zip").exists(), "rm-recovery 不碰归档");

        let (files, bytes) = remove_children(&backups).unwrap();
        assert_eq!((files, bytes), (1, 3));
        assert!(backups.exists(), "backups/ 目录本身保留");
        assert_eq!(std::fs::read_dir(&backups).unwrap().count(), 0);
    }

    /// 造一个 agent 目录：既有该备份的资产，也有必须排除的文件。
    fn seed(agent: &Path) {
        std::fs::create_dir_all(agent.join("skills/demo")).unwrap();
        std::fs::create_dir_all(agent.join("extensions/mcp")).unwrap();
        std::fs::create_dir_all(agent.join("sessions")).unwrap();
        std::fs::create_dir_all(agent.join("extensions/permissions/logs")).unwrap();
        std::fs::create_dir_all(agent.join("extensions/memory/projects")).unwrap();
        std::fs::create_dir_all(agent.join("extensions/schedules")).unwrap();

        std::fs::write(agent.join("settings.json"), b"{\"theme\":\"x\"}").unwrap();
        std::fs::write(agent.join("skills/demo/SKILL.md"), b"# demo").unwrap();
        std::fs::write(
            agent.join("extensions/mcp/mcp.json"),
            b"{\"mcpServers\":{}}",
        )
        .unwrap();
        std::fs::write(agent.join("auth.json"), b"secret").unwrap();
        std::fs::write(agent.join("sessions/s1.json"), b"huge history").unwrap();
        std::fs::write(agent.join("extensions/permissions/logs/a.log"), b"audit").unwrap();
        std::fs::write(agent.join("extensions/memory/projects/p.json"), b"{}").unwrap();
        std::fs::write(agent.join("extensions/schedules/s.json"), b"{}").unwrap();
    }

    /// `~`/`~/` 展开成家目录，`@` 前缀照旧剥掉，相对路径仍按 cwd 拼。
    #[test]
    fn resolve_target_expands_tilde() {
        let _home = crate::test_support::HomeGuard::temp();
        let home_path = crate::utils::paths::home_dir();
        let cwd = Path::new("/work");
        let fallback = Path::new("/default");

        assert_eq!(
            resolve_target("~/x", cwd, fallback).unwrap(),
            home_path.join("x")
        );
        assert_eq!(resolve_target("~", cwd, fallback).unwrap(), home_path);
        assert_eq!(
            resolve_target("@~/x.zip", cwd, fallback).unwrap(),
            home_path.join("x.zip")
        );
        assert_eq!(
            resolve_target("rel/x", cwd, fallback).unwrap(),
            PathBuf::from("/work/rel/x")
        );
        assert_eq!(resolve_target("", cwd, fallback).unwrap(), fallback);
    }

    /// `@` 只剥一层，其它原样。
    #[test]
    fn clean_path_strips_a_single_at() {
        assert_eq!(clean_path("@/tmp/x"), "/tmp/x");
        assert_eq!(clean_path("@@x"), "@x");
        assert_eq!(clean_path("/tmp/x"), "/tmp/x");
        assert_eq!(clean_path(""), "");
    }

    /// `--force` 可以从任意位置出现，剩下的仍然是一条（可能带空格的）路径。
    #[test]
    fn split_force_takes_the_flag_out() {
        assert_eq!(split_force(""), (String::new(), false));
        assert_eq!(split_force("--force"), (String::new(), true));
        assert_eq!(
            split_force("a/b.zip --force"),
            ("a/b.zip".to_string(), true)
        );
        assert_eq!(
            split_force("/tmp/my dir --force"),
            ("/tmp/my dir".to_string(), true)
        );
    }

    /// 归档内路径不许跑出根目录。
    #[test]
    fn unsafe_archive_paths_are_rejected() {
        for bad in ["../evil", "/etc/passwd", "a/../../b", "", "C:/x"] {
            assert!(safe_relative(bad).is_err(), "{bad} 不该被接受");
        }
        assert_eq!(
            safe_relative("extensions/mcp/mcp.json").unwrap(),
            PathBuf::from("extensions/mcp/mcp.json")
        );
        // Windows 风格分隔符按 `/` 处理后再判定。
        assert_eq!(safe_relative("a\\b").unwrap(), PathBuf::from("a/b"));
        assert!(safe_relative("..\\evil").is_err());
    }

    /// 打包 → 删源码 → 恢复：资产回来了，排除项从没进过归档。
    #[test]
    fn create_then_restore_round_trips_and_skips_excluded() {
        let _guard = AgentDirGuard::temp();
        let agent = agent_dir();
        seed(&agent);

        let archive = agent.join("backups/one.zip");
        let created = create_archive(&archive, &agent).expect("打包失败");
        assert_eq!(created.entries, 3, "3 份资产（不含被排除的）");
        assert!(
            created.skipped.is_empty(),
            "没有需要跳过的: {:?}",
            created.skipped
        );

        let names = archive_names(&archive);
        assert!(names.contains(&MANIFEST.to_string()));
        assert!(names.contains(&"settings.json".to_string()));
        assert!(!names.iter().any(|n| n.contains("auth.json")));
        assert!(!names.iter().any(|n| n.contains("sessions")));
        assert!(!names.iter().any(|n| n.contains("permissions/logs")));
        assert!(!names.iter().any(|n| n.contains("memory/projects")));
        assert!(!names.iter().any(|n| n.contains("schedules")));

        // 把带进归档的资产全删掉，恢复时目标必须一个都不剩。
        std::fs::remove_dir_all(agent.join("skills")).unwrap();
        std::fs::remove_dir_all(agent.join("extensions")).unwrap();
        std::fs::remove_file(agent.join("settings.json")).unwrap();

        let done = restore_archive(&archive, &agent, false).expect("恢复失败");
        assert_eq!(done.written, 3);
        assert_eq!(done.replaced, 0);
        assert_eq!(
            std::fs::read(agent.join("settings.json")).unwrap(),
            b"{\"theme\":\"x\"}"
        );
    }

    /// 目标已存在且不给 `--force`：一个字节都不写，报错里点名冲突文件。
    #[test]
    fn restore_refuses_conflicts_without_force() {
        let _guard = AgentDirGuard::temp();
        let agent = agent_dir();
        seed(&agent);

        let archive = agent.join("backups/one.zip");
        create_archive(&archive, &agent).expect("打包失败");
        std::fs::write(agent.join("settings.json"), b"{\"theme\":\"changed\"}").unwrap();

        let error = restore_archive(&archive, &agent, false).unwrap_err();
        assert!(error.contains("settings.json"), "报错要点名冲突: {error}");
        assert_eq!(
            std::fs::read(agent.join("settings.json")).unwrap(),
            b"{\"theme\":\"changed\"}",
            "拒绝时不得覆盖"
        );

        // --force 覆盖，原文件挪进 backups/recovery/<stamp>/（打包时不会又收进归档）
        let done = restore_archive(&archive, &agent, true).expect("强制恢复失败");
        assert!(done.replaced > 0);
        assert_eq!(
            std::fs::read(agent.join("settings.json")).unwrap(),
            b"{\"theme\":\"x\"}"
        );
        let aside = done.recovery.join("settings.json");
        assert!(done.backups.contains(&aside), "副本要保留归档内的相对路径");
        assert_eq!(
            std::fs::read(&aside).unwrap(),
            b"{\"theme\":\"changed\"}",
            "覆盖前的原文件应完整挪到 recovery 目录"
        );
        assert!(done.recovery.starts_with(agent.join("backups/recovery")));

        let again = agent.join("backups/two.zip");
        let created = create_archive(&again, &agent).expect("再次打包失败");
        assert_eq!(created.entries, 3, "recovery 目录不该被打进归档");
        assert!(
            !archive_names(&again)
                .iter()
                .any(|n| n.contains("bak-") || n.contains("recovery")),
            "归档里不该出现恢复副本"
        );
    }

    /// 目录里有两份归档时，`pick_archive` 取最新的一份。
    #[test]
    fn pick_archive_prefers_the_newest_in_a_folder() {
        let _guard = AgentDirGuard::temp();
        let agent = agent_dir();
        let dir = agent.join("backups");
        std::fs::create_dir_all(&dir).unwrap();
        let older = dir.join(format!("{ARCHIVE_PREFIX}2020.zip"));
        let newer = dir.join(format!("{ARCHIVE_PREFIX}2021.zip"));
        std::fs::write(&older, b"x").unwrap();
        std::fs::write(&newer, b"x").unwrap();
        // 文件系统时间精度差别大，直接把比较口径钉在名字上更稳：这里只验证前缀过滤。
        let listed = list_archives(&dir).unwrap();
        assert_eq!(listed.len(), 2);
        assert!(listed.iter().all(|p| p.starts_with(&dir)));

        let picked = pick_archive("", &dir, &dir).expect("应能挑到一份");
        assert!(
            picked
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(ARCHIVE_PREFIX)
        );
    }

    /// 不是 prux 备份的 zip 被拒绝，且错误文案点出缺清单。
    #[test]
    fn restore_rejects_a_plain_zip() {
        let _guard = AgentDirGuard::temp();
        let agent = agent_dir();
        let path = agent.join("plain.zip");
        std::fs::create_dir_all(&agent).unwrap();
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file("hello.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"hi").unwrap();
        zip.finish().unwrap();

        let error = restore_archive(&path, &agent, false).unwrap_err();
        assert!(error.contains(MANIFEST), "{error}");
    }

    /// 读出归档里的条目名（断言排除项没进包）。
    fn archive_names(path: &Path) -> Vec<String> {
        let file = std::fs::File::open(path).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        (0..zip.len())
            .map(|i| {
                let entry = zip.by_index(i).unwrap();
                entry.name().unwrap().to_string()
            })
            .collect()
    }
}

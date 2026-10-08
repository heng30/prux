//! 崩溃日志（`agent_dir()/crashes.json`）。
//!
//! 进程内发生未捕获 panic 时把记录追加进 JSON 文件（最多保留 [`MAX_CRASH_RECORDS`] 条），
//! 下次启动读取「未被提示过且未过期」的一条提醒用户运行 `/bug`，`/bug` 导出时把剩余记录一并附上并清空。
//!
//! 写入路径全程 best-effort：崩溃现场自身不能再崩，失败静默。

use crate::{
    core::changelog::VERSION,
    core::settings_manager::agent_dir,
    utils::time::{now_iso, now_ms, parse_iso_ms},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    panic::PanicHookInfo,
    path::{Path, PathBuf},
};

/// 最多保留的记录条数（超出丢弃最旧的）
const MAX_CRASH_RECORDS: usize = 5;

/// 超过该时长（7 天）的崩溃不再提示（但仍在导出时附带）
const MAX_AGE_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// 一条崩溃记录
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrashRecord {
    /// ISO-8601 UTC 时间戳
    pub timestamp: String,
    /// 崩溃时的 prux 版本
    pub version: String,
    /// `uncaught_exception`：未捕获的 panic
    pub kind: String,
    /// panic 文本（非字符串负载退化为 `<non-string panic payload>`）
    pub message: String,
    /// panic 位置 `file:line:col`（拿不到位置信息时为 `None`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stack: Option<String>,
    /// 崩溃发生时会话文件的绝对路径（无会话时为 `None`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_file: Option<String>,
    /// 崩溃时进程的工作目录
    pub cwd: String,
    /// 是否已在启动时提示过用户（提示后置位，避免反复打扰）
    #[serde(default)]
    pub notified: bool,
}

/// 崩溃日志文件路径
pub fn crash_log_path() -> PathBuf {
    agent_dir().join("crashes.json")
}

/// 读取崩溃日志（文件缺失/不可解析/非数组 → 空列表；非法条目过滤掉）。
pub fn read_crash_log() -> Vec<CrashRecord> {
    read_crash_log_at(&crash_log_path())
}

/// 从指定路径读取崩溃日志；文件缺失/不可解析/非数组时返回空列表，非法条目被过滤。
fn read_crash_log_at(path: &Path) -> Vec<CrashRecord> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        return Vec::new();
    };

    items
        .iter()
        .filter_map(|v| {
            let timestamp = v.get("timestamp")?.as_str()?;
            let message = v.get("message")?.as_str()?;
            Some(CrashRecord {
                timestamp: timestamp.to_string(),
                version: v
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                kind: v
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or("uncaught_exception")
                    .to_string(),
                message: message.to_string(),
                stack: v
                    .get("stack")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                session_file: v
                    .get("sessionFile")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                cwd: v
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string(),
                notified: v.get("notified").and_then(|v| v.as_bool()).unwrap_or(false),
            })
        })
        .collect()
}

/// 把记录以 JSON 写回指定路径（自动创建父目录）；写失败返回 io 错误。
fn write_crash_log_at(path: &Path, records: &[CrashRecord]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let text = serde_json::to_string_pretty(records).unwrap_or_else(|_| "[]".to_string());
    std::fs::write(path, format!("{text}\n"))
}

/// 记录一次崩溃（best-effort：任何失败静默忽略，绝不在崩溃路径上再抛错）。
///
/// `session_file` 为崩溃发生时会话文件的绝对路径（无会话传 `None`）。
pub fn record_crash(
    kind: &str,
    message: &str,
    stack: Option<String>,
    session_file: Option<String>,
    cwd: &str,
) -> Option<CrashRecord> {
    record_crash_at(&crash_log_path(), kind, message, stack, session_file, cwd)
}

/// 向指定路径追加一条崩溃记录并裁剪到上限；写入失败返回 `None`（best-effort）。
fn record_crash_at(
    path: &Path,
    kind: &str,
    message: &str,
    stack: Option<String>,
    session_file: Option<String>,
    cwd: &str,
) -> Option<CrashRecord> {
    let record = CrashRecord {
        timestamp: now_iso(),
        version: VERSION.to_string(),
        kind: kind.to_string(),
        message: message.to_string(),
        stack,
        session_file,
        cwd: cwd.to_string(),
        notified: false,
    };
    let mut records = read_crash_log_at(path);
    records.push(record.clone());
    let start = records.len().saturating_sub(MAX_CRASH_RECORDS);
    write_crash_log_at(path, &records[start..]).ok()?;
    Some(record)
}

/// 取出最近一条「未被提示且未过期」的崩溃记录，并把所有未提示记录标记为已提示。
///
/// 每次都把全部记录置为 `notified`（提示一次即可，后续不再打扰）。
pub fn take_unnotified_crash() -> Option<CrashRecord> {
    take_unnotified_crash_at(&crash_log_path(), now_ms())
}

/// 取指定路径中最近一条「未提示且未过期」的崩溃，并把全部未提示记录标记为已提示；
/// 无合格记录时返回 `None`。
fn take_unnotified_crash_at(path: &Path, now: u64) -> Option<CrashRecord> {
    let records = read_crash_log_at(path);
    let crash = records
        .iter()
        .rev()
        .find(|r| {
            !r.notified
                && parse_iso_ms(&r.timestamp)
                    .map(|t| now.saturating_sub(t) <= MAX_AGE_MS)
                    .unwrap_or(false)
        })
        .cloned();

    crash.as_ref()?;

    let marked: Vec<CrashRecord> = records
        .into_iter()
        .map(|mut r| {
            r.notified = true;
            r
        })
        .collect();
    write_crash_log_at(path, &marked).ok();
    crash
}

/// 清空崩溃日志（`/bug` 成功导出并附带过崩溃记录后调用）
pub fn clear_crash_log() {
    _ = std::fs::remove_file(crash_log_path());
}

/// 安装 panic 钩子：把未捕获 panic 记为 `uncaught_exception`，随后交给原钩子输出。
///
/// 只应在进程入口调用一次。钩子内部不开新线程、不 panic。
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = panic_message(info);
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        _ = record_crash("uncaught_exception", &message, location, None, &cwd);
        previous(info);
    }));
}

/// panic 负载 → 文本（非字符串负载退化为 `<non-string panic payload>`）
fn panic_message(info: &PanicHookInfo<'_>) -> String {
    if let Some(s) = info.payload().downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = info.payload().downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crashes.json");
        (dir, path)
    }

    #[test]
    fn record_read_roundtrip_and_shape() {
        let (_d, path) = temp_path();
        let r = record_crash_at(
            &path,
            "uncaught_exception",
            "boom",
            Some("src/main.rs:1:1".to_string()),
            Some("/tmp/s.jsonl".to_string()),
            "/tmp",
        )
        .unwrap();
        assert_eq!(r.version, VERSION);
        // camelCase 落盘
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw[0]["sessionFile"], "/tmp/s.jsonl");
        assert_eq!(raw[0]["notified"], false);
        let back = read_crash_log_at(&path);
        assert_eq!(back, vec![r]);
    }

    #[test]
    fn keeps_only_newest_records() {
        let (_d, path) = temp_path();
        for i in 0..(MAX_CRASH_RECORDS + 3) {
            record_crash_at(&path, "fatal_error", &format!("e{i}"), None, None, "/tmp");
        }
        let records = read_crash_log_at(&path);
        assert_eq!(records.len(), MAX_CRASH_RECORDS);
        assert_eq!(records.last().unwrap().message, "e7");
        assert_eq!(records.first().unwrap().message, "e3");
    }

    #[test]
    fn take_marks_notified_and_returns_newest() {
        let (_d, path) = temp_path();
        record_crash_at(&path, "uncaught_exception", "old", None, None, "/tmp");
        record_crash_at(&path, "uncaught_exception", "new", None, None, "/tmp");

        let taken = take_unnotified_crash_at(&path, now_ms()).unwrap();
        assert_eq!(taken.message, "new");
        // 已全部置为 notified：再取为空，且文件里的标记确实落盘
        assert!(take_unnotified_crash_at(&path, now_ms()).is_none());
        assert!(read_crash_log_at(&path).iter().all(|r| r.notified));
        assert_eq!(read_crash_log_at(&path).len(), 2);
    }

    #[test]
    fn stale_records_are_not_reported() {
        let (_d, path) = temp_path();
        record_crash_at(&path, "uncaught_exception", "ancient", None, None, "/tmp");
        // 把时间戳改到 8 天前
        let old = crate::utils::time::rfc3339_days_ago(8);
        let mut records = read_crash_log_at(&path);
        records[0].timestamp = old;
        write_crash_log_at(&path, &records).unwrap();
        assert!(take_unnotified_crash_at(&path, now_ms()).is_none());
        // 未过期（6 天前）仍提示
        records[0].timestamp = crate::utils::time::rfc3339_days_ago(6);
        records[0].notified = false;
        write_crash_log_at(&path, &records).unwrap();
        assert_eq!(
            take_unnotified_crash_at(&path, now_ms()).map(|r| r.message),
            Some("ancient".to_string())
        );
    }

    #[test]
    fn malformed_file_reads_as_empty() {
        let (_d, path) = temp_path();
        std::fs::write(&path, "not json").unwrap();
        assert!(read_crash_log_at(&path).is_empty());
        std::fs::write(&path, "{\"a\":1}").unwrap();
        assert!(read_crash_log_at(&path).is_empty());
        // 条目缺 timestamp/message → 过滤
        std::fs::write(&path, "[{\"timestamp\":\"x\"},{\"message\":\"y\"}]").unwrap();
        assert!(read_crash_log_at(&path).is_empty());
        assert!(read_crash_log_at(&path.parent().unwrap().join("nope.json")).is_empty());
    }

    #[test]
    fn panic_hook_records_and_chains() {
        // 在子进程里验证：钩子记录崩溃并仍然向 stderr 输出原 panic 信息
        let dir = tempfile::tempdir().unwrap();
        let agent_dir = dir.path().to_path_buf();
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args([
                "--exact",
                "core::crash_log::tests::panic_hook_child",
                "--nocapture",
                "--ignored",
            ])
            .env("PRUX_AGENT_DIR", &agent_dir)
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        // 原钩子仍然输出 panic 信息
        assert!(stderr.contains("boom-panic"), "{stderr}");
        let records = read_crash_log_at(&agent_dir.join("crashes.json"));
        assert_eq!(records.len(), 1, "{records:?}");
        assert_eq!(records[0].kind, "uncaught_exception");
        assert!(records[0].message.contains("boom-panic"));
        assert!(
            records[0]
                .stack
                .as_deref()
                .unwrap_or("")
                .contains("crash_log")
        );
    }

    /// 子进程用的被忽略测试：安装钩子后 panic（由父测试调用）。
    #[test]
    #[ignore]
    fn panic_hook_child() {
        install_panic_hook();
        panic!("boom-panic");
    }
}

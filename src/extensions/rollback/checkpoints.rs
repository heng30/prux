//! 回合级文件检查点：改动前快照、`/rollback` 还原。
//!
//! 与 [`super`] 的分工：`/rewind` 只把**会话分支**退回去（磁盘不动），本模块补上另一半——
//! agent 改坏的**文件**也能退回这一回合开始前的样子。
//!
//! 快照时机在工具前钩子里（`edit` / `write` 真正写盘之前），按**回合**分组：
//! 一条用户消息 = 一回合，`/rollback 3` 一次退回第 3 回合改过的全部文件。
//! 分桶的单位是**工作目录**（一个工程一个桶，跟着会话的 cwd 走），因此恢复的是
//! 「这个工程里改过什么」，换会话不影响；磁盘布局：
//!
//! ```text
//! agent_dir()/checkpoints/<workspace-key>/
//!   index.json           # 条目表（回合号、绝对路径、是否本来就有、备份文件）
//!   files/<turn>-<n>.bak # 快照内容
//! ```
//!
//! 两条安全约定：**还原前先给当前内容拍一张新快照**（所以还原本身也能被下一次还原撤销），
//! 以及快照里记 `existed`——本来不存在的文件还原 = 删掉，而不是留一个空文件。
//! 条目超过 [`MAX_ENTRIES`] 时最老的一批（含备份文件）被惰性清掉。

use crate::{core::settings_manager::agent_dir, utils::paths::absolutize};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::{SystemTime, UNIX_EPOCH},
};

/// 会写盘的内置工具：只有它们需要在执行前拍快照。
const WRITE_TOOLS: &[&str] = &["edit", "write"];

/// 条目总数上限；超过后清掉最老的 [`PRUNE_RATIO`] 那一批。
const MAX_ENTRIES: usize = 500;

/// 清理时按比例丢弃最老的部分。
const PRUNE_RATIO: f64 = 0.2;

/// 条目表文件名。
const INDEX_FILE: &str = "index.json";

/// 备份文件目录名。
const FILES_DIR: &str = "files";

/// 取不到工作目录时使用的桶名。
const FALLBACK_WORKSPACE: &str = "default";

/// 一条快照记录。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    /// 属于哪一回合（同一工作目录内从 1 递增）。
    turn: u32,
    /// 被快照文件的绝对路径。
    path: String,
    /// 拍快照时文件是否存在；`false` 表示还原时要删掉它。
    existed: bool,
    /// 备份文件相对桶目录的路径；`existed == false` 时为 `None`。
    backup: Option<String>,
    /// 快照时间（Unix 秒），用于清理排序与展示。
    created_at: i64,
}

/// 桶目录里的条目表（`index.json` 的内容）。
#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    /// 全部快照记录，按拍摄顺序追加。
    #[serde(default)]
    entries: Vec<Entry>,
}

/// 一个工作目录的检查点状态（进程内）。
#[derive(Debug, Default)]
struct State {
    /// 当前工作目录桶名；`None` = 还没绑定工作目录。
    workspace: Option<String>,
    /// 当前工作目录（相对路径按它绝对化）。
    cwd: PathBuf,
    /// 当前回合号；新回合 = 已有最大值 + 1，跨进程不重置。
    turn: u32,
    /// 当前工作目录的条目表（与磁盘内容一致，改完即落盘）。
    index: Index,
}

/// 一回合的文件清单（`/rollback` 列表与 `diff` 共用）。
pub(super) struct TurnView {
    /// 回合号。
    pub turn: u32,
    /// 该回合改过的文件绝对路径（按首次快照顺序）。
    pub paths: Vec<String>,
}

/// 进程级状态：prux 同一时刻只有一个活动工作目录。
static STATE: Mutex<State> = Mutex::new(State {
    workspace: None,
    cwd: PathBuf::new(),
    turn: 0,
    index: Index {
        entries: Vec::new(),
    },
});

/// 取状态锁；锁被毒化时取回内部数据继续用。
fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 绑定工作目录：换成该目录的桶与条目表，并把回合号推到「已有最大值 + 1」。
///
/// 由 `on_exec_ctx` 在会话建立 / 每回合开始 / 会话切换后调用，因此中途启用扩展也不会
/// 记到错的地方。同一个目录重复调用是空操作。
pub(super) fn set_workspace(cwd: &str) {
    let key = workspace_key(cwd);
    let mut st = state();
    if st.workspace.as_deref() == Some(key.as_str()) {
        return;
    }
    let index = load_index(&workspace_dir(&key));
    st.turn = index.entries.iter().map(|e| e.turn).max().unwrap_or(0);
    st.index = index;
    st.workspace = Some(key);
    st.cwd = PathBuf::from(cwd);
}

/// 开始新回合（用户提交、且当时空闲时调用）；回合号单调递增。
pub(super) fn start_turn() {
    let mut st = state();
    st.turn = st.turn.saturating_add(1);
}

/// 工具执行前快照目标文件；只认 `edit` / `write`，其余工具直接返回。
///
/// 同一回合里同一文件只拍第一张（那才是回合开始前的状态），后续改动不再覆盖。
pub(super) fn snapshot(tool: &str, args: &Value) {
    if !WRITE_TOOLS.contains(&tool) {
        return;
    }
    let Some(raw) = args.get("path").and_then(Value::as_str) else {
        return;
    };
    let mut st = state();
    let path = absolutize(raw, &st.cwd);
    capture(&mut st, &path, false);
}

/// 给 `path` 拍一张快照并追加进条目表；`force` 为真时无视「本回合已拍过」。
fn capture(st: &mut State, path: &Path, force: bool) {
    let Some(key) = st.workspace.clone() else {
        return;
    };
    let turn = st.turn.max(1);
    let text = path.to_string_lossy().to_string();
    if !force
        && st
            .index
            .entries
            .iter()
            .any(|e| e.turn == turn && e.path == text)
    {
        return;
    }

    let dir = workspace_dir(&key);
    let existed = path.is_file();
    let backup = if existed {
        let rel = format!("{FILES_DIR}/{turn}-{}.bak", st.index.entries.len());
        let target = dir.join(&rel);
        if let Some(parent) = target.parent() {
            let _ = fs::create_dir_all(parent);
        }
        // 快照失败（权限、文件刚被删）就不落条目：宁可这一回合没有检查点，
        // 也不要留一条还原时会把文件清掉的记录。
        match fs::copy(path, &target) {
            Ok(_) => Some(rel),
            Err(_) => return,
        }
    } else {
        None
    };

    st.index.entries.push(Entry {
        turn,
        path: text,
        existed,
        backup,
        created_at: now(),
    });
    prune(&mut st.index, &dir);
    save_index(&dir, &st.index);
}

/// 按回合同聚合文件清单，回合号从大到小（最近的在前）。
pub(super) fn turns() -> Vec<TurnView> {
    let st = state();
    let mut out: Vec<TurnView> = Vec::new();
    for entry in &st.index.entries {
        match out.iter_mut().find(|t| t.turn == entry.turn) {
            Some(view) => {
                if !view.paths.contains(&entry.path) {
                    view.paths.push(entry.path.clone());
                }
            }
            None => out.push(TurnView {
                turn: entry.turn,
                paths: vec![entry.path.clone()],
            }),
        }
    }
    out.sort_by_key(|view| std::cmp::Reverse(view.turn));
    out
}

/// 把某一回合改过的文件还原到回合开始前的样子，返回还原过的路径。
///
/// 还原前先给**当前**内容拍一张快照（`force`），所以手滑还原也能再还原回来。
/// 该回合没有任何检查点时返回 `Err`。
pub(super) fn restore(turn: u32) -> Result<Vec<String>, String> {
    if turn == 0 {
        return Err("turn numbers start at 1".to_string());
    }
    let mut st = state();
    let Some(key) = st.workspace.clone() else {
        return Err("no active working directory".to_string());
    };

    // 每个文件只取本回合**最早**那张快照——那才是回合开始前的状态。
    let mut targets: Vec<Entry> = Vec::new();
    for entry in st.index.entries.iter().filter(|e| e.turn == turn) {
        if !targets.iter().any(|t| t.path == entry.path) {
            targets.push(entry.clone());
        }
    }
    if targets.is_empty() {
        return Err(format!("no checkpoints recorded for turn {turn}"));
    }

    let dir = workspace_dir(&key);
    for entry in &targets {
        let path = PathBuf::from(&entry.path);
        capture(&mut st, &path, true);
        let result = match &entry.backup {
            Some(rel) => fs::copy(dir.join(rel), &path).map(|_| ()),
            None => match fs::remove_file(&path) {
                Ok(()) => Ok(()),
                // 还原「本来不存在」的文件时，它可能已经又被删了。
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            },
        };
        if let Err(e) = result {
            save_index(&dir, &st.index);
            return Err(format!("restore {} failed: {e}", entry.path));
        }
    }
    save_index(&dir, &st.index);

    Ok(targets.into_iter().map(|e| e.path).collect())
}

/// 工作目录键：目录末级名 + 完整路径的 FNV-1a 短哈希（同名工程不串）。
fn workspace_key(cwd: &str) -> String {
    let stem = Path::new(cwd)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| FALLBACK_WORKSPACE.to_string());
    let safe: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("{safe}-{:08x}", fnv1a(cwd))
}

/// FNV-1a 32 位哈希：稳定、无需依赖，用来给工作目录路径取短后缀。
fn fnv1a(text: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for byte in text.as_bytes() {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

/// 工作目录的检查点目录：`agent_dir()/checkpoints/<workspace-key>`。
fn workspace_dir(key: &str) -> PathBuf {
    agent_dir().join("checkpoints").join(key)
}

/// 读条目表；文件缺失或损坏都当空表（检查点丢得起，不能让它拦住工具调用）。
fn load_index(dir: &Path) -> Index {
    fs::read_to_string(dir.join(INDEX_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// 原子写条目表：先写临时文件再 rename，避免写一半留下坏 JSON。
fn save_index(dir: &Path, index: &Index) {
    let text = match serde_json::to_string_pretty(index) {
        Ok(text) => text,
        Err(_) => return,
    };
    if fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = dir.join(format!("{INDEX_FILE}.tmp"));
    let written = fs::File::create(&tmp).and_then(|mut file| file.write_all(text.as_bytes()));
    if written.is_err() || fs::rename(&tmp, dir.join(INDEX_FILE)).is_err() {
        _ = fs::remove_file(&tmp);
    }
}

/// 条目过多时丢掉最老的一批（连同备份文件）。
fn prune(index: &mut Index, dir: &Path) {
    if index.entries.len() <= MAX_ENTRIES {
        return;
    }
    let drop_count = ((index.entries.len() as f64) * PRUNE_RATIO).ceil() as usize;
    let mut order: Vec<usize> = (0..index.entries.len()).collect();
    order.sort_by_key(|i| (index.entries[*i].turn, index.entries[*i].created_at));
    let doomed: Vec<usize> = order.into_iter().take(drop_count).collect();
    for i in doomed.iter().rev() {
        let entry = index.entries.remove(*i);
        if let Some(rel) = entry.backup {
            let _ = fs::remove_file(dir.join(rel));
        }
    }
}

/// 当前 Unix 秒；系统时钟早于 1970 时返回 0。
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 测试用：清空进程内状态（工作目录、回合与条目）。
#[cfg(test)]
pub(super) fn reset_for_tests() {
    let mut st = state();
    *st = State::default();
}

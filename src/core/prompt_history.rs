//! prompt 历史持久化（按项目根分桶的 JSONL）
//!
//! 设计约定：
//! - **分桶 key = 项目根**：[`project_root`] 从 cwd 向上找最近的 `.git`，把同一项目的
//!   子目录（`src/`、`tests/`…）归到一份历史；非 git 目录回退精确 cwd。
//! - **文件**：`<agent_dir>/history/<encode_cwd_dir(key)>.jsonl`，权限 `0600`
//!   （prompt 里可能含密钥）。与 `sessions/` 同构，都落在全局配置目录。
//! - **每行一条** `{"text":"…","ts":1758…}`：JSON 天然转义多行 prompt，无需自造续行协议。
//! - **追加写**：提交时只追加，不做 read-modify-write（多实例并发天然安全）。
//! - **读入去重**：同 text 只留**最后一次**出现的一条，顺序取**文件顺序**（追加写的文件
//!   天然按提交先后排列；`ts` 只有毫秒精度且会被时钟回拨带偏，不参与排序），
//!   再截断到最近 `max` 条。最旧在前，对齐
//!   [`crate::modes::interactive::editor::Editor::history`] 的 `len()-1` = 最新约定。
//! - **压缩**：行数达到 `2 × max` 时重写一次（去重 + 截断），把「纯追加」从无界增长
//!   里救回来；不引入跨进程文件锁，接受压缩竞态（最坏丢几条，不影响会话本体）。
//! - **`max == 0` = 不落盘**：不读、不追加、不压缩；**已存在的文件不动**（不销毁数据）。

use crate::{
    core::{session_manager::encode_cwd_dir, settings_manager},
    utils::{mime::strip_bom, paths::project_root, time::now_ms},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
};

/// 一条 prompt 历史。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryEntry {
    /// prompt 原文，可能含多行。
    pub text: String,
    /// 提交时间（毫秒时间戳），仅作展示不参与排序。
    #[serde(default)]
    pub ts: u64,
}

/// 删除结果（命令反馈与确认面板的爆炸半径都靠它）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ClearSummary {
    /// 实际删掉的文件数
    pub files: usize,
    /// 被删掉的条目总数
    pub entries: usize,
    /// 删完目录空了，目录本身也一并移除
    pub removed_dir: bool,
}

/// 落盘/读盘开关。
///
/// 单测隔离：`cargo test` 里若没有显式声明临时 agent 目录（`AgentDirGuard`），
/// 直接跳过读写——否则通过 handler 提交的测试会把构造出来的 prompt 写进开发者真实的
/// `~/.prux/history/`（且桶会是 prux 仓库本身），既污染真实历史又引入环境依赖。
/// 生产构建与集成测试（`test-support` feature）不受影响。
fn persistence_enabled() -> bool {
    #[cfg(test)]
    if crate::test_support::agent_dir_override().is_none() {
        return false;
    }
    true
}

/// 历史目录 `<agent_dir>/history`（扁平：一个项目一个 `.jsonl`，不建子目录）。
pub fn history_dir() -> PathBuf {
    settings_manager::agent_dir().join("history")
}

/// 当前 cwd 对应的历史文件：按项目根分桶。
pub fn history_path(cwd: &str) -> PathBuf {
    history_dir().join(format!("{}.jsonl", encode_cwd_dir(&project_root(cwd))))
}

/// 是否本工具生成的历史文件：`--<编码路径>--.jsonl`，
/// 及其压缩中途的临时文件 `--<编码路径>--.jsonl.<pid>.tmp`。
///
/// 删除命令**只认这个形状**：`@clear-all` 从「精确删一个路径」放开到「目录下多个文件」，
/// 若不做形状过滤，用户手工放进该目录的任何东西（备份、导出副本）都会被不可恢复地抹掉。
fn is_history_file(name: &str) -> bool {
    let stem = match name.strip_suffix(".tmp") {
        // 临时名：`<stem>.jsonl.<pid>.tmp`
        Some(base) => match base.rsplit_once('.') {
            Some((head, pid)) if !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit()) => head,
            _ => return false,
        },
        None => name,
    };
    stem.starts_with("--") && stem.ends_with(".jsonl")
}

/// 删除当前项目的历史文件（`@clear`）。
///
/// 返回实际删掉的文件与条目数（无文件时全为 0）。
/// **不受历史档位影响**：档位 `0` 只是不落盘，用户显式要求清空时照删。
pub fn clear_project(cwd: &str) -> ClearSummary {
    if !persistence_enabled() {
        return ClearSummary::default();
    }

    let path = history_path(cwd);
    let entries = count_entries(&path);
    let mut done = ClearSummary {
        files: 0,
        entries: 0,
        removed_dir: false,
    };

    if std::fs::remove_file(&path).is_ok() {
        done.files = 1;
        done.entries = entries;
    }
    done.removed_dir = remove_dir_if_empty();
    done
}

/// 删除历史目录下所有本工具生成的历史文件（`@clear-all`）。
///
/// 只删 [`is_history_file`] 认得的形状（含 `.tmp` 草稿残留）；目录空了则连目录一起移除。
pub fn clear_all() -> ClearSummary {
    if !persistence_enabled() {
        return ClearSummary::default();
    }

    let dir = history_dir();
    let mut done = ClearSummary::default();
    let Ok(read) = std::fs::read_dir(&dir) else {
        return done;
    };

    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_history_file(&name) {
            continue;
        }

        // 只处理普通文件/符号链接（不递归，也不碰同名目录）
        let Ok(kind) = entry.file_type() else {
            continue;
        };

        if !kind.is_file() && !kind.is_symlink() {
            continue;
        }

        let path = entry.path();
        let entries = count_entries(&path);
        if std::fs::remove_file(&path).is_ok() {
            done.files += 1;
            done.entries += entries;
        }
    }

    done.removed_dir = remove_dir_if_empty();
    done
}

/// 当前项目的历史文件概况（文件名, 条目数）；无文件时 None。
/// 供 `@clear` 确认面板展示爆炸半径。
pub fn project_summary(cwd: &str) -> Option<(String, usize)> {
    if !persistence_enabled() {
        return None;
    }

    let path = history_path(cwd);
    let name = path.file_name()?.to_string_lossy().to_string();
    let entries = count_entries(&path);
    path.is_file().then_some((name, entries))
}

/// 清空操作的整体概况（文件数, 条目总数）；供 `@clear-all` 确认面板展示。
pub fn all_summary() -> (usize, usize) {
    if !persistence_enabled() {
        return (0, 0);
    }

    let Ok(read) = std::fs::read_dir(history_dir()) else {
        return (0, 0);
    };

    let mut files = 0;
    let mut entries = 0;

    for entry in read.flatten() {
        if !is_history_file(&entry.file_name().to_string_lossy()) {
            continue;
        }

        if !entry
            .file_type()
            .map(|t| t.is_file() || t.is_symlink())
            .unwrap_or(false)
        {
            continue;
        }

        files += 1;
        entries += count_entries(&entry.path());
    }

    (files, entries)
}

/// 删除空的历史目录（非空时 `remove_dir` 失败，保持原样）。
/// 好处是 `@clear-all` 之后不留一个空壳目录。
fn remove_dir_if_empty() -> bool {
    std::fs::remove_dir(history_dir()).is_ok()
}

/// 数一个文件里的有效条目数（坏行与空文本行不计）。
fn count_entries(path: &Path) -> usize {
    std::fs::read_to_string(path)
        .map(|text| parse_lines(strip_bom(&text)).len())
        .unwrap_or(0)
}

/// 加载当前 cwd 的历史（见模块级文档的排序/去重约定）。
/// `max == 0`（不落盘）时不读文件，直接返回空。
pub fn load(cwd: &str, max: usize) -> Vec<HistoryEntry> {
    if max == 0 || !persistence_enabled() {
        return Vec::new();
    }

    let Ok(text) = std::fs::read_to_string(history_path(cwd)) else {
        return Vec::new();
    };

    dedup_and_cap(parse_lines(strip_bom(&text)), max)
}

/// 追加一条历史（纯追加）。`max == 0` 或空白文本时什么都不做。
/// 追加后行数达到 `2 × max` 时压缩重写。
///
/// **不做内容过滤**：`/` 命令与 `!`/`!!` shell 行照记。历史是「用户输入过什么」的
/// 无损归档——`/compact <指令>`、`/skill:name <附加要求>`、模板参数、`!` 手写命令
/// 都是用户真实输入，丢掉就找不回来（`!` 尤其：命令由本进程 fork 执行，不经过
/// 交互式 shell，没有第二份副本）。元命令在导航里的噪声属于展示层问题，
/// 不该用删数据来解。空白文本仍跳过：它写进文件后会被读盘的 [`parse_lines`] 滤掉，白占一行且永不生效。
pub fn append(cwd: &str, text: &str, max: usize) {
    if max == 0 || text.trim().is_empty() || !persistence_enabled() {
        return;
    }

    let path = history_path(cwd);
    if let Some(dir) = path.parent() {
        _ = std::fs::create_dir_all(dir);
    }

    let record = HistoryEntry {
        text: text.to_string(),
        ts: now_ms(),
    };

    let Ok(line) = serde_json::to_string(&record) else {
        return;
    };

    let existed = path.exists();
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
        _ = writeln!(f, "{line}");
    }

    // 新建的文件立刻收紧权限；已存在的文件不重复 set_permissions（每次提交都调是浪费）
    if !existed {
        restrict_permissions(&path);
    }

    compact_if_needed(&path, max);
}

/// 解析 JSONL 文本：跳过损坏行与空文本行（历史是便利性数据，不因一行坏掉全丢）。
fn parse_lines(text: &str) -> Vec<HistoryEntry> {
    text.lines()
        .filter_map(|line| serde_json::from_str::<HistoryEntry>(line).ok())
        .filter(|e| !e.text.trim().is_empty())
        .collect()
}

/// 去重（同 text 只留**最后一次**出现的一条）+ 截断到最近 `max` 条。
///
/// 顺序取自**文件顺序**而不是 `ts`：追加写的文件天然按提交先后排列，而 `ts` 只有毫秒
/// 精度（同毫秒并列）且在系统时钟回拨时会失真。做法是先记下每个 text 的最后出现下标，
/// 再按文件顺序过滤掉更早的重复项——O(n)，且同刻提交的相对顺序稳定。
/// `ts` 保留为元数据（人工排查 / 将来按时间筛选），不参与排序。
fn dedup_and_cap(entries: Vec<HistoryEntry>, max: usize) -> Vec<HistoryEntry> {
    // 先收集每个 text 的最后出现下标（借用到此为止，随后才能把 entries move 出去）
    let keep: HashSet<usize> = {
        let mut last: HashMap<&str, usize> = HashMap::new();
        for (i, e) in entries.iter().enumerate() {
            last.insert(e.text.as_str(), i);
        }
        last.into_values().collect()
    };

    let mut out: Vec<HistoryEntry> = entries
        .into_iter()
        .enumerate()
        .filter(|(i, _)| keep.contains(i))
        .map(|(_, e)| e)
        .collect();

    if out.len() > max {
        out.drain(..out.len() - max);
    }
    out
}

/// 行数达到 `2 × max` 时重写文件：去重 + 截断到最近 `max` 条。
///
/// 先写同目录临时文件再 `rename`（原子替换），避免压缩中途崩溃留下半截文件；
/// 临时名带 pid，降低两个实例撞同一临时文件的机会。
fn compact_if_needed(path: &Path, max: usize) {
    if max == 0 {
        return;
    }

    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };

    let stripped = strip_bom(&text);
    if stripped.lines().count() < max.saturating_mul(2) {
        return;
    }

    let mut body = String::new();
    for e in dedup_and_cap(parse_lines(stripped), max) {
        if let Ok(line) = serde_json::to_string(&e) {
            body.push_str(&line);
            body.push('\n');
        }
    }

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("history.jsonl");
    let tmp = path.with_file_name(format!("{}.{}.tmp", name, std::process::id()));

    if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, path).is_err() {
        _ = std::fs::remove_file(&tmp);
    }

    restrict_permissions(path);
}

/// 历史文件权限收紧到 `0600`（prompt 可能含密钥）；非 unix 不处理。
fn restrict_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).ok();
    }

    #[cfg(not(unix))]
    {
        _ = path;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    /// 全新临时 agent 目录 + 临时 cwd（不在任何 git 仓库内，避免踩到真实仓库）
    fn with_env(f: impl FnOnce(&str)) {
        let _ad = AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        f(cwd.path().to_str().unwrap());
    }

    #[test]
    fn append_then_load_roundtrip_in_chronological_order() {
        with_env(|cwd| {
            append(cwd, "first", 500);
            append(cwd, "second", 500);
            append(cwd, "third", 500);

            let entries = load(cwd, 500);
            let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
            // ts 升序：最旧在前（Editor::history 的 len()-1 才是最新）
            assert_eq!(texts, vec!["first", "second", "third"]);
        });
    }

    #[test]
    fn multiline_prompt_survives_roundtrip() {
        with_env(|cwd| {
            let text = "line one\nline two\n\nline four";
            append(cwd, text, 500);
            let entries = load(cwd, 500);
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].text, text, "换行由 JSON 转义，原样还原");
        });
    }

    #[test]
    fn duplicate_keeps_last_occurrence_and_collapses() {
        with_env(|cwd| {
            append(cwd, "a", 500);
            append(cwd, "b", 500);
            append(cwd, "a", 500); // 重复
            let entries = load(cwd, 500);
            assert_eq!(entries.len(), 2, "同文本去重: {entries:?}");
            // 取「最后一次出现」的位置：a 重新提交过，排在 b 之后
            let texts: Vec<&str> = entries.iter().map(|e| e.text.as_str()).collect();
            assert_eq!(texts, vec!["b", "a"]);
        });
    }

    #[test]
    fn load_caps_to_most_recent() {
        with_env(|cwd| {
            for i in 0..5 {
                append(cwd, &format!("p{i}"), 500);
            }
            let texts: Vec<String> = load(cwd, 2).into_iter().map(|e| e.text).collect();
            assert_eq!(
                texts,
                vec!["p3".to_string(), "p4".to_string()],
                "保留最近 2 条"
            );
        });
    }

    #[test]
    fn compaction_rewrites_file_at_twice_max() {
        with_env(|cwd| {
            let path = history_path(cwd);
            // max=2 → 阈值 4 行；写 4 条不重复 → 触发压缩
            for i in 0..4 {
                append(cwd, &format!("q{i}"), 2);
            }
            let raw = std::fs::read_to_string(&path).unwrap();
            assert!(!raw.is_empty(), "压缩后文件仍在");
            assert!(raw.lines().count() < 4, "压缩后行数应低于阈值: {raw:?}");
            let texts: Vec<String> = load(cwd, 2).into_iter().map(|e| e.text).collect();
            assert_eq!(texts, vec!["q2".to_string(), "q3".to_string()]);
        });
    }

    #[test]
    fn zero_means_no_persistence() {
        with_env(|cwd| {
            append(cwd, "secret", 0);
            assert!(!history_path(cwd).exists(), "max=0 不应创建文件");
            assert!(load(cwd, 0).is_empty());
        });
    }

    #[test]
    fn zero_keeps_existing_file_untouched() {
        with_env(|cwd| {
            append(cwd, "kept", 500);
            let before = std::fs::read_to_string(history_path(cwd)).unwrap();
            // 切到不落盘：既不追加也不删除
            append(cwd, "dropped", 0);
            assert!(load(cwd, 0).is_empty(), "不读文件");
            assert_eq!(
                std::fs::read_to_string(history_path(cwd)).unwrap(),
                before,
                "已存在的文件不能被改写或删除"
            );
        });
    }

    #[test]
    fn meta_commands_are_persisted() {
        with_env(|cwd| {
            append(cwd, "/model gpt-5", 500);
            append(cwd, "!ls", 500);
            append(cwd, "!!git status", 500);
            append(cwd, "/compact 聚焦并发部分", 500);
            let texts: Vec<String> = load(cwd, 500).into_iter().map(|e| e.text).collect();
            assert_eq!(
                texts,
                vec![
                    "/model gpt-5",
                    "!ls",
                    "!!git status",
                    "/compact 聚焦并发部分"
                ],
                "任何用户输入都要进历史，包括 / 与 ! 开头的行"
            );
        });
    }

    #[test]
    fn blank_text_is_not_persisted() {
        with_env(|cwd| {
            append(cwd, "", 500);
            append(cwd, "  \n ", 500);
            assert!(!history_path(cwd).exists(), "空白文本不进历史");
        });
    }

    #[test]
    fn corrupt_lines_are_skipped() {
        with_env(|cwd| {
            let path = history_path(cwd);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let good = serde_json::to_string(&HistoryEntry {
                text: "good".into(),
                ts: 1,
            })
            .unwrap();
            std::fs::write(
                &path,
                format!("not json\n{good}\n{{}}\n{{\"text\":\"  \"}}\n"),
            )
            .unwrap();
            let entries = load(cwd, 500);
            assert_eq!(entries.len(), 1, "坏行与空文本行跳过: {entries:?}");
            assert_eq!(entries[0].text, "good");
        });
    }

    #[test]
    fn nested_dirs_in_same_repo_share_one_bucket() {
        let _ad = AgentDirGuard::temp();
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join(".git")).unwrap();
        let nested = repo.path().join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();

        append(repo.path().to_str().unwrap(), "from root", 500);
        let from_nested = load(nested.to_str().unwrap(), 500);
        assert_eq!(from_nested.len(), 1, "子目录与项目根共用一份历史");
        assert_eq!(from_nested[0].text, "from root");
        assert_eq!(
            history_path(repo.path().to_str().unwrap()),
            history_path(nested.to_str().unwrap())
        );
    }

    #[test]
    fn different_projects_do_not_share_buckets() {
        let _ad = AgentDirGuard::temp();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(a.path().join(".git")).unwrap();
        std::fs::create_dir_all(b.path().join(".git")).unwrap();

        append(a.path().to_str().unwrap(), "project a", 500);
        assert!(load(b.path().to_str().unwrap(), 500).is_empty());
    }

    // ---- @clear / @clear-all ----

    #[test]
    fn is_history_file_only_accepts_our_shapes() {
        assert!(is_history_file("--home-user-proj--.jsonl"));
        assert!(is_history_file("--home-user-proj--.jsonl.12345.tmp"));
        // 非本工具的文件一律不认（@clear-all 不能误删用户放进来的东西）
        assert!(!is_history_file("README.md"));
        assert!(!is_history_file("backup.jsonl"));
        assert!(!is_history_file("-single-dash--.jsonl"));
        assert!(!is_history_file("--x--.txt"));
        assert!(!is_history_file("--x--.jsonl.abc.tmp")); // pid 非数字
        assert!(!is_history_file("--x--.jsonl.tmp")); // 缺 pid
    }

    #[test]
    fn clear_project_removes_bucket_and_empty_dir() {
        with_env(|cwd| {
            append(cwd, "a", 500);
            let path = history_path(cwd);
            assert!(path.is_file());

            let done = clear_project(cwd);
            assert_eq!(done.files, 1);
            assert_eq!(done.entries, 1);
            assert!(!path.exists());
            assert!(done.removed_dir, "删空后目录应一并移除");
            assert!(!history_dir().exists());

            // 无文件时是安全的空操作
            let again = clear_project(cwd);
            assert_eq!(again.files, 0);
            assert_eq!(again.entries, 0);
        });
    }

    #[test]
    fn clear_project_leaves_other_buckets_alone() {
        let _ad = AgentDirGuard::temp();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (a, b) = (a.path().to_str().unwrap(), b.path().to_str().unwrap());
        append(a, "from a", 500);
        append(b, "from b", 500);

        clear_project(a);
        assert!(load(a, 500).is_empty());
        assert_eq!(load(b, 500).len(), 1, "@clear 只影响当前项目");
    }

    #[test]
    fn clear_all_removes_our_files_but_keeps_foreign_ones() {
        let _ad = AgentDirGuard::temp();
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let (a, b) = (a.path().to_str().unwrap(), b.path().to_str().unwrap());
        append(a, "from a", 500);
        append(b, "from b", 500);
        // 用户手工放进来的东西：形状不匹配，必须保留
        let foreign = history_dir().join("README.md");
        std::fs::write(&foreign, "keep me").unwrap();

        assert_eq!(all_summary(), (2, 2), "概况只数本工具的文件");
        let done = clear_all();
        assert_eq!(done.files, 2);
        assert_eq!(done.entries, 2);
        assert!(!done.removed_dir, "目录里还有外来文件，不能删目录");
        assert!(foreign.is_file(), "非本工具文件必须保留");
        assert!(load(a, 500).is_empty());
        assert!(load(b, 500).is_empty());
    }

    #[test]
    fn clear_all_removes_dir_and_compaction_tmp_leftovers() {
        let _ad = AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd = cwd.path().to_str().unwrap();
        append(cwd, "kept", 500);
        // 模拟压缩中途崩溃留下的临时文件
        let tmp = history_dir().join("--stale--.jsonl.999.tmp");
        std::fs::write(&tmp, "{}\n").unwrap();

        let done = clear_all();
        assert_eq!(done.files, 2, ".jsonl 与 .tmp 残留都要清: {done:?}");
        assert!(done.removed_dir, "全空了，目录应一并移除");
        assert!(!history_dir().exists());
    }

    #[test]
    fn clear_all_on_missing_dir_is_a_noop() {
        let _ad = AgentDirGuard::temp();
        let done = clear_all();
        assert_eq!(done, ClearSummary::default());
        assert_eq!(all_summary(), (0, 0));
    }

    #[cfg(unix)]
    #[test]
    fn history_file_is_0600() {
        use std::os::unix::fs::PermissionsExt;
        with_env(|cwd| {
            append(cwd, "has a token sk-xxx", 500);
            let mode = std::fs::metadata(history_path(cwd))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "prompt 可能含密钥，文件必须 0600");
        });
    }
}

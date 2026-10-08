//! 项目信任机制

use crate::{
    PROJECT_SCOPE_NAME,
    core::{self, config_cache::ConfigCache, settings_manager},
    utils::paths::{expand_tilde, normalize_cwd},
};
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Mutex, RwLock},
};

/// 信任状态持久化文件名，位于 agent_dir 下的 trust.json。
const TRUST_FILE: &str = "trust.json";

/// `<cwd>/<PROJECT_SCOPE_NAME>/` 下**必须受项目信任门控**的资源条目。
///
/// 不变式：凡是只在项目受信任时才加载的资源，都必须在这里列出。
/// 漏一个，启动时就不会弹信任面板（`has_trust_requiring_project_resources` 为 false →
/// `resolve_trusted` 直接返回 true），用户既看不到询问也看不到未信任警告，
/// 而该资源在运行期又被 `is_project_trusted` 静默丢弃。
///
/// 新增受信任门控的项目资源时，把它加进来；
/// 具体在哪读取/执行不要写在这里（同一事实写两处会过期）。
pub(crate) const PRUX_RESOURCES: &[&str] = &[
    "settings.json",
    "extensions",
    "skills",
    "agents",
    "prompts",
    "themes",
    "SYSTEM.md",
    "APPEND_SYSTEM.md",
    // 扩展
    "workflows",
    "agent-memory",
    "agent-memory-local",
];

/// trust.json 读写锁（本文件自己的锁）。
static TRUST_LOCK: RwLock<()> = RwLock::new(());

/// trust.json 的缓存层。读/写都只在 [`with_trust_read`] / [`with_trust_write`] 的
/// `RwLock` 作用域内进行，锁序恒为「RwLock → 缓存 Mutex」。
static TRUST_CACHE: ConfigCache = ConfigCache::new();

/// 本次运行的信任决策表（路径 → 是否信任），优先于 trust.json。
///
/// 来源：启动决策（[`crate::cli::project_trust::resolve_trusted`]，含 `-a/--approve`、
/// `--no-approve`）与启动面板的「Trust/Do not trust (this session only)」选项。
/// 命中规则与 trust.json 一致（最近祖先优先），只是**不落盘**、仅本进程有效。
///
/// 必须是**进程级**而非 thread-local：子代理工具在 worker 线程/异步任务里执行，thread-local 传不过去。
/// 每个会话目录只应有一条记录，用表是为了测试并行时不互相覆盖。
static SESSION_TRUST: Mutex<BTreeMap<PathBuf, bool>> = Mutex::new(BTreeMap::new());

/// 项目信任策略：询问、始终信任或永不信任，决定项目级资源是否加载。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectTrust {
    /// 默认：对尚无显式决策的项目弹出询问
    Ask,
    /// 始终信任，直接加载项目级资源
    Always,
    /// 永不信任，不加载项目级资源
    Never,
}

/// trust.json 的路径（位于 `agent_dir` 下，文件名见 [`TRUST_FILE`]）。
fn trust_path(agent_dir: &Path) -> PathBuf {
    agent_dir.join(TRUST_FILE)
}

/// trust.json 的绝对路径（`/trust` 清空确认面板展示「将清空哪个文件」用）。
pub fn trust_file_path(agent_dir: &Path) -> PathBuf {
    trust_path(agent_dir)
}

/// 在 trust.json 读锁内执行闭包。
fn with_trust_read<R>(f: impl FnOnce() -> R) -> R {
    let _guard = TRUST_LOCK.read().unwrap_or_else(|e| e.into_inner());
    f()
}

/// 在 trust.json 写锁内执行闭包（读-改-写整体在锁内）。
fn with_trust_write<R>(f: impl FnOnce() -> R) -> R {
    let _guard = TRUST_LOCK.write().unwrap_or_else(|e| e.into_inner());
    f()
}

/// 在写锁内对 trust.json 做一次 read-modify-write（保留其他路径决策）。
fn modify_trust_file<F>(agent_dir: &Path, f: F)
where
    F: FnOnce(&mut Map<String, Value>),
{
    with_trust_write(|| {
        let mut map = read_trust_file_unlocked(agent_dir);
        f(&mut map);
        write_trust_file(agent_dir, map);
    });
}

/// 读取 trust.json：`{"/absolute/path": true|false|null}`（持读锁）。
fn read_trust_file(agent_dir: &Path) -> Map<String, Value> {
    with_trust_read(|| read_trust_file_unlocked(agent_dir))
}

/// 不加锁读取 trust.json（调用方已持锁：写路径 read-modify-write 用）。
/// 走 [`TRUST_CACHE`]：命中直接返回内存值，未命中读盘并回填。
fn read_trust_file_unlocked(agent_dir: &Path) -> Map<String, Value> {
    let path = trust_path(agent_dir);
    let v = TRUST_CACHE.read(&path, || {
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .unwrap_or(Value::Null)
    });

    match v {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// 清空 trust.json 的全部路径决策（文件保留为空 JSON 对象 `{}`）。
///
/// `/trust` 面板的「Remove all saved trust decisions」用；与 `/trust` 其它选项一样
/// 只改持久决策、不动本次运行的会话决策，重启后完全生效。
pub fn clear_all_trust(agent_dir: &Path) {
    modify_trust_file(agent_dir, |map| map.clear());
}

/// 写盘：按 key 排序，只保留 bool。
fn write_trust_file(agent_dir: &Path, map: Map<String, Value>) {
    let path = trust_path(agent_dir);
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort();

    let mut sorted = Map::new();
    for key in keys {
        let value = map.get(key).cloned().unwrap_or(Value::Null);
        if value.is_boolean() {
            sorted.insert(key.clone(), value);
        }
    }

    if let Some(dir) = path.parent() {
        _ = std::fs::create_dir_all(dir);
    }

    let value = Value::Object(sorted);
    let Ok(text) = serde_json::to_string_pretty(&value) else {
        return;
    };

    if settings_manager::atomic_write_config(&path, format!("{}\n", text).as_bytes(), None).is_ok()
    {
        // 落盘成功才回填缓存（取写盘后的元数据）。
        TRUST_CACHE.store(&path, value);
    }
}

/// 读取某个路径键的持久信任决策；文件里无该键、或值为 `null` 占位（旧格式）时返回 None。
fn trust_value(agent_dir: &Path, key: &str) -> Option<bool> {
    let map = read_trust_file(agent_dir);
    match map.get(key).cloned() {
        Some(Value::Bool(b)) => Some(b),
        Some(Value::Null) => None,
        _ => None,
    }
}

/// 从当前目录向祖先查找最近的显式 trust 决策
fn find_nearest_trust_entry(cwd: &Path, agent_dir: &Path) -> Option<(String, bool)> {
    let mut current = PathBuf::from(normalize_cwd(cwd));
    loop {
        let key = normalize_cwd(&current);
        if let Some(decision) = trust_value(agent_dir, &key) {
            return Some((key, decision));
        }
        if !current.pop() {
            return None;
        }
    }
}

/// 当前 cwd（或祖先）是否已有显式信任决策：`(决策路径, 是否信任)`。
/// 无任何显式决策（包括旧格式留下的 `null` 占位）时返回 None——启动提示据此决定是否弹出。
pub fn trust_decision(cwd: &Path, agent_dir: &Path) -> Option<(String, bool)> {
    find_nearest_trust_entry(cwd, agent_dir)
}

/// pi 当前默认策略保存在 settings.json 的 defaultProjectTrust；trust.json 只是路径决策表
pub fn default_project_trust(_agent_dir: &Path) -> ProjectTrust {
    let settings = core::settings_manager::read_settings();
    match settings.default_project_trust.as_deref() {
        Some("always") => ProjectTrust::Always,
        Some("never") => ProjectTrust::Never,
        _ => ProjectTrust::Ask,
    }
}

/// 记录本次运行的信任决策（对 `cwd` 及其子目录生效，直到出现更近的会话决策）。
pub fn set_session_trust(cwd: &Path, trusted: bool) {
    let key = PathBuf::from(normalize_cwd(cwd));
    let mut map = SESSION_TRUST.lock().unwrap_or_else(|e| e.into_inner());
    map.insert(key, trusted);
}

/// 清除某个路径的会话决策（「Trust parent folder」把决策改记到父目录时用）。
pub fn clear_session_trust(cwd: &Path) {
    let key = PathBuf::from(normalize_cwd(cwd));
    let mut map = SESSION_TRUST.lock().unwrap_or_else(|e| e.into_inner());
    map.remove(&key);
}

/// 最近祖先的会话决策（没有则 None）。
fn session_trust_decision(cwd: &Path) -> Option<bool> {
    let map = SESSION_TRUST.lock().unwrap_or_else(|e| e.into_inner());
    if map.is_empty() {
        return None;
    }

    let mut current = PathBuf::from(normalize_cwd(cwd));
    loop {
        if let Some(trusted) = map.get(&current) {
            return Some(*trusted);
        }
        if !current.pop() {
            return None;
        }
    }
}

/// 项目是否受信任：会话决策优先，其次最近祖先的持久决策，最后回退 default_project_trust。
/// 会话决策在前是关键——「仅本次信任/不信任」与 `-a/--no-approve` 必须盖过 trust.json，
/// 否则面板上选了「仅本次」也照样读不到项目 agents/workflows/agent-memory。
pub fn is_project_trusted(cwd: &Path, agent_dir: &Path) -> bool {
    if let Some(trusted) = session_trust_decision(cwd) {
        return trusted;
    }

    if let Some((_, trusted)) = find_nearest_trust_entry(cwd, agent_dir) {
        return trusted;
    }

    match default_project_trust(agent_dir) {
        ProjectTrust::Always => true,
        ProjectTrust::Never | ProjectTrust::Ask => false,
    }
}

/// 没有需要信任的项目资源时视为可信任。
pub fn has_trust_requiring_project_resources(cwd: &Path) -> bool {
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .map(expand_tilde)
        .unwrap_or_default();
    let user_agents_skills = home.join(".agents").join("skills");
    let user_agents_skills = normalize_cwd(&user_agents_skills);

    let prux_dir = cwd.join(PROJECT_SCOPE_NAME);
    for resource in PRUX_RESOURCES {
        if prux_dir.join(resource).exists() {
            return true;
        }
    }

    let mut current = PathBuf::from(normalize_cwd(cwd));
    loop {
        let agents_skills = current.join(".agents").join("skills");
        let agents_skills = normalize_cwd(&agents_skills);
        if agents_skills != user_agents_skills && Path::new(&agents_skills).exists() {
            return true;
        }
        if !current.pop() {
            return false;
        }
    }
}

/// 为 `cwd` 写入显式信任决策到 trust.json（键为规范化后的绝对路径）。
///
/// 走读-改-写并持写锁，保留其它路径的既有决策；只改持久文件，不影响本次运行的会话决策。
pub fn set_project_trust(cwd: &Path, agent_dir: &Path, trusted: bool) {
    let key = normalize_cwd(cwd);
    modify_trust_file(agent_dir, |map| {
        map.insert(key, json!(trusted));
    });
}

/// 删除某个路径的显式决策，让它回落到祖先决策（「Trust parent folder」用）。
pub fn clear_project_trust(cwd: &Path, agent_dir: &Path) {
    let key = normalize_cwd(cwd);
    modify_trust_file(agent_dir, |map| {
        map.remove(&key);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_trust_writes_do_not_lose_updates() {
        // trust.json 的读-改-写必须串行（并行为不同路径保存信任决策不得丢条目）。
        // spawn 线程读不到线程本地 override → 用进程级 PRUX_AGENT_DIR + AUTH_TEST_LOCK 串行。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::test_support::pin_test_agent_dir();
        let dir = core::settings_manager::agent_dir();
        let _ = std::fs::remove_file(dir.join(TRUST_FILE));

        let n: u64 = 16;
        std::thread::scope(|s| {
            for i in 0..n {
                let dir = dir.clone();
                s.spawn(move || {
                    let cwd = PathBuf::from(format!("/tmp/prux-trust-{i}"));
                    for _ in 0..20 {
                        set_project_trust(&cwd, &dir, true);
                    }
                });
            }
        });

        let map = read_trust_file(&dir);
        for i in 0..n {
            let key = normalize_cwd(Path::new(&format!("/tmp/prux-trust-{i}")));
            assert_eq!(
                map.get(&key),
                Some(&Value::Bool(true)),
                "并发写入丢失 trust 条目 {key}"
            );
        }
        let _ = std::fs::remove_file(dir.join(TRUST_FILE));
    }

    #[test]
    fn project_workflows_require_trust() {
        // 回归：`.prux/workflows/` 是受信任门控的项目扩展
        // （extensions/subagent/workflow/saved.rs 用 is_project_trusted 决定是否加入查找根）。
        // 漏进 PRUX_RESOURCES 时：启动不弹信任面板、不警告，项目工作流被静默丢弃。
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(cwd.join(".prux").join("workflows")).unwrap();
        std::fs::write(
            cwd.join(".prux").join("workflows").join("test-simple.js"),
            "export const meta = { name: 'x', description: 'y' };",
        )
        .unwrap();

        assert!(
            has_trust_requiring_project_resources(&cwd),
            ".prux/workflows 应触发项目信任询问"
        );
    }

    #[test]
    fn every_trust_gated_project_resource_is_detected() {
        // 不变式：PRUX_RESOURCES 必须覆盖所有受信任门控的项目资源，
        // 否则启动询问会被跳过而资源在运行期静默消失。
        let expected = [
            "settings.json",
            "extensions",
            "skills",
            "agents",
            "workflows",
            "agent-memory",
            "agent-memory-local",
            "prompts",
            "themes",
            "SYSTEM.md",
            "APPEND_SYSTEM.md",
        ];
        for entry in expected {
            assert!(
                PRUX_RESOURCES.contains(&entry),
                "PRUX_RESOURCES 漏掉受信任门控的资源 {entry:?}"
            );
        }

        // 逐条验证：仅存在该条目也应触发询问（含文件与目录两种形态）
        for entry in expected {
            let dir = tempfile::tempdir().unwrap();
            let cwd = dir.path().join("proj");
            let prux_dir = cwd.join(PROJECT_SCOPE_NAME);
            std::fs::create_dir_all(&prux_dir).unwrap();
            let path = prux_dir.join(entry);
            if entry.contains('.') {
                std::fs::write(&path, "x").unwrap();
            } else {
                std::fs::create_dir_all(&path).unwrap();
            }
            assert!(
                has_trust_requiring_project_resources(&cwd),
                ".prux/{entry} 应触发项目信任询问"
            );
        }
    }

    #[test]
    fn bare_project_scope_dir_without_tracked_entries_does_not_require_trust() {
        // 对齐 pi：`.prux/` 目录本身、以及不在这张表里的子项（如 docs/）不触发询问。
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(cwd.join(".prux").join("docs")).unwrap();
        std::fs::write(cwd.join(".prux").join("AGENTS.md"), "x").unwrap();

        assert!(!has_trust_requiring_project_resources(&cwd));
    }

    #[test]
    fn session_trust_overrides_persisted_and_covers_subdirs() {
        // 「仅本次信任/不信任」与 `-a/--no-approve` 必须盖过 trust.json：
        // 否则面板上选了「仅本次」也照样读不到项目 agents/workflows/agent-memory。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let agent_dir = settings_manager::agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().join("proj");
        let sub = cwd.join("nested");
        std::fs::create_dir_all(&sub).unwrap();

        // 持久决策 false，会话决策 true 盖过它（且覆盖子目录）
        set_project_trust(&cwd, &agent_dir, false);
        assert!(!is_project_trusted(&cwd, &agent_dir));
        set_session_trust(&cwd, true);
        assert!(
            is_project_trusted(&cwd, &agent_dir),
            "会话信任应盖过持久决策"
        );
        assert!(is_project_trusted(&sub, &agent_dir), "会话决策应覆盖子目录");

        // 持久决策 true，会话决策 false 反向盖过
        set_project_trust(&cwd, &agent_dir, true);
        set_session_trust(&cwd, false);
        assert!(
            !is_project_trusted(&cwd, &agent_dir),
            "会话不信任应盖过持久信任"
        );

        // 清除会话决策后回落 trust.json
        clear_session_trust(&cwd);
        assert!(
            is_project_trusted(&cwd, &agent_dir),
            "清除后应回落到 trust.json"
        );

        // 会话决策不越出目录树：会话目录的兄弟/祖先不受影响
        let sibling = dir.path().join("other");
        std::fs::create_dir_all(&sibling).unwrap();
        set_session_trust(&cwd, true);
        assert!(
            !is_project_trusted(&sibling, &agent_dir),
            "会话决策不得外溢到兄弟目录"
        );
        assert!(
            !is_project_trusted(dir.path(), &agent_dir),
            "会话决策不得外溢到父目录"
        );
        clear_session_trust(&cwd);
    }

    #[test]
    fn trust_parent_session_decision_is_cleared_from_cwd() {
        // 「Trust parent folder」在会话层把决策改记到父目录；启动时给 cwd 登记的
        // 决策必须先撤掉，否则更近的 cwd 条目会盖过刚写下的父目录决策。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let agent_dir = settings_manager::agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("repo");
        let cwd = parent.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();

        set_session_trust(&cwd, false);
        set_session_trust(&parent, true);
        clear_session_trust(&cwd);
        assert!(is_project_trusted(&cwd, &agent_dir), "父目录决策应生效");
        assert!(is_project_trusted(&parent, &agent_dir));
        clear_session_trust(&parent);
    }

    #[test]
    fn clear_project_trust_deletes_key_instead_of_writing_null() {
        // 「Trust parent folder」写盘结果应与 pi 一致：只留 `{parent: true}`，
        // 而不是 `{parent: true, cwd: null}` —— null 占位是永久墓碑，
        // `cat trust.json` 看不出这其实是「没决策」。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let agent_dir = settings_manager::agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("repo");
        let cwd = parent.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();

        // cwd 先有显式 false（挡住父目录），进面板选「Trust parent folder」后清掉它
        set_project_trust(&cwd, &agent_dir, false);
        set_project_trust(&parent, &agent_dir, true);
        assert!(!is_project_trusted(&cwd, &agent_dir));

        clear_project_trust(&cwd, &agent_dir);

        assert!(is_project_trusted(&cwd, &agent_dir), "应回落到父目录决策");
        assert_eq!(
            trust_decision(&cwd, &agent_dir),
            Some((normalize_cwd(&parent), true))
        );

        let map = read_trust_file(&agent_dir);
        assert_eq!(map.get(&normalize_cwd(&parent)), Some(&Value::Bool(true)));
        assert!(
            !map.contains_key(&normalize_cwd(&cwd)),
            "clear 应删除 key：{map:?}"
        );
    }

    #[test]
    fn clear_all_trust_empties_the_file() {
        // 「Remove all saved trust decisions」：清空后所有路径都回落到
        // default_project_trust，且文件只剩空对象（不是删文件、也不是 null 墓碑）。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let agent_dir = settings_manager::agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        let cwd = repo.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();

        set_project_trust(&repo, &agent_dir, true);
        set_project_trust(&cwd, &agent_dir, false);
        assert!(!read_trust_file(&agent_dir).is_empty());

        clear_all_trust(&agent_dir);

        assert!(read_trust_file(&agent_dir).is_empty(), "应清空所有决策");
        assert!(
            trust_decision(&cwd, &agent_dir).is_none(),
            "不应再有显式决策"
        );
        assert!(
            !is_project_trusted(&cwd, &agent_dir),
            "清空后应回落到 default_project_trust（Ask → 不信任）"
        );
    }

    #[test]
    fn trust_cache_reflects_external_edits() {
        // 缓存以 (path, mtime, len) 为有效键：外部（非本模块 API）改写/删除 trust.json 后，
        // 下一次读取必须看到新值，而不是永远返回缓存里的旧快照。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let agent_dir = settings_manager::agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let key = normalize_cwd(&repo);
        let path = trust_file_path(&agent_dir);

        std::fs::write(&path, format!("{{\"{key}\": true}}")).unwrap();
        assert!(is_project_trusted(&repo, &agent_dir));

        // 外部改写 → 缓存失效，读到新决策
        std::fs::write(&path, format!("{{\"{key}\": false}}")).unwrap();
        assert!(!is_project_trusted(&repo, &agent_dir));

        // 删除文件同样让缓存失效，回落默认策略（Ask → 不信任）
        let _ = std::fs::remove_file(&path);
        assert!(!is_project_trusted(&repo, &agent_dir));
    }

    #[test]
    fn legacy_null_placeholder_is_treated_as_no_decision() {
        // 旧版本写下的 null 占位仍须能读：等价于「无决策」，不阻塞启动询问、不掩盖祖先决策。
        let _ad = crate::test_support::AgentDirGuard::temp();
        let agent_dir = settings_manager::agent_dir();
        let dir = tempfile::tempdir().unwrap();
        let parent = dir.path().join("repo");
        let cwd = parent.join("proj");
        std::fs::create_dir_all(&cwd).unwrap();

        let mut map = Map::new();
        map.insert(normalize_cwd(&cwd), Value::Null);
        write_trust_file(&agent_dir, map);
        assert!(
            trust_decision(&cwd, &agent_dir).is_none(),
            "仅 null 占位 → 视为无显式决策"
        );

        // 祖先有决策时，null 占位不得掩盖它；并且写盘时这个占位被规范化掉
        set_project_trust(&parent, &agent_dir, true);
        assert_eq!(
            trust_decision(&cwd, &agent_dir),
            Some((normalize_cwd(&parent), true))
        );
        let map = read_trust_file(&agent_dir);
        assert!(
            !map.contains_key(&normalize_cwd(&cwd)),
            "旧 null 占位应在写盘时被清掉：{map:?}"
        );
    }
}

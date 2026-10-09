use crate::{
    PROJECT_SCOPE_NAME,
    core::{
        compaction::{DEFAULT_KEEP_RECENT_TOKENS, DEFAULT_RESERVE_TOKENS},
        config_cache::ConfigCache,
        project_trust,
    },
    error::{Error, Result},
    utils::{
        display::random_uuid_v4,
        file_lock::FileLock,
        mime::strip_bom,
        paths::{cwd, expand_home, expand_tilde, home_dir},
        wheel::WheelScrollLines,
    },
};
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock, RwLock,
        atomic::{AtomicBool, Ordering},
    },
};
use strum_macros::{EnumString, IntoStaticStr};

/// prompt 历史档位默认值。
pub const HISTORY_MAX_ENTRIES_DEFAULT: usize = 500;

/// prompt 历史档位可选值（`/settings` 循环项）；`0` = 不落盘。
pub const HISTORY_MAX_ENTRIES_TIERS: [usize; 5] = [0, 100, 250, 500, 1000];

/// 标准默认内置工具名（`defaultTools` 未给出纯工具名时的基线）
pub const DEFAULT_TOOL_NAMES: [&str; 4] = ["read", "bash", "edit", "write"];

/// `/settings` Retry max attempts 的可选档位；`0` = 出错不重试。
pub const RETRY_MAX_RETRIES_TIERS: [u32; 6] = [0, 1, 2, 3, 5, 10];

/// `/settings` Compact reserve tokens 的可选档位（为模型回复预留的 token 数）。
pub const COMPACT_RESERVE_TIERS: [u32; 5] = [4096, 8192, 16384, 32768, 65536];

/// `/settings` Compact keep recent tokens 的可选档位（不参与摘要的最近 token 数）。
pub const COMPACT_KEEP_RECENT_TIERS: [u32; 5] = [8000, 12000, 20000, 40000, 80000];

/// `/settings` Temperature 的可选档位；`default` 表示删除该键、回落到模型自身的采样参数。
pub const TEMPERATURE_TIERS: [&str; 6] = ["default", "0", "0.2", "0.5", "0.7", "1"];

/// settings.json 读写锁。锁作用域限定在 [`with_settings_read`] / [`with_settings_write`]
/// 的闭包内；文件另有自己的缓存层（[`GLOBAL_SETTINGS_CACHE`] / [`PROJECT_SETTINGS_CACHE`]），
/// 以 `(path, mtime, len)` 判定缓存有效性。写入口在写锁内完成整个读-改-写并回填缓存，
/// 因此不会丢更新；读入口在读锁内取缓存或读盘，不会与写盘交错（也就不会读到半截/空文件）。
static SETTINGS_LOCK: RwLock<()> = RwLock::new(());

/// 交互式 TUI 是否已接入：接入后，写盘发现 settings.json 无法解析时不再直接报错，
/// 而是暂存待写内容，由 TUI 弹窗让用户选择「覆写 / 取消」。
/// 非交互模式（print / SDK）保持 false：直接拒绝覆写并返回错误，避免静默摧毁用户文件。
static INTERACTIVE_SETTINGS_UI: AtomicBool = AtomicBool::new(false);

/// 待用户确认的 settings.json 覆写请求（path + 待写内容 + 展示原因）
static PENDING_SETTINGS_OVERWRITE: OnceLock<Mutex<Option<PendingSettingsOverwrite>>> =
    OnceLock::new();

/// settings.json 各自的缓存层（各自一把 `Mutex`，惰性初始化）。
/// 读/写都只在 [`with_settings_read`] / [`with_settings_write`] 的 `RwLock` 作用域内进行，
/// 锁序恒为「RwLock → 缓存 Mutex」。
static GLOBAL_SETTINGS_CACHE: ConfigCache = ConfigCache::new();
/// 项目级 settings.json 的解析缓存，减少重复读盘与 JSON 解析开销。
static PROJECT_SETTINGS_CACHE: ConfigCache = ConfigCache::new();

/// 在 settings.json 读锁内执行闭包。
fn with_settings_read<R>(f: impl FnOnce() -> R) -> R {
    let _guard = SETTINGS_LOCK.read().unwrap_or_else(|e| e.into_inner());
    f()
}

/// 在 settings.json 写锁内执行闭包：整个读-改-写都在锁内完成，保证进程内串行、无丢失。
///
/// **闭包内不得再调 [`with_settings_read`]**（同一把 `RwLock` 写锁内取读锁会自死锁）；
/// 需要读当前配置作基准时用不加锁的 [`read_settings_file`]，经由缓存（[`GLOBAL_SETTINGS_CACHE`]）读取。
fn with_settings_write<R>(f: impl FnOnce() -> R) -> R {
    let _guard = SETTINGS_LOCK.write().unwrap_or_else(|e| e.into_inner());
    f()
}

/// settings.json 跨进程写锁。
///
/// 进程内的 [`SETTINGS_LOCK`] 只串行本进程的写。两个 prux 进程并发写不同键时，
/// 各自基于读到的旧快照回写，后写者会用陈旧值覆盖前写者刚写的键
/// （read-modify-write 丢失更新）——`disabledExtensions` 等键会被静默改回旧值，
/// 表现为「footer 恢复默认值 / 配置丢失」。这里把整段「读-改-写」串行到跨进程粒度
/// （实现见 [`crate::utils::file_lock`]）。
fn acquire_settings_file_lock(path: &Path) -> Result<FileLock> {
    FileLock::acquire(path).map_err(|e| Error::msg(e.to_string()))
}

/// 暂存的 settings.json 覆写请求
#[derive(Debug, Clone)]
struct PendingSettingsOverwrite {
    /// 待写入的目标 settings.json 路径。
    path: PathBuf,
    /// 待写入的完整 JSON 值（本进程内所有被推迟的改动已合并）
    value: Value,
    /// 弹窗/提示展示的文件路径 + 解析问题原因
    detail: String,
}

/// 进程内暂存待确认覆写的全局槽位（懒初始化）。
fn pending_settings_overwrite_slot() -> &'static Mutex<Option<PendingSettingsOverwrite>> {
    PENDING_SETTINGS_OVERWRITE.get_or_init(|| Mutex::new(None))
}

/// 由交互式模式在启动时调用，标记 TUI 已接入（可弹覆写确认窗）。
pub fn set_interactive_settings_ui(attached: bool) {
    INTERACTIVE_SETTINGS_UI.store(attached, Ordering::Relaxed);
}

/// 当前是否有交互式 TUI 接入（决定损坏的 settings.json 是弹窗确认还是直接报错）。
/// 测试构建下线程本地 override 优先，用于隔离并行测试。
fn interactive_settings_ui_attached() -> bool {
    // 测试接缝：线程本地 override 优先（同步测试隔离，避免并行测试互踩）。
    #[cfg(any(test, feature = "test-support"))]
    if let Some(v) = crate::test_support::settings_ui_override() {
        return v;
    }

    INTERACTIVE_SETTINGS_UI.load(Ordering::Relaxed)
}

/// 暂存一次待确认的覆写（同文件的多次推迟改动合并为一个对象，避免丢失中间键）。
fn stage_settings_overwrite(path: PathBuf, value: Value, detail: String) {
    let mut guard = pending_settings_overwrite_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner());

    match guard.as_mut() {
        Some(pending) if pending.path == path => {
            match (pending.value.as_object_mut(), value.as_object()) {
                (Some(dst), Some(src)) => {
                    for (k, v) in src {
                        dst.insert(k.clone(), v.clone());
                    }
                }
                _ => pending.value = value,
            }
            pending.detail = detail;
        }
        _ => {
            *guard = Some(PendingSettingsOverwrite {
                path,
                value,
                detail,
            });
        }
    }
}

/// 是否有待确认的 settings.json 覆写请求；返回（展示用路径 + 原因）。
pub fn pending_settings_overwrite() -> Option<(String, String)> {
    pending_settings_overwrite_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|p| (p.path.display().to_string(), p.detail.clone()))
}

/// 用户选择「覆写」：把暂存内容强制写入（绕过守卫）。
pub fn confirm_settings_overwrite() -> Result<()> {
    // 与常规写盘共用同一把写锁：避免与并发 save_settings 交错
    with_settings_write(|| {
        let Some(pending) = pending_settings_overwrite_slot()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        else {
            return Ok(());
        };

        // 与常规写路径一致：跨进程写锁内落盘。
        let _file_lock = acquire_settings_file_lock(&pending.path)?;
        write_settings_unchecked(pending.value, pending.path)
    })
}

/// 用户选择「取消」：丢弃暂存请求，保留原文件。
pub fn cancel_settings_overwrite() {
    *pending_settings_overwrite_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
}

/// 写盘基准值：文件仍损坏且已有「暂存待确认」的覆写时，以待写内容为工作态。
///
/// 损坏文件的覆写被暂存后，磁盘上仍是旧内容；若后续写盘重新读盘，那些旧值会盖掉
/// 已暂存的改动（暂存合并是「后写赢」）。文件一旦恢复可解析则以磁盘为准：
/// 用户手工修复的内容优先，陈旧暂存请求在下次写盘成功时丢弃。
fn settings_value_for_write(path: &Path) -> Value {
    if settings_file_problem(path).is_none() {
        // 调用方已持写锁 + 跨进程文件锁。经由读缓存取基准：缓存层在“读取期间元数据
        // 变化”时会重读，避免并发非原子写（编辑器保存 / 旧版进程 / shell 重定向）
        // 的截断中间态被当成基准回写，把 disabledExtensions 等键洗没。
        // （无外部改写时缓存就是磁盘最新值，不会回写陈旧快照。）
        return GLOBAL_SETTINGS_CACHE.read(path, || read_settings_file(path));
    }

    let guard = pending_settings_overwrite_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    match guard.as_ref().filter(|p| p.path.as_path() == path) {
        Some(pending) => pending.value.clone(),
        None => {
            drop(guard);
            GLOBAL_SETTINGS_CACHE.read(path, || read_settings_file(path))
        }
    }
}

/// 丢弃同路径的待确认覆写请求（文件已恢复可写入时调用：推迟的原因已消失，
/// 陈旧暂存不得再在用户确认时把刚写好的文件盖回去）。
fn clear_pending_settings_overwrite(path: &Path) {
    let mut guard = pending_settings_overwrite_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if guard.as_ref().is_some_and(|p| p.path.as_path() == path) {
        *guard = None;
    }
}

/// 在写锁内对 settings.json 做一次 read-modify-write（保留其他键）。
/// `f` 接收当前对象并就地修改；文件不存在/非法与 `save_settings` 语义一致
/// （非法文件在交互模式会暂存待确认，否则报错）。
fn modify_global_settings<F>(f: F) -> Result<()>
where
    F: FnOnce(&mut serde_json::Map<String, Value>),
{
    with_settings_write(|| {
        let path = agent_dir().join("settings.json");

        // 跨进程写锁：整个读-改-写都在锁内完成（见 [`acquire_settings_file_lock`]），
        // 使其它进程不会用陈旧快照覆盖本次改动（反之亦然）。
        let _file_lock = acquire_settings_file_lock(&path)?;

        // 调用方已持写锁：直接用不加锁的基准读取。
        let mut v = settings_value_for_write(&path);
        if !v.is_object() {
            v = json!({});
        }

        let obj = v
            .as_object_mut()
            .ok_or_else(|| Error::msg("settings.json is not an object"))?;
        f(obj);
        save_settings(v, path)
    })
}

/// 全局配置目录（agent 目录）：测试 override > `$PRUX_AGENT_DIR` > `~/.prux`。
///
/// **测试构建**（`cfg(test)` / `feature = "test-support"`）的优先级：线程本地
/// `AgentDirGuard` override > 显式 `$PRUX_AGENT_DIR` pin > 测试沙箱
/// （见 [`test_agent_dir_fallback`]）。没有显式隔离时不回落真实 `~/.prux`，而是落到
/// 测试沙箱：配置写盘遍布单测，只要有一个测试忘了 `AgentDirGuard`/`pin_test_agent_dir`
/// 就会把用户的 `disabledExtensions` 等写坏。真实目录只由生产构建使用。
pub fn agent_dir() -> PathBuf {
    // 测试接缝：线程本地 override 优先（同步测试隔离，无需 set_var/锁）。
    #[cfg(any(test, feature = "test-support"))]
    {
        if let Some(dir) = crate::test_support::agent_dir_override() {
            return dir;
        }

        // 进程级 env pin 次之：spawn 到别的线程的 worker/agent 读不到 thread_local，
        // 只能靠 env 隔离（调用侧持叶级锁串行，见 test_support::pin_test_agent_dir）。
        if let Ok(dir) = std::env::var("PRUX_AGENT_DIR")
            && !dir.is_empty()
        {
            return expand_tilde(PathBuf::from(dir));
        }

        return test_agent_dir_fallback();
    }

    #[cfg(not(any(test, feature = "test-support")))]
    {
        if let Ok(dir) = std::env::var("PRUX_AGENT_DIR") {
            return expand_tilde(PathBuf::from(dir));
        }
        default_agent_dir()
    }
}

/// 测试构建下的兜底 agent 目录（进程级共享，与 `pin_test_agent_dir` 同一位置）。
///
/// 未显式隔离的测试写盘只会落在这里，绝不污染真实 `~/.prux`。
#[cfg(any(test, feature = "test-support"))]
fn test_agent_dir_fallback() -> PathBuf {
    let dir = std::env::temp_dir().join("prux-auth-tests");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 真实用户配置目录 `~/.prux`（生产默认；测试里仅用于断言与显式覆盖）。
///
/// 测试构建下 `agent_dir()` 不再调用它（改用测试沙箱），仅测试模块引用，
/// 故对测试构建放开 dead_code。
#[cfg_attr(any(test, feature = "test-support"), allow(dead_code))]
fn default_agent_dir() -> PathBuf {
    home_dir().join(PROJECT_SCOPE_NAME)
}

/// settings.json 解析出的用户配置：默认模型、主题、技能与启用模型等。
#[derive(Debug, Clone)]
pub struct Settings {
    /// 缺省模型 id；未配置时为 `None`，需由 CLI 或环境变量指定。
    pub default_model: Option<String>,
    /// 缺省 provider 名；未配置时为 `None`。
    pub default_provider: Option<String>,
    /// 缺省思考等级（如 off/high）；`None` 表示不覆盖内置默认。
    pub default_thinking_level: Option<String>,
    /// 项目信任策略，取 always / never；`None` 或其他值表示每次询问。
    pub default_project_trust: Option<String>,
    /// TUI 主题名或 JSON 路径；`None` 时回落到内置主题。
    pub theme: Option<String>,
    /// Ctrl+G 调用的外部编辑器命令；`None` 时回退 VISUAL/EDITOR/vi。
    pub external_editor: Option<String>,
    /// 自动发现技能的过滤规则：`!glob` 排除、`+path` 精确强制包含、`-path` 精确强制排除，
    /// 按数组顺序后者覆盖前者；**不**作用于 `--skill` 显式路径。
    /// 由 `/skills` 面板与手写配置共同维护（面板写 `-name`/删）。
    pub skills: Vec<String>,
    /// 额外的主题搜索路径（目录或 .json 文件）。
    pub themes: Vec<String>,
    /// 模型选择器循环名单（glob 模式，同 `--models` 格式）。
    pub enabled_models: Vec<String>,
}

/// 取 `v[key]` 中的字符串数组；键缺失、非数组或元素非字符串时该元素被丢弃，
/// 整体缺失返回空 Vec。
fn string_array(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

/// 读 settings.json（不加锁）：不存在/为空/非对象 → 空对象；整体解析成功 → 原值；
/// 存在但解析失败 → 尽力从损坏文本恢复顶层键（供读取与写盘合并，避免损坏时丢配置）。
fn read_settings_file(path: &Path) -> Value {
    let Ok(text) = std::fs::read_to_string(path) else {
        return json!({});
    };
    let text = strip_bom(&text);
    if text.trim().is_empty() {
        return json!({});
    }

    match serde_json::from_str::<Value>(text) {
        Ok(v) if v.is_object() => v,
        // 非对象（数组/标量）对齐「非法即默认」：读回空对象，由写盘守卫拒绝覆写
        Ok(_) => json!({}),
        Err(_) => Value::Object(salvage_settings_object(text)),
    }
}

/// 尽力从无法整体解析的 settings.json 文本中恢复顶层 `"key": value` 键值对。
///
/// 背景：损坏的文件若只读回「空对象」，写盘就只剩本次改动的键，其余配置全丢。
/// 这里在顶层（花括号深度 1）逐个识别键，值交给 serde_json 解析（支持嵌套对象/数组/
/// 字符串转义），无法解析的片段跳过继续扫描——破坏点之前的完整键值对都能救回。
fn salvage_settings_object(text: &str) -> serde_json::Map<String, Value> {
    let mut out = serde_json::Map::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    let mut depth = 0i32;

    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                // 字符串整体跳过（其中可能含 `:` `,` `{}`，不能按裸字符扫描）
                let Some((s, end)) = parse_json_token(text, i) else {
                    i += 1;
                    continue;
                };
                let key = match s {
                    Value::String(s) if depth == 1 => Some(s),
                    _ => None,
                };
                let Some(key) = key else {
                    i = end;
                    continue;
                };

                // 顶层字符串 + 紧邻 `:` + 值可解析 ⇒ 视为一个键值对
                let mut j = skip_ws(bytes, end);
                if bytes.get(j) != Some(&b':') {
                    i = end;
                    continue;
                }
                j = skip_ws(bytes, j + 1);
                match parse_json_token(text, j) {
                    Some((value, after)) => {
                        out.insert(key, value);
                        i = after;
                    }
                    // 值损坏：跳过键，继续从 `:` 之后扫描（同层后续键仍可救回）
                    None => i = j,
                }
            }
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }

    out
}

/// 从 `start` 起解析一个 JSON 值，返回（值, 结束后的字节偏移）。
/// `start` 不在字符边界或无法解析时返回 None。
fn parse_json_token(text: &str, start: usize) -> Option<(Value, usize)> {
    let de = serde_json::Deserializer::from_str(text.get(start..)?);
    let mut stream = de.into_iter::<Value>();
    match stream.next()? {
        Ok(v) => Some((v, start + stream.byte_offset())),
        Err(_) => None,
    }
}

/// 从字节偏移 `i` 起跳过 ASCII 空白，返回第一个非空白字符的下标（越界则返回长度）。
fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    i
}

/// 读全局 settings.json（持读锁，走 [`GLOBAL_SETTINGS_CACHE`]）。
fn settings_value() -> Value {
    let path = agent_dir().join("settings.json");
    with_settings_read(|| GLOBAL_SETTINGS_CACHE.read(&path, || read_settings_file(&path)))
}

/// 读一段扩展设置（`settings.json` 里的 `key` 对象）：项目层逐键覆盖全局层。
///
/// 两层都没有该键（或都不是对象）时返回 `Null`；类型校验、默认值由调用方负责。
pub fn settings_section(key: &str) -> Value {
    let global = settings_value().get(key).cloned().unwrap_or(Value::Null);
    let project = project_settings_value()
        .get(key)
        .cloned()
        .unwrap_or(Value::Null);

    let mut merged = global.as_object().cloned().unwrap_or_default();
    if let Some(project) = project.as_object() {
        for (k, v) in project {
            merged.insert(k.clone(), v.clone());
        }
    }

    if merged.is_empty() {
        Value::Null
    } else {
        Value::Object(merged)
    }
}

/// 项目级 PROJECT_SCOPE_NAME/settings.json（若存在且含 defaultTools 则替换全局）。
///
/// **受项目信任门控**：未信任的项目返回 Null。项目文件不该在未受信任的仓库里改掉启动行为
fn project_settings_value() -> Value {
    let cwd = cwd();

    // 信任判定可能回落到 default_project_trust → read_settings，必须在其返回后、
    // 而不是之前取锁（保证不会在持读锁时又取读锁）。
    if !project_trust::is_project_trusted(&cwd, &agent_dir()) {
        return Value::Null;
    }

    let path = cwd.join(PROJECT_SCOPE_NAME).join("settings.json");
    with_settings_read(|| PROJECT_SETTINGS_CACHE.read(&path, || read_settings_file(&path)))
}

/// 检查全局/项目 settings.json 是否可解析；返回完整警告文案（文件不存在不算错误）。
/// 无效 settings 文件应在交互启动时显示警告而非静默忽略。
pub fn settings_load_error() -> Option<String> {
    let path = agent_dir().join("settings.json");
    if let Ok(text) = std::fs::read_to_string(&path)
        && let Err(e) = serde_json::from_str::<Value>(strip_bom(&text))
    {
        return Some(format!("Invalid settings file {}: {}", path.display(), e));
    }

    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join(PROJECT_SCOPE_NAME).join("settings.json");
        if let Ok(t) = std::fs::read_to_string(&p)
            && let Err(e) = serde_json::from_str::<Value>(strip_bom(&t))
        {
            return Some(format!("Invalid settings file {}: {}", p.display(), e));
        }
    }
    None
}

/// `defaultTools` / `--tools` 的增删条目（`+name` / `-name`）
pub fn is_tool_modifier(entry: &str) -> bool {
    entry.starts_with('+') || entry.starts_with('-')
}

/// 校验 `--tools` 的条目列表：要么是纯名字/通配的 allowlist，要么全是 `+name`/`-name` 增删条目
/// （两者不可混用，且增删条目只接受精确名）。返回问题描述，合法时为 `None`。
pub fn get_tool_list_error(entries: &[String]) -> Option<String> {
    let modifiers = entries.iter().filter(|e| is_tool_modifier(e)).count();
    if modifiers == 0 {
        return None;
    }

    if modifiers < entries.len() {
        return Some("tool names cannot be mixed with +name or -name entries".to_string());
    }

    entries.iter().find(|e| e.contains('*')).map(|pattern| {
        format!("+name and -name entries take exact tool names, not patterns: {pattern}")
    })
}

/// 按顺序把 `+name` / `-name` 条目应用到 `base`：`+name` 追加（尚不存在时）、`-name` 删除。
///
/// 非增删条目一律忽略（调用方要么先过滤，要么已经校验过没有混用）。
pub fn apply_tool_modifiers(base: &[String], entries: &[String]) -> Vec<String> {
    let mut tools = base.to_vec();
    for entry in entries {
        if !is_tool_modifier(entry) {
            continue;
        }

        let name = &entry[1..];
        let index = tools.iter().position(|t| t == name);

        if entry.starts_with('+') {
            if index.is_none() && !name.is_empty() {
                tools.push(name.to_string());
            }
        } else if let Some(index) = index {
            tools.remove(index);
        }
    }
    tools
}

/// 合并两层 `defaultTools`：覆盖层全是 `+`/`-` 时追加到继承层（增删语义），否则整体替换。均未设置返回 `None`。
fn merge_default_tools(
    base: Option<&[String]>,
    overrides: Option<&[String]>,
) -> Option<Vec<String>> {
    let Some(overrides) = overrides else {
        return base.map(|b| b.to_vec());
    };
    let Some(base) = base else {
        return Some(overrides.to_vec());
    };
    if !overrides.iter().all(|e| is_tool_modifier(e)) {
        return Some(overrides.to_vec());
    }

    let mut out = base.to_vec();
    out.extend(overrides.iter().cloned());
    Some(out)
}

/// 求值合并后的 `defaultTools`：纯工具名替换基线，再按顺序应用 `+name` / `-name`。
/// 全部是增删条目时基线为 [`DEFAULT_TOOL_NAMES`]。
fn resolve_default_tools(entries: &[String]) -> Vec<String> {
    let plain: Vec<String> = entries
        .iter()
        .filter(|e| !is_tool_modifier(e))
        .cloned()
        .collect();

    let base: Vec<String> = if !plain.is_empty() || entries.is_empty() {
        plain
    } else {
        DEFAULT_TOOL_NAMES.iter().map(|s| s.to_string()).collect()
    };

    apply_tool_modifiers(&base, entries)
}

/// 从一份 settings 值里读 `defaultTools`；键缺失或不是数组时返回 `None`
/// （区别于「空数组」：空数组表示显式选择不使用任何工具）。
fn default_tools_of(v: &Value) -> Option<Vec<String>> {
    v.get("defaultTools")
        .and_then(|v| v.as_array())
        .map(|_| string_array(v, "defaultTools"))
}

/// 启动时的内置工具初始选择（settings.json `defaultTools`）。
///
/// 语法：纯工具名**替换**默认选择；`+name` 追加、`-name` 移除。
/// 项目设置叠加在用户设置之上：项目列表全是 `+`/`-` 时追加到用户列表（只增删），
/// 含纯工具名时整体替换项目自己的选择。两层均未设置返回 `None`
/// （调用方回落到 [`DEFAULT_TOOL_NAMES`]）。
pub fn read_settings_default_tools() -> Option<Vec<String>> {
    let global = default_tools_of(&settings_value());
    let project = default_tools_of(&project_settings_value());
    let merged = merge_default_tools(global.as_deref(), project.as_deref())?;
    Some(resolve_default_tools(&merged))
}

/// 写回 settings.json 的 `defaultTools`；`None` 或空列表删除该键（回落 [`DEFAULT_TOOL_NAMES`]）。
pub fn write_settings_default_tools(entries: Option<&[String]>) -> Result<()> {
    modify_global_settings(|obj| match entries {
        Some(list) if !list.is_empty() => {
            obj.insert("defaultTools".to_string(), json!(list));
        }
        _ => {
            obj.remove("defaultTools");
        }
    })
}

/// 切换某个内置工具在**全局** `defaultTools` 里的启用状态（`+name` / `-name` 增量语法）。
///
/// `enabled_now` 是调用方从 [`read_settings_default_tools`]（已含项目层）读到的**当前有效**状态。
/// 写入只改全局键、且只增删与 `name` 相关的条目：
/// - 关闭：已有 `+name` 就删掉它，否则在仍需移除时追加 `-name`；
/// - 开启：已有 `-name` 就删掉它，否则在仍被排除时追加 `+name`。
///
/// 这样手写的纯工具名列表不会被面板覆盖（只在其上叠加增删）。结果为空时删除整个键。
/// 唯一例外是**显式空数组**（`[]` = 「不要任何内置工具」）：增量语法表达不了它
/// （`+name` 会把基线拉回 [`DEFAULT_TOOL_NAMES`]，`-name` 又无可移除项），
/// 此时改写为纯工具名列表。
///
/// 返回写盘后的原始条目列表。
pub fn toggle_settings_default_tool(name: &str, enabled_now: bool) -> Result<Vec<String>> {
    let plus = format!("+{name}");
    let minus = format!("-{name}");
    let mut result: Vec<String> = Vec::new();

    modify_global_settings(|obj| {
        let explicit = obj.get("defaultTools").and_then(|v| v.as_array()).is_some();
        let mut entries: Vec<String> = obj
            .get("defaultTools")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        if explicit && entries.is_empty() {
            // 显式空数组：当前有效集就是空，开启则写纯工具名列表（关闭无可关）
            let mut tools = resolve_default_tools(&entries);
            if !enabled_now && !tools.iter().any(|t| t == name) {
                tools.push(name.to_string());
            }
            entries = tools;
        } else if enabled_now {
            if entries.contains(&plus) {
                entries.retain(|e| *e != plus);
            } else {
                let plain = entries.iter().any(|e| *e == name);
                let baseline = entries.iter().all(|e| is_tool_modifier(e))
                    && DEFAULT_TOOL_NAMES.contains(&name);
                if plain || baseline {
                    entries.push(minus);
                }
            }
        } else if entries.contains(&minus) {
            entries.retain(|e| *e != minus);
        } else {
            let plain = entries.iter().any(|e| *e == name);
            let baseline =
                entries.iter().all(|e| is_tool_modifier(e)) && DEFAULT_TOOL_NAMES.contains(&name);
            if !plain && !baseline {
                entries.push(plus);
            }
        }

        if entries.is_empty() {
            obj.remove("defaultTools");
        } else {
            obj.insert("defaultTools".to_string(), json!(entries));
        }
        result = entries;
    })?;

    Ok(result)
}

/// 读全局 settings.json 并投影为 [`Settings`]。
/// 缺失或类型不符的键一律为 `None`/空 Vec，由调用方回落到各自默认值；不返回错误。
pub fn read_settings() -> Settings {
    let v = settings_value();
    Settings {
        default_model: v
            .get("defaultModel")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        default_provider: v
            .get("defaultProvider")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        default_thinking_level: v
            .get("defaultThinkingLevel")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        default_project_trust: v
            .get("defaultProjectTrust")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        theme: v
            .get("theme")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        external_editor: v
            .get("externalEditor")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        skills: string_array(&v, "skills"),
        themes: string_array(&v, "themes"),
        enabled_models: string_array(&v, "enabledModels"),
    }
}

/// 读取 settings.json 中的 temperature 覆盖（优先级高于模型 samplingParams）
pub fn read_settings_temperature() -> Option<f64> {
    settings_value().get("temperature").and_then(|v| v.as_f64())
}

/// 读取 settings.json 中的 externalEditor（TUI Ctrl+G 使用）
pub fn read_settings_external_editor() -> Option<String> {
    settings_value()
        .get("externalEditor")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

/// settings.json 中禁用的扩展名列表（未列出默认启用）。
/// /extension 面板用；启动注册扩展时读取以应用持久化状态。
/// 注意：声明 `default_enabled() == false` 的扩展默认禁用，不在此列
/// （它们的显式开启写在 `enabledExtensions`）。
///
/// 项目层叠加在全局之上（两层取并集）：在项目 `.prux/settings.json` 里写
/// `disabledExtensions`，即可只在本项目禁用某内置扩展。
/// 面板写入仍固定写全局（见 [`write_disabled_extensions`]）。
/// 未受信任的项目不参与合并（见 [`project_settings_value`]）。
pub fn read_disabled_extensions() -> Vec<String> {
    merge_extension_names(
        string_array(&settings_value(), "disabledExtensions"),
        string_array(&project_settings_value(), "disabledExtensions"),
    )
}

/// settings.json 中显式启用的扩展名列表。
/// 仅用于声明 `default_enabled() == false` 的扩展（如 plan-mode、goal）：
/// 它们默认关闭，在此列出或经 /extension 面板开启后才生效；默认启用的扩展不写此键。
/// 项目层同样与全局取并集（项目可在自己仓库里启用那些默认关闭的扩展）。
pub fn read_enabled_extensions() -> Vec<String> {
    merge_extension_names(
        string_array(&settings_value(), "enabledExtensions"),
        string_array(&project_settings_value(), "enabledExtensions"),
    )
}

/// 合并全局与项目的扩展名列表：项目条目追加在全局之后，同名去重（保留首次出现）。
fn merge_extension_names(mut global: Vec<String>, project: Vec<String>) -> Vec<String> {
    for name in project {
        if !global.contains(&name) {
            global.push(name);
        }
    }
    global
}

/// settings.json 的扩展模式（"minimal" / "dev" / "creator" / "all"，缺失默认 "all"）。
/// /extension 面板 Tab 切换；重启后保持上次选择。
pub fn read_extension_mode() -> String {
    settings_value()
        .get("extensionMode")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "all".to_string())
}

/// 写回 settings.json 的 extensionMode（read-modify-write，保留其他键；不触碰各扩展的 enabled 状态）。
pub fn write_extension_mode(mode: &str) -> Result<()> {
    modify_global_settings(|obj| {
        obj.insert("extensionMode".to_string(), serde_json::json!(mode));
    })
}

/// 写回 settings.json 的 defaultProvider/defaultModel（read-modify-write，保留其他键）。
/// 切换模型时调用，重启后 resolve_provider_model 读取以恢复上次使用的模型。
pub fn write_default_model_and_provider(provider: &str, model_id: &str) -> Result<()> {
    modify_global_settings(|obj| {
        obj.insert("defaultProvider".to_string(), serde_json::json!(provider));
        obj.insert("defaultModel".to_string(), serde_json::json!(model_id));
    })
}

/// 读取 settings.json 的 enabledModels（Ctrl+P 循环名单，同 `--models` 格式）。
pub fn read_enabled_models() -> Vec<String> {
    string_array(&settings_value(), "enabledModels")
}

/// 读 settings.json 的 `skills` 过滤数组（原始规则字符串，未解析；
/// 语义见 [`crate::core::skills::load_skills`]）。
pub fn read_settings_skills() -> Vec<String> {
    string_array(&settings_value(), "skills")
}

/// 写回 settings.json 的 `skills` 过滤数组（read-modify-write，保留其他键）。
/// `/skills` 面板的 Ctrl+S 调用；空数组时删键（不写空数组噪声）。
pub fn write_settings_skills(rules: &[String]) -> Result<()> {
    modify_global_settings(|obj| {
        if rules.is_empty() {
            obj.remove("skills");
        } else {
            obj.insert("skills".to_string(), serde_json::json!(rules));
        }
    })
}

/// 写回 settings.json 的 enabledModels（read-modify-write，保留其他键）。
/// None / 空数组 = 全部启用：删除键（对齐 pi onPersist 的全选折叠）。
pub fn write_enabled_models(patterns: Option<&[String]>) -> Result<()> {
    modify_global_settings(|obj| match patterns {
        Some(pats) if !pats.is_empty() => {
            obj.insert("enabledModels".to_string(), serde_json::json!(pats));
        }
        _ => {
            obj.remove("enabledModels");
        }
    })
}

/// 写回 settings.json 的 theme（read-modify-write，保留其他键；/theme 命令与主题面板切换成功后调用，重启后恢复）。
pub fn write_theme(name: &str) -> Result<()> {
    modify_global_settings(|obj| {
        obj.insert("theme".to_string(), serde_json::json!(name));
    })
}

/// 写回**全局** settings.json 的 disabledExtensions（read-modify-write，保留其他键）。
/// /extension 面板切换启用状态后调用；项目级禁用靠手写项目 `.prux/settings.json`
/// （读取侧与全局取并集，见 [`read_disabled_extensions`]）。
pub fn write_disabled_extensions(disabled: &[String]) -> Result<()> {
    modify_global_settings(|obj| {
        obj.insert(
            "disabledExtensions".to_string(),
            serde_json::json!(disabled),
        );
    })
}

/// 一次写回**全局** settings.json 的 disabledExtensions 与 enabledExtensions
/// （read-modify-write，保留其他键）。/extension 面板切换后调用，
/// 见 [`crate::core::extensions::persist_extension_states`]。
pub fn write_extension_states(disabled: &[String], enabled: &[String]) -> Result<()> {
    modify_global_settings(|obj| {
        obj.insert(
            "disabledExtensions".to_string(),
            serde_json::json!(disabled),
        );
        obj.insert("enabledExtensions".to_string(), serde_json::json!(enabled));
    })
}

/// settings.json: deviceId（本机安装的稳定 UUID，仅供需要安装标识的登录流程使用）
pub fn read_settings_device_id() -> Option<String> {
    settings_value()
        .get("deviceId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
}

/// 取本机稳定 deviceId；不存在时生成 UUID v4 并写入全局设置。
pub fn get_or_create_device_id() -> Result<String> {
    if let Some(id) = read_settings_device_id() {
        return Ok(id);
    }
    let id = random_uuid_v4()?;
    write_settings_value("deviceId", json!(id))?;
    Ok(id)
}

/// 通用 settings.json 写单个键（read-modify-write，保留其他键）。
/// /settings 面板各设置项共用；
fn write_settings_value(key: &str, value: Value) -> Result<()> {
    modify_global_settings(|obj| {
        obj.insert(key.to_string(), value);
    })
}

/// settings.json: autoCompact（/settings Auto-compact；缺失默认 true）。
/// Agent 自动压缩由 core/agent_session 启动时读取。
pub fn read_settings_auto_compact() -> bool {
    settings_value()
        .get("autoCompact")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// settings.json: compactReserveTokens（默认 16384）；
/// 兼容嵌套键 `compaction.reserveTokens`（两者都缺时回落默认值）。
pub fn read_settings_compact_reserve() -> u32 {
    let v = settings_value();
    v.get("compactReserveTokens")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            v.pointer("/compaction/reserveTokens")
                .and_then(|v| v.as_u64())
        })
        .map(|v| v as u32)
        .unwrap_or(DEFAULT_RESERVE_TOKENS)
}

/// settings.json: compactKeepRecentTokens（默认 20000）；
/// 兼容嵌套键 `compaction.keepRecentTokens`。
pub fn read_settings_compact_keep_recent() -> u32 {
    let v = settings_value();
    v.get("compactKeepRecentTokens")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            v.pointer("/compaction/keepRecentTokens")
                .and_then(|v| v.as_u64())
        })
        .map(|v| v as u32)
        .unwrap_or(DEFAULT_KEEP_RECENT_TOKENS)
}

/// `compaction.modelOverrides["<provider>/<modelId>"].<field>`。
/// 键为精确的 `provider/modelId`；缺失/负整数时返回 None。
fn compact_model_override(provider: &str, model_id: &str, field: &str) -> Option<u32> {
    let key = format!("{provider}/{model_id}");
    settings_value()
        .pointer("/compaction/modelOverrides")
        .and_then(|v| v.get(key.as_str()))
        .and_then(|v| v.get(field))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
}

/// 该模型的 reserveTokens：modelOverrides → 全局 → 默认。
pub fn read_settings_compact_reserve_for(provider: &str, model_id: &str) -> u32 {
    compact_model_override(provider, model_id, "reserveTokens")
        .unwrap_or_else(read_settings_compact_reserve)
}

/// 该模型的 keepRecentTokens：modelOverrides → 全局 → 默认。
pub fn read_settings_compact_keep_recent_for(provider: &str, model_id: &str) -> u32 {
    compact_model_override(provider, model_id, "keepRecentTokens")
        .unwrap_or_else(read_settings_compact_keep_recent)
}

/// 写回 settings.json 的 exposeSessionEnvironment（read-modify-write，保留其他键）。
pub fn write_settings_expose_session_environment(enabled: bool) -> Result<()> {
    write_settings_value("exposeSessionEnvironment", json!(enabled))
}

/// 写回 settings.json 的 autoCompact（read-modify-write，保留其他键）。
pub fn write_settings_auto_compact(enabled: bool) -> Result<()> {
    write_settings_value("autoCompact", json!(enabled))
}

/// settings.json: exposeSessionEnvironment（默认 true）
pub fn read_settings_expose_session_environment() -> bool {
    settings_value()
        .get("exposeSessionEnvironment")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// settings.json: shellCommandPrefix（默认空）
pub fn read_settings_shell_command_prefix() -> String {
    settings_value()
        .get("shellCommandPrefix")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_default()
}

/// settings.json: shellPath（含 ~ 展开）
pub fn read_settings_shell_path() -> Option<String> {
    settings_value()
        .get("shellPath")
        .and_then(|v| v.as_str())
        .map(expand_home)
        .map(|s| s.to_string())
}

/// settings.json: retry.enabled（默认 true）
pub fn read_settings_retry_enabled() -> bool {
    settings_value()
        .get("retry")
        .and_then(|v| v.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// settings.json: retry.maxRetries（默认 3）
pub fn read_settings_retry_max_retries() -> u32 {
    settings_value()
        .get("retry")
        .and_then(|v| v.get("maxRetries"))
        .and_then(|v| v.as_u64())
        .map(|v| v as u32)
        .unwrap_or(3)
}

/// settings.json: retry.baseDelayMs（默认 2000）
pub fn read_settings_retry_base_delay_ms() -> u64 {
    settings_value()
        .get("retry")
        .and_then(|v| v.get("baseDelayMs"))
        .and_then(|v| v.as_u64())
        .unwrap_or(2000)
}

/// settings.json: images.autoResize（默认 true）
pub fn read_settings_images_auto_resize() -> bool {
    settings_value()
        .get("images")
        .and_then(|v| v.get("autoResize"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// 写回 settings.json 的 `images.autoResize`（保留 `images` 下其他键）。
pub fn write_settings_images_auto_resize(enabled: bool) -> Result<()> {
    write_settings_nested("images", "autoResize", json!(enabled))
}

/// 写回 settings.json 的 `images.blockImages`（保留 `images` 下其他键）。
pub fn write_settings_images_block_images(enabled: bool) -> Result<()> {
    write_settings_nested("images", "blockImages", json!(enabled))
}

/// 写回 settings.json 的 `retry.enabled`（保留 `retry` 下其他键）。
pub fn write_settings_retry_enabled(enabled: bool) -> Result<()> {
    write_settings_nested("retry", "enabled", json!(enabled))
}

/// 写回 settings.json 的 `retry.maxRetries`（保留 `retry` 下其他键）。
pub fn write_settings_retry_max_retries(max_retries: u32) -> Result<()> {
    write_settings_nested("retry", "maxRetries", json!(max_retries))
}

/// 写回 settings.json 的 `temperature`；`None` 删除该键（回落到模型自身的采样参数）。
pub fn write_settings_temperature(value: Option<f64>) -> Result<()> {
    modify_global_settings(|obj| match value {
        Some(t) => {
            obj.insert("temperature".to_string(), json!(t));
        }
        None => {
            obj.remove("temperature");
        }
    })
}

/// 写回 settings.json 的 `compactReserveTokens`（顶层键；不触碰 `compaction` 嵌套写法）。
pub fn write_settings_compact_reserve(tokens: u32) -> Result<()> {
    write_settings_value("compactReserveTokens", json!(tokens))
}

/// 写回 settings.json 的 `compactKeepRecentTokens`（顶层键；不触碰 `compaction` 嵌套写法）。
pub fn write_settings_compact_keep_recent(tokens: u32) -> Result<()> {
    write_settings_value("compactKeepRecentTokens", json!(tokens))
}

/// 在 settings.json 的嵌套对象 `group` 下写入 `key`；`group` 不是对象时重建为对象
/// （原值不是对象则无可保留的兄弟键）。
fn write_settings_nested(group: &str, key: &str, value: Value) -> Result<()> {
    modify_global_settings(|obj| {
        let entry = obj.entry(group.to_string()).or_insert_with(|| json!({}));
        if !entry.is_object() {
            *entry = json!({});
        }
        if let Some(map) = entry.as_object_mut() {
            map.insert(key.to_string(), value);
        }
    })
}

/// settings.json: images.blockImages（默认 false）
pub fn read_settings_images_block_images() -> bool {
    settings_value()
        .get("images")
        .and_then(|v| v.get("blockImages"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// settings.json: cacheWarming（"off" | "streaming" | "idle"，缺失/非法默认 "off"）。
///
/// **只读全局** settings：项目级不生效（每次保温都真花钱，项目文件不该偷偷改变花费行为）。
pub fn read_settings_cache_warming() -> String {
    let value = settings_value();
    let raw = value
        .get("cacheWarming")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if matches!(raw, "off" | "streaming" | "idle") {
        raw.to_string()
    } else {
        "off".to_string()
    }
}

/// 写回 settings.json 的 cacheWarming（/settings Cache warming）。
/// 返回写盘后的规范化值（非法输入按默认处理）。
pub fn write_settings_cache_warming(mode: &str) -> Result<String> {
    let normalized = if matches!(mode, "off" | "streaming" | "idle") {
        mode
    } else {
        "off"
    };

    write_settings_value("cacheWarming", serde_json::json!(normalized))?;
    Ok(normalized.to_string())
}

/// settings.json: showCacheMissNotices（缓存保温通知，默认 false）
pub fn read_settings_show_cache_miss_notices() -> bool {
    settings_value()
        .get("showCacheMissNotices")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 写回 settings.json 的 showCacheMissNotices。
pub fn write_settings_show_cache_miss_notices(enabled: bool) -> Result<()> {
    write_settings_value("showCacheMissNotices", serde_json::json!(enabled))
}

/// settings.json: steeringMode（/settings Steering mode；缺失默认 one-at-a-time）。
pub fn read_settings_steering_mode() -> String {
    settings_value()
        .get("steeringMode")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "one-at-a-time".to_string())
}

/// 写回 settings.json 的 steeringMode（/settings Steering mode）。
pub fn write_settings_steering_mode(mode: &str) -> Result<()> {
    write_settings_value("steeringMode", json!(mode))
}

/// settings.json: followUpMode（/settings Follow-up mode；缺失默认 one-at-a-time）。
pub fn read_settings_follow_up_mode() -> String {
    settings_value()
        .get("followUpMode")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| "one-at-a-time".to_string())
}

/// 写回 settings.json 的 followUpMode（/settings Follow-up mode）。
pub fn write_settings_follow_up_mode(mode: &str) -> Result<()> {
    write_settings_value("followUpMode", json!(mode))
}

/// settings.json: hideThinkingBlock（/settings Hide thinking；缺失默认 false）。
/// TUI 启动时读取覆盖 App.show_thinking。
pub fn read_settings_hide_thinking() -> bool {
    settings_value()
        .get("hideThinkingBlock")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 写回 settings.json 的 hideThinkingBlock（/settings Hide thinking）。
pub fn write_settings_hide_thinking(enabled: bool) -> Result<()> {
    write_settings_value("hideThinkingBlock", json!(enabled))
}

/// settings.json `quietStartup` 的档位：启动横幅与启动资源清单怎么显示。
///
/// 档位与字符串（`"false"` / `"header"` / `"true"`）的互转由 `strum` 派生，
/// 见 [`QuietStartup::as_str`] / [`QuietStartup::parse`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
pub enum QuietStartup {
    /// `false`：横幅与资源清单都显示
    #[strum(serialize = "false")]
    Full,
    /// `"header"`：只显示横幅（版本与按键提示），不输出资源清单。
    #[strum(serialize = "header")]
    Header,
    /// `true`：横幅与资源清单都不显示。
    #[strum(serialize = "true")]
    Silent,
}

impl QuietStartup {
    /// 是否显示启动横幅（版本 / 常用工具 / 帮助提示）。
    pub fn shows_banner(self) -> bool {
        !matches!(self, QuietStartup::Silent)
    }

    /// 是否在启动时输出资源清单（模型 / 会话 / 工具 / 上下文 / 技能 / 模板 / 扩展）。
    pub fn shows_details(self) -> bool {
        matches!(self, QuietStartup::Full)
    }

    /// 档位名（`strum` 派生：`false` / `header` / `true`），写回 settings.json 与设置面板显示共用。
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 档位名（去首尾空白）→ 档位；无法识别（含未设置）时为 `None`。
    pub fn parse(value: &str) -> Option<QuietStartup> {
        value.trim().parse().ok()
    }
}

/// settings.json: quietStartup（缺失 / 非法值按 [`QuietStartup::Header`]）。
///
/// pi 的默认值是 `false`（每次都输出资源清单），prux 保持自己的既有行为：
/// 未设置时不输出清单（相当于 `"header"`），需要清单就显式写 `false` 或加 `--verbose`。
pub fn read_settings_quiet_startup() -> QuietStartup {
    settings_value()
        .get("quietStartup")
        .and_then(|v| match v {
            Value::Bool(true) => Some(QuietStartup::Silent),
            Value::Bool(false) => Some(QuietStartup::Full),
            Value::String(s) => QuietStartup::parse(s),
            _ => None,
        })
        .unwrap_or(QuietStartup::Header)
}

/// 写回 settings.json 的 quietStartup。
pub fn write_settings_quiet_startup(level: QuietStartup) -> Result<()> {
    let value = match level {
        QuietStartup::Full => json!(false),
        QuietStartup::Header => json!("header"),
        QuietStartup::Silent => json!(true),
    };
    write_settings_value("quietStartup", value)
}

/// settings.json: syntaxHighlight（/settings Syntax highlight；缺失默认 true）。
/// TUI 启动时读取覆盖 Theme.syntax_highlight（代码块语法高亮开关）。
pub fn read_settings_syntax_highlight() -> bool {
    settings_value()
        .get("syntaxHighlight")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// 写回 settings.json 的 syntaxHighlight（/settings Syntax highlight）。
pub fn write_settings_syntax_highlight(enabled: bool) -> Result<()> {
    write_settings_value("syntaxHighlight", json!(enabled))
}

/// settings.json: mermaid（/settings Mermaid；缺失默认 true）。
/// Mermaid 代码块 → Unicode 流程图渲染开关；关闭时按普通代码块原样展示。
pub fn read_settings_mermaid() -> bool {
    settings_value()
        .get("mermaid")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// 写回 settings.json 的 mermaid（/settings Mermaid）。
pub fn write_settings_mermaid(enabled: bool) -> Result<()> {
    write_settings_value("mermaid", json!(enabled))
}

/// settings.json: showImages（/settings Show images；缺失默认 false）。
///
/// 开启后在支持图形协议的终端里内联显示消息与工具结果里的图片；关闭（默认）时
/// 图片块整块不渲染，且不探测终端能力、不占用启动时间。
pub fn read_settings_show_images() -> bool {
    settings_value()
        .get("showImages")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 写回 settings.json 的 showImages（/settings Show images）。
pub fn write_settings_show_images(enabled: bool) -> Result<()> {
    write_settings_value("showImages", json!(enabled))
}

/// settings.json: latex（/settings LaTeX；缺失默认 true）。
/// `$...$`/`$$...$$` 公式 → Unicode 渲染开关；关闭时保留原始文本。
pub fn read_settings_latex() -> bool {
    settings_value()
        .get("latex")
        .and_then(|v| v.as_bool())
        .unwrap_or(true)
}

/// 写回 settings.json 的 latex（/settings LaTeX）。
pub fn write_settings_latex(enabled: bool) -> Result<()> {
    write_settings_value("latex", json!(enabled))
}

/// settings.json: autocompleteMaxVisible（/settings Autocomplete max items；缺失默认 5）。
/// TUI 候选列表渲染读取
pub fn read_settings_autocomplete_max_visible() -> usize {
    settings_value()
        .get("autocompleteMaxVisible")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .filter(|n| *n > 0)
        .unwrap_or(5)
}

/// 写回 settings.json 的 autocompleteMaxVisible（/settings Autocomplete max items）。
pub fn write_settings_autocomplete_max_visible(n: usize) -> Result<()> {
    write_settings_value("autocompleteMaxVisible", json!(n))
}

/// settings.json: fullscreenWheelScrollLines（/settings Wheel scroll lines）。
/// 数字 1-100 = 每次滚轮事件的行数；`"auto"`/缺失 = 按滚动速度加速（孤立一格 1 行、
/// 快速连滚最多 10 行）。local macOS 终端已由系统加速，auto 恒 1 行。
pub fn read_settings_fullscreen_wheel_scroll_lines() -> WheelScrollLines {
    WheelScrollLines::from_json(settings_value().get("fullscreenWheelScrollLines"))
}

/// 写回 settings.json 的 fullscreenWheelScrollLines；`Lines(n)` 会先钳制到 1..=[`WheelScrollLines::MAX`]。
pub fn write_settings_fullscreen_wheel_scroll_lines(lines: WheelScrollLines) -> Result<()> {
    let value = match lines {
        WheelScrollLines::Auto => json!("auto"),
        WheelScrollLines::Lines(n) => json!(n.clamp(1, WheelScrollLines::MAX)),
    };
    write_settings_value("fullscreenWheelScrollLines", value)
}

/// settings.json: historyMaxEntries（/settings History max entries；缺失默认 500）。
///
/// **必须区分「键缺失 → 500」和「键 = 0 → 不落盘」**：这里刻意不做
/// `filter(|n| *n > 0)`（`autocompleteMaxVisible` 那样会把 0 静默变成默认值，
/// 用户以为关了历史、其实在偷偷写盘）。手改 JSON 的任意非负值都按原样生效，
/// 面板只提供 [`HISTORY_MAX_ENTRIES_TIERS`] 这几档。
pub fn read_settings_history_max_entries() -> usize {
    settings_value()
        .get("historyMaxEntries")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(HISTORY_MAX_ENTRIES_DEFAULT)
}

/// 写回 settings.json 的 historyMaxEntries；`0` 按原样落盘（表示不写历史）。
pub fn write_settings_history_max_entries(n: usize) -> Result<()> {
    write_settings_value("historyMaxEntries", json!(n))
}

/// 写回 settings.json 的 defaultProjectTrust（/settings Default project trust）。
/// 值域 ask/always/never，与 project_trust::default_project_trust 读取一致。
pub fn write_settings_default_project_trust(trust: &str) -> Result<()> {
    write_settings_value("defaultProjectTrust", json!(trust))
}

/// 写回 settings.json 的 defaultThinkingLevel（/settings Thinking level 子菜单）。
/// 重启后启动阶段的 thinking 级别恢复。
pub fn write_settings_default_thinking_level(level: &str) -> Result<()> {
    write_settings_value("defaultThinkingLevel", json!(level))
}

/// settings.json: skipUpdateVersion（更新提示里选「Don't ask again」时记录的版本号）。
///
/// 只抑制**该版本**的提示：后来发布的更新版本仍会重新提示。缺失 / 空串 = 未忽略任何版本。
pub fn read_skip_update_version() -> Option<String> {
    settings_value()
        .get("skipUpdateVersion")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// 写回 settings.json 的 skipUpdateVersion（read-modify-write，保留其他键）。
pub fn write_skip_update_version(version: &str) -> Result<()> {
    write_settings_value("skipUpdateVersion", json!(version))
}

/// 目标 settings.json 是否存在但无法解析为 JSON 对象；返回展示原因。
///
/// 背景：`read_settings_file` 对解析失败只能救回部分键，各写函数会在此基础上回写。
/// 损坏/非对象的文件必须拒绝直接覆写，否则用户原有条目会被静默重置。
/// 因此写盘前必须判定：合法对象/不存在/空文件 → `None`（可直接写）；否则返回问题描述。
fn settings_file_problem(path: &Path) -> Option<String> {
    let existing = std::fs::read_to_string(path).ok()?;
    let text = strip_bom(&existing);
    if text.trim().is_empty() {
        return None;
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Object(_)) => None,
        Ok(_) => Some(format!("{} is not a JSON object", path.display())),
        Err(e) => Some(format!("{} is not valid JSON ({})", path.display(), e)),
    }
}

/// 设置文件权限（写临时文件后、rename 前调用，避免敏感文件以默认权限短暂暴露）。
fn set_file_mode(path: &Path, mode: Option<u32>) {
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).ok();
    }

    #[cfg(not(unix))]
    let _ = (path, mode);
}

/// 原子写配置文件：同目录临时文件写入后 rename 覆盖。
///
/// 直接 `std::fs::write` 是 truncate→write：并发读者（其它进程，或本进程
/// 不持读锁的读取方）会看到空/半截文件；写盘途中崩溃也会
/// 把原文件截断。临时文件 + rename 使读者只可能看到完整的旧值或新值，
/// 从根上消除「一次空读被后续 read-modify-write 固化成配置丢失」。
///
/// `mode` 非空时在 rename 前设置权限（如 auth.json/models-store.json 的 0600）。
pub(crate) fn atomic_write_config(path: &Path, bytes: &[u8], mode: Option<u32>) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| Error::Io {
            context: format!("Failed to create dir: {}", parent.display()),
            source,
        })?;
    }

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");

    // 同目录临时文件：rename 必须同文件系统才能原子；pid 区分并发进程。
    let tmp = path.with_file_name(format!("{}.{}.tmp", name, std::process::id()));

    let result = (|| -> std::io::Result<()> {
        std::fs::write(&tmp, bytes)?;
        set_file_mode(&tmp, mode);
        std::fs::rename(&tmp, path)
    })();

    match result {
        Ok(()) => Ok(()),
        Err(source) => {
            _ = std::fs::remove_file(&tmp);
            Err(Error::Io {
                context: format!("Failed to write {}", path.display()),
                source,
            })
        }
    }
}

/// 无条件写入 settings.json（不做守卫；供「用户确认覆写」路径使用）。
fn write_settings_unchecked(v: Value, path: PathBuf) -> Result<()> {
    let text = serde_json::to_string_pretty(&v).map_err(|source| Error::Json {
        context: "Failed to serialize settings.json".to_string(),
        source,
    })?;
    atomic_write_config(&path, text.as_bytes(), None)?;

    // 落盘成功后回填缓存（取写盘后的元数据），下次读取直接命中，无需再读盘/解析。
    GLOBAL_SETTINGS_CACHE.store(&path, v);
    Ok(())
}

/// 带守卫地写 settings.json：目标文件存在但无法解析为 JSON 对象时拒绝覆写。
/// 交互式 TUI 已接入则把内容暂存待用户确认并返回 `Ok`；否则返回 `Err` 保留原文件。
fn save_settings(v: Value, path: PathBuf) -> Result<()> {
    // 守卫：目标文件已存在但无法解析为 JSON 对象时不直接覆盖。
    // - 交互式 TUI 已接入：暂存待写内容，返回 Ok 由 TUI 弹窗让用户选择覆写/取消；
    // - 非交互模式：拒绝覆写并报错（保留原文件供修复）。
    if let Some(detail) = settings_file_problem(&path) {
        if interactive_settings_ui_attached() {
            stage_settings_overwrite(path, v, detail);
            return Ok(());
        }
        return Err(Error::msg(format!(
            "{}; refusing to overwrite it. Fix or remove the file and retry.",
            detail
        )));
    }

    // 文件可正常写入：推迟的原因已消失，丢弃同路径的陈旧暂存请求
    clear_pending_settings_overwrite(&path);
    write_settings_unchecked(v, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每测试独立临时 agent_dir（线程本地 override）：f 内所有 agent_dir() 调用隔离，
    /// 并行测试互不干扰。
    fn with_temp_agent_dir(f: impl FnOnce()) {
        let _ad = crate::test_support::AgentDirGuard::temp();
        f();
    }

    /// 默认全局目录是 HOME 下的 `~/.prux`。
    ///
    /// 直接测 [`default_agent_dir`]：`agent_dir()` 还会看测试 override / `PRUX_AGENT_DIR`
    /// （两者都是进程级/线程级共享状态，断言里读它们会与其他测试互踩）。
    #[test]
    fn default_agent_dir_is_dot_prux_under_home() {
        let home = tempfile::tempdir().unwrap();
        let _home = crate::test_support::HomeGuard::set(home.path());

        assert_eq!(default_agent_dir(), home.path().join(PROJECT_SCOPE_NAME));
        assert_eq!(default_agent_dir().file_name().unwrap(), ".prux");
    }

    /// 回归：测试构建下没有显式隔离时，`agent_dir()` 也绝不能落到真实 `~/.prux`。
    ///
    /// 单测里配置写盘遍地都是，只要一个测试忘了 `AgentDirGuard`/`pin_test_agent_dir`
    /// 就会把用户的 `disabledExtensions` 等写坏（重启 prux 后发现被清空）。
    #[test]
    fn agent_dir_never_defaults_to_real_home_in_tests() {
        // 本测试线程没有 AgentDirGuard（若进程已被 pin，env 分支也算通过）。
        let dir = agent_dir();
        match std::env::var("PRUX_AGENT_DIR") {
            Ok(pinned) if !pinned.is_empty() => {
                assert_eq!(dir, expand_tilde(PathBuf::from(pinned)));
            }
            _ => {
                assert_ne!(dir, default_agent_dir(), "测试构建不得落到真实 ~/.prux");
                assert!(
                    dir.starts_with(std::env::temp_dir()),
                    "应回落到测试沙箱，实际 {}",
                    dir.display()
                );
            }
        }
    }

    /// 项目 `.prux/settings.json` 的 disabledExtensions / enabledExtensions
    /// 叠加在全局之上（并集去重）；未受信任的项目不参与合并。
    #[test]
    fn extension_state_lists_merge_project_layer() {
        // 项目目录走线程本地 CwdGuard（不再 set_current_dir）；仍持锁：
        // 会话信任与 settings 缓存是进程级共享态，与其它碰它们的测试串行
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);
            let _ = write_settings_value("disabledExtensions", json!(["global-only"]));

            let proj = std::env::temp_dir().join("prux-ext-proj");
            let proj_conf = proj.join(".prux");
            let _ = std::fs::remove_dir_all(&proj);
            std::fs::create_dir_all(&proj_conf).unwrap();
            std::fs::write(
                proj_conf.join("settings.json"),
                r#"{"disabledExtensions":["codemode","global-only"],"enabledExtensions":["plan-mode"]}"#,
            )
            .unwrap();

            // 未信任：项目层被忽略（项目目录用线程本地 CwdGuard 指向，不 chdir）
            let cwd_untrusted = crate::test_support::CwdGuard::set(proj.clone());
            let untrusted = read_disabled_extensions();
            let untrusted_enabled = read_enabled_extensions();
            drop(cwd_untrusted);
            assert_eq!(untrusted, vec!["global-only".to_string()]);
            assert!(untrusted_enabled.is_empty());

            // 受信任：并集（项目条目追加在后，同名去重）
            crate::core::project_trust::set_session_trust(&proj, true);
            let cwd_trusted = crate::test_support::CwdGuard::set(proj.clone());
            let merged = read_disabled_extensions();
            let enabled = read_enabled_extensions();
            drop(cwd_trusted);
            assert_eq!(
                merged,
                vec!["global-only".to_string(), "codemode".to_string()]
            );
            assert_eq!(enabled, vec!["plan-mode".to_string()]);

            crate::core::project_trust::clear_session_trust(&proj);
            let _ = std::fs::remove_dir_all(&proj);
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn disabled_extensions_roundtrip_keeps_other_keys() {
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);

            // 默认：无禁用列表
            assert!(read_disabled_extensions().is_empty());

            // 写入后读回
            write_disabled_extensions(&["a".to_string(), "b".to_string()]).unwrap();
            let mut got = read_disabled_extensions();
            got.sort();
            assert_eq!(got, vec!["a".to_string(), "b".to_string()]);

            // read-modify-write：不覆盖已有键（theme）
            let v = settings_value();
            assert!(v.get("disabledExtensions").is_some());
            write_disabled_extensions(&["c".to_string()]).unwrap();
            assert_eq!(read_disabled_extensions(), vec!["c".to_string()]);

            // 覆盖为全空：相当于恢复默认（全部扩展启用）
            write_disabled_extensions(&[]).unwrap();
            assert!(read_disabled_extensions().is_empty());
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn enabled_extensions_roundtrip_keeps_other_keys() {
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);

            // 默认：无显式启用列表（默认关闭的内置扩展保持关闭）
            assert!(read_enabled_extensions().is_empty());

            // 一次写入两键（disabledExtensions + enabledExtensions）：read-modify-write 保留 theme
            write_theme("synthwave").unwrap();
            write_extension_states(&["x".to_string()], &["plan-mode".to_string()]).unwrap();
            assert_eq!(read_disabled_extensions(), vec!["x".to_string()]);
            assert_eq!(read_enabled_extensions(), vec!["plan-mode".to_string()]);
            assert_eq!(settings_value()["theme"], "synthwave");

            // 覆盖为空：两键都清空，theme 仍在
            write_extension_states(&[], &[]).unwrap();
            assert!(read_disabled_extensions().is_empty());
            assert!(read_enabled_extensions().is_empty());
            assert_eq!(settings_value()["theme"], "synthwave");
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn default_model_and_provider_roundtrip() {
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);

            // 默认：无保存的模型
            let s = read_settings();
            assert!(s.default_provider.is_none());
            assert!(s.default_model.is_none());

            // 写入后读回（对齐 pi setDefaultModelAndProvider）
            write_default_model_and_provider("deepseek", "deepseek-v4-pro").unwrap();
            let s = read_settings();
            assert_eq!(s.default_provider.as_deref(), Some("deepseek"));
            assert_eq!(s.default_model.as_deref(), Some("deepseek-v4-pro"));

            // read-modify-write：不覆盖已有键（theme）
            let v = settings_value();
            assert!(v.get("theme").is_none());
            write_default_model_and_provider("opencode", "some-model").unwrap();
            let s = read_settings();
            assert_eq!(s.default_provider.as_deref(), Some("opencode"));
            assert_eq!(s.default_model.as_deref(), Some("some-model"));

            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn enabled_models_roundtrip_and_collapse() {
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);

            // 默认：无 enabledModels
            assert!(read_enabled_models().is_empty());
            assert!(read_settings().enabled_models.is_empty());

            // 写入后读回（对齐 pi enabledModels patterns）
            let pats = vec!["claude-*".to_string(), "deepseek-v4-pro:high".to_string()];
            write_enabled_models(Some(&pats)).unwrap();
            assert_eq!(read_enabled_models(), pats);
            assert_eq!(read_settings().enabled_models, pats);

            // read-modify-write：不覆盖已有键（defaultProvider）
            write_default_model_and_provider("deepseek", "deepseek-v4-pro").unwrap();
            write_enabled_models(Some(&["gpt-4o".to_string()])).unwrap();
            let s = read_settings();
            assert_eq!(s.default_provider.as_deref(), Some("deepseek"));
            assert_eq!(s.enabled_models, vec!["gpt-4o".to_string()]);

            // 空数组/None：删除键（全选折叠，对齐 pi）
            write_enabled_models(Some(&[])).unwrap();
            assert!(read_enabled_models().is_empty());
            assert!(settings_value().get("enabledModels").is_none());
            write_enabled_models(Some(&["gpt-4o".to_string()])).unwrap();
            write_enabled_models(None).unwrap();
            assert!(settings_value().get("enabledModels").is_none());

            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn default_tools_plus_minus_syntax() {
        // 对齐 pi 0.99：`+name` 追加、`-name` 移除；纯工具名才替换基线
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // 只给增删条目：基线为标准默认 read/bash/edit/write
        assert_eq!(
            resolve_default_tools(&s(&["+web_search"])),
            s(&["read", "bash", "edit", "write", "web_search"])
        );
        assert_eq!(
            resolve_default_tools(&s(&["-bash"])),
            s(&["read", "edit", "write"])
        );
        // 已存在不重复添加；移除后可以再加回
        assert_eq!(
            resolve_default_tools(&s(&["+bash"])),
            s(&["read", "bash", "edit", "write"])
        );
        assert_eq!(
            resolve_default_tools(&s(&["read", "-read", "+read"])),
            s(&["read"])
        );
        // 纯工具名替换基线；空数组 = 无内置（扩展保留）
        assert_eq!(
            resolve_default_tools(&s(&["read", "bash"])),
            s(&["read", "bash"])
        );
        assert_eq!(resolve_default_tools(&s(&[])), Vec::<String>::new());
        // `-name` 移除不存在的工具是 no-op
        assert_eq!(
            resolve_default_tools(&s(&["-nope"])),
            s(&["read", "bash", "edit", "write"])
        );

        // 层叠：项目全是增删 → 追加（只改增量）；含纯名 → 整体替换
        assert_eq!(
            merge_default_tools(Some(&s(&["read"])), Some(&s(&["+bash"]))),
            Some(s(&["read", "+bash"]))
        );
        assert_eq!(
            merge_default_tools(Some(&s(&["read"])), Some(&s(&["edit", "+bash"]))),
            Some(s(&["edit", "+bash"]))
        );
        assert_eq!(
            merge_default_tools(Some(&s(&["read"])), None),
            Some(s(&["read"]))
        );
        assert_eq!(merge_default_tools(None, None), None);
        // 继承层缺失 + 覆盖层纯增量：按基线应用增量
        assert_eq!(
            resolve_default_tools(&merge_default_tools(None, Some(&s(&["+web_search"]))).unwrap()),
            s(&["read", "bash", "edit", "write", "web_search"])
        );
    }

    /// `--tools` 的条目校验：纯名字/通配与 `+name`/`-name` 不可混用，后者只接受精确名。
    /// 对齐 pi 的 `getToolListError()`。
    #[test]
    fn tool_list_validation_matches_pi() {
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();

        // 纯名字或纯通配：合法
        assert_eq!(get_tool_list_error(&s(&[])), None);
        assert_eq!(get_tool_list_error(&s(&["read", "mcp__docs__*"])), None);
        // 纯修饰符：合法（含混合的 `+` 与 `-`）
        assert_eq!(get_tool_list_error(&s(&["+codemode", "-write"])), None);
        // 名字与修饰符混用：报错
        assert_eq!(
            get_tool_list_error(&s(&["read", "+bash"])),
            Some("tool names cannot be mixed with +name or -name entries".to_string())
        );
        // 修饰符带通配：报错并回显该条目
        assert_eq!(
            get_tool_list_error(&s(&["+mcp__docs__*"])),
            Some(
                "+name and -name entries take exact tool names, not patterns: +mcp__docs__*"
                    .to_string()
            )
        );

        // apply_tool_modifiers：按顺序追加/移除，非修饰符条目忽略
        assert_eq!(
            apply_tool_modifiers(&s(&["read"]), &s(&["+bash", "-read"])),
            s(&["bash"])
        );
        assert_eq!(
            apply_tool_modifiers(&s(&["read"]), &s(&["read", "+read*"])),
            s(&["read", "read*"])
        );
    }

    #[test]
    fn default_tools_project_modifiers_layer_on_global() {
        // 端到端：全局写纯工具名，项目层用 +/- 在其上增删（项目受信任才生效）
        // 项目目录走线程本地 CwdGuard（不再 set_current_dir）；仍持锁：
        // 会话信任与 settings 缓存是进程级共享态，与其它碰它们的测试串行
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);
            let _ = write_settings_value("defaultTools", json!(["read", "bash"]));

            let proj = std::env::temp_dir().join("prux-dt-modifiers");
            let proj_conf = proj.join(".prux");
            let _ = std::fs::remove_dir_all(&proj);
            std::fs::create_dir_all(&proj_conf).unwrap();
            std::fs::write(
                proj_conf.join("settings.json"),
                r#"{"defaultTools":["+web_search","-bash"]}"#,
            )
            .unwrap();
            crate::core::project_trust::set_session_trust(&proj, true);

            let cwd = crate::test_support::CwdGuard::set(proj.clone());
            let value = read_settings_default_tools();
            drop(cwd);
            assert_eq!(
                value,
                Some(vec!["read".to_string(), "web_search".to_string()])
            );

            crate::core::project_trust::clear_session_trust(&proj);
            let _ = std::fs::remove_dir_all(&proj);
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn default_tools_global_and_project_override() {
        // 对齐 pi：settings.defaultTools 控制初始内置工具；项目 .prux/settings.json 数组替换全局
        // 项目目录走线程本地 CwdGuard（不再 set_current_dir）；仍持锁：
        // 会话信任与 settings 缓存是进程级共享态，与其它碰它们的测试串行
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);

            // 未设置 → None（调用方回落标准默认 read/bash/edit/write）
            assert_eq!(read_settings_default_tools(), None);

            // 全局设置生效
            let _ = write_settings_value("defaultTools", json!(["read", "bash"]));
            assert_eq!(
                read_settings_default_tools(),
                Some(vec!["read".to_string(), "bash".to_string()])
            );

            // 项目 .prux/settings.json 覆盖全局（项目目录走线程本地 CwdGuard，用完还原）
            let proj = std::env::temp_dir().join("prux-dt-proj");
            let proj_conf = proj.join(".prux");
            let _ = std::fs::remove_dir_all(&proj);
            std::fs::create_dir_all(&proj_conf).unwrap();
            std::fs::write(
                proj_conf.join("settings.json"),
                r#"{"defaultTools":["edit","write"]}"#,
            )
            .unwrap();

            // 未信任：项目文件不得生效（回落到全局 defaultTools）
            let cwd_untrusted = crate::test_support::CwdGuard::set(proj.clone());
            let untrusted = read_settings_default_tools();
            drop(cwd_untrusted);
            assert_eq!(
                untrusted,
                Some(vec!["read".to_string(), "bash".to_string()]),
                "未受信任的项目 settings 必须被忽略"
            );

            // 受信任（这里用会话信任，等价于启动面板选 Trust）：项目层覆盖全局
            crate::core::project_trust::set_session_trust(&proj, true);
            let cwd_trusted = crate::test_support::CwdGuard::set(proj.clone());
            let project_value = read_settings_default_tools();
            drop(cwd_trusted);
            assert_eq!(
                project_value,
                Some(vec!["edit".to_string(), "write".to_string()])
            );

            // 项目空数组：**不**清空全局（空列表不含任何 `+`/`-` 条目，按「无修改」叠加）。
            // 对齐 pi mergeDefaultTools 的 `overrides.every(isToolModifier)` 空列表恒真行为
            std::fs::write(proj_conf.join("settings.json"), r#"{"defaultTools":[]}"#).unwrap();
            let cwd_empty = crate::test_support::CwdGuard::set(proj.clone());
            let empty_value = read_settings_default_tools();
            drop(cwd_empty);
            assert_eq!(
                empty_value,
                Some(vec!["read".to_string(), "bash".to_string()]),
                "项目空数组叠加在全局之上（不改变继承选择）"
            );

            // 全局空数组 → Some([])：无内置但扩展保留（对齐 pi defaultTools:[]）
            let _ = std::fs::remove_dir_all(&proj);
            crate::core::project_trust::clear_session_trust(&proj);
            let _ = write_settings_value("defaultTools", json!([]));
            assert_eq!(read_settings_default_tools(), Some(vec![]));

            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn theme_roundtrip_keeps_other_keys() {
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);

            assert!(read_settings().theme.is_none());

            // 写入后读回（对齐 pi settingsManager.setTheme）
            write_theme("synthwave").unwrap();
            let s = read_settings();
            assert_eq!(s.theme.as_deref(), Some("synthwave"));

            // read-modify-write：主题与 defaultModel 互不覆盖
            write_default_model_and_provider("deepseek", "deepseek-v4-pro").unwrap();
            let s = read_settings();
            assert_eq!(s.theme.as_deref(), Some("synthwave"));
            assert_eq!(s.default_model.as_deref(), Some("deepseek-v4-pro"));

            // 覆盖主题
            write_theme("dark").unwrap();
            assert_eq!(read_settings().theme.as_deref(), Some("dark"));
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn compaction_model_overrides_take_precedence() {
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);

            // 默认：内置默认值
            assert_eq!(read_settings_compact_reserve(), DEFAULT_RESERVE_TOKENS);
            assert_eq!(
                read_settings_compact_keep_recent(),
                DEFAULT_KEEP_RECENT_TOKENS
            );
            assert_eq!(
                read_settings_compact_reserve_for("anthropic", "claude-opus-4-8"),
                DEFAULT_RESERVE_TOKENS
            );

            // pi 兼容的嵌套全局键 + modelOverrides
            write_settings_value(
                "compaction",
                json!({
                    "reserveTokens": 1000,
                    "keepRecentTokens": 2000,
                    "modelOverrides": {
                        "anthropic/claude-opus-4-8": {
                            "reserveTokens": 111,
                            "keepRecentTokens": 222
                        },
                        "openai/gpt-5.5": { "reserveTokens": 333 }
                    }
                }),
            )
            .unwrap();

            assert_eq!(read_settings_compact_reserve(), 1000);
            assert_eq!(read_settings_compact_keep_recent(), 2000);

            // 覆盖命中
            assert_eq!(
                read_settings_compact_reserve_for("anthropic", "claude-opus-4-8"),
                111
            );
            assert_eq!(
                read_settings_compact_keep_recent_for("anthropic", "claude-opus-4-8"),
                222
            );
            // 部分字段覆盖：未覆盖的字段回落全局
            assert_eq!(read_settings_compact_reserve_for("openai", "gpt-5.5"), 333);
            assert_eq!(
                read_settings_compact_keep_recent_for("openai", "gpt-5.5"),
                2000
            );
            // 未覆盖的模型回落全局
            assert_eq!(read_settings_compact_reserve_for("deepseek", "x"), 1000);

            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn extension_mode_roundtrip_keeps_other_keys() {
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);

            // 默认 all
            assert_eq!(read_extension_mode(), "all");
            write_extension_mode("minimal").unwrap();
            assert_eq!(read_extension_mode(), "minimal");
            // 与其他键共存（read-modify-write）
            write_disabled_extensions(&["x".to_string()]).unwrap();
            assert_eq!(read_extension_mode(), "minimal");

            write_extension_mode("all").unwrap();
            assert_eq!(read_extension_mode(), "all");
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn new_selector_writers_roundtrip_and_keep_siblings() {
        // /settings 新增项：images.* / retry.* / temperature / compact* 读写独立，
        // 嵌套键只改自己那一个（images.autoResize 不得动 blockImages，反之亦然）
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);

            // 默认值
            assert!(read_settings_images_auto_resize());
            assert!(!read_settings_images_block_images());
            assert!(read_settings_retry_enabled());
            assert_eq!(read_settings_retry_max_retries(), 3);
            assert_eq!(read_settings_temperature(), None);
            assert_eq!(read_settings_compact_reserve(), DEFAULT_RESERVE_TOKENS);
            assert_eq!(
                read_settings_compact_keep_recent(),
                DEFAULT_KEEP_RECENT_TOKENS
            );

            write_settings_images_auto_resize(false).unwrap();
            write_settings_images_block_images(true).unwrap();
            write_settings_retry_enabled(false).unwrap();
            write_settings_retry_max_retries(10).unwrap();
            write_settings_temperature(Some(0.2)).unwrap();
            write_settings_compact_reserve(4096).unwrap();
            write_settings_compact_keep_recent(8000).unwrap();

            assert!(!read_settings_images_auto_resize());
            assert!(read_settings_images_block_images(), "同组兄弟键不得互踩");
            assert!(!read_settings_retry_enabled());
            assert_eq!(read_settings_retry_max_retries(), 10, "同组兄弟键不得互踩");
            assert_eq!(read_settings_temperature(), Some(0.2));
            assert_eq!(read_settings_compact_reserve(), 4096);
            assert_eq!(read_settings_compact_keep_recent(), 8000);

            // temperature: None 删键（回落模型自身采样参数）
            write_settings_temperature(None).unwrap();
            assert_eq!(read_settings_temperature(), None);
            assert!(
                settings_value().get("temperature").is_none(),
                "删键而不是写 null"
            );

            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn default_tool_toggle_writes_deltas_and_preserves_plain_lists() {
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);

            // 无键时：关掉基线工具（read）→ `-read`；开启非基线工具（grep）→ `+grep`
            toggle_settings_default_tool("read", true).unwrap();
            assert_eq!(settings_value()["defaultTools"], json!(["-read"]));
            assert!(
                !read_settings_default_tools()
                    .unwrap()
                    .contains(&"read".to_string())
            );
            toggle_settings_default_tool("grep", false).unwrap();
            assert_eq!(settings_value()["defaultTools"], json!(["-read", "+grep"]));
            assert!(
                read_settings_default_tools()
                    .unwrap()
                    .contains(&"grep".to_string())
            );

            // 反向切换：只删自己那条，不碰另一条
            toggle_settings_default_tool("read", false).unwrap();
            assert_eq!(settings_value()["defaultTools"], json!(["+grep"]));
            toggle_settings_default_tool("grep", true).unwrap();
            assert!(
                settings_value().get("defaultTools").is_none(),
                "条目清空后应删键"
            );

            // 手写纯工具名列表：增量叠加，不覆盖用户的选择
            std::fs::write(&path, r#"{"defaultTools":["read","bash"]}"#).unwrap();
            toggle_settings_default_tool("grep", false).unwrap();
            assert_eq!(
                settings_value()["defaultTools"],
                json!(["read", "bash", "+grep"])
            );
            toggle_settings_default_tool("read", true).unwrap();
            assert_eq!(
                settings_value()["defaultTools"],
                json!(["read", "bash", "+grep", "-read"])
            );
            let effective = read_settings_default_tools().unwrap();
            assert!(effective.contains(&"grep".to_string()));
            assert!(!effective.contains(&"read".to_string()));

            // 再开回 read：删掉 `-read`，回到纯列表
            toggle_settings_default_tool("read", false).unwrap();
            assert_eq!(
                settings_value()["defaultTools"],
                json!(["read", "bash", "+grep"])
            );

            // 显式空数组（不要任何内置工具）：增量语法表达不了，改写为纯工具名列表
            std::fs::write(&path, r#"{"defaultTools":[]}"#).unwrap();
            assert_eq!(read_settings_default_tools(), Some(Vec::new()));
            toggle_settings_default_tool("read", false).unwrap();
            assert_eq!(settings_value()["defaultTools"], json!(["read"]));
            assert_eq!(
                read_settings_default_tools(),
                Some(vec!["read".to_string()])
            );
            // 再关掉：纯列表 + `-read` → 有效集回到空
            toggle_settings_default_tool("read", true).unwrap();
            assert_eq!(settings_value()["defaultTools"], json!(["read", "-read"]));
            assert_eq!(read_settings_default_tools(), Some(Vec::new()));

            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn settings_selector_roundtrips_keep_other_keys() {
        // /settings 各项：autoCompact/steeringMode/followUpMode/hideThinkingBlock/
        // autocompleteMaxVisible/defaultProjectTrust/defaultThinkingLevel 读写互相独立
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);

            // 默认值
            assert!(read_settings_auto_compact());
            assert_eq!(read_settings_steering_mode(), "one-at-a-time");
            assert_eq!(read_settings_follow_up_mode(), "one-at-a-time");
            assert!(!read_settings_hide_thinking());
            assert_eq!(read_settings_autocomplete_max_visible(), 5);
            assert_eq!(
                read_settings_fullscreen_wheel_scroll_lines(),
                WheelScrollLines::Auto,
                "fullscreenWheelScrollLines 默认 auto"
            );
            assert!(read_settings_syntax_highlight(), "syntaxHighlight 默认启用");
            assert!(read_settings_mermaid(), "mermaid 默认启用");
            assert!(read_settings_latex(), "latex 默认启用");

            // 写回后读回
            write_settings_auto_compact(false).unwrap();
            write_settings_steering_mode("all").unwrap();
            write_settings_follow_up_mode("all").unwrap();
            write_settings_hide_thinking(true).unwrap();
            write_settings_autocomplete_max_visible(10).unwrap();
            write_settings_fullscreen_wheel_scroll_lines(WheelScrollLines::Lines(3)).unwrap();
            write_settings_syntax_highlight(false).unwrap();
            write_settings_mermaid(false).unwrap();
            write_settings_latex(false).unwrap();
            write_settings_default_project_trust("always").unwrap();
            write_settings_default_thinking_level("high").unwrap();
            assert!(!read_settings_auto_compact());
            assert_eq!(read_settings_steering_mode(), "all");
            assert_eq!(read_settings_follow_up_mode(), "all");
            assert!(read_settings_hide_thinking());
            assert_eq!(read_settings_autocomplete_max_visible(), 10);
            assert_eq!(
                read_settings_fullscreen_wheel_scroll_lines(),
                WheelScrollLines::Lines(3),
                "固定行数写盘后读回"
            );
            // 越界值写盘时钳制到 1..=100
            write_settings_fullscreen_wheel_scroll_lines(WheelScrollLines::Lines(0)).unwrap();
            assert_eq!(
                read_settings_fullscreen_wheel_scroll_lines(),
                WheelScrollLines::Lines(1)
            );
            write_settings_fullscreen_wheel_scroll_lines(WheelScrollLines::Auto).unwrap();
            assert_eq!(
                read_settings_fullscreen_wheel_scroll_lines(),
                WheelScrollLines::Auto
            );
            assert!(!read_settings_syntax_highlight());
            assert!(!read_settings_mermaid());
            assert!(!read_settings_latex());
            assert_eq!(
                read_settings().default_project_trust.as_deref(),
                Some("always")
            );
            assert_eq!(
                read_settings().default_thinking_level.as_deref(),
                Some("high")
            );

            // read-modify-write：不覆盖已有键，且互不干扰
            write_settings_auto_compact(true).unwrap();
            assert_eq!(read_settings_steering_mode(), "all");
            assert_eq!(read_settings_autocomplete_max_visible(), 10);
            assert!(!read_settings_syntax_highlight());
            let v = settings_value();
            assert_eq!(
                v.get("defaultProjectTrust").and_then(|x| x.as_str()),
                Some("always")
            );

            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn history_max_entries_distinguishes_zero_from_missing() {
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);

            // 键缺失 → 默认 500（不是 0！否则默认就不落盘了）
            assert_eq!(
                read_settings_history_max_entries(),
                HISTORY_MAX_ENTRIES_DEFAULT
            );

            // 键 = 0 → 0（不落盘），绝不能被当成非法值滤成默认值
            write_settings_history_max_entries(0).unwrap();
            assert_eq!(read_settings_history_max_entries(), 0);

            // 面板档位往返
            for tier in HISTORY_MAX_ENTRIES_TIERS {
                write_settings_history_max_entries(tier).unwrap();
                assert_eq!(read_settings_history_max_entries(), tier);
            }

            // 手改 JSON 的非档位值也按原样生效
            write_settings_history_max_entries(42).unwrap();
            assert_eq!(read_settings_history_max_entries(), 42);

            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn settings_read_tolerates_bom() {
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            std::fs::write(&path, "\u{FEFF}{\"steeringMode\":\"one-at-a-time\"}").unwrap();
            assert_eq!(settings_value()["steeringMode"], "one-at-a-time");
            assert!(
                settings_load_error().is_none(),
                "带 BOM 的合法 settings 不应报解析错误"
            );
            // 守卫需先剥离 BOM 再判定：带 BOM 的合法文件仍可写入
            write_settings_steering_mode("all").unwrap();
            assert_eq!(settings_value()["steeringMode"], "all");
        });
    }

    #[test]
    fn settings_load_error_reports_invalid_json() {
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            std::fs::write(&path, "{invalid").unwrap();
            let err = settings_load_error();
            assert!(err.is_some(), "无效 settings 应报告警告");
            assert!(err.unwrap().contains("Invalid settings file"));
        });
    }

    #[test]
    fn settings_cache_reflects_external_edits() {
        // 缓存以 (path, mtime, len) 为有效键：外部（非本模块 API）改写/删除文件后，
        // 下一次读取必须看到新值，而不是永远返回缓存里的旧快照。
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            std::fs::write(&path, r#"{"theme":"dark"}"#).unwrap();
            assert_eq!(read_settings().theme.as_deref(), Some("dark"));
            assert_eq!(read_extension_mode(), "all");

            // 外部改写（长度也不同）→ 缓存失效，读到新值
            std::fs::write(&path, r#"{"theme":"nord","extensionMode":"minimal"}"#).unwrap();
            assert_eq!(read_settings().theme.as_deref(), Some("nord"));
            assert_eq!(read_extension_mode(), "minimal");

            // 删除文件同样让缓存失效（元数据 → `(None, None)`），回落默认值
            let _ = std::fs::remove_file(&path);
            assert!(read_settings().theme.is_none());
            assert_eq!(read_extension_mode(), "all");
        });
    }

    #[test]
    fn invalid_settings_is_never_overwritten() {
        // 回归：settings.json 解析失败（如手工新增条目时引入语法错误）时，
        // 任何写操作都不得把它重置为只含本次改动的对象——否则旧配置全部丢失。
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let broken = r#"{"theme":"dark","autoCompact":false,"#;
            std::fs::write(&path, broken).unwrap();

            // 写入被拒绝，原文件保持不变
            let err = write_theme("light").unwrap_err();
            assert!(
                err.to_string().contains("not valid JSON"),
                "应报告文件无效: {}",
                err
            );
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                broken,
                "无效 settings 不得被覆盖"
            );

            // 非对象 JSON 同样拒绝覆盖
            std::fs::write(&path, "[]").unwrap();
            let err = write_settings_auto_compact(false).unwrap_err();
            assert!(
                err.to_string().contains("not a JSON object"),
                "应报告非对象: {}",
                err
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "[]");

            // 修复文件后写入恢复，且已有条目保留
            std::fs::write(&path, r#"{"theme":"dark","newEntry":123}"#).unwrap();
            write_theme("light").unwrap();
            let v = settings_value();
            assert_eq!(v["theme"], "light");
            assert_eq!(v["newEntry"], 123, "已有条目应保留");

            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn salvage_recovers_top_level_pairs_from_damaged_json() {
        // 损坏文本里破坏点之前的完整键值对全部救回；嵌套值（对象/数组/含 `,` `}` 的字符串）
        // 由 serde_json 整体解析，不进错位；*值* 里的 `":"` 不得被当成键。
        let m = salvage_settings_object(
            r#"{"a":1,"b":{"c":2,"d":[1,{"e":"}"}]},"url":"http://x?a=1:b",,"g":true"#,
        );
        assert_eq!(m["a"], 1);
        assert_eq!(m["b"]["c"], 2);
        assert_eq!(m["b"]["d"][1]["e"], "}");
        assert_eq!(m["url"], "http://x?a=1:b");
        assert_eq!(m["g"], true);
        assert_eq!(m.len(), 4, "不应把字符串内容误当成键: {m:?}");

        // 截断的尾声：完整键值对留下，残缺部分跳过
        let m = salvage_settings_object(r#"{"theme":"synthwave","extensionMode":"all","oops"#);
        assert_eq!(m["theme"], "synthwave");
        assert_eq!(m["extensionMode"], "all");
        assert_eq!(m.len(), 2);

        // 数组内（非顶层）的键不是设置项；完全不可解析时无收获
        assert!(salvage_settings_object(r#"[{"a":1}]"#).is_empty());
        assert!(salvage_settings_object("not json at all").is_empty());
        assert!(salvage_settings_object("{invalid").is_empty());
    }

    #[test]
    fn damaged_settings_without_snapshot_recovers_readable_keys() {
        // 文件损坏时：从损坏原文救回完整键值对，保存一项设置后这些键必须还在。
        // （无缓存层：不做「上次成功读取」快照，仅依据当前磁盘内容可解析的部分。）
        let _serial = crate::test_support::SETTINGS_OVERWRITE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            // 手改时多写了一个尾随逗号
            std::fs::write(
                &path,
                r#"{"theme":"synthwave","extensionMode":"all","defaultProvider":"opencode-go","defaultModel":"mimo-v2.5",}"#,
            )
            .unwrap();

            let s = read_settings();
            assert_eq!(s.theme.as_deref(), Some("synthwave"));
            assert_eq!(s.default_provider.as_deref(), Some("opencode-go"));
            assert_eq!(s.default_model.as_deref(), Some("mimo-v2.5"));
            assert_eq!(read_extension_mode(), "all");

            let _ui = crate::test_support::SettingsUiGuard::set(true);
            write_settings_auto_compact(false).unwrap();
            confirm_settings_overwrite().unwrap();

            let v = settings_value();
            assert_eq!(v["autoCompact"], false);
            assert_eq!(v["theme"], "synthwave");
            assert_eq!(v["extensionMode"], "all");
            assert_eq!(v["defaultProvider"], "opencode-go");
            assert_eq!(v["defaultModel"], "mimo-v2.5");

            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn pending_overwrite_is_dropped_once_file_is_writable() {
        // 暂存的覆写只在文件仍损坏时作基准：用户手工修好文件后，写盘以磁盘为准
        // （手工修复的键保留），陈旧暂存被丢弃，不会在确认时把刚写好的文件盖回去。
        let _serial = crate::test_support::SETTINGS_OVERWRITE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ui = crate::test_support::SettingsUiGuard::set(true);

            std::fs::write(&path, r#"{"theme":"dark","#).unwrap();
            write_theme("light").unwrap();
            assert!(pending_settings_overwrite().is_some(), "损坏时应暂存");

            // 用户手工修好文件（保留 theme 并新增一条）
            std::fs::write(&path, r#"{"theme":"nord","newEntry":7}"#).unwrap();
            write_settings_auto_compact(false).unwrap();

            let v = settings_value();
            assert_eq!(v["theme"], "nord", "手工修复的内容应优先于陈旧暂存");
            assert_eq!(v["newEntry"], 7, "手工新增的键不得丢失");
            assert_eq!(v["autoCompact"], false);
            assert!(
                pending_settings_overwrite().is_none(),
                "文件已可写入，陈旧暂存应被丢弃"
            );

            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn concurrent_writes_do_not_lose_updates() {
        // 回归：settings.json 的读-改-写必须串行。多个线程并发写不同键时，
        // 若无写锁，各线程基于同一旧快照写回，后写者覆盖先写者（丢更新）。
        // 用进程级 PRUX_AGENT_DIR（线程本地 override 传不到 spawn 的线程）+ AUTH_TEST_LOCK 串行。
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::test_support::pin_test_agent_dir();
        let path = agent_dir().join("settings.json");
        let _ = std::fs::remove_file(&path);

        let n: u64 = 16;
        std::thread::scope(|s| {
            for i in 0..n {
                s.spawn(move || {
                    for _ in 0..20 {
                        write_settings_value(&format!("k{i}"), json!(i)).unwrap();
                    }
                });
            }
        });

        let v = settings_value();
        let obj = v.as_object().expect("settings.json 应为对象");
        for i in 0..n {
            assert_eq!(
                obj.get(&format!("k{i}")).and_then(|x| x.as_u64()),
                Some(i),
                "并发写入丢失键 k{i}: {obj:?}"
            );
        }
        let _ = std::fs::remove_file(&path);
    }

    /// 回归：写盘必须是原子的（临时文件 + rename），读取方（不持写锁）绝不能
    /// 在写入过程中读到空/半截 settings.json。
    ///
    /// 背景：`std::fs::write` 是 truncate→write，若启动注册扩展（读
    /// disabledExtensions）恰逢另一进程/线程写盘，一次空读会让 footer 全部
    /// 按「启用」注册 → 回退默认底栏；缓存空值再被后续 read-modify-write
    /// 固化，就把用户设置彻底洗成空列表。修复前本测试能稳定复现（读计数数千）。
    #[test]
    fn readers_do_not_observe_truncated_settings_during_writes() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::test_support::pin_test_agent_dir();
        let path = agent_dir().join("settings.json");
        let _ = std::fs::remove_file(&path);
        write_disabled_extensions(&["footer(minimal)".to_string(), "footer(normal)".to_string()])
            .unwrap();

        let empty_reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let done = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::scope(|s| {
            {
                let empty_reads = empty_reads.clone();
                let stop = stop.clone();
                s.spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        if read_disabled_extensions().is_empty() {
                            empty_reads.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
            for _ in 0..8 {
                let done = done.clone();
                let stop = stop.clone();
                s.spawn(move || {
                    for _ in 0..300 {
                        write_theme("synthwave").unwrap();
                    }
                    if done.fetch_add(1, Ordering::Relaxed) + 1 == 8 {
                        stop.store(true, Ordering::Relaxed);
                    }
                });
            }
        });
        let empties = empty_reads.load(Ordering::Relaxed);
        assert_eq!(
            empties, 0,
            "并发写期间读到空 disabledExtensions 次数={empties}"
        );
        assert_eq!(
            read_disabled_extensions(),
            vec!["footer(minimal)".to_string(), "footer(normal)".to_string()]
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 回归：启动时 worker 线程持久化 defaultProvider/defaultModel（switch_model persist），
    /// 主线程同时注册扩展、读 disabledExtensions。读写锁必须保证读到的始终是完整配置，
    /// 且最终文件不得丢掉 disabledExtensions（旧实现下缓存空值被 RMW 固化 → footer 回退默认）。
    #[test]
    fn startup_read_write_concurrency_keeps_disabled_extensions() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::test_support::pin_test_agent_dir();
        let path = agent_dir().join("settings.json");
        let _ = std::fs::remove_file(&path);
        write_disabled_extensions(&["footer(minimal)".to_string(), "footer(normal)".to_string()])
            .unwrap();

        let start = std::sync::Barrier::new(2);
        let empty_reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::scope(|s| {
            {
                let start = &start;
                let empty_reads = empty_reads.clone();
                let stop = stop.clone();
                s.spawn(move || {
                    start.wait();
                    while !stop.load(Ordering::Relaxed) {
                        if read_disabled_extensions().is_empty() {
                            empty_reads.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
            {
                let start = &start;
                let stop = stop.clone();
                s.spawn(move || {
                    start.wait();
                    // 模拟启动阶段 worker 持久化所选模型（不触碰 disabledExtensions）
                    for i in 0..500 {
                        write_default_model_and_provider("deepseek", &format!("m{i}")).unwrap();
                    }
                    stop.store(true, Ordering::Relaxed);
                });
            }
        });

        let empties = empty_reads.load(Ordering::Relaxed);
        assert_eq!(
            empties, 0,
            "并发读期间读到空 disabledExtensions 次数={empties}"
        );
        assert_eq!(
            read_disabled_extensions(),
            vec!["footer(minimal)".to_string(), "footer(normal)".to_string()],
            "并发读写后 disabledExtensions 不得丢失"
        );
        // 再写一次：RMW 基准必须包含 disabledExtensions（旧实现会以空缓存为底把它洗掉）
        write_default_model_and_provider("opencode", "gpt-4o").unwrap();
        assert_eq!(
            read_disabled_extensions(),
            vec!["footer(minimal)".to_string(), "footer(normal)".to_string()]
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_settings_is_created_on_write() {
        // 守卫不能误拦截「文件不存在」的正常创建路径
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let _ = std::fs::remove_file(&path);
            write_theme("dark").unwrap();
            assert_eq!(read_settings().theme.as_deref(), Some("dark"));
            let _ = std::fs::remove_file(&path);
        });
    }

    #[test]
    fn interactive_ui_stages_overwrite_and_can_confirm_or_cancel() {
        // 交互式 TUI 接入时：不直接拒绝，也不覆写；改为暂存待确认，
        // 由弹窗选择「覆写」（confirm_settings_overwrite）或「取消」。
        let _serial = crate::test_support::SETTINGS_OVERWRITE_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        with_temp_agent_dir(|| {
            let path = agent_dir().join("settings.json");
            let broken = r#"{"theme":"dark","autoCompact":false,"#;
            let _ui = crate::test_support::SettingsUiGuard::set(true);

            // 写入被推迟（返回 Ok），原文件不变
            std::fs::write(&path, broken).unwrap();
            write_theme("light").unwrap();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
            let (shown_path, detail) = pending_settings_overwrite().expect("应暂存待确认覆写");
            assert_eq!(shown_path, path.display().to_string());
            assert!(
                detail.contains("not valid JSON"),
                "原因应含解析错误: {detail}"
            );

            // 同文件多次推迟改动合并（不丢失中间键）
            write_settings_auto_compact(false).unwrap();

            // 确认覆写：写出合并后的对象并清空暂存
            confirm_settings_overwrite().unwrap();
            let v = settings_value();
            assert_eq!(v["theme"], "light");
            assert_eq!(v["autoCompact"], false);
            assert!(pending_settings_overwrite().is_none());

            // 取消：保留损坏文件，不写任何内容
            std::fs::write(&path, broken).unwrap();
            write_theme("dark").unwrap();
            assert!(pending_settings_overwrite().is_some());
            cancel_settings_overwrite();
            assert_eq!(std::fs::read_to_string(&path).unwrap(), broken);
            assert!(pending_settings_overwrite().is_none());

            let _ = std::fs::remove_file(&path);
        });
    }

    /// `showImages`：缺失默认 false（不探测终端、不内联图片），写入后可读回。
    #[test]
    fn show_images_defaults_to_false_and_round_trips() {
        let _ad = crate::test_support::AgentDirGuard::temp();

        assert!(!read_settings_show_images(), "未设置时默认关闭");

        write_settings_show_images(true).unwrap();
        assert!(read_settings_show_images());
        assert_eq!(settings_value()["showImages"], json!(true));

        write_settings_show_images(false).unwrap();
        assert!(!read_settings_show_images());
        assert_eq!(settings_value()["showImages"], json!(false));
    }

    /// `quietStartup`：布尔与 `"header"` 两种写法都读得回来，未设置 / 非法值 === `"header"`。
    #[test]
    fn quiet_startup_reads_booleans_and_header_string() {
        with_temp_agent_dir(|| {
            let set = |value: Value| {
                modify_global_settings(|obj| {
                    if value.is_null() {
                        obj.remove("quietStartup");
                    } else {
                        obj.insert("quietStartup".to_string(), value.clone());
                    }
                })
                .unwrap();
            };

            // 未设置：保持 prux 既有默认（不打资源清单，但显示横幅）
            set(Value::Null);
            assert_eq!(read_settings_quiet_startup(), QuietStartup::Header);

            for (stored, expected) in [
                (json!(false), QuietStartup::Full),
                (json!(true), QuietStartup::Silent),
                (json!("header"), QuietStartup::Header),
                (json!("bogus"), QuietStartup::Header),
                (json!(3), QuietStartup::Header),
            ] {
                set(stored.clone());
                assert_eq!(read_settings_quiet_startup(), expected, "stored: {stored}");
            }

            // 写回：`false`/`true` 写布尔，`"header"` 写字符串（与 pi 字段类型一致）
            write_settings_quiet_startup(QuietStartup::Full).unwrap();
            assert_eq!(settings_value()["quietStartup"], json!(false));
            write_settings_quiet_startup(QuietStartup::Header).unwrap();
            assert_eq!(settings_value()["quietStartup"], json!("header"));
            write_settings_quiet_startup(QuietStartup::Silent).unwrap();
            assert_eq!(settings_value()["quietStartup"], json!(true));
        });
    }
}

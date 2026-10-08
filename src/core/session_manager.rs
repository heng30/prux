// 会话持久化：v4 JSONL record-log 格式

use crate::{
    core::{
        provider::{AgentMessage, Usage},
        session_v4::{
            self, ContextEditReplacement, EffectiveLaneConfiguration, Entry, EntryBase,
            ForkPosition, ForkScope, JsonlV4Header, LaneRecord, LaneReductionInput,
            LaneReductionResult, RecordBase, RecordLogError, SessionMutation, SessionState,
            entry_base, reduce_lane_state, set_record_base,
        },
        settings_manager::agent_dir,
    },
    error::{Error, Result},
    utils::{
        paths::{normalize_dir, read_first_line},
        time::{chrono_ms_to_iso, now_ms},
    },
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

/// 默认泳道名，会话的消息树与状态默认挂在这条 lane 上。
const MAIN_LANE: &str = "main";
/// 当前会话文件格式版本号，写入 header 供兼容性判断。
pub const SESSION_VERSION: u32 = 4;

/// 抢救上限：`min(32, 总 mutation 行数 * 5%)`，至少为 8。
/// 至少 8：同毫秒批量工具调用可能一次撞出多个重复 id，逐个跳过需要多个名额；
/// 上限 32：防止整文件损坏时疯狂跳过导致数据完整性崩溃。
const SALVAGE_CAP_MIN: usize = 8;
/// 抢救损坏会话时允许跳过的最大行数上限。
const SALVAGE_CAP_MAX: usize = 32;

/// 修复计划：`open_checked` 检测到会话损坏时产出，TUI 面板据此展示档位与两种修复路径。
/// 两种路径都提供可直接写回的文件内容（seq 在抢救路径中已重排，保持严格连续）。
#[derive(Debug, Clone)]
pub struct SessionRepairPlan {
    /// 损坏详情（含行号，格式与 open 报错一致）
    pub error: String,
    /// 文件总物理行数（含 header）
    pub total_lines: usize,
    /// 截断路径可保留的行数（含 header）
    pub valid_lines: usize,
    /// 截断路径写回内容：header + 首个损坏行之前的全部有效 mutation（未重排）
    pub truncate_content: String,
    /// 抢救路径写回内容：header + 跳过可救损坏行后的全部有效 mutation（seq 已重排）；
    /// None = 不可抢救（引用断裂/依赖冲突/无跳过），仅能截断
    pub salvage_content: Option<String>,
    /// 抢救路径跳过（丢弃）的损坏行数
    pub salvaged_skipped: usize,
}

impl SessionRepairPlan {
    /// 是否提供抢救选项（跳过损坏行、保留后缀）
    pub fn salvage_available(&self) -> bool {
        self.salvage_content.is_some()
    }

    /// 截断路径丢弃的行数（展示用）
    pub fn dropped_lines(&self) -> usize {
        self.total_lines.saturating_sub(self.valid_lines)
    }
}

/// 打开结果：正常 / 不可修复错误 / 中间行损坏（带修复计划）。
/// `Session` 体积大（约 700B），`Ok` 变体装箱避免栈上大值拷贝。
pub enum OpenSessionResult {
    /// 打开成功
    Ok(Box<Session>),
    /// 文件缺失、header 损坏等不可修复错误
    Invalid(Error),
    /// 中间行损坏：携带修复计划（截断/抢救），由调用方决定如何修复
    Repairable {
        /// 待修复的会话文件路径
        path: PathBuf,
        /// 具体的修复方案（截断或抢救内容）
        plan: SessionRepairPlan,
    },
}

impl From<OpenSessionResult> for Result<Session> {
    /// 把打开结果折叠成 `Result`：`Ok` 解箱、`Invalid` 原样透传错误，
    /// `Repairable` 折叠为携带修复计划 error 文本的 `Err`。
    fn from(ret: OpenSessionResult) -> Result<Session> {
        match ret {
            OpenSessionResult::Ok(s) => Ok(*s),
            OpenSessionResult::Invalid(e) => Err(e),
            OpenSessionResult::Repairable { plan, .. } => Err(Error::msg(plan.error)),
        }
    }
}

/// 继续最近会话的结果：区分正常打开 / 新建 / 可修复损坏 / 失败。
pub enum ContinueRecentResult {
    /// 打开了最近的会话
    Ok(Session),
    /// 无最近会话，新建
    Created(Session),
    /// 最近的会话中间行损坏，携带修复计划
    Repairable {
        /// 待修复的会话文件路径
        path: PathBuf,
        /// 具体的修复方案（截断或抢救内容）
        plan: SessionRepairPlan,
    },
    /// 不可修复损坏或创建失败
    Failed,
}

impl From<ContinueRecentResult> for Option<Session> {
    /// 只保留打开成功与新建两种结果；可修复损坏与失败都折叠为 `None`。
    fn from(ret: ContinueRecentResult) -> Option<Session> {
        match ret {
            ContinueRecentResult::Ok(s) | ContinueRecentResult::Created(s) => Some(s),
            ContinueRecentResult::Repairable { .. } | ContinueRecentResult::Failed => None,
        }
    }
}

/// 一条会话的内存句柄：id、目录、持久化开关与消息树状态，负责落盘读写。
#[derive(Debug, Clone)]
pub struct Session {
    /// 会话唯一 id（v7 UUID，与文件名、header 一致）
    pub session_id: String,
    /// 会话创建时的工作目录
    pub cwd: String,
    /// 存放会话文件的目录
    pub session_dir: PathBuf,
    /// 会话文件路径；persist 为 false 时为 None（仅内存）
    pub session_file: Option<PathBuf>,
    /// 是否将变更写入磁盘（false 时仅内存会话）
    pub persist: bool,
    /// 内存中的消息树与引用状态
    pub state: SessionState,
    /// 用户指定的会话名；未命名时为 None
    pub name: Option<String>,
    /// 最近一次持久化写盘失败的描述（经 TUI error 事件提示，避免静默丢失）
    persist_error: Option<String>,
    /// 会话文件头（id/cwd/创建时间/父会话 id 等）
    header: JsonlV4Header,
}

/// 抢救行数
fn salvage_cap_for(total_lines: usize) -> usize {
    let mutations = total_lines.saturating_sub(1);
    (mutations.saturating_mul(5) / 100).clamp(SALVAGE_CAP_MIN, SALVAGE_CAP_MAX)
}

/// 原子写回修复后的会话文件：先备份原文件到 `.jsonl.bak`，再 tmp+rename（避免写一半损坏）。
/// 备份失败**中止修复**（返回 Err，源文件保持原样）：截断/抢救会覆盖源文件，
/// 没有成功备份就动手等于让“删掉的后缀”永远无法找回。
fn write_repaired_file(file: &Path, content: &str) -> std::io::Result<()> {
    if file.exists() {
        let bak = file.with_extension("jsonl.bak");
        std::fs::copy(file, &bak).map_err(|source| {
            std::io::Error::new(
                source.kind(),
                format!(
                    "failed to back up {} to {} before repair: {}",
                    file.display(),
                    bak.display(),
                    source
                ),
            )
        })?;
    }

    let tmp = file.with_extension("jsonl.tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, file)
}

/// D 档判定：引用断裂类错误——被丢弃行/缺失的 state 导致后缀依赖缺失，跳过该行也救不回来，应就地截断。
/// seq 跳号**不属于** D 档：行内容可能完全自洽，只修编号即可整行保留（见 `scan_mutations`）。
fn is_d_marker_error(e: &RecordLogError) -> bool {
    matches!(
        e,
        RecordLogError::MissingLane { .. }
            | RecordLogError::MissingLaneTarget { .. }
            | RecordLogError::MissingParent { .. }
            | RecordLogError::DoesNotChainToLaneLeaf { .. }
            | RecordLogError::MissingLabelTarget { .. }
    )
}

/// 一行 JSON 里"本行产出、可被下游引用"的 id（entry/record 的自身 id）。
/// 解析失败/无 id 的行返回空集（跳过它不会产生悬空引用）。
fn produced_line_ids(line: &str) -> Vec<String> {
    serde_json::from_str::<Value>(line)
        .ok()
        .and_then(|v| {
            v.get("id")
                .and_then(|x| x.as_str())
                .map(|s| vec![s.to_string()])
        })
        .unwrap_or_default()
}

/// 一行 JSON 里引用的外部 id（parent/leaf/run/target 等），用于悬空检查。
fn referenced_line_ids(v: &Value) -> Vec<String> {
    /// JSON 行中承载外部引用的字段名，用于悬空引用检查。
    const REF_KEYS: &[&str] = &[
        "parentId",
        "leafId",
        "runId",
        "entryId",
        "assistantEntryId",
        "resultEntryId",
        "targetId",
        "sourceLeafId",
    ];
    let mut out = Vec::new();
    for key in REF_KEYS {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            out.push(s.to_string());
        }
    }
    if let Some(s) = v.pointer("/target/id").and_then(|x| x.as_str()) {
        out.push(s.to_string());
    }
    out
}

/// 依赖检查闸门：后缀中是否存在对 `dropped_ids`（且不在当前已应用状态中）的悬空引用。
/// 若存在，说明后缀依赖被丢弃行的内容 → 抢救会产出运行时损坏（reduce 阶段才暴露），应就地截断。
fn suffix_has_dangling_reference(
    suffix: &[&str],
    dropped_ids: &[String],
    state: &SessionState,
) -> bool {
    if dropped_ids.is_empty() {
        return false;
    }
    for line in suffix {
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue; // 本身损坏的行不参与检查（迭代中会单独处理）
        };

        for r in referenced_line_ids(&v) {
            if dropped_ids.iter().any(|d| d == &r) && !state.has_id(&r) {
                return true;
            }
        }
    }
    false
}

/// `open_checked` 阶段 2 的逐行归约结果：损坏档位、成功前缀与统计，供自动修复/组装使用。
struct MutationScan {
    /// 全部成功应用（必要时已重排/已修正 seq）的 mutation
    kept: Vec<SessionMutation>,
    /// 已跳过的损坏行数（上限/展示用）
    skipped: usize,
    /// 已就地修正 seq 的行数（内容保留，仅重写编号；自动写回条件）
    seq_fixed: usize,
    /// 首个损坏行之前的成功行数（截断路径）
    prefix_len: usize,
    /// 首个中间损坏行 (物理行号, 损坏详情)
    first_failure: Option<(usize, String)>,
    /// 最后一行非 JSON（torn tail，后面无内容可自动截断）
    torn_tail: Option<(usize, String)>,
    /// 归约出的状态（含恢复出的 name）
    state: SessionState,
    /// 文件总物理行数（含 header）
    total_lines: usize,
}

/// `open_checked` 阶段 1 的产物：会话全文 + 已校验的 header。
/// 任何不可修复问题（文件缺失 / 空文件 / header 非 v4 / header 字段损坏）都折叠为 `Err`。
fn read_session_file(path: &Path) -> Result<(String, JsonlV4Header)> {
    let content = std::fs::read_to_string(path).map_err(|source| Error::Io {
        context: format!("Failed to read session file: {}", path.display()),
        source,
    })?;

    let header_line = content.lines().next().ok_or_else(|| {
        Error::msg(format!(
            "session file is missing a header: {}",
            path.display()
        ))
    })?;

    let header_value: Value = serde_json::from_str(header_line).map_err(|source| Error::Json {
        context: format!("Failed to parse session header: {}", path.display()),
        source,
    })?;

    let kind = header_value
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let version = header_value
        .get("version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    if kind != "header" || version != u64::from(SESSION_VERSION) {
        return Err(Error::msg(format!(
            "session file is not a v4 record-log (kind=header, version={}): {}",
            SESSION_VERSION,
            path.display()
        )));
    }

    let header: JsonlV4Header =
        serde_json::from_value(header_value).map_err(|source| Error::Json {
            context: format!("Failed to parse session header: {}", path.display()),
            source,
        })?;
    Ok((content, header))
}

/// `open_checked` 阶段 2：逐行归约。中间行损坏按档位处理：
/// - seq 跳号（`NonConsecutiveSeq`）：行内容未必损坏（写路径漏行/上次修复残留），
///   先把该行 seq 修正为期望值重试 apply，成功即整行保留——不截断、不删除任何条目；
/// - 可救（重复/解析坏行且后缀无悬空引用）：跳过该行、后缀 seq 重排后继续；
/// - 不可救（引用断裂 / 超抢救上限 / 依赖冲突）：就地截断（保留此前全部成功行）。
///   最后一行 JSON 语法错误视为 torn tail（后面无内容，可自动截断）。
fn scan_mutations(lines: &[&str], path: &Path) -> MutationScan {
    let mut state = SessionState::new();
    let total_lines = lines.len();
    let cap = salvage_cap_for(total_lines);
    let mut kept: Vec<SessionMutation> = Vec::new(); // 已成功应用（必要时已重排/修正 seq）
    let mut skipped = 0usize; // 已跳过的损坏行数（上限/展示用）
    let mut seq_fixed = 0usize; // 已修正 seq 的行数（自动写回条件）
    let mut skipped_seq = 0usize; // 其中“消费了 seq”的跳过行数（重排用）
    let mut prefix_len = 0usize; // 首个损坏行之前的成功行数（截断路径）
    let mut first_failure: Option<(usize, String)> = None; // (物理行号, 损坏详情)
    let mut torn_tail: Option<(usize, String)> = None; // 最后一行非 JSON (物理行号, 详情)
    let mut physical = 1usize; // 当前物理行号（0 = header）

    for idx in 1..lines.len() {
        let trimmed = lines[idx].trim();
        if trimmed.is_empty() {
            physical += 1;
            continue;
        }
        let is_last = idx + 1 == lines.len();

        let mut m = match session_v4::parse_mutation(trimmed) {
            Ok(m) => m,
            Err(e) => {
                if is_last && matches!(e, RecordLogError::InvalidJson { .. }) {
                    torn_tail = Some((
                        physical,
                        format!(
                            "Invalid session mutation at line {} ({}): {}",
                            physical + 1,
                            path.display(),
                            e
                        ),
                    ));
                    break;
                }

                // 解析坏行：记录为首个损坏，未超上限且过依赖闸门即可跳过
                if first_failure.is_none() {
                    prefix_len = kept.len();
                    first_failure = Some((
                        physical,
                        format!(
                            "Invalid session mutation at line {} ({}): {}",
                            physical + 1,
                            path.display(),
                            e
                        ),
                    ));
                }

                if skipped >= cap {
                    break;
                }

                if suffix_has_dangling_reference(
                    &lines[idx + 1..],
                    &produced_line_ids(trimmed),
                    &state,
                ) {
                    break;
                }

                skipped += 1;
                physical += 1;
                continue;
            }
        };

        // 跳过行后的 seq 重排：保持全局严格连续不变量。
        // 只对"消费了 seq"的跳过行（apply 阶段失败的重复/协议坏行）生效；
        // 纯解析坏行（插入的垃圾行）从未占过 seq，重排会错位，不能计入。
        if skipped_seq > 0
            && session_v4::shift_mutation_seq(&mut m, -(skipped_seq as i64)).is_none()
        {
            // seq 下溢 = 行序本身非递增，视为真实缺口（D 档）
            if first_failure.is_none() {
                prefix_len = kept.len();
                first_failure = Some((
                    physical,
                    format!(
                        "Invalid session mutation at line {} ({}): has non-consecutive seq",
                        physical + 1,
                        path.display()
                    ),
                ));
            }
            break;
        }

        match state.apply_mutation(m.clone()) {
            Ok(()) => {
                kept.push(m);
            }
            Err(mut e) => {
                // seq 跳号 ≠ 内容损坏：写路径丢行（persist 写失败被忽略）/ 上次修复残留
                // 都会让某行 seq 与期望不符，但该行引用链仍自洽。把 seq 修正为期望值后
                // 重试 apply，成功即整行保留——绝不因 seq 缺口删除后续任何条目。
                if matches!(e, RecordLogError::NonConsecutiveSeq { .. }) {
                    session_v4::set_mutation_seq(&mut m, state.next_sequence());
                    match state.apply_mutation(m.clone()) {
                        Ok(()) => {
                            seq_fixed += 1;
                            kept.push(m);
                            physical += 1;
                            continue;
                        }
                        Err(e2) => {
                            // seq 修正后仍失败 = 行内容本身损坏（引用断裂/重复 id），按真实错误继续走档位（截断/跳过）。
                            e = e2;
                        }
                    }
                }

                let msg = e.to_string();
                if first_failure.is_none() {
                    prefix_len = kept.len();
                    first_failure = Some((
                        physical,
                        format!(
                            "Invalid session mutation at line {} ({}): {}",
                            physical + 1,
                            path.display(),
                            msg
                        ),
                    ));
                }

                if is_d_marker_error(&e) {
                    break; // D 档：引用断裂，后缀依赖缺失 state，就地截断
                }

                if skipped >= cap {
                    break; // 抢救上限：不再跳过，就地截断
                }

                // 依赖检查闸门：跳过本行前确认后缀不会悬空引用本行产出的 id
                if suffix_has_dangling_reference(
                    &lines[idx + 1..],
                    &produced_line_ids(trimmed),
                    &state,
                ) {
                    break;
                }

                skipped += 1;
                skipped_seq += 1; // 该行通过了 seq 检查（消费过 seq），后续行需重排
            }
        }
        physical += 1;
    }

    MutationScan {
        kept,
        skipped,
        seq_fixed,
        prefix_len,
        first_failure,
        torn_tail,
        state,
        total_lines,
    }
}

/// header 行 + `kept[..len]` 编码为写回内容（encode_mutation 自带换行）。
fn encode_prefix(header_line: &str, kept: &[SessionMutation], len: usize) -> String {
    let mut out = String::from(header_line);
    out.push('\n');
    for m in &kept[..len] {
        out.push_str(&session_v4::encode_mutation(m));
    }
    out
}

/// `open_checked` 阶段 3：torn tail 自动修复写回，再据剩余损坏组装 `Repairable` / `Ok`。
fn finalize_open(
    file: PathBuf,
    header: JsonlV4Header,
    header_line: &str,
    content: &str,
    scan: MutationScan,
) -> OpenSessionResult {
    let MutationScan {
        kept,
        skipped,
        seq_fixed,
        prefix_len,
        first_failure,
        torn_tail,
        state,
        total_lines,
    } = scan;

    // 自动修复写回（两种都无需用户确认，且都不删除任何完整行）：
    // - torn tail（未跳过损坏行）：丢弃最后一行不完整 JSON，写回此前全部成功 mutation；
    // - 纯 seq 跳号修复（无其它损坏）：整文件重写，持久化修好的 seq。
    if (torn_tail.is_some() && skipped == 0)
        || (torn_tail.is_none() && first_failure.is_none() && seq_fixed > 0)
    {
        _ = write_repaired_file(&file, &encode_prefix(header_line, &kept, kept.len()));
    } else if torn_tail.is_none() && first_failure.is_none() && !content.ends_with('\n') {
        // 干净文件但缺末尾换行：补写换行
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&file) {
            _ = f.write_all(b"\n");
        }
    }

    // 存在未自动处理的损坏 → 返回修复计划，由调用方（TUI 面板）决定写回方式
    let plan_error = if torn_tail.is_some() && skipped == 0 {
        None // torn tail 且无跳行：已自动截断写回
    } else {
        first_failure
            .as_ref()
            .map(|(_, e)| e.clone())
            .or_else(|| torn_tail.as_ref().map(|(_, e)| e.clone()))
    };

    if let Some(err) = plan_error {
        let truncate_content = encode_prefix(header_line, &kept, prefix_len);
        let salvage_content = if skipped > 0 {
            Some(encode_prefix(header_line, &kept, kept.len()))
        } else {
            None
        };

        return OpenSessionResult::Repairable {
            path: file,
            plan: SessionRepairPlan {
                error: err,
                total_lines,
                valid_lines: truncate_content.lines().count(),
                truncate_content,
                salvage_content,
                salvaged_skipped: skipped,
            },
        };
    }

    OpenSessionResult::Ok(Box::new(Session {
        session_id: header.id.clone(),
        cwd: header.cwd.clone(),
        session_dir: file.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
        session_file: Some(file),
        persist: true,
        name: state.get_name().map(|s| s.to_string()),
        state,
        persist_error: None,
        header,
    }))
}

/// 会话构造参数（[`Session::create_with`] 的唯一入口形态）。
///
/// `#[derive(Default)]` 是刻意的：未来新增字段（如 worktree 场景的额外元数据）
/// 可用 `..Default::default()` 加性扩展，不必改动既有调用点。
#[derive(Debug, Clone, Default)]
pub struct SessionCreateOptions {
    /// 会话工作目录
    pub cwd: String,
    /// 会话目录；None = `default_session_dir(cwd, agent_dir())`
    pub session_dir: Option<PathBuf>,
    /// 是否落盘
    pub persist: bool,
    /// 指定会话 id；None = 生成新的 v7 UUID
    pub session_id: Option<String>,
    /// 父会话 id（fork / 子代理会话场景）；None = header 不写 `parentSessionId`
    pub parent_session_id: Option<String>,
}

impl Session {
    /// 创建新会话
    pub fn create(cwd: &str, session_dir: Option<PathBuf>, persist: bool) -> Result<Session> {
        Self::create_with(SessionCreateOptions {
            cwd: cwd.to_string(),
            session_dir,
            persist,
            ..Default::default()
        })
    }

    /// 按指定 ID 创建会话（--session-id：不存在时创建）
    pub fn create_with_id(
        cwd: &str,
        session_dir: Option<PathBuf>,
        persist: bool,
        session_id: &str,
    ) -> Result<Session> {
        Self::create_with(SessionCreateOptions {
            cwd: cwd.to_string(),
            session_dir,
            persist,
            session_id: Some(session_id.to_string()),
            ..Default::default()
        })
    }

    /// 唯一构造入口：按 [`SessionCreateOptions`] 创建会话。
    ///
    /// `parent_session_id` 与 `session_id` 都会写进 session 文件头（落盘时），
    /// 前者用于 `/resume` 选择器把子代理会话嵌套显示在父会话之下。
    pub fn create_with(opts: SessionCreateOptions) -> Result<Session> {
        let cwd = opts.cwd.as_str();
        let session_id = match &opts.session_id {
            Some(id) => {
                if !is_valid_session_id(id) {
                    return Err(Error::Message(format!(
                        "Invalid session id: {}. Session id must be a valid v7 UUID (e.g. 0193f1b2-...-...-...-...)",
                        id
                    )));
                }
                id.clone()
            }
            None => new_session_id(),
        };
        let persist = opts.persist;
        let dir = opts
            .session_dir
            .unwrap_or_else(|| default_session_dir(cwd, &agent_dir()));
        let created_at = now_ms() as i64;
        let header = JsonlV4Header::new(
            session_id.clone(),
            cwd.to_string(),
            created_at,
            opts.parent_session_id.clone(),
        );
        let mut session = Session {
            session_id: session_id.clone(),
            cwd: cwd.to_string(),
            session_dir: dir,
            session_file: None,
            persist,
            state: SessionState::new(),
            name: None,
            persist_error: None,
            header: header.clone(),
        };
        if persist {
            std::fs::create_dir_all(&session.session_dir).map_err(|source| Error::Io {
                context: format!(
                    "Failed to create session directory: {}",
                    session.session_dir.display()
                ),
                source,
            })?;
            let file = session
                .session_dir
                .join(session_file_name(created_at, &session_id));
            std::fs::write(&file, header.encode()).map_err(|source| Error::Io {
                context: format!("Failed to write session file: {}", file.display()),
                source,
            })?;
            session.session_file = Some(file);
        }
        Ok(session)
    }

    /// 父会话 id（fork / 子代理会话；`/resume` 嵌套显示依赖它）。
    pub fn parent_session_id(&self) -> Option<&str> {
        self.header.parent_session_id.as_deref()
    }

    /// 是否持久化模式
    pub fn is_persisted(&self) -> bool {
        self.persist
    }

    /// 是否已落盘（session_file 存在）
    pub fn has_durable_file(&self) -> bool {
        self.session_file.is_some()
    }

    /// 会话文件路径；仅内存会话（persist = false）时为 `None`。
    pub fn get_session_file(&self) -> Option<&Path> {
        self.session_file.as_deref()
    }

    /// 打开已有会话文件（header + 逐行 mutation 严格 apply）。
    ///
    /// 中间行损坏（seq 不连续 / parent 缺失 / 非法 JSON）时返回
    /// [`OpenSessionResult::Repairable`]，由调用方决定是否截断修复；
    /// 旧签名行为不变（可修复损坏同样折叠为 Err）。
    pub fn open(path: &str) -> Result<Session> {
        Self::open_checked(path).into()
    }

    /// 打开会话，中间行损坏时返回可修复信息而不是直接报错。
    ///
    /// 校验策略：逐行 apply，前面全部成功的行构成自洽前缀，
    /// 所以**任何**中间行损坏都可以用"截断到最后一个成功行"修复；
    /// 最后一行 JSON 语法错误仍按 torn tail 自动截断（不做确认）。
    pub fn open_checked(path: &str) -> OpenSessionResult {
        let file = PathBuf::from(path);

        // 阶段 1：读取文件并校验 header（不可修复错误 → Invalid）
        let (content, header) = match read_session_file(&file) {
            Ok(parsed) => parsed,
            Err(e) => return OpenSessionResult::Invalid(e),
        };
        let lines: Vec<&str> = content.lines().collect();
        let header_line = lines[0]; // read_session_file 已保证首行存在

        // 阶段 2：逐行归约，中间行损坏按档位跳过 / 截断
        let scan = scan_mutations(&lines, &file);

        // 阶段 3：torn tail 自动修复 + 组装返回值
        finalize_open(file, header, header_line, &content, scan)
    }

    /// 按修复计划写回损坏会话：`use_salvage=true` 走抢救路径（跳过损坏行、后缀 seq 已重排），
    /// 否则截断路径（只留首个损坏行之前的有效前缀）。写回前先备份原文件到 `.jsonl.bak`。
    pub fn repair_write(path: &str, plan: &SessionRepairPlan, use_salvage: bool) -> Result<()> {
        let content = if use_salvage {
            plan.salvage_content.clone().ok_or_else(|| {
                Error::msg("session salvage unavailable (broken suffix cannot be recovered)")
            })?
        } else {
            plan.truncate_content.clone()
        };
        let file = PathBuf::from(path);
        write_repaired_file(&file, &content).map_err(|source| Error::Io {
            context: format!("Failed to write repaired session file: {}", file.display()),
            source,
        })
    }

    /// 继续最近的会话；无则创建新会话（最近会话无用户消息视为空会话：删除并继续找下一个非空会话）
    pub fn continue_recent(
        cwd: &str,
        session_dir: Option<PathBuf>,
        persist: bool,
    ) -> Result<Session> {
        match Self::continue_recent_checked(cwd, session_dir, persist) {
            ContinueRecentResult::Ok(s) | ContinueRecentResult::Created(s) => Ok(s),
            ContinueRecentResult::Repairable { plan, .. } => Err(Error::msg(plan.error)),
            ContinueRecentResult::Failed => Err(Error::msg("Failed to continue recent session")),
        }
    }

    /// 继续最近会话（checked 版）：最近的会话为空（无用户消息）时删除并向后找下一个；
    /// 中间行损坏时返回可修复信息，供调用方（启动路径）stage 进 TUI 弹确认面板。
    ///
    /// 候选按修改时间新→旧（`list_sessions`）逐个检查，**先读首行 header**：
    /// - 显式 `--session-dir`（且不是本 cwd 的默认目录）时按 header `cwd` 过滤，命中即停，不再对其它项目的会话做整文件解析
    /// - 只有 cwd 匹配的候选才做完整打开（`has_user_messages` 判定需要全部条目）。
    pub fn continue_recent_checked(
        cwd: &str,
        session_dir: Option<PathBuf>,
        persist: bool,
    ) -> ContinueRecentResult {
        let default_dir = default_session_dir(cwd, &agent_dir());
        // 显式 session-dir 且不同于本 cwd 默认目录时，需要按 header cwd 过滤
        let filter_cwd = session_dir
            .as_ref()
            .is_some_and(|d| normalize_dir(d) != normalize_dir(&default_dir));
        let dir = session_dir.unwrap_or(default_dir);

        // list_sessions 按创建/修改时间新→旧排序
        for path in list_sessions(&dir) {
            if filter_cwd && read_header_cwd(&path).as_deref() != Some(cwd) {
                continue;
            }

            match Session::open_checked(&path.to_string_lossy()) {
                OpenSessionResult::Ok(s) => {
                    if s.has_user_messages() {
                        return ContinueRecentResult::Ok(*s);
                    }

                    // 空会话：删除并继续找下一个有用户消息的会话
                    _ = std::fs::remove_file(&path);
                }
                OpenSessionResult::Invalid(_) => return ContinueRecentResult::Failed,
                OpenSessionResult::Repairable { path, plan } => {
                    return ContinueRecentResult::Repairable { path, plan };
                }
            }
        }
        match Session::create(cwd, Some(dir), persist) {
            Ok(s) => ContinueRecentResult::Created(s),
            Err(_) => ContinueRecentResult::Failed,
        }
    }

    /// 会话是否包含至少一条用户消息（空会话判断）
    pub fn has_user_messages(&self) -> bool {
        self.state.entries().iter().any(|e| {
            matches!(
                e,
                session_v4::Entry::Message { message, .. } if message.role == "user"
            )
        })
    }

    /// 从已有会话 fork 出新会话：scope=tree
    pub fn fork_from(
        source_path: &str,
        cwd: &str,
        session_dir: Option<PathBuf>,
    ) -> Result<Session> {
        let source = Session::open(source_path)?;
        Self::fork_impl(&source, cwd, session_dir, ForkScope::Tree)
    }

    /// 从 source 复制指定 leaf 的活动分支
    pub fn fork_at(
        source_path: &str,
        leaf_id: &str,
        cwd: &str,
        session_dir: Option<PathBuf>,
    ) -> Result<Session> {
        let source = Session::open(source_path)?;
        Self::fork_impl(
            &source,
            cwd,
            session_dir,
            ForkScope::Branch {
                entry_id: Some(leaf_id.to_string()),
                position: Some(ForkPosition::At),
            },
        )
    }

    /// fork 的公共实现：按 `scope` 从 `source` 取出派生 mutation，
    /// 新建会话文件并重建内存 state；建目录/写盘失败返回 `Err`。
    fn fork_impl(
        source: &Session,
        cwd: &str,
        session_dir: Option<PathBuf>,
        scope: ForkScope,
    ) -> Result<Session> {
        let dir = session_dir.unwrap_or_else(|| default_session_dir(cwd, &agent_dir()));
        std::fs::create_dir_all(&dir).map_err(|source| Error::Io {
            context: format!("Failed to create session directory: {}", dir.display()),
            source,
        })?;
        let session_id = new_session_id();
        let created_at = now_ms() as i64;
        let header = JsonlV4Header::new(
            session_id.clone(),
            cwd.to_string(),
            created_at,
            Some(source.session_id.clone()),
        );
        let mutations = source.state.create_fork_mutations(&scope)?;

        let mut content = header.encode();
        for m in &mutations {
            content.push_str(&session_v4::encode_mutation(m));
        }

        let file = dir.join(session_file_name(created_at, &session_id));
        std::fs::write(&file, content).map_err(|source| Error::Io {
            context: format!("Failed to write session file: {}", file.display()),
            source,
        })?;

        let mut state = SessionState::new();
        for m in mutations {
            state.apply_mutation(m)?;
        }

        Ok(Session {
            session_id,
            cwd: cwd.to_string(),
            session_dir: dir,
            session_file: Some(file),
            persist: true,
            name: state.get_name().map(|s| s.to_string()),
            state,
            persist_error: None,
            header,
        })
    }

    /// 把条目补齐 seq/时间戳/parent（挂到 main lane 当前叶子）后持久化并同步内存，
    /// 返回条目 id；同时刷新缓存会话名。
    fn commit_entry_lane(&mut self, entry: Entry) -> String {
        let id = entry.id().to_string();
        let seq = self.state.next_sequence();
        let timestamp = now_ms() as i64;
        let mut entry = entry;
        {
            let b = match &mut entry {
                Entry::Message { base, .. }
                | Entry::ModelChange { base, .. }
                | Entry::ThinkingLevelChange { base, .. }
                | Entry::ActiveToolsChange { base, .. }
                | Entry::Compaction { base, .. }
                | Entry::BranchSummary { base, .. }
                | Entry::ContextEdit { base, .. }
                | Entry::Custom { base, .. } => base,
            };
            b.seq = seq;
            b.timestamp = timestamp;
            b.parent_id = self.state.require_lane(MAIN_LANE).ok().flatten();
        }
        let mutation = SessionMutation::Entry {
            lane: Some(MAIN_LANE.to_string()),
            entry,
        };
        self.persist_mutation(&mutation);
        _ = self.state.apply_mutation(mutation);
        self.name = self.state.get_name().map(|s| s.to_string());
        id
    }

    /// 把 record 补齐 id/seq/lane/时间戳后持久化并同步内存，返回 record id。
    fn commit_record(&mut self, mut record: LaneRecord) -> String {
        let id = record.id().to_string();
        let seq = self.state.next_sequence();
        let timestamp = now_ms() as i64;
        set_record_base(
            &mut record,
            RecordBase {
                id: id.clone(),
                seq,
                lane: MAIN_LANE.to_string(),
                timestamp,
            },
        );
        let mutation = SessionMutation::Record { record };
        self.persist_mutation(&mutation);
        _ = self.state.apply_mutation(mutation);
        id
    }

    /// 持久化一条 mutation；写盘失败不 panic 也不阻塞运行，
    /// 只把错误记进 `persist_error` 供收尾时经 TUI 提示。
    fn persist_mutation(&mut self, mutation: &SessionMutation) {
        if let Err(e) = self.try_persist_mutation(mutation) {
            // 写盘失败不 panic、不阻塞运行（正在进行的 AI 会话比落盘重要），
            // 但绝不能静默：记录错误，由 run 收尾处经 TUI error 事件提示用户。
            // 否则磁盘日志缺行，内存投影与磁盘分叉（open op 残留的另一个来源）。
            self.persist_error = Some(format!("failed to persist session update: {e}"));
        }
    }

    /// 以追加模式把 mutation 编码成一行写入会话文件；非持久化会话直接返回 `Ok`，
    /// IO 错误原样上抛。
    fn try_persist_mutation(&mut self, mutation: &SessionMutation) -> std::io::Result<()> {
        if self.persist
            && let Some(file) = &self.session_file
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(file)?;
            let line = session_v4::encode_mutation(mutation);
            f.write_all(line.as_bytes())?;
        }
        Ok(())
    }

    /// 取走最近一次持久化写盘失败（消费即清除）。
    /// Agent 在 run 收尾时检查并转成 TUI error 事件。
    pub fn take_persist_error(&mut self) -> Option<String> {
        self.persist_error.take()
    }

    /// 当前 lane 上处于打开状态的 operation 数（含崩溃残留的僵尸）。
    /// TUI 打开会话时用它判断是否弹"未完成操作"提示面板（>= 2 才弹）。
    pub fn open_operations_count(&self) -> usize {
        self.state.find_open_operations(MAIN_LANE, None).len()
    }

    /// 压平崩溃残留的僵尸 operation：为除最新（seq 最大）外的所有未关闭操作
    /// 追加一条 `operation_finished(outcome="interrupted")` 记录（持久化 + 同步内存），
    /// 使磁盘日志在下次恢复时只剩一个打开的 operation。返回压平个数。
    /// 仅由用户在 TUI 面板**显式选择**后调用（打开路径不隐式写盘）。
    pub fn squash_stale_operations(&mut self) -> usize {
        let ops = self.state.find_open_operations(MAIN_LANE, None);
        if ops.len() <= 1 {
            return 0;
        }

        let newest_seq = ops.iter().map(|r| r.seq()).max().unwrap_or(0);
        let mut squashed = 0usize;

        for op in ops.iter().filter(|r| r.seq() < newest_seq) {
            let run_id = op.id().to_string();
            self.finish_operation(&run_id);
            squashed += 1;
        }
        squashed
    }

    /// 当前 lane 上未关闭 operation 的 id（seq 降序，最新在前）。
    pub fn open_operation_ids(&self) -> Vec<String> {
        self.state
            .find_open_operations(MAIN_LANE, None)
            .iter()
            .map(|r| r.id().to_string())
            .collect()
    }

    /// 为「轮次开始后才打开、且此刻仍未关闭」的 operation 补写
    /// `operation_finished(outcome="interrupted")`，返回补写个数。
    ///
    /// 用于回合被**硬中止**的场景（TUI Esc/Ctrl+C 直接 drop prompt future）：
    /// `run_loop` 的 operation_finished 随 future 一起被丢弃，不补写则磁盘残留
    /// 未闭合的 operation，每次中断累积一个，重开会话即成僵尸条目。
    /// `before` 是轮次开始前已打开的 operation id 快照：继承来的僵尸不在补写范围内。
    pub fn interrupt_operations_opened_since(&mut self, before: &HashSet<String>) -> usize {
        let opened: Vec<String> = self
            .open_operation_ids()
            .into_iter()
            .filter(|id| !before.contains(id))
            .collect();
        for id in &opened {
            self.finish_operation(id);
        }
        opened.len()
    }

    /// 为当前所有未关闭 operation 补写 `operation_finished(outcome="interrupted")`，
    /// 返回补写个数。用于进程退出收尾：进行中的回合不会再有机会写 finish。
    pub fn interrupt_all_open_operations(&mut self) -> usize {
        let opened = self.open_operation_ids();
        for id in &opened {
            self.finish_operation(id);
        }
        opened.len()
    }

    /// 为一个仍处于打开状态的 operation 补写 `operation_finished(outcome="interrupted")`。
    /// 已关闭（含找不到）时不做任何事。
    fn finish_operation(&mut self, run_id: &str) {
        if !self.open_operation_ids().iter().any(|id| id == run_id) {
            return;
        }

        self.commit_record(LaneRecord::OperationFinished {
            base: RecordBase {
                id: new_record_id("op-fin"),
                seq: 0,
                lane: MAIN_LANE.to_string(),
                timestamp: now_ms() as i64,
            },
            run_id: run_id.to_string(),
            outcome: "interrupted".to_string(),
            error: None,
        });
    }

    /// 提交一条 Fact mutation：非 Fact 直接返回；Fact 补 seq 后持久化并同步内存，
    /// 再刷新缓存会话名。
    fn commit_fact(&mut self, mut m: SessionMutation) {
        if let SessionMutation::Fact { seq, .. } = &mut m {
            *seq = self.state.next_sequence();
        } else {
            return;
        }

        self.persist_mutation(&m);
        _ = self.state.apply_mutation(m);
        self.name = self.state.get_name().map(|s| s.to_string());
    }

    /// 追加消息
    pub fn append_message(&mut self, message: &AgentMessage) -> String {
        self.commit_entry_lane(Entry::Message {
            base: EntryBase {
                id: new_session_id(),
                seq: 0,
                timestamp: 0,
                parent_id: None,
            },
            message: message.clone(),
            terminate: None,
        })
    }

    /// 追加 custom 条目
    pub fn append_custom_entry(&mut self, custom_type: &str, data: Option<Value>) -> String {
        self.commit_entry_lane(Entry::Custom {
            base: EntryBase {
                id: new_session_id(),
                seq: 0,
                timestamp: 0,
                parent_id: None,
            },
            custom_type: custom_type.to_string(),
            data,
        })
    }

    /// 追加一条模型切换条目（记录 provider 与 model id），返回条目 id。
    pub fn append_model_change(&mut self, provider: &str, model_id: &str) -> String {
        self.commit_entry_lane(Entry::ModelChange {
            base: EntryBase {
                id: new_session_id(),
                seq: 0,
                timestamp: 0,
                parent_id: None,
            },
            provider: provider.to_string(),
            model_id: model_id.to_string(),
        })
    }

    /// 追加一条推理级别切换条目，返回条目 id。
    pub fn append_thinking_level_change(&mut self, thinking_level: &str) -> String {
        self.commit_entry_lane(Entry::ThinkingLevelChange {
            base: EntryBase {
                id: new_session_id(),
                seq: 0,
                timestamp: 0,
                parent_id: None,
            },
            thinking_level: thinking_level.to_string(),
        })
    }

    /// 追加一条活动工具集变更条目，返回条目 id。
    pub fn append_active_tools_change(&mut self, tools: &[String]) -> String {
        self.commit_entry_lane(Entry::ActiveToolsChange {
            base: EntryBase {
                id: new_session_id(),
                seq: 0,
                timestamp: 0,
                parent_id: None,
            },
            active_tool_names: tools.to_vec(),
        })
    }

    /// 追加一条 append-only 的**上下文编辑**
    ///
    /// - `replacement = None`：在**模型上下文**中省略 `target_id`（不改写历史）；
    /// - `replacement = Some(_)`：只替换其内容。
    ///
    /// 这里做三项校验（违反即 `Err`，不写盘）：
    /// 1. 目标条目存在；
    /// 2. 目标在当前活动分支上（跨分支编辑无意义，只会在导航后静默失效）；
    /// 3. 目标是可编辑的模型内容条目（user / assistant / toolResult 消息）。
    pub fn append_context_edit(
        &mut self,
        target_id: &str,
        replacement: Option<ContextEditReplacement>,
    ) -> Result<String> {
        let Some(target) = self
            .state
            .get_entries()
            .iter()
            .find(|e| e.id() == target_id)
        else {
            return Err(Error::EntryNotFound(target_id.to_string()));
        };

        let on_branch = self
            .state
            .require_lane(MAIN_LANE)
            .ok()
            .flatten()
            .map(|leaf| {
                self.state
                    .find_entries_on_branch(&leaf, None, None, true)
                    .map(|entries| entries.iter().any(|e| e.id() == target_id))
                    .unwrap_or(false)
            })
            .unwrap_or(false);

        if !on_branch {
            return Err(Error::msg(format!(
                "context edit target {target_id} is not on the active branch"
            )));
        }

        let editable = matches!(
            target,
            Entry::Message { message, .. }
                if matches!(message.role.as_str(), "user" | "assistant" | "toolResult")
        );

        if !editable {
            return Err(Error::msg(format!(
                "context edit target {target_id} does not contribute editable model content"
            )));
        }

        Ok(self.commit_entry_lane(Entry::ContextEdit {
            base: EntryBase {
                id: new_session_id(),
                seq: 0,
                timestamp: 0,
                parent_id: None,
            },
            target_id: target_id.to_string(),
            replacement,
        }))
    }

    /// 追加一条 **retain-none** 压缩条目：只有摘要、不保留任何前置条目
    pub fn append_retain_none_compaction(
        &mut self,
        summary: &str,
        tokens_before: u64,
        details: Option<Value>,
        usage: Option<&Usage>,
    ) -> String {
        self.append_compaction(summary, tokens_before, None, details, usage)
    }

    /// 追加一条任意类型的 lane record，返回其 id。
    pub fn append_lane_record(&mut self, record: &LaneRecord) -> String {
        self.commit_record(record.clone())
    }

    /// 追加一条提示缓存保温的用量记录（`cause = "cache_warm"`）。保温不是会话回合，但仍要计入 token/成本统计。
    ///
    /// `provider` / `model`用于 `/session` 的成本分列归集：
    /// 没有归属的用量只能进 `Tools/summaries` 桶，看不出是哪个模型烧的。
    pub fn append_cache_warm_usage(
        &mut self,
        usage: &Usage,
        provider: &str,
        model: &str,
    ) -> String {
        self.append_lane_record(&LaneRecord::UsageRecord {
            base: session_v4::RecordBase {
                id: new_session_id(),
                seq: 0,
                lane: MAIN_LANE.to_string(),
                timestamp: 0,
            },
            usage: usage.clone(),
            cause: "cache_warm".to_string(),
            provider: Some(provider.to_string()),
            model: Some(model.to_string()),
            run_id: None,
            entry_id: None,
            attempt: None,
            stop_reason: None,
            tool_call_id: None,
        })
    }

    /// 追加一条压缩条目
    ///
    /// `first_kept_entry_id` = 保留尾部**起点条目 id**：投影时该条目（含）到本压缩条目
    /// （不含）之间的路径条目会原样进入模型上下文（不是内联副本）。`None` = retain-none，落盘时自引用
    pub fn append_compaction(
        &mut self,
        summary: &str,
        tokens_before: u64,
        first_kept_entry_id: Option<&str>,
        details: Option<Value>,
        usage: Option<&Usage>,
    ) -> String {
        let id = new_session_id();
        self.commit_entry_lane(Entry::Compaction {
            base: EntryBase {
                id: id.clone(),
                seq: 0,
                timestamp: 0,
                parent_id: None,
            },
            summary: summary.to_string(),
            first_kept_entry_id: Some(first_kept_entry_id.unwrap_or(&id).to_string()),
            tokens_before,
            details,
            usage: usage.cloned(),
        })
    }

    /// 追加一条分支摘要条目；`from_id` 为摘要起点（空或 `"root"` 表示从头开始），
    /// 指向不存在的条目时返回 `Err`。
    pub fn append_branch_summary(
        &mut self,
        from_id: &str,
        summary: &str,
        details: Option<Value>,
        usage: Option<&Usage>,
    ) -> Result<String> {
        if !from_id.is_empty()
            && from_id != "root"
            && !self.state.get_entries().iter().any(|e| e.id() == from_id)
        {
            return Err(Error::EntryNotFound(from_id.to_string()));
        }
        Ok(self.commit_entry_lane(Entry::BranchSummary {
            base: EntryBase {
                id: new_session_id(),
                seq: 0,
                timestamp: 0,
                parent_id: None,
            },
            from_id: from_id.to_string(),
            summary: summary.to_string(),
            details,
            usage: usage.cloned(),
        }))
    }

    /// 设置会话名
    pub fn append_session_info(&mut self, name: &str) {
        self.commit_fact(SessionMutation::Fact {
            seq: 0,
            fact: "name".to_string(),
            name: Some(name.to_string()),
            target_id: None,
            label: None,
        });
    }

    /// 设置/清除 label
    pub fn append_label_change(&mut self, target_id: &str, label: Option<&str>) -> Result<()> {
        if !self.state.get_entries().iter().any(|e| e.id() == target_id) {
            return Err(Error::EntryNotFound(target_id.to_string()));
        }
        self.commit_fact(SessionMutation::Fact {
            seq: 0,
            fact: "label".to_string(),
            name: None,
            target_id: Some(target_id.to_string()),
            label: label.map(|s| s.to_string()),
        });
        Ok(())
    }

    /// 设置当前 leaf：移动 lane 指针并持久化 lane mutation）
    pub fn set_leaf(&mut self, id: Option<&str>) -> Result<()> {
        if let Some(id) = id
            && !self.state.get_entries().iter().any(|e| e.id() == id)
        {
            return Err(Error::EntryNotFound(id.to_string()));
        }

        let mut m = SessionMutation::Lane {
            seq: 0,
            lane: MAIN_LANE.to_string(),
            leaf_id: id.map(|s| s.to_string()),
        };
        if let SessionMutation::Lane { seq, .. } = &mut m {
            *seq = self.state.next_sequence();
        }
        self.persist_mutation(&m);
        _ = self.state.apply_mutation(m);
        Ok(())
    }

    /// 回退最后一次用户输入：把 `main` lane 的叶子指针移到活动分支上
    /// **最近一条 user 消息的父节点**——该 user 消息与它之后的所有条目一起
    /// 离开活动分支，不再进入模型上下文（原始日志仍在会话树上，`/tree` 可找回）。
    ///
    /// 返回被回退的用户输入文本；活动分支上没有 user 消息（或叶子为空）时返回 `Ok(None)`。
    /// 只移动指针 + 追加一条 lane mutation，不改写历史。
    pub fn rewind_last_user_input(&mut self) -> Result<Option<String>> {
        let Some(leaf) = self.state.require_lane(MAIN_LANE).ok().flatten() else {
            return Ok(None);
        };

        let path = self
            .state
            .find_entries_on_branch(&leaf, None, None, true)
            .map_err(|e| Error::msg(format!("failed to read session branch: {e}")))?;

        // 从叶子往回找最近一条 user 消息（root-first 路径取 rev）。
        let target = path.iter().rev().find_map(|entry| match entry {
            Entry::Message { message, .. } if message.role == "user" => {
                Some((entry_base(entry).parent_id.clone(), message.text()))
            }
            _ => None,
        });

        let Some((parent, text)) = target else {
            return Ok(None);
        };

        // 父节点可能是 root（None）：`set_leaf(None)` 清空该 lane，回退第一条 user 消息时即如此。
        self.set_leaf(parent.as_deref())?;
        Ok(Some(text))
    }

    /// 从指定 leaf 回溯到 root（root-first），返回 entries 的 JSON 表示
    pub fn get_branch(&self, leaf_id: &str) -> Vec<Value> {
        self.state
            .find_entries_on_branch(leaf_id, None, None, true)
            .map(|entries| entries.iter().map(entry_to_json).collect())
            .unwrap_or_default()
    }

    /// 当前活动分支的可导入 JSONL：header + 分支 mutation 行。
    ///
    /// 供 `/bug` 附带 transcript：与 fork 写出的会话文件同构，可被 `Session::open`
    /// （即 `/import`）直接还原为同一分支；分支为空时只返回 header。
    pub fn serialize_branch_jsonl(&self) -> String {
        let mut out = self.header.encode();
        if let Ok(mutations) = self.state.branch_mutations(MAIN_LANE) {
            for m in &mutations {
                out.push_str(&session_v4::encode_mutation(m));
            }
        }
        out
    }

    /// 从 leaf 回溯到 root，构建当前分支的 message 列表（投影的消息部分）
    pub fn build_context_messages(&self) -> Vec<AgentMessage> {
        self.build_context_projection()
            .into_iter()
            .flat_map(|p| p.messages)
            .collect()
    }

    /// 构建「源条目 → 消息」投影
    ///
    /// 上下文条目集合 = `[最新 compaction] + 其保留区间内的原始条目 + compaction 之后的条目`。
    /// 保留区间 = `firstKeptEntryId`（含）到最新 compaction（不含）之间的路径条目；
    /// 起点条目不在本分支上（如切分支后）时区间为空。
    ///
    /// 保留的是**原始条目**（不是内联副本），因此：追加在压缩之后的 `context_edit`
    /// 仍能命中被保留的条目，`resume` / `fork` 之后也一致（条目 id 会持久化）。
    pub fn build_context_projection(&self) -> Vec<ProjectedContextEntry> {
        // 会话恢复前先经 reduce_lane_state 校验（损坏的 record log 拒绝恢复）
        if let Err(e) = self.reduce_lane() {
            eprintln!("session recovery rejected (corruption): {e}");
            return Vec::new();
        }
        let Some(leaf) = self.state.require_lane(MAIN_LANE).ok().flatten() else {
            return Vec::new();
        };

        // 分支以 root-first 顺序（root 在前，leaf 在后）
        let path = match self.state.find_entries_on_branch(&leaf, None, None, true) {
            Ok(p) => p,
            Err(_) => return Vec::new(),
        };

        if path.is_empty() {
            return Vec::new();
        }

        // 最新 compaction 排在最前（唯一贡献摘要的压缩，即 context[0]），其后是它的保留区间与之后的条目
        let newest = path
            .iter()
            .rposition(|e| matches!(e, Entry::Compaction { .. }));

        let mut context: Vec<&Entry> = Vec::with_capacity(path.len());
        match newest {
            Some(ci) => {
                context.push(&path[ci]);
                // 保留区间起点：按条目 id 定位。`None` / 自引用 / 不在本分支上 = 区间为空（retain-none）
                let start = match &path[ci] {
                    Entry::Compaction {
                        first_kept_entry_id,
                        ..
                    } => first_kept_entry_id
                        .as_deref()
                        .and_then(|id| path[..ci].iter().position(|e| e.id() == id)),
                    _ => None,
                };

                if let Some(s) = start {
                    context.extend(path[s..ci].iter());
                }

                context.extend(path[ci + 1..].iter());
            }
            None => context.extend(path.iter()),
        }

        // 上下文编辑（append-only 投影指令）：同一目标以**最新**一条为准；
        // 只在本次「上下文条目集合」内生效——被压缩摘要吞掉的目标不受影响
        let mut edits: HashMap<&str, &Option<ContextEditReplacement>> = HashMap::new();
        for entry in &context {
            if let Entry::ContextEdit {
                target_id,
                replacement,
                ..
            } = entry
            {
                edits.insert(target_id.as_str(), replacement);
            }
        }

        let mut result: Vec<ProjectedContextEntry> = Vec::with_capacity(context.len());
        for (i, entry) in context.iter().enumerate() {
            let messages = match entry {
                Entry::Message { message, .. } => {
                    if message.role == "assistant"
                        && message.stop_reason.as_deref() == Some("deferred")
                    {
                        Vec::new()
                    } else {
                        let mut m = message.clone();
                        m.entry_id = Some(entry.id().to_string());
                        match edits.get(entry.id()) {
                            // replacement = null：在模型上下文中省略该条目
                            Some(edit) => apply_context_edit(&m, edit).into_iter().collect(),
                            None => vec![m],
                        }
                    }
                }
                Entry::Compaction { summary, usage, .. } => {
                    // 只有**最新**压缩贡献摘要（`index > 0` 的压缩条目投影为空——
                    // 落在保留区间里的旧压缩，其保留条目已单独进入上下文）
                    let mut msgs = Vec::new();
                    if i == 0 {
                        msgs.push(Self::summary_message(
                            "compactionSummary",
                            summary,
                            usage.as_ref(),
                        ));
                    }
                    msgs
                }
                Entry::BranchSummary { summary, usage, .. } => {
                    vec![Self::summary_message(
                        "branchSummary",
                        summary,
                        usage.as_ref(),
                    )]
                }
                // context_edit 自身不产生模型消息（其余配置/自定义条目同理）
                _ => Vec::new(),
            };

            result.push(ProjectedContextEntry {
                entry_id: entry.id().to_string(),
                messages,
            });
        }
        result
    }

    /// 把 `messages` 上的切点索引映射为**投影条目 id**
    ///
    /// `messages` 应与 [`Self::build_context_messages`] 同序（后者是它的子序列：投影会省略
    /// deferred assistant 等条目）；对齐失败返回 `None`，由调用方回退到启发式。
    pub fn first_kept_entry_id_for_cut(
        &self,
        messages: &[AgentMessage],
        cut_index: usize,
    ) -> Option<String> {
        let projection = self.build_context_projection();
        let mut projected = projection
            .iter()
            .flat_map(|p| p.messages.iter().map(move |m| (p.entry_id.as_str(), m)));
        let mut cur = projected.next();

        for (i, m) in messages.iter().enumerate() {
            let (entry_id, projected_msg) = cur?;
            if !same_message(m, projected_msg) {
                continue; // 该消息不在投影里（deferred assistant 等）：跳过
            }

            if i == cut_index {
                return Some(entry_id.to_string());
            }
            cur = projected.next();
        }
        None
    }

    /// 对 main lane 做一次完整归约，得到活动分支、有效配置与各类统计；
    /// 归约失败（如引用断裂）时返回 `Err`。
    pub fn reduce_lane(&self) -> Result<LaneReductionResult> {
        let entries = self.state.entries().to_vec();
        let records = self.state.records().to_vec();
        let open_operations = self.state.find_open_operations(MAIN_LANE, None);
        let leaf_id = self.state.require_lane(MAIN_LANE).ok().flatten();
        let cfg_entries = entries.clone();

        Ok(reduce_lane_state(LaneReductionInput {
            lane: MAIN_LANE.to_string(),
            leaf_id,
            open_operations,
            records,
            entries,
            own_entries: vec![],
            configuration_entries: cfg_entries,
            defaults: EffectiveLaneConfiguration::default(),
        })?)
    }

    /// 恢复出的有效配置（模型/推理级别/活动工具）
    pub fn effective_config(&self) -> EffectiveLaneConfiguration {
        self.reduce_lane()
            .map(|r| r.effective_configuration)
            .unwrap_or_default()
    }

    /// 返回可导航树 + 已解析 label
    pub fn get_tree(&self) -> Value {
        let labels: HashMap<String, String> = self
            .state
            .entries()
            .iter()
            .filter_map(|e| {
                self.state
                    .get_label(e.id())
                    .map(|l| (e.id().to_string(), l.to_string()))
            })
            .collect();
        let mut children: HashMap<String, Vec<&Entry>> = HashMap::new();
        let mut roots: Vec<&Entry> = Vec::new();

        for e in self.state.entries() {
            let parent = match e {
                Entry::Message { base, .. }
                | Entry::ModelChange { base, .. }
                | Entry::ThinkingLevelChange { base, .. }
                | Entry::ActiveToolsChange { base, .. }
                | Entry::Compaction { base, .. }
                | Entry::BranchSummary { base, .. }
                | Entry::ContextEdit { base, .. }
                | Entry::Custom { base, .. } => &base.parent_id,
            };
            match parent {
                None => roots.push(e),
                Some(p) if p.is_empty() || p == "null" => roots.push(e),
                Some(pid) => children.entry(pid.clone()).or_default().push(e),
            }
        }

        for list in children.values_mut() {
            list.sort_by(|a, b| {
                let ta = entry_timestamp(a);
                let tb = entry_timestamp(b);
                ta.cmp(&tb).then_with(|| a.id().cmp(b.id()))
            });
        }
        let leaf_id = self.state.require_lane(MAIN_LANE).ok().flatten();
        let tree: Vec<Value> = roots
            .iter()
            .map(|e| Self::build_node(e, &children, &labels))
            .collect();
        json!({ "tree": tree, "leafId": leaf_id })
    }

    /// 递归把条目与已解析 label 组装成树节点 JSON（`entry` + `children` + 可选 `label`）。
    #[stacksafe::stacksafe]
    fn build_node(
        entry: &Entry,
        children: &HashMap<String, Vec<&Entry>>,
        labels: &HashMap<String, String>,
    ) -> Value {
        let id = entry.id().to_string();
        let mut obj = serde_json::Map::new();
        obj.insert("entry".into(), entry_to_json(entry));

        let child_values: Vec<Value> = children
            .get(&id)
            .map(|list| {
                list.iter()
                    .map(|c| Self::build_node(c, children, labels))
                    .collect()
            })
            .unwrap_or_default();
        obj.insert("children".into(), Value::Array(child_values));

        if let Some(label) = labels.get(&id) {
            obj.insert("label".into(), json!(label));
        }

        Value::Object(obj)
    }

    /// 全部 mutation 行的 JSON 表示（不含 header），保留旧消费形状（含 type/id/parentId 的 entry 直出）
    pub fn get_entries(&self) -> Vec<Value> {
        let mut out = Vec::new();
        for e in self.state.entries() {
            out.push(entry_to_json(e));
        }
        for r in self.state.records() {
            out.push(serde_json::to_value(r).unwrap_or(Value::Null));
        }
        out
    }

    /// 会话 header 的 JSON 表示；序列化失败时为 `None`（正常不会发生）。
    pub fn get_header(&self) -> Option<Value> {
        serde_json::to_value(&self.header).ok()
    }

    /// main lane 当前叶子条目 id；lane 尚无条目时为 `None`。
    pub fn get_leaf_id(&self) -> Option<&str> {
        self.state.lane_leaf(MAIN_LANE)
    }

    /// 最近一次记录的 active tools（按 seq 倒序取最后一条 ActiveToolsChange 条目）。
    pub fn last_active_tools(&self) -> Option<Vec<String>> {
        self.state.entries().iter().rev().find_map(|e| match e {
            Entry::ActiveToolsChange {
                active_tool_names, ..
            } => Some(active_tool_names.clone()),
            _ => None,
        })
    }

    /// /clone 的 fork 目标：main lane 叶子若不是 message（如配置变更条目），
    /// 向上回溯到最近一条 message entry 并返回其 id；无 message 时返回 None。
    pub fn clone_target_id(&self) -> Option<String> {
        let mut current = self.state.lane_leaf(MAIN_LANE)?.to_string();
        loop {
            match self.state.get_entry(&current) {
                Some(Entry::Message { .. }) => return Some(current),
                Some(entry) => current = entry_base(entry).parent_id.clone()?,
                None => return None,
            }
        }
    }

    /// 摘要类投影消息。带上条目上的 usage（生成摘要那一次调用的开销）：
    /// 不带就等于把摘要成本从底栏成本里漏掉——`ctx.messages` 是 footer 唯一的数据源。
    fn summary_message(role: &str, summary: &str, usage: Option<&Usage>) -> AgentMessage {
        let mut m = AgentMessage::user_text(summary);
        m.role = role.to_string();
        m.usage = usage.cloned();
        m
    }
}

/// 取出任意条目变体的时间戳（毫秒），用于树内按时间排序。
fn entry_timestamp(entry: &Entry) -> i64 {
    match entry {
        Entry::Message { base, .. }
        | Entry::ModelChange { base, .. }
        | Entry::ThinkingLevelChange { base, .. }
        | Entry::ActiveToolsChange { base, .. }
        | Entry::Compaction { base, .. }
        | Entry::BranchSummary { base, .. }
        | Entry::ContextEdit { base, .. }
        | Entry::Custom { base, .. } => base.timestamp,
    }
}

/// 应用一条上下文编辑：`replacement = None` 表示在模型上下文中省略（返回 `None`）；
/// `Some(rep)` 只替换 `content`，保留 role / provider / model / stopReason / toolCallId 等其余元数据
fn apply_context_edit(
    message: &AgentMessage,
    replacement: &Option<ContextEditReplacement>,
) -> Option<AgentMessage> {
    let rep = replacement.as_ref()?;
    let mut edited = message.clone();
    edited.content = rep.content.clone();
    Some(edited)
}

/// 投影条目：一个源条目 + 它对模型上下文的贡献（可能为空：`context_edit`、落在保留区间内的
/// 旧压缩条目、被投影忽略或被编辑省略的消息……）。
///
/// 保留源条目是为了让调用方把「按消息索引的切点」映射回条目 id
/// （compaction 的 `firstKeptEntryId`，见 [`Session::first_kept_entry_id_for_cut`]）。
pub struct ProjectedContextEntry {
    /// 源条目 id（投影消息按它回溯到条目；compaction 的 `firstKeptEntryId` 也取自它）
    pub entry_id: String,
    /// 该条目贡献给模型上下文的消息；被省略或忽略时为空
    pub messages: Vec<AgentMessage>,
}

/// 两条消息是否指同一条上下文消息：优先比条目 id（投影消息都带 id），
/// 合成消息（摘要）没有 id 时退回 role + 文本。
fn same_message(a: &AgentMessage, b: &AgentMessage) -> bool {
    match (a.entry_id.as_deref(), b.entry_id.as_deref()) {
        (Some(x), Some(y)) => x == y,
        _ => a.role == b.role && a.text() == b.text(),
    }
}

/// 把条目序列化为 JSON；序列化失败时返回 `Value::Null`。
fn entry_to_json(entry: &Entry) -> Value {
    serde_json::to_value(entry).unwrap_or(Value::Null)
}

/// 目录名编码：--<path>--，开头的 / 去掉，/ 和 : 替换为 -
pub fn encode_cwd_dir(cwd: &str) -> String {
    let trimmed = cwd.trim_start_matches(['/', '\\']);
    let safe = trimmed.replace(['/', '\\', ':'], "-");
    format!("--{}--", safe)
}

/// 某 cwd 的默认会话目录：`<agent_dir>/sessions/--<编码路径>--`。
pub fn default_session_dir(cwd: &str, agent_dir: &Path) -> PathBuf {
    agent_dir.join("sessions").join(encode_cwd_dir(cwd))
}

/// 进程内单调的毫秒分组计数器：同毫秒内多次生成 record id 时依次追加后缀。
static LAST_RECORD_MS: AtomicU64 = AtomicU64::new(u64::MAX);
/// 同毫秒内生成多个 record id 时依次追加的自增后缀序号。
static SAME_MS_SEQ: AtomicU64 = AtomicU64::new(0);

/// 生成唯一的 v4 record id：形状 `{prefix}-{now_ms}`；同毫秒内（或时钟倒退后）再次调用
/// 追加 `-{n}`（进程内单调，从 1 起）。旧形状在无碰撞时保持不变：会话文件不解析 id 形状，
/// 仅用于全局去重（`used_ids`），故后缀不影响任何既有/未来会话。
pub fn new_record_id(prefix: &str) -> String {
    let ms = now_ms();
    let prev = LAST_RECORD_MS.swap(ms, Ordering::Relaxed);
    let n = if prev == ms || ms < prev {
        SAME_MS_SEQ.fetch_add(1, Ordering::Relaxed)
    } else {
        SAME_MS_SEQ.store(1, Ordering::Relaxed);
        return format!("{prefix}-{ms}");
    };
    format!("{prefix}-{ms}-{n}")
}

/// uuidv7：48bit 毫秒时间戳 + version 7 + 随机位
fn new_session_id() -> String {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0) as u64;
    let mut rand = [0u8; 10];

    if getrandom::fill(&mut rand).is_ok() {
        // 系统随机源就绪
    } else {
        // 无系统随机源兜底：时间+pid 种子
        let seed = t ^ (std::process::id() as u64);
        for (i, b) in rand.iter_mut().enumerate() {
            *b = ((seed >> ((i % 8) * 8))
                ^ (seed
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(i as u64))) as u8;
        }
    }

    let mut bytes = [0u8; 16];
    bytes[0..6].copy_from_slice(&t.to_be_bytes()[2..8]);
    bytes[6..16].copy_from_slice(&rand);
    bytes[6] = (bytes[6] & 0x0F) | 0x70; // version 7
    bytes[8] = (bytes[8] & 0x3F) | 0x80; // variant
    let hex: String = bytes.iter().map(|b| format!("{:02x}", b)).collect();

    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

/// 会话 id 格式校验（标准 UUID 形式）
pub fn is_valid_session_id(id: &str) -> bool {
    let b = id.as_bytes();
    if b.len() != 36 {
        return false;
    }
    let dash_positions = [8usize, 13, 18, 23];
    for (i, &c) in b.iter().enumerate() {
        if dash_positions.contains(&i) {
            if c != b'-' {
                return false;
            }
        } else if !c.is_ascii_hexdigit() {
            return false;
        }
    }
    // 版本位：位置 14（0-indexed 14）为版本 7（uuidv7）
    b[14] == b'7'
}

/// 会话文件名：ISO 时间戳（:/. → -）_ sessionId.jsonl
fn session_file_name(created_at: i64, id: &str) -> String {
    let ts = chrono_ms_to_iso(created_at).replace([':', '.'], "-");
    format!("{ts}_{id}.jsonl")
}

/// 会话排序键：主键为文件 mtime（按 modified 排序），次键为 header createdAt。
fn session_ordering_key(path: &Path) -> (u128, u64) {
    let mtime_ns = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let header_ms = read_header_timestamp_ms(path).unwrap_or(0);
    (mtime_ns, header_ms)
}

/// 只读首行 header 取 `cwd`（不加载正文）；文件不可读/非 v4 header 返回 None。
fn read_header_cwd(path: &Path) -> Option<String> {
    let line = read_first_line(path)?;
    let v: Value = serde_json::from_str(&line).ok()?;
    v.get("cwd").and_then(|c| c.as_str()).map(|c| c.to_string())
}

/// 只读会话文件首行 header，取出 `parent_session_id`（fork/clone 记录的父会话）。
///
/// [`Session::open`] 会解析整个会话（大文件代价高），只需要文件头的调用方
/// （如 fork 继承）应走这里。文件不可读/非 v4 header 返回 None。
pub fn session_parent_id(path: &Path) -> Option<String> {
    let line = read_first_line(path)?;
    let header: JsonlV4Header = serde_json::from_str(&line).ok()?;
    header.parent_session_id
}

/// 解析会话文件头部的 createdAt（Unix 毫秒）为自 epoch 起的毫秒。
fn read_header_timestamp_ms(path: &Path) -> Option<u64> {
    let first_line = std::fs::File::open(path).ok()?;
    let reader = std::io::BufReader::new(first_line);
    let line = reader.lines().next()?.ok()?;
    let v: Value = serde_json::from_str(&line).ok()?;
    let ts = v.get("createdAt")?.as_i64()?;
    if ts >= 0 { Some(ts as u64) } else { None }
}

/// 查找目录下最近的会话文件
pub fn find_most_recent_session(dir: &Path) -> Option<PathBuf> {
    let mut most_recent: Option<((u128, u64), PathBuf)> = None;
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }

            let order = session_ordering_key(&path);
            if most_recent
                .as_ref()
                .map(|(o, _)| order > *o)
                .unwrap_or(true)
            {
                most_recent = Some((order, path));
            }
        }
    }
    most_recent.map(|(_, p)| p)
}

/// 列出目录下的会话文件，按修改时间倒序
pub fn list_sessions(dir: &Path) -> Vec<PathBuf> {
    let mut sessions: Vec<((u128, u64), PathBuf)> = Vec::new();
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            sessions.push((session_ordering_key(&path), path));
        }
    }
    sessions.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.as_os_str().cmp(a.1.as_os_str()))
    });
    sessions.into_iter().map(|(_, p)| p).collect()
}

/// 会话列表展示元数据（每行信息：名称/首条消息、消息数、修改时间）
#[derive(Debug, Clone)]
pub struct SessionMeta {
    /// 会话名；未命名时为 None
    pub name: Option<String>,
    /// 首条 user 消息压成单行后的摘要，无则空串
    pub first_message: String,
    /// 会话中 Message 类条目的总数
    pub message_count: usize,
    /// 相对当前时间的修改时长（如 "5m"、"2d"）
    pub modified_age: String,
}

/// 打开会话文件并提取展示元数据（仅读，不建树）
pub fn session_meta(path: &Path) -> Option<SessionMeta> {
    let s = Session::open(&path.to_string_lossy()).ok()?;
    let mut first_message = String::new();
    let mut message_count = 0usize;
    for e in s.state.entries() {
        if let session_v4::Entry::Message { message, .. } = e {
            message_count += 1;
            if first_message.is_empty() && message.role == "user" {
                first_message = message.text().replace(['\r', '\n'], " ").trim().to_string();
            }
        }
    }
    let modified_age = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .map(format_session_age)
        .unwrap_or_default();
    Some(SessionMeta {
        name: s.name.clone(),
        first_message,
        message_count,
        modified_age,
    })
}

/// 相对时间（now/5m/3h/2d/1w/4mo/1y，floor 取整）
pub(crate) fn format_session_age(modified: SystemTime) -> String {
    let diff = SystemTime::now()
        .duration_since(modified)
        .unwrap_or_default()
        .as_secs();
    let mins = diff / 60;
    let hours = diff / 3600;
    let days = diff / 86400;
    if mins < 1 {
        "now".to_string()
    } else if mins < 60 {
        format!("{}m", mins)
    } else if hours < 24 {
        format!("{}h", hours)
    } else if days < 7 {
        format!("{}d", days)
    } else if days < 30 {
        format!("{}w", days / 7)
    } else if days < 365 {
        format!("{}mo", days / 30)
    } else {
        format!("{}y", days / 365)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::{AgentMessage, ContentBlock};

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user_text(text)
    }

    fn session_at(dir: &Path) -> Session {
        let d = dir.join("sessions");
        Session::create(dir.to_str().unwrap(), Some(d.clone()), true).unwrap()
    }

    /// `create_with` 是唯一构造入口：`create` / `create_with_id` 是它的薄包装，
    /// 三者产出的会话形态必须一致。
    #[test]
    fn create_with_is_the_single_construction_path() {
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("sessions");

        let a = Session::create_with(SessionCreateOptions {
            cwd: "/tmp/a".to_string(),
            session_dir: Some(sess.clone()),
            persist: true,
            ..Default::default()
        })
        .unwrap();
        let b = Session::create("/tmp/a", Some(sess.clone()), true).unwrap();
        let c = Session::create_with_id("/tmp/a", Some(sess.clone()), true, &a.session_id).unwrap();

        assert_eq!(a.cwd, "/tmp/a");
        assert!(a.session_file.is_some());
        // 生成 id 的路径不重复，指定 id 的路径复用
        assert_ne!(a.session_id, b.session_id);
        assert_eq!(a.session_id, c.session_id);
        // 薄包装与显式 options 的 header 形态一致（parent 均为 None）
        assert_eq!(a.header.parent_session_id, None);
        assert_eq!(c.header.parent_session_id, None);
        assert_eq!(c.header.cwd, a.header.cwd);
    }

    /// 子代理会话：`parent_session_id` 必须落进 header，供 `/resume` 嵌套显示。
    #[test]
    fn create_with_persists_parent_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("sessions");

        let parent = Session::create("/tmp/p", Some(sess.clone()), true).unwrap();
        let child = Session::create_with(SessionCreateOptions {
            cwd: "/tmp/p".to_string(),
            session_dir: Some(sess.clone()),
            persist: true,
            parent_session_id: Some(parent.session_id.clone()),
            ..Default::default()
        })
        .unwrap();

        let path = child.session_file.clone().unwrap();
        let reopened = Session::open(&path.to_string_lossy()).unwrap();
        assert_eq!(
            reopened.parent_session_id(),
            Some(parent.session_id.as_str())
        );

        // 两台会话都在目录里，且 child 能读到自己的父会话
        let listed = list_sessions(&sess);
        assert_eq!(listed.len(), 2, "{listed:?}");
    }

    /// fork 的会话文件头带 `parent_session_id`，[`session_parent_id`] 能只读首行取回它——
    /// 这正是 `/fork`、`/clone` 后 tasks 扩展识别父会话的依据。
    #[test]
    fn fork_header_exposes_parent_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let sessions = dir.path().join("sessions");

        let parent = Session::create(&cwd, Some(sessions.clone()), true).unwrap();
        let parent_file = parent.get_session_file().unwrap().to_path_buf();
        let forked =
            Session::fork_from(&parent_file.to_string_lossy(), &cwd, Some(sessions.clone()))
                .unwrap();

        let fork_file = forked.get_session_file().unwrap();
        assert_eq!(
            session_parent_id(fork_file).as_deref(),
            Some(parent.session_id.as_str()),
            "fork 文件头应记录父会话 id"
        );

        // 普通新会话没有父会话
        let plain = Session::create(&cwd, Some(sessions), true).unwrap();
        assert_eq!(session_parent_id(plain.get_session_file().unwrap()), None);
    }

    /// `persist: false` 时不落盘（子代理可关闭会话持久化）。
    #[test]
    fn create_with_without_persist_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let sess = dir.path().join("sessions-none");
        let s = Session::create_with(SessionCreateOptions {
            cwd: "/tmp/n".to_string(),
            session_dir: Some(sess.clone()),
            persist: false,
            ..Default::default()
        })
        .unwrap();
        assert!(s.session_file.is_none());
        assert!(!sess.exists());
    }

    /// 非法 id 仍然被拒绝（与旧 `create_with_id` 行为一致）。
    #[test]
    fn create_with_rejects_invalid_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let err = Session::create_with(SessionCreateOptions {
            cwd: "/tmp/x".to_string(),
            session_dir: Some(dir.path().join("s")),
            persist: true,
            session_id: Some("not-a-uuid".to_string()),
            ..Default::default()
        })
        .unwrap_err();
        assert!(format!("{err}").contains("Invalid session id"), "{err}");
    }

    #[test]
    fn new_record_id_unique_within_same_ms() {
        // 同毫秒内多次生成必须唯一（旧实现 `tool-{now_ms}` 同 ms 撞车导致会话损坏）
        let ids: Vec<String> = (0..16)
            .map(|i| new_record_id(if i % 2 == 0 { "tool" } else { "step" }))
            .collect();
        let unique: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len(), "ids must be unique: {ids:?}");
        // 形状 `{prefix}-{ms}` 或 `{prefix}-{ms}-{n}`：前缀 + 纯数字/短横线后缀
        for (i, id) in ids.iter().enumerate() {
            let prefix = if i % 2 == 0 { "tool" } else { "step" };
            let Some(suffix) = id.strip_prefix(&format!("{prefix}-")) else {
                panic!("bad prefix in {id}");
            };
            assert!(
                suffix.chars().all(|c| c == '-' || c.is_ascii_digit()),
                "bad suffix in {id}"
            );
        }
    }

    #[test]
    fn create_immediately_persists_v4_header() {
        let dir = tempfile::tempdir().unwrap();
        let s = session_at(dir.path());
        assert!(s.is_persisted());
        assert!(
            s.get_session_file().is_some(),
            "v4 create 即落盘 header 文件"
        );
        assert_eq!(s.get_leaf_id(), None, "新建会话 main lane 为空");
        let first = std::fs::read_to_string(s.get_session_file().unwrap()).unwrap();
        let v: Value = serde_json::from_str(first.lines().next().unwrap()).unwrap();
        assert_eq!(v["kind"], "header");
        assert_eq!(v["version"], 4);
        assert!(
            v.get("createdAt").and_then(|x| x.as_i64()).is_some(),
            "createdAt 为毫秒数"
        );
    }

    #[test]
    fn append_message_chains_to_lane_leaf_and_advances() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        let id1 = s.append_message(&user("hi"));
        assert_eq!(s.get_leaf_id(), Some(id1.as_str()));
        let id2 = s.append_message(&user("again"));
        assert_eq!(s.get_leaf_id(), Some(id2.as_str()));
        // 第二条消息 parentId = 第一条（lane leaf）
        let e2 = s.state.get_entry(&id2).unwrap();
        let parent = match e2 {
            session_v4::Entry::Message { base, .. } => base.parent_id.clone(),
            _ => None,
        };
        assert_eq!(parent.as_deref(), Some(id1.as_str()));
        // 落盘行带 kind=entry + lane=main
        let text = std::fs::read_to_string(s.get_session_file().unwrap()).unwrap();
        let v: Value = serde_json::from_str(text.lines().nth(1).unwrap()).unwrap();
        assert_eq!(v["kind"], "entry");
        assert_eq!(v["lane"], "main");
    }

    // ---- append-only 上下文编辑（pi 0.87.0 `ContextEditEntry`）----

    #[test]
    fn context_edit_omits_target_from_rebuilt_context_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("hello"));
        let mut assistant = AgentMessage::user_text("failed attempt");
        assistant.role = "assistant".into();
        assistant.stop_reason = Some("error".into());
        let failed = s.append_message(&assistant);
        s.append_message(&user("after"));

        // 编辑前：三条都在投影里
        assert_eq!(s.build_context_messages().len(), 3);

        let edit = s.append_context_edit(&failed, None).unwrap();
        assert_eq!(s.get_leaf_id(), Some(edit.as_str()), "编辑自身成为新叶子");

        let msgs = s.build_context_messages();
        assert_eq!(msgs.len(), 2, "被省略的条目不再进入模型上下文");
        assert!(
            !msgs
                .iter()
                .any(|m| m.entry_id.as_deref() == Some(failed.as_str())),
            "原始 transcript 保留、但投影中不出现"
        );
        // 原始条目仍在（append-only，不改写历史）
        assert!(s.state.get_entry(&failed).is_some());

        // 重开（resume）后编辑仍生效
        let path = s.get_session_file().unwrap().to_string_lossy().to_string();
        let reopened = Session::open(&path).unwrap();
        let msgs = reopened.build_context_messages();
        assert_eq!(msgs.len(), 2, "resume 后省略仍生效");
        assert!(
            !msgs
                .iter()
                .any(|m| m.entry_id.as_deref() == Some(failed.as_str()))
        );
    }

    #[test]
    fn context_edit_replaces_content_and_latest_edit_wins() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        let target = s.append_message(&user("original"));

        s.append_context_edit(
            &target,
            Some(ContextEditReplacement {
                content: vec![ContentBlock::Text {
                    text: "rewritten".into(),
                    text_signature: None,
                }],
            }),
        )
        .unwrap();
        let msgs = s.build_context_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text(), "rewritten");
        assert_eq!(msgs[0].role, "user", "只替换 content，role 等元数据保留");

        // 同一目标再追加一条：最新一条生效
        s.append_context_edit(&target, None).unwrap();
        assert!(s.build_context_messages().is_empty(), "最新编辑为省略");
    }

    #[test]
    fn context_edit_before_latest_compaction_has_no_effect() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        let old = s.append_message(&user("old turn"));
        let edit = s.append_context_edit(&old, None).unwrap();
        // 压缩吞掉 old（保留尾部为空：retain-none）
        s.append_retain_none_compaction("sum", 10, None, None);
        assert_eq!(
            s.build_context_messages().len(),
            1,
            "投影 = [compactionSummary]"
        );
        // 编辑在压缩之前、目标已被吞掉：即使重开也不影响投影
        let path = s.get_session_file().unwrap().to_string_lossy().to_string();
        let reopened = Session::open(&path).unwrap();
        let msgs = reopened.build_context_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "compactionSummary");
        // 编辑条目本身仍在历史里（可审计）
        assert!(
            reopened
                .get_entries()
                .iter()
                .any(|e| e["id"] == edit.as_str())
        );
    }

    /// 文本投影（便于断言）
    fn texts(s: &Session) -> Vec<String> {
        s.build_context_messages()
            .iter()
            .map(|m| m.text())
            .collect()
    }

    fn replace_with(text: &str) -> Option<ContextEditReplacement> {
        Some(ContextEditReplacement {
            content: vec![ContentBlock::Text {
                text: text.to_string(),
                text_signature: None,
            }],
        })
    }

    #[test]
    fn context_edit_after_compaction_applies_to_retained_entry_and_survives_reopen() {
        // 对齐 pi `applies post-compaction edits to retained pre-compaction entries`：
        // 保留的是**原始条目**，所以压缩之后追加的 context_edit 仍能命中它——且 resume 后一致
        // （条目 id 落盘，不依赖 `AgentMessage.entry_id` 的进程内内存）。
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("summarized"));
        let retained = s.append_message(&user("original retained"));
        s.append_compaction("sum", 100, Some(&retained), None, None);
        assert_eq!(texts(&s), vec!["sum", "original retained"]);

        s.append_context_edit(&retained, replace_with("edited retained"))
            .unwrap();
        assert_eq!(texts(&s), vec!["sum", "edited retained"]);

        let path = s.get_session_file().unwrap().to_string_lossy().to_string();
        let reopened = Session::open(&path).unwrap();
        assert_eq!(
            texts(&reopened),
            vec!["sum", "edited retained"],
            "resume 后编辑必须仍生效（保留条目 + 编辑条目都在上下文条目集合里）"
        );
    }

    #[test]
    fn context_edit_before_compaction_applies_to_retained_entry() {
        // pi `buildContextEntries` 会把保留区间内的**原始条目**（含 context_edit 自身）
        // 一并带进上下文集合，因此「先编辑、后压缩」的编辑不会因压缩而失效。
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        let target = s.append_message(&user("original"));
        s.append_context_edit(&target, replace_with("rewritten"))
            .unwrap();
        s.append_compaction("sum", 100, Some(&target), None, None);
        assert_eq!(texts(&s), vec!["sum", "rewritten"]);

        let path = s.get_session_file().unwrap().to_string_lossy().to_string();
        let reopened = Session::open(&path).unwrap();
        assert_eq!(texts(&reopened), vec!["sum", "rewritten"]);
    }

    #[test]
    fn older_compaction_in_retained_range_contributes_no_summary() {
        // pi：`index > 0` 的压缩条目投影为空——保留区间跨过旧压缩时，旧摘要不得重复出现。
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        let first = s.append_message(&user("first"));
        s.append_compaction("sum1", 10, Some(&first), None, None);
        let second = s.append_message(&user("second"));
        // 第二次压缩的保留区间跨过 sum1（起点 = first，在 sum1 之前）
        s.append_compaction("sum2", 20, Some(&first), None, None);
        assert_eq!(texts(&s), vec!["sum2", "first", "second"]);
        let _ = second;
    }

    #[test]
    fn context_edit_rejects_missing_off_branch_and_non_editable_targets() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        let msg = s.append_message(&user("hi"));

        // 目标不存在
        assert!(s.append_context_edit("nope", None).is_err());

        // 非可编辑条目（配置类）
        let tools = s.append_active_tools_change(&["read".into()]);
        assert!(s.append_context_edit(&tools, None).is_err());

        // 不在活动分支上：另起一条分支后回到主线
        let branch_msg = s.append_message(&user("branch"));
        s.set_leaf(Some(&msg)).unwrap();
        assert!(s.append_context_edit(&branch_msg, None).is_err());
        // 分支上的目标回到该分支后可以编辑
        s.set_leaf(Some(&branch_msg)).unwrap();
        assert!(s.append_context_edit(&branch_msg, None).is_ok());
    }

    #[test]
    fn retain_none_compaction_keeps_only_summary() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("a"));
        s.append_message(&user("b"));
        s.append_retain_none_compaction("self-retaining", 42, None, None);

        let msgs = s.build_context_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "compactionSummary");
        assert_eq!(msgs[0].text(), "self-retaining");

        let path = s.get_session_file().unwrap().to_string_lossy().to_string();
        let reopened = Session::open(&path).unwrap();
        let msgs = reopened.build_context_messages();
        assert_eq!(msgs.len(), 1, "retain-none 在 resume 后仍只留摘要");
    }

    #[test]
    fn open_roundtrip_restores_messages_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("first"));
        s.append_message(&AgentMessage {
            role: "assistant".into(),
            thinking_level: None,
            ..AgentMessage::user_text("reply")
        });
        s.append_session_info("my session");
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);
        let opened = Session::open(&path.to_string_lossy()).unwrap();
        let msgs = opened.build_context_messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text(), "first");
        assert_eq!(msgs[1].text(), "reply");
        assert_eq!(opened.name.as_deref(), Some("my session"));
    }

    #[test]
    fn open_rejects_seq_jump_with_broken_chain() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("m"));
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);
        // 篡改文件：插入一条 seq=10 的行（跳号）——父为 null 无法挂到当前 leaf=m 之后。
        // seq 跳号会先被就地修复（不再因编号拒绝），但修复后链条仍断 → D 档截断。
        let content = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = content.lines().collect();
        let bad = "{\"kind\":\"entry\",\"lane\":\"main\",\"type\":\"message\",\"id\":\"bad\",\"seq\":10,\"timestamp\":1,\"parentId\":null,\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"x\"}]}}".to_string();
        lines.insert(2, &bad);
        std::fs::write(&path, lines.join("\n")).unwrap();
        let err = Session::open(&path.to_string_lossy())
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not chain"), "{err}");
    }

    #[test]
    fn open_rejects_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("m"));
        s.append_message(&user("n"));
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);
        // 篡改：第二条消息 parentId 指向不存在的 entry
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        let bad = lines[2].replace(".parentId\":\n", ""); // no-op guard
        let lines_orig = content.lines().collect::<Vec<_>>();
        let fixed = lines_orig
            .iter()
            .map(|l| {
                if l.contains("\"type\":\"message\"") && l.contains("\"text\":\"n\"") {
                    l.replace_suffix_parent()
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>();
        let _ = (bad, fixed);
        // 直接逐行修改：把第二条消息的 parentId 替换为 phantom-id
        let mut out = String::new();
        let mut seen = 0;
        for l in content.lines() {
            if l.contains("\"text\":\"n\"") && seen == 0 {
                seen = 1;
                let v: Value = serde_json::from_str(l).unwrap();
                // message 行是 kind=entry，parentId 在 entry 顶层
                let mut map = v.as_object().unwrap().clone();
                map.insert("parentId".into(), json!("phantom-missing"));
                out.push_str(&serde_json::to_string(&Value::Object(map)).unwrap());
                out.push('\n');
            } else {
                out.push_str(l);
                out.push('\n');
            }
        }
        std::fs::write(&path, out).unwrap();
        let err = Session::open(&path.to_string_lossy())
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("references missing parent")
                || err.contains("does not chain to the lane leaf"),
            "{err}"
        );
    }

    #[test]
    fn continue_recent_skips_and_deletes_empty_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        // 最早：有用户消息
        let mut s1 = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        s1.append_message(&user("hello"));
        drop(s1);
        // 同毫秒创建的会话 mtime/createdAt 可能完全相同，list_sessions 排序 tie 后
        // fallback 按文件名（随机 uuid）倒序 → 遍历顺序随机 → s1 偶发排最前导致
        // s2/s3 不被清理（flaky）。每次创建前 sleep 强制时间戳严格递增。
        std::thread::sleep(std::time::Duration::from_millis(2));
        // 其后两个：空会话（只写 header，无用户消息）
        let s2 = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        let f2 = s2.get_session_file().unwrap().to_path_buf();
        drop(s2);
        std::thread::sleep(std::time::Duration::from_millis(2));
        let s3 = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        let f3 = s3.get_session_file().unwrap().to_path_buf();
        drop(s3);

        let ret = Session::continue_recent_checked(cwd, Some(sess_dir.clone()), false);
        match ret {
            ContinueRecentResult::Ok(s) => {
                let msgs = s.build_context_messages();
                assert_eq!(msgs.len(), 1);
                assert_eq!(msgs[0].text(), "hello");
            }
            _ => panic!("expected Ok with messages"),
        }
        assert!(!f2.exists(), "空会话 2 应被删除");
        assert!(!f3.exists(), "空会话 3 应被删除");
    }

    /// 显式 `--session-dir` 且不是本 cwd 默认目录时按 header `cwd` 过滤：
    /// 更新但属于别的项目的会话被跳过（不被解析、不被删除），命中本 cwd 的最新会话即停。
    #[test]
    fn continue_recent_filters_other_cwd_sessions_in_explicit_dir() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let other_cwd = dir.path().join("other");
        std::fs::create_dir_all(&other_cwd).unwrap();
        let other = other_cwd.to_str().unwrap();
        let shared = dir.path().join("shared-sessions");

        // 本 cwd 的旧会话（有用户消息）
        let mut mine = Session::create(cwd, Some(shared.clone()), true).unwrap();
        mine.append_message(&user("mine"));
        drop(mine);

        // 更新：别的项目的空会话（若被解析会被当空会话删除）
        std::thread::sleep(std::time::Duration::from_millis(2));
        let theirs = Session::create(other, Some(shared.clone()), true).unwrap();
        let theirs_file = theirs.get_session_file().unwrap().to_path_buf();
        drop(theirs);

        match Session::continue_recent_checked(cwd, Some(shared.clone()), false) {
            ContinueRecentResult::Ok(s) => {
                let msgs = s.build_context_messages();
                assert_eq!(msgs[0].text(), "mine", "应命中本 cwd 的会话");
            }
            _ => panic!("expected Ok with this cwd's session"),
        }
        assert!(theirs_file.exists(), "其它项目的会话不应被当成空会话删除");
    }

    #[test]
    fn continue_recent_all_empty_creates_new() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        let s1 = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        let f1 = s1.get_session_file().unwrap().to_path_buf();
        drop(s1);
        let s2 = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        let f2 = s2.get_session_file().unwrap().to_path_buf();
        drop(s2);

        match Session::continue_recent_checked(cwd, Some(sess_dir.clone()), false) {
            ContinueRecentResult::Created(_) => {}
            _ => panic!("全部为空应新建"),
        }
        assert!(!f1.exists());
        assert!(!f2.exists());
    }

    #[test]
    fn has_user_messages_detects_empty_and_nonempty() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        let empty = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        assert!(!empty.has_user_messages(), "新建空会话无用户消息");
        drop(empty);
        let mut full = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        assert!(!full.has_user_messages());
        full.append_message(&user("hi"));
        assert!(full.has_user_messages(), "追加用户消息后非空");
    }

    #[test]
    fn squash_stale_operations_marks_old_runs_finished() {
        // 崩溃残留：两个未关闭的 operation（无 finish）。用户从面板选择 Rewrite 压平后，
        // 旧 op 应补 operation_finished(outcome="interrupted")，只剩最新 op 打开，
        // 磁盘文件可再次正常恢复（不再报 MultipleOpenOperations）。
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("hi"));
        let start = |id: &str| session_v4::LaneRecord::OperationStarted {
            base: session_v4::RecordBase {
                id: id.into(),
                seq: 0,
                lane: MAIN_LANE.into(),
                timestamp: 0,
            },
            source_leaf_id: None,
            intent: session_v4::OperationIntent::Run {
                original_prompt: vec![user("q")],
                initial_messages: vec![],
                system_prompt_override: None,
            },
        };
        s.append_lane_record(&start("run-old-1"));
        s.append_lane_record(&start("run-old-2"));
        s.append_lane_record(&start("run-new"));
        assert_eq!(s.open_operations_count(), 3);
        let path = s.get_session_file().unwrap().to_path_buf();

        let n = s.squash_stale_operations();
        assert_eq!(n, 2, "除最新 op 外都应压平");
        assert_eq!(s.open_operations_count(), 1);
        let open = s.state.find_open_operations(MAIN_LANE, None);
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].id(), "run-new", "保留 seq 最新的 operation");

        // 磁盘尾部追加了 2 条 operation_finished（interrupted）
        let content = std::fs::read_to_string(&path).unwrap();
        let finishes = content
            .lines()
            .filter(|l| l.contains("\"type\":\"operation_finished\""))
            .collect::<Vec<_>>();
        assert_eq!(finishes.len(), 2, "finish 记录写入文件: {content}");
        assert!(
            finishes.iter().all(|l| l.contains("interrupted")),
            "outcome 应为 interrupted: {content}"
        );

        // 重新打开：reduce_lane 通过且只有一个 open op
        let reopened = Session::open(&path.to_string_lossy()).unwrap();
        assert_eq!(reopened.open_operations_count(), 1);
        assert!(reopened.reduce_lane().is_ok());
    }

    #[test]
    fn interrupt_operations_opened_since_closes_only_new_ops() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("hi"));
        let start = |id: &str| session_v4::LaneRecord::OperationStarted {
            base: session_v4::RecordBase {
                id: id.into(),
                seq: 0,
                lane: MAIN_LANE.into(),
                timestamp: 0,
            },
            source_leaf_id: None,
            intent: session_v4::OperationIntent::Run {
                original_prompt: vec![user("q")],
                initial_messages: vec![],
                system_prompt_override: None,
            },
        };
        // 继承来的僵尸（轮次开始前已存在）不在补写范围内
        s.append_lane_record(&start("inherited-zombie"));
        let before: HashSet<String> = s.open_operation_ids().into_iter().collect();

        // 轮次中新开了两个 op（如 overflow 恢复重跑 run_loop）
        s.append_lane_record(&start("round-op-1"));
        s.append_lane_record(&start("round-op-2"));
        assert_eq!(s.open_operations_count(), 3);

        let n = s.interrupt_operations_opened_since(&before);
        assert_eq!(n, 2, "只补写轮次期间新开的 op");
        assert_eq!(s.open_operations_count(), 1, "继承僵尸保留原样");
        let open = s.open_operation_ids();
        assert_eq!(open, vec!["inherited-zombie".to_string()]);

        // 再次调用应为空操作（不重复写 finish）
        assert_eq!(s.interrupt_operations_opened_since(&before), 0);

        // 磁盘尾部确实有 2 条 interrupted finish
        let path = s.get_session_file().unwrap().to_path_buf();
        let content = std::fs::read_to_string(&path).unwrap();
        let finishes: Vec<&str> = content
            .lines()
            .filter(|l| l.contains("\"type\":\"operation_finished\""))
            .collect();
        assert_eq!(finishes.len(), 2, "finish 记录写入文件: {content}");
        assert!(finishes.iter().all(|l| l.contains("interrupted")));

        // 重开可正常归约
        let reopened = Session::open(&path.to_string_lossy()).unwrap();
        assert!(reopened.reduce_lane().is_ok());
        assert_eq!(reopened.open_operations_count(), 1);
    }

    #[test]
    fn interrupt_all_open_operations_closes_everything() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("hi"));
        let start = |id: &str| session_v4::LaneRecord::OperationStarted {
            base: session_v4::RecordBase {
                id: id.into(),
                seq: 0,
                lane: MAIN_LANE.into(),
                timestamp: 0,
            },
            source_leaf_id: None,
            intent: session_v4::OperationIntent::Run {
                original_prompt: vec![user("q")],
                initial_messages: vec![],
                system_prompt_override: None,
            },
        };
        s.append_lane_record(&start("r1"));
        s.append_lane_record(&start("r2"));
        assert_eq!(s.open_operations_count(), 2);
        assert_eq!(s.interrupt_all_open_operations(), 2);
        assert_eq!(s.open_operations_count(), 0);
        assert_eq!(s.interrupt_all_open_operations(), 0, "再调为空操作");
    }

    #[test]
    fn open_operations_count_reports_stale_runs() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("hi"));
        assert_eq!(s.open_operations_count(), 0);
        let start = |id: &str| session_v4::LaneRecord::OperationStarted {
            base: session_v4::RecordBase {
                id: id.into(),
                seq: 0,
                lane: MAIN_LANE.into(),
                timestamp: 0,
            },
            source_leaf_id: None,
            intent: session_v4::OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
            },
        };
        s.append_lane_record(&start("r1"));
        s.append_lane_record(&start("r2"));
        assert_eq!(s.open_operations_count(), 2, "残留 open op 计入");
    }

    #[test]
    fn import_repairable_flow_truncates_and_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("first"));
        s.append_message(&user("second"));
        s.append_message(&user("third"));
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);
        // 篡改中间行：插入非法 JSON（语法错误，可跳过抢救）
        let content = std::fs::read_to_string(&path).unwrap();
        let mut lines: Vec<&str> = content.lines().collect();
        lines.insert(2, "{this is not valid json}");
        std::fs::write(&path, lines.join("\n")).unwrap();
        // /import 用的 open_checked：中间行损坏应返回 Repairable 而不是直接报错
        let plan = match Session::open_checked(&path.to_string_lossy()) {
            OpenSessionResult::Repairable { plan, .. } => plan,
            _ => panic!("expected Repairable"),
        };
        assert_eq!(plan.total_lines, 5);
        assert!(
            plan.salvage_available(),
            "非法 JSON 行可跳过抢救，后缀应完整保留"
        );
        let truncate_msgs = |filename: &str| {
            let opened = Session::open(filename).unwrap();
            opened.build_context_messages().len()
        };
        // 截断路径：只留首个损坏行之前的有效前缀
        Session::repair_write(&path.to_string_lossy(), &plan, false).unwrap();
        assert_eq!(truncate_msgs(&path.to_string_lossy()), 1);
        // 抢救路径：跳过损坏行、seq 重排，重新打开应完整保留 3 条消息
        Session::repair_write(&path.to_string_lossy(), &plan, true).unwrap();
        let reopened = Session::open(&path.to_string_lossy()).unwrap();
        let msgs = reopened.build_context_messages();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].text(), "first");
        assert_eq!(msgs[2].text(), "third");
        // 备份文件应存在
        assert!(path.with_extension("jsonl.bak").exists());
    }

    #[test]
    fn salvage_duplicate_id_keeps_suffix() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("first"));
        s.append_lane_record(&session_v4::LaneRecord::OperationStarted {
            base: session_v4::RecordBase {
                id: "run-1".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 1,
            },
            source_leaf_id: None,
            intent: session_v4::OperationIntent::Run {
                original_prompt: vec![user("first")],
                initial_messages: vec![],
                system_prompt_override: None,
            },
        });
        s.append_lane_record(&session_v4::LaneRecord::StepAttempt {
            base: session_v4::RecordBase {
                id: "step-1".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 1,
            },
            run_id: "run-1".into(),
            step: "assistant".into(),
            attempt: 1,
            result_entry_id: "m-none".into(),
            compaction_reason: None,
        });
        s.append_lane_record(&session_v4::LaneRecord::OperationFinished {
            base: session_v4::RecordBase {
                id: "fin-1".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 1,
            },
            run_id: "run-1".into(),
            outcome: "completed".into(),
            error: None,
        });
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);
        // 篡改：在 step-1 后插入同 id（step-1）但 seq 连续（4）的重复行——正是用户遇到的
        // “新 seq + 旧 id”场景（如 duplicate tool id）；fin-1 顺延为 seq 5。
        let content = std::fs::read_to_string(&path).unwrap();
        let mut out = String::new();
        for l in content.lines() {
            if l.contains("\"id\":\"step-1\"") {
                out.push_str(l);
                out.push('\n');
                let mut dup: Value = serde_json::from_str(l).unwrap();
                dup["seq"] = json!(4);
                out.push_str(&serde_json::to_string(&dup).unwrap());
                out.push('\n');
            } else if l.contains("\"id\":\"fin-1\"") {
                let mut v: Value = serde_json::from_str(l).unwrap();
                v["seq"] = json!(5);
                out.push_str(&serde_json::to_string(&v).unwrap());
                out.push('\n');
            } else {
                out.push_str(l);
                out.push('\n');
            }
        }
        std::fs::write(&path, out).unwrap();
        let plan = match Session::open_checked(&path.to_string_lossy()) {
            OpenSessionResult::Repairable { plan, .. } => plan,
            _ => panic!("expected Repairable"),
        };
        assert!(
            plan.salvage_available(),
            "重复 id 属 B 档冗余，可跳过抢救：{} ",
            plan.error
        );
        assert_eq!(plan.salvaged_skipped, 1);
        assert!(
            plan.error.contains("contains duplicate id"),
            "{}",
            plan.error
        );
        Session::repair_write(&path.to_string_lossy(), &plan, true).unwrap();
        // 重开成功；磁盘应为 header + 4 条 mutation（重复 step-1 已丢弃），fin-1 的 seq 已重排为 4
        let opened = Session::open_checked(&path.to_string_lossy());
        assert!(
            matches!(opened, OpenSessionResult::Ok(_)),
            "repair 后应可正常打开"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines.len(),
            5,
            "header + run-1/step-1/fin-1/message（重复行已丢弃）"
        );
        let fin: Value = serde_json::from_str(
            lines
                .iter()
                .find(|l| l.contains("\"id\":\"fin-1\""))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(fin["seq"], 4, "fin-1 原 seq5 应重排为 4，保持严格连续");
        // 备份文件应存在
        assert!(path.with_extension("jsonl.bak").exists());
    }

    #[test]
    fn salvage_gate_blocks_dangling_reference() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("first"));
        s.append_message(&user("second"));
        s.append_message(&user("third"));
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);
        // 篡改：把第 2 条消息改成 seq=0（解析失败，可跳过），但第 3 条消息仍指向它的 id
        // → 后缀悬空引用该 id，依赖闸门应拦住抢救，就地截断
        let content = std::fs::read_to_string(&path).unwrap();
        let mut out = String::new();
        for l in content.lines() {
            if l.contains("\"text\":\"second\"") {
                let mut v: Value = serde_json::from_str(l).unwrap();
                v["seq"] = json!(0);
                out.push_str(&serde_json::to_string(&v).unwrap());
            } else {
                out.push_str(l);
            }
            out.push('\n');
        }
        std::fs::write(&path, out).unwrap();
        let plan = match Session::open_checked(&path.to_string_lossy()) {
            OpenSessionResult::Repairable { plan, .. } => plan,
            _ => panic!("expected Repairable"),
        };
        assert!(
            !plan.salvage_available(),
            "后缀依赖被丢弃的 id，抢救应被闸门拦下"
        );
        assert_eq!(plan.salvaged_skipped, 0);
        // 截断路径复原：只保留第 1 条
        Session::repair_write(&path.to_string_lossy(), &plan, false).unwrap();
        let reopened = Session::open(&path.to_string_lossy()).unwrap();
        let msgs = reopened.build_context_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text(), "first");
    }

    #[test]
    fn torn_tail_auto_truncates_keeps_valid_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("first"));
        s.append_message(&user("second"));
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);
        // 追加半截行（最后一行非法 JSON）：torn tail，应自动截断且保留有效前缀
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        use std::io::Write;
        writeln!(f, "{{\"kind\":\"entry\",\"lane\":\"main").unwrap();
        drop(f);
        let session = Session::open(&path.to_string_lossy()).unwrap();
        let msgs = session.build_context_messages();
        assert_eq!(
            msgs.len(),
            2,
            "torn tail 自动修复后两条消息都在（不再只留 header）"
        );
        // 磁盘上应已写回有效前缀（header + 前两条，无半截行）
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3);
        // 再次打开依然完整
        let reopened = Session::open(&path.to_string_lossy()).unwrap();
        assert_eq!(reopened.build_context_messages().len(), 2);
    }

    #[test]
    fn non_consecutive_seq_is_repaired_keeps_following_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("first"));
        s.append_session_info("myname"); // fact seq2：后面模拟该行写失败丢失
        s.append_message(&user("third")); // seq3，parent = first
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);
        // 模拟 persist 漏写：从磁盘删掉 fact 行（内存里它不参与任何引用链）
        let content = std::fs::read_to_string(&path).unwrap();
        let mut out = String::new();
        for l in content.lines() {
            if !l.contains("\"kind\":\"fact\"") {
                out.push_str(l);
                out.push('\n');
            }
        }
        std::fs::write(&path, out).unwrap();
        // third 的 seq 从 3 变“非连续”（期望 2）：内容自洽（parent=first 仍在磁盘），
        // 应就地修复 seq 而不是截断——third 必须保留。
        match Session::open_checked(&path.to_string_lossy()) {
            OpenSessionResult::Ok(opened) => {
                let msgs = opened.build_context_messages();
                assert_eq!(
                    msgs.len(),
                    2,
                    "seq 跳号行内容自洽：修复后 first+third 都保留，不截断"
                );
                assert_eq!(msgs[0].text(), "first");
                assert_eq!(msgs[1].text(), "third");
            }
            _ => panic!("seq 跳号（无引用断裂）应自动修复为 Ok，而不是 Repairable/截断"),
        }
        // 磁盘应已自动写回：seq 重新严格连续，且仍无 fact 行
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "header + first + third");
        let mut seqs: Vec<u64> = Vec::new();
        for l in &lines[1..] {
            let v: Value = serde_json::from_str(l).unwrap();
            seqs.push(v["seq"].as_u64().unwrap());
        }
        assert_eq!(seqs, vec![1, 2], "修复后磁盘 seq 必须严格连续");
        // 修复前源文件已备份
        assert!(path.with_extension("jsonl.bak").exists());
    }

    #[test]
    fn seq_fix_still_fails_on_dangling_parent_truncates() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("first"));
        s.append_message(&user("second"));
        s.append_message(&user("third"));
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);
        // 模拟整条 second 写失败丢失：third 的 seq 跳号且 parent=second 悬空。
        // 修 seq 后 third 仍链不上（apply 先报 does_not_chain）→ 引用断裂，只能截断到 first
        let content = std::fs::read_to_string(&path).unwrap();
        let mut out = String::new();
        for l in content.lines() {
            if !l.contains("\"text\":\"second\"") {
                out.push_str(l);
                out.push('\n');
            }
        }
        std::fs::write(&path, out).unwrap();
        let plan = match Session::open_checked(&path.to_string_lossy()) {
            OpenSessionResult::Repairable { plan, .. } => plan,
            _ => panic!("expected Repairable"),
        };
        assert!(
            plan.error.contains("does not chain"),
            "{}。修 seq 后真实损坏应报链条断裂而非 seq 错误",
            plan.error
        );
        Session::repair_write(&path.to_string_lossy(), &plan, false).unwrap();
        let reopened = Session::open(&path.to_string_lossy()).unwrap();
        let msgs = reopened.build_context_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text(), "first");
    }

    #[test]
    fn salvage_skips_multiple_garbage_lines_keeps_all() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("first"));
        s.append_message(&user("second"));
        s.append_message(&user("third"));
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);
        // 小文件旧 cap = min(16, 5*2%) 下限 1；新 cap 下限 8：插入两处垃圾行应全部跳过，
        // 三条消息完整保留（旧行为：只跳一处，第二处就地截断 → 丢失后续）。
        let content = std::fs::read_to_string(&path).unwrap();
        let mut out = String::new();
        let mut inserted = 0;
        for l in content.lines() {
            out.push_str(l);
            out.push('\n');
            if l.contains("\"text\":\"first\"") || l.contains("\"text\":\"second\"") {
                inserted += 1;
                out.push_str("{this is not valid json}");
                out.push('\n');
            }
        }
        assert_eq!(inserted, 2);
        std::fs::write(&path, out).unwrap();
        let plan = match Session::open_checked(&path.to_string_lossy()) {
            OpenSessionResult::Repairable { plan, .. } => plan,
            _ => panic!("expected Repairable"),
        };
        assert_eq!(plan.salvaged_skipped, 2, "两处垃圾行均应跳过");
        assert!(plan.salvage_available());
        Session::repair_write(&path.to_string_lossy(), &plan, true).unwrap();
        let reopened = Session::open(&path.to_string_lossy()).unwrap();
        let msgs = reopened.build_context_messages();
        assert_eq!(msgs.len(), 3, "抢救保留全部三条消息");
        assert_eq!(msgs[0].text(), "first");
        assert_eq!(msgs[1].text(), "second");
        assert_eq!(msgs[2].text(), "third");
    }

    #[test]
    fn salvage_skips_multiple_duplicate_ids_keeps_suffix() {
        // 真实场景：同毫秒两个工具调用撞出两个重复 record id（tool-{now_ms}），
        // 旧 cap=1 时第二个重复行触发 `skipped >= cap` 就地截断，后缀全部丢失；
        // 新 cap 下限 8：两个重复行都应跳过，末尾消息保留。
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("first"));
        s.append_lane_record(&session_v4::LaneRecord::OperationStarted {
            base: session_v4::RecordBase {
                id: "run-1".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 1,
            },
            source_leaf_id: None,
            intent: session_v4::OperationIntent::Run {
                original_prompt: vec![user("first")],
                initial_messages: vec![],
                system_prompt_override: None,
            },
        });
        s.append_lane_record(&session_v4::LaneRecord::StepAttempt {
            base: session_v4::RecordBase {
                id: "step-1".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 1,
            },
            run_id: "run-1".into(),
            step: "assistant".into(),
            attempt: 1,
            result_entry_id: "m-none".into(),
            compaction_reason: None,
        });
        s.append_lane_record(&session_v4::LaneRecord::OperationFinished {
            base: session_v4::RecordBase {
                id: "fin-1".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 1,
            },
            run_id: "run-1".into(),
            outcome: "completed".into(),
            error: None,
        });
        let last_id = s.append_message(&user("last"));
        let path = s.get_session_file().unwrap().to_path_buf();
        drop(s);
        // 篡改：step-1 与 fin-1 各插入一个同 id 重复行，seq 连续顺延（first=1 run=2 step=3
        // step'=4 fin=5 fin'=6 last=7）。旧 cap = min(16, 6*2%) 下限 1，只能跳过第一个。
        let content = std::fs::read_to_string(&path).unwrap();
        let mut out = String::new();
        for l in content.lines() {
            out.push_str(l);
            out.push('\n');
            if l.contains("\"id\":\"step-1\"") {
                let mut dup: Value = serde_json::from_str(l).unwrap();
                dup["seq"] = json!(4);
                out.push_str(&serde_json::to_string(&dup).unwrap());
                out.push('\n');
            } else if l.contains("\"id\":\"fin-1\"") {
                let mut dup: Value = serde_json::from_str(l).unwrap();
                dup["seq"] = json!(6);
                out.push_str(&serde_json::to_string(&dup).unwrap());
                out.push('\n');
            }
        }
        std::fs::write(&path, out).unwrap();
        let plan = match Session::open_checked(&path.to_string_lossy()) {
            OpenSessionResult::Repairable { plan, .. } => plan,
            _ => panic!("expected Repairable"),
        };
        assert_eq!(plan.salvaged_skipped, 2, "两个重复 id 都应跳过");
        assert!(plan.salvage_available());
        Session::repair_write(&path.to_string_lossy(), &plan, true).unwrap();
        let reopened = Session::open(&path.to_string_lossy()).unwrap();
        let entries: Vec<String> = reopened
            .state
            .entries()
            .iter()
            .map(|e| e.id().to_string())
            .collect();
        assert!(entries.contains(&last_id), "后缀消息必须保留：{entries:?}");
        assert!(plan.error.contains("contains duplicate id"));
    }

    #[test]
    fn lane_record_does_not_participate_in_tree() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        let id1 = s.append_message(&user("hi"));
        s.append_lane_record(&session_v4::LaneRecord::OperationStarted {
            base: session_v4::RecordBase {
                id: "run-1".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 1,
            },
            source_leaf_id: None,
            intent: session_v4::OperationIntent::Run {
                original_prompt: vec![user("hi")],
                initial_messages: vec![],
                system_prompt_override: None,
            },
        });
        // record 不改变 leaf
        assert_eq!(s.get_leaf_id(), Some(id1.as_str()));
        let msgs = s.build_context_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text(), "hi");
    }

    #[test]
    fn serialize_branch_jsonl_roundtrips_via_import() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        let mut s = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        // 空会话：只有 header + 空 lane 指针，不 panic
        let empty = s.serialize_branch_jsonl();
        assert!(
            empty
                .lines()
                .next()
                .unwrap()
                .contains("\"kind\":\"header\"")
        );
        assert!(
            !empty.contains("\"kind\":\"entry\""),
            "空分支不应有条目行: {empty}"
        );

        let id1 = s.append_message(&user("hi"));
        s.append_label_change(&id1, Some("keep")).unwrap();
        // 尾部是配置变更条目（非 message）：fork 会拒绝，这里仍应完整导出
        s.append_active_tools_change(&["read".to_string(), "bash".to_string()]);

        let jsonl = s.serialize_branch_jsonl();
        let out = dir.path().join("branch.jsonl");
        std::fs::write(&out, &jsonl).unwrap();
        let reopened = Session::open(out.to_str().unwrap()).unwrap();

        // 条目、标签与 lane 指针都被还原
        assert_eq!(
            reopened.get_entries().len(),
            s.get_entries().len(),
            "entry 数一致"
        );
        assert_eq!(reopened.get_leaf_id(), s.get_leaf_id());
        assert_eq!(reopened.build_context_messages().len(), 1);
        assert_eq!(
            find_label_in_tree(&reopened.get_tree(), &id1).as_deref(),
            Some("keep"),
            "label 应随分支导出"
        );
    }

    #[test]
    fn fork_tree_copies_entries_and_facts_not_records() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        let mut s = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        let id1 = s.append_message(&user("original"));
        s.append_message(&AgentMessage {
            role: "assistant".into(),
            thinking_level: None,
            ..AgentMessage::user_text("")
        });
        s.append_lane_record(&session_v4::LaneRecord::OperationStarted {
            base: session_v4::RecordBase {
                id: "run-1".into(),
                seq: 0,
                lane: "main".into(),
                timestamp: 1,
            },
            source_leaf_id: None,
            intent: session_v4::OperationIntent::Run {
                original_prompt: vec![],
                initial_messages: vec![],
                system_prompt_override: None,
            },
        });
        s.append_label_change(&id1, Some("keep")).unwrap();
        s.append_session_info("name of source");
        let path = s.get_session_file().unwrap().to_string_lossy().to_string();
        drop(s);

        let forked = Session::fork_from(&path, cwd, Some(sess_dir.clone())).unwrap();
        // entries + facts 复制；records 不复制
        assert_eq!(forked.state.records().len(), 0, "fork 不复制 records");
        assert_eq!(forked.name.as_deref(), Some("name of source"));
        let tree = forked.get_tree();
        let found = find_label_in_tree(&tree, &id1);
        assert_eq!(found.as_deref(), Some("keep"));
        let msgs = forked.build_context_messages();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].text(), "original");
    }

    #[test]
    fn fork_branch_requires_message_target() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        let mut s = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        s.append_active_tools_change(&["read".to_string()]);
        let atc_id = s.state.entries().last().unwrap().id().to_string();
        s.append_message(&user("hi"));
        let path = s.get_session_file().unwrap().to_string_lossy().to_string();
        drop(s);
        let err = Session::fork_at(&path, &atc_id, cwd, Some(sess_dir.clone()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("not a message entry"),
            "fork branch 目标必须是 message: {err}"
        );
    }

    #[test]
    fn compaction_context_is_summary_plus_retained_entries() {
        // pi `buildContextEntries`：上下文 = [最新 compaction] + 保留区间内的**原始条目** + 其后条目
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        let _old = s.append_message(&user("old"));
        let kept1 = s.append_message(&user("kept1"));
        let kept2 = AgentMessage {
            role: "assistant".into(),
            thinking_level: None,
            ..user("kept2")
        };
        s.append_message(&kept2);
        s.append_compaction("## Goal\nsum", 100, Some(&kept1), None, None);
        let msgs = s.build_context_messages();
        let texts: Vec<String> = msgs.iter().map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec![
                "## Goal\nsum".to_string(),
                "kept1".to_string(),
                "kept2".to_string()
            ]
        );
        // 保留的是原始条目：消息带回 entryId（resume / 编辑都靠它）
        assert_eq!(msgs[1].entry_id.as_deref(), Some(kept1.as_str()));
    }

    #[test]
    fn retain_none_compaction_self_references_first_kept_entry() {
        // pi `appendCompaction`：`firstKeptEntryId ?? id`——retain-none 自引用
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("old"));
        let cid = s.append_retain_none_compaction("sum", 10, None, None);
        let comp = s.state.get_entry(&cid).unwrap();
        match comp {
            Entry::Compaction {
                first_kept_entry_id,
                ..
            } => {
                assert_eq!(first_kept_entry_id.as_deref(), Some(cid.as_str()));
            }
            other => panic!("expected compaction, got {other:?}"),
        }
        let msgs = s.build_context_messages();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "compactionSummary");
    }

    /// 摘要类投影消息必须带上条目上的 usage：底栏的唯一数据源就是这批消息
    /// （不带就等于把摘要成本从会话成本里漏掉），
    #[test]
    fn projected_summaries_carry_entry_usage() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        let usage = Usage {
            input: 4_000,
            output: 200,
            total_tokens: 4_200,
            cost: crate::core::provider::Cost {
                total: 0.04,
                ..Default::default()
            },
            ..Default::default()
        };
        s.append_message(&user("old"));
        s.append_retain_none_compaction("sum", 10, None, Some(&usage));
        let msgs = s.build_context_messages();
        assert_eq!(msgs[0].role, "compactionSummary");
        assert_eq!(
            msgs[0].usage.as_ref().map(|u| u.cost.total),
            Some(0.04),
            "投影消息应携带压缩条目的 usage"
        );

        // 重开后仍能拿到（usage 来自条目，不依赖投影时的内存状态）
        let path = s.get_session_file().unwrap().to_string_lossy().to_string();
        let reopened = Session::open(&path).unwrap();
        let msgs = reopened.build_context_messages();
        assert_eq!(msgs[0].usage.as_ref().map(|u| u.cost.total), Some(0.04));
    }

    #[test]
    fn set_leaf_moves_lane_and_chain() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        let _id1 = s.append_message(&user("m1"));
        let id2 = s.append_message(&user("m2"));
        let _id3 = s.append_message(&user("m3"));
        // 回退到 m2，新消息挂 m2 下
        s.set_leaf(Some(&id2)).unwrap();
        let id4 = s.append_message(&user("m4"));
        assert_eq!(s.get_leaf_id(), Some(id4.as_str()));
        let e4 = s.state.get_entry(&id4).unwrap();
        let parent = match e4 {
            session_v4::Entry::Message { base, .. } => base.parent_id.clone(),
            _ => None,
        };
        assert_eq!(parent.as_deref(), Some(id2.as_str()));
        let msgs = s.build_context_messages();
        let texts: Vec<String> = msgs.iter().map(|m| m.text()).collect();
        assert_eq!(texts, vec!["m1", "m2", "m4"]);
    }

    /// 辅助：构造 assistant 消息（回退测试需要 user/assistant 交替）。
    fn assistant(text: &str) -> AgentMessage {
        let mut m = AgentMessage::user_text(text);
        m.role = "assistant".into();
        m
    }

    /// 回退最后一次用户输入：叶子移到最后一条 user 消息之前，它与其后的输出一起
    /// 离开活动分支；历史仍在树上，落盘后重新打开仍然一致。
    #[test]
    fn rewind_last_user_input_moves_leaf_before_last_user() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        let u1 = s.append_message(&user("first"));
        let _a1 = s.append_message(&assistant("a1"));
        let _u2 = s.append_message(&user("second"));
        let a2 = s.append_message(&assistant("a2"));
        let _u3 = s.append_message(&user("third"));
        let _a3 = s.append_message(&assistant("a3"));

        let removed = s.rewind_last_user_input().unwrap();
        assert_eq!(removed.as_deref(), Some("third"));
        assert_eq!(
            s.get_leaf_id(),
            Some(a2.as_str()),
            "叶子应回到 second 的回复"
        );

        let texts: Vec<String> = s
            .build_context_messages()
            .iter()
            .map(|m| m.text())
            .collect();
        assert_eq!(texts, vec!["first", "a1", "second", "a2"]);

        // 历史条目仍在树中（只移动了叶子指针）
        assert!(s.state.get_entry(&u1).is_some());
        assert!(s.state.get_entry(&_a3).is_some());

        // 落盘：重新打开后投影一致（lane mutation 已持久化）
        let path = s.get_session_file().unwrap().to_string_lossy().to_string();
        let reopened = Session::open(&path).unwrap();
        let reopened_texts: Vec<String> = reopened
            .build_context_messages()
            .iter()
            .map(|m| m.text())
            .collect();
        assert_eq!(reopened_texts, vec!["first", "a1", "second", "a2"]);
        assert_eq!(reopened.get_leaf_id(), Some(a2.as_str()));
    }

    /// 回退第一条（root）用户输入：清空活动分支，叶子变为 `None`。
    #[test]
    fn rewind_last_user_input_first_message_clears_branch() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        s.append_message(&user("only"));
        s.append_message(&assistant("reply"));
        assert_eq!(s.build_context_messages().len(), 2);

        let removed = s.rewind_last_user_input().unwrap();
        assert_eq!(removed.as_deref(), Some("only"));
        assert_eq!(s.get_leaf_id(), None, "回退第一条 user 消息应清空 lane");
        assert!(s.build_context_messages().is_empty());

        // 落盘后重新打开仍然为空
        let path = s.get_session_file().unwrap().to_string_lossy().to_string();
        let reopened = Session::open(&path).unwrap();
        assert_eq!(reopened.get_leaf_id(), None);
        assert!(reopened.build_context_messages().is_empty());
    }

    /// 分支上没有 user 消息（或空会话）：返回 `None`，叶子不变。
    #[test]
    fn rewind_last_user_input_without_user_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        assert_eq!(s.rewind_last_user_input().unwrap(), None);

        let a = s.append_message(&assistant("synthetic"));
        assert_eq!(s.rewind_last_user_input().unwrap(), None);
        assert_eq!(s.get_leaf_id(), Some(a.as_str()), "无变化时叶子不应移动");
        assert_eq!(s.build_context_messages().len(), 1);
    }

    #[test]
    fn name_and_label_are_facts_not_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        let id1 = s.append_message(&user("m"));
        s.append_session_info("display");
        s.append_label_change(&id1, Some("lbl")).unwrap();
        // fact 不进 entries 树
        let entries = s.get_entries();
        let entry_types: Vec<&str> = entries
            .iter()
            .filter_map(|e| e.get("type").and_then(|v| v.as_str()))
            .collect();
        assert!(!entry_types.contains(&"session_info"), "{entry_types:?}");
        assert!(!entry_types.contains(&"label"), "{entry_types:?}");
        // 文件行包含 kind=fact
        let text = std::fs::read_to_string(s.get_session_file().unwrap()).unwrap();
        let fact_lines: Vec<&str> = text
            .lines()
            .filter(|l| l.contains("\"kind\":\"fact\""))
            .collect();
        assert_eq!(fact_lines.len(), 2, "name + label 各一条 fact: {text}");
        // label 通过 get_tree 挂节点
        let tree = s.get_tree();
        let found = find_label_in_tree(&tree, &id1);
        assert_eq!(found.as_deref(), Some("lbl"));
    }

    fn find_label_in_tree(tree: &Value, target: &str) -> Option<String> {
        let nodes = tree.get("tree")?.as_array()?;
        fn walk(node: &Value, target: &str) -> Option<String> {
            let entry_id = node
                .pointer("/entry/id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if entry_id == target {
                return node
                    .get("label")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
            }
            for child in node
                .get("children")
                .and_then(|v| v.as_array())
                .into_iter()
                .flatten()
            {
                if let Some(l) = walk(child, target) {
                    return Some(l);
                }
            }
            None
        }
        nodes.iter().find_map(|n| walk(n, target))
    }

    trait ReplaceSuffixParent {
        fn replace_suffix_parent(&self) -> String;
    }
    impl ReplaceSuffixParent for &str {
        fn replace_suffix_parent(&self) -> String {
            self.to_string()
        }
    }

    #[test]
    fn fork_keeps_compaction_boundary() {
        // 对齐 pi 0.85.0（2631b25c3）：fork 不得丢失 compaction boundary。
        // pi 式 firstKeptEntryId 是**条目 id 指针**，fork 必须把保留区间内的原始条目一起复制，
        // 否则 fork 后投影会丢掉保留尾部（pi 的悬空指针问题）。
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        let mut s = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        let _old = s.append_message(&user("old"));
        let kept = s.append_message(&user("kept-tail"));
        s.append_compaction("## Goal\nsum", 100, Some(&kept), None, None);
        let id_new = s.append_message(&user("after-compaction"));
        // pi 修复场景：compaction 后条目打标签，fork 过滤 label 时不得影响 boundary
        s.append_label_change(&id_new, Some("keep")).unwrap();
        let path = s.get_session_file().unwrap().to_string_lossy().to_string();
        drop(s);

        let forked = Session::fork_from(&path, cwd, Some(sess_dir.clone())).unwrap();
        let msgs = forked.build_context_messages();
        let texts: Vec<String> = msgs.iter().map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec![
                "## Goal\nsum".to_string(),
                "kept-tail".to_string(),
                "after-compaction".to_string()
            ],
            "fork 后上下文必须以 compaction summary + retained tail + 后续消息开头"
        );
    }

    #[test]
    fn fork_branch_keeps_compaction_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_str().unwrap();
        let sess_dir = dir.path().join("sessions");
        let mut s = Session::create(cwd, Some(sess_dir.clone()), true).unwrap();
        let _old = s.append_message(&user("old"));
        let kept = s.append_message(&user("kept-tail"));
        s.append_compaction("## Goal\nsum", 100, Some(&kept), None, None);
        let target = s.append_message(&user("after-compaction"));
        let path = s.get_session_file().unwrap().to_string_lossy().to_string();
        drop(s);

        // branch fork：目标在 compaction 之后，路径应包含 compaction → boundary 保留
        let forked = Session::fork_at(&path, &target, cwd, Some(sess_dir.clone())).unwrap();
        let msgs = forked.build_context_messages();
        let texts: Vec<String> = msgs.iter().map(|m| m.text()).collect();
        assert_eq!(
            texts,
            vec![
                "## Goal\nsum".to_string(),
                "kept-tail".to_string(),
                "after-compaction".to_string()
            ],
            "branch fork 后 compaction boundary 必须保留"
        );
    }

    /// pi `appendUsage("cache_warm", …)`：保温用量落成 `cause = cache_warm` 的记录，
    /// 并计入会话统计（cached/uncached/total/cost），带上 provider/model 供成本分列归集。
    #[test]
    fn cache_warm_usage_is_recorded_and_counted_in_stats() {
        let dir = tempfile::tempdir().unwrap();
        let mut s = session_at(dir.path());
        let usage = Usage {
            input: 10,
            output: 1,
            cache_read: 1_000,
            cache_write: 2_000,
            total_tokens: 3_011,
            cost: crate::core::provider::Cost {
                total: 0.030015,
                ..Default::default()
            },
            ..Default::default()
        };
        s.append_cache_warm_usage(&usage, "anthropic", "claude-sonnet-5-5");

        let record = s
            .get_entries()
            .into_iter()
            .find(|e| e["type"] == "usage_record")
            .expect("usage_record 应出现在 get_entries 里");
        assert_eq!(record["cause"], "cache_warm");
        assert_eq!(record["provider"], "anthropic");
        assert_eq!(record["model"], "claude-sonnet-5-5");

        let stats = s.state.get_stats();
        assert_eq!(stats.cached_tokens, 1_000);
        assert_eq!(stats.uncached_tokens, 2_010);
        assert_eq!(stats.total_tokens, 3_011);
        assert!((stats.cost_total - 0.030015).abs() < 1e-12);

        // 落盘为 kind=record 且 cause=cache_warm
        let text = std::fs::read_to_string(s.get_session_file().unwrap()).unwrap();
        assert!(
            text.lines().any(|l| {
                let v: Value = serde_json::from_str(l).unwrap_or(Value::Null);
                v["kind"] == "record" && v["cause"] == "cache_warm"
            }),
            "{text}"
        );
    }
}

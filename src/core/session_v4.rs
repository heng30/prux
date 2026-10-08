//! v4 会话存储核心：record-log 格式 + 归约器
//!
//! # 为什么 session 设计成当前这种格式？
//!
//! v4 采用**追加式 record-log（事件溯源）+ 归约器（reducer）** 的设计：磁盘上只保存
//! 一串不可变的 JSONL mutation（`SessionMutation`），而运行期需要的所有"编排状态"
//! （当前打开的 operation、待执行的队列、tool 批次、是否已终止失败……）**不单独持久化**，
//! 而是由归约器（`reduce_lane_state`）在需要时从日志里**重新推导**出来。
//!
//! ## 1. 单一事实来源：日志即真相
//! 内存里的运行状态不是一等公民，而是日志的投影（projection）。这意味着：
//! - **崩溃可恢复（resume）**：任何时刻都能从日志重建完整状态，进程被杀 / 断电后
//!   无需额外的"运行状态文件"就能无缝续跑，也不存在日志与运行状态分叉的隐患。
//! - **可审计、可重放**：日志是只追加的，且每条 mutation 都带严格递增的 `seq`，
//!   历史即真相，可逐行回放、差分、导出。
//!
//! ## 2. 写路径严格校验，坏数据显式报错
//! 所有变更只能经 `SessionState::apply_mutation` 顺序写入，并在写前校验：
//! `seq` 严格连续、`id` 全局唯一、entry 必须通过 `parent_id` 链到 lane 叶子、引用必须存在。
//! 归约前的 `validate_record_log` 再做结构校验（attempt 是否跳号、
//! tool 调用是否匹配 assistant 序号、队列取消是否合法……）。
//! 这样**损坏会被显式检测并报错**（`RecordLogError`），而不是静默错乱，
//! 保证了恢复结果的可靠性。
//!
//! 例外：同一 lane 存在多个未关闭的 operation **不视为损坏**——单写者架构下它只可能是
//! 崩溃残留（上一次运行的 `operation_finished` 未落盘），归约器降级为"取最新 operation
//! 继续、旧 operation 视为已中断（僵尸）"，而不是拒绝恢复（否则一次崩溃会永久锁死会话）。
//!
//! ## 3. entry 树 + lane 指针支撑分支（branch / fork）
//! - **entry** 是持久化转写条目，通过 `parent_id` 构成**树**，而非单一线性列表；
//! - **lane** 只是"指向某条分支叶子"的轻量指针，本身不复制数据；
//! - 因此 **fork** 只需复制 entry 子集 + 重设 lane 指针（`create_fork_mutations`），
//!   天然支持分支导航、回退，以及对被放弃分支生成 `branch_summary`，成本极低。
//!
//! ## 4. record 记录"运行编排过程"，而非只记结果
//! 一次运行被建模为**操作（operation）**：`operation_started` → 若干
//! `step_attempt` / `tool_started` / `queue_enqueued` → `operation_finished`。
//! 工具调用、队列（steer / followUp / nextRun）、延迟写（write_deferred）、usage 都以
//! record 落盘，使"进行到哪一步、还有哪些没执行、当前 tool 批次如何"都能被归约器还原，
//! 即便中途崩溃也丢失不了进度。
//!
//! ## 5. 归约器把 bounded 切片还原为 LaneState
//! `reduce_lane_state` 吃进**有界（bounded）** 的 record + entry 切片（按 lane 分片），
//! 输出可操作的 `LaneState`（open operation、pending step、tool batch、pending 队列、
//! 有效配置、terminal failure 等）。切片有界使归约成本可控，不随会话历史无限增长；
//! 归约是**纯函数**（无副作用），极易单测，这也体现在本文件的测试集中。
//!
//! ## 好处汇总
//! 崩溃恢复、可审计 / 可重放、坏数据显式报错、fork / 分支天然低成本、纯函数归约可测、
//! 单写者顺序一致、通过版本化 header（`JsonlV4Header`）向前兼容。
//!
//! # Entry / LaneRecord / SessionMutation 三者的关系
//!
//! 三者是**同一份日志的三个层次**（信封 / 内容 / 编排）。磁盘上只有一行行
//! `SessionMutation`，每行恰好承载其中一层的一次变化：
//!
//! ```text
//!   磁盘（JSONL，只追加）
//!   ┌────────────────────────────────────────────────────────────────┐
//!   │ {"kind":"entry",  "lane":…, …Entry 展开…}        ← 内容层载荷  │
//!   │ {"kind":"record", …LaneRecord 展开…}             ← 编排层载荷  │
//!   │ {"kind":"lane",   "lane":…, "leafId":…}         ← 只改指针     │
//!   │ {"kind":"fact",   "fact":"name"|"label", …}     ← 只改元数据   │
//!   └────────────────────────────────────────────────────────────────┘
//!             │ parse_mutation              │ reduce_lane_state
//!             ▼                             ▼
//!     SessionMutation ──▶ Entry（树）/ LaneRecord（轨迹） ──▶ LaneState
//! ```
//!
//! - **`SessionMutation`：传输 / 落盘信封（"这一行是什么"）**。
//!   它是**唯一**被 `encode_mutation` / `parse_mutation` 处理、也唯一出现在磁盘上的类型，
//!   用 `kind` 区分四种行：`entry` / `record` / `lane` / `fact`。
//!   其中 `entry` 与 `record` 直接 `flatten` 内层类型（`Entry`、`LaneRecord`），
//!   另外两种只改指针与元数据（重设 lane 叶子、写会话名 / label），
//!   **不产生任何 Entry 或 Record**。也就是说 `Entry` / `LaneRecord` 没有独立的"行身份"，
//!   它们是 `SessionMutation` 的载荷；反之 `lane` / `fact` 行没有对应的内层类型。
//!
//! - **`Entry`：内容层（"会话里说了什么"）**。
//!   消息、模型 / 推理级别 / 工具集变更、压缩、分支摘要、插件自定义数据。
//!   它只描述**内容事实**，通过 `parentId` 组成一棵 **entry 树**（分支的物理形态），并带 `seq`。
//!   Entry **从不引用** `LaneRecord`——依赖方向单向：编排指向内容，内容不知道编排。
//!
//! - **`LaneRecord`：编排层（"运行过程中做了什么"）**。
//!   隶属于某条 `lane`，描述一次 operation 的开始 / 中止 / 结束、step 尝试、工具发起、
//!   队列入队 / 取消、延迟写、usage。它总是以 **id 引用 Entry**
//!   （`assistantEntryId` / `resultEntryId` / `target.id` / `entryId`…），
//!   把"运行走到哪一步"钉在"哪条内容"上；但**不携带内容本身**，
//!   所以崩溃后只要 Entry 还在，运行轨迹就能重新对上号。
//!
//! 二者靠两点维系一致性：
//! 1. **全局统一的 `seq`**：`EntryBase.seq`、`RecordBase.seq`、`SessionMutation::Lane.seq`、
//!    `SessionMutation::Fact.seq` 共用同一个全局单调整数，因此 entry 与 record 可以按同一把
//!    时间尺合并排序（归约器正是这么做的）；
//! 2. **引用完整性**：写路径（`SessionState::apply_mutation`）与结构校验
//!    （`validate_record_log`）共同保证 record 引用的 entry / operation / lane 真实存在，
//!    且 `ProvisionedEntry`（意图）与最终落地的 Entry 内容一致。
//!
//! 依赖方向一句话：
//! `SessionMutation`（信封）承载 `Entry`（内容树）与 `LaneRecord`（编排轨迹）；
//! `LaneRecord` 按 id 引用 `Entry`；`reduce_lane_state` 消费两者，产出 `LaneState`。
//!
//! # `reduce_lane_state` 详解
//!
//! 它是"日志 → 运行状态"的唯一投影函数：纯函数、无副作用，可重复调用、易单测。
//! 输入是**有界的** lane 分片，输出是可直接驱动下一步动作的编排状态。
//!
//! ```text
//!   LaneReductionInput
//!     │
//!     ├─(1) validate_record_log        结构校验，损坏即 Err（拒绝归约）
//!     ├─(2) 按 seq 排序 + 建 id→entry 索引
//!     ├─(3) 算 pending 队列 / effective_configuration
//!     ├─(4) 选"最新的 open operation"（多 open 时旧的视为僵尸）
//!     ├─(5) 逐项推导 pending_steer / follow_up / writes / missing_initial_messages
//!     ├─(6) 还原 step（结果未落地则 pending）与 tool_batch
//!     ├─(7) 判定 overflow_recovery_used 与 terminal_failure
//!     ▼
//!   LaneReductionResult { lane_state, effective_configuration, terminal_failure }
//! ```
//!
//! **输入（`LaneReductionInput`）**：全部是**有界切片**，由调用方（session_manager）
//! 按 lane 分片给出，因此归约成本不随会话总历史增长：
//! `open_operations`（该 lane 未关闭的 start 记录）、`records`（该 lane 的运行记录）、
//! `entries`（跨 lane 参与校验的条目）、`own_entries`（本 lane 拥有的条目，用于判 pending）、
//! `configuration_entries`（推导有效配置用的历史条目）、`defaults`（配置推导起点）。
//!
//! **各步骤在做什么**：
//!
//! 1. **校验（步骤 1）**：`validate_record_log` 检查 operation 引用是否存在、finish/abort 之后
//!    是否还有记录、step 的 attempt 是否严格递增、ToolStarted 是否与 assistant 的 tool_call
//!    序号匹配、队列取消是否对应一次未落地的入队、`ProvisionedEntry` 与已落地 Entry 是否
//!    内容一致。**任何违规直接 `Err`**，不做"尽力恢复"。
//!    特例：同一 lane 有多个未关闭 operation **不算损坏**（单写者下只能是崩溃残留），
//!    不在本层报错。
//!
//! 2. **排序与索引（步骤 2）**：entry 与 record 各自的切片按 `seq` 升序排列（`by_sequence`），
//!    并建立 `id → Entry` 索引。后续所有"某条内容是否已落地"的判断都退化为一次 map 查询。
//!
//! 3. **队列与配置（步骤 3）**：
//!    - "仍 pending 的入队" = 存在 `QueueEnqueued` **且** 目标 id 尚未出现在 entry 索引中
//!      **且** 未被 `QueueCancelled` 取消（\`cancelled\` 集合先行收集）；
//!    - 其中 `queue == "nextRun"` 的归入 `pending_next_run`；
//!    - `derive_effective_configuration` 按 seq 顺序把 `ModelChange` / `ThinkingLevelChange` /
//!      `ActiveToolsChange` 以及 assistant 消息携带的 provider/model 叠加到 `defaults` 上，
//!      得到"当前有效配置"。
//!
//! 4. **选 operation（步骤 4）**：从 `open_operations` 里取 `seq` 最大的一条作为当前 operation。
//!    若存在多条（崩溃残留），更早的视为已中断（僵尸）而不回归约——这是把
//!    "一次崩溃"降级为"重跑一次"、而非永久锁死会话的关键。若一条都没有，
//!    lane 视为空闲：立即返回 `operation: None` + `pending_next_run`。
//!
//! 5. **推导待执行项（步骤 5）**：先按 `runId == 当前 operation id` 过滤出 `operation_records`，
//!    然后分别求出：
//!    - `aborting`：是否出现过 `AbortRequested`（中止后 steer / followUp 一律清空）；
//!    - `pending_steer` / `pending_follow_up`：属于本次 operation、已入队但未落地、未取消的目标；
//!    - `pending_writes`：`WriteDeferred` 声明过、但目标 Entry 尚未落地的延迟写；
//!    - `missing_initial_messages`：`Run` 意图里预置、但尚未落地的初始消息（需补齐）。
//!
//! 6. **step 与 tool_batch（步骤 6）**：
//!    - `step`：取最新一条 `StepAttempt`；若其 `resultEntryId` **已落地**则该步已完成（`None`），
//!      否则输出 `StepState`，表示"这一步需要重放 / 重试"——这是断点续跑的核心信号。
//!    - `tool_batch`：找最新一条**含 tool_call 的 assistant 条目**，把它的 tool_call
//!      按序号与 `ToolStarted` 记录对齐；每个调用的结果优先取 `ToolStarted.resultEntryId`，
//!      退而求其次找 seq 更大、`tool_call_id` 相同且未被延迟写声明的 `toolResult`。
//!      任何调用没有结果即置 `unresolved = true`；`stop_reason == "length"` 则标记 `truncated`。
//!
//! 7. **状态判定（步骤 7）**：
//!    - `overflow_recovery_used`：是否存在一次 `compaction(reason=overflow)` 的 attempt，
//!      且其 `seq` 晚于"已消费条目"（Run 的 initial_messages + 非 nextRun 的入队目标）中最大的 seq
//!      ——用于判断本轮是否已经因上下文溢出压缩过；
//!    - `terminal_failure`：最新 own 条目是 `stop_reason == "error"` 的 assistant 消息，
//!      **且**它确实由本 operation 的 `StepAttempt` 或 `deferred_fetch` 产生（排除"只是延迟写
//!      还没完成"的假阳性），才认定为必须上报的终结失败，并标注 `source`（step / deferred_fetch）。
//!
//! **输出（`LaneReductionResult`）**：
//! - `lane_state`：leaf 指针 + operation（`None` 表示空闲）+ `pending_next_run`；
//! - `effective_configuration`：当前 provider / model / thinking level / 活动工具集；
//! - `terminal_failure`：需要上报的终结失败（或 `None`）。
//!
//! 值得注意的是：归约结果**不含任何可变句柄**（tool 的 deferred handle 只在校验时检查存在性），
//! 因此同一份输入永远得到同一份输出；调用方可以在任何时刻重新归约，用结果覆盖内存状态。
//!
//! # 本文件结构
//! - **数据类型**：`Entry`（转写条目）/ `LaneRecord`（运行记录）/ `SessionMutation`（磁盘行）/ 编解码；
//! - **校验**：`validate_record_log` 及一系列 `validate_*` 子过程；
//! - **归约**：`reduce_lane_state` 及 `derive_*` 辅助；
//! - **状态机**：`SessionState`（内存投影 + 写路径 `apply_mutation`）+ fork；
//! - **测试**：文件尾部 `#[cfg(test)] mod tests`。

use crate::core::provider::{AgentMessage, ContentBlock, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};

/// 归约器/结构校验错误。用 thiserror 枚举承载，分三类：
/// - **D 档**（引用断裂 / 序列缺口）：`session_manager` 抢救时命中即就地截断，
///   跳过该行也救不回来（后缀依赖缺失的 state）；
/// - **归约损坏**：日志不一致（重复打开、attempt 跳号、tool 调用不匹配……）；
/// - **结构错误**：单行 JSON 的浅层结构/类型不合法（解析与载荷校验），
///   与 corruption 语义不同，Display 不带 `corruption<...>` 前缀。
#[derive(Debug, thiserror::Error)]
pub enum RecordLogError {
    // ---- D 档：引用断裂 / 序列缺口，就地截断 ----
    /// 日志序号出现缺口，后续行无法链式推进，抢救时就地截断。
    #[error("corruption<non_consecutive_seq>: {message}")]
    NonConsecutiveSeq { message: String },
    /// 记录或条目引用了日志中从未出现过的 lane。
    #[error("corruption<references_missing_lane>: {message}")]
    MissingLane { message: String },
    /// lane 的叶子指针指向了一个不存在的条目。
    #[error("corruption<references_missing_lane_target>: {message}")]
    MissingLaneTarget { message: String },
    /// 条目的 parentId 指向不存在的父节点，分支树断裂。
    #[error("corruption<references_missing_parent>: {message}")]
    MissingParent { message: String },
    /// 条目未接在该 lane 当前叶子之下，破坏链式推进。
    #[error("corruption<does_not_chain_to_lane_leaf>: {message}")]
    DoesNotChainToLaneLeaf { message: String },
    /// label 事实指向的目标条目不存在，标签无处挂靠。
    #[error("corruption<references_missing_label_target>: {message}")]
    MissingLabelTarget { message: String },

    // ---- 归约损坏 / 结构不一致 ----
    // 注意：multiple_open_operations 已不再是错误——单写者架构下同一 lane 出现
    // 多个未关闭的 operation 只可能是崩溃残留（operation_finished 丢失），
    // 归约器按"取最新、旧 op 视为僵尸"降级处理，而不是拒绝恢复。
    /// 记录引用了没有对应 operation_started 的 runId。
    #[error("corruption<unknown_operation>: {message}")]
    UnknownOperation { message: String },
    /// 该记录出现在所属 operation 已 finish 之后。
    #[error("corruption<record_after_finish>: {message}")]
    RecordAfterFinish { message: String },
    /// 同一 step 的 attempt 序号跳号，未满足严格递增。
    #[error("corruption<non_consecutive_attempt>: {message}")]
    NonConsecutiveAttempt { message: String },
    /// compaction 步骤缺少合法 reason，或非压缩步骤误带 reason。
    #[error("corruption<invalid_compaction_reason>: {message}")]
    InvalidCompactionReason { message: String },
    /// 非 nextRun 的队列项在 operation 被 abort 之后才入队。
    #[error("corruption<queue_after_abort>: {message}")]
    QueueAfterAbort { message: String },
    /// 队列取消找不到此前未落地的对应入队项。
    #[error("corruption<invalid_queue_cancellation>: {message}")]
    InvalidQueueCancellation { message: String },
    /// 同 step 的连续 attempt 在结果条目或压缩原因上不一致。
    #[error("corruption<inconsistent_step>: {message}")]
    InconsistentStep { message: String },
    /// 工具发起记录与其 assistant 条目的 tool_call 序号或名称不符。
    #[error("corruption<tool_call_mismatch>: {message}")]
    ToolCallMismatch { message: String },
    /// 同一 assistant 条目的同一 tool 序号被重复发起。
    #[error("corruption<duplicate_tool_invocation>: {message}")]
    DuplicateToolInvocation { message: String },
    /// 已落地的预置条目内容与当初声明的意图不一致。
    #[error("corruption<provisioned_entry_mismatch>: {message}")]
    ProvisionedEntryMismatch { message: String },
    /// stop_reason 为 deferred 的 assistant 条目缺少 deferred handle。
    #[error("corruption<invalid_deferred_handle>: {message}")]
    InvalidDeferredHandle { message: String },

    // ---- 非 D 档的引用 / 结构问题 ----
    /// 条目或记录的 id 与已出现过的 id 重复。
    #[error("corruption<duplicate_id>: {message}")]
    DuplicateId { message: String },
    /// 请求访问的 lane 在会话状态中不存在。
    #[error("corruption<lane_not_found>: {message}")]
    LaneNotFound { message: String },
    /// 引用的条目 id 在会话状态中不存在。
    #[error("corruption<entry_not_found>: {message}")]
    EntryNotFound { message: String },
    /// 沿 parentId 回溯时检测到环，分支树非法。
    #[error("corruption<cycle>: {message}")]
    Cycle { message: String },
    /// fork 的目标条目不是消息类型，无法据此分叉。
    #[error("corruption<fork_target_not_message>: {message}")]
    ForkTargetNotMessage { message: String },

    // ---- 结构错误（解析 / 载荷校验，非 corruption，无前缀）----
    /// 该行不是合法 JSON，无法解析。
    #[error("{message}")]
    InvalidJson { message: String },
    /// 该行是合法 JSON，但顶层不是对象。
    #[error("{message}")]
    NotJsonObject { message: String },
    /// mutation 缺少合法的 kind 字段。
    #[error("{message}")]
    InvalidKind { message: String },
    /// entry 载荷反序列化失败或字段不合法。
    #[error("{message}")]
    InvalidEntry { message: String },
    /// record 载荷反序列化失败或字段不合法。
    #[error("{message}")]
    InvalidRecord { message: String },
    /// lane 载荷反序列化失败或字段不合法。
    #[error("{message}")]
    InvalidLane { message: String },
    /// fact 载荷反序列化失败或字段不合法。
    #[error("{message}")]
    InvalidFact { message: String },
    /// fact 类型不是已知的 name 或 label。
    #[error("{message}")]
    UnknownFactType { message: String },
    /// kind 不是已知的 mutation 类型。
    #[error("{message}")]
    UnknownMutationKind { message: String },
    /// seq 取值为 0 等非法值（要求从 1 起）。
    #[error("{message}")]
    InvalidSeq { message: String },
    /// 时间戳为负值，超出合法范围。
    #[error("{message}")]
    InvalidTimestamp { message: String },
    /// Custom 条目的 customType 为空字符串。
    #[error("{message}")]
    InvalidCustomType { message: String },
}

/// 便捷构造：包装一个归约器/结构错误。
fn corrupt<T>(e: RecordLogError) -> Result<T, RecordLogError> {
    Err(e)
}

/// 条目基字段：所有 Entry 变体共享的公共字段。
/// - `id`：全局唯一标识（同时是树节点 id）；
/// - `seq`：全局单调递增序号（写路径要求严格连续）；
/// - `timestamp`：Unix 毫秒时间戳；
/// - `parent_id`：树中的父节点（None/空表示根），构成分支树。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryBase {
    /// 全局唯一标识，同时用作分支树的节点 id。
    pub id: String,
    /// 全局单调递增序号，写路径要求严格连续。
    pub seq: u64,
    /// Unix 毫秒时间戳
    pub timestamp: i64,
    /// 分支树中的父节点 id；`None` 表示根节点。
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "parentId")]
    pub parent_id: Option<String>,
}

/// 持久化转写条目：会话里"发生了的事"。
/// 通过 `#[serde(tag = "type")]` 以 `type` 字段区分变体（snake_case）。
/// 每个变体都 `flatten` 了 `EntryBase`，因此都有 id/seq/timestamp/parentId。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum Entry {
    /// 一条消息（user / assistant / toolResult / system……）
    Message {
        /// 所有变体共享的元数据：全局 id、序号、时间戳与父节点 id。
        #[serde(flatten)]
        base: EntryBase,
        /// 消息本体：role / content（含 tool_call、thinking 块）/ provider / model /
        /// stop_reason / tool_call_id / tool_name / deferred handle 等。
        message: AgentMessage,
        /// 工具结果是否请求终止本轮循环（仅 toolResult 有意义）。
        /// `Some(true)` 表示结果要求立刻结束运行，不再继续下一步。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminate: Option<bool>,
    },
    /// 模型切换
    ModelChange {
        /// 所有变体共享的元数据：全局 id、序号、时间戳与父节点 id。
        #[serde(flatten)]
        base: EntryBase,
        /// 新的 provider 名（如 anthropic / openai）。
        provider: String,
        /// 新 provider 下的模型 id。
        #[serde(rename = "modelId")]
        model_id: String,
    },
    /// 推理级别切换（off/low/medium/high……）
    ThinkingLevelChange {
        /// 所有变体共享的元数据：全局 id、序号、时间戳与父节点 id。
        #[serde(flatten)]
        base: EntryBase,
        /// 新的推理级别字符串（取值由上层约定，本层不校验枚举）。
        #[serde(rename = "thinkingLevel")]
        thinking_level: String,
    },
    /// 活动工具集变更
    ActiveToolsChange {
        /// 所有变体共享的元数据：全局 id、序号、时间戳与父节点 id。
        #[serde(flatten)]
        base: EntryBase,
        /// 变更后生效的工具名全量列表（是全量快照，不是增量）。
        #[serde(rename = "activeToolNames")]
        active_tool_names: Vec<String>,
    },
    /// 上下文压缩（compaction）：记录摘要与**保留尾部起点**
    ///
    /// 投影时把该条目（含）到本压缩条目（不含）之间的路径条目原样带进模型上下文
    /// （见 `session_manager::Session::build_context_projection`）。
    Compaction {
        /// 所有变体共享的元数据：全局 id、序号、时间戳与父节点 id。
        #[serde(flatten)]
        base: EntryBase,
        /// 压缩产生的摘要文本，替代被压缩掉的历史。
        summary: String,
        /// 保留尾部起点条目 id
        /// `None` / 自引用（等于本条目 id）= 不保留任何前置条目（retain-none）。
        #[serde(
            default,
            rename = "firstKeptEntryId",
            skip_serializing_if = "Option::is_none"
        )]
        first_kept_entry_id: Option<String>,
        /// 压缩前的 token 数，用于统计节省量与阈值判断。
        #[serde(rename = "tokensBefore")]
        tokens_before: u64,
        /// provider 特有的额外信息（如摘要用到的分段结构），本层不解释。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        /// 本次压缩调用消耗的 token / 成本。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// 分支摘要：从某分支切走时，为被放弃分支生成的总结
    BranchSummary {
        /// 所有变体共享的元数据：全局 id、序号、时间戳与父节点 id。
        #[serde(flatten)]
        base: EntryBase,
        /// 被放弃分支上"切走点"的条目 id（摘要覆盖的起点）。
        #[serde(rename = "fromId")]
        from_id: String,
        /// 总结文本，用于把被放弃分支的上下文带进新分支。
        summary: String,
        /// provider 特有的额外信息。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<Value>,
        /// 生成摘要消耗的 token / 成本。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
    },
    /// **append-only 上下文编辑**：在不改写历史的前提下，改变某条**更早条目**对**模型上下文**的贡献
    ///
    /// - `replacement = None`（JSON `null`）：在模型上下文中**省略**目标条目
    ///   （原始 transcript 不受影响，仍可被 UI / `/bug` / fork 完整看到）；
    /// - `replacement = Some(_)`：只替换目标条目的 `content`，保留其余元数据
    ///   （role / provider / model / stopReason / toolCallId ……）。
    ///
    /// 本条目自身不产生任何模型消息。投影规则见 `session_manager::Session::build_context_messages`：
    /// 只在「当前分支的上下文条目集合」内生效——同一目标的多次编辑以**最新**为准，被压缩摘要吞掉的目标不受影响。
    ContextEdit {
        /// 所有变体共享的元数据：全局 id、序号、时间戳与父节点 id。
        #[serde(flatten)]
        base: EntryBase,
        /// 被编辑的目标条目 id（必须在本分支上，且是 user/assistant/toolResult 消息条目）。
        #[serde(rename = "targetId")]
        target_id: String,
        /// `null` = 省略目标；非 null = 只替换其内容。
        replacement: Option<ContextEditReplacement>,
    },
    /// 自定义扩展条目（插件可自由使用）
    Custom {
        /// 所有变体共享的元数据：全局 id、序号、时间戳与父节点 id。
        #[serde(flatten)]
        base: EntryBase,
        /// 自定义类型标签（非空，用于区分插件自己的多种条目）。
        #[serde(rename = "customType")]
        custom_type: String,
        /// 插件自定义载荷，本层不解释也不校验其形状。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        data: Option<Value>,
    },
}

/// 上下文编辑的替换内容。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextEditReplacement {
    /// 替换后的消息内容（可为空数组 = 该消息在上下文中变空）。
    pub content: Vec<ContentBlock>,
}

impl Entry {
    /// 条目的全局唯一 id（所有变体共享 `EntryBase` 里的同一个字段）。
    pub fn id(&self) -> &str {
        match self {
            Entry::Message { base, .. }
            | Entry::ModelChange { base, .. }
            | Entry::ThinkingLevelChange { base, .. }
            | Entry::ActiveToolsChange { base, .. }
            | Entry::Compaction { base, .. }
            | Entry::BranchSummary { base, .. }
            | Entry::ContextEdit { base, .. }
            | Entry::Custom { base, .. } => &base.id,
        }
    }

    /// 条目的全局序号（写入顺序；与 record 共用同一把计数器）。
    pub fn seq(&self) -> u64 {
        match self {
            Entry::Message { base, .. }
            | Entry::ModelChange { base, .. }
            | Entry::ThinkingLevelChange { base, .. }
            | Entry::ActiveToolsChange { base, .. }
            | Entry::Compaction { base, .. }
            | Entry::BranchSummary { base, .. }
            | Entry::ContextEdit { base, .. }
            | Entry::Custom { base, .. } => base.seq,
        }
    }

    /// 本条目是否为「上下文编辑」（append-only 投影指令，不产生模型消息）。
    pub fn is_context_edit(&self) -> bool {
        matches!(self, Entry::ContextEdit { .. })
    }

    /// 上下文编辑的目标条目 id（非 context_edit 返回 `None`）。
    pub fn context_edit_target(&self) -> Option<&str> {
        match self {
            Entry::ContextEdit { target_id, .. } => Some(target_id),
            _ => None,
        }
    }

    /// 载荷部分（不含 parentId/seq/timestamp）——用于 ProvisionedEntry 深度相等比较。
    /// 序列化后剥离元数据字段，只留下"业务内容"，供意图与已落地条目做比对。
    pub fn payload_json(&self) -> Value {
        let v = serde_json::to_value(self).unwrap_or(Value::Null);
        let mut m = match v {
            Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        m.remove("parentId");
        m.remove("seq");
        m.remove("timestamp");
        Value::Object(m)
    }
}

/// ProvisionedEntry：尚未落地的"目标条目"（intent 里的预置条目）。
/// 运行开始 / 入队 / 延迟写时会先记录一个"意图"（id + 期望的 payload），
/// 之后真正的 entry 落地时再比对 payload 是否一致（`matches_entry`）。
/// 这样即使崩溃，也能知道"这条原本打算写什么、写没写成"。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvisionedEntry {
    /// 未来落地条目的 id（意图与落地共享同一个 id，以此关联）。
    pub id: String,
    /// 期望的条目内容（含 `type` 等字段，但不含 seq/timestamp/parentId）。
    #[serde(flatten)]
    pub payload: Value,
}

impl ProvisionedEntry {
    /// 返回意图载荷的拷贝（不含 seq/timestamp/parentId 等元数据），
    /// 供与已落地条目比对业务内容。
    pub fn payload_json(&self) -> Value {
        self.payload.clone()
    }

    /// 与已落地条目做深度相等比较（payload 部分）。
    /// 比较时忽略 id/type 等结构性字段，只比业务内容。
    pub fn matches_entry(&self, entry: &Entry) -> bool {
        let mut ours = self.payload.clone();
        let mut theirs = entry.payload_json();
        if let (Value::Object(a), Value::Object(b)) = (&mut ours, &mut theirs) {
            a.remove("id");
            b.remove("type");
            b.remove("id");
        }
        ours == theirs
    }
}

/// 文件头：record-log 的第一行，描述会话元信息。
/// `kind` 恒为 "header"，`version` 为 4，用于向前兼容（旧版工具可识别）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JsonlV4Header {
    /// 行标签，恒为 "header".
    pub kind: String,
    /// 日志格式版本，v4 恒为 4.
    pub version: u32,
    /// 会话 id。
    pub id: String,
    /// 会话创建时间（Unix 毫秒）。
    pub created_at: i64,
    /// 会话创建时的工作目录。
    pub cwd: String,
    /// 父会话 id（fork / 子会话场景）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// 调用方自定义元数据，本层不解释。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl JsonlV4Header {
    /// 构造 v4 文件头：kind 固定为 "header"、version 固定为 4，metadata 留空待调用方填写；
    /// `parent_session_id` 为 fork / 子会话场景的父会话 id。
    pub fn new(
        id: String,
        cwd: String,
        created_at: i64,
        parent_session_id: Option<String>,
    ) -> Self {
        Self {
            kind: "header".to_string(),
            version: 4,
            id,
            created_at,
            cwd,
            parent_session_id,
            metadata: None,
        }
    }

    /// 序列化为一行 JSON 并补上结尾换行，作为 record-log 的首行写出；
    /// 序列化失败时退化为 `{}`。
    pub fn encode(&self) -> String {
        format!(
            "{}\n",
            serde_json::to_string(self).unwrap_or_else(|_| "{}".to_string())
        )
    }
}

/// 持久化 mutation：磁盘上每一行的类型。`kind` 作为行标签区分四种行。
/// - `entry`：转写条目（可带 `lane`，表示写入到该 lane 并推进其叶子）；
/// - `record`：运行记录（flatten 一个 `LaneRecord`）；
/// - `lane`：重设某 lane 的叶子指针；
/// - `fact`：会话级事实（name / label）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(clippy::large_enum_variant)]
pub enum SessionMutation {
    /// 内容层：落一条转写条目。
    Entry {
        /// 若给出 lane 名：该条目同时写入该 lane 并推进其叶子指针；
        /// `None` 则只加入 entry 树，不触碰任何 lane（如纯历史 / 配置类条目）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lane: Option<String>,
        /// 内容层的转写条目本体，其 `type` 决定具体变体。
        #[serde(flatten)]
        entry: Entry,
    },
    /// 编排层：落一条运行记录（record 展开后自带 lane 字段）。
    Record {
        /// 编排层的运行记录，flatten 后自带 lane / seq 等公共元数据。
        #[serde(flatten)]
        record: LaneRecord,
    },
    /// 指针层：只重设某 lane 的叶子指针，不产生 entry / record。
    Lane {
        /// 全局序号（与 entry / record 共用同一把计数器）。
        seq: u64,
        /// 目标 lane 名。
        lane: String,
        /// 新的叶子条目 id；`None` 表示清空该 lane（叶子不存在）。
        #[serde(rename = "leafId")]
        leaf_id: Option<String>,
    },
    /// 元数据层：写会话级事实（会话名 / 条目 label）。
    Fact {
        /// 全局序号。
        seq: u64,
        /// 事实类型："name"（会话名）或 "label"（条目标签）。
        fact: String,
        /// 类型为 "name" 时：会话名（可清空）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        /// 类型为 "label" 时：被标注的条目 id。
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "targetId")]
        target_id: Option<String>,
        /// 类型为 "label" 时：标签文本（可清除）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

/// 行编码：把一条 mutation 序列化为一行 JSON（含换行），用于追加到文件。
/// 序列化失败时退化为 `{}`（正常情况不应发生）。
pub fn encode_mutation(m: &SessionMutation) -> String {
    let v = serde_json::to_value(m).unwrap_or(Value::Null);
    format!("{}\n", v)
}

/// 解析单行 mutation。
/// 只做**结构 / 类型**校验（JSON 是否合法、kind/seq/字段类型对不对）；
/// **链式**校验（parent 是否存在、lane 是否匹配、seq 是否连续）在 `apply_mutation` 阶段做。
pub fn parse_mutation(line: &str) -> Result<SessionMutation, RecordLogError> {
    let value: Value = serde_json::from_str(line).map_err(|e| RecordLogError::InvalidJson {
        message: format!("is not valid JSON: {e}"),
    })?;
    let obj = match value {
        Value::Object(m) => m,
        _ => {
            return Err(RecordLogError::NotJsonObject {
                message: "is not a JSON object".to_string(),
            });
        }
    };
    let kind =
        obj.get("kind")
            .and_then(|v| v.as_str())
            .ok_or_else(|| RecordLogError::InvalidKind {
                message: "has invalid kind".to_string(),
            })?;

    match kind {
        "entry" => {
            let m: SessionMutation = serde_json::from_value(Value::Object(obj)).map_err(|e| {
                RecordLogError::InvalidEntry {
                    message: format!("has invalid entry: {e}"),
                }
            })?;
            let SessionMutation::Entry { entry, .. } = &m else {
                unreachable!()
            };
            validate_entry_payload(entry)?;
            Ok(m)
        }
        "record" => {
            let m: SessionMutation = serde_json::from_value(Value::Object(obj)).map_err(|e| {
                RecordLogError::InvalidRecord {
                    message: format!("has invalid record: {e}"),
                }
            })?;
            let SessionMutation::Record { record } = &m else {
                unreachable!()
            };
            validate_record_payload(record)?;
            Ok(m)
        }
        "lane" => {
            let m: SessionMutation = serde_json::from_value(Value::Object(obj)).map_err(|e| {
                RecordLogError::InvalidLane {
                    message: format!("has invalid lane: {e}"),
                }
            })?;
            Ok(m)
        }
        "fact" => {
            let m: SessionMutation = serde_json::from_value(Value::Object(obj)).map_err(|e| {
                RecordLogError::InvalidFact {
                    message: format!("has invalid fact: {e}"),
                }
            })?;
            let SessionMutation::Fact { fact, .. } = &m else {
                unreachable!()
            };
            if fact != "name" && fact != "label" {
                return Err(RecordLogError::UnknownFactType {
                    message: format!("has unknown fact type {fact}"),
                });
            }
            Ok(m)
        }
        _ => Err(RecordLogError::UnknownMutationKind {
            message: format!("has unknown mutation kind {kind}"),
        }),
    }
}

/// entry 载荷的浅层结构校验：seq 非 0、时间戳非负、customType 非空。
fn validate_entry_payload(entry: &Entry) -> Result<(), RecordLogError> {
    if entry.seq() == 0 {
        return Err(RecordLogError::InvalidSeq {
            message: "has invalid seq".to_string(),
        });
    }

    let base = entry_base(entry);
    if base.timestamp < 0 {
        return Err(RecordLogError::InvalidTimestamp {
            message: "has invalid timestamp".to_string(),
        });
    }

    if let Entry::Custom { custom_type, .. } = entry
        && custom_type.is_empty()
    {
        return Err(RecordLogError::InvalidCustomType {
            message: "has invalid customType".to_string(),
        });
    }

    if let Entry::ContextEdit { target_id, .. } = entry
        && target_id.is_empty()
    {
        return Err(RecordLogError::InvalidEntry {
            message: "context_edit has empty targetId".to_string(),
        });
    }

    Ok(())
}

/// record 载荷的浅层结构校验：seq 非 0、时间戳非负。
fn validate_record_payload(record: &LaneRecord) -> Result<(), RecordLogError> {
    if record.seq() == 0 {
        return Err(RecordLogError::InvalidSeq {
            message: "has invalid seq".to_string(),
        });
    }
    if record.timestamp() < 0 {
        return Err(RecordLogError::InvalidTimestamp {
            message: "has invalid timestamp".to_string(),
        });
    }
    Ok(())
}

/// 取回 entry 的基字段（各变体共享）。
pub fn entry_base(entry: &Entry) -> &EntryBase {
    match entry {
        Entry::Message { base, .. }
        | Entry::ModelChange { base, .. }
        | Entry::ThinkingLevelChange { base, .. }
        | Entry::ActiveToolsChange { base, .. }
        | Entry::Compaction { base, .. }
        | Entry::BranchSummary { base, .. }
        | Entry::ContextEdit { base, .. }
        | Entry::Custom { base, .. } => base,
    }
}

/// record 基字段：所有 LaneRecord 变体共享的公共字段。
/// 与 EntryBase 类似，额外带 `lane`（record 属于哪条 lane）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordBase {
    /// 记录 id（全局唯一）。
    pub id: String,
    /// 全局序号（与 entry 共用同一计数器，供跨层合并排序）。
    pub seq: u64,
    /// 记录所属 lane 名（记录总是属于某条分支）。
    pub lane: String,
    /// Unix 毫秒时间戳。
    pub timestamp: i64,
}

/// 操作意图（operation intent）：描述一次 operation 想做什么。
/// 归约器根据它，知道如何校验、如何还原运行目标（targets）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OperationIntent {
    /// 普通运行：记录原始提示、预置的初始消息、系统提示覆盖
    Run {
        /// 运行发起时的原始提示序列（快照，供审计 / 回放）。
        #[serde(rename = "originalPrompt")]
        original_prompt: Vec<AgentMessage>,
        /// 预置的"目标条目"：运行开始前就已声明的初始消息意图，
        /// 落地后要求内容与意图一致（由归约器校验 missing / mismatch）。
        #[serde(rename = "initialMessages")]
        initial_messages: Vec<ProvisionedEntry>,
        /// 本次运行对系统提示的覆盖（可选）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "systemPromptOverride")]
        system_prompt_override: Option<String>,
    },
    /// 压缩操作：可选自定义指令，目标是把某个 compaction 结果落地
    Compaction {
        /// 压缩时的自定义指令（可选）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "customInstructions")]
        custom_instructions: Option<String>,
        /// 期望落地的 compaction 条目 id；归约时据此判断目标是否达成。
        #[serde(rename = "resultEntryId")]
        result_entry_id: String,
    },
    /// 树导航：把 leaf 切到目标条目，可选生成被放弃分支的 branch_summary
    Navigation {
        /// 导航目标条目 id；`None` 表示回到根。
        #[serde(rename = "targetId")]
        target_id: Option<String>,
        /// 是否在切换时为被放弃分支生成 branch_summary。
        summarize: bool,
        /// 生成摘要时的自定义指令（可选）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "customInstructions")]
        custom_instructions: Option<String>,
        /// 导航后给目标条目打的标签（可选，如 "checkpoint"）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        label: Option<String>,
        /// summarize 时期望落地的 branch_summary 条目 id。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "summaryEntryId")]
        summary_entry_id: Option<String>,
    },
}

/// 运行记录："运行编排过程"的持久化轨迹。
/// 通过 `#[serde(tag = "type")]` 区分变体。归约器消费这些记录来还原运行状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum LaneRecord {
    /// 操作开始：打开一个 operation，携带意图
    OperationStarted {
        /// 所有变体共享的元数据：记录 id、全局序号、所属 lane 与时间戳。
        #[serde(flatten)]
        base: RecordBase,
        /// 发起此次运行起点所在的叶子条目 id；`None` 表示从空 / 根开始。
        /// （归约出的 OperationState.source_leaf_id 即取自这里。）
        #[serde(rename = "sourceLeafId")]
        source_leaf_id: Option<String>,
        /// 本次 operation 想做什么（Run / Compaction / Navigation）。
        /// 归约器据此推导 missing_initial_messages、targets 等。
        intent: OperationIntent,
    },
    /// 请求中止某个运行
    AbortRequested {
        /// 所有变体共享的元数据：记录 id、全局序号、所属 lane 与时间戳。
        #[serde(flatten)]
        base: RecordBase,
        /// 被中止的 operation id（须已存在 operation_started）。
        #[serde(rename = "runId")]
        run_id: String,
    },
    /// 操作结束（completed / failed / aborted……）
    OperationFinished {
        /// 所有变体共享的元数据：记录 id、全局序号、所属 lane 与时间戳。
        #[serde(flatten)]
        base: RecordBase,
        /// 结束的 operation id。
        #[serde(rename = "runId")]
        run_id: String,
        /// 结束原因（completed / failed / aborted……，取值由上层约定）。
        outcome: String,
        /// 失败时的错误载荷（可选）。
        error: Option<Value>,
    },
    /// 某一步的某次尝试（assistant / compaction / branch_summary……）
    StepAttempt {
        /// 所有变体共享的元数据：记录 id、全局序号、所属 lane 与时间戳。
        #[serde(flatten)]
        base: RecordBase,
        /// 所属 operation id。
        #[serde(rename = "runId")]
        run_id: String,
        /// 步骤名（assistant / compaction / branch_summary……）。
        step: String,
        /// 该 step 的第几次尝试（同 step 连续时严格 +1，跳号即损坏）。
        attempt: u32,
        /// 本次尝试应该产出的结果条目 id；归约时若未落地则标记为 pending step。
        #[serde(rename = "resultEntryId")]
        result_entry_id: String,
        /// 仅 compaction 步骤携带：manual / threshold / overflow，其余步骤必须为 None。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "compactionReason")]
        compaction_reason: Option<String>,
    },
    /// 工具调用发起：把 assistant 里某个 tool_call 序号与结果 entry 关联起来
    ToolStarted {
        /// 所有变体共享的元数据：记录 id、全局序号、所属 lane 与时间戳。
        #[serde(flatten)]
        base: RecordBase,
        /// 所属 operation id。
        #[serde(rename = "runId")]
        run_id: String,
        /// 发出这批 tool_call 的 assistant 条目 id。
        #[serde(rename = "assistantEntryId")]
        assistant_entry_id: String,
        /// 该 assistant 条目内 tool_call 块的序号（从 0 起）。
        /// 与 (assistant_entry_id, tool_index) 共同构成不可重复的调用标识。
        #[serde(rename = "toolIndex")]
        tool_index: usize,
        /// 该调用的 tool_call_id（与 assistant 条目内一致）。
        #[serde(rename = "toolCallId")]
        tool_call_id: String,
        /// 工具名（与 assistant 条目内一致）。
        #[serde(rename = "toolName")]
        tool_name: String,
        /// 实际生效的参数（可能已对原始 arguments 做过求值 / 覆盖）。
        #[serde(rename = "effectiveArgs")]
        effective_args: Value,
        /// 该调用的工具结果条目 id（toolResult）。
        #[serde(rename = "resultEntryId")]
        result_entry_id: String,
        /// 重放模式（如 normal / replay），供执行层区分是否真执行。
        replay: String,
    },
    /// 往某个队列（steer / followUp / nextRun）入队一个目标条目
    QueueEnqueued {
        /// 所有变体共享的元数据：记录 id、全局序号、所属 lane 与时间戳。
        #[serde(flatten)]
        base: RecordBase,
        /// 队列名：steer / followUp / nextRun。语义不同：
        /// nextRun 属于"下一次运行"，steer / followUp 属于本次（且中止后清空）。
        queue: String,
        /// 关联的 operation id；nextRun 可以没有（独立于当前运行）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "runId")]
        run_id: Option<String>,
        /// 入队的目标条目（意图）；落地后从 pending 中消失。
        target: ProvisionedEntry,
    },
    /// 取消某个已入队但尚未落地的条目
    QueueCancelled {
        /// 所有变体共享的元数据：记录 id、全局序号、所属 lane 与时间戳。
        #[serde(flatten)]
        base: RecordBase,
        /// 关联的 operation id（须与对应入队的 runId 一致）。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[serde(rename = "runId")]
        run_id: Option<String>,
        /// 被取消的目标条目 id。
        #[serde(rename = "entryId")]
        entry_id: String,
    },
    /// 延迟写：先记录意图，真正落地在之后（用于回放/恢复时补齐）
    WriteDeferred {
        /// 所有变体共享的元数据：记录 id、全局序号、所属 lane 与时间戳。
        #[serde(flatten)]
        base: RecordBase,
        /// 所属 operation id。
        #[serde(rename = "runId")]
        run_id: String,
        /// 延迟写入的目标条目意图；落地前计入 pending_writes。
        target: ProvisionedEntry,
    },
    /// token/成本用量记录
    UsageRecord {
        /// 所有变体共享的元数据：记录 id、全局序号、所属 lane 与时间戳。
        #[serde(flatten)]
        base: RecordBase,
        /// 本次用量明细（cached / uncached / total / cost）。
        usage: Usage,
        /// 用途标签（assistant / deferred_fetch / compaction……），terminal_failure
        /// 判定会用到 cause == "deferred_fetch"。
        cause: String,
        /// 归属的 provider。供 `/session` 的成本分列归集（无归属只能进 `Tools/summaries` 桶）；
        /// 旧会话文件里的用量记录没有这两个字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
        /// 归属的模型
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// 关联的 operation id；跨运行汇总类用量没有归属，此时为 None。
        #[serde(rename = "runId")]
        run_id: Option<String>,
        /// 关联的条目 id（如被计费的 assistant / 结果条目）。
        #[serde(rename = "entryId")]
        entry_id: Option<String>,
        /// 关联的 step 尝试号（可选）。
        attempt: Option<u32>,
        /// 关联调用的停止原因（如 length / error）。
        #[serde(rename = "stopReason")]
        stop_reason: Option<String>,
        /// 关联的 tool_call_id（工具调用计费场景）。
        #[serde(rename = "toolCallId")]
        tool_call_id: Option<String>,
    },
}

impl LaneRecord {
    /// 记录的全局序号（与 entry 共用同一把计数器）。
    pub fn seq(&self) -> u64 {
        match self {
            LaneRecord::OperationStarted { base, .. }
            | LaneRecord::AbortRequested { base, .. }
            | LaneRecord::OperationFinished { base, .. }
            | LaneRecord::StepAttempt { base, .. }
            | LaneRecord::ToolStarted { base, .. }
            | LaneRecord::QueueEnqueued { base, .. }
            | LaneRecord::QueueCancelled { base, .. }
            | LaneRecord::WriteDeferred { base, .. }
            | LaneRecord::UsageRecord { base, .. } => base.seq,
        }
    }

    /// 记录的写入时间（Unix 毫秒）。
    pub fn timestamp(&self) -> i64 {
        match self {
            LaneRecord::OperationStarted { base, .. }
            | LaneRecord::AbortRequested { base, .. }
            | LaneRecord::OperationFinished { base, .. }
            | LaneRecord::StepAttempt { base, .. }
            | LaneRecord::ToolStarted { base, .. }
            | LaneRecord::QueueEnqueued { base, .. }
            | LaneRecord::QueueCancelled { base, .. }
            | LaneRecord::WriteDeferred { base, .. }
            | LaneRecord::UsageRecord { base, .. } => base.timestamp,
        }
    }

    /// 记录所属的 lane 名（每个变体的 `RecordBase` 都带）。
    pub fn lane(&self) -> &str {
        match self {
            LaneRecord::OperationStarted { base, .. }
            | LaneRecord::AbortRequested { base, .. }
            | LaneRecord::OperationFinished { base, .. }
            | LaneRecord::StepAttempt { base, .. }
            | LaneRecord::ToolStarted { base, .. }
            | LaneRecord::QueueEnqueued { base, .. }
            | LaneRecord::QueueCancelled { base, .. }
            | LaneRecord::WriteDeferred { base, .. }
            | LaneRecord::UsageRecord { base, .. } => &base.lane,
        }
    }

    /// 记录的全局唯一 id。
    pub fn id(&self) -> &str {
        match self {
            LaneRecord::OperationStarted { base, .. }
            | LaneRecord::AbortRequested { base, .. }
            | LaneRecord::OperationFinished { base, .. }
            | LaneRecord::StepAttempt { base, .. }
            | LaneRecord::ToolStarted { base, .. }
            | LaneRecord::QueueEnqueued { base, .. }
            | LaneRecord::QueueCancelled { base, .. }
            | LaneRecord::WriteDeferred { base, .. }
            | LaneRecord::UsageRecord { base, .. } => &base.id,
        }
    }

    /// 记录关联的 operation / run id；`OperationStarted` 自身开启运行故无归属，
    /// `QueueEnqueued` / `QueueCancelled` / `UsageRecord` 也可能没有归属，这些情况均返回 `None`。
    fn run_id(&self) -> Option<&str> {
        match self {
            LaneRecord::OperationStarted { .. } => None,
            LaneRecord::AbortRequested { run_id, .. }
            | LaneRecord::OperationFinished { run_id, .. }
            | LaneRecord::StepAttempt { run_id, .. }
            | LaneRecord::ToolStarted { run_id, .. }
            | LaneRecord::WriteDeferred { run_id, .. } => Some(run_id),
            LaneRecord::QueueEnqueued { run_id, .. } => run_id.as_deref(),
            LaneRecord::QueueCancelled { run_id, .. } => run_id.as_deref(),
            LaneRecord::UsageRecord { run_id, .. } => run_id.as_deref(),
        }
    }
}

/// 有效 lane 配置：由归约器从配置类 entry（ModelChange / ThinkingLevelChange /
/// ActiveToolsChange 及 assistant 消息携带的 provider/model）按时间序推导出来的当前配置。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EffectiveLaneConfiguration {
    /// 当前生效的 provider 名。
    pub provider: String,
    /// 当前生效的模型 id。
    pub model_id: String,
    /// 当前生效的推理级别。
    pub thinking_level: String,
    /// 当前生效的活动工具名全量列表。
    pub active_tool_names: Vec<String>,
}

/// 单步执行状态：某一步尚未落地（result 缺失）时表示"还需要重放/重试"。
#[derive(Debug, Clone)]
pub struct StepState {
    /// 步骤名（assistant / compaction / branch_summary……）。
    pub kind: String,
    /// 已尝试次数（= 最新 StepAttempt 的 attempt）。
    pub attempts: u32,
    /// 期望产出但尚未落地的结果条目 id。
    pub result_entry_id: String,
    /// compaction 步骤的 reason；非 compaction 步骤为 None。
    pub compaction_reason: Option<String>,
}

/// 工具批次状态：一个 assistant 条目里批量发起的工具调用集合。
/// 归约器从 ToolStarted 记录 + assistant 条目的工具调用列表还原。
#[derive(Debug, Clone)]
pub struct ToolBatchState {
    /// 发起这批调用的 assistant 条目 id。
    pub assistant_entry_id: String,
    /// 按 tool_call 序号排列的调用明细。
    pub calls: Vec<ToolBatchCall>,
    /// 是否因达到长度上限（stop_reason == "length"）被截断。
    pub truncated: bool,
    /// 是否还有未拿到结果的调用（结果缺失需要重放 / 重试）。
    pub unresolved: bool,
}

/// 批次里的单个工具调用
#[derive(Debug, Clone)]
pub struct ToolBatchCall {
    /// 在 assistant 条目内的 tool_call 序号。
    pub tool_index: usize,
    /// 工具调用 id。
    pub tool_call_id: String,
    /// 工具名。
    pub tool_name: String,
    /// 参数（取自 assistant 条目内的 arguments）。
    pub arguments: Value,
    /// 是否已拿到结果条目。
    pub result_exists: bool,
    /// 结果是否请求终止运行（取自 toolResult 的 terminate 字段）。
    pub terminate: Option<bool>,
}

/// 操作（operation）的运行状态：归约器还原出的"当前运行进行到哪"。
#[derive(Debug, Clone, Default)]
pub struct OperationState {
    /// operation id（= 其 operation_started 记录的 id）。
    pub id: String,
    /// 意图种类：run / compaction / navigation。
    pub kind: String,
    /// 发起运行的起点叶子条目 id。
    pub source_leaf_id: Option<String>,
    /// 是否已收到中止请求（中止后 steer / followUp 视作清空）。
    pub aborting: bool,
    /// 当前待重放 / 重试的 step；结果已落地则为 None。
    pub step: Option<StepState>,
    /// 当前工具批次（若最新 assistant 条目含 tool_call）。
    pub tool_batch: Option<ToolBatchState>,
    /// Run 意图预置但尚未落地的初始消息（崩溃后需补齐）。
    pub missing_initial_messages: Vec<ProvisionedEntry>,
    /// 本次运行待执行的 steer 队列（未落地、未取消）。
    pub pending_steer: Vec<ProvisionedEntry>,
    /// 本次运行待执行的 followUp 队列。
    pub pending_follow_up: Vec<ProvisionedEntry>,
    /// 声明了延迟写但尚未落地的条目。
    pub pending_writes: Vec<ProvisionedEntry>,
    /// 是否已做过 overflow 恢复（compaction(overflow) 出现在已消费条目之后）。
    pub overflow_recovery_used: bool,
    /// 最新自产条目（本 lane 的最后一条 own entry）的 payload 快照，供外部状态展示。
    pub newest_own: Option<Value>,
    /// 意图目标的达成情况快照（如 {"result": true}、{"summary": false}）。
    pub targets: Value,
}

/// 一条 lane 的整体状态：叶子指针 + 打开的 operation + 待执行的 next_run 队列。
#[derive(Debug, Clone)]
pub struct LaneState {
    /// lane 名。
    pub lane: String,
    /// 当前叶子条目 id；`None` 表示空 lane。
    pub leaf_id: Option<String>,
    /// 当前打开的 operation；`None` 表示 lane 空闲。
    pub operation: Option<OperationState>,
    /// 排给"下一次运行"的待执行队列（未落地、未取消、未被本次捕获）。
    pub pending_next_run: Vec<ProvisionedEntry>,
}

/// 终止性失败：assistant 以 stop_reason=error 结束且是当前运行产生的（非延迟写），
/// 视为一次必须上报的终结失败。
#[derive(Debug, Clone)]
pub struct TerminalFailureState {
    /// 失败条目 id（stop_reason == "error" 的 assistant 消息）。
    pub entry_id: String,
    /// 产生来源："step"（由 StepAttempt 产出）或 "deferred_fetch"。
    pub source: String,
    /// 失败消息本体，供上层上报 / 展示。
    pub message: AgentMessage,
}

/// 归约输出：一次 `reduce_lane_state` 的结果。
#[derive(Debug, Clone)]
pub struct LaneReductionResult {
    /// 还原出的 lane 编排状态（leaf + operation + next_run 队列）。
    pub lane_state: LaneState,
    /// 由配置类条目推导出的当前有效配置。
    pub effective_configuration: EffectiveLaneConfiguration,
    /// 需要上报的终结失败（若最新 assistant 消息是 error 且确属本 operation）。
    pub terminal_failure: Option<TerminalFailureState>,
}

/// 归约输入切片：按 lane 分片出的有界数据，供归约器重建该 lane 的状态。
/// - `open_operations`：该 lane 当前打开的 operation（harness 分片传递）；
/// - `records`：该 lane 的全部运行记录；
/// - `entries` / `own_entries`：参与校验 / 归约的条目切片；
/// - `configuration_entries`：用于推导有效配置的条目；
/// - `defaults`：配置推导的起始默认值。
pub struct LaneReductionInput {
    /// 目标 lane 名。
    pub lane: String,
    /// 该 lane 当前叶子指针（来自 session_manager，不参与归约推导）。
    pub leaf_id: Option<String>,
    /// 该 lane 未关闭的 operation_started 记录（0..n 个；多开是崩溃残留的合法形态）。
    pub open_operations: Vec<LaneRecord>,
    /// 该 lane 的全部运行记录（含已关闭操作的，供校验与队列推导）。
    pub records: Vec<LaneRecord>,
    /// 参与校验 / 索引的条目集（可能跨 lane，用于验证引用与内容一致性）。
    pub entries: Vec<Entry>,
    /// 本 lane 拥有的条目（主要用于 pending / tool_batch / newest_own 判定）。
    pub own_entries: Vec<Entry>,
    /// 用于推导有效配置的历史条目（配置类 entry 按时间序叠加）。
    pub configuration_entries: Vec<Entry>,
    /// 配置推导的起始默认值（provider / model / thinking / tools 的初始快照）。
    pub defaults: EffectiveLaneConfiguration,
}

/// 判断已落地条目是否与预置意图的业务内容一致（忽略 id/type 等结构字段）。
fn matches_provisioned(target: &ProvisionedEntry, entry: &Entry) -> bool {
    target.matches_entry(entry)
}

/// 校验一个 ProvisionedEntry 若已落地则内容必须与意图一致。
/// 如果条目存在但内容不一致，说明写入方与意图分叉，报 ProvisionedEntryMismatch。
fn validate_exact_provisioned(
    entries_by_id: &HashMap<String, Entry>,
    target: &ProvisionedEntry,
) -> Result<(), RecordLogError> {
    if let Some(entry) = entries_by_id.get(&target.id)
        && !matches_provisioned(target, entry)
    {
        return corrupt(RecordLogError::ProvisionedEntryMismatch {
            message: format!(
                "Provisioned entry {} exists with content different from its intent",
                target.id
            ),
        });
    }
    Ok(())
}

/// 校验一个"结果条目"若已存在则必须符合某种谓词（例如 assistant 结果必须是
/// role=assistant 的 Message）。若存在但不匹配则报损坏。
fn validate_result_entry(
    entries_by_id: &HashMap<String, Entry>,
    result_entry_id: &str,
    matches: impl Fn(&Entry) -> bool,
    description: &str,
) -> Result<(), RecordLogError> {
    if let Some(entry) = entries_by_id.get(result_entry_id)
        && !matches(entry)
    {
        return corrupt(RecordLogError::ProvisionedEntryMismatch {
            message: format!(
                "Provisioned {} entry {} exists with different content",
                description, result_entry_id
            ),
        });
    }
    Ok(())
}

/// 校验 compaction 步骤的 reason 合法性：
/// 只有 step == "compaction" 时允许 reason 为 manual/threshold/overflow，
/// 其他步骤不应携带 reason。
fn validate_attempt_reason(record: &LaneRecord) -> Result<(), RecordLogError> {
    if let LaneRecord::StepAttempt {
        step,
        compaction_reason,
        base,
        ..
    } = record
    {
        let reason = compaction_reason.as_deref();
        if step == "compaction" {
            if !matches!(
                reason,
                Some("manual") | Some("threshold") | Some("overflow")
            ) {
                return corrupt(RecordLogError::InvalidCompactionReason {
                    message: format!(
                        "Compaction attempt {} has no valid compaction reason",
                        base.id
                    ),
                });
            }
        } else if reason.is_some() {
            return corrupt(RecordLogError::InvalidCompactionReason {
                message: format!("{} attempt {} has a compaction reason", step, base.id),
            });
        }
    }
    Ok(())
}

/// 记录某一步最近一次 attempt 的包装，供同 step 的连续 attempt 校验。
#[derive(Clone)]
struct AttemptSeries {
    /// 该步骤最近一次 attempt 的记录，用于校验后续尝试序号是否连续。
    record: LaneRecord,
}

/// 校验同一 operation 内 step 的 attempt 序号严格递增：
/// 同 step 连续则 attempt 必须 = 前一次 + 1，否则（跳号 / 从 1 开始不符）报损坏。
/// 对非 assistant 步骤还校验其 result_entry_id / compaction_reason 与上一次一致。
fn validate_attempt_sequence(
    record: &LaneRecord,
    previous: Option<&AttemptSeries>,
    _entries_by_id: &HashMap<String, Entry>,
) -> Result<(), RecordLogError> {
    let LaneRecord::StepAttempt {
        step,
        attempt,
        result_entry_id,
        compaction_reason,
        base,
        ..
    } = record
    else {
        return Ok(());
    };

    let continues_series = match &previous {
        Some(p) => {
            let prev_step = match &p.record {
                LaneRecord::StepAttempt { step, .. } => step,
                _ => return Ok(()),
            };
            // run_loop 为 write-behind（结果先落地再写记录）。
            // 系列判定放宽为同 step 连续，不变量仍是 attempt 严格递增（非连续即 corruption）。
            prev_step == step
        }
        None => false,
    };
    let expected = if continues_series {
        match &previous.unwrap().record {
            LaneRecord::StepAttempt { attempt, .. } => attempt + 1,
            _ => 1,
        }
    } else {
        1
    };

    if *attempt != expected {
        return corrupt(RecordLogError::NonConsecutiveAttempt {
            message: format!(
                "{} attempt {} is {}; expected {}",
                step, base.id, attempt, expected
            ),
        });
    }

    // 非 assistant 步骤的连续性一致性
    if !continues_series || step == "assistant" || previous.is_none() {
        return Ok(());
    }

    let prev = previous.unwrap();
    let (_prev_step, prev_result, prev_reason) = match &prev.record {
        LaneRecord::StepAttempt {
            step,
            result_entry_id,
            compaction_reason,
            ..
        } => (step.as_str(), result_entry_id, compaction_reason.as_deref()),
        _ => return Ok(()),
    };

    if prev_result != result_entry_id {
        return corrupt(RecordLogError::InconsistentStep {
            message: format!("{} attempts disagree on their result entry id", step),
        });
    }

    if prev_reason != compaction_reason.as_deref() {
        return corrupt(RecordLogError::InconsistentStep {
            message: format!("{} attempts disagree on their compaction reason", step),
        });
    }
    Ok(())
}

/// 校验 StepAttempt 记录引用的结果条目类型与步骤匹配：
/// assistant→assistant 消息、compaction→Compaction、branch_summary→BranchSummary。
/// 条目尚未落地时通过；已落地但类型不符则报损坏。
fn validate_attempt_result(
    entries_by_id: &HashMap<String, Entry>,
    record: &LaneRecord,
) -> Result<(), RecordLogError> {
    let LaneRecord::StepAttempt {
        step,
        result_entry_id,
        ..
    } = record
    else {
        return Ok(());
    };

    // 按步骤类型校验结果条目的类型：assistant→Message(assistant)，
    // compaction→Compaction，branch_summary→BranchSummary。
    match step.as_str() {
        "assistant" => validate_result_entry(
            entries_by_id,
            result_entry_id,
            |e| matches!(e, Entry::Message { message, .. } if message.role == "assistant"),
            "assistant result",
        ),
        "compaction" => validate_result_entry(
            entries_by_id,
            result_entry_id,
            |e| matches!(e, Entry::Compaction { .. }),
            "compaction result",
        ),
        "branch_summary" => validate_result_entry(
            entries_by_id,
            result_entry_id,
            |e| matches!(e, Entry::BranchSummary { .. }),
            "branch-summary result",
        ),
        _ => Ok(()),
    }
}

/// 校验一次工具发起：
/// 1. (assistant_entry_id, tool_index) 不重复；
/// 2. 引用的 assistant 条目确实存在且该序号的 tool_call 的 id/name 与记录一致；
/// 3. 对应的工具结果条目类型正确（toolResult 且 tool_call_id/tool_name 匹配）。
fn validate_tool_start(
    record: &LaneRecord,
    entries_by_id: &HashMap<String, Entry>,
    invocations: &mut HashSet<String>,
) -> Result<(), RecordLogError> {
    let LaneRecord::ToolStarted {
        assistant_entry_id,
        tool_index,
        tool_call_id,
        tool_name,
        result_entry_id,
        base,
        ..
    } = record
    else {
        return Ok(());
    };

    let invocation = format!("{}\u{0}{}", assistant_entry_id, tool_index);
    if !invocations.insert(invocation) {
        return corrupt(RecordLogError::DuplicateToolInvocation {
            message: format!(
                "Tool invocation {}:{} is duplicated",
                assistant_entry_id, tool_index
            ),
        });
    }

    let assistant = entries_by_id.get(assistant_entry_id);
    match assistant {
        Some(Entry::Message { message, .. }) if message.role == "assistant" => {
            let tool_calls: Vec<&ContentBlock> = message
                .content
                .iter()
                .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
                .collect();
            let tool_call = tool_calls.get(*tool_index);
            let (call_id, call_name, _args) = match tool_call {
                Some(ContentBlock::ToolCall { id, name, .. }) => (id.as_str(), name.as_str(), ()),
                _ => {
                    return corrupt(RecordLogError::ToolCallMismatch {
                        message: format!(
                            "Tool start {} does not reference an assistant entry with tool calls",
                            base.id
                        ),
                    });
                }
            };
            if call_id != tool_call_id || call_name != tool_name {
                return corrupt(RecordLogError::ToolCallMismatch {
                    message: format!(
                        "Tool start {} does not match its assistant tool-call ordinal",
                        base.id
                    ),
                });
            }
        }
        _ => {
            return corrupt(RecordLogError::ToolCallMismatch {
                message: format!(
                    "Tool start {} does not reference an assistant entry",
                    base.id
                ),
            });
        }
    }

    validate_result_entry(
        entries_by_id,
        result_entry_id,
        |e| {
            matches!(
                e,
                Entry::Message {
                    message,
                    ..
                } if message.role == "toolResult" && message.tool_call_id.as_deref() == Some(tool_call_id)
                    && message.tool_name.as_deref() == Some(tool_name)
            )
        },
        "tool result",
    )
}

/// 校验 deferred 的 assistant 条目必须携带 handle（否则恢复时无法找回结果）。
fn validate_deferred_handles(entries: Vec<&Entry>) -> Result<(), RecordLogError> {
    for entry in entries {
        if let Entry::Message { message, .. } = entry
            && message.role == "assistant"
            && message.stop_reason.as_deref() == Some("deferred")
            && message.deferred.is_none()
        {
            return corrupt(RecordLogError::InvalidDeferredHandle {
                message: format!(
                    "Deferred assistant entry {} does not carry a handle",
                    entry.id()
                ),
            });
        }
    }
    Ok(())
}

/// 校验 operation 的意图目标：
/// - Run：每个 initial_message 若已落地则必须与意图一致；
/// - Compaction：result_entry_id 若已存在则必须是 Compaction；
/// - Navigation：summary_entry_id 若已存在则必须是 BranchSummary。
fn validate_operation_result(
    entries_by_id: &HashMap<String, Entry>,
    record: &LaneRecord,
) -> Result<(), RecordLogError> {
    let LaneRecord::OperationStarted { intent, .. } = record else {
        return Ok(());
    };
    match intent {
        OperationIntent::Run {
            initial_messages, ..
        } => {
            for target in initial_messages {
                validate_exact_provisioned(entries_by_id, target)?;
            }
        }
        OperationIntent::Compaction {
            result_entry_id, ..
        } => {
            validate_result_entry(
                entries_by_id,
                result_entry_id,
                |e| matches!(e, Entry::Compaction { .. }),
                "manual compaction",
            )?;
        }
        OperationIntent::Navigation {
            summary_entry_id, ..
        } => {
            if let Some(id) = summary_entry_id {
                validate_result_entry(
                    entries_by_id,
                    id,
                    |e| matches!(e, Entry::BranchSummary { .. }),
                    "navigation summary",
                )?;
            }
        }
    }
    Ok(())
}

/// 结构校验：在归约前对一整个 record-log 切片做一致性检查。
/// 校验通过不代表状态可归约，但任何违规都会以明确的 `RecordLogError` 报错。
/// 主要检查：operation 引用是否合法、finish/abort 后是否仍有记录、
/// step attempt 连续性、tool 调用匹配、队列取消合法性、ProvisionedEntry 一致性等。
/// 注意：`open_operations` 数量不设上限——多个未关闭的 operation 是崩溃残留的
/// 正常形态，不在本层报错（由归约器降级取最新）。
pub fn validate_record_log(
    open_operations: &[LaneRecord],
    records: &[LaneRecord],
    entries: &[Entry],
) -> Result<(), RecordLogError> {
    // 打开的 operation 可以为 0..n 个：多个未关闭的 operation 是崩溃残留的正常形态
    // （此前一次 run 的 operation_finished 未落盘），不是损坏。归约器在 reduce_lane_state
    // 中取 seq 最新的 operation 继续，更早的视为已中断（僵尸）。这里仍把每个 open
    // operation 的 start 记录 seed 进 starts，保证其后续记录被完整校验。
    // 建立 id→entry 索引，供后续查找
    let entries_by_id: HashMap<String, Entry> = entries
        .iter()
        .map(|e| (e.id().to_string(), e.clone()))
        .collect();
    validate_deferred_handles(entries.iter().collect())?;

    // 各状态跟踪：operation 起点、finish/abort 位置、队列入队、最近 attempt、已用 tool 调用
    let mut starts: HashMap<String, LaneRecord> = HashMap::new();
    let mut finished_at: HashMap<String, u64> = HashMap::new();
    let mut aborted_at: HashMap<String, u64> = HashMap::new();
    let mut queue_enqueues: HashMap<String, LaneRecord> = HashMap::new();
    let mut latest_attempt: HashMap<String, AttemptSeries> = HashMap::new();
    let mut tool_invocations: HashSet<String> = HashSet::new();

    // open_operations 本身也是 operation_started 记录（harness 分片传递），
    // 先 seed starts，使后续 runId 引用可解析。
    for op in open_operations {
        if let LaneRecord::OperationStarted { base, .. } = op {
            starts.insert(base.id.clone(), op.clone());
            validate_operation_result(&entries_by_id, op)?;
        }
    }

    // 按 seq 升序逐条检查记录
    let mut sorted: Vec<&LaneRecord> = records.iter().collect();
    sorted.sort_by_key(|r| r.seq());

    for record in sorted {
        if matches!(record, LaneRecord::OperationStarted { .. }) {
            starts.insert(record_base_id(record).to_string(), record.clone());
            validate_operation_result(&entries_by_id, record)?;
            continue;
        }

        // 所有引用 runId 的记录必须先有对应的 operation_started
        if let Some(run_id) = record.run_id() {
            if !starts.contains_key(run_id) {
                return corrupt(RecordLogError::UnknownOperation {
                    message: format!(
                        "Record {} references unknown operation {}",
                        record_base_id(record),
                        run_id
                    ),
                });
            }

            // 不能出现在 operation_finished 之后
            if let Some(finish_seq) = finished_at.get(run_id)
                && record.seq() > *finish_seq
            {
                return corrupt(RecordLogError::RecordAfterFinish {
                    message: format!(
                        "Record {} follows the finish of operation {}",
                        record_base_id(record),
                        run_id
                    ),
                });
            }
        }

        // 按记录类型做针对性校验
        match record {
            LaneRecord::OperationStarted { .. } => {}
            LaneRecord::OperationFinished { run_id, base, .. } => {
                finished_at.insert(run_id.clone(), base.seq);
            }
            LaneRecord::AbortRequested { run_id, base, .. } => {
                aborted_at.insert(run_id.clone(), base.seq);
            }
            LaneRecord::StepAttempt { .. } => {
                validate_attempt_reason(record)?;
                let prev = record.run_id().and_then(|rid| latest_attempt.get(rid));
                validate_attempt_sequence(record, prev, &entries_by_id)?;
                validate_attempt_result(&entries_by_id, record)?;

                if let Some(rid) = record.run_id() {
                    latest_attempt.insert(
                        rid.to_string(),
                        AttemptSeries {
                            record: record.clone(),
                        },
                    );
                }
            }
            LaneRecord::ToolStarted { .. } => {
                validate_tool_start(record, &entries_by_id, &mut tool_invocations)?;
            }
            LaneRecord::QueueEnqueued {
                queue,
                run_id,
                target,
                base,
                ..
            } => {
                // 除 nextRun 外，abort 之后的入队不合法
                if let Some(run_id) = run_id.as_ref()
                    && queue != "nextRun"
                    && aborted_at
                        .get(run_id)
                        .is_some_and(|&aborted| base.seq > aborted)
                {
                    return corrupt(RecordLogError::QueueAfterAbort {
                        message: format!("{} item {} was enqueued after abort", queue, target.id),
                    });
                }
                queue_enqueues.insert(target.id.clone(), record.clone());
                validate_exact_provisioned(&entries_by_id, target)?;
            }
            LaneRecord::QueueCancelled {
                entry_id,
                run_id,
                base,
                ..
            } => {
                // 取消必须对应一个未落地的、且在该取消之前的入队，runId 也须一致
                let enqueue = queue_enqueues.get(entry_id);
                let enqueue_seq = enqueue.map(|e| e.seq()).unwrap_or(u64::MAX);
                let enqueue_run = enqueue.and_then(|e| e.run_id().map(|s| s.to_string()));
                if enqueue.is_none()
                    || enqueue_seq >= base.seq
                    || enqueue_run != run_id.clone().or(None)
                    || entries_by_id.contains_key(entry_id)
                {
                    return corrupt(RecordLogError::InvalidQueueCancellation {
                        message: format!(
                            "Queue cancellation {} has no pending matching enqueue",
                            base.id
                        ),
                    });
                }
            }
            LaneRecord::WriteDeferred { target, .. } => {
                validate_exact_provisioned(&entries_by_id, target)?;
            }
            LaneRecord::UsageRecord { .. } => {}
        }
    }
    Ok(())
}

/// 取记录公共元数据里的 id（供日志 / 错误信息引用）。
fn record_base_id(record: &LaneRecord) -> &str {
    match record {
        LaneRecord::OperationStarted { base, .. }
        | LaneRecord::AbortRequested { base, .. }
        | LaneRecord::OperationFinished { base, .. }
        | LaneRecord::StepAttempt { base, .. }
        | LaneRecord::ToolStarted { base, .. }
        | LaneRecord::QueueEnqueued { base, .. }
        | LaneRecord::QueueCancelled { base, .. }
        | LaneRecord::WriteDeferred { base, .. }
        | LaneRecord::UsageRecord { base, .. } => &base.id,
    }
}

/// 按 seq 升序排序（归约前统一排序，保证处理顺序确定）。
fn by_sequence<T: Clone + HasSeq>(values: &[T]) -> Vec<T> {
    let mut v: Vec<T> = values.to_vec();
    v.sort_by_key(|a| a.seq());
    v
}

/// 统一取 seq 的 trait，让 `by_sequence` 对 Entry 和 LaneRecord 都可用。
trait HasSeq {
    /// 取全局序号，使 `by_sequence` 能同时排序 `Entry` 与 `LaneRecord`。
    fn seq(&self) -> u64;
}

impl HasSeq for Entry {
    /// 直接转发 `Entry::seq`。
    fn seq(&self) -> u64 {
        self.seq()
    }
}
impl HasSeq for LaneRecord {
    /// 直接转发 `LaneRecord::seq`。
    fn seq(&self) -> u64 {
        self.seq()
    }
}

/// 从配置类条目推导出"当前有效配置"：按时间序应用 ModelChange /
/// ThinkingLevelChange / ActiveToolsChange，assistant 消息携带的 provider/model 也会更新。
fn derive_effective_configuration(input: &LaneReductionInput) -> EffectiveLaneConfiguration {
    let mut configuration = input.defaults.clone();
    let mut all: Vec<Entry> = Vec::new();
    all.extend(input.configuration_entries.iter().cloned());
    all.extend(input.own_entries.iter().cloned());

    for entry in by_sequence(&all) {
        match &entry {
            Entry::ModelChange {
                provider, model_id, ..
            } => {
                configuration.provider = provider.clone();
                configuration.model_id = model_id.clone();
            }
            Entry::ThinkingLevelChange { thinking_level, .. } => {
                configuration.thinking_level = thinking_level.clone();
            }
            Entry::ActiveToolsChange {
                active_tool_names, ..
            } => {
                configuration.active_tool_names = active_tool_names.clone();
            }
            Entry::Message { message, .. } if message.role == "assistant" => {
                if let (Some(prov), Some(model)) = (&message.provider, &message.model) {
                    configuration.provider = prov.clone();
                    configuration.model_id = model.clone();
                }
            }
            _ => {}
        }
    }
    configuration
}

/// 取 own_entries 中最新一条的 payload（用于上报给外部的最新状态快照）。
fn derive_newest_own(entries: &[Entry]) -> Option<Value> {
    let entry = entries.last()?;
    let v = serde_json::to_value(entry.payload_json()).unwrap_or(Value::Null);
    Some(v)
}

/// 核心归约函数：把一条 lane 的有界 record + entry 切片还原为可操作的运行状态。
/// 先做 `validate_record_log` 结构校验（损坏即拒绝），再按 seq 排序记录并逐段推导：
/// 打开的 operation、待执行的队列、tool 批次、pending 写入、terminal failure 等。
/// 归约是纯函数（无副作用），因此可重复调用、可单测。
pub fn reduce_lane_state(input: LaneReductionInput) -> Result<LaneReductionResult, RecordLogError> {
    validate_record_log(&input.open_operations, &input.records, &input.entries)?;

    // 统一排序，建立 id→entry 索引
    let records = by_sequence(&input.records);
    let own_entries = by_sequence(&input.own_entries);
    let mut entries_by_id: HashMap<String, Entry> = HashMap::new();
    for entry in input.entries.iter().chain(input.own_entries.iter()) {
        entries_by_id.insert(entry.id().to_string(), entry.clone());
    }

    // 已取消的入队条目
    let cancelled: HashSet<String> = records
        .iter()
        .filter(|r| matches!(r, LaneRecord::QueueCancelled { .. }))
        .filter_map(|r| match r {
            LaneRecord::QueueCancelled { entry_id, .. } => Some(entry_id.clone()),
            _ => None,
        })
        .collect();
    // 仍 pending 的入队：已入队、未落地、未被取消
    let pending_queue_records: Vec<LaneRecord> = records
        .iter()
        .filter(|r| {
            matches!(r, LaneRecord::QueueEnqueued { .. })
                && !entries_by_id.contains_key(target_id_of(r))
                && !cancelled.contains(target_id_of(r))
        })
        .cloned()
        .collect();

    // 打开的 operation：按 seq 取最新。多 open op（崩溃残留）时旧 op 视为僵尸，
    // 只归约最新一次运行；不依赖调用方排序（纯函数）。
    let started = input
        .open_operations
        .iter()
        .max_by_key(|r| r.seq())
        .cloned();

    // 若打开的 operation 是 Run，记录其已被 initial_messages 占用的 id，这些条目不计入 pending_next_run。
    let captured_initial = if let Some(LaneRecord::OperationStarted { intent, .. }) = &started {
        match intent {
            OperationIntent::Run {
                initial_messages, ..
            } => initial_messages
                .iter()
                .map(|t| t.id.clone())
                .collect::<HashSet<_>>(),
            _ => Default::default(),
        }
    } else {
        Default::default()
    };

    // 下一次运行的待执行队列（排除已被本次 operation 捕获的初始消息）
    let pending_next_run: Vec<ProvisionedEntry> = pending_queue_records
        .iter()
        .filter(|r| queue_of(r) == "nextRun" && !captured_initial.contains(target_id_of(r)))
        .filter_map(target_of)
        .cloned()
        .collect();
    let effective_configuration = derive_effective_configuration(&input);

    // 没有打开的 operation：lane 空闲，直接返回
    let Some(started) = started else {
        return Ok(LaneReductionResult {
            lane_state: LaneState {
                lane: input.lane,
                leaf_id: input.leaf_id,
                operation: None,
                pending_next_run,
            },
            effective_configuration,
            terminal_failure: None,
        });
    };

    let started_id = match &started {
        LaneRecord::OperationStarted { base, .. } => base.id.clone(),
        _ => String::new(),
    };

    // 只保留属于当前 operation 的记录（runId == started_id，或本身就是该 start）
    let operation_records: Vec<&LaneRecord> = records
        .iter()
        .filter(|r| {
            if matches!(r, LaneRecord::OperationStarted { .. }) {
                record_base_id(r) == started_id
            } else {
                r.run_id().map(|rid| rid == started_id).unwrap_or(false)
            }
        })
        .collect();

    // 是否已请求中止
    let aborting = operation_records
        .iter()
        .any(|r| matches!(r, LaneRecord::AbortRequested { .. }));

    // abort 后不再有 pending 的 steer / followUp
    let pending_steer = if aborting {
        vec![]
    } else {
        pending_queue_records
            .iter()
            .filter(|r| {
                queue_of(r) == "steer" && r.run_id().map(|rid| rid == started_id).unwrap_or(false)
            })
            .filter_map(target_of)
            .cloned()
            .collect()
    };

    let pending_follow_up = if aborting {
        vec![]
    } else {
        pending_queue_records
            .iter()
            .filter(|r| {
                queue_of(r) == "followUp"
                    && r.run_id().map(|rid| rid == started_id).unwrap_or(false)
            })
            .filter_map(target_of)
            .cloned()
            .collect()
    };

    // 延迟写但尚未落地的条目
    let pending_writes: Vec<ProvisionedEntry> = operation_records
        .iter()
        .filter(|r| {
            matches!(r, LaneRecord::WriteDeferred { .. })
                && !entries_by_id.contains_key(target_id_of(r))
        })
        .filter_map(|r| target_of(r))
        .cloned()
        .collect();

    // Run 意图里声明但尚未落地的初始消息
    let missing_initial_messages = if let LaneRecord::OperationStarted { intent, .. } = &started {
        match intent {
            OperationIntent::Run {
                initial_messages, ..
            } => initial_messages
                .iter()
                .filter(|t| !entries_by_id.contains_key(&t.id))
                .cloned()
                .collect(),
            _ => vec![],
        }
    } else {
        vec![]
    };

    // 最新的 step attempt：若其结果尚未落地，则归约为一个 pending step（需要重放/重试）
    let newest_attempt = operation_records
        .iter()
        .rfind(|r| matches!(r, LaneRecord::StepAttempt { .. }));
    let step = newest_attempt.and_then(|r| match r {
        LaneRecord::StepAttempt {
            step,
            attempt,
            result_entry_id,
            compaction_reason,
            ..
        } => {
            if entries_by_id.contains_key(result_entry_id) {
                None
            } else {
                Some(StepState {
                    kind: step.clone(),
                    attempts: *attempt,
                    result_entry_id: result_entry_id.clone(),
                    compaction_reason: compaction_reason.clone(),
                })
            }
        }
        _ => None,
    });

    // 已消费的条目：Run 的 initial_messages + 非 nextRun 的入队目标
    let mut consumed: HashSet<String> = HashSet::new();
    if let LaneRecord::OperationStarted { intent, .. } = &started
        && let OperationIntent::Run {
            initial_messages, ..
        } = intent
    {
        for t in initial_messages {
            consumed.insert(t.id.clone());
        }
    }

    for record in &operation_records {
        if let LaneRecord::QueueEnqueued { queue, target, .. } = record
            && queue != "nextRun"
        {
            consumed.insert(target.id.clone());
        }
    }

    // 已消费条目里最大的 seq，用于判断 overflow 恢复是否发生在这些条目之后
    let mut newest_consumed_seq = 0u64;
    for id in &consumed {
        if let Some(Entry::Message { base, .. }) = entries_by_id.get(id) {
            newest_consumed_seq = newest_consumed_seq.max(base.seq);
        }
    }

    // overflow 恢复：compaction(overflow) 的 attempt 出现在已消费条目之后
    let overflow_recovery_used = operation_records.iter().any(|r| {
        matches!(
            r,
            LaneRecord::StepAttempt {
                step,
                compaction_reason,
                base,
                ..
            } if step == "compaction"
                && compaction_reason.as_deref() == Some("overflow")
                && base.seq > newest_consumed_seq
        )
    });

    // 最新的「有模型内容贡献」的 own 条目：`context_edit` 只是投影指令、不产生消息，
    // 不应顶掉 terminal_failure / deferred 的判定（恢复省略会在失败 assistant 之后追加它）。
    let newest_own_entry = own_entries
        .iter()
        .rev()
        .find(|e| !e.is_context_edit())
        .cloned();

    let newest_own = derive_newest_own(&own_entries);
    let _deferred = match &newest_own_entry {
        Some(Entry::Message { message, .. })
            if message.role == "assistant"
                && message.stop_reason.as_deref() == Some("deferred") =>
        {
            message.deferred.clone()
        }
        _ => None,
    };

    // 根据意图计算 operation 的 targets（目标是否已落地）
    let mut targets = serde_json::Map::new();
    if let LaneRecord::OperationStarted { intent, .. } = &started {
        match intent {
            OperationIntent::Compaction {
                result_entry_id, ..
            } => {
                targets.insert(
                    "result".into(),
                    Value::Bool(entries_by_id.contains_key(result_entry_id)),
                );
            }
            OperationIntent::Navigation {
                summary_entry_id: Some(id),
                ..
            } => {
                targets.insert(
                    "summary".into(),
                    Value::Bool(entries_by_id.contains_key(id)),
                );
            }
            _ => {}
        }
    }

    // 已声明延迟写的条目 id 集合（用于排除 terminal failure 判定）
    let deferred_write_ids: HashSet<String> = operation_records
        .iter()
        .filter(|r| matches!(r, LaneRecord::WriteDeferred { .. }))
        .filter_map(|r| {
            let id = target_id_of(r);
            if id.is_empty() {
                None
            } else {
                Some(id.to_string())
            }
        })
        .collect();

    // 判定终结性失败：最新 own 条目是 error 的 assistant 消息，且确实由本 operation
    // 的 step 或 deferred_fetch 产生（排除单纯延迟写未完成的场景）
    let mut terminal_failure: Option<TerminalFailureState> = None;
    if let Some(Entry::Message { message, base, .. }) = &newest_own_entry
        && message.role == "assistant"
        && message.stop_reason.as_deref() == Some("error")
        && !deferred_write_ids.contains(&base.id)
    {
        let produced_by_step = operation_records.iter().any(|r| {
                matches!(r, LaneRecord::StepAttempt { result_entry_id, .. } if result_entry_id == &base.id)
            });

        let produced_by_deferred_fetch = operation_records.iter().any(|r| {
            matches!(
                r,
                LaneRecord::UsageRecord {
                    cause,
                    entry_id,
                    ..
                } if cause == "deferred_fetch" && entry_id.as_deref() == Some(&base.id)
            )
        }) || matches!(
            own_entries.get(own_entries.len().wrapping_sub(2)),
            Some(Entry::Message {
                message,
                ..
            }) if message.role == "assistant" && message.stop_reason.as_deref() == Some("deferred")
        );

        if produced_by_step || produced_by_deferred_fetch {
            terminal_failure = Some(TerminalFailureState {
                entry_id: base.id.clone(),
                source: if produced_by_step {
                    "step".into()
                } else {
                    "deferred_fetch".into()
                },
                message: message.clone(),
            });
        }
    }

    // 还原工具批次：从最新含 tool_call 的 assistant 条目 + ToolStarted 记录 + 结果条目
    let tool_batch = {
        // 已声明延迟写的条目 id（这些结果不算"已解决"）
        let dw: HashSet<String> = deferred_write_ids.clone();
        // 找最新一条含 tool_call 的 assistant 条目
        let ass_id = own_entries
            .iter()
            .rev()
            .find(|e| {
                matches!(
                    e,
                    Entry::Message {
                        message,
                        ..
                    } if message.role == "assistant"
                        && message.content.iter().any(|b| matches!(b, ContentBlock::ToolCall { .. }))
                )
            })
            .map(|e| e.id().to_string());

        if let Some(ass_id) = ass_id {
            // 收集该 assistant 条目下每个 tool_index 的 ToolStarted 记录
            let mut starts: HashMap<usize, LaneRecord> = HashMap::new();
            for record in &operation_records {
                if let LaneRecord::ToolStarted {
                    assistant_entry_id,
                    tool_index,
                    ..
                } = record
                    && assistant_entry_id == &ass_id
                {
                    starts.insert(*tool_index, (*record).clone());
                }
            }
            let assistant_entry = own_entries.iter().find(|e| e.id() == ass_id).cloned();
            if let Some(Entry::Message { message, base, .. }) = assistant_entry {
                // 只取工具调用块
                let tool_calls: Vec<ContentBlock> = message
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
                    .cloned()
                    .collect();
                let mut calls = Vec::new();
                let mut unresolved = false;

                // 逐个工具调用：优先用 ToolStarted 指向的结果，否则找之后 seq 更大的 toolResult
                for (tool_index, tc) in tool_calls.iter().enumerate() {
                    let (call_id, call_name, call_args) = match tc {
                        ContentBlock::ToolCall {
                            id,
                            name,
                            arguments,
                            ..
                        } => (id.clone(), name.clone(), arguments.clone()),
                        _ => continue,
                    };
                    let started = starts.get(&tool_index);
                    let started_result = started.and_then(|s| match s {
                        LaneRecord::ToolStarted {
                            result_entry_id, ..
                        } => entries_by_id.get(result_entry_id),
                        _ => None,
                    });
                    let blocked_result = own_entries.iter().find(|e| {
                        matches!(
                            e,
                            Entry::Message {
                                message,
                                base: eb,
                                ..
                            } if message.role == "toolResult"
                                && message.tool_call_id.as_deref() == Some(&call_id)
                                && eb.seq > base.seq
                                && !dw.contains(&eb.id)
                        )
                    });

                    let result = started_result.or(blocked_result);
                    let terminate = result.and_then(|e| match e {
                        Entry::Message { terminate, .. } => *terminate,
                        _ => None,
                    });

                    if result.is_none() {
                        unresolved = true;
                    }

                    calls.push(ToolBatchCall {
                        tool_index,
                        tool_call_id: call_id,
                        tool_name: call_name,
                        arguments: call_args,
                        result_exists: result.is_some(),
                        terminate,
                    });
                }
                Some(ToolBatchState {
                    assistant_entry_id: base.id.clone(),
                    calls,
                    truncated: message.stop_reason.as_deref() == Some("length"),
                    unresolved,
                })
            } else {
                None
            }
        } else {
            None
        }
    };

    Ok(LaneReductionResult {
        lane_state: LaneState {
            lane: input.lane,
            leaf_id: input.leaf_id,
            operation: Some(OperationState {
                id: started_id,
                kind: match &started {
                    LaneRecord::OperationStarted { intent, .. } => match intent {
                        OperationIntent::Run { .. } => "run".into(),
                        OperationIntent::Compaction { .. } => "compaction".into(),
                        OperationIntent::Navigation { .. } => "navigation".into(),
                    },
                    _ => String::new(),
                },
                source_leaf_id: match &started {
                    LaneRecord::OperationStarted { source_leaf_id, .. } => source_leaf_id.clone(),
                    _ => None,
                },
                aborting,
                step,
                tool_batch,
                missing_initial_messages,
                pending_steer,
                pending_follow_up,
                pending_writes,
                overflow_recovery_used,
                newest_own,
                targets: Value::Object(targets),
            }),
            pending_next_run,
        },
        effective_configuration,
        terminal_failure,
    })
}

/// 取队列 / 延迟写记录的目标条目 id；其余记录类型无目标，返回空串。
fn target_id_of(record: &LaneRecord) -> &str {
    match record {
        LaneRecord::QueueEnqueued { target, .. } | LaneRecord::WriteDeferred { target, .. } => {
            &target.id
        }
        _ => "",
    }
}

/// 取 `QueueEnqueued` 记录的队列名；其余记录类型返回空串。
fn queue_of(record: &LaneRecord) -> &str {
    match record {
        LaneRecord::QueueEnqueued { queue, .. } => queue.as_str(),
        _ => "",
    }
}

/// 取队列 / 延迟写记录携带的预置条目；其余记录类型返回 `None`。
fn target_of(record: &LaneRecord) -> Option<&ProvisionedEntry> {
    match record {
        LaneRecord::QueueEnqueued { target, .. } | LaneRecord::WriteDeferred { target, .. } => {
            Some(target)
        }
        _ => None,
    }
}

/// 聚合的会话统计（消息数 / token / 成本），由 usage 记录累加得到。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionStats {
    /// 消息条数。
    pub message_count: usize,
    /// 缓存命中的 token 总量。
    pub cached_tokens: u64,
    /// 未命中缓存的 token 总量。
    pub uncached_tokens: u64,
    /// cached + uncached 的合计。
    pub total_tokens: u64,
    /// 累计成本（provider 计价口径）。
    pub cost_total: f64,
}

/// 日志项：与磁盘行一一对应，用于重放 / 导出 / 回显。
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum LogItem {
    /// 一条内容条目（对应一行 kind=entry）。
    Entry { seq: u64, entry: Entry },
    /// 一条运行记录（对应一行 kind=record）。
    Record { seq: u64, record: LaneRecord },
    /// 一次 lane 指针重设（对应一行 kind=lane）。
    Lane {
        /// 该行的全局序号，与 entry / record 行共用同一计数器。
        seq: u64,
        /// 被重设叶子指针的 lane 名。
        lane: String,
        /// 新的叶子条目 id；`None` 表示清空该 lane 的叶子。
        leaf_id: Option<String>,
    },
    /// 一次会话名变更（对应一行 kind=fact, fact=name）。
    FactName { seq: u64, name: Option<String> },
    /// 一次 label 变更（对应一行 kind=fact, fact=label）。
    FactLabel {
        /// 该行的全局序号，与其他行共用同一计数器。
        seq: u64,
        /// 被标注的条目 id。
        target_id: String,
        /// 标签文本；`None` 表示清除该条目上的标签。
        label: Option<String>,
    },
}

/// fork 位置：在目标条目"之前"（其父处）还是"从该条目开始"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkPosition {
    /// 在目标条目"之前"切开（目标条目属于父分支）。
    Before,
    /// 从目标条目"本身"开始（目标条目成为新分支的第一条）。
    At,
}

/// fork 范围：整树 fork 还是只 fork 某条分支。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ForkScope {
    /// 复制整棵 entry 树，保留所有 lane。
    Tree,
    /// 只复制到指定条目的分支（entry_id + position）。
    Branch {
        /// 分支终点条目 id；`None` 表示整根分支（到根为止）。
        entry_id: Option<String>,
        /// 在目标之前 / 从目标开始切开。
        position: Option<ForkPosition>,
    },
}

/// 持久化会话状态机：磁盘 record-log 的内存投影 + 写路径。
/// 结构：entries 树 + lane 指针 + records（运行记录）+ 全局 facts（name/labels）。
/// 所有写入必须经 `apply_mutation`（严格校验），以此保证内存投影与磁盘日志一致。
#[derive(Debug, Clone, Default)]
pub struct SessionState {
    /// 全局递增序号，下一条 mutation 必须等于 sequence+1
    sequence: u64,
    /// 已用 id，防止重复
    used_ids: HashSet<String>,
    /// 全部转写条目（按写入顺序）
    entries: Vec<Entry>,
    /// id → entry 快速索引
    entries_by_id: HashMap<String, Entry>,
    /// 全部运行记录
    records: Vec<LaneRecord>,
    /// lane → (operation_id → operation_started 记录)：用于跟踪打开的 operation
    open_operations_by_lane: HashMap<String, HashMap<String, LaneRecord>>,
    /// lane 名 → 当前叶子 id（None 表示空 lane）
    lanes: HashMap<String, Option<String>>,
    /// 用于重放 / 导出的按序日志
    log: Vec<LogItem>,
    /// 聚合统计（消息数 / token / 成本）
    stats: SessionStats,
    /// 会话名
    name: Option<String>,
    /// entry_id → label
    labels: HashMap<String, String>,
}

impl SessionState {
    /// 新建空会话：预置一条 "main" lane。
    pub fn new() -> Self {
        let mut lanes = HashMap::new();
        lanes.insert("main".to_string(), None);
        Self {
            lanes,
            ..Default::default()
        }
    }

    /// 下一条 mutation 应使用的 seq
    pub fn next_sequence(&self) -> u64 {
        self.sequence + 1
    }

    /// 列出所有 lane（排序后的 (名, 叶子) 列表）
    pub fn get_lanes(&self) -> Vec<(String, Option<String>)> {
        let mut lanes: Vec<(String, Option<String>)> = self
            .lanes
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        lanes.sort();
        lanes
    }

    /// 取某 lane 的叶子，若 lane 不存在则报损坏错误。
    pub fn require_lane(&self, lane: &str) -> Result<Option<String>, RecordLogError> {
        self.lanes
            .get(lane)
            .cloned()
            .ok_or_else(|| RecordLogError::LaneNotFound {
                message: format!("Lane not found: {lane}"),
            })
    }

    /// 借用版 lane leaf（不构图错误语义，供读取路径使用）
    pub fn lane_leaf(&self, lane: &str) -> Option<&str> {
        self.lanes.get(lane).and_then(|l| l.as_deref())
    }

    /// 按 id 取已落地条目；不存在时返回 `None`。
    pub fn get_entry(&self, id: &str) -> Option<&Entry> {
        self.entries_by_id.get(id)
    }

    /// 指定的 id 是否已在本状态中出现（entry 或 record 均计入）。
    /// 截断/抢救修复的依赖检查用它判断被丢弃行的 id 是否会造成悬空引用。
    pub fn has_id(&self, id: &str) -> bool {
        self.used_ids.contains(id)
    }

    /// 按写入顺序返回全部条目切片。
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// 与 `entries` 等价的别名（保留给偏好 `get_` 前缀的调用方）。
    pub fn get_entries(&self) -> &[Entry] {
        &self.entries
    }

    /// 按写入顺序返回全部运行记录切片。
    pub fn records(&self) -> &[LaneRecord] {
        &self.records
    }

    /// 返回会话名；从未设置或被清除时为 `None`。
    pub fn get_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// 返回指定条目的 label；该条目没有标签时为 `None`。
    pub fn get_label(&self, id: &str) -> Option<&str> {
        self.labels.get(id).map(|s| s.as_str())
    }

    /// 返回由 usage 记录累加出的会话统计（消息数 / token / 成本）。
    pub fn get_stats(&self) -> &SessionStats {
        &self.stats
    }

    /// 逐行应用 mutation：先校验 seq 严格连续，再按类型做链式校验并更新内存投影。
    /// 这是唯一的写入口，保证投影与日志永远一致。
    pub fn apply_mutation(&mut self, m: SessionMutation) -> Result<(), RecordLogError> {
        let seq = mutation_seq(&m);
        // 全局 seq 必须严格连续（从 1 开始），否则拒绝
        if seq != self.sequence + 1 {
            return corrupt(RecordLogError::NonConsecutiveSeq {
                message: format!("has non-consecutive seq {seq}"),
            });
        }

        match m {
            SessionMutation::Entry { lane, entry } => {
                // id 全局唯一
                if self.used_ids.contains(entry.id()) {
                    return corrupt(RecordLogError::DuplicateId {
                        message: format!("contains duplicate id {}", entry.id()),
                    });
                }

                let base = entry_base(&entry);
                // 若带 lane，则 parent 必须恰好是该 lane 的当前叶子（保证链式推进）
                if let Some(lane_name) = lane.as_deref() {
                    let leaf_id =
                        self.lanes
                            .get(lane_name)
                            .ok_or_else(|| RecordLogError::MissingLane {
                                message: format!("references missing lane {lane_name}"),
                            })?;
                    if base.parent_id.as_deref() != leaf_id.as_deref() {
                        return corrupt(RecordLogError::DoesNotChainToLaneLeaf {
                            message: "does not chain to the lane leaf".to_string(),
                        });
                    }
                }

                // parent 必须已存在（除非是根）
                if base
                    .parent_id
                    .as_deref()
                    .is_some_and(|p| !self.entries_by_id.contains_key(p))
                {
                    return corrupt(RecordLogError::MissingParent {
                        message: format!("references missing parent {:?}", base.parent_id),
                    });
                }

                self.sequence = seq;
                self.used_ids.insert(entry.id().to_string());
                self.entries.push(entry.clone());
                self.entries_by_id
                    .insert(entry.id().to_string(), entry.clone());

                // 推进 lane 叶子指针
                if let Some(lane_name) = lane.as_deref() {
                    self.lanes
                        .insert(lane_name.to_string(), Some(entry.id().to_string()));
                }

                self.log.push(LogItem::Entry {
                    seq,
                    entry: entry.clone(),
                });

                if matches!(entry, Entry::Message { .. }) {
                    self.stats.message_count += 1;
                }
            }
            SessionMutation::Record { record } => {
                let lane = record.lane().to_string();
                // 引用的 lane 必须存在
                if !self.lanes.contains_key(&lane) {
                    return corrupt(RecordLogError::MissingLane {
                        message: format!("references missing lane {lane}"),
                    });
                }

                // id 全局唯一
                if self.used_ids.contains(record.id()) {
                    return corrupt(RecordLogError::DuplicateId {
                        message: format!("contains duplicate id {}", record.id()),
                    });
                }

                self.sequence = seq;
                self.used_ids.insert(record.id().to_string());
                let op_id = record.id().to_string();
                let is_start = matches!(record, LaneRecord::OperationStarted { .. });
                let is_finish = matches!(record, LaneRecord::OperationFinished { .. });

                // 维护打开的 operation 集合：start 加入，finish 移除
                if is_start {
                    self.open_operations_by_lane
                        .entry(lane.clone())
                        .or_default()
                        .insert(op_id.clone(), record.clone());
                } else if is_finish
                    && let LaneRecord::OperationFinished { run_id, .. } = &record
                    && let Some(open) = self.open_operations_by_lane.get_mut(&lane)
                {
                    open.remove(run_id);
                }

                self.records.push(record.clone());
                self.log.push(LogItem::Record {
                    seq,
                    record: record.clone(),
                });

                // usage 记录聚合 token / 成本统计
                if let LaneRecord::UsageRecord { usage, .. } = &record {
                    self.stats.cached_tokens += u64::from(usage.cache_read);
                    self.stats.uncached_tokens +=
                        u64::from(usage.input) + u64::from(usage.cache_write);
                    self.stats.total_tokens += u64::from(usage.total_tokens);
                    self.stats.cost_total += usage.cost.total;
                }
            }
            SessionMutation::Lane {
                seq: _seq,
                lane,
                leaf_id,
            } => {
                // 叶子指针必须指向已存在的条目
                if leaf_id
                    .as_deref()
                    .is_some_and(|id| !self.entries_by_id.contains_key(id))
                {
                    return corrupt(RecordLogError::MissingLaneTarget {
                        message: format!("references missing lane target {:?}", leaf_id),
                    });
                }

                self.sequence = seq;
                self.lanes.insert(lane.clone(), leaf_id.clone());
                self.log.push(LogItem::Lane { seq, lane, leaf_id });
            }
            SessionMutation::Fact {
                seq: _seq,
                fact,
                name,
                target_id,
                label,
            } => {
                // label 目标必须存在
                if fact == "label"
                    && target_id
                        .as_deref()
                        .is_some_and(|id| !self.entries_by_id.contains_key(id))
                {
                    return corrupt(RecordLogError::MissingLabelTarget {
                        message: format!("references missing label target {:?}", target_id),
                    });
                }

                self.sequence = seq;
                match fact.as_str() {
                    "name" => {
                        self.name = name.clone();
                        self.log.push(LogItem::FactName { seq, name });
                    }
                    "label" => {
                        if let Some(t) = target_id {
                            match label.clone() {
                                Some(l) => {
                                    self.labels.insert(t.clone(), l.clone());
                                }
                                None => {
                                    self.labels.remove(&t);
                                }
                            }
                            self.log.push(LogItem::FactLabel {
                                seq,
                                target_id: t,
                                label,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// 从 start 沿 parentId 走到根（或停在指定 id / 类型），并检测环。
    /// 返回从 start 到根（含）的路径，新条目在前。
    fn walk_to_root<'a>(
        &'a self,
        start: &str,
        stop_at_id: Option<&str>,
        stop_at_type: Option<&str>,
    ) -> Result<Vec<&'a Entry>, RecordLogError> {
        let mut visited = HashSet::new();
        let mut current =
            self.entries_by_id
                .get(start)
                .ok_or_else(|| RecordLogError::EntryNotFound {
                    message: format!("Entry not found: {start}"),
                })?;
        let mut path = Vec::new();

        loop {
            if visited.contains(current.id()) {
                return corrupt(RecordLogError::Cycle {
                    message: format!("Session branch contains a cycle at {}", current.id()),
                });
            }
            visited.insert(current.id().to_string());
            path.push(current);

            let base = entry_base(current);
            let Some(parent_id) = base.parent_id.as_deref() else {
                break;
            };

            if parent_id == "null" || parent_id.is_empty() {
                break;
            }

            if stop_at_id.is_some_and(|id| current.id() == id)
                || stop_at_type.is_some_and(|t| entry_type_name(current) == t)
            {
                break;
            }

            current =
                self.entries_by_id
                    .get(parent_id)
                    .ok_or_else(|| RecordLogError::EntryNotFound {
                        message: format!("Entry not found: {parent_id}"),
                    })?;
        }
        Ok(path)
    }

    /// 取某条分支上的条目序列。oldest_first=true 时根在前，否则叶子在前。
    /// 可选的 stop_at_id / stop_at_type 用于截断到某一祖先。
    pub fn find_entries_on_branch(
        &self,
        start: &str,
        stop_at_id: Option<&str>,
        stop_at_type: Option<&str>,
        oldest_first: bool,
    ) -> Result<Vec<Entry>, RecordLogError> {
        let path = self.walk_to_root(start, stop_at_id, stop_at_type)?;
        let mut entries: Vec<Entry> = path.into_iter().cloned().collect();
        if oldest_first {
            entries.reverse();
        }
        Ok(entries)
    }

    /// 取某 lane 当前打开的 operation（按 seq 倒序，可选截断条数）。
    pub fn find_open_operations(&self, lane: &str, limit: Option<usize>) -> Vec<LaneRecord> {
        let mut ops: Vec<LaneRecord> = match self.open_operations_by_lane.get(lane) {
            Some(map) => map.values().cloned().collect(),
            None => Vec::new(),
        };
        ops.sort_by_key(|b| std::cmp::Reverse(b.seq()));
        if let Some(limit) = limit {
            ops.truncate(limit);
        }
        ops
    }

    /// 生成 fork 所需的 mutation 序列（**不复制 records**，只复制 entry 子集 + lane 指针 + facts）。
    /// 返回的 mutation 可写入一个新会话文件，从而产生一个独立分支。
    pub fn create_fork_mutations(
        &self,
        scope: &ForkScope,
    ) -> Result<Vec<SessionMutation>, RecordLogError> {
        // 依据 fork 范围决定要复制的条目与 fork 后的 lane 指针
        let (copied_entries, fork_lanes) = match scope {
            // 整树 fork：复制全部条目，保留所有 lane 指针
            ForkScope::Tree => (self.entries.clone(), self.get_lanes()),
            // 分支 fork：只复制到目标条目（或其父）为止的分支
            ForkScope::Branch { entry_id, position } => {
                let selected_entry_id = match entry_id {
                    Some(id) => Some(id.clone()),
                    None => self.require_lane("main")?,
                };
                let mut target_id: Option<String> = None;

                if let Some(id) = selected_entry_id {
                    let entry = self.entries_by_id.get(&id).ok_or_else(|| {
                        RecordLogError::EntryNotFound {
                            message: format!("Entry not found: {id}"),
                        }
                    })?;

                    // fork 目标必须是消息条目
                    if !matches!(entry, Entry::Message { .. }) {
                        return corrupt(RecordLogError::ForkTargetNotMessage {
                            message: format!("Fork target is not a message entry: {id}"),
                        });
                    }
                    // position：At=从目标本身开始，Before=从目标的父开始
                    let pos = match position {
                        Some(p) => *p,
                        None => {
                            if entry_id.is_none() {
                                ForkPosition::At
                            } else {
                                ForkPosition::Before
                            }
                        }
                    };
                    target_id = match pos {
                        ForkPosition::At => Some(id.clone()),
                        ForkPosition::Before => entry_base(entry).parent_id.clone(),
                    };
                }

                let copied = match &target_id {
                    Some(t) => self.find_entries_on_branch(t, None, None, true)?,
                    None => Vec::new(),
                };
                (copied, vec![("main".to_string(), target_id)])
            }
        };

        let mut mutations: Vec<SessionMutation> = Vec::new();
        let mut sequence = 1u64;
        // 先收集 facts（labels 引用 copied_entries，需在 move 前）
        let mut fact_mutations: Vec<SessionMutation> = Vec::new();
        if let Some(name) = &self.name {
            fact_mutations.push(SessionMutation::Fact {
                seq: 0,
                fact: "name".to_string(),
                name: Some(name.clone()),
                target_id: None,
                label: None,
            });
        }

        // 复制被 fork 条目所带的 label
        for entry in &copied_entries {
            if let Some(label) = self.labels.get(entry.id()) {
                fact_mutations.push(SessionMutation::Fact {
                    seq: 0,
                    fact: "label".to_string(),
                    name: None,
                    target_id: Some(entry.id().to_string()),
                    label: Some(label.clone()),
                });
            }
        }

        // 复制的条目重新分配 seq（从 1 开始连续）
        for mut entry in copied_entries {
            entry_base_mut(&mut entry).seq = sequence;
            mutations.push(SessionMutation::Entry { lane: None, entry });
            sequence += 1;
        }

        // 写入 fork 后的 lane 指针
        for (lane, leaf_id) in fork_lanes {
            mutations.push(SessionMutation::Lane {
                seq: sequence,
                lane,
                leaf_id,
            });
            sequence += 1;
        }

        // 最后写入 facts（name / labels）
        for mut fact in fact_mutations {
            let SessionMutation::Fact { seq, .. } = &mut fact else {
                continue;
            };
            *seq = sequence;
            mutations.push(fact);
            sequence += 1;
        }
        Ok(mutations)
    }

    /// 某 lane 当前活动分支的 mutation 序列（重新编号 + 末尾 lane 指针 + labels）。
    ///
    /// 与 [`Self::create_fork_mutations`] 写出的文件同构：header 之外的行直接拼接
    /// 就能得到一份可被 `Session::open` 导入的分支会话文件（供 `/bug` 附 transcript）。
    /// 与 fork 的差别：**不要求分支末端是 message 条目**（尾部可能只是配置变更条目）。
    pub fn branch_mutations(&self, lane: &str) -> Result<Vec<SessionMutation>, RecordLogError> {
        let leaf = self.require_lane(lane)?;
        let entries = match &leaf {
            Some(leaf_id) => self.find_entries_on_branch(leaf_id, None, None, true)?,
            None => Vec::new(),
        };

        let mut labels: Vec<(String, String)> = Vec::new();
        for entry in &entries {
            if let Some(label) = self.labels.get(entry.id()) {
                labels.push((entry.id().to_string(), label.clone()));
            }
        }

        let mut mutations: Vec<SessionMutation> = Vec::new();
        let mut sequence = 1u64;
        for mut entry in entries {
            entry_base_mut(&mut entry).seq = sequence;
            mutations.push(SessionMutation::Entry { lane: None, entry });
            sequence += 1;
        }

        mutations.push(SessionMutation::Lane {
            seq: sequence,
            lane: lane.to_string(),
            leaf_id: leaf,
        });
        sequence += 1;

        for (target_id, label) in labels {
            mutations.push(SessionMutation::Fact {
                seq: sequence,
                fact: "label".to_string(),
                name: None,
                target_id: Some(target_id),
                label: Some(label),
            });
            sequence += 1;
        }
        Ok(mutations)
    }
}

/// 从 mutation 里取全局 seq（entry/record 在嵌套结构里，lane/fact 直接携带）。
fn mutation_seq(m: &SessionMutation) -> u64 {
    match m {
        SessionMutation::Entry { entry, .. } => entry.seq(),
        SessionMutation::Record { record } => record.seq(),
        SessionMutation::Lane { seq, .. } | SessionMutation::Fact { seq, .. } => *seq,
    }
}

/// 可变借用 record 的基字段（各变体共享）。
fn record_base_mut(record: &mut LaneRecord) -> &mut RecordBase {
    match record {
        LaneRecord::OperationStarted { base, .. }
        | LaneRecord::AbortRequested { base, .. }
        | LaneRecord::OperationFinished { base, .. }
        | LaneRecord::StepAttempt { base, .. }
        | LaneRecord::ToolStarted { base, .. }
        | LaneRecord::QueueEnqueued { base, .. }
        | LaneRecord::QueueCancelled { base, .. }
        | LaneRecord::WriteDeferred { base, .. }
        | LaneRecord::UsageRecord { base, .. } => base,
    }
}

/// 将 mutation 的全局 seq 平移 `delta`（负 = 重排到更早）。
/// 供截断/抢救修复在跳过损坏行后保持 seq 严格连续；下溢（异常输入）返回 None。
/// 返回平移后的新 seq。
pub fn shift_mutation_seq(m: &mut SessionMutation, delta: i64) -> Option<u64> {
    let new_seq = mutation_seq(m).checked_add_signed(delta)?;
    match m {
        SessionMutation::Entry { entry, .. } => entry_base_mut(entry).seq = new_seq,
        SessionMutation::Record { record } => record_base_mut(record).seq = new_seq,
        SessionMutation::Lane { seq, .. } | SessionMutation::Fact { seq, .. } => *seq = new_seq,
    }
    Some(new_seq)
}

/// 将 mutation 的全局 seq 直接设为 `seq`。
/// 供 seq 跳号修复使用：行内容自洽、仅编号与期望不符时，重写编号即可整行保留。
pub fn set_mutation_seq(m: &mut SessionMutation, seq: u64) {
    match m {
        SessionMutation::Entry { entry, .. } => entry_base_mut(entry).seq = seq,
        SessionMutation::Record { record } => record_base_mut(record).seq = seq,
        SessionMutation::Lane { seq: s, .. } | SessionMutation::Fact { seq: s, .. } => *s = seq,
    }
}

/// entry 变体的类型名（与 serde tag 的 snake_case 一致）。
fn entry_type_name(entry: &Entry) -> &'static str {
    match entry {
        Entry::Message { .. } => "message",
        Entry::ModelChange { .. } => "model_change",
        Entry::ThinkingLevelChange { .. } => "thinking_level_change",
        Entry::ActiveToolsChange { .. } => "active_tools_change",
        Entry::Compaction { .. } => "compaction",
        Entry::BranchSummary { .. } => "branch_summary",
        Entry::ContextEdit { .. } => "context_edit",
        Entry::Custom { .. } => "custom",
    }
}

/// 可变借用 entry 的基字段（各变体共享）。
fn entry_base_mut(entry: &mut Entry) -> &mut EntryBase {
    match entry {
        Entry::Message { base, .. }
        | Entry::ModelChange { base, .. }
        | Entry::ThinkingLevelChange { base, .. }
        | Entry::ActiveToolsChange { base, .. }
        | Entry::Compaction { base, .. }
        | Entry::BranchSummary { base, .. }
        | Entry::ContextEdit { base, .. }
        | Entry::Custom { base, .. } => base,
    }
}

/// 整体替换 entry 的基字段（fork 时用于重设 seq）。
pub fn set_entry_base(entry: &mut Entry, base: EntryBase) {
    match entry {
        Entry::Message { base: b, .. }
        | Entry::ModelChange { base: b, .. }
        | Entry::ThinkingLevelChange { base: b, .. }
        | Entry::ActiveToolsChange { base: b, .. }
        | Entry::Compaction { base: b, .. }
        | Entry::BranchSummary { base: b, .. }
        | Entry::ContextEdit { base: b, .. }
        | Entry::Custom { base: b, .. } => *b = base,
    }
}

/// 整体替换 record 的基字段（fork / 重写时用于重设 seq）。
pub fn set_record_base(record: &mut LaneRecord, base: RecordBase) {
    match record {
        LaneRecord::OperationStarted { base: b, .. }
        | LaneRecord::AbortRequested { base: b, .. }
        | LaneRecord::OperationFinished { base: b, .. }
        | LaneRecord::StepAttempt { base: b, .. }
        | LaneRecord::ToolStarted { base: b, .. }
        | LaneRecord::QueueEnqueued { base: b, .. }
        | LaneRecord::QueueCancelled { base: b, .. }
        | LaneRecord::WriteDeferred { base: b, .. }
        | LaneRecord::UsageRecord { base: b, .. } => *b = base,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::ContentBlock;

    fn msg_entry(id: &str, seq: u64, role: &str, text: &str) -> Entry {
        Entry::Message {
            base: EntryBase {
                id: id.into(),
                seq,
                timestamp: 0,
                parent_id: None,
            },
            message: if role == "user" {
                AgentMessage::user_text(text)
            } else {
                let mut m = AgentMessage::user_text(text);
                m.role = role.to_string();
                m
            },
            terminate: None,
        }
    }

    fn assistant_toolcall_entry(id: &str, seq: u64, calls: Vec<(&str, &str)>) -> Entry {
        let content: Vec<ContentBlock> = calls
            .into_iter()
            .map(|(cid, name)| ContentBlock::ToolCall {
                id: cid.into(),
                name: name.into(),
                arguments: serde_json::json!({}),
                thought_signature: None,
                namespace: None,
            })
            .collect();
        let mut m = AgentMessage::user_text("");
        m.role = "assistant".into();
        m.stop_reason = Some("toolUse".into());
        m.content = content;
        Entry::Message {
            base: EntryBase {
                id: id.into(),
                seq,
                timestamp: 0,
                parent_id: None,
            },
            message: m,
            terminate: None,
        }
    }

    /// 兼容旧会话文件：本字段是本轮新增的，旧 `usage_record` 行里没有 `provider`/`model`
    /// （缺省为 None，不是解析失败）
    #[test]
    fn usage_record_without_attribution_still_loads() {
        let legacy = serde_json::json!({
            "type": "usage_record",
            "id": "r1",
            "seq": 1,
            "lane": "main",
            "timestamp": 0,
            "cause": "cache_warm",
            "usage": { "input": 1, "output": 2, "cacheRead": 3, "cacheWrite": 4, "totalTokens": 10 }
        });
        let record: LaneRecord = serde_json::from_value(legacy).unwrap();
        match record {
            LaneRecord::UsageRecord {
                provider,
                model,
                usage,
                ..
            } => {
                assert!(provider.is_none() && model.is_none());
                assert_eq!(usage.total_tokens, 10);
            }
            other => panic!("expected usage_record, got {other:?}"),
        }
    }

    fn op_started(id: &str, seq: u64, lane: &str) -> LaneRecord {
        LaneRecord::OperationStarted {
            base: RecordBase {
                id: id.into(),
                seq,
                lane: lane.into(),
                timestamp: 0,
            },
            source_leaf_id: None,
            intent: OperationIntent::Run {
                original_prompt: vec![AgentMessage::user_text("hi")],
                initial_messages: vec![],
                system_prompt_override: None,
            },
        }
    }

    fn op_finished(id: &str, run_id: &str, seq: u64, lane: &str, outcome: &str) -> LaneRecord {
        LaneRecord::OperationFinished {
            base: RecordBase {
                id: id.into(),
                seq,
                lane: lane.into(),
                timestamp: 0,
            },
            run_id: run_id.into(),
            outcome: outcome.into(),
            error: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn step_attempt(
        id: &str,
        run_id: &str,
        seq: u64,
        lane: &str,
        step: &str,
        attempt: u32,
        result_entry_id: &str,
        compaction_reason: Option<&str>,
    ) -> LaneRecord {
        LaneRecord::StepAttempt {
            base: RecordBase {
                id: id.into(),
                seq,
                lane: lane.into(),
                timestamp: 0,
            },
            run_id: run_id.into(),
            step: step.into(),
            attempt,
            result_entry_id: result_entry_id.into(),
            compaction_reason: compaction_reason.map(|s| s.to_string()),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn tool_started(
        id: &str,
        run_id: &str,
        seq: u64,
        lane: &str,
        assistant_entry_id: &str,
        tool_index: usize,
        tool_call_id: &str,
        tool_name: &str,
        result_entry_id: &str,
    ) -> LaneRecord {
        LaneRecord::ToolStarted {
            base: RecordBase {
                id: id.into(),
                seq,
                lane: lane.into(),
                timestamp: 0,
            },
            run_id: run_id.into(),
            assistant_entry_id: assistant_entry_id.into(),
            tool_index,
            tool_call_id: tool_call_id.into(),
            tool_name: tool_name.into(),
            effective_args: serde_json::json!({}),
            result_entry_id: result_entry_id.into(),
            replay: "never".into(),
        }
    }

    fn queue_enqueued(
        id: &str,
        run_id: &str,
        seq: u64,
        lane: &str,
        queue: &str,
        target: ProvisionedEntry,
    ) -> LaneRecord {
        LaneRecord::QueueEnqueued {
            base: RecordBase {
                id: id.into(),
                seq,
                lane: lane.into(),
                timestamp: 0,
            },
            queue: queue.into(),
            run_id: if run_id.is_empty() {
                None
            } else {
                Some(run_id.into())
            },
            target,
        }
    }

    fn provisioned(id: &str) -> ProvisionedEntry {
        ProvisionedEntry {
            id: id.into(),
            payload: serde_json::json!({ "type": "message", "message": { "role": "user", "content": [] } }),
        }
    }

    fn base_input(lane: &str) -> LaneReductionInput {
        LaneReductionInput {
            lane: lane.into(),
            leaf_id: None,
            open_operations: vec![],
            records: vec![],
            entries: vec![],
            own_entries: vec![],
            configuration_entries: vec![],
            defaults: EffectiveLaneConfiguration {
                provider: "p".into(),
                model_id: "m".into(),
                thinking_level: "off".into(),
                active_tool_names: vec![],
            },
        }
    }

    #[test]
    fn idle_lane_reduces_empty_operation() {
        let result = reduce_lane_state(base_input("l1")).unwrap();
        assert!(result.lane_state.operation.is_none());
        assert!(result.lane_state.pending_next_run.is_empty());
        assert_eq!(result.effective_configuration.provider, "p");
        assert!(result.terminal_failure.is_none());
    }

    #[test]
    fn effective_configuration_derived_from_entries() {
        let mut input = base_input("l1");
        input.configuration_entries = vec![
            Entry::ModelChange {
                base: EntryBase {
                    id: "m1".into(),
                    seq: 1,
                    timestamp: 0,
                    parent_id: None,
                },
                provider: "anthropic".into(),
                model_id: "claude".into(),
            },
            Entry::ThinkingLevelChange {
                base: EntryBase {
                    id: "t1".into(),
                    seq: 2,
                    timestamp: 0,
                    parent_id: None,
                },
                thinking_level: "high".into(),
            },
            Entry::ActiveToolsChange {
                base: EntryBase {
                    id: "a1".into(),
                    seq: 3,
                    timestamp: 0,
                    parent_id: None,
                },
                active_tool_names: vec!["read".into(), "bash".into()],
            },
        ];
        let result = reduce_lane_state(input).unwrap();
        let ctx = result.effective_configuration;
        assert_eq!(ctx.provider, "anthropic");
        assert_eq!(ctx.model_id, "claude");
        assert_eq!(ctx.thinking_level, "high");
        assert_eq!(
            ctx.active_tool_names,
            vec!["read".to_string(), "bash".to_string()]
        );
    }

    #[test]
    fn open_operation_with_step_attempts_reduces() {
        let mut input = base_input("l1");
        let run_id = "run1";
        input.open_operations = vec![op_started("run1", 1, "l1")];
        input.records = vec![
            step_attempt("s1", run_id, 5, "l1", "assistant", 1, "e-assistant-1", None),
            op_finished("f1", run_id, 9, "l1", "completed"),
        ];
        input.own_entries = vec![msg_entry("e-user", 2, "user", "hi")];
        input.entries = vec![assistant_toolcall_entry("e-assistant-1", 6, vec![])];
        let result = reduce_lane_state(input).unwrap();
        let st = result.lane_state.operation.unwrap();
        assert_eq!(st.kind, "run");
        assert!(!st.aborting);
        assert!(st.step.is_none());
        assert!(!st.overflow_recovery_used);
        assert!(result.terminal_failure.is_none());
    }

    #[test]
    fn unresolved_step_reduces_to_pending_step() {
        let mut input = base_input("l1");
        let run_id = "run1";
        input.open_operations = vec![op_started("run1", 1, "l1")];
        input.records = vec![step_attempt(
            "s1",
            run_id,
            5,
            "l1",
            "assistant",
            1,
            "e-missing",
            None,
        )];
        input.own_entries = vec![msg_entry("e-user", 2, "user", "hi")];
        let result = reduce_lane_state(input).unwrap();
        let st = result.lane_state.operation.unwrap();
        let step = st.step.unwrap();
        assert_eq!(step.kind, "assistant");
        assert_eq!(step.attempts, 1);
        assert_eq!(step.result_entry_id, "e-missing");
    }

    #[test]
    fn tool_batch_recovered_from_tool_started_records() {
        let mut input = base_input("l1");
        let run_id = "run1";
        input.open_operations = vec![op_started("run1", 1, "l1")];
        input.own_entries = vec![
            assistant_toolcall_entry("e-assistant", 3, vec![("call-1", "read")]),
            msg_entry("e-tr", 4, "toolResult", "file contents"),
        ];
        if let Entry::Message { message, .. } = &mut input.own_entries[1] {
            message.tool_call_id = Some("call-1".into());
            message.tool_name = Some("read".into());
        }
        // validateRecordLog 只读 entries：assistant 条目需在切片 entries 中
        input.entries = vec![assistant_toolcall_entry(
            "e-assistant",
            3,
            vec![("call-1", "read")],
        )];
        input.records = vec![tool_started(
            "t1",
            run_id,
            6,
            "l1",
            "e-assistant",
            0,
            "call-1",
            "read",
            "e-tr",
        )];
        let result = reduce_lane_state(input).unwrap();
        let st = result.lane_state.operation.unwrap();
        let batch = st.tool_batch.unwrap();
        assert_eq!(batch.assistant_entry_id, "e-assistant");
        assert_eq!(batch.calls.len(), 1);
        assert_eq!(batch.calls[0].tool_call_id, "call-1");
        assert!(batch.calls[0].result_exists);
        assert!(!batch.unresolved);
        assert!(!batch.truncated);
        assert!(result.terminal_failure.is_none());
    }

    #[test]
    fn pending_steer_follow_up_and_next_run_reduced() {
        let mut input = base_input("l1");
        let run_id = "run1";
        input.open_operations = vec![op_started("run1", 1, "l1")];
        input.records = vec![
            queue_enqueued("q1", run_id, 7, "l1", "steer", provisioned("e-steer")),
            queue_enqueued("q2", run_id, 8, "l1", "followUp", provisioned("e-fu")),
            queue_enqueued("q3", "", 9, "l1", "nextRun", provisioned("e-nr")),
        ];
        input.own_entries = vec![msg_entry("e-user", 2, "user", "hi")];
        let result = reduce_lane_state(input).unwrap();
        let st = result.lane_state.operation.unwrap();
        assert_eq!(st.pending_steer.len(), 1);
        assert_eq!(st.pending_steer[0].id, "e-steer");
        assert_eq!(st.pending_follow_up.len(), 1);
        assert_eq!(st.pending_follow_up[0].id, "e-fu");
        assert_eq!(result.lane_state.pending_next_run.len(), 1);
        assert_eq!(result.lane_state.pending_next_run[0].id, "e-nr");
    }

    #[test]
    fn aborted_operation_clears_pending_queues() {
        let mut input = base_input("l1");
        let run_id = "run1";
        input.open_operations = vec![op_started("run1", 1, "l1")];
        input.records = vec![
            queue_enqueued("q1", run_id, 5, "l1", "steer", provisioned("e-steer")),
            LaneRecord::AbortRequested {
                base: RecordBase {
                    id: "ab1".into(),
                    seq: 6,
                    lane: "l1".into(),
                    timestamp: 0,
                },
                run_id: run_id.into(),
            },
        ];
        input.own_entries = vec![msg_entry("e-user", 2, "user", "hi")];
        let result = reduce_lane_state(input).unwrap();
        let st = result.lane_state.operation.unwrap();
        assert!(st.aborting);
        assert!(st.pending_steer.is_empty());
    }

    #[test]
    fn terminal_failure_from_error_step() {
        let mut input = base_input("l1");
        let run_id = "run1";
        input.open_operations = vec![op_started("run1", 1, "l1")];
        let mut failure = AgentMessage::user_text("");
        failure.role = "assistant".into();
        failure.stop_reason = Some("error".into());
        failure.error_message = Some("boom".into());
        input.own_entries = vec![
            msg_entry("e-user", 2, "user", "hi"),
            Entry::Message {
                base: EntryBase {
                    id: "e-err".into(),
                    seq: 5,
                    timestamp: 0,
                    parent_id: None,
                },
                message: failure,
                terminate: None,
            },
        ];
        input.records = vec![step_attempt(
            "s1",
            run_id,
            5,
            "l1",
            "assistant",
            1,
            "e-err",
            None,
        )];
        // 注意：step_attempt 的 result 是 e-err，且 e-err 在 own_entries（已落地）→ step 归约为 None；
        // terminal failure 判定只看 newestOwn=error 且 producedByStep 由 records 命中。
        let result = reduce_lane_state(input).unwrap();
        let tf = result.terminal_failure.unwrap();
        assert_eq!(tf.entry_id, "e-err");
        assert_eq!(tf.source, "step");
        assert_eq!(tf.message.error_message.as_deref(), Some("boom"));
    }

    #[test]
    fn terminal_failure_survives_trailing_context_edit() {
        // 恢复省略（context_edit）追加在失败 assistant 之后，不应让终结失败判定消失
        let mut input = base_input("l1");
        let run_id = "run1";
        input.open_operations = vec![op_started("run1", 1, "l1")];
        let mut failure = AgentMessage::user_text("");
        failure.role = "assistant".into();
        failure.stop_reason = Some("error".into());
        failure.error_message = Some("boom".into());
        input.own_entries = vec![
            msg_entry("e-user", 2, "user", "hi"),
            Entry::Message {
                base: EntryBase {
                    id: "e-err".into(),
                    seq: 5,
                    timestamp: 0,
                    parent_id: None,
                },
                message: failure,
                terminate: None,
            },
            Entry::ContextEdit {
                base: EntryBase {
                    id: "e-edit".into(),
                    seq: 6,
                    timestamp: 0,
                    parent_id: Some("e-err".into()),
                },
                target_id: "e-err".into(),
                replacement: None,
            },
        ];
        input.records = vec![step_attempt(
            "s1",
            run_id,
            5,
            "l1",
            "assistant",
            1,
            "e-err",
            None,
        )];
        let result = reduce_lane_state(input).unwrap();
        let tf = result
            .terminal_failure
            .expect("context_edit 不应顶掉终结失败判定");
        assert_eq!(tf.entry_id, "e-err");
    }

    #[test]
    fn corruption_unknown_operation() {
        let mut input = base_input("l1");
        input.records = vec![step_attempt(
            "s1",
            "no-such-run",
            5,
            "l1",
            "assistant",
            1,
            "e-x",
            None,
        )];
        let err = reduce_lane_state(input).unwrap_err();
        assert!(
            matches!(err, RecordLogError::UnknownOperation { .. }),
            "{err}"
        );
    }

    #[test]
    fn multiple_open_operations_takes_newest_and_old_are_zombies() {
        // 崩溃残留：两个未关闭的 operation。不再报损坏，而是取 seq 最新的继续，
        // 旧的视为僵尸（其记录仍参与日志自洽校验）。
        let mut input = base_input("l1");
        input.open_operations = vec![op_started("op1", 1, "l1"), op_started("op2", 2, "l1")];
        input.own_entries = vec![msg_entry("e-user", 3, "user", "hi")];
        input.records = vec![
            // op1（旧，僵尸）留有尚未落地的 step
            step_attempt("s1", "op1", 4, "l1", "assistant", 1, "e-missing-1", None),
            // op2（最新）的 step 已落地
            step_attempt("s2", "op2", 5, "l1", "assistant", 1, "e-ok", None),
        ];
        input.entries = vec![msg_entry("e-ok", 6, "assistant", "done")];
        let result = reduce_lane_state(input).unwrap();
        let st = result.lane_state.operation.unwrap();
        assert_eq!(st.id, "op2");
        // 只归约最新 op：op1 的 pending step 不算进来，op2 的 step 已落地 → 无 pending step
        assert!(st.step.is_none());
        // 最新 own 条目不是 error → 无 terminal failure
        assert!(result.terminal_failure.is_none());
    }

    #[test]
    fn multiple_open_operations_oldest_step_is_ignored() {
        // 僵尸 op 的未落地 step 不应污染最新 op 的归约结果。
        let mut input = base_input("l1");
        input.open_operations = vec![op_started("op1", 1, "l1"), op_started("op2", 2, "l1")];
        input.own_entries = vec![msg_entry("e-user", 3, "user", "hi")];
        input.records = vec![step_attempt(
            "s1",
            "op2",
            7,
            "l1",
            "assistant",
            1,
            "e-missing-2",
            None,
        )];
        let result = reduce_lane_state(input).unwrap();
        let st = result.lane_state.operation.unwrap();
        assert_eq!(st.id, "op2");
        let step = st.step.unwrap();
        assert_eq!(step.result_entry_id, "e-missing-2");
    }

    #[test]
    fn corruption_non_consecutive_attempt() {
        let mut input = base_input("l1");
        let run_id = "run1";
        input.open_operations = vec![op_started("run1", 1, "l1")];
        input.records = vec![
            step_attempt("s1", run_id, 5, "l1", "assistant", 1, "e-1", None),
            step_attempt("s2", run_id, 7, "l1", "assistant", 3, "e-2", None),
        ];
        let err = reduce_lane_state(input).unwrap_err();
        assert!(
            matches!(err, RecordLogError::NonConsecutiveAttempt { .. }),
            "{err}"
        );
    }

    #[test]
    fn corruption_tool_call_mismatch() {
        let mut input = base_input("l1");
        let run_id = "run1";
        input.open_operations = vec![op_started("run1", 1, "l1")];
        input.own_entries = vec![assistant_toolcall_entry(
            "e-assistant",
            3,
            vec![("call-1", "read")],
        )];
        input.entries = vec![assistant_toolcall_entry(
            "e-assistant",
            3,
            vec![("call-1", "read")],
        )];
        input.records = vec![tool_started(
            "t1",
            run_id,
            6,
            "l1",
            "e-assistant",
            0,
            "WRONG-CALL",
            "read",
            "e-tr",
        )];
        let err = reduce_lane_state(input).unwrap_err();
        assert!(
            matches!(err, RecordLogError::ToolCallMismatch { .. }),
            "{err}"
        );
    }

    #[test]
    fn corruption_invalid_compaction_reason() {
        let mut input = base_input("l1");
        let run_id = "run1";
        input.open_operations = vec![op_started("run1", 1, "l1")];
        input.records = vec![step_attempt(
            "s1",
            run_id,
            5,
            "l1",
            "compaction",
            1,
            "e-c",
            Some("bogus"),
        )];
        let err = reduce_lane_state(input).unwrap_err();
        assert!(
            matches!(err, RecordLogError::InvalidCompactionReason { .. }),
            "{err}"
        );
    }

    #[test]
    fn corruption_record_after_finish() {
        let mut input = base_input("l1");
        let run_id = "run1";
        input.open_operations = vec![op_started("run1", 1, "l1")];
        input.records = vec![
            op_finished("f1", run_id, 9, "l1", "completed"),
            step_attempt("s1", run_id, 10, "l1", "assistant", 1, "e-1", None),
        ];
        let err = reduce_lane_state(input).unwrap_err();
        assert!(
            matches!(err, RecordLogError::RecordAfterFinish { .. }),
            "{err}"
        );
    }

    #[test]
    fn json_roundtrip_records() {
        let records = vec![
            op_started("op1", 1, "l1"),
            step_attempt("s1", "run1", 5, "l1", "assistant", 1, "e-1", None),
            op_finished("f1", "run1", 9, "l1", "completed"),
        ];
        for r in &records {
            let v = serde_json::to_value(r).unwrap();
            let back: LaneRecord = serde_json::from_value(v).unwrap();
            assert_eq!(r.seq(), back.seq());
        }
    }

    #[test]
    fn corruption_duplicate_tool_invocation() {
        let mut input = base_input("l1");
        let run_id = "run1";
        input.open_operations = vec![op_started("run1", 1, "l1")];
        input.own_entries = vec![assistant_toolcall_entry(
            "e-assistant",
            3,
            vec![("call-1", "read")],
        )];
        input.entries = vec![assistant_toolcall_entry(
            "e-assistant",
            3,
            vec![("call-1", "read")],
        )];
        input.records = vec![
            tool_started(
                "t1",
                run_id,
                6,
                "l1",
                "e-assistant",
                0,
                "call-1",
                "read",
                "e-tr",
            ),
            tool_started(
                "t2",
                run_id,
                8,
                "l1",
                "e-assistant",
                0,
                "call-1",
                "read",
                "e-tr",
            ),
        ];
        let err = reduce_lane_state(input).unwrap_err();
        assert!(
            matches!(err, RecordLogError::DuplicateToolInvocation { .. }),
            "{err}"
        );
    }

    #[test]
    fn validation_missing_provisioned_entry_ok() {
        // ProvisionedEntry 未落地（entries 中不存在）时校验通过（恢复中场景）
        let mut input = base_input("l1");
        input.records = vec![queue_enqueued(
            "q1",
            "run1",
            7,
            "l1",
            "steer",
            provisioned("e-steer"),
        )];
        input.open_operations = vec![op_started("run1", 1, "l1")];
        let result = reduce_lane_state(input).unwrap();
        let st = result.lane_state.operation.unwrap();
        assert_eq!(st.pending_steer.len(), 1);
    }

    #[test]
    fn entry_payload_json_excludes_metadata() {
        let e = msg_entry("e1", 5, "user", "hi");
        let p = e.payload_json();
        assert!(p.get("parentId").is_none());
        assert!(p.get("seq").is_none());
        assert!(p.get("timestamp").is_none());
    }

    // ------------------------------------------------------------------
    // SessionState / codec（对齐 pi state.ts / codec.ts）
    // ------------------------------------------------------------------

    fn state_entry(
        state: &mut SessionState,
        id: &str,
        parent: Option<&str>,
    ) -> Result<(), RecordLogError> {
        let seq = state.next_sequence();
        state.apply_mutation(SessionMutation::Entry {
            lane: Some("main".to_string()),
            entry: Entry::Message {
                base: EntryBase {
                    id: id.into(),
                    seq,
                    timestamp: 1,
                    parent_id: parent.map(|s| s.to_string()),
                },
                message: AgentMessage::user_text("x"),
                terminate: None,
            },
        })
    }

    #[test]
    fn state_seq_must_be_consecutive_from_one() {
        let mut st = SessionState::new();
        // 首条 seq 必须是 1
        let err = st
            .apply_mutation(SessionMutation::Entry {
                lane: Some("main".to_string()),
                entry: Entry::Message {
                    base: EntryBase {
                        id: "e0".into(),
                        seq: 0,
                        timestamp: 1,
                        parent_id: None,
                    },
                    message: AgentMessage::user_text("x"),
                    terminate: None,
                },
            })
            .unwrap_err();
        assert!(err.to_string().contains("non-consecutive"), "{}", err);
        // seq 跳号拒绝
        let mut st = SessionState::new();
        let _ = state_entry(&mut st, "e1", None);
        let err = st
            .apply_mutation(SessionMutation::Entry {
                lane: Some("main".to_string()),
                entry: Entry::Message {
                    base: EntryBase {
                        id: "e3".into(),
                        seq: 5,
                        timestamp: 1,
                        parent_id: Some("e1".into()),
                    },
                    message: AgentMessage::user_text("x"),
                    terminate: None,
                },
            })
            .unwrap_err();
        assert!(err.to_string().contains("non-consecutive"), "{}", err);
    }

    #[test]
    fn state_entry_must_chain_to_lane_leaf() {
        let mut st = SessionState::new();
        let _ = state_entry(&mut st, "e1", None);
        // 直接提交 parent 非 lane leaf 的 entry 被拒绝
        let err = st
            .apply_mutation(SessionMutation::Entry {
                lane: Some("main".to_string()),
                entry: Entry::Message {
                    base: EntryBase {
                        id: "e2".into(),
                        seq: 2,
                        timestamp: 1,
                        parent_id: Some("phantom".into()),
                    },
                    message: AgentMessage::user_text("x"),
                    terminate: None,
                },
            })
            .unwrap_err();
        assert!(
            err.to_string().contains("chain to the lane leaf"),
            "{}",
            err
        );
    }

    #[test]
    fn state_lane_pointer_and_walk_to_root() {
        let mut st = SessionState::new();
        let _ = state_entry(&mut st, "a", None);
        let _ = state_entry(&mut st, "b", Some("a"));
        let _ = state_entry(&mut st, "c", Some("b"));
        assert_eq!(st.lane_leaf("main"), Some("c"));
        // branch: leaf -> root（newest first）
        let branch = st.find_entries_on_branch("c", None, None, false).unwrap();
        let ids: Vec<&str> = branch.iter().map(|e| e.id()).collect();
        assert_eq!(ids, vec!["c", "b", "a"]);
        // root-first
        let rev = st.find_entries_on_branch("c", None, None, true).unwrap();
        let ids: Vec<&str> = rev.iter().map(|e| e.id()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
        // 缺失父 → 损坏（绕过 apply 校验直接注入，模拟外部写入的损坏文件）
        let mut st2 = SessionState::new();
        let _ = state_entry(&mut st2, "a", None);
        let seq = st2.next_sequence();
        let e = Entry::Message {
            base: EntryBase {
                id: "x".into(),
                seq,
                timestamp: 1,
                parent_id: Some("missing".into()),
            },
            message: AgentMessage::user_text("x"),
            terminate: None,
        };
        st2.entries.push(e.clone());
        st2.entries_by_id.insert("x".into(), e);
        let err = st2
            .find_entries_on_branch("x", None, None, true)
            .unwrap_err();
        assert!(err.to_string().contains("Entry not found"), "{}", err);
    }

    #[test]
    fn state_open_operations_tracked_and_cleared() {
        let mut st = SessionState::new();
        let _ = state_entry(&mut st, "a", None);
        let seq = st.next_sequence();
        st.apply_mutation(SessionMutation::Record {
            record: LaneRecord::OperationStarted {
                base: RecordBase {
                    id: "run-1".into(),
                    seq,
                    lane: "main".into(),
                    timestamp: 1,
                },
                source_leaf_id: None,
                intent: OperationIntent::Run {
                    original_prompt: vec![AgentMessage::user_text("q")],
                    initial_messages: vec![],
                    system_prompt_override: None,
                },
            },
        })
        .unwrap();
        assert_eq!(st.find_open_operations("main", None).len(), 1);
        let seq = st.next_sequence();
        st.apply_mutation(SessionMutation::Record {
            record: LaneRecord::OperationFinished {
                base: RecordBase {
                    id: "op-f".into(),
                    seq,
                    lane: "main".into(),
                    timestamp: 2,
                },
                run_id: "run-1".into(),
                outcome: "completed".into(),
                error: None,
            },
        })
        .unwrap();
        assert_eq!(st.find_open_operations("main", None).len(), 0);
    }

    #[test]
    fn state_fork_mutations_exclude_records() {
        let mut st = SessionState::new();
        let _ = state_entry(&mut st, "a", None);
        let _ = state_entry(&mut st, "b", Some("a"));
        let seq = st.next_sequence();
        st.apply_mutation(SessionMutation::Record {
            record: LaneRecord::OperationStarted {
                base: RecordBase {
                    id: "run-1".into(),
                    seq,
                    lane: "main".into(),
                    timestamp: 1,
                },
                source_leaf_id: None,
                intent: OperationIntent::Run {
                    original_prompt: vec![],
                    initial_messages: vec![],
                    system_prompt_override: None,
                },
            },
        })
        .unwrap();
        st.apply_mutation(SessionMutation::Fact {
            seq: st.next_sequence(),
            fact: "name".into(),
            name: Some("n".into()),
            target_id: None,
            label: None,
        })
        .unwrap();
        let ms = st.create_fork_mutations(&ForkScope::Tree).unwrap();
        let kinds: Vec<&str> = ms
            .iter()
            .map(|m| match m {
                SessionMutation::Entry { .. } => "entry",
                SessionMutation::Record { .. } => "record",
                SessionMutation::Lane { .. } => "lane",
                SessionMutation::Fact { .. } => "fact",
            })
            .collect();
        assert_eq!(kinds, vec!["entry", "entry", "lane", "fact"]);
    }

    #[test]
    fn codec_roundtrip_header_and_mutations() {
        let header = JsonlV4Header::new("id-1".into(), "/tmp".into(), 12345, Some("parent".into()));
        let line = header.encode();
        let v: Value = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(v["kind"], "header");
        assert_eq!(v["version"], 4);
        assert_eq!(v["createdAt"], 12345);

        let m = SessionMutation::Entry {
            lane: Some("main".to_string()),
            entry: Entry::Message {
                base: EntryBase {
                    id: "e1".into(),
                    seq: 1,
                    timestamp: 7,
                    parent_id: None,
                },
                message: AgentMessage::user_text("hi"),
                terminate: None,
            },
        };
        let encoded = encode_mutation(&m);
        let parsed = parse_mutation(encoded.trim()).unwrap();
        assert!(matches!(parsed, SessionMutation::Entry { lane: Some(l), .. } if l == "main"));

        // 未知 kind 拒绝
        assert!(parse_mutation(r#"{"kind":"bogus","seq":1}"#).is_err());
        // seq=0 拒绝
        assert!(parse_mutation(
            r#"{"kind":"entry","lane":"main","type":"message","id":"x","parentId":null,"seq":0,"timestamp":1,"message":{"role":"user","content":[]}}"#
        )
        .is_err());
    }

    #[test]
    fn context_edit_entry_roundtrips_and_validates() {
        // 省略形态（replacement: null）
        let line = r#"{"kind":"entry","lane":"main","type":"context_edit","id":"ce1","parentId":"e1","seq":9,"timestamp":3,"targetId":"e1","replacement":null}"#;
        let parsed = parse_mutation(line).unwrap();
        let SessionMutation::Entry { entry, .. } = &parsed else {
            panic!("expected entry mutation");
        };
        assert!(entry.is_context_edit());
        assert_eq!(entry.context_edit_target(), Some("e1"));
        assert!(matches!(
            entry,
            Entry::ContextEdit {
                replacement: None,
                ..
            }
        ));
        // 重新编码后仍可解析（往返一致）
        let v: Value = serde_json::from_str(encode_mutation(&parsed).trim()).unwrap();
        assert_eq!(v["type"], "context_edit");
        assert_eq!(v["targetId"], "e1");
        assert!(v["replacement"].is_null());

        // 替换形态
        let line = r#"{"kind":"entry","lane":"main","type":"context_edit","id":"ce2","seq":10,"timestamp":3,"targetId":"e1","replacement":{"content":[{"type":"text","text":"rewritten"}]}}"#;
        let parsed = parse_mutation(line).unwrap();
        let SessionMutation::Entry { entry, .. } = &parsed else {
            panic!("expected entry mutation");
        };
        match entry {
            Entry::ContextEdit { replacement, .. } => {
                let rep = replacement.as_ref().expect("replacement present");
                assert_eq!(rep.content.len(), 1);
            }
            other => panic!("expected context_edit, got {other:?}"),
        }

        // 空 targetId 拒绝（载荷校验）
        let bad = r#"{"kind":"entry","lane":"main","type":"context_edit","id":"ce3","seq":11,"timestamp":3,"targetId":"","replacement":null}"#;
        assert!(parse_mutation(bad).is_err());

        // 基字段访问与类型名（枚举穷举点已同步）
        let parsed = parse_mutation(line).unwrap();
        let SessionMutation::Entry { entry, .. } = &parsed else {
            unreachable!()
        };
        assert_eq!(entry_base(entry).id, "ce2");
        assert_eq!(entry_type_name(entry), "context_edit");
    }
}

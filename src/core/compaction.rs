//! 上下文压缩
//!
//! - [`compaction`] 主逻辑：阈值判断、上下文估算
//! - [`utils`] token 估算与文件操作跟踪
//! - [`branch_summarization`] 摘要 prompt 与分支摘要

mod branch_summarization;
mod engine;
mod utils;

pub use branch_summarization::{
    BRANCH_SUMMARY_PREAMBLE, BRANCH_SUMMARY_PROMPT, SUMMARIZATION_PROMPT,
    SUMMARIZATION_SYSTEM_PROMPT, UPDATE_SUMMARIZATION_PROMPT, build_summary_prompt,
};
pub use engine::{
    CompactionReason, CompactionSettings, ContextUsageEstimate, DEFAULT_KEEP_RECENT_TOKENS,
    DEFAULT_RESERVE_TOKENS, calculate_context_tokens, estimate_context_tokens,
    find_compaction_cut_index, should_compact, usable_usage, usage_anchor_is_stale,
};
pub use utils::{
    FileOperations, compute_file_lists, estimate_tokens, extract_file_ops_from_message,
    format_file_operations, serialize_conversation,
};

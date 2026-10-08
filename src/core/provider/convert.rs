//! 三协议共享的转换辅助：compaction/branch 摘要包装、sanitize、错误截断。

/// compaction 摘要的包裹前缀，说明此前历史已被压缩为摘要。
pub const COMPACTION_SUMMARY_PREFIX: &str = "The conversation history before this point was compacted into the following summary:\n\n<summary>\n";
/// compaction 摘要的结束标记，与前缀配对闭合 summary 标签。
pub const COMPACTION_SUMMARY_SUFFIX: &str = "\n</summary>";
/// 分支摘要的包裹前缀，说明对话来自某条回退分支。
pub const BRANCH_SUMMARY_PREFIX: &str =
    "The following is a summary of a branch that this conversation came back from:\n\n<summary>\n";
/// 分支摘要的结束标记，与分支前缀配对闭合 summary 标签。
pub const BRANCH_SUMMARY_SUFFIX: &str = "</summary>";

/// 把错误文本截到 1000 字节以内（超出时加省略号），避免错误信息撑爆上下文。
pub(crate) fn truncate_for_error(s: &str) -> String {
    if s.len() > 1000 {
        format!("{}...", &s[..1000])
    } else {
        s.to_string()
    }
}

// Rust String 不含孤立 surrogate；直接原样返回
/// 清理字符串中的孤立 surrogate；Rust 的 String 本就不含此类字符，故原样返回。
pub(crate) fn sanitize_surrogates(s: &str) -> String {
    s.to_string()
}

//! review log：把每一次判定追加进 `permissions-review.jsonl`。
//!
//! 一次判定一行（NDJSON），字段固定，便于 `jq` 直接筛：
//! `{"ts","tool","surface","pattern","action","value","reason","sessionApproved"}`
//!
//! 用途是事后复盘——「刚才那条 `rm` 是谁拦的、按哪条规则、值长什么样」。
//! 写盘失败一律静默（日志是旁路，绝不能因为写不了日志而改变门控结论）；
//! 过长的值按 [`VALUE_MAX_CHARS`] 截断，避免一条 here-doc 命令把日志撑爆。
//!
//! **内容敏感性**：`value` 保留被检查的原始值（bash 命令或路径），因此日志会包含
//! 命令行里出现过的字符串。不需要时把 `permissionReviewLog` 关掉即可。

use super::{
    config::{Settings, review_log_path},
    policy::Decision,
};
use crate::utils::{time::now_rfc3339_secs, truncate::truncate_chars};
use serde_json::json;
use std::{
    fs::{self, OpenOptions},
    io::Write,
};

/// 单个 `value` 字段的最大字符数；超出按字符边界截断并加省略号。
const VALUE_MAX_CHARS: usize = 1000;

/// 记录一次判定。
///
/// `value` 是被检查的对象（bash 命令原文或文件路径）；`approved` 表示这条判定最终
/// 是否由会话放行（`/permissions allow`）从 `ask` 转成了放行——复盘时能区分
/// 「策略允许」与「用户当场放行」。
pub(super) fn record(
    settings: &Settings,
    tool: &str,
    decision: &Decision,
    value: &str,
    approved: bool,
) {
    if !settings.review_log {
        return;
    }

    let path = review_log_path();
    if let Some(parent) = path.parent()
        && fs::create_dir_all(parent).is_err()
    {
        return;
    }
    let line = json!({
        "ts": now_rfc3339_secs(),
        "tool": tool,
        "surface": decision.surface,
        "pattern": decision.pattern,
        "action": decision.action.to_string(),
        "value": truncate_chars(value, VALUE_MAX_CHARS, Some("…"), false),
        "reason": decision.reason,
        "sessionApproved": approved,
    });
    let Ok(text) = serde_json::to_string(&line) else {
        return;
    };
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        _ = writeln!(file, "{text}");
    }
}

/// 读回最后 `limit` 行原始记录（`/permissions log` 用）。读不到时返回空表。
pub(super) fn tail(limit: usize) -> Vec<String> {
    let path = review_log_path();
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(str::to_string)
        .collect();
    if lines.len() > limit {
        lines.drain(..lines.len() - limit);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_is_well_formed() {
        let ts = now_rfc3339_secs();
        assert_eq!(ts.len(), 20, "{ts}");
        assert!(ts.ends_with('Z'));
        assert_eq!(&ts[4..5], "-");
    }

    #[test]
    fn truncation_respects_chars() {
        let long = "汉".repeat(VALUE_MAX_CHARS + 10);
        let cut = truncate_chars(&long, VALUE_MAX_CHARS, Some("…"), false);
        assert_eq!(cut.chars().count(), VALUE_MAX_CHARS + 1);
        assert!(cut.ends_with('…'));
        assert_eq!(
            truncate_chars("short", VALUE_MAX_CHARS, Some("…"), false),
            "short"
        );
    }
}

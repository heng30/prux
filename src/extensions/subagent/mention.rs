//! `@handle` 语法
//!
//! - 发送语法只在**输入开头**识别：`^@([\w-]+)\s+([\s\S]+)$`（句柄后必须非空消息）；
//!   因此裸 `@explore`、前导文件路径（`@src/foo.ts`）、句子中间的 `@x` 都不会被认领。
//! - 句柄由**类型名**（或调用方给的 `name`）派生：小写化 → 非 `[\w-]` 折叠为 `-` →
//!   去首尾 `-` → 截断 64 → 再去尾 `-`；全空回退 `agent`。
//! - 冲突编号 `base`、`base-2`、…；`main` 为保留句柄（永不分配、也不解析到类型）。

use std::collections::HashSet;

/// 句柄长度上限
pub(super) const MAX_HANDLE_LENGTH: usize = 64;

/// 一次 `@handle message` 发送。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Mention {
    /// 目标句柄（不含 `@`）。
    pub handle: String,
    /// 要注入的消息。
    pub message: String,
}

/// 类型名/名称 → 可输入的句柄基础串。
pub(super) fn handle_base(raw: &str) -> String {
    let mut slug = String::new();
    let mut prev_hyphen = false;

    for ch in raw.chars() {
        let c = ch.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            slug.push(c);
            prev_hyphen = c == '-';
        } else if !prev_hyphen {
            // 非 `[\w-]` 的整段折叠成一个 `-`（对齐 `[^a-z0-9_-]+` → `-`）
            slug.push('-');
            prev_hyphen = true;
        }
    }

    // 去首尾 `-`，再截断、再去尾 `-`（截断可能切在连字符串中间）
    let mut capped: String = slug
        .trim_matches('-')
        .chars()
        .take(MAX_HANDLE_LENGTH)
        .collect();

    while capped.ends_with('-') {
        capped.pop();
    }

    if capped.is_empty() {
        "agent".to_string()
    } else {
        capped
    }
}

/// `main` 保留给主对话，永不分配给子代理。
pub(super) fn is_reserved_handle(handle: &str) -> bool {
    handle.eq_ignore_ascii_case("main")
}

/// `base`，否则 `base-2`、`base-3`…（首个既不在 `taken` 也不是保留名的形态）。
pub(super) fn assign_handle(base: &str, taken: &HashSet<String>) -> String {
    let mut candidate = base.to_string();
    let mut n = 1;
    while taken.contains(&candidate) || is_reserved_handle(&candidate) {
        n += 1;
        candidate = format!("{base}-{n}");
    }
    candidate
}

/// 把输入的句柄映射回某个类型：`handle_base(type) == handle`（大小写不敏感）。
/// 保留句柄不解析（`@main` 必须落回主对话）。
pub(super) fn resolve_handle_to_type(handle: &str, types: &[String]) -> Option<String> {
    let wanted = handle.to_ascii_lowercase();
    if is_reserved_handle(&wanted) {
        return None;
    }
    types.iter().find(|t| handle_base(t) == wanted).cloned()
}

/// `@agent-<type>` 手写拼写；没有前缀或前缀后为空时 `None`。
pub(super) fn strip_agent_prefix(handle: &str) -> Option<String> {
    /// `@agent-` 手写句柄前缀，用于从完整句柄还原出代理类型名。
    const PREFIX: &str = "agent-";

    if handle.len() > PREFIX.len()
        && handle
            .get(..PREFIX.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(PREFIX))
    {
        Some(handle[PREFIX.len()..].to_string())
    } else {
        None
    }
}

/// 用消息首行作代理的短描述（折叠空白，超 40 字符截断加省略号）。
pub(super) fn describe_mention(message: &str) -> String {
    let one_line = message
        .split('\n')
        .next()
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");

    if one_line.chars().count() > 40 {
        let truncated: String = one_line.chars().take(39).collect();
        format!("{}…", truncated.trim_end())
    } else {
        one_line
    }
}

/// 解析 `@handle message`；不是一次发送时返回 `None`。
pub(super) fn parse_mention(text: &str) -> Option<Mention> {
    let rest = text.strip_prefix('@')?;

    // 句柄：开头的 `[\w-]+`
    let mut end = 0;
    for (i, c) in rest.char_indices() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }

    if end == 0 {
        return None;
    }

    let handle = &rest[..end];
    let after = &rest[end..];

    // 句柄后必须至少一个空白
    let trimmed = after.trim_start();
    if trimmed.len() == after.len() {
        return None;
    }

    let message = trimmed.trim();
    if message.is_empty() {
        return None;
    }

    Some(Mention {
        handle: handle.to_string(),
        message: message.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn taken(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn handle_base_lowercases() {
        assert_eq!(handle_base("Explore"), "explore");
    }

    #[test]
    fn handle_base_keeps_hyphenated_type() {
        assert_eq!(handle_base("general-purpose"), "general-purpose");
    }

    #[test]
    fn handle_base_collapses_and_trims() {
        assert_eq!(handle_base("Code Review!"), "code-review");
        assert_eq!(handle_base("  spaced  out  "), "spaced-out");
    }

    #[test]
    fn handle_base_always_typeable() {
        assert_eq!(handle_base("!!!"), "agent");
        assert_eq!(handle_base(""), "agent");
        // CJK 全被折叠
        assert_eq!(handle_base("デバッグ"), "agent");
    }

    #[test]
    fn handle_base_caps_length() {
        assert_eq!(handle_base(&"x".repeat(200)).chars().count(), 64);
    }

    #[test]
    fn handle_base_never_leaves_trailing_hyphen() {
        // 63 个 x 后跟空白段：朴素的 slice(0,64) 会把连字符留下
        assert!(!handle_base(&format!("{}   tail", "x".repeat(63))).ends_with('-'));
    }

    #[test]
    fn handle_base_preserves_underscore() {
        assert_eq!(handle_base("my_agent"), "my_agent");
    }

    #[test]
    fn assign_handle_plain_and_numbered() {
        assert_eq!(assign_handle("explore", &taken(&[])), "explore");
        assert_eq!(assign_handle("explore", &taken(&["explore"])), "explore-2");
        assert_eq!(
            assign_handle("explore", &taken(&["explore", "explore-2"])),
            "explore-3"
        );
    }

    #[test]
    fn assign_handle_never_hands_out_main() {
        assert_eq!(assign_handle("main", &taken(&[])), "main-2");
    }

    #[test]
    fn assign_handle_skips_gap_not_live_handle() {
        assert_eq!(
            assign_handle("explore", &taken(&["explore", "explore-3"])),
            "explore-2"
        );
    }

    #[test]
    fn resolve_handle_to_type_matches_and_is_exact() {
        let types: Vec<String> = ["general-purpose", "Explore", "Code Review!"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            resolve_handle_to_type("explore", &types).as_deref(),
            Some("Explore")
        );
        assert_eq!(
            resolve_handle_to_type("EXPLORE", &types).as_deref(),
            Some("Explore")
        );
        assert_eq!(
            resolve_handle_to_type("code-review", &types).as_deref(),
            Some("Code Review!")
        );
        assert!(resolve_handle_to_type("ex", &types).is_none());
        assert!(resolve_handle_to_type("explore-2", &types).is_none());
    }

    #[test]
    fn resolve_handle_to_type_refuses_reserved() {
        let types: Vec<String> = ["main", "Explore"].iter().map(|s| s.to_string()).collect();
        assert!(resolve_handle_to_type("main", &types).is_none());
    }

    #[test]
    fn reserved_handle_is_case_insensitive() {
        assert!(is_reserved_handle("main"));
        assert!(is_reserved_handle("MAIN"));
        for h in ["explore", "mainframe", "main-2", "ma"] {
            assert!(!is_reserved_handle(h));
        }
    }

    #[test]
    fn strip_agent_prefix_only_at_start() {
        assert_eq!(
            strip_agent_prefix("agent-explore").as_deref(),
            Some("explore")
        );
        assert_eq!(
            strip_agent_prefix("agent-agent-foo").as_deref(),
            Some("agent-foo")
        );
        assert_eq!(
            strip_agent_prefix("AGENT-explore").as_deref(),
            Some("explore")
        );
        assert!(strip_agent_prefix("explore").is_none());
        assert!(strip_agent_prefix("agent-").is_none());
        assert!(strip_agent_prefix("agentexplore").is_none());
        assert!(strip_agent_prefix("sub-agent-explore").is_none());
    }

    #[test]
    fn describe_mention_first_line_collapsed() {
        assert_eq!(
            describe_mention("find every retry marker"),
            "find every retry marker"
        );
        assert_eq!(
            describe_mention("  audit   the RPC path\nthen report back  "),
            "audit the RPC path"
        );
        let long = describe_mention(&"x".repeat(200));
        assert_eq!(long.chars().count(), 40);
        assert!(long.ends_with('…'));
    }

    #[test]
    fn parse_mention_splits_leading_handle() {
        assert_eq!(
            parse_mention("@explore check the RPC path"),
            Some(Mention {
                handle: "explore".into(),
                message: "check the RPC path".into()
            })
        );
    }

    #[test]
    fn parse_mention_trims_and_accepts_newline() {
        assert_eq!(
            parse_mention("@explore   spaced   "),
            Some(Mention {
                handle: "explore".into(),
                message: "spaced".into()
            })
        );
        assert_eq!(
            parse_mention("@explore\nline1\nline2"),
            Some(Mention {
                handle: "explore".into(),
                message: "line1\nline2".into()
            })
        );
    }

    #[test]
    fn parse_mention_rejects_bare_and_paths_and_midsentence() {
        assert!(parse_mention("@explore").is_none());
        assert!(parse_mention("@explore ").is_none());
        assert!(parse_mention("@explore \t ").is_none());
        assert!(parse_mention("@src/index.ts summarize this").is_none());
        assert!(parse_mention("@README.md what changed").is_none());
        assert!(parse_mention("hey @explore look at this").is_none());
        assert!(parse_mention(" @explore look at this").is_none());
    }

    #[test]
    fn parse_mention_accepts_numbered_and_reserved_handles() {
        assert_eq!(
            parse_mention("@explore-2 you take the second half")
                .map(|m| m.handle)
                .as_deref(),
            Some("explore-2")
        );
        assert_eq!(
            parse_mention("@main do this").map(|m| m.handle).as_deref(),
            Some("main")
        );
    }
}

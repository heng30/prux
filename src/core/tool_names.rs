//! 工具名匹配与 MCP 工具名判定。
//!
//! 供 `--tools` / `--exclude-tools`与工具表的 allowlist 过滤共用。
//! 用手写通配匹配**现算**（工具数与条目数都很小，省掉正则编译与编译失败的路径）；匹配语义一致：
//! 条目不含 `*` 时逐字比较，含 `*` 时 `*` 匹配任意长度（含空）的字符序列并锚定首尾。

/// MCP 网关工具名。
///
/// 把每个服务器的工具聚合成单个 `mcp` 工具（模型经 `action` 调用），
/// 本常量必须与 `extensions::mcp::TOOL` 一致（由该模块的测试守卫生效）。
pub const MCP_GATEWAY_TOOL_NAME: &str = "mcp";

/// MCP 资源工具名。
///
/// 由 `mcp` 工具的 `resources` / `read_resource` action 承接，
/// 这几个名字只用于 [`is_mcp_tool_name`] 的判定，目前不注册同名工具。
pub const MCP_RESOURCE_TOOL_NAMES: &[&str] = &[
    "list_mcp_resources",
    "list_mcp_resource_templates",
    "read_mcp_resource",
];

/// 通配匹配：`*` 匹配任意长度（含空）的字符序列，其余字符逐字比较，整体锚定首尾。
/// `pattern` 不含 `*` 时等价于 `pattern == text`。
pub fn wildcard_matches(pattern: &str, text: &str) -> bool {
    let p = pattern.as_bytes();
    let t = text.as_bytes();
    let (mut pi, mut ti) = (0usize, 0usize);
    // 最近一次 `*` 的位置与它当时对齐的文本下标，用于失配时回溯。
    let (mut star, mut mark) = (None::<usize>, 0usize);

    while ti < t.len() {
        if pi < p.len() && p[pi] == b'*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }

    // 文本已耗尽：模式剩下的必须全是 `*` 才能匹配（如 `read*` 匹配 `read`）。
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }

    pi == p.len()
}

/// 条目列表里是否有任一条目命中 `name`。
///
/// 条目含 `*` 时按 [`wildcard_matches`]，否则精确比较；空列表恒为 `false`。
pub fn matches_any(entries: &[String], name: &str) -> bool {
    entries.iter().any(|e| {
        if e.contains('*') {
            wildcard_matches(e, name)
        } else {
            e == name
        }
    })
}

/// 是否 MCP 工具：`mcp__<server>__<tool>` 形态、聚合网关名、或 MCP 资源工具名。
pub fn is_mcp_tool_name(name: &str) -> bool {
    name.starts_with("mcp__")
        || name == MCP_GATEWAY_TOOL_NAME
        || MCP_RESOURCE_TOOL_NAMES.contains(&name)
}

/// allowlist 是否连 MCP 一起过滤
///
/// 空名单（`--no-tools`）一律过滤；名单里出现 `mcp__` 前缀条目时过滤
/// （用户点名了具体服务器，就不再整片保留 MCP）；否则不过滤。
pub fn allowlist_filters_mcp(entries: &[String]) -> bool {
    entries.is_empty() || entries.iter().any(|e| e.starts_with("mcp__"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造条目列表（省去 `to_string()` 噪音）。
    fn entries(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// `*` 匹配任意字符序列，含空序列与中间段。
    #[test]
    fn wildcard_matches_arbitrary_segments() {
        assert!(wildcard_matches("mcp__radius__*", "mcp__radius__get"));
        assert!(wildcard_matches("mcp__radius__*", "mcp__radius__"));
        assert!(wildcard_matches("*", "anything"));
        assert!(wildcard_matches("*", ""));
        assert!(wildcard_matches("read*", "read"));
        assert!(wildcard_matches("*write*", "readwrite"));
        assert!(wildcard_matches("a*b*c", "aXXbYYc"));
    }

    /// 通配匹配锚定首尾，且非 `*` 字符必须逐字相等。
    #[test]
    fn wildcard_matches_is_anchored() {
        assert!(!wildcard_matches("read", "reader"));
        assert!(!wildcard_matches("mcp__radius__*", "mcp__other__get"));
        assert!(!wildcard_matches("a*b*c", "aXbY"));
        assert!(!wildcard_matches("abc", "abd"));
    }

    /// 条目不含 `*` 时精确匹配，含 `*` 时走通配。
    #[test]
    fn matches_any_mixes_exact_and_pattern_entries() {
        let list = entries(&["read", "mcp__radius__*"]);
        assert!(matches_any(&list, "read"));
        assert!(matches_any(&list, "mcp__radius__put"));
        assert!(!matches_any(&list, "reader"));
        assert!(!matches_any(&list, "mcp__other__put"));
        assert!(!matches_any(&[], "read"));
    }

    /// MCP 名字判定覆盖网关名、`mcp__` 前缀与资源工具。
    #[test]
    fn mcp_tool_names_are_recognized() {
        assert!(is_mcp_tool_name("mcp"));
        assert!(is_mcp_tool_name("mcp__docs__search"));
        assert!(is_mcp_tool_name("read_mcp_resource"));
        assert!(!is_mcp_tool_name("read"));
        assert!(!is_mcp_tool_name("mcpx"));
    }

    /// allowlist 过滤 MCP 的三种情形与 pi 一致。
    #[test]
    fn allowlist_filters_mcp_matches_pi() {
        assert!(allowlist_filters_mcp(&[]));
        assert!(allowlist_filters_mcp(&entries(&["read", "mcp__radius__*"])));
        assert!(!allowlist_filters_mcp(&entries(&["read", "codemode"])));
    }
}

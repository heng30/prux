//! `tool_search` 内置扩展
//!
//! 背景：工具一多（典型是 MCP 服务器），系统提示与工具 schema 会被大量用不上的工具撑爆。
//! 扩展可以把自己的一部分工具声明为**延迟工具**（`ExtensionTool::with_exposure(ToolExposure::Deferred)`）：
//! 它们仍可执行，但不进模型可见列表；模型再用本工具按 BM25 检索工具元数据，命中项被激活，**下一次模型调用**即可见。
//!
//! 本扩展默认不启用（无延迟工具时它只会占一个工具位）。

use super::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_TOOL_SEARCH};
use crate::core::{
    extensions::{
        self, Extension, ExtensionHook, ExtensionMode, ExtensionTool, ToolExposure,
        activate_deferred_tools, unactivated_deferred_tools,
    },
    tools::{ToolError, ToolResult},
};
use serde_json::{Value, json};
use std::{collections::HashMap, sync::Arc};

/// 本扩展在注册表里的名字（[`Extension::name`] 的返回值，用于把自己从遍历中排除）
pub const EXTENSION_NAME: &str = "tool-search";
/// 本扩展暴露的工具名（模型通过它检索延迟工具）
pub const TOOL_NAME: &str = "tool_search";
/// 单次最多返回 / 激活的工具数
pub const DEFAULT_LIMIT: usize = 8;
/// （名称, 得分）按分数降序
pub type ToolMatch = (String, f64);

/// 分词时丢弃的英文停用词
const STOP_WORDS: [&str; 20] = [
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "in", "is", "it", "of", "on",
    "or", "that", "the", "this", "to",
];

/// 自声明工厂：linkme 分布式切片
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static TOOL_SEARCH_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_TOOL_SEARCH,
    make: || -> Arc<dyn Extension> { Arc::new(ToolSearch) },
};

/// 延迟工具检索扩展（BM25 检索 + 激活已声明的延迟工具）
pub struct ToolSearch;

impl Extension for ToolSearch {
    /// 扩展名 [`EXTENSION_NAME`]。
    fn name(&self) -> &str {
        EXTENSION_NAME
    }

    /// 一句话说明：用 BM25 检索延迟工具元数据并加载命中项。
    fn description(&self) -> &str {
        "Let the model discover deferred tools (BM25 over tool metadata) and load the matches"
    }

    /// 默认启用：因为其他插件可能会定义延时工具，这个忘记启用，就会出现问题
    fn default_enabled(&self) -> bool {
        true
    }

    /// 仅在 Dev / Creator 模式下提供。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 暴露唯一的 `tool_search` 工具（描述按当前注册表动态生成）。
    fn tools(&self) -> Vec<ExtensionTool> {
        vec![ExtensionTool::simple(
            TOOL_NAME,
            &description(),
            schema(),
            "Search for tools that are not loaded yet and load the matches",
        )]
    }

    /// 执行检索：按 `query` 打分取前 `limit`（默认 [`DEFAULT_LIMIT`]）个未激活延迟工具并激活。
    ///
    /// 工具名不符、`query` 为空或 `limit` 非正整数时返回 [`ToolError`]。
    fn execute_tool(&self, name: &str, args: &Value) -> std::result::Result<ToolResult, ToolError> {
        if name != TOOL_NAME {
            return Err(ToolError(format!("tool-search: unknown tool: {name}")));
        }

        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        if query.is_empty() {
            return Err(ToolError("query must not be empty".to_string()));
        }

        let limit = match args.get("limit") {
            None | Some(Value::Null) => DEFAULT_LIMIT,
            Some(v) => match v.as_u64() {
                Some(n) if n > 0 => n as usize,
                _ => return Err(ToolError("limit must be a positive integer".to_string())),
            },
        };

        // 搜索域：已启用扩展声明的、尚未激活的延迟工具
        let candidates = unactivated_deferred_tools();
        let documents: Vec<ToolDocument> = candidates.iter().map(tool_document).collect();
        let matches = rank(&query, &documents, limit);

        let loaded: Vec<String> = matches.iter().map(|m| m.0.clone()).collect();
        if !loaded.is_empty() {
            // 只改激活集合：agent 循环在工具执行后重建工具表，下一轮模型调用即可见
            activate_deferred_tools(&loaded);
        }

        let text = if loaded.is_empty() {
            "No matching tools found.".to_string()
        } else {
            let listed: Vec<String> = loaded
                .iter()
                .map(|name| {
                    let desc = candidates
                        .iter()
                        .find(|t| &t.name == name)
                        .map(|t| first_line(&t.description))
                        .unwrap_or_default();
                    format!("- {name}: {desc}")
                })
                .collect();

            format!(
                "Loaded {} tool{}. They are available from your next call:\n{}",
                loaded.len(),
                if loaded.len() == 1 { "" } else { "s" },
                listed.join("\n")
            )
        };

        Ok(ToolResult {
            text,
            details: Some(json!({ "loaded": loaded })),
            ..ToolResult::default()
        })
    }

    /// 不订阅任何钩子（无 dock / overlay 需求）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        Vec::new()
    }
}

/// 工具描述：列出可检索的来源（声明了延迟工具的扩展）
fn description() -> String {
    let mut sources: Vec<String> = Vec::new();
    for ext in extensions::registered() {
        // 排除自身：本扩展的 tools() 会回调 description()，再对自身调 tools() 就是无限递归
        if ext.name() == EXTENSION_NAME {
            continue;
        }

        if !ext
            .tools()
            .iter()
            .any(|t| t.exposure == ToolExposure::Deferred)
        {
            continue;
        }

        let desc = first_line(ext.description());
        sources.push(if desc.is_empty() {
            format!("- {}", ext.name())
        } else {
            format!("- {}: {}", ext.name(), desc)
        });
    }

    let listed = if sources.is_empty() {
        "None currently enabled.".to_string()
    } else {
        sources.join("\n")
    };

    format!(
        "# Tool discovery\n\nSearches deferred tool metadata with BM25 and exposes matching tools for the next model call.\n\nYou have access to tools from the following sources:\n{listed}\n\nSome of the tools may not have been provided to you upfront; use this tool (`{TOOL_NAME}`) to search for the ones you need."
    )
}

/// `tool_search` 的 JSON schema：必填 `query`，可选正整数 `limit`。
fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "query": {
                "type": "string",
                "description": "Search query for deferred tools."
            },
            "limit": {
                "type": "integer",
                "minimum": 1,
                "description": format!("Maximum number of tools to return. Defaults to {DEFAULT_LIMIT}.")
            }
        },
        "required": ["query"]
    })
}

/// 取文本首个非空行的 trim 结果，用作列表里的一行描述。
fn first_line(text: &str) -> String {
    text.trim().lines().next().unwrap_or("").trim().to_string()
}

/// 检索文档：工具名 + 名称分词 + 描述 + 参数 schema 的字段名/描述
#[derive(Debug, Clone)]
pub struct ToolDocument {
    /// 工具名（检索命中后用于激活）
    name: String,
    /// 该工具元数据经分词/词干化后的检索词
    tokens: Vec<String>,
}

/// 把工具元数据（名称、描述、label、参数 schema 的字段名/描述）转成可检索的分词文档。
pub fn tool_document(tool: &ExtensionTool) -> ToolDocument {
    let mut parts = vec![
        tool.name.clone(),
        tool.name.replace('_', " "),
        tool.description.clone(),
        tool.label.clone().unwrap_or_default(),
    ];

    if let Some(ns) = &tool.namespace {
        parts.push(ns.clone());
    }

    schema_text(&tool.parameters, &mut parts);

    // 声明的结构化输出 schema 也进检索文本：按输出字段名同样能搜到该工具
    if let Some(output) = &tool.output_schema {
        schema_text(output, &mut parts);
    }

    ToolDocument {
        name: tool.name.clone(),
        tokens: tokenize(&parts.join(" ")),
    }
}

/// 递归收集 schema 里的字段名与描述
#[stacksafe::stacksafe]
fn schema_text(schema: &Value, parts: &mut Vec<String>) {
    let Some(obj) = schema.as_object() else {
        return;
    };

    if let Some(desc) = obj.get("description").and_then(|v| v.as_str()) {
        parts.push(desc.to_string());
    }

    if let Some(props) = obj.get("properties").and_then(|v| v.as_object()) {
        for (name, value) in props {
            parts.push(name.clone());
            schema_text(value, parts);
        }
    }

    if let Some(items) = obj.get("items") {
        schema_text(items, parts);
    }

    for key in ["anyOf", "oneOf", "allOf"] {
        if let Some(variants) = obj.get(key).and_then(|v| v.as_array()) {
            for variant in variants {
                schema_text(variant, parts);
            }
        }
    }
}

/// Okapi BM25（k1 = 1.2，b = 0.75），并列保持文档顺序
pub fn rank(query: &str, documents: &[ToolDocument], limit: usize) -> Vec<ToolMatch> {
    let mut query_terms: Vec<String> = Vec::new();
    for term in tokenize(query) {
        if !query_terms.contains(&term) {
            query_terms.push(term);
        }
    }

    if query_terms.is_empty() || documents.is_empty() || limit == 0 {
        return Vec::new();
    }

    let term_counts: Vec<HashMap<String, usize>> = documents
        .iter()
        .map(|d| {
            let mut counts: HashMap<String, usize> = HashMap::new();
            for term in &d.tokens {
                *counts.entry(term.clone()).or_insert(0) += 1;
            }
            counts
        })
        .collect();
    let lengths: Vec<usize> = term_counts
        .iter()
        .map(|counts| counts.values().sum::<usize>())
        .collect();
    let total: usize = lengths.iter().sum();
    let average_length = if total == 0 {
        1.0
    } else {
        total as f64 / documents.len() as f64
    };

    let k1 = 1.2f64;
    let b = 0.75f64;
    let idf: Vec<(String, f64)> = query_terms
        .iter()
        .map(|term| {
            let frequency = term_counts.iter().filter(|c| c.contains_key(term)).count();
            let value = (1.0
                + (documents.len() as f64 - frequency as f64 + 0.5) / (frequency as f64 + 0.5))
                .ln();
            (term.clone(), value)
        })
        .collect();

    let mut matches: Vec<ToolMatch> = Vec::new();
    for (index, document) in documents.iter().enumerate() {
        let mut score = 0.0f64;
        for (term, term_idf) in &idf {
            let Some(count) = term_counts[index].get(term).copied() else {
                continue;
            };
            let count = count as f64;
            let norm = k1 * (1.0 - b + (b * lengths[index] as f64) / average_length);
            score += term_idf * ((count * (k1 + 1.0)) / (count + norm));
        }
        if score > 0.0 {
            matches.push((document.name.clone(), score));
        }
    }

    matches.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    matches.truncate(limit);
    matches
}

/// 朴素单数化：`issues` → `issue`、`searches` → `search`
fn stem(term: &str) -> String {
    let len = term.chars().count();
    if len > 4 && term.ends_with("ies") {
        return format!("{}y", &term[..term.len() - 3]);
    }
    if len > 4
        && ["ches", "shes", "sses", "xes", "zes"]
            .iter()
            .any(|s| term.ends_with(s))
    {
        return term[..term.len() - 2].to_string();
    }
    if len > 3 && term.ends_with('s') && !term.ends_with("ss") {
        return term[..term.len() - 1].to_string();
    }
    term.to_string()
}

/// 小写分词：camelCase 边界 + 非字母数字分隔，去停用词，再取词干
pub fn tokenize(text: &str) -> Vec<String> {
    let mut spaced = String::with_capacity(text.len() + 8);
    let chars: Vec<char> = text.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        if *c == '_' {
            spaced.push(' ');
            continue;
        }
        if c.is_uppercase() {
            let prev = i.checked_sub(1).and_then(|j| chars.get(j)).copied();
            let next = chars.get(i + 1).copied();
            let boundary = match prev {
                Some(p) if p.is_lowercase() || p.is_ascii_digit() => true,
                // 连续大写后的词尾：`HTTPServer` → `HTTP Server`
                Some(p) if p.is_uppercase() && next.is_some_and(|n| n.is_lowercase()) => true,
                _ => false,
            };
            if boundary {
                spaced.push(' ');
            }
        }
        spaced.push(*c);
    }

    spaced
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|term| !term.is_empty() && !STOP_WORDS.contains(term))
        .map(stem)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(name: &str, description: &str) -> ToolDocument {
        tool_document(&ExtensionTool::simple(
            name,
            description,
            json!({ "type": "object" }),
            "",
        ))
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// 端到端：搜索命中未激活的延迟工具 → 激活 → 置「需重建工具表」标志
    #[test]
    fn executes_search_and_activates_matches() {
        use crate::core::extensions::{
            Extension, activate_deferred_tools, clear_deferred_activations,
            is_deferred_tool_activated, register_extension, take_deferred_activation_dirty,
            unregister_extension,
        };

        let _g = lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        struct McpLike;
        impl Extension for McpLike {
            fn name(&self) -> &str {
                "mcp-demo"
            }
            fn description(&self) -> &str {
                "Demo MCP server tools"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![
                    ExtensionTool::simple(
                        "github_list_issues",
                        "List issues in a GitHub repository",
                        json!({"type": "object", "properties": {"repository_name": {"type": "string"}}}),
                        "list GitHub issues",
                    )
                    .with_exposure(ToolExposure::Deferred),
                    ExtensionTool::simple(
                        "github_create_issue",
                        "Create an issue in a GitHub repository",
                        json!({"type": "object"}),
                        "create GitHub issue",
                    )
                    .with_exposure(ToolExposure::Deferred),
                    ExtensionTool::simple(
                        "weather_now",
                        "Current weather for a city",
                        json!({"type": "object"}),
                        "weather",
                    )
                    .with_exposure(ToolExposure::Deferred),
                ]
            }
        }

        register_extension(McpLike);
        clear_deferred_activations();
        let _ = take_deferred_activation_dirty();

        let ext = ToolSearch;
        // 描述会列出提供延迟工具的来源
        let description = &ext.tools()[0].description;
        assert!(description.contains("mcp-demo"), "{description}");

        let result = ext
            .execute_tool(TOOL_NAME, &json!({"query": "github issues", "limit": 1}))
            .expect("search ok");
        assert!(result.text.contains("Loaded 1 tool."), "{}", result.text);
        let loaded: Vec<String> = result
            .details
            .as_ref()
            .and_then(|d| d.get("loaded"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .expect("details.loaded");
        assert_eq!(loaded.len(), 1, "limit=1 只激活一个: {loaded:?}");
        assert!(
            loaded[0].starts_with("github_"),
            "应命中 GitHub 类工具（BM25 可能更偏短文档）: {loaded:?}"
        );
        assert!(is_deferred_tool_activated(&loaded[0]));
        assert!(
            !is_deferred_tool_activated("weather_now"),
            "不相关的不被激活"
        );
        assert!(take_deferred_activation_dirty(), "激活后应请求重建工具表");

        // 按语义无关的词检索：命中另一个工具
        let weather = ext
            .execute_tool(TOOL_NAME, &json!({"query": "current weather city"}))
            .expect("search ok");
        assert!(weather.text.contains("weather_now"), "{}", weather.text);

        // 已激活的不再重复出现在搜索结果里
        let again = ext
            .execute_tool(TOOL_NAME, &json!({"query": "github issues"}))
            .expect("search ok");
        assert!(
            !again.text.contains(&format!("- {}:", loaded[0])),
            "已激活的工具不应重复返回: {}",
            again.text
        );

        // 无命中 / 非法入参
        let none = ext
            .execute_tool(TOOL_NAME, &json!({"query": "zzzz-nothing"}))
            .expect("search ok");
        assert_eq!(none.text, "No matching tools found.");
        assert!(
            ext.execute_tool(TOOL_NAME, &json!({"query": "  "}))
                .is_err()
        );
        assert!(
            ext.execute_tool(TOOL_NAME, &json!({"query": "x", "limit": 0}))
                .is_err()
        );
        assert!(ext.execute_tool("nope", &json!({})).is_err());

        activate_deferred_tools(&["weather_now".to_string()]);
        clear_deferred_activations();
        assert!(unregister_extension("mcp-demo"));
    }

    /// pi #10285：会话恢复（重启 / fork / `/resume`）后，历史里用过的延迟工具应重新激活，
    /// 不能因激活集是内存状态而丢掉；扩展已下线的名字不算。
    #[test]
    fn restore_activations_from_restored_history() {
        use crate::core::extensions::{
            Extension, clear_deferred_activations, is_deferred_tool_activated, register_extension,
            restore_deferred_activations, unregister_extension,
        };
        use crate::core::provider::{AgentMessage, ContentBlock};

        let _g = lock();
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        fn call(name: &str) -> AgentMessage {
            let mut msg = AgentMessage::user_text("");
            msg.role = "assistant".to_string();
            msg.content = vec![ContentBlock::ToolCall {
                id: format!("call-{name}"),
                name: name.to_string(),
                arguments: json!({}),
                thought_signature: None,
                namespace: None,
            }];
            msg
        }

        struct Deferred;
        impl Extension for Deferred {
            fn name(&self) -> &str {
                "restore-demo"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![
                    ExtensionTool::simple("restore_hit", "d", json!({}), "s")
                        .with_exposure(ToolExposure::Deferred),
                    ExtensionTool::simple("restore_other", "d", json!({}), "s")
                        .with_exposure(ToolExposure::Deferred),
                ]
            }
        }

        register_extension(Deferred);
        clear_deferred_activations();

        // 历史里调用过 restore_hit；扩展未声明的名字一律不激活
        let history = vec![
            call("restore_hit"),
            call("gone_tool"),
            call("restore_other"),
        ];
        assert!(restore_deferred_activations(&history), "应报告激活集有变化");
        assert!(is_deferred_tool_activated("restore_hit"));
        assert!(is_deferred_tool_activated("restore_other"));

        // 已全部激活：重复恢复不再报告变化
        clear_deferred_activations();
        assert!(restore_deferred_activations(&[call("restore_hit")]));
        assert!(!restore_deferred_activations(&[call("restore_hit")]));

        // 历史里没有延迟工具调用 / 空历史：不动激活集
        clear_deferred_activations();
        assert!(!restore_deferred_activations(&[]));
        assert!(!restore_deferred_activations(&[call("read")]));
        assert!(!is_deferred_tool_activated("restore_hit"));

        clear_deferred_activations();
        assert!(unregister_extension("restore-demo"));
    }

    /// 2.3 子集：`namespace` 与 `outputSchema` 也进检索文本——按输出字段名也能搜到该工具。
    #[test]
    fn namespace_and_output_schema_are_searchable() {
        use crate::core::extensions::{
            Extension, clear_deferred_activations, register_extension, unregister_extension,
        };

        let _g = lock();
        let _ad = crate::test_support::AgentDirGuard::temp();

        struct SchemaProbe;
        impl Extension for SchemaProbe {
            fn name(&self) -> &str {
                "schema-probe"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                vec![
                    ExtensionTool::simple(
                        "convert_currency",
                        "Convert money between currencies",
                        json!({ "type": "object" }),
                        "convert money",
                    )
                    .with_namespace("finance")
                    .with_output_schema(json!({
                        "type": "object",
                        "properties": { "settlement_amount": { "type": "number" } }
                    }))
                    .with_exposure(ToolExposure::Deferred),
                ]
            }
        }

        register_extension(SchemaProbe);
        clear_deferred_activations();
        let ext = ToolSearch;

        // 输出 schema 的字段名 `settlement_amount` → 词元 `settlement`/`amount`
        let r = ext
            .execute_tool(
                TOOL_NAME,
                &json!({ "query": "settlement amount", "limit": 3 }),
            )
            .expect("search ok");
        let loaded: Vec<String> = r
            .details
            .as_ref()
            .and_then(|d| d.get("loaded"))
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default();
        assert!(
            loaded.contains(&"convert_currency".to_string()),
            "{}",
            r.text
        );

        // namespace 也在检索文本里
        clear_deferred_activations();
        let r = ext
            .execute_tool(TOOL_NAME, &json!({ "query": "finance", "limit": 3 }))
            .expect("search ok");
        assert!(r.text.contains("convert_currency"), "{}", r.text);

        clear_deferred_activations();
        assert!(unregister_extension("schema-probe"));
    }

    #[test]
    fn tokenizes_camel_case_and_stems() {
        assert_eq!(tokenize("readFile"), vec!["read", "file"]);
        assert_eq!(tokenize("HTTPServer"), vec!["http", "server"]);
        assert_eq!(tokenize("list_issues"), vec!["list", "issue"]);
        // 停用词被丢弃
        assert_eq!(tokenize("the of and"), Vec::<String>::new());
    }

    #[test]
    fn ranks_relevant_tools_first() {
        let docs = vec![
            doc("bash", "Run a shell command"),
            doc("github_list_issues", "List issues in a GitHub repository"),
            doc("read", "Read a file"),
        ];
        let matches = rank("list github issues", &docs, DEFAULT_LIMIT);
        assert_eq!(matches[0].0, "github_list_issues");
        assert!(
            matches
                .iter()
                .all(|(name, _)| name != "read" || name.is_empty())
        );
    }

    #[test]
    fn ranking_respects_limit_and_empty_query() {
        let docs = vec![doc("alpha", "alpha tool"), doc("beta", "beta tool")];
        assert!(rank("", &docs, 8).is_empty());
        assert!(rank("alpha", &docs, 0).is_empty());
        assert_eq!(rank("tool", &docs, 1).len(), 1);
        assert!(rank("nonexistent", &docs, 8).is_empty());
    }

    #[test]
    fn search_document_includes_schema_property_names() {
        let tool = ExtensionTool::simple(
            "mcp_call",
            "Invoke an MCP tool",
            json!({
                "type": "object",
                "properties": { "repository_name": { "type": "string", "description": "GitHub repo" } }
            }),
            "",
        );
        let document = tool_document(&tool);
        // 字段名与字段描述都在检索文本里（`repository_name` → repository / name）
        assert!(document.tokens.contains(&"repository".to_string()));
        assert!(document.tokens.contains(&"git".to_string()));
    }
}

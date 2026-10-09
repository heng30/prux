//! `codemode` 工具描述的生成
//!
//! 描述是**动态**的：可调工具表一变（激活集合、`tool_search` 加载了新工具…），
//! 本模块就重新生成，把工具的 TypeScript 声明按 `inlineBudget` 预算内联进去；
//! 放不下的工具只进命名空间计数，脚本再用 `searchTools()` / `describeTool()` /
//! `describeNamespace()` 找。

use super::{declarations, declarations::render_tool_sample};
use crate::core::{extensions::ExtensionTool, system_prompt};
use std::collections::{BTreeMap, HashSet};
use strum_macros::{EnumIter, EnumString, IntoStaticStr};

/// 估算 token 时的每 token 字符数（字符数 ÷ 4）。
pub const CHARS_PER_TOKEN: usize = 4;

/// 默认的内联预算（估算 token 数）。
pub const DEFAULT_INLINE_BUDGET: usize = 3_000;

/// 工具声明没被全部内联时的提示段
const DEFERRED_TOOLS_GUIDANCE: &str = r#"Some deferred nested tools may be omitted from this description. They are still available on the global `tools` object and listed in `ALL_TOOLS`.
To find one, call `await searchTools(query)`, browse a namespace with `await describeNamespace(name)`, or filter `ALL_TOOLS` by `name` and `description`."#;

/// `models.*` 的五个全局签名（类型定义与字段语义在 codemode 文档里，**不进描述**）。
const MODEL_API_SIGNATURES: &str = r#"```ts
declare const models: {
  getModelsOfType(type: ModelType, provider?: string): Promise<ModelInfo[]>;
  getAvailableOfType(type: ModelType, provider?: string): Promise<ModelInfo[]>;
  getModelOfType(type: ModelType, provider: string, id: string): Promise<ModelInfo | undefined>;
  classify(model: ModelInfo, context: ClassifierContext): Promise<ClassifierResult>;
  generateImages(model: ModelInfo, context: ImagesContext): Promise<ImagesResult>;
};
```"#;

/// `models.classify()` / `models.generateImages()` 的调用约定（并发闸门、用量、图片展示）。
const MODEL_API_CONVENTIONS: &str = "At most four `classify()` / `generateImages()` calls run at once per script (the rest queue); their usage counts toward the session cost. Show generated images with `image(block)`, never print their `data`.";

/// codemode 文档的完整路径（模型可用 `read` 取回类型定义与更多例子）。
fn codemode_docs_path() -> String {
    system_prompt::docs_dir()
        .join("extensions")
        .join("codemode.md")
        .to_string_lossy()
        .to_string()
}

/// `models.*` 段：文档指引 + 签名清单 + 调用约定。
fn model_api_section() -> String {
    format!(
        "Model API (`ModelType` is `\"chat\" | `\"image\" | `\"classifier\"`; field types and semantics are in the codemode doc):\n{}\n{}",
        MODEL_API_SIGNATURES, MODEL_API_CONVENTIONS
    )
}

/// 指令段（[`INTRO`] 填上文档路径后的成品）。
fn intro_section() -> String {
    INTRO.replace("{docs}", &codemode_docs_path())
}

/// 指令段：脚本全局、输出助手、返回值语义。
///
/// 刻意压到 6 行以内（这部分是每次请求的固定开销）：沙箱细节、`@options` 字段含义、
/// `searchTools` 选项与命名空间别名等都在 codemode 文档里，模型需要时用 `read` 取。
///
/// `{docs}` 由 [`intro_section`] 替换为文档绝对路径。
const INTRO: &str = r#"Run JavaScript code to orchestrate/compose tool calls
- Runs as the body of an async function in a fresh QuickJS sandbox: top-level `await`/`return` work; no Node, filesystem, network or timers; 256 MB heap.
- `tools.<name>(args)` calls a nested tool (object in, object or string out) and rejects with an Error carrying its error text on failure, denial or bad arguments. Calls are real and have side effects.
- `text(value)` / `console.log(...)` append output, `image(<base64 data URI>)` appends an image and saves it to a temp file (the result names its path), top-level `return value` appends like `text()`, `exit()` ends it successfully. With several text items each starts with a `==> text N/M <==` line, and `console` lines follow the other output in one `<console_output>` block.
- `store(key, value)` / `load(key)` keep JSON values across scripts; `ALL_TOOLS` lists nested tools; `await searchTools(query, { limit, namespace })`, `await describeTool(name)`, `await describeNamespace(name)` find tools and declarations.
- Optional first line: `// @options: {"max_output_tokens": 1000, "timeout_ms": 60000}` (output token budget, hard deadline). Docs and examples: {docs}"#;

/// `codemode.mode`：脚本可调的工具在模型侧怎么呈现。
///
/// 两个变体的差别集中在三处：是否给 `direct` 工具追加脚本调用样例、`codemode` 自己的描述
/// 列哪些工具、以及是否把工具声明从请求里摘掉。具体处理见 [`build_loadout`]。
/// 变体名与配置字符串的互转由 `strum` 派生（全小写、大小写不敏感），见 [`Mode::as_str`] / [`Mode::parse`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, EnumIter, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum Mode {
    /// `on`（默认；[`Mode::parse`] 对无法识别的取值返回 `None`，调用方兜底到这个）：
    ///
    /// - `declared` 中属于 `callable` 的工具，描述改写成「原文 + `codemode tool declaration` 代码块」，模型既能直接调，也知道能写进脚本；
    /// - `codemode` 自己的描述只列 `!is_direct` 的工具，即模型侧没有独立声明的那些
    ///   （`codemode` exposure 的 MCP 工具、`deferred` 工具）；
    /// - 不隐藏任何声明（`hidden_declarations` 为空）。
    ///
    /// 注意 `deferred` 工具会同时出现在自身声明和 `codemode` 描述里：`is_direct` 只认
    /// `ToolExposure::Direct`，而激活不改变曝光类型。
    #[default]
    On,
    /// `only`：可调工具**在请求里不再声明**，模型只能经脚本调。
    ///
    /// - `callable` 全量进 `codemode` 的描述（含 `direct` 工具），因此不再追加脚本调用样例；
    /// - `direct` 且出现在 `declared` 里的工具名进入 `hidden_declarations`，由 `agent_loop`
    ///   在发请求前从工具表里过滤掉——只是不发给模型，工具仍激活、仍可被脚本和嵌套调用。
    Only,
}

impl Mode {
    /// 该模式在配置里的字符串（`strum` 派生的小写名：`on` / `only`）。
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析配置字符串（去首尾空白、大小写不敏感）；无法识别时返回 `None`。
    pub fn parse(value: &str) -> Option<Self> {
        value.trim().parse().ok()
    }
}

/// 一次描述生成的结果：`codemode` 自身的描述 + 需要改写的其它工具描述 + 要隐藏声明的工具。
#[derive(Debug, Clone, Default)]
pub struct Loadout {
    /// 按工具名覆盖的描述（含 `codemode` 自己）
    pub descriptions: BTreeMap<String, String>,
    /// `only` 模式下从请求里摘掉声明的工具名
    pub hidden_declarations: Vec<String>,
}

/// 单个工具在描述里的一段：`### \`id\` (\`raw name\`)` + 描述与声明。
fn render_tool_section(tool: &ExtensionTool) -> String {
    let id = declarations::to_codemode_identifier(&tool.name);
    let heading = if id == tool.name {
        format!("### `{id}`")
    } else {
        format!("### `{id}` (`{}`)", tool.name)
    };

    let sample = render_tool_sample(
        &tool.name,
        &tool.description,
        &tool.prompt_guidelines,
        Some(&tool.parameters),
        tool.output_schema.as_ref(),
    );

    format!("{heading}\n{}", sample.trim())
}

/// 目录里的一项
struct CatalogEntry<'a> {
    /// 工具
    tool: &'a ExtensionTool,
    /// 它的段文本
    section: String,
    /// 估算成本
    cost: usize,
    /// 是否属于 deferred。
    deferred: bool,
}

/// 按命名空间分组（`None` 组排在最前，其余按命名空间名字典序）。
fn group_by_namespace<'a>(
    tools: &[&'a ExtensionTool],
) -> Vec<(Option<String>, Vec<CatalogEntry<'a>>)> {
    let mut groups: BTreeMap<Option<String>, Vec<CatalogEntry<'a>>> = BTreeMap::new();

    for tool in tools {
        let section = render_tool_section(tool);
        let cost = section.chars().count().div_ceil(CHARS_PER_TOKEN);

        groups
            .entry(tool.namespace.clone())
            .or_default()
            .push(CatalogEntry {
                tool,
                section,
                cost,
                deferred: false,
            });
    }

    // `None`（无命名空间的工具）排最前，其余按名字典序。
    let mut out: Vec<(Option<String>, Vec<CatalogEntry<'a>>)> = groups.into_iter().collect();
    out.sort_by(|a, b| match (&a.0, &b.0) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Less,
        (Some(_), None) => std::cmp::Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(y),
    });

    out
}

/// 在预算内挑选要内联的工具段：每轮让每个分组放它当前最便宜的一个，
/// 放不下的分组退出而其余继续——保证每个命名空间先被代表，再被列全
fn select_catalog(
    groups: &[(Option<String>, Vec<CatalogEntry<'_>>)],
    budget: Option<usize>,
) -> HashSet<String> {
    let mut queues: Vec<Vec<&CatalogEntry<'_>>> = groups
        .iter()
        .map(|(_, entries)| {
            let mut listable: Vec<&CatalogEntry<'_>> =
                entries.iter().filter(|e| !e.deferred).collect();
            listable.sort_by_key(|e| e.cost);
            listable
        })
        .filter(|q| !q.is_empty())
        .collect();

    if budget.is_none() {
        return queues
            .iter()
            .flat_map(|q| q.iter().map(|e| e.tool.name.clone()))
            .collect();
    }

    let mut remaining = budget.unwrap_or(0);
    let mut shown = HashSet::new();
    loop {
        let mut progressed = false;
        queues.retain_mut(|queue| {
            let Some(next) = queue.first().copied() else {
                return false;
            };

            if next.cost > remaining {
                return false;
            }

            remaining -= next.cost;
            shown.insert(next.tool.name.clone());
            queue.remove(0);
            progressed = true;
            !queue.is_empty()
        });

        if !progressed {
            break;
        }
    }

    shown
}

/// 生成 `codemode` 的工具描述。
///
/// `callable` 是脚本可调的工具（按 `namespace` 分组）；`deferred` 里的工具照旧可调但**不内联**
/// （脚本用 `searchTools()` 找）；`inline_budget` 为 None 时内联全部非 deferred 工具。
pub fn create_description(
    callable: &[&ExtensionTool],
    deferred: &HashSet<String>,
    inline_budget: Option<usize>,
) -> String {
    let mut groups = group_by_namespace(callable);
    for (_, entries) in groups.iter_mut() {
        for entry in entries.iter_mut() {
            entry.deferred = deferred.contains(&entry.tool.name);
        }
    }

    let shown = select_catalog(&groups, inline_budget);
    let complete = shown.len() == callable.len();

    let mut sections = vec![intro_section()];
    if !complete {
        sections.push(DEFERRED_TOOLS_GUIDANCE.to_string());
    }
    sections.push(model_api_section());

    if callable.is_empty() {
        sections
            .push("No nested tools: there is currently nothing to call from a script.".to_string());
        return sections.join("\n\n");
    }

    let mut tool_sections = vec![if complete {
        format!(
            "Nested tools: COMPLETE list ({} tool{}).",
            callable.len(),
            if callable.len() == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "Nested tools: PARTIAL - {} of {} shown.",
            shown.len(),
            callable.len()
        )
    }];

    for (namespace, entries) in &groups {
        let visible: Vec<&CatalogEntry<'_>> = entries
            .iter()
            .filter(|e| shown.contains(&e.tool.name))
            .collect();

        if let Some(namespace) = namespace {
            let count = format!(
                "{} tool{}",
                entries.len(),
                if entries.len() == 1 { "" } else { "s" }
            );

            let suffix = if visible.len() == entries.len() {
                String::new()
            } else if visible.is_empty() {
                ", none shown".to_string()
            } else {
                format!(", {} shown", visible.len())
            };

            tool_sections.push(format!("## {namespace} ({count}{suffix})"));
        }

        for entry in visible {
            tool_sections.push(entry.section.clone());
        }
    }

    sections.push(tool_sections.join("\n\n"));
    sections.join("\n\n")
}

/// 由激活集合算出本次装载的全部改动（`on`/`only` 两种模式，差别见 [`Mode`]）。
///
/// 返回的 `descriptions` 按工具名覆盖描述（含 `codemode` 自己），
/// `hidden_declarations` 是本次请求不该声明的工具名。
///
/// - `declared`：模型可见的工具（`direct` + 已激活 `deferred` 等）
/// - `callable`：脚本可调的工具（⊇ `declared`，含 `codemode` exposure 的）
/// - `is_direct`：按工具名回答它在 `declared` 里是否 `direct` 曝光；决定 `on` 模式下 `codemode` 描述列哪些工具、`only` 模式下隐藏哪些声明
/// - `deferred`：可调但默认不内联的工具名（预算足够时也只进命名空间计数）
pub fn build_loadout(
    codemode_tool_name: &str,
    declared: &[ExtensionTool],
    callable: &[ExtensionTool],
    is_direct: &dyn Fn(&str) -> bool,
    deferred: &HashSet<String>,
    mode: Mode,
    inline_budget: Option<usize>,
) -> Loadout {
    let callable_refs: Vec<&ExtensionTool> = callable.iter().collect();
    let callable_names: HashSet<&str> = callable.iter().map(|t| t.name.as_str()).collect();

    let mut descriptions = BTreeMap::new();
    if mode == Mode::On {
        for tool in declared {
            if callable_names.contains(tool.name.as_str()) {
                descriptions.insert(
                    tool.name.clone(),
                    render_tool_sample(
                        &tool.name,
                        &tool.description,
                        &tool.prompt_guidelines,
                        Some(&tool.parameters),
                        tool.output_schema.as_ref(),
                    ),
                );
            }
        }
    }

    let listed: Vec<&ExtensionTool> = match mode {
        Mode::On => callable_refs
            .iter()
            .copied()
            .filter(|t| !is_direct(&t.name))
            .collect(),
        Mode::Only => callable_refs.clone(),
    };
    descriptions.insert(
        codemode_tool_name.to_string(),
        create_description(&listed, deferred, inline_budget),
    );

    let hidden_declarations = match mode {
        Mode::Only => callable
            .iter()
            .filter(|t| {
                t.name != codemode_tool_name
                    && is_direct(&t.name)
                    && declared.iter().any(|d| d.name == t.name)
            })
            .map(|t| t.name.clone())
            .collect(),
        Mode::On => Vec::new(),
    };

    Loadout {
        descriptions,
        hidden_declarations,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 造一个扩展工具（描述 + 参数 schema + 可选命名空间）。
    fn tool(name: &str, description: &str, namespace: Option<&str>) -> ExtensionTool {
        let mut t = ExtensionTool::simple(
            name,
            description,
            json!({"type": "object", "properties": {"path": {"type": "string"}}}),
            "",
        );
        t.namespace = namespace.map(|s| s.to_string());
        t
    }

    /// 无预算时内联全部工具，并标 COMPLETE。
    #[test]
    fn lists_every_tool_without_budget() {
        let tools = [
            tool("read", "Read a file", None),
            tool("bash", "Run a command", None),
        ];
        let refs: Vec<&ExtensionTool> = tools.iter().collect();
        let text = create_description(&refs, &HashSet::new(), None);
        assert!(
            text.contains("Nested tools: COMPLETE list (2 tools)."),
            "{text}"
        );
        assert!(text.contains("### `read`"), "{text}");
        assert!(text.contains("### `bash`"), "{text}");
        assert!(!text.contains("PARTIAL"), "{text}");
    }

    /// 预算不够时只内联放得下的，并给出 PARTIAL + 提示。
    #[test]
    fn respects_inline_budget() {
        let tools = [
            tool("read", "Read a file", None),
            tool("bash", "Run a command", None),
        ];
        let refs: Vec<&ExtensionTool> = tools.iter().collect();
        let text = create_description(&refs, &HashSet::new(), Some(1));
        assert!(
            text.contains("Nested tools: PARTIAL - 0 of 2 shown."),
            "{text}"
        );
        assert!(
            text.contains("Some deferred nested tools may be omitted"),
            "{text}"
        );
    }

    /// 命名空间分组：无命名空间的排前，命名空间带工具计数。
    #[test]
    fn groups_by_namespace() {
        let tools = [
            tool("read", "Read a file", None),
            tool("mcp__a__x", "A", Some("ologs")),
            tool("mcp__a__y", "B", Some("ologs")),
        ];
        let refs: Vec<&ExtensionTool> = tools.iter().collect();
        let text = create_description(&refs, &HashSet::new(), None);
        assert!(text.contains("## ologs (2 tools)"), "{text}");
        let plain = text.find("### `read`").unwrap();
        let grouped = text.find("## ologs").unwrap();
        assert!(plain < grouped, "{text}");
    }

    /// deferred 工具可调但不内联（预算足够时也只进计数）。
    #[test]
    fn deferred_tools_are_not_inlined() {
        let tools = [tool("hidden_one", "Deferred", None)];
        let refs: Vec<&ExtensionTool> = tools.iter().collect();
        let deferred: HashSet<String> = ["hidden_one".to_string()].into_iter().collect();
        let text = create_description(&refs, &deferred, None);
        assert!(!text.contains("### `hidden_one`"), "{text}");
        assert!(
            text.contains("Nested tools: PARTIAL - 0 of 1 shown."),
            "{text}"
        );
    }

    /// 空工具表给出明确说明。
    #[test]
    fn empty_catalog_says_so() {
        let text = create_description(&[], &HashSet::new(), None);
        assert!(text.contains("No nested tools"), "{text}");
    }

    /// 描述带 `models.*` 的签名清单与文档路径；类型定义已搬进文档，不得再回描述里。
    #[test]
    fn description_includes_model_api_signatures_and_docs_path() {
        let tools = [tool("read", "Read a file", None)];
        let refs: Vec<&ExtensionTool> = tools.iter().collect();
        for text in [
            create_description(&refs, &HashSet::new(), None),
            create_description(&[], &HashSet::new(), None),
        ] {
            assert!(text.contains("Model API"), "{text}");
            assert!(
                text.contains("getModelsOfType(type: ModelType, provider?: string)"),
                "{text}"
            );
            assert!(
                text.contains("classify(model: ModelInfo, context: ClassifierContext)"),
                "{text}"
            );
            assert!(
                text.contains("generateImages(model: ModelInfo, context: ImagesContext)"),
                "{text}"
            );
            let docs = crate::core::system_prompt::docs_dir()
                .join("extensions")
                .join("codemode.md")
                .to_string_lossy()
                .to_string();
            assert!(text.contains(&docs), "应指向 codemode 文档 {docs}: {text}");
            // 类型块搬走后不得再回来（固定开销主要靠这一条守住）
            assert!(!text.contains("interface ModelInfo"), "{text}");
            assert!(!text.contains("type ClassifierQuestion"), "{text}");
            assert!(!text.contains("type ModelType"), "{text}");
        }
    }

    /// 2.7：固定开销（压缩后的 INTRO + Model API，不含工具声明段）压到 ~500 token 量级。
    ///
    /// 这部上限的真正意图是守「类型定义不回流描述」（见下方三条断言）；
    /// 数值本身是量出来的：类型块搬走后曾降到 ~460，2.7 补上 image 落盘与
    /// 输出分隔说明后升到 ~520，因此上限定在 530。
    #[test]
    fn fixed_overhead_stays_small() {
        let text = create_description(&[], &HashSet::new(), None);
        let tokens = text.chars().count().div_ceil(CHARS_PER_TOKEN);
        assert!(
            tokens <= 530,
            "固定开销 {tokens} token，已超出 ~500 的目标（类型定义是否又回描述里了？）: {text}"
        );
    }

    /// `on` 模式：direct 工具的声明改写成「描述 + 脚本声明」，codemode 描述只列非 direct。
    #[test]
    fn on_mode_appends_declarations_to_direct_tools() {
        let declared = [tool("read", "Read a file", None)];
        let callable = [
            tool("read", "Read a file", None),
            tool("mcp_x", "MCP tool", None),
        ];
        let loadout = build_loadout(
            "codemode",
            &declared,
            &callable,
            &|n| n == "read",
            &HashSet::new(),
            Mode::On,
            None,
        );
        assert!(
            loadout.descriptions["read"].contains("codemode tool declaration:"),
            "{:?}",
            loadout.descriptions
        );
        assert!(loadout.descriptions["codemode"].contains("### `mcp_x`"));
        assert!(!loadout.descriptions["codemode"].contains("### `read`"));
        assert!(loadout.hidden_declarations.is_empty());
    }

    /// `only` 模式：direct 工具的声明进 hidden_declarations，codemode 描述列全部。
    #[test]
    fn only_mode_hides_direct_declarations() {
        let declared = [tool("read", "Read a file", None)];
        let callable = [tool("read", "Read a file", None)];
        let loadout = build_loadout(
            "codemode",
            &declared,
            &callable,
            &|n| n == "read",
            &HashSet::new(),
            Mode::Only,
            None,
        );
        assert_eq!(loadout.hidden_declarations, vec!["read".to_string()]);
        assert!(loadout.descriptions["codemode"].contains("### `read`"));
    }

    /// 可调工具为空时描述只含指令段 + 「没有工具」说明。
    #[test]
    fn no_callable_tools_says_so() {
        let loadout = build_loadout(
            "codemode",
            &[],
            &[],
            &|_| false,
            &HashSet::new(),
            Mode::On,
            None,
        );
        assert!(
            loadout.descriptions["codemode"].contains("No nested tools"),
            "{:?}",
            loadout.descriptions
        );
    }

    /// strum 派生的双向转换：`as_str` 是规范小写名，`parse` 去空白且大小写不敏感。
    #[test]
    fn mode_strum_conversions() {
        assert_eq!(Mode::On.as_str(), "on");
        assert_eq!(Mode::Only.as_str(), "only");
        assert_eq!(Mode::parse("only"), Some(Mode::Only));
        assert_eq!(Mode::parse(" ON "), Some(Mode::On));
        assert_eq!(Mode::parse("weird"), None);
        assert_eq!(Mode::default(), Mode::On);
    }
}

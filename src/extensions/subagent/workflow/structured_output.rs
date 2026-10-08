//! `StructuredOutput` 注入工具：`agent({schema})` 的落点。
//!
//! 一个 workflow 脚本传`schema` 时要的是**对象**，不是需要自己解析的散文。
//! 做法是给子代理注入一个名为 `StructuredOutput` 的工具，
//! 其 input schema 就是调用方的 schema，校验通过的载荷作为该 agent 的结果回传。
//!
//! 用两重压力逼近"必须从这次调用交回答案"：
//!
//! 1. 工具描述 / snippet / guidelines；
//! 2. 这里的校验 —— 不匹配的载荷返回 `Err`，核心转成 `is_error` 工具结果，模型在**同一 run 内**看得见原因并改正。
//!    这一层最要紧，故用 `jsonschema` 做真校验（嵌套 / `enum` / 数组 `items` / 数值与字符串约束全查），
//!    而不是复用核心那份浅层校验。

use crate::core::{
    extensions::{ExtensionTool, InjectedChildTool, ToolAnnotations, ToolExposure},
    tools::{ToolError, ToolResult},
};
use jsonschema::Validator;
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// 捕获槽的共享类型
pub(crate) type CaptureSlot = Arc<Mutex<StructuredCapture>>;

/// 工具名
pub(crate) const STRUCTURED_OUTPUT_TOOL_NAME: &str = "StructuredOutput";

/// schema 序列化后的字节上限
const MAX_SCHEMA_BYTES: usize = 64 * 1024;

/// 报告给模型的最大错误条数，免得一个全错的载荷塞满上下文。
const MAX_REPORTED_ERRORS: usize = 5;

/// 编译好的 schema：原样保留（journal 键与工具 `parameters` 都要它）+ 校验器。
#[derive(Debug)]
pub(crate) struct CompiledSchema {
    /// 原样 schema（成为工具的 input schema）。
    schema: Value,
    /// 编译好的校验器。
    validator: Validator,
}

impl CompiledSchema {
    /// 原样 schema（成为工具的 input schema）。
    pub(crate) fn schema(&self) -> &Value {
        &self.schema
    }

    /// JSON Pointer 的 `/a/b` → `$.a.b`——schema 是模型用 JavaScript写的，`$.a.b` 对它远比 `/a/b` 好读。
    pub(crate) fn check(&self, value: &Value) -> Result<(), String> {
        let mut reported: Vec<String> = Vec::new();

        for error in self.validator.iter_errors(value) {
            let pointer = error.instance_path().as_str();
            let at = if pointer.is_empty() {
                "$".to_string()
            } else {
                format!("${}", pointer.replace('/', "."))
            };

            reported.push(format!("{at}: {error}"));
            if reported.len() >= MAX_REPORTED_ERRORS {
                break;
            }
        }

        if reported.is_empty() {
            Ok(())
        } else {
            Err(reported.join("; "))
        }
    }
}

/// 把脚本给的 schema 编译成可校验的东西。
///
/// 三条检查都在**调用发生的时刻**完成，失败就报错。
/// 一个根本不能当 input schema 的 schema 会毁掉子代理的**每一次**请求，而不是最后一次，
/// 所以应该在模型被花钱发现这件事之前听见。
pub(crate) fn compile(schema: &Value) -> Result<CompiledSchema, String> {
    if !schema.is_object() {
        return Err("agent() opts.schema must be a JSON Schema object.".to_string());
    }

    if schema.get("type").and_then(|v| v.as_str()) != Some("object") {
        return Err(
            "agent() opts.schema must have `type: \"object\"` at its root — it becomes the \
             tool's input schema, and a non-object root is not something a model can be asked \
             to fill."
                .to_string(),
        );
    }

    let serialized = serde_json::to_string(schema)
        .map_err(|_| "agent() opts.schema must be JSON-serializable.".to_string())?;

    if serialized.len() > MAX_SCHEMA_BYTES {
        return Err(format!(
            "agent() opts.schema is too large ({} bytes; the limit is {MAX_SCHEMA_BYTES}).",
            serialized.len()
        ));
    }

    let validator = jsonschema::validator_for(schema).map_err(|e| {
        format!("agent() opts.schema is not a schema this runtime can validate: {e}")
    })?;

    // 冒烟：让"校验器走不动的 schema"在写出它的那次调用就暴露，
    // 而不是在某个子代理的工具处理器里——那里的唯一症状是一个永远不返回的 agent。
    _ = validator.is_valid(&Value::Object(serde_json::Map::new()));
    Ok(CompiledSchema {
        schema: schema.clone(),
        validator,
    })
}

/// 重放出来的文本是否仍满足 schema。
///
/// 键里已经含 schema（改了 schema 就不会命中重放），这道检查是给**没被任何东西校验过**的那条路径兜底：
/// 手改过的 journal、被截断的末行留下的半截内容。
pub(crate) fn check_text(schema: &Value, text: &str) -> Result<(), String> {
    let compiled = compile(schema)?;
    let parsed: Value = serde_json::from_str(text).map_err(|_| {
        "The agent did not return structured output: its answer was not JSON.".to_string()
    })?;
    compiled.check(&parsed)
}

/// 子代理交回的东西，随工具调用逐步填上。
#[derive(Debug, Default)]
pub(crate) struct StructuredCapture {
    /// 最后一次**校验通过**的载荷。
    pub(crate) json: Option<Value>,
    /// 这个工具**被调用过**没有——"从没试过"与"试过但不对"需要不同的措辞。
    pub(crate) called: bool,
    /// 最近一次被拒的原因，供失败文案取用。
    pub(crate) last_error: Option<String>,
}

/// 新建一个空的捕获槽。
pub(crate) fn capture_slot() -> CaptureSlot {
    Arc::new(Mutex::new(StructuredCapture::default()))
}

/// 造出要给 child 注入的 `StructuredOutput` 工具。
///
/// `parameters` 就是调用方的 schema 本身——这正是让 provider 去填字段的东西；
/// `prepare_arguments` 把"模型把整个载荷当成一个 JSON 字符串发过来"救回来（零成本，省掉一整次重试）；
/// handler 里的校验失败走 `Err`，核心会把它变成 `is_error` 工具结果。
pub(crate) fn build_tool(compiled: Arc<CompiledSchema>, capture: CaptureSlot) -> InjectedChildTool {
    let checker = compiled.clone();
    let handler: Arc<dyn Fn(Value) -> Result<ToolResult, ToolError> + Send + Sync> = Arc::new(
        move |args: Value| {
            let verdict = {
                let mut slot = capture.lock().unwrap();
                slot.called = true;
                match checker.check(&args) {
                    Ok(()) => {
                        slot.json = Some(args.clone());
                        slot.last_error = None;
                        None
                    }
                    Err(verdict) => {
                        slot.last_error = Some(verdict.clone());
                        Some(verdict)
                    }
                }
            };

            // 最后一次有效调用胜出：模型调两次就是第二次的意思。
            match verdict {
                None => Ok(ToolResult::text("Recorded.")),
                Some(verdict) => Err(ToolError(format!(
                    "StructuredOutput did not match the required schema:\n{verdict}\nCall it again with a corrected value."
                ))),
            }
        },
    );

    InjectedChildTool {
        meta: ExtensionTool {
            exposure: ToolExposure::Direct,
            namespace: None,
            annotations: ToolAnnotations::default(),
            output_schema: Some(compiled.schema().clone()),
            name: STRUCTURED_OUTPUT_TOOL_NAME.to_string(),
            description: "Report your final answer. Call this exactly once, with the complete \
                          result, and put everything the caller needs inside the arguments — \
                          text written outside this call is discarded. If a call is rejected \
                          for not matching the schema, fix the reported fields and call it \
                          again."
                .to_string(),
            label: Some("Structured Output".to_string()),
            parameters: compiled.schema().clone(),
            snippet: "Report your final answer as structured data".to_string(),
            prompt_guidelines: vec![
                "Your final answer MUST be reported by calling StructuredOutput. \
                Prose outside that call is discarded."
                    .to_string(),
            ],
            // 声明成 true 会是撒谎：核心按工具名查全局注册表，per-child 注入的工具不在其中，
            // 所以 provider 侧 strict 约束采样实际拿不到。
            constrained_sampling: false,
            render_shell: None,
            execution_mode: None,
            // 模型偶尔把整个载荷当成一个 JSON 字符串发过来；救回来不要钱，能省一次重试。
            prepare_arguments: Some(|args: &Value| {
                if let Value::String(raw) = args
                    && let Ok(parsed) = serde_json::from_str::<Value>(raw)
                {
                    return parsed;
                }
                args.clone()
            }),
            grammar_sampling: None,
        },
        handler,
    }
}

/// 子代理结束时没有可用结构化载荷 → 该 agent 的失败原因
pub(crate) fn failure_message(capture: &StructuredCapture) -> String {
    match capture.last_error.as_deref() {
        Some(last) => {
            format!("The agent's StructuredOutput call did not match the required schema: {last}")
        }
        None => "The agent did not report its answer through StructuredOutput.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn compiled(schema: Value) -> CompiledSchema {
        compile(&schema).expect("schema should compile")
    }

    #[test]
    fn compile_rejects_a_non_object_root() {
        assert!(compile(&json!("nope")).is_err());
        assert!(compile(&json!({ "type": "array" })).is_err());
        assert!(
            compile(&json!({})).is_err(),
            "missing type must be rejected"
        );
    }

    #[test]
    fn compile_rejects_an_oversized_schema() {
        // 一个 65KB+ 的 description 足以越过上限
        let big = "x".repeat(MAX_SCHEMA_BYTES);
        let err = compile(&json!({ "type": "object", "description": big })).unwrap_err();
        assert!(err.contains("too large"), "{err}");
    }

    #[test]
    fn compile_rejects_a_schema_the_validator_cannot_walk() {
        let err = compile(&json!({ "type": "object", "properties": { "a": { "type": 7 } } }))
            .unwrap_err();
        assert!(
            err.contains("not a schema this runtime can validate"),
            "{err}"
        );
    }

    #[test]
    fn deep_validation_catches_nesting_enums_and_bounds() {
        let c = compiled(json!({
            "type": "object",
            "required": ["name"],
            "additionalProperties": false,
            "properties": {
                "name": { "type": "string", "minLength": 2 },
                "level": { "enum": ["low", "high"] },
                "tags": { "type": "array", "items": { "type": "string" } },
                "inner": {
                    "type": "object",
                    "required": ["n"],
                    "properties": { "n": { "type": "integer", "minimum": 3 } }
                }
            }
        }));
        assert!(c.check(&json!({"name": "ok"})).is_ok());
        // 嵌套对象的嵌套必填 —— 核心那份浅层校验看不到这一层
        let err = c.check(&json!({"name": "ok", "inner": {}})).unwrap_err();
        assert!(err.contains("$.inner"), "{err}");
        // 嵌套数值下界
        assert!(c.check(&json!({"name": "ok", "inner": {"n": 1}})).is_err());
        // enum
        assert!(c.check(&json!({"name": "ok", "level": "medium"})).is_err());
        // 数组 items
        assert!(c.check(&json!({"name": "ok", "tags": ["a", 2]})).is_err());
        // additionalProperties: false
        assert!(c.check(&json!({"name": "ok", "extra": 1})).is_err());
        // 顶层 required 缺失时路径是 `$`
        let err = c.check(&json!({})).unwrap_err();
        assert!(err.starts_with("$:") || err.contains("$:"), "{err}");
    }

    #[test]
    fn handler_records_last_valid_call_and_rejects_mismatches() {
        let schema = json!({
            "type": "object",
            "required": ["n"],
            "properties": { "n": { "type": "integer" } }
        });
        let compiled = Arc::new(compiled(schema.clone()));
        let slot = capture_slot();
        let tool = build_tool(compiled, slot.clone());
        assert_eq!(tool.meta.name, STRUCTURED_OUTPUT_TOOL_NAME);
        assert_eq!(tool.meta.parameters, schema);
        assert_eq!(tool.meta.label.as_deref(), Some("Structured Output"));

        // 第一次不匹配
        let err = (tool.handler)(json!({"n": "no"})).unwrap_err();
        assert!(
            err.0.contains("did not match the required schema"),
            "{}",
            err.0
        );
        {
            let c = slot.lock().unwrap();
            assert!(c.called);
            assert!(c.json.is_none());
            assert!(c.last_error.is_some());
            assert!(failure_message(&c).contains("did not match"));
        }

        // 之后两次有效：最后一次胜出
        (tool.handler)(json!({"n": 1})).unwrap();
        (tool.handler)(json!({"n": 2})).unwrap();
        let c = slot.lock().unwrap();
        assert_eq!(c.json, Some(json!({"n": 2})));
        assert!(c.last_error.is_none());
    }

    #[test]
    fn prepare_arguments_recovers_a_stringified_payload() {
        let schema = json!({
            "type": "object",
            "properties": { "n": { "type": "integer" } }
        });
        let tool = build_tool(Arc::new(compiled(schema)), capture_slot());
        let f = tool.meta.prepare_arguments.expect("prepare_arguments");
        assert_eq!(f(&json!("{\"n\": 1}")), json!({"n": 1}));
        // 不是 JSON 字符串就原样返回，交给校验去拒
        assert_eq!(f(&json!("not json")), json!("not json"));
        assert_eq!(f(&json!({"n": 1})), json!({"n": 1}));
    }

    #[test]
    fn check_text_parses_and_validates_replayed_text() {
        let schema = json!({
            "type": "object",
            "required": ["n"],
            "properties": { "n": { "type": "integer" } }
        });
        assert!(check_text(&schema, "{\"n\": 1}").is_ok());
        assert!(
            check_text(&schema, "{\"n\": \"x\"}").is_err(),
            "不符合 schema"
        );
        assert!(
            check_text(&schema, "not json at all")
                .unwrap_err()
                .contains("was not JSON"),
            "手改坏的条目要按'不是 JSON'报，而不是 panic"
        );
    }

    #[test]
    fn failure_message_distinguishes_never_called_from_mismatch() {
        let never = StructuredCapture::default();
        assert!(failure_message(&never).contains("did not report its answer"));
        let wrong = StructuredCapture {
            called: true,
            last_error: Some("$: bad".into()),
            ..Default::default()
        };
        assert!(failure_message(&wrong).contains("did not match the required schema: $: bad"));
    }
}

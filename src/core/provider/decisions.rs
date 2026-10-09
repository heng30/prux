//! OpenAI Decisions 分类协议：一次性（非流式）`POST <base_url>/decisions`。
//!
//! 请求体 `{ model, input, questions }`：
//! - `input` 是 `state` 的 JSON **文本**；带图片时改为一条 user 消息
//!   （`input_text` 的状态 + 每个图片一个 `input_image` 的 data URL）；
//! - 公开契约的问题映射到 Decisions 类型：`choice` → `choice`、`score` → `score`、
//!   `bool` → `predicate`。`predicate` 没有判据字段，因此把 `true` / `false`
//!   各自的含义追加到 `instructions` 后面。
//!
//! 只有 API key 能用这条路由：Sign in with ChatGPT 的 token 会被拒
//! （见 [`crate::core::model_resolver::list_models_of_type`] 的可见性过滤）。
//!
//! 失败永不抛错：由 [`super::classify`] 把错误转成 `stop_reason: "error"` 的结果。

use super::{
    ClassifierAnswer, ClassifierContext, ClassifierQuestion, ClassifierResult, ImageContent,
    MAX_TIMEOUT_MS, ModelConfig,
    classifier::{error_result, parse_usage},
    convert::truncate_for_error,
    provider_extra_headers,
    retry::{DEFAULT_MAX_RETRIES, send_with_retry_ex},
};
use crate::{
    error::{Error, Result},
    utils::{http, time::now_ms},
};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, time::Duration};

/// 错误文案里的服务名
const LABEL: &str = "OpenAI Decisions";

/// 单次请求允许携带的图片数上限（服务端限制）。
const MAX_IMAGES: usize = 128;

/// 网关 504 不重试：Cloudflare 挡在 api.openai.com 前面，超长输入（约 60 万 token 以上）
/// 会撞上它的时间上限，重试同样的输入只会再撞一次。
const NO_RETRY_STATUSES: &[u16] = &[504];

/// 504 的专属文案：直接说明这是网关超时且与输入过大相关，避免用户反复重试。
const GATEWAY_TIMEOUT_MESSAGE: &str = "OpenAI Decisions error (504): the request timed out at the gateway. Very large inputs (above roughly 600K tokens) currently exceed its time limit.";

/// `openai-decisions` 协议实现：一次非流式 POST，永不抛错。
///
/// **用量在解析答案之前就落进结果**——答案格式不对的请求同样已经计费，
/// 因此那种失败返回的是带 `usage` 的 error 结果，而不是把用量丢掉。
pub(crate) async fn classify_openai_decisions(
    model: &ModelConfig,
    context: &ClassifierContext,
) -> ClassifierResult {
    if model.api_key.is_empty() {
        return error_result(
            model,
            format!("Provider is not configured: {}", model.provider),
        );
    }

    let input = match wire_input(context) {
        Ok(input) => input,
        Err(err) => return error_result(model, err.to_string()),
    };

    let body = json!({
        "model": model.model_id,
        "input": input,
        "questions": wire_questions(context),
    })
    .to_string();

    let client = match http::build_client() {
        Ok(client) => client,
        Err(err) => return error_result(model, err.to_string()),
    };

    let response = match send_with_retry_ex(
        || {
            client
                .post(endpoint(model))
                .header("Content-Type", "application/json")
                .bearer_auth(&model.api_key)
                .headers(provider_extra_headers(model))
                .timeout(Duration::from_millis(MAX_TIMEOUT_MS))
                .body(body.clone())
        },
        DEFAULT_MAX_RETRIES,
        model.max_retry_delay_ms,
        NO_RETRY_STATUSES,
    )
    .await
    {
        Ok(response) => response,
        Err(err) => return error_result(model, err.to_string()),
    };

    let status = response.status();
    if !status.is_success() {
        if status.as_u16() == 504 {
            return error_result(model, GATEWAY_TIMEOUT_MESSAGE);
        }
        let text = response.text().await.unwrap_or_default();
        return error_result(
            model,
            Error::ProviderStatus {
                status,
                body: truncate_for_error(&text),
            }
            .to_string(),
        );
    }

    let payload: Value = match response.json().await {
        Ok(payload) => payload,
        Err(err) => return error_result(model, err.to_string()),
    };

    let mut result = ClassifierResult {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.model_id.clone(),
        answers: BTreeMap::new(),
        usage: parse_usage(payload.get("usage"), model),
        stop_reason: "stop".to_string(),
        error_message: None,
        timestamp: now_ms(),
    };

    match parse_answers(context, payload.get("answers")) {
        Ok(answers) => result.answers = answers,
        Err(err) => {
            result.stop_reason = "error".to_string();
            result.error_message = Some(err.to_string());
        }
    }
    result
}

/// Decisions 接口地址：`base_url` 去尾斜杠后追加 `/decisions`。
fn endpoint(model: &ModelConfig) -> String {
    format!("{}/decisions", model.base_url.trim_end_matches('/'))
}

/// 组装请求体的 `input`：无图时是 `state` 的 JSON 文本；有图时是一条 user 消息。
///
/// 图片超过 [`MAX_IMAGES`] 或出现非图片内容（`ClassifierContext::images` 与图片生成
/// 共用 [`ImageContent`]）时报错，由调用方转成错误结果。
fn wire_input(context: &ClassifierContext) -> Result<Value> {
    let state = serde_json::to_string(&context.state)
        .map_err(|err| Error::msg(format!("failed to serialize classifier state: {err}")))?;
    let images = context.images.as_deref().unwrap_or_default();
    if images.is_empty() {
        return Ok(Value::String(state));
    }
    if images.len() > MAX_IMAGES {
        return Err(Error::msg(format!(
            "{LABEL} accepts at most {MAX_IMAGES} images, got {}",
            images.len()
        )));
    }

    let mut content = vec![json!({ "type": "input_text", "text": state })];
    for image in images {
        let ImageContent::Image { data, mime_type } = image else {
            return Err(Error::msg(format!(
                "{LABEL} accepts only image content, got a text item"
            )));
        };
        content.push(json!({
            "type": "input_image",
            "image_url": format!("data:{mime_type};base64,{data}"),
        }));
    }

    Ok(json!([{ "role": "user", "content": content }]))
}

/// 按问题 id 字典序展开所有问题（答案按 `name` 对号入座，顺序不影响解析）。
fn wire_questions(context: &ClassifierContext) -> Vec<Value> {
    context
        .questions
        .iter()
        .map(|(id, question)| wire_question(id, question))
        .collect()
}

/// 单个问题的线上表示：选择题带 `choices`、评分题带 `levels`、判断题转 `predicate`。
fn wire_question(name: &str, question: &ClassifierQuestion) -> Value {
    match question {
        ClassifierQuestion::Choice {
            instructions,
            criteria,
        } => {
            let choices: Vec<Value> = criteria
                .iter()
                .map(|(value, description)| {
                    if description.is_empty() {
                        json!({ "value": value })
                    } else {
                        json!({ "value": value, "description": description })
                    }
                })
                .collect();
            json!({
                "type": "choice",
                "name": name,
                "instructions": instructions,
                "choices": choices,
            })
        }
        ClassifierQuestion::Score {
            instructions,
            criteria,
        } => {
            let levels: Vec<Value> = criteria
                .iter()
                .map(|label| json!({ "label": label }))
                .collect();
            json!({
                "type": "score",
                "name": name,
                "instructions": instructions,
                "levels": levels,
            })
        }
        ClassifierQuestion::Bool {
            instructions,
            criteria,
        } => json!({
            "type": "predicate",
            "name": name,
            "instructions": predicate_instructions(instructions, criteria),
        }),
    }
}

/// 判断题的题面：`predicate` 没有判据字段，把两侧含义并入 `instructions`。
///
/// 两侧判据都为空时原样返回 `instructions`（不追加空行）。
fn predicate_instructions(instructions: &str, criteria: &super::BoolCriteria) -> String {
    let mut meanings = Vec::new();
    if !criteria.yes.is_empty() {
        meanings.push(format!("True means: {}", criteria.yes));
    }
    if !criteria.no.is_empty() {
        meanings.push(format!("False means: {}", criteria.no));
    }
    if meanings.is_empty() {
        instructions.to_string()
    } else {
        format!("{instructions}\n\n{}", meanings.join("\n"))
    }
}

/// 按请求里的问题逐个解析答案：`answers` 是数组（每项带 `name`），按名字对号入座。
///
/// 缺答案、类型对不上、字段不合法都算失败；响应里多出来的问答条目忽略。
fn parse_answers(
    context: &ClassifierContext,
    value: Option<&Value>,
) -> Result<BTreeMap<String, ClassifierAnswer>> {
    let Some(answers) = value.and_then(Value::as_array) else {
        return Err(Error::msg(format!(
            "{LABEL} returned an unexpected response"
        )));
    };

    let mut by_name: Map<String, Value> = Map::new();
    for answer in answers {
        if let Some(name) = answer.get("name").and_then(Value::as_str) {
            by_name.insert(name.to_string(), answer.clone());
        }
    }

    let mut parsed = BTreeMap::new();
    for (id, question) in &context.questions {
        let answer = by_name
            .get(id)
            .ok_or_else(|| Error::msg(format!("{LABEL} did not return an answer for {id}")))?;
        parsed.insert(id.clone(), parse_answer(id, question, answer)?);
    }
    Ok(parsed)
}

/// 解析单道题的答案：`refusal` 视为失败，其余按问题类型核对答案类型。
fn parse_answer(
    id: &str,
    question: &ClassifierQuestion,
    answer: &Value,
) -> Result<ClassifierAnswer> {
    let answer_type = answer.get("type").and_then(Value::as_str);
    if answer_type == Some("refusal") {
        return Err(Error::msg(format!("{LABEL} refused to answer {id}")));
    }

    match question {
        ClassifierQuestion::Choice { .. } => {
            if answer_type != Some("choice") {
                return Err(Error::msg(format!(
                    "{LABEL} did not return a choice answer for {id}"
                )));
            }
            Ok(ClassifierAnswer::Choice {
                choice: required_string(answer, "choice", &format!("choice for {id}"))?,
                probabilities: choice_probabilities(answer.get("probabilities"), id)?,
                confidence: required_number(answer, "confidence", &format!("confidence for {id}"))?,
            })
        }
        ClassifierQuestion::Score { .. } => {
            if answer_type != Some("score") {
                return Err(Error::msg(format!(
                    "{LABEL} did not return a score answer for {id}"
                )));
            }
            Ok(ClassifierAnswer::Score {
                score: required_number(answer, "score", &format!("score for {id}"))?,
                confidence: required_number(answer, "confidence", &format!("confidence for {id}"))?,
            })
        }
        ClassifierQuestion::Bool { .. } => {
            if answer_type != Some("predicate") {
                return Err(Error::msg(format!(
                    "{LABEL} did not return a predicate answer for {id}"
                )));
            }
            Ok(ClassifierAnswer::Bool {
                probability: required_number(
                    answer,
                    "probability",
                    &format!("probability for {id}"),
                )?,
            })
        }
    }
}

/// 解析选择题的 `probabilities`：线上是 `[{ value, probability }]` 数组。
fn choice_probabilities(value: Option<&Value>, id: &str) -> Result<BTreeMap<String, f64>> {
    let Some(entries) = value.and_then(Value::as_array) else {
        return Err(Error::msg(format!(
            "{LABEL} returned invalid probabilities for {id}"
        )));
    };

    let mut parsed = BTreeMap::new();
    for entry in entries {
        let Some(option) = entry.get("value").and_then(Value::as_str) else {
            return Err(Error::msg(format!(
                "{LABEL} returned invalid probabilities for {id}"
            )));
        };
        let probability = required_number(
            entry,
            "probability",
            &format!("probability for {id}.{option}"),
        )?;
        parsed.insert(option.to_string(), probability);
    }
    Ok(parsed)
}

/// 取答案对象里的必填数字字段；缺失或不是有限数字时报错（`field` 为文案里的字段描述）。
fn required_number(answer: &Value, key: &str, field: &str) -> Result<f64> {
    answer
        .get(key)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite())
        .ok_or_else(|| Error::msg(format!("{LABEL} returned an invalid {field}")))
}

/// 取答案对象里的必填字符串字段；缺失或不是字符串时报错。
fn required_string(answer: &Value, key: &str, field: &str) -> Result<String> {
    answer
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| Error::msg(format!("{LABEL} returned an invalid {field}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{model_resolver, provider::ModelType};
    use serde_json::json;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    /// 网络测试统一护栏：90s 未完成即 panic（挂起 → 可诊断失败）。
    fn run_net_test(f: impl FnOnce() + Send + 'static) {
        crate::test_support::run_with_timeout(std::time::Duration::from_secs(90), f)
    }

    /// 读一个完整 HTTP 请求（含 `Content-Length` body），返回原始请求文本。
    async fn read_request(socket: &mut tokio::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let mut header_end = None;
        loop {
            let n = socket.read(&mut tmp).await.unwrap();
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if header_end.is_none() {
                header_end = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4);
            }
            if let Some(he) = header_end {
                let head = String::from_utf8_lossy(&buf[..he]).to_lowercase();
                let len: usize = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if buf.len() >= he + len {
                    break;
                }
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    /// 启动假 Decisions 服务器：每个请求回同一段 JSON。
    ///
    /// 收到首个请求后仍多等 1s 才退出，用于观察是否发生了重试
    /// （重试退避 0.5s 起、带 ±25% 抖动，1s 足以覆盖第一次重试）。
    /// 返回 base_url、请求计数与服务器任务（任务值是所有收到的请求文本）。
    async fn mock_json_server(
        status: u16,
        body: &'static str,
    ) -> (
        String,
        Arc<AtomicUsize>,
        tokio::task::JoinHandle<Vec<String>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let counter = count.clone();
        let handle = tokio::spawn(async move {
            let mut requests = Vec::new();
            let mut idle_window = std::time::Duration::from_secs(2);
            while let Ok(Ok((mut socket, _))) =
                tokio::time::timeout(idle_window, listener.accept()).await
            {
                counter.fetch_add(1, Ordering::SeqCst);
                requests.push(read_request(&mut socket).await);
                let reason = if status == 200 {
                    "OK"
                } else {
                    "Gateway Timeout"
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
                socket.shutdown().await.unwrap();
                idle_window = std::time::Duration::from_secs(1);
            }
            requests
        });
        (format!("http://127.0.0.1:{}", addr.port()), count, handle)
    }

    /// 用真实目录条目构造分类器模型配置（`openai/gpt-6-luna`，api=openai-decisions），
    /// 并把 base_url 指向本地假服务器。
    fn classifier_model(base_url: &str) -> ModelConfig {
        let entry =
            model_resolver::find_model_of_type("openai", "gpt-6-luna", ModelType::Classifier)
                .expect("目录里应有 openai 的 gpt-6-luna 分类器条目");
        let mut model =
            model_resolver::model_config_from_entry(&entry, Some("sk-test".into()), None);
        model.base_url = base_url.to_string();
        model
    }

    /// 三类问题各一道：选择题（含空描述选项）、评分题、判断题。
    fn context() -> ClassifierContext {
        ClassifierContext {
            state: json!({ "text": "The deployment succeeded, thank you." }),
            images: None,
            questions: BTreeMap::from([
                (
                    "approved".to_string(),
                    ClassifierQuestion::Bool {
                        instructions: "Does the user approve?".into(),
                        criteria: super::super::BoolCriteria {
                            yes: "Approval".into(),
                            no: "No approval".into(),
                        },
                    },
                ),
                (
                    "category".to_string(),
                    ClassifierQuestion::Choice {
                        instructions: "Classify the message".into(),
                        criteria: BTreeMap::from([
                            ("success".to_string(), "Successful".to_string()),
                            ("failure".to_string(), String::new()),
                        ]),
                    },
                ),
                (
                    "satisfaction".to_string(),
                    ClassifierQuestion::Score {
                        instructions: "Score satisfaction".into(),
                        criteria: vec!["low".into(), "neutral".into(), "high".into()],
                    },
                ),
            ]),
        }
    }

    /// 线上答案的形状（按 id 乱序返回）。
    const ANSWER_BODY: &str = r#"{
        "model": "gpt-6-luna",
        "answers": [
            {"type": "predicate", "name": "approved", "probability": 0.95},
            {"type": "choice", "name": "category", "choice": "success",
             "probabilities": [{"value": "success", "probability": 0.9}, {"value": "failure", "probability": 0.1}],
             "confidence": 0.8},
            {"type": "score", "name": "satisfaction", "score": 1.8, "confidence": 0.7}
        ],
        "usage": {"input_tokens": 164, "output_tokens": 0, "total_tokens": 164}
    }"#;

    #[test]
    fn maps_questions_to_decisions_types_and_parses_answers() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let _ad = crate::test_support::AgentDirGuard::temp();
                let (base_url, count, server) = mock_json_server(200, ANSWER_BODY).await;
                let model = classifier_model(&base_url);

                let result = classify_openai_decisions(&model, &context()).await;

                assert_eq!(result.stop_reason, "stop", "{:?}", result.error_message);
                assert_eq!(result.answers.len(), 3);
                match &result.answers["approved"] {
                    ClassifierAnswer::Bool { probability } => assert_eq!(*probability, 0.95),
                    other => panic!("expected bool answer, got {other:?}"),
                }
                match &result.answers["category"] {
                    ClassifierAnswer::Choice {
                        choice,
                        probabilities,
                        confidence,
                    } => {
                        assert_eq!(choice, "success");
                        assert_eq!(probabilities["success"], 0.9);
                        assert_eq!(*confidence, 0.8);
                    }
                    other => panic!("expected choice answer, got {other:?}"),
                }
                match &result.answers["satisfaction"] {
                    ClassifierAnswer::Score { score, confidence } => {
                        assert_eq!(*score, 1.8);
                        assert_eq!(*confidence, 0.7);
                    }
                    other => panic!("expected score answer, got {other:?}"),
                }
                assert_eq!(result.usage.as_ref().unwrap().input, 164);

                let requests = server.await.unwrap();
                assert_eq!(count.load(Ordering::SeqCst), 1);
                let request = &requests[0];
                assert!(request.starts_with("POST /decisions "), "{request}");
                assert!(
                    request.to_lowercase().contains("authorization: bearer sk-test"),
                    "{request}"
                );

                let body: Value = serde_json::from_str(
                    request.split("\r\n\r\n").nth(1).expect("request has a body"),
                )
                .unwrap();
                assert_eq!(body["model"], "gpt-6-luna");
                // 无图片时 input 是 state 的 JSON 文本，不是消息数组
                assert_eq!(body["input"], json!(r#"{"text":"The deployment succeeded, thank you."}"#));
                // 问题按 id 字典序下发（answers 按 name 对号入座，顺序不影响解析）
                assert_eq!(
                    body["questions"],
                    json!([
                        {
                            "type": "predicate",
                            "name": "approved",
                            "instructions": "Does the user approve?\n\nTrue means: Approval\nFalse means: No approval",
                        },
                        {
                            "type": "choice",
                            "name": "category",
                            "instructions": "Classify the message",
                            "choices": [{"value": "failure"}, {"value": "success", "description": "Successful"}],
                        },
                        {
                            "type": "score",
                            "name": "satisfaction",
                            "instructions": "Score satisfaction",
                            "levels": [{"label": "low"}, {"label": "neutral"}, {"label": "high"}],
                        }
                    ])
                );
            });
        });
    }

    #[test]
    fn images_become_one_user_message_with_data_urls() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let _ad = crate::test_support::AgentDirGuard::temp();
                let (base_url, _, server) = mock_json_server(200, ANSWER_BODY).await;
                let model = classifier_model(&base_url);
                let mut ctx = context();
                ctx.images = Some(vec![ImageContent::Image {
                    data: "aW1hZ2U=".into(),
                    mime_type: "image/png".into(),
                }]);

                let result = classify_openai_decisions(&model, &ctx).await;
                assert_eq!(result.stop_reason, "stop", "{:?}", result.error_message);

                let requests = server.await.unwrap();
                let body: Value = serde_json::from_str(
                    requests[0].split("\r\n\r\n").nth(1).expect("request has a body"),
                )
                .unwrap();
                assert_eq!(
                    body["input"],
                    json!([{
                        "role": "user",
                        "content": [
                            {"type": "input_text", "text": "{\"text\":\"The deployment succeeded, thank you.\"}"},
                            {"type": "input_image", "image_url": "data:image/png;base64,aW1hZ2U="}
                        ]
                    }])
                );
            });
        });
    }

    #[test]
    fn gateway_504_fails_without_retry() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let _ad = crate::test_support::AgentDirGuard::temp();
                let (base_url, count, server) = mock_json_server(504, "{}").await;
                let model = classifier_model(&base_url);

                let result = classify_openai_decisions(&model, &context()).await;
                assert_eq!(result.stop_reason, "error");
                assert_eq!(
                    result.error_message.as_deref(),
                    Some(GATEWAY_TIMEOUT_MESSAGE)
                );

                // 等服务器退出后计数才稳定：重试会在 ~0.5s 后到达，窗口内没有第二次请求
                let _ = server.await.unwrap();
                assert_eq!(count.load(Ordering::SeqCst), 1);
            });
        });
    }
}

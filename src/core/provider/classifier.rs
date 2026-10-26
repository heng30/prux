//! 分类器调用：一次性（非流式）请求。
//!
//! 目前唯一内置的协议实现是 `typesafe-system-one`（TypeSafe 的 System One 协议，
//! OpenRouter / OpenCode Zen / Vercel AI Gateway 共用同一套协议，只是 `base_url` 不同），
//! 请求发到 `<base_url>/systemone`。
//!
//! 公开契约：问题分 `choice` / `score` / `bool` 三类，
//! `bool` 在线上以 TypeSafe 的 `noul` 表示（问题 `type` 与答案 `type` 都要转）。
//! 失败永不抛错：由 [`super::classify`] 把错误转成 `stop_reason: "error"` 的结果。

use super::{
    ImageContent, MAX_TIMEOUT_MS, ModelConfig, Usage,
    convert::truncate_for_error,
    provider_extra_headers,
    retry::{DEFAULT_MAX_RETRIES, send_with_retry},
    usage::compute_cost,
};
use crate::{
    error::{Error, Result},
    utils::{http, time::now_ms},
};
use reqwest::header::{HeaderName, HeaderValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, time::Duration};

/// 错误文案里的服务名
const LABEL: &str = "System One API";

/// 分类器图片形态：TypeSafe 的 `assets` 映射（默认）。
const IMAGE_INPUT_ASSETS: &str = "assets";
/// 分类器图片形态：Clef 的 `images` data URL 列表。
const IMAGE_INPUT_IMAGES: &str = "images";
/// 分类器图片形态：OpenRouter 的 `state` 内 `image_url` 片段。
const IMAGE_INPUT_STATE_IMAGE_URLS: &str = "state-image-urls";
/// Clef 单次请求接受的图片张数上限。
const MAX_DATA_URL_IMAGES: usize = 4;

/// 判断题两侧的判据文案（线上原样下发，不做转换）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoolCriteria {
    /// 判为真时应匹配的情形说明。
    #[serde(rename = "true")]
    pub yes: String,
    /// 判为假时应匹配的情形说明。
    #[serde(rename = "false")]
    pub no: String,
}

/// 一道分类问题：类型决定答案的形状与线上表示。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClassifierQuestion {
    /// 选择题：`criteria` 为 选项 id → 选项说明，答案从其中选一个。
    Choice {
        /// 交给分类器的题面指令（自然语言）。
        instructions: String,
        /// 候选选项：选项 id → 该选项的含义说明。
        criteria: BTreeMap<String, String>,
    },
    /// 评分题：`criteria` 给出刻度文案，答案落在该刻度上。
    Score {
        /// 交给分类器的题面指令（自然语言）。
        instructions: String,
        /// 刻度文案，从低到高排列（如 `["dissatisfied","neutral","satisfied"]`）。
        criteria: Vec<String>,
    },
    /// 判断题：线上以 TypeSafe 的 `noul` 类型下发，答案回来也是 `noul`。
    Bool {
        /// 交给分类器的题面指令（自然语言）。
        instructions: String,
        /// 两侧判据；线上原样下发。
        criteria: BoolCriteria,
    },
}

/// 一次分类请求的输入：待分类状态 + 若干带 id 的问题。
///
/// 答案按同一批问题 id 返回；`state` 通常是 `{"<字段名>": <值>}` 的 JSON 对象，
/// 原样下发给分类服务，不做解释。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClassifierContext {
    /// 待分类的状态对象（如 `{"message": "..."}`）。
    pub state: Value,
    /// 与 `state` 一起判定的一组图片（每项是 `{"type":"image","data":…,"mimeType":…}`）。
    ///
    /// 只有目录 `input` 含 `"image"` 的模型接受它们；其它模型在 [`super::classify`] 入口就返回错误结果。
    /// 图片按模型的 `compat.imageInput` 拼接（`assets` / `images` / `state-image-urls`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<ImageContent>>,
    /// 问题 id → 问题；id 由调用方自取，用于在答案里对号入座。
    pub questions: BTreeMap<String, ClassifierQuestion>,
}

/// 一道问题的答案。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClassifierAnswer {
    /// 选择题答案：选中的选项 id、各选项概率与本题置信度。
    Choice {
        /// 选中的选项 id（`criteria` 的键之一；服务端不保证出现在 `probabilities` 里）。
        choice: String,
        /// 各选项 id → 概率（0..1）。
        probabilities: BTreeMap<String, f64>,
        /// 本题置信度（0..1）。
        confidence: f64,
    },
    /// 评分题答案：分数与本题置信度。
    Score {
        /// 分数，刻度语义见问题的 `criteria`（不一定是整数）。
        score: f64,
        /// 本题置信度（0..1）。
        confidence: f64,
    },
    /// 判断题答案：判为真的概率（0..1）；线上字段名为 `noul`。
    Bool {
        /// 判为真的概率（0..1）。
        probability: f64,
    },
}

/// 一次分类调用的结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClassifierResult {
    /// 实际使用的协议实现（目前只有 typesafe-system-one）。
    pub api: String,
    /// 提供方标识（如 openrouter）。
    pub provider: String,
    /// 请求的模型 id。
    pub model: String,
    /// 问题 id → 答案；失败时为空（`error_message` 说明原因）。
    pub answers: BTreeMap<String, ClassifierAnswer>,
    /// token 用量与按目录单价算出的费用；服务未报告 token 数时为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
    /// 停止原因：`stop`（正常）/ `error`（失败）。
    pub stop_reason: String,
    /// 失败描述；正常结束为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// 结果生成时间（Unix 毫秒）。
    pub timestamp: u64,
}

/// 构造失败结果：`stop_reason = "error"` 且无答案，供入口层统一返回（不抛错）。
pub(crate) fn error_result(model: &ModelConfig, message: impl Into<String>) -> ClassifierResult {
    ClassifierResult {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.model_id.clone(),
        answers: BTreeMap::new(),
        usage: None,
        stop_reason: "error".to_string(),
        error_message: Some(message.into()),
        timestamp: now_ms(),
    }
}

/// 组装 `typesafe-system-one` 请求的 URL：`base_url` 去尾斜杠后补 `/systemone`。
fn system_one_url(model: &ModelConfig) -> String {
    format!("{}/systemone", model.base_url.trim_end_matches('/'))
}

/// `typesafe-system-one` 协议实现：一次非流式 POST，永不抛错。
///
/// **用量在解析答案之前就落进结果**——答案格式不对的请求同样已经计费，
/// 因此那种失败返回的是带 `usage` 的 error 结果，而不是把用量丢掉。
pub(crate) async fn classify_typesafe_system_one(
    model: &ModelConfig,
    context: &ClassifierContext,
) -> ClassifierResult {
    if model.api_key.is_empty() {
        return error_result(
            model,
            format!("Provider is not configured: {}", model.provider),
        );
    }

    let mut body = build_body(model, context);
    let images = context
        .images
        .as_deref()
        .filter(|images| !images.is_empty());
    let opt_in_header = images.is_some() && sends_image_opt_in_header(model);
    if let Some(images) = images
        && let Err(err) = add_images(model, images, &mut body)
    {
        return error_result(model, err.to_string());
    }
    let body = body.to_string();
    let url = system_one_url(model);
    let client = match http::build_client() {
        Ok(client) => client,
        Err(err) => return error_result(model, err.to_string()),
    };

    let response = match send_with_retry(
        || {
            let mut headers = provider_extra_headers(model);
            if opt_in_header {
                headers.insert(
                    HeaderName::from_static("typesafe-danger-zone-images"),
                    HeaderValue::from_static("true"),
                );
            }
            client
                .post(&url)
                .header("Content-Type", "application/json")
                .bearer_auth(&model.api_key)
                .headers(headers)
                .timeout(Duration::from_millis(MAX_TIMEOUT_MS))
                .body(body.clone())
        },
        DEFAULT_MAX_RETRIES,
        model.max_retry_delay_ms,
    )
    .await
    {
        Ok(response) => response,
        Err(err) => return error_result(model, err.to_string()),
    };

    let status = response.status();
    if !status.is_success() {
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

/// 组装请求体：`{ model, state, questions }`（`bool` 问题转成线上的 `noul`）。
fn build_body(model: &ModelConfig, context: &ClassifierContext) -> Value {
    let questions: Map<String, Value> = context
        .questions
        .iter()
        .map(|(id, question)| (id.clone(), wire_question(question)))
        .collect();

    json!({
        "model": model.model_id,
        "state": context.state,
        "questions": questions,
    })
}

/// 图片的线上形态（`compat.imageInput`）：`assets` / `images` / `state-image-urls`。
///
/// - `assets`：TypeSafe 的 `assets` 映射，文件名按 `<image-N>` 放进 `state` 之前，仅 PNG / JPEG；
/// - `images`：Clef 的 data URL 列表，最多 4 张，PNG / JPEG / WebP；
/// - `state-image-urls`：OpenRouter 的 `image_url` 片段放在 `state` 之前，任意 MIME 交给模型判。
fn image_input_mode(model: &ModelConfig) -> &str {
    model
        .image_input
        .as_deref()
        .unwrap_or(if is_openrouter(model) {
            IMAGE_INPUT_STATE_IMAGE_URLS
        } else {
            IMAGE_INPUT_ASSETS
        })
}

/// provider 是否 OpenRouter（按 provider id 或 baseUrl，后者覆盖区域端点与自建代理）。
fn is_openrouter(model: &ModelConfig) -> bool {
    model.provider == "openrouter" || model.base_url.contains("openrouter.ai")
}

/// 带图请求是否发 TypeSafe 的实验性 opt-in 头；未声明时只有非 OpenRouter 的服务要发。
fn sends_image_opt_in_header(model: &ModelConfig) -> bool {
    model
        .send_image_opt_in_header
        .unwrap_or(!is_openrouter(model))
}

/// 把 `images` 里实际图片部分按该服务的形态拼进请求体 `body`。
///
/// 不支持的 MIME 或超过数量上阳时报错（由调用方转成 error 结果，不发请求）。
fn add_images(model: &ModelConfig, images: &[ImageContent], body: &mut Value) -> Result<()> {
    let images: Vec<(&str, &str)> = images
        .iter()
        .filter_map(|image| match image {
            ImageContent::Image { data, mime_type } => Some((data.as_str(), mime_type.as_str())),
            ImageContent::Text { .. } => None,
        })
        .collect();
    if images.is_empty() {
        return Ok(());
    }

    match image_input_mode(model) {
        IMAGE_INPUT_IMAGES => {
            if images.len() > MAX_DATA_URL_IMAGES {
                return Err(Error::msg(format!(
                    "{LABEL} accepts at most {MAX_DATA_URL_IMAGES} images for {}, got {}",
                    model.model_id,
                    images.len()
                )));
            }
            let urls: Vec<String> = images
                .iter()
                .map(|(data, mime)| match *mime {
                    "image/png" | "image/jpeg" | "image/webp" => {
                        Ok(format!("data:{mime};base64,{data}"))
                    }
                    other => Err(Error::msg(format!(
                        "{LABEL} accepts PNG, JPEG, and WebP images for {}, got {other}",
                        model.model_id
                    ))),
                })
                .collect::<Result<_>>()?;
            body["images"] = json!(urls);
        }
        IMAGE_INPUT_STATE_IMAGE_URLS => {
            let parts: Vec<Value> = images
                .iter()
                .map(|(data, mime)| {
                    json!({"type": "image_url", "image_url": {"url": format!("data:{mime};base64,{data}")}})
                })
                .collect();
            prepend_state(body, parts);
        }
        _ => {
            let mut assets = Map::new();
            for (index, (data, mime)) in images.iter().enumerate() {
                let format = match *mime {
                    "image/png" => "png",
                    "image/jpeg" => "jpeg",
                    other => {
                        return Err(Error::msg(format!(
                            "{LABEL} accepts PNG and JPEG images for {}, got {other}",
                            model.model_id
                        )));
                    }
                };
                assets.insert(
                    format!("<image-{}>", index + 1),
                    json!({"type": "image", "format": format, "data_base64": data}),
                );
            }
            let names: Vec<Value> = assets.keys().map(|name| json!(name)).collect();
            body["assets"] = Value::Object(assets);
            prepend_state(body, names);
        }
    }
    Ok(())
}

/// 把 `prefix` 插到请求体 `state` 之前（`state` 变数组，原内容跟在后面）。
///
/// 每个 asset 都必须被 `state` 引用才能生效，因此名字列表也放在这里。
fn prepend_state(body: &mut Value, prefix: Vec<Value>) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let state = obj.remove("state").unwrap_or(Value::Null);
    let rest = match state {
        Value::Array(items) => items,
        other => vec![other],
    };
    obj.insert(
        "state".to_string(),
        Value::Array(prefix.into_iter().chain(rest).collect()),
    );
}

/// 单个问题的线上表示：公开契约的 `bool` 在这里变成 TypeSafe 的 `noul`。
fn wire_question(question: &ClassifierQuestion) -> Value {
    match question {
        ClassifierQuestion::Choice {
            instructions,
            criteria,
        } => json!({
            "type": "choice",
            "instructions": instructions,
            "criteria": criteria,
        }),
        ClassifierQuestion::Score {
            instructions,
            criteria,
        } => json!({
            "type": "score",
            "instructions": instructions,
            "criteria": criteria,
        }),
        ClassifierQuestion::Bool {
            instructions,
            criteria,
        } => json!({
            "type": "noul",
            "instructions": instructions,
            "criteria": criteria,
        }),
    }
}

/// 按请求里的问题逐个解析答案：缺答案、类型对不上或字段不是数字都算失败。
///
/// 只认请求中列出的问题 id；响应里多出来的键忽略
fn parse_answers(
    context: &ClassifierContext,
    value: Option<&Value>,
) -> Result<BTreeMap<String, ClassifierAnswer>> {
    let Some(answers) = value.and_then(Value::as_object) else {
        return Err(Error::msg(format!(
            "{LABEL} returned an unexpected response"
        )));
    };

    let mut parsed = BTreeMap::new();
    for (id, question) in &context.questions {
        let answer = answers
            .get(id)
            .ok_or_else(|| Error::msg(format!("{LABEL} did not return an answer for {id}")))?;
        let answer = match question {
            ClassifierQuestion::Choice { .. } => {
                if answer.get("type").and_then(Value::as_str) != Some("choice") {
                    return Err(Error::msg(format!(
                        "{LABEL} did not return a choice answer for {id}"
                    )));
                }
                ClassifierAnswer::Choice {
                    choice: required_string(answer, "choice", &format!("choice for {id}"))?,
                    probabilities: probabilities(answer.get("probabilities"), id)?,
                    confidence: required_number(
                        answer,
                        "confidence",
                        &format!("confidence for {id}"),
                    )?,
                }
            }
            ClassifierQuestion::Score { .. } => {
                if answer.get("type").and_then(Value::as_str) != Some("score") {
                    return Err(Error::msg(format!(
                        "{LABEL} did not return a score answer for {id}"
                    )));
                }
                ClassifierAnswer::Score {
                    score: required_number(answer, "score", &format!("score for {id}"))?,
                    confidence: required_number(
                        answer,
                        "confidence",
                        &format!("confidence for {id}"),
                    )?,
                }
            }
            ClassifierQuestion::Bool { .. } => {
                if answer.get("type").and_then(Value::as_str) != Some("noul") {
                    return Err(Error::msg(format!(
                        "{LABEL} did not return a bool answer for {id}"
                    )));
                }
                ClassifierAnswer::Bool {
                    probability: required_number(answer, "noul", &format!("probability for {id}"))?,
                }
            }
        };
        parsed.insert(id.clone(), answer);
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

/// 解析选择题的 `probabilities`：必须是 选项 id → 数字 的对象。
fn probabilities(value: Option<&Value>, id: &str) -> Result<BTreeMap<String, f64>> {
    let Some(raw) = value.and_then(Value::as_object) else {
        return Err(Error::msg(format!(
            "{LABEL} returned invalid probabilities for {id}"
        )));
    };

    let mut parsed = BTreeMap::new();
    for (key, probability) in raw {
        let number = probability
            .as_f64()
            .filter(|n| n.is_finite())
            .ok_or_else(|| {
                Error::msg(format!(
                    "{LABEL} returned an invalid probability for {id}.{key}"
                ))
            })?;
        parsed.insert(key.clone(), number);
    }
    Ok(parsed)
}

/// 解析响应用的 token 用量：`{ input_tokens, output_tokens }`，按目录单价计费。
///
/// 两个字段都没有（或 `usage` 不是对象）时返回 None；字段不是正数时按 0 计。
/// 即“报告了但值不合法”仍算有用量，只是那部分 token 记 0。
/// 分类器协议（System One / OpenAI Decisions）共用这套口径。
pub(crate) fn parse_usage(value: Option<&Value>, model: &ModelConfig) -> Option<Usage> {
    let raw = value.and_then(Value::as_object)?;
    if raw.get("input_tokens").is_none() && raw.get("output_tokens").is_none() {
        return None;
    }

    let input = token_count(raw.get("input_tokens"));
    let output = token_count(raw.get("output_tokens"));
    let mut usage = Usage {
        input,
        output,
        cache_read: 0,
        cache_write: 0,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output,
        cost: Default::default(),
    };
    compute_cost(&mut usage, model.cost.as_ref());
    Some(usage)
}

/// token 计数：非数字、非有限或非正数一律按 0。
pub(crate) fn token_count(value: Option<&Value>) -> u32 {
    value
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n > 0.0)
        .map(|n| n as u32)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{model_resolver, provider::ModelType};
    use serde_json::json;
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
    };

    /// 网络测试统一护栏：90s 未完成即 panic（挂起 → 可诊断失败）。
    fn run_net_test(f: impl FnOnce() + Send + 'static) {
        crate::test_support::run_with_timeout(std::time::Duration::from_secs(90), f)
    }

    /// 启动假 HTTP 服务器：读完整请求（含 `Content-Length` body）后回一段固定 JSON。
    /// 返回 base_url 与服务器 task；task 的返回值是**收到的原始请求文本**（含请求行，可断言路径）。
    async fn mock_json_server(
        status: u16,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
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

            let request = String::from_utf8_lossy(&buf).to_string();
            let reason = if status == 200 { "OK" } else { "Bad Request" };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
            request
        });
        (format!("http://127.0.0.1:{}", addr.port()), handle)
    }

    /// 用真实目录条目构造分类器模型配置（`~typesafe/jev-latest`，api=typesafe-system-one），
    /// 并把 base_url 指向本地假服务器。
    fn classifier_model(base_url: &str) -> ModelConfig {
        let entry = model_resolver::find_model_of_type(
            "openrouter",
            "~typesafe/jev-latest",
            ModelType::Classifier,
        )
        .unwrap();
        let mut model =
            model_resolver::model_config_from_entry(&entry, Some("sk-test".into()), None);
        model.base_url = base_url.to_string();
        model
    }

    /// 三类问题各一道：选择题、评分题、判断题。
    fn context() -> ClassifierContext {
        ClassifierContext {
            state: json!({ "text": "The deployment succeeded, thank you." }),
            images: None,
            questions: BTreeMap::from([
                (
                    "category".to_string(),
                    ClassifierQuestion::Choice {
                        instructions: "Classify the message".into(),
                        criteria: BTreeMap::from([
                            ("success".to_string(), "Successful".to_string()),
                            ("failure".to_string(), "Failed".to_string()),
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
                (
                    "approved".to_string(),
                    ClassifierQuestion::Bool {
                        instructions: "Does the user approve?".into(),
                        criteria: BoolCriteria {
                            yes: "Approval".into(),
                            no: "No approval".into(),
                        },
                    },
                ),
            ]),
        }
    }

    /// 端到端：URL `/systemone`、`bool`→`noul` 的问题映射、三类答案解析、usage 按目录单价计费。
    #[test]
    fn typesafe_system_one_maps_bool_questions_and_answers() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let body = r#"{"answers":{
                    "category":{"type":"choice","choice":"success","probabilities":{"success":0.9,"failure":0.1},"confidence":0.8},
                    "satisfaction":{"type":"score","score":2,"confidence":0.7},
                    "approved":{"type":"noul","noul":0.95}},
                    "usage":{"input_tokens":308,"output_tokens":23}}"#;
                let (base, handle) = mock_json_server(200, body).await;

                let mut model = classifier_model(&base);
                model.cost = Some(json!({ "input": 0.042, "output": 0.0 }));

                let result = super::super::classify(&model, &context()).await;

                let request = handle.await.unwrap();
                assert!(request.starts_with("POST /systemone "), "{request}");
                assert!(request.contains("Bearer sk-test"), "{request}");
                assert!(request.contains("\"model\":\"~typesafe/jev-latest\""), "{request}");
                assert!(
                    request.contains("\"text\":\"The deployment succeeded, thank you.\""),
                    "{request}"
                );
                // bool 问题在线上是 noul
                assert!(request.contains("\"type\":\"noul\""), "{request}");
                assert!(!request.contains("\"type\":\"bool\""), "{request}");

                assert_eq!(result.stop_reason, "stop");
                assert_eq!(result.error_message, None);
                assert_eq!(result.api, "typesafe-system-one");
                assert_eq!(result.provider, "openrouter");
                assert_eq!(result.model, "~typesafe/jev-latest");
                assert_eq!(
                    result.answers.get("category"),
                    Some(&ClassifierAnswer::Choice {
                        choice: "success".into(),
                        probabilities: BTreeMap::from([
                            ("success".to_string(), 0.9),
                            ("failure".to_string(), 0.1),
                        ]),
                        confidence: 0.8,
                    })
                );
                assert_eq!(
                    result.answers.get("satisfaction"),
                    Some(&ClassifierAnswer::Score {
                        score: 2.0,
                        confidence: 0.7,
                    })
                );
                assert_eq!(
                    result.answers.get("approved"),
                    Some(&ClassifierAnswer::Bool { probability: 0.95 })
                );

                let usage = result.usage.unwrap();
                assert_eq!((usage.input, usage.output), (308, 23));
                assert_eq!(usage.total_tokens, 331);
                assert!((usage.cost.total - 308.0 / 1e6 * 0.042).abs() < 1e-12);
            });
        });
    }

    /// 答案格式不对：仍然返回已计费的 usage（请求已经发出去并计费了）。
    #[test]
    fn malformed_answers_keep_usage_and_report_error() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let (base, _handle) = mock_json_server(
                    200,
                    r#"{"answers":{},"usage":{"input_tokens":10,"output_tokens":2}}"#,
                )
                .await;
                let mut model = classifier_model(&base);
                model.cost = Some(json!({ "input": 1.0, "output": 1.0 }));

                let result = super::super::classify(&model, &context()).await;

                assert_eq!(result.stop_reason, "error");
                assert!(result.answers.is_empty());
                assert!(
                    result
                        .error_message
                        .unwrap_or_default()
                        .contains("did not return an answer for"),
                );
                let usage = result.usage.unwrap();
                assert_eq!((usage.input, usage.output), (10, 2));
            });
        });
    }

    /// 类型不匹配的答案也算失败（认题不认字段）。
    #[test]
    fn mismatched_answer_types_are_rejected() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let body = r#"{"answers":{
                    "category":{"type":"score","score":1,"confidence":0.5},
                    "satisfaction":{"type":"score","score":2,"confidence":0.7},
                    "approved":{"type":"noul","noul":0.95}}}"#;
                let (base, _handle) = mock_json_server(200, body).await;

                let result = super::super::classify(&classifier_model(&base), &context()).await;

                assert_eq!(result.stop_reason, "error");
                assert!(
                    result
                        .error_message
                        .unwrap_or_default()
                        .contains("did not return a choice answer for category"),
                );
                assert!(result.usage.is_none());
            });
        });
    }

    /// usage 容错：字段缺失/非数字按 0 计，但两个字段都缺 → 视为服务未报告用量（None）。
    #[test]
    fn malformed_usage_is_ignored() {
        let model = classifier_model("http://127.0.0.1:1");
        let usage = parse_usage(
            Some(&json!({ "input_tokens": "many", "output_tokens": 3 })),
            &model,
        )
        .unwrap();
        assert_eq!((usage.input, usage.output, usage.total_tokens), (0, 3, 3));

        assert!(parse_usage(Some(&json!({ "cost": 0.1 })), &model).is_none());
        assert!(parse_usage(Some(&json!(null)), &model).is_none());
        assert!(parse_usage(None, &model).is_none());
    }

    /// 缺凭据 → error 结果，不发请求。
    #[test]
    fn missing_api_key_returns_error_result() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut model = classifier_model("http://127.0.0.1:1");
                model.api_key = String::new();
                let result = super::super::classify(&model, &context()).await;

                assert_eq!(result.stop_reason, "error");
                assert!(
                    result
                        .error_message
                        .unwrap_or_default()
                        .contains("Provider is not configured: openrouter"),
                );
                assert!(result.usage.is_none());
            });
        });
    }

    /// 非 2xx（不可重试的 400）→ error 结果，带上状态码与响应体。
    #[test]
    fn provider_error_returns_error_result() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let (base, _handle) =
                    mock_json_server(400, r#"{"error":{"message":"bad model"}}"#).await;
                let result = super::super::classify(&classifier_model(&base), &context()).await;

                assert_eq!(result.stop_reason, "error");
                let message = result.error_message.unwrap_or_default();
                assert!(message.contains("400"), "{message}");
                assert!(message.contains("bad model"), "{message}");
                assert!(result.answers.is_empty());
            });
        });
    }

    /// chat 模型与未实现的 api 都在发起请求前被拒（不联网）。
    #[test]
    fn non_classifier_model_and_unknown_api_are_rejected_before_request() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut chat = classifier_model("http://127.0.0.1:1");
                chat.model_type = ModelType::Chat;
                let result = super::super::classify(&chat, &context()).await;
                assert_eq!(result.stop_reason, "error");
                assert!(
                    result
                        .error_message
                        .unwrap_or_default()
                        .contains("is not a classifier model"),
                );

                let mut other = classifier_model("http://127.0.0.1:1");
                other.api = "llama-cpp-classify".into();
                let result = super::super::classify(&other, &context()).await;
                assert_eq!(result.stop_reason, "error");
                assert!(
                    result
                        .error_message
                        .unwrap_or_default()
                        .contains("does not support classification"),
                );
            });
        });
    }

    /// 2.8：模型不支持图片输入时在发请求前就给错误结果
    /// （对齐 pi `assertClassifierInputSupported()`）。
    #[test]
    fn image_input_requires_a_model_that_accepts_images() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut model = classifier_model("http://127.0.0.1:1");
                // 要验证的是「模型不接受图片时在发请求前就拦住」，与目录当前的输入能力无关：
                // 目录里的分类器条目声明随目录同步变化，故显式固定为纯文本。
                model.input = vec!["text".to_string()];
                let mut with_images = context();
                with_images.images = Some(vec![ImageContent::Image {
                    data: "AAAA".into(),
                    mime_type: "image/png".into(),
                }]);

                let result = super::super::classify(&model, &with_images).await;
                let expected = format!(
                    "Model {}/{} does not accept image input",
                    model.provider, model.model_id
                );
                assert_eq!(result.stop_reason, "error");
                assert_eq!(result.error_message.as_deref(), Some(expected.as_str()));
            });
        });
    }

    /// 图片按 `compat.imageInput` 拼：OpenRouter 前置 `image_url`、TypeSafe 走 `assets`、
    /// Clef 走 data URL 列表；opt-in 头只随 TypeSafe 形态发。
    #[test]
    fn image_input_follows_the_model_compat() {
        let images = [
            ImageContent::Image {
                data: "AAAA".into(),
                mime_type: "image/png".into(),
            },
            ImageContent::Image {
                data: "BBBB".into(),
                mime_type: "image/jpeg".into(),
            },
        ];
        let mut with_images = context();
        with_images.images = Some(images.to_vec());

        // 目录里 openrouter 的分类器条目已声明 state-image-urls / 不发 opt-in 头
        let openrouter = classifier_model("http://127.0.0.1:1");
        assert_eq!(image_input_mode(&openrouter), IMAGE_INPUT_STATE_IMAGE_URLS);
        assert!(!sends_image_opt_in_header(&openrouter));
        let mut body = build_body(&openrouter, &with_images);
        add_images(&openrouter, &images, &mut body).unwrap();
        assert_eq!(body["state"][0]["type"], "image_url");
        assert_eq!(
            body["state"][0]["image_url"]["url"],
            "data:image/png;base64,AAAA"
        );
        assert_eq!(
            body["state"][2]["text"],
            "The deployment succeeded, thank you."
        );
        assert!(body.get("assets").is_none());

        // TypeSafe：`assets` 映射 + 名字前置到 `state`
        let mut typesafe = openrouter.clone();
        typesafe.provider = "typesafe".into();
        typesafe.image_input = Some(IMAGE_INPUT_ASSETS.into());
        typesafe.send_image_opt_in_header = Some(true);
        assert!(sends_image_opt_in_header(&typesafe));
        let mut body = build_body(&typesafe, &with_images);
        add_images(&typesafe, &images, &mut body).unwrap();
        assert_eq!(body["assets"]["<image-1>"]["format"], "png");
        assert_eq!(body["assets"]["<image-2>"]["data_base64"], "BBBB");
        assert_eq!(body["state"][0], "<image-1>");
        assert_eq!(
            body["state"][2]["text"],
            "The deployment succeeded, thank you."
        );

        // Clef：data URL 列表，超过 4 张直接报错
        let mut clef = typesafe.clone();
        clef.image_input = Some(IMAGE_INPUT_IMAGES.into());
        let mut body = build_body(&clef, &with_images);
        add_images(&clef, &images, &mut body).unwrap();
        assert_eq!(body["images"][1], "data:image/jpeg;base64,BBBB");
        let too_many = vec![images[0].clone(); MAX_DATA_URL_IMAGES + 1];
        let mut body = build_body(&clef, &with_images);
        assert!(add_images(&clef, &too_many, &mut body).is_err());

        // 未声明 compat 时按 provider / baseUrl 探测
        let mut bare = openrouter.clone();
        bare.image_input = None;
        bare.send_image_opt_in_header = None;
        assert_eq!(image_input_mode(&bare), IMAGE_INPUT_STATE_IMAGE_URLS);
        assert!(!sends_image_opt_in_header(&bare));
        bare.base_url = "https://systemone.example.com".into();
        bare.provider = "typesafe".into();
        assert_eq!(image_input_mode(&bare), IMAGE_INPUT_ASSETS);
        assert!(sends_image_opt_in_header(&bare));
    }

    /// 请求体形状（不含信封、无 temperature）与 URL 拼接。
    #[test]
    fn wire_question_and_url_shapes() {
        let model = classifier_model("http://127.0.0.1:1/");
        assert_eq!(system_one_url(&model), "http://127.0.0.1:1/systemone");

        let body = build_body(&model, &context());
        assert_eq!(body["model"], "~typesafe/jev-latest");
        assert_eq!(body["questions"]["category"]["type"], "choice");
        assert_eq!(body["questions"]["satisfaction"]["type"], "score");
        assert_eq!(body["questions"]["approved"]["type"], "noul");
        assert_eq!(
            body["questions"]["approved"]["criteria"]["true"],
            "Approval"
        );
        assert!(body.get("temperature").is_none());
    }
}

//! 图片生成：一次性（非流式）请求。
//!
//! 目前唯一内置的协议实现是 `openrouter-images` 。走 OpenRouter 的 chat completions 端点，
//! 请求体用 `modalities` 声明要图，生成的图片从 `choices[0].message.images[].image_url` 的 `data:` URL 里剥出 base64。
//!
//! 失败永不抛错：由 [`super::generate_images`] 把错误转成 `stop_reason: "error"` 的结果

use super::{
    MAX_TIMEOUT_MS, ModelConfig, Usage,
    convert::{sanitize_surrogates, truncate_for_error},
    provider_extra_headers,
    retry::{DEFAULT_MAX_RETRIES, send_with_retry},
    usage::compute_cost,
};
use crate::{
    error::{Error, Result},
    utils::{http, time::now_ms},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::time::Duration;

/// 图片生成请求与结果的一项内容：提示词文本或图片。
///
/// JSON 形状：`{"type":"text","text":…}` 与 `{"type":"image","mimeType":…,"data":…}`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ImageContent {
    /// 提示词文本（图生图时与输入图片一起构成一次请求）。
    Text {
        /// 提示词正文。
        text: String,
    },
    /// 图片内容：base64 数据（不含 `data:` 前缀）与 MIME 类型。
    Image {
        /// 图片数据，base64 编码（不含 data URI 前缀）。
        data: String,
        /// 图片 MIME 类型，如 image/png。
        #[serde(rename = "mimeType")]
        mime_type: String,
    },
}

/// 一次图片生成的结果
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantImages {
    /// 实际使用的协议实现（如 openrouter-images）。
    pub api: String,
    /// 提供方标识（如 openrouter）。
    pub provider: String,
    /// 请求的模型 id。
    pub model: String,
    /// 输出内容：文本与生成的图片（base64），按供应商返回顺序排列。
    pub output: Vec<ImageContent>,
    /// 供应商返回的响应 id；未返回时为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    /// 本次请求的用量与费用；供应商未返回 usage 时为 None。
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

/// 构造失败结果：`stop_reason = "error"` 且不带输出，供入口层统一返回（不抛错）。
pub(crate) fn error_result(model: &ModelConfig, message: impl Into<String>) -> AssistantImages {
    AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.model_id.clone(),
        output: Vec::new(),
        response_id: None,
        usage: None,
        stop_reason: "error".to_string(),
        error_message: Some(message.into()),
        timestamp: now_ms(),
    }
}

/// `openrouter-images` 协议实现：一次非流式 chat completions 请求。
///
/// 失败返回 `Err`（由 [`super::generate_images`] 转成 error 结果）；
/// 只有拿到 2xx 响应并解析成功才返回 `Ok`。
pub(crate) async fn generate_openrouter_images(
    model: &ModelConfig,
    input: &[ImageContent],
) -> Result<AssistantImages> {
    if model.api_key.is_empty() {
        return Err(Error::msg(format!(
            "Provider is not configured: {}",
            model.provider
        )));
    }

    let body = build_body(model, input);
    let client = http::build_client()?;
    let response = send_with_retry(
        || {
            client
                .post(model.endpoint())
                .header("Content-Type", "application/json")
                .bearer_auth(&model.api_key)
                .headers(provider_extra_headers(model))
                .timeout(Duration::from_millis(MAX_TIMEOUT_MS))
                .body(body.clone())
        },
        DEFAULT_MAX_RETRIES,
        model.max_retry_delay_ms,
    )
    .await?;

    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(Error::ProviderStatus {
            status,
            body: truncate_for_error(&text),
        });
    }

    let payload: Value = response.json().await.map_err(Error::from)?;
    Ok(parse_response(model, &payload))
}

/// 组装请求体：单条 user 消息（文本 / `image_url` 内容块）+ `modalities`。
///
/// `modalities` 含 `text` 的条件是目录 `output` 里有 `text`（该模型除图片外还能返回文本）。
fn build_body(model: &ModelConfig, input: &[ImageContent]) -> String {
    let content: Vec<Value> = input
        .iter()
        .map(|item| match item {
            ImageContent::Text { text } => json!({
                "type": "text",
                "text": sanitize_surrogates(text),
            }),
            ImageContent::Image { data, mime_type } => json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{mime_type};base64,{data}") },
            }),
        })
        .collect();

    let modalities = if model.output.iter().any(|m| m == "text") {
        json!(["image", "text"])
    } else {
        json!(["image"])
    };

    json!({
        "model": model.model_id,
        "messages": [{ "role": "user", "content": content }],
        "stream": false,
        "modalities": modalities,
    })
    .to_string()
}

/// 解析响应体：`usage` 计费 + 首个 choice 的文本与 `data:` 图片。
fn parse_response(model: &ModelConfig, payload: &Value) -> AssistantImages {
    let mut output = Vec::new();
    let mut usage = None;

    if let Some(raw_usage) = payload.get("usage") {
        let mut parsed = parse_usage(raw_usage);
        compute_cost(&mut parsed, model.cost.as_ref());
        usage = Some(parsed);
    }

    if let Some(message) = payload
        .get("choices")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .and_then(|c| c.get("message"))
    {
        if let Some(text) = message.get("content").and_then(|v| v.as_str())
            && !text.is_empty()
        {
            output.push(ImageContent::Text {
                text: text.to_string(),
            });
        }

        if let Some(images) = message.get("images").and_then(|v| v.as_array()) {
            for image in images {
                let url = match image.get("image_url") {
                    Some(Value::String(s)) => Some(s.as_str()),
                    Some(v) => v.get("url").and_then(|u| u.as_str()),
                    None => None,
                };
                let Some((mime_type, data)) = url.and_then(parse_data_url) else {
                    continue;
                };
                output.push(ImageContent::Image {
                    data: data.to_string(),
                    mime_type: mime_type.to_string(),
                });
            }
        }
    }

    AssistantImages {
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.model_id.clone(),
        output,
        response_id: payload
            .get("id")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        usage,
        stop_reason: "stop".to_string(),
        error_message: None,
        timestamp: now_ms(),
    }
}

/// 解析 `data:<mime>;base64,<data>`；非 data URL 或结构不符时返回 None
fn parse_data_url(url: &str) -> Option<(&str, &str)> {
    let rest = url.strip_prefix("data:")?;
    let (mime_type, data) = rest.split_once(";base64,")?;
    if mime_type.is_empty() || data.is_empty() {
        return None;
    }
    Some((mime_type, data))
}

/// 从响应 `usage` 解析 token 用量：
/// `prompt_tokens_details.cache_write_tokens` 属于缓存写入，需从 `cached_tokens` 里扣除；
/// 费用由调用方经 [`compute_cost`] 按目录单价计算。
fn parse_usage(raw: &Value) -> Usage {
    let num = |v: Option<&Value>| v.and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let details = raw.get("prompt_tokens_details");

    let prompt = num(raw.get("prompt_tokens"));
    let reported_cached = num(details.and_then(|d| d.get("cached_tokens")));
    let cache_write = num(details.and_then(|d| d.get("cache_write_tokens")));
    let cache_read = if cache_write > 0 {
        reported_cached.saturating_sub(cache_write)
    } else {
        reported_cached
    };
    let input = prompt.saturating_sub(cache_read + cache_write);
    let output = num(raw.get("completion_tokens"));

    Usage {
        input,
        output,
        cache_read,
        cache_write,
        cache_write_1h: None,
        reasoning: None,
        total_tokens: input + output + cache_read + cache_write,
        cost: Default::default(),
    }
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
    /// 返回 base_url 与服务器 task；task 的返回值是**收到的原始请求文本**。
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

    /// 用真实目录条目构造图片模型配置，并把 base_url 指向本地假服务器。
    fn image_model(base_url: &str, id: &str) -> ModelConfig {
        let entry = model_resolver::find_model_of_type("openrouter", id, ModelType::Image).unwrap();
        let mut model =
            model_resolver::model_config_from_entry(&entry, Some("sk-test".into()), None);
        model.base_url = base_url.to_string();
        model
    }

    /// 端到端：请求体形状（content 块 / modalities / stream）+ 文本与 data: 图片解析
    /// + usage 计费 + stop_reason=stop。非 data: 的图片 URL 被跳过。
    #[test]
    fn openrouter_images_parses_output_and_usage() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let body = r#"{"id":"gen_1","choices":[{"message":{"content":"here you go","images":[
                    {"image_url":"data:image/png;base64,AAAA"},
                    {"image_url":{"url":"data:image/jpeg;base64,BBBB"}},
                    {"image_url":"https://example.com/x.png"}]}}],
                    "usage":{"prompt_tokens":100,"completion_tokens":50,
                    "prompt_tokens_details":{"cached_tokens":30,"cache_write_tokens":10}}}"#;
                let (base, handle) = mock_json_server(200, body).await;

                let mut model = image_model(&base, "google/gemini-3-pro-image");
                model.cost = Some(json!({
                    "input": 1.0, "output": 2.0, "cacheRead": 0.5, "cacheWrite": 4.0
                }));

                let result = super::super::generate_images(
                    &model,
                    &[ImageContent::Text {
                        text: "draw a circle".into(),
                    }],
                )
                .await;

                let request = handle.await.unwrap();
                assert!(request.contains("\"modalities\":[\"image\",\"text\"]"), "{request}");
                assert!(request.contains("\"stream\":false"), "{request}");
                assert!(
                    request.contains("\"model\":\"google/gemini-3-pro-image\""),
                    "{request}"
                );
                assert!(request.contains("\"text\":\"draw a circle\""), "{request}");
                assert!(request.contains("Bearer sk-test"), "{request}");

                assert_eq!(result.stop_reason, "stop");
                assert_eq!(result.error_message, None);
                assert_eq!(result.response_id.as_deref(), Some("gen_1"));
                assert_eq!(result.api, "openrouter-images");
                assert_eq!(result.provider, "openrouter");
                assert_eq!(
                    result.output,
                    vec![
                        ImageContent::Text {
                            text: "here you go".into()
                        },
                        ImageContent::Image {
                            data: "AAAA".into(),
                            mime_type: "image/png".into()
                        },
                        ImageContent::Image {
                            data: "BBBB".into(),
                            mime_type: "image/jpeg".into()
                        },
                    ]
                );

                // cached 30 含 cache_write 10 → cache_read 20、input 100-20-10=70
                let usage = result.usage.unwrap();
                assert_eq!(
                    (usage.input, usage.output, usage.cache_read, usage.cache_write),
                    (70, 50, 20, 10)
                );
                assert_eq!(usage.total_tokens, 150);
                let expected =
                    70.0 / 1e6 * 1.0 + 50.0 / 1e6 * 2.0 + 20.0 / 1e6 * 0.5 + 10.0 / 1e6 * 4.0;
                assert!((usage.cost.total - expected).abs() < 1e-12);
            });
        });
    }

    /// 只出图的模型（目录 `output: ["image"]`）不发 text 模态。
    #[test]
    fn image_only_model_requests_image_modality() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let (base, handle) = mock_json_server(200, r#"{"choices":[]}"#).await;
                let model = image_model(&base, "black-forest-labs/flux.2-flex");
                let result = super::super::generate_images(
                    &model,
                    &[ImageContent::Image {
                        data: "AAAA".into(),
                        mime_type: "image/png".into(),
                    }],
                )
                .await;

                let request = handle.await.unwrap();
                assert!(request.contains("\"modalities\":[\"image\"]"), "{request}");
                assert!(
                    request.contains("data:image/png;base64,AAAA"),
                    "输入图片按 data URL 内联：{request}"
                );
                assert_eq!(result.stop_reason, "stop");
                assert!(result.output.is_empty());
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
                let model = image_model(&base, "black-forest-labs/flux.2-flex");
                let result = super::super::generate_images(
                    &model,
                    &[ImageContent::Text { text: "x".into() }],
                )
                .await;

                assert_eq!(result.stop_reason, "error");
                let message = result.error_message.unwrap_or_default();
                assert!(message.contains("400"), "{message}");
                assert!(message.contains("bad model"), "{message}");
                assert!(result.output.is_empty());
            });
        });
    }

    /// 缺凭据 → error 结果（不发请求）。
    #[test]
    fn missing_api_key_returns_error_result() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut model = image_model("http://127.0.0.1:1", "black-forest-labs/flux.2-flex");
                model.api_key = String::new();
                let result = super::super::generate_images(
                    &model,
                    &[ImageContent::Text { text: "x".into() }],
                )
                .await;

                assert_eq!(result.stop_reason, "error");
                assert!(
                    result
                        .error_message
                        .unwrap_or_default()
                        .contains("Provider is not configured: openrouter")
                );
            });
        });
    }

    /// chat 模型与未实现的 api 都在发起请求前被拒（不联网）。
    #[test]
    fn non_image_model_and_unknown_api_are_rejected_before_request() {
        run_net_test(|| {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let mut chat = image_model("http://127.0.0.1:1", "black-forest-labs/flux.2-flex");
                chat.model_type = ModelType::Chat;
                let result = super::super::generate_images(
                    &chat,
                    &[ImageContent::Text { text: "x".into() }],
                )
                .await;
                assert_eq!(result.stop_reason, "error");
                assert!(
                    result
                        .error_message
                        .unwrap_or_default()
                        .contains("is not an image model"),
                );

                let mut other = image_model("http://127.0.0.1:1", "black-forest-labs/flux.2-flex");
                other.api = "typesafe-system-one".into();
                let result = super::super::generate_images(
                    &other,
                    &[ImageContent::Text { text: "x".into() }],
                )
                .await;
                assert_eq!(result.stop_reason, "error");
                assert!(
                    result
                        .error_message
                        .unwrap_or_default()
                        .contains("does not support image generation"),
                );
            });
        });
    }

    /// data URL 解析：只接受 `data:<mime>;base64,<非空>`。
    #[test]
    fn data_url_parsing_rejects_non_data_urls() {
        assert_eq!(
            parse_data_url("data:image/png;base64,AA"),
            Some(("image/png", "AA"))
        );
        assert_eq!(parse_data_url("https://example.com/x.png"), None);
        assert_eq!(parse_data_url("data:image/png;base64,"), None);
        assert_eq!(parse_data_url("data:;base64,AA"), None);
        assert_eq!(parse_data_url("data:image/png,AA"), None);
    }

    /// usage 解析：`cache_write_tokens` 从 `cached_tokens` 里扣除（对齐 pi parseUsage）。
    #[test]
    fn usage_splits_cache_write_from_cached() {
        let u = parse_usage(&json!({
            "prompt_tokens": 10,
            "completion_tokens": 4,
            "prompt_tokens_details": { "cached_tokens": 6 }
        }));
        assert_eq!(
            (u.input, u.cache_read, u.cache_write, u.output),
            (4, 6, 0, 4)
        );

        let u = parse_usage(&json!({
            "prompt_tokens": 10,
            "completion_tokens": 4,
            "prompt_tokens_details": { "cached_tokens": 6, "cache_write_tokens": 6 }
        }));
        assert_eq!(
            (u.input, u.cache_read, u.cache_write, u.output),
            (4, 0, 6, 4)
        );
        assert_eq!(u.total_tokens, 14);
    }
}

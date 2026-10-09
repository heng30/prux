//! 发送层重试。规则：
//!
//! - `x-should-retry` 头显式优先（true/false）
//! - 408/409/429/>=500 可重试
//! - `retry-after-ms` / `retry-after` 头决定等待时长（超过上限直接放弃）
//! - 退避 min(0.5 * 2^n, 8)s 并带 ±25% 抖动
//! - 服务端要求 delay 超过 max_delay_ms（默认 60s）时直接放弃

use crate::{error::Result, utils::http};
use reqwest::{RequestBuilder, Response, StatusCode, header::HeaderMap};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 设置项 `retry.maxRetries` 未配置时的默认重试次数。
pub const DEFAULT_MAX_RETRIES: u32 = 3;
/// 单次重试等待时长上限（毫秒）；服务端要求的延迟超过它就直接放弃。
const DEFAULT_MAX_RETRY_DELAY_MS: u64 = 60_000;

/// 异步等待指定毫秒数（重试退避用）。
async fn sleep_ms(ms: u64) {
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

/// 简单伪随机 [0,1)
fn jitter() -> f64 {
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let mut x = t
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    x = (x ^ (x >> 32)).wrapping_mul(12741317482634936597);
    ((x >> 33) as f64) / (1u64 << 31) as f64
}

/// 判断该响应是否应重试：`x-should-retry` 头显式优先（true/false），
/// 否则 408/409/429 与所有 5xx 可重试。
fn should_retry(status: StatusCode, headers: &HeaderMap) -> bool {
    let status_u16 = status.as_u16();

    // 服务端显式指令优先
    if let Some(v) = headers.get("x-should-retry").and_then(|v| v.to_str().ok()) {
        match v.trim().to_ascii_lowercase().as_str() {
            "true" => return true,
            "false" => return false,
            _ => {}
        }
    }
    status_u16 == 408 || status_u16 == 409 || status_u16 == 429 || status_u16 >= 500
}

/// 发送请求并执行 provider 重试策略。
///
/// `build_request` 每次重试时重新构建（reqwest::RequestBuilder 不可克隆）。
/// 成功（2xx）返回响应；网络错误直接返回；重试耗尽或请求不可重试时
/// 返回最后一次失败响应，由调用方读取 body 构造 `ProviderStatus` 错误。
pub(crate) async fn send_with_retry(
    build_request: impl Fn() -> RequestBuilder,
    max_retries: u32,
    max_delay_ms: Option<u64>,
) -> Result<Response> {
    send_with_retry_ex(build_request, max_retries, max_delay_ms, &[]).await
}

/// [`send_with_retry`] 的扩展形式：`no_retry_statuses` 里的状态码即使属于
/// [`should_retry`] 认可的可重试类别，也立即返回该响应而不重试。
///
/// 用于「重试同样的请求必然再次失败」的网关行为，例如 OpenAI Decisions 在
/// Cloudflare 网关前对超长输入固定回 504。
pub(crate) async fn send_with_retry_ex(
    build_request: impl Fn() -> RequestBuilder,
    max_retries: u32,
    max_delay_ms: Option<u64>,
    no_retry_statuses: &[u16],
) -> Result<Response> {
    let delay_cap = max_delay_ms.unwrap_or(DEFAULT_MAX_RETRY_DELAY_MS);
    let mut retries_remaining = max_retries;
    loop {
        let response = build_request().send().await?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let headers = response.headers().clone();
        let retryable =
            should_retry(status, &headers) && !no_retry_statuses.contains(&status.as_u16());
        let last_response = response;
        if !retryable || retries_remaining == 0 {
            return Ok(last_response);
        }
        let retry_index = max_retries - retries_remaining;
        retries_remaining -= 1;

        let backoff_ms = ((0.5 * 2f64.powi(retry_index as i32)).min(8.0) * 1000.0) as u64;
        let delay_ms = http::retry_after_ms(&headers).unwrap_or(backoff_ms);
        if delay_ms > delay_cap {
            return Ok(last_response);
        }
        let jittered = ((delay_ms as f64) * (1.0 - 0.25 * jitter())).max(1.0) as u64;
        sleep_ms(jittered).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> reqwest::header::HeaderMap {
        let mut m = reqwest::header::HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    #[test]
    fn retryable_status_codes() {
        let h = headers(&[]);
        assert!(!should_retry(reqwest::StatusCode::OK, &h));
        assert!(!should_retry(reqwest::StatusCode::BAD_REQUEST, &h));
        assert!(should_retry(reqwest::StatusCode::REQUEST_TIMEOUT, &h)); // 408
        assert!(should_retry(reqwest::StatusCode::CONFLICT, &h)); // 409
        assert!(should_retry(reqwest::StatusCode::TOO_MANY_REQUESTS, &h)); // 429
        assert!(should_retry(reqwest::StatusCode::BAD_GATEWAY, &h)); // 502
    }

    #[test]
    fn x_should_retry_overrides_status() {
        let h = headers(&[("x-should-retry", "true")]);
        assert!(should_retry(reqwest::StatusCode::BAD_REQUEST, &h));
        let h = headers(&[("x-should-retry", "false")]);
        assert!(!should_retry(reqwest::StatusCode::TOO_MANY_REQUESTS, &h));
    }

    #[test]
    fn jitter_stays_in_unit_range() {
        for _ in 0..50 {
            let j = jitter();
            assert!((0.0..=1.0).contains(&j));
        }
    }
}

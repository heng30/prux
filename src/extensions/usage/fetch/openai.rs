//! OpenAI（ChatGPT 订阅）的配额端点：`GET chatgpt.com/backend-api/wham/usage`。
//!
//! 后端要求带上 ChatGPT 账户 id，它就在 access token 的 JWT 声明里（[`chatgpt_account_id`]）；
//! 解析不出来就不带该头，让服务端按默认账户处理，而不是直接放弃。

use super::{
    ProviderReport, QuotaWindow,
    common::{
        as_f64, clamp_percent, parse_reset_ms, percent_from_object, resets_at_from_object,
        window_label,
    },
    token_for,
};
use crate::utils::http::get_json;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Client;
use serde_json::Value;

/// ChatGPT 后端的用量端点（Codex CLI 同款）。
pub(super) const OPENAI_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

/// OpenAI：5 小时 / 周窗口（含按模型的额外窗口）与重置券张数。
pub(super) async fn fetch_openai(client: &Client) -> Option<ProviderReport> {
    let token = match token_for("openai").await {
        Ok(Some(token)) => token,
        Ok(None) => return None,
        Err(err) => return Some(ProviderReport::failed("openai", err)),
    };
    Some(fetch_openai_at(client, OPENAI_USAGE_URL, &token).await)
}

/// [`fetch_openai`] 的可注入端点版本（测试用）。
pub(super) async fn fetch_openai_at(client: &Client, url: &str, token: &str) -> ProviderReport {
    let mut headers = vec![
        ("Authorization".to_string(), format!("Bearer {token}")),
        (
            "User-Agent".to_string(),
            crate::extensions::util::default_user_agent(),
        ),
    ];
    // 后端要求带上 ChatGPT 账户 id，否则按「无账户」处理；它就在 access token 的 JWT 声明里。
    if let Some(account_id) = chatgpt_account_id(token) {
        headers.push(("chatgpt-account-id".to_string(), account_id));
    }
    match get_json(client, url, &headers).await {
        Ok(json) => parse_openai(&json),
        Err(err) => ProviderReport::failed("openai", err),
    }
}

/// 从 access token（JWT）里取 ChatGPT 账户 id。
///
/// 只看声明里的 `https://api.openai.com/auth.chatgpt_account_id`（新版签发）与顶层
/// `chatgpt_account_id`（旧版）；解析失败一律 `None`，不影响请求本身。
pub(super) fn chatgpt_account_id(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let decoded = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let json: Value = serde_json::from_slice(&decoded).ok()?;
    json.get("https://api.openai.com/auth")
        .and_then(|auth| auth.get("chatgpt_account_id"))
        .or_else(|| json.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

/// 解析 ChatGPT 用量响应。
pub(super) fn parse_openai(json: &Value) -> ProviderReport {
    let mut report = ProviderReport::new("openai");

    if let Some(rate_limit) = json.get("rate_limit") {
        report
            .windows
            .extend(parse_wham_windows(rate_limit, "5-hour", "Weekly"));
    }

    if let Some(additional) = json.get("additional_rate_limits").and_then(Value::as_array) {
        for entry in additional {
            let Some(rate_limit) = entry.get("rate_limit") else {
                continue;
            };
            let prefix = entry
                .get("limit_name")
                .and_then(Value::as_str)
                .unwrap_or("Additional");
            for mut window in parse_wham_windows(rate_limit, "5-hour", "Weekly") {
                window.name = format!("{prefix} · {}", window.name);
                report.windows.push(window);
            }
        }
    }

    if report.windows.is_empty()
        && let Some(windows) = json.get("rate_limits").and_then(Value::as_array)
    {
        for entry in windows {
            let Some(object) = entry.as_object() else {
                continue;
            };
            let Some(percent) = percent_from_object(object) else {
                continue;
            };
            report.windows.push(QuotaWindow {
                name: entry
                    .get("name")
                    .or_else(|| entry.get("limit_name"))
                    .and_then(Value::as_str)
                    .unwrap_or("window")
                    .to_string(),
                percent,
                resets_at: resets_at_from_object(object),
            });
        }
    }

    if let Some(plan) = json
        .get("plan_type")
        .or_else(|| json.get("plan"))
        .and_then(Value::as_str)
    {
        report.extra.push(("Plan".to_string(), plan.to_string()));
    }

    if let Some(count) = json
        .get("rate_limit_reset_credits")
        .and_then(|credits| credits.get("available_count"))
        .and_then(Value::as_u64)
    {
        report
            .extra
            .push(("Reset credits".to_string(), count.to_string()));
    }

    if let Some(allowed) = json
        .get("rate_limit")
        .and_then(|limit| limit.get("allowed"))
        .and_then(Value::as_bool)
        && !allowed
    {
        report
            .extra
            .push(("Status".to_string(), "limit reached".to_string()));
    }

    report
}

/// 解析 `rate_limit` 的 `primary_window` / `secondary_window`。
pub(super) fn parse_wham_windows(
    rate_limit: &Value,
    primary_name: &str,
    secondary_name: &str,
) -> Vec<QuotaWindow> {
    let mut out = Vec::new();
    for (key, fallback) in [
        ("primary_window", primary_name),
        ("secondary_window", secondary_name),
    ] {
        let Some(window) = rate_limit.get(key).filter(|window| !window.is_null()) else {
            continue;
        };
        let Some(percent) = window.get("used_percent").and_then(as_f64) else {
            continue;
        };
        let name = window
            .get("limit_window_seconds")
            .and_then(as_f64)
            .map(window_label)
            .unwrap_or_else(|| fallback.to_string());
        out.push(QuotaWindow {
            name,
            percent: clamp_percent(percent),
            resets_at: window.get("reset_at").and_then(parse_reset_ms),
        });
    }
    out
}

#[cfg(test)]
mod tests {

    use super::*;

    /// 主 / 次窗口、附加（按模型）窗口、套餐与重置券张数都进报告。
    #[test]
    fn parse_openai_reads_primary_secondary_and_additional() {
        let json = serde_json::json!({
            "rate_limit": {
                "allowed": false,
                "primary_window": {"used_percent": 12.5, "limit_window_seconds": 18000, "reset_at": 1_700_000_000},
                "secondary_window": {"used_percent": 88.0, "limit_window_seconds": 604800}
            },
            "additional_rate_limits": [
                {"limit_name": "Codex", "rate_limit": {"primary_window": {"used_percent": 3.0}}}
            ],
            "rate_limit_reset_credits": {"available_count": 2},
            "plan_type": "pro"
        });
        let report = parse_openai(&json);
        assert_eq!(report.provider, "openai");
        assert_eq!(report.windows.len(), 3);
        assert_eq!(report.windows[0].name, "5-hour");
        assert_eq!(report.windows[0].percent, 12.5);
        assert_eq!(report.windows[0].resets_at, Some(1_700_000_000_000));
        assert_eq!(report.windows[1].name, "Weekly");
        assert_eq!(
            report.windows[2].name, "Codex · 5-hour",
            "附加窗口带前缀与回退名"
        );
        assert!(
            report
                .extra
                .contains(&("Plan".to_string(), "pro".to_string()))
        );
        assert!(
            report
                .extra
                .contains(&("Reset credits".to_string(), "2".to_string()))
        );
        assert!(
            report
                .extra
                .contains(&("Status".to_string(), "limit reached".to_string())),
            "allowed=false 应标出已达上限"
        );
    }

    /// 只给扁平 `rate_limits[]` 的旧形状也能读。
    #[test]
    fn parse_openai_falls_back_to_flat_rate_limits() {
        let json = serde_json::json!({
            "rate_limits": [
                {"name": "5h", "used_percent": 10.0},
                {"used_percent": 20.0}
            ]
        });
        let report = parse_openai(&json);
        assert_eq!(report.windows.len(), 2);
        assert_eq!(report.windows[0].name, "5h");
        assert_eq!(report.windows[1].name, "window");
    }

    /// 账户 id 从 access token 的 JWT 声明里取（新旧两种位置）。
    #[test]
    fn chatgpt_account_id_reads_jwt_claim() {
        let payload = serde_json::json!({
            "https://api.openai.com/auth": {"chatgpt_account_id": "acct-1"}
        });
        let token = format!(
            "header.{}.signature",
            base64::Engine::encode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                serde_json::to_vec(&payload).unwrap()
            )
        );
        assert_eq!(chatgpt_account_id(&token).as_deref(), Some("acct-1"));

        // 旧版顶层字段
        let payload = serde_json::json!({"chatgpt_account_id": "acct-2"});
        let token = format!(
            "header.{}.signature",
            base64::Engine::encode(
                &base64::engine::general_purpose::URL_SAFE_NO_PAD,
                serde_json::to_vec(&payload).unwrap()
            )
        );
        assert_eq!(chatgpt_account_id(&token).as_deref(), Some("acct-2"));

        assert_eq!(chatgpt_account_id("not-a-jwt"), None);
        assert_eq!(chatgpt_account_id(""), None);
    }
}

//! DeepSeek 的余额端点：`GET https://api.deepseek.com/user/balance`。
//!
//! 只报余额与 key 是否可用——DeepSeek 没有配额窗口这类概念，所以报告里不会出现进度条。

use super::{ProviderReport, common::value_text, token_for};
use crate::utils::http::get_json;
use reqwest::Client;
use serde_json::Value;

/// DeepSeek 的余额端点。
pub(super) const DEEPSEEK_BALANCE_URL: &str = "https://api.deepseek.com/user/balance";

/// DeepSeek：只有余额，没有配额窗口。
pub(super) async fn fetch_deepseek(client: &Client) -> Option<ProviderReport> {
    let token = match token_for("deepseek").await {
        Ok(Some(token)) => token,
        Ok(None) => return None,
        Err(err) => return Some(ProviderReport::failed("deepseek", err)),
    };
    Some(fetch_deepseek_at(client, DEEPSEEK_BALANCE_URL, &token).await)
}

/// [`fetch_deepseek`] 的可注入端点版本（测试用）。
pub(super) async fn fetch_deepseek_at(client: &Client, url: &str, token: &str) -> ProviderReport {
    let headers = vec![("Authorization".to_string(), format!("Bearer {token}"))];
    match get_json(client, url, &headers).await {
        Ok(json) => {
            let mut report = ProviderReport::new("deepseek");
            if let Some(available) = json.get("is_available").and_then(Value::as_bool) {
                report.extra.push((
                    "Key status".to_string(),
                    if available {
                        "valid (balance available)".to_string()
                    } else {
                        "balance exhausted".to_string()
                    },
                ));
            }
            if let Some(infos) = json.get("balance_infos").and_then(Value::as_array) {
                for info in infos {
                    let currency = info.get("currency").and_then(Value::as_str).unwrap_or("?");
                    let total = info
                        .get("total_balance")
                        .and_then(value_text)
                        .unwrap_or_else(|| "?".to_string());
                    let mut detail = format!("{total} {currency}");
                    if let (Some(paid), Some(granted)) = (
                        info.get("topped_up_balance").and_then(value_text),
                        info.get("granted_balance").and_then(value_text),
                    ) {
                        detail.push_str(&format!(" ({paid} paid + {granted} granted)"));
                    }
                    report.extra.push(("Balance".to_string(), detail));
                }
            }
            if report.extra.is_empty() {
                report.error = Some("no balance info in response".to_string());
            }
            report
        }
        Err(err) => ProviderReport::failed("deepseek", err),
    }
}

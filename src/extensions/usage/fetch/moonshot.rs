//! Moonshot（含大陆站）的余额端点：`GET {base}/users/me/balance`。
//!
//! 与 [`super::deepseek`] 一样是余额型 provider：没有配额窗口，只报可用余额与现金 / 代金券拆分。

use super::{ProviderReport, common::as_f64, token_for};
use crate::{core::login_registry, utils::http::get_json};
use reqwest::Client;

/// Moonshot：`GET {base}/users/me/balance`，只有余额。
pub(super) async fn fetch_moonshot(client: &Client, provider: &str) -> Option<ProviderReport> {
    let token = match token_for(provider).await {
        Ok(Some(token)) => token,
        Ok(None) => return None,
        Err(err) => return Some(ProviderReport::failed(provider, err)),
    };
    let base = login_registry::LOGIN_PROVIDERS
        .iter()
        .find(|p| p.id == provider)
        .map(|p| p.base_url.trim_end_matches('/').to_string())
        .unwrap_or_default();
    let url = format!("{base}/users/me/balance");
    Some(fetch_moonshot_at(client, &url, provider, &token).await)
}

/// [`fetch_moonshot`] 的可注入端点版本（测试用）。
pub(super) async fn fetch_moonshot_at(
    client: &Client,
    url: &str,
    provider: &str,
    token: &str,
) -> ProviderReport {
    let headers = vec![("Authorization".to_string(), format!("Bearer {token}"))];
    match get_json(client, url, &headers).await {
        Ok(json) => {
            let mut report = ProviderReport::new(provider);
            let currency = if provider.ends_with("-cn") {
                "CNY"
            } else {
                "USD"
            };
            let Some(data) = json.get("data") else {
                report.error = Some("no balance data in response".to_string());
                return report;
            };
            if let Some(available) = data.get("available_balance").and_then(as_f64) {
                report.extra.push((
                    "Balance".to_string(),
                    format!("{available:.2} {currency} available"),
                ));
                report.extra.push((
                    "Key status".to_string(),
                    if available > 0.0 {
                        "valid (balance available)".to_string()
                    } else {
                        "balance exhausted".to_string()
                    },
                ));
            }
            if let (Some(cash), Some(voucher)) = (
                data.get("cash_balance").and_then(as_f64),
                data.get("voucher_balance").and_then(as_f64),
            ) {
                report.extra.push((
                    "Balance breakdown".to_string(),
                    format!("{cash:.2} cash + {voucher:.2} voucher {currency}"),
                ));
            }
            if report.extra.is_empty() {
                report.error = Some("no balance fields in response".to_string());
            }
            report
        }
        Err(err) => ProviderReport::failed(provider, err),
    }
}

//! OpenRouter 的配额来源：`GET /api/v1/key` 与 `GET /api/v1/credits`。
//!
//! 两个端点各答一半：`credits` 给总额度与已用额度（画进度条），`key` 给今日 / 本周 / 本月
//! 花费与该 key 自己的限额。两个请求并发发出，任一成功就先把那半边报出来。

use super::{
    ProviderReport, QuotaWindow,
    common::{as_f64, used_limit_percent},
    token_for,
};
use crate::utils::http::get_json;
use reqwest::Client;
use serde_json::Value;

/// OpenRouter 的 key 与 credits 端点。
pub(super) const OPENROUTER_KEY_URL: &str = "https://openrouter.ai/api/v1/key";
/// OpenRouter 的 credits 端点。
pub(super) const OPENROUTER_CREDITS_URL: &str = "https://openrouter.ai/api/v1/credits";

/// OpenRouter：额度条 + 今日/本周/本月花费。
pub(super) async fn fetch_openrouter(client: &Client) -> Option<ProviderReport> {
    let token = match token_for("openrouter").await {
        Ok(Some(token)) => token,
        Ok(None) => return None,
        Err(err) => return Some(ProviderReport::failed("openrouter", err)),
    };
    Some(fetch_openrouter_at(client, OPENROUTER_KEY_URL, OPENROUTER_CREDITS_URL, &token).await)
}

/// [`fetch_openrouter`] 的可注入端点版本（测试用）。
pub(super) async fn fetch_openrouter_at(
    client: &Client,
    key_url: &str,
    credits_url: &str,
    token: &str,
) -> ProviderReport {
    let headers = vec![("Authorization".to_string(), format!("Bearer {token}"))];
    let (key, credits) = futures_util::join!(
        get_json(client, key_url, &headers),
        get_json(client, credits_url, &headers)
    );

    let mut report = ProviderReport::new("openrouter");
    match (key, credits) {
        (Err(key_err), Err(credits_err)) => {
            report.error = Some(format!("{key_err}; {credits_err}"));
            return report;
        }
        (key, credits) => {
            if let Ok(json) = credits {
                parse_openrouter_credits(&json, &mut report);
            }
            if let Ok(json) = key {
                parse_openrouter_key(&json, &mut report);
            }
        }
    }
    report
}

/// 解析 `/credits`：总额度与已用额度。
pub(super) fn parse_openrouter_credits(json: &Value, report: &mut ProviderReport) {
    let Some(data) = json.get("data") else {
        return;
    };
    let total = data.get("total_credits").and_then(as_f64).unwrap_or(0.0);
    let usage = data.get("total_usage").and_then(as_f64).unwrap_or(0.0);
    if let Some(percent) = used_limit_percent(usage, total) {
        report.windows.push(QuotaWindow {
            name: "Credits".to_string(),
            percent,
            resets_at: None,
        });
        report.extra.push((
            "Balance".to_string(),
            format!("${:.2} / ${:.2}", (total - usage).max(0.0), total),
        ));
    }
}

/// 解析 `/key`：日/周/月花费与 key 自身限额。
pub(super) fn parse_openrouter_key(json: &Value, report: &mut ProviderReport) {
    let Some(data) = json.get("data") else {
        return;
    };
    for (key, label) in [
        ("usage_daily", "Today"),
        ("usage_weekly", "This week"),
        ("usage_monthly", "This month"),
    ] {
        if let Some(value) = data.get(key).and_then(as_f64) {
            report
                .extra
                .push((label.to_string(), format!("${value:.2}")));
        }
    }
    if let Some(limit) = data.get("limit").and_then(as_f64) {
        let remaining = data.get("limit_remaining").and_then(as_f64).unwrap_or(0.0);
        if let Some(percent) = used_limit_percent(limit - remaining, limit) {
            report.windows.push(QuotaWindow {
                name: "Key limit".to_string(),
                percent,
                resets_at: None,
            });
        }
        report.extra.push((
            "Key limit".to_string(),
            format!("${remaining:.2} left of ${limit:.2}"),
        ));
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    /// `credits` 画额度条，`key` 补花费与 key 自身限额。
    #[test]
    fn parse_openrouter_credits_and_key_limits() {
        let mut report = ProviderReport::new("openrouter");
        parse_openrouter_credits(
            &serde_json::json!({"data": {"total_credits": 20.0, "total_usage": 5.0}}),
            &mut report,
        );
        assert_eq!(report.windows[0].name, "Credits");
        assert_eq!(report.windows[0].percent, 25.0);
        assert!(
            report
                .extra
                .contains(&("Balance".to_string(), "$15.00 / $20.00".to_string()))
        );

        parse_openrouter_key(
            &serde_json::json!({"data": {
                "usage_daily": 1.5, "usage_weekly": 4.0, "usage_monthly": 9.0,
                "limit": 10.0, "limit_remaining": 1.0
            }}),
            &mut report,
        );
        assert_eq!(report.windows[1].name, "Key limit");
        assert_eq!(report.windows[1].percent, 90.0);
        assert!(
            report
                .extra
                .contains(&("Today".to_string(), "$1.50".to_string()))
        );
    }
}

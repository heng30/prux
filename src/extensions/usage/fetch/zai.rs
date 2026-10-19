//! Z.AI Coding Plan 的配额端点：`GET {origin}/api/monitor/usage/quota/limit`。
//!
//! origin 跟着 key 的区域走（国际 `api.z.ai` / 大陆 `open.bigmodel.cn`），两边的 key 互不通用，
//! 走错 host 会被直接拒掉。按量付费的 key 在该端点没有窗口，由上层显示成错误行。

use super::{
    ProviderReport, QuotaWindow,
    common::{as_f64, clamp_percent, parse_reset_ms, used_limit_percent},
    token_for,
};
use crate::{core::login_registry, utils::http::get_json};
use reqwest::Client;
use serde_json::Value;

/// Z.AI 的配额端点路径（挂在区域 origin 上）。
pub(super) const ZAI_QUOTA_PATH: &str = "/api/monitor/usage/quota/limit";

/// Z.AI：Coding Plan 的 5 小时 / 周 / MCP 月窗口。
pub(super) async fn fetch_zai(client: &Client, provider: &str) -> Option<ProviderReport> {
    let token = match token_for(provider).await {
        Ok(Some(token)) => token,
        Ok(None) => return None,
        Err(err) => return Some(ProviderReport::failed(provider, err)),
    };
    let base = login_registry::LOGIN_PROVIDERS
        .iter()
        .find(|p| p.id == provider)
        .map(|p| p.base_url.to_string())
        .unwrap_or_default();
    Some(fetch_zai_at(client, &zai_quota_url(&base), provider, &token).await)
}

/// 由 provider 的 API base 推出配额端点所在的 origin。
///
/// 大陆（`bigmodel.cn`）与国际（`api.z.ai`）的 key 互不通用，端点必须跟着 key 的区域走；
/// 自定义代理等未知 host 按国际站处理。
pub(super) fn zai_quota_url(api_base: &str) -> String {
    let host = url::Url::parse(api_base)
        .ok()
        .and_then(|url| url.host_str().map(str::to_ascii_lowercase));
    let origin = match host.as_deref() {
        Some(host) if host == "bigmodel.cn" || host.ends_with(".bigmodel.cn") => {
            "https://open.bigmodel.cn"
        }
        _ => "https://api.z.ai",
    };
    format!("{origin}{ZAI_QUOTA_PATH}")
}

/// [`fetch_zai`] 的可注入端点版本（测试用）。
pub(super) async fn fetch_zai_at(
    client: &Client,
    url: &str,
    provider: &str,
    token: &str,
) -> ProviderReport {
    let headers = vec![("Authorization".to_string(), format!("Bearer {token}"))];
    match get_json(client, url, &headers).await {
        Ok(json) => {
            let mut report = ProviderReport::new(provider);
            report.windows = parse_zai(&json);
            if report.windows.is_empty() {
                report.error = Some("no Coding Plan quota (pay-as-you-go key?)".to_string());
            }
            report
        }
        Err(err) => ProviderReport::failed(provider, err),
    }
}

/// 解析 Z.AI 配额响应：`TOKENS_LIMIT`（unit 6 = 周，其余 = 5 小时）与 `TIME_LIMIT`（MCP 月）。
pub(super) fn parse_zai(json: &Value) -> Vec<QuotaWindow> {
    let root = json
        .get("data")
        .filter(|data| data.is_object())
        .unwrap_or(json);
    let Some(limits) = root.get("limits").and_then(Value::as_array) else {
        return Vec::new();
    };

    let mut windows = Vec::new();
    for limit in limits {
        let resets_at = ["nextResetTime", "resetAt", "reset_time"]
            .iter()
            .find_map(|key| limit.get(*key).and_then(parse_reset_ms));
        let percent = limit.get("percentage").and_then(as_f64).map(clamp_percent);
        let (name, percent) = match limit.get("type").and_then(Value::as_str) {
            Some("TOKENS_LIMIT") => {
                let name = if limit.get("unit").and_then(as_f64) == Some(6.0) {
                    "Weekly"
                } else {
                    "5-hour"
                };
                let Some(percent) = percent else { continue };
                (name.to_string(), percent)
            }
            Some("TIME_LIMIT") => {
                let percent = percent.or_else(|| {
                    used_limit_percent(
                        limit.get("currentValue").and_then(as_f64).unwrap_or(0.0),
                        limit.get("usage").and_then(as_f64).unwrap_or(0.0),
                    )
                });
                let Some(percent) = percent else { continue };
                ("MCP monthly".to_string(), percent)
            }
            _ => continue,
        };
        windows.push(QuotaWindow {
            name,
            percent,
            resets_at,
        });
    }

    windows.sort_by_key(|window| match window.name.as_str() {
        "5-hour" => 0,
        "Weekly" => 1,
        _ => 2,
    });
    windows
}

#[cfg(test)]
mod tests {

    use super::*;

    /// `TOKENS_LIMIT`（按 unit 分 5 小时 / 周）与 `TIME_LIMIT`（MCP 月）按序排列。
    #[test]
    fn parse_zai_sorts_windows_and_skips_unknown_types() {
        let json = serde_json::json!({"data": {"limits": [
            {"type": "TIME_LIMIT", "percentage": 10.0, "nextResetTime": 1_700_000_000},
            {"type": "TOKENS_LIMIT", "unit": 6, "percentage": 20.0},
            {"type": "TOKENS_LIMIT", "unit": 3, "percentage": 30.0},
            {"type": "OTHER", "percentage": 40.0}
        ]}});
        let windows = parse_zai(&json);
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0].name, "5-hour");
        assert_eq!(windows[1].name, "Weekly");
        assert_eq!(windows[2].name, "MCP monthly");
        assert_eq!(windows[2].resets_at, Some(1_700_000_000_000));
    }

    /// 配额端点跟着 key 区域走；未知 host 回落国际站。
    #[test]
    fn zai_quota_url_follows_key_region() {
        assert_eq!(
            zai_quota_url("https://api.z.ai/api/coding/paas/v4"),
            "https://api.z.ai/api/monitor/usage/quota/limit"
        );
        assert_eq!(
            zai_quota_url("https://open.bigmodel.cn/api/coding/paas/v4"),
            "https://open.bigmodel.cn/api/monitor/usage/quota/limit"
        );
        assert_eq!(
            zai_quota_url("https://proxy.example/v1"),
            "https://api.z.ai/api/monitor/usage/quota/limit",
            "未知 host 按国际站"
        );
    }
}

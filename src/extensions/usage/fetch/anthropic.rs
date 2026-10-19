//! Anthropic（Claude 订阅）的配额端点：`GET /api/oauth/usage`。
//!
//! 响应里的 `five_hour` / `seven_day` / `seven_day_opus` 是三个固定窗口，`limits[]` 额外给出
//! 带模型作用域的周窗口（`weekly_scoped`）。`utilization` 是比例（`0.42` = 42%），但也见过
//! 百分比写法，[`super::common::ratio_percent`] 两种都吃。

use super::{
    ProviderReport, QuotaWindow,
    common::{as_f64, clamp_percent, parse_reset_ms, ratio_percent},
    token_for,
};
use crate::utils::http::get_json;
use reqwest::Client;
use serde_json::Value;

/// Claude 订阅的用量端点（与 Claude Code 的 `/usage` 同源）。
pub(super) const ANTHROPIC_USAGE_PATH: &str = "/api/oauth/usage";

/// Claude 订阅的 base URL。
pub(super) const ANTHROPIC_BASE: &str = "https://api.anthropic.com";

/// Anthropic：5 小时 / 7 天窗口 + 模型作用域的周窗口。
pub(super) async fn fetch_anthropic(client: &Client) -> Option<ProviderReport> {
    let token = match token_for("anthropic").await {
        Ok(Some(token)) => token,
        Ok(None) => return None,
        Err(err) => return Some(ProviderReport::failed("anthropic", err)),
    };
    Some(fetch_anthropic_at(client, ANTHROPIC_BASE, &token).await)
}

/// [`fetch_anthropic`] 的可注入端点版本（测试用）。
pub(super) async fn fetch_anthropic_at(client: &Client, base: &str, token: &str) -> ProviderReport {
    let url = format!("{base}{ANTHROPIC_USAGE_PATH}");
    let headers = vec![
        ("Authorization".to_string(), format!("Bearer {token}")),
        (
            "User-Agent".to_string(),
            format!(
                "claude-cli/{}",
                crate::core::provider::anthropic::CLAUDE_CODE_VERSION
            ),
        ),
        ("x-app".to_string(), "cli".to_string()),
        (
            "anthropic-beta".to_string(),
            "oauth-2025-04-20,claude-code-20250219".to_string(),
        ),
    ];
    match get_json(client, &url, &headers).await {
        Ok(json) => parse_anthropic(&json),
        Err(err) => ProviderReport::failed("anthropic", err),
    }
}

/// 解析 Claude 用量响应。
pub(super) fn parse_anthropic(json: &Value) -> ProviderReport {
    let mut report = ProviderReport::new("anthropic");

    for (key, name) in [
        ("five_hour", "5-hour"),
        ("seven_day", "7-day"),
        ("seven_day_opus", "7-day Opus"),
    ] {
        if let Some(window) = json.get(key).and_then(Value::as_object)
            && let Some(percent) = window.get("utilization").and_then(as_f64)
        {
            report.windows.push(QuotaWindow {
                name: name.to_string(),
                percent: ratio_percent(percent),
                resets_at: window.get("resets_at").and_then(parse_reset_ms),
            });
        }
    }

    if let Some(limits) = json.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let Some(percent) = limit.get("percent").and_then(as_f64) else {
                continue;
            };
            let model = limit
                .get("scope")
                .and_then(|scope| scope.get("model"))
                .and_then(|model| model.get("display_name"))
                .and_then(Value::as_str);
            let name = match model {
                Some(model) => format!("7-day {model}"),
                None => limit
                    .get("kind")
                    .and_then(Value::as_str)
                    .unwrap_or("scoped")
                    .to_string(),
            };
            report.windows.push(QuotaWindow {
                name,
                percent: clamp_percent(percent),
                resets_at: limit.get("resets_at").and_then(parse_reset_ms),
            });
        }
    }

    if let Some(enabled) = json
        .get("extra_usage")
        .and_then(|extra| extra.get("is_enabled"))
        .and_then(Value::as_bool)
    {
        report.extra.push((
            "Extra usage".to_string(),
            if enabled { "enabled" } else { "disabled" }.to_string(),
        ));
    }

    report
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::test_support::run_with_timeout;
    use crate::test_support::spawn_json_server;

    /// 三个固定窗口 + `limits[]` 的模型周窗口都进报告，比例与百分比写法都吃。
    #[test]
    fn parse_anthropic_reads_windows_and_model_scopes() {
        let json = serde_json::json!({
            "five_hour": {"utilization": 26.0, "resets_at": "2026-11-01T00:00:00Z"},
            "seven_day": {"utilization": 0.5},
            "seven_day_opus": {"utilization": 100.0},
            "limits": [
                {"kind": "weekly_scoped", "percent": 61.0,
                 "scope": {"model": {"display_name": "Fable"}}},
                {"kind": "weekly_scoped", "percent": 5.0}
            ],
            "extra_usage": {"is_enabled": true}
        });
        let report = parse_anthropic(&json);
        assert_eq!(report.provider, "anthropic");
        assert_eq!(report.label, "Anthropic");
        assert_eq!(report.windows.len(), 5);
        assert_eq!(report.windows[0].name, "5-hour");
        assert_eq!(report.windows[0].percent, 26.0);
        assert_eq!(report.windows[0].resets_at, Some(1_793_491_200_000));
        assert_eq!(report.windows[1].percent, 50.0, "0.5 视为比例");
        assert_eq!(report.windows[2].name, "7-day Opus");
        assert_eq!(report.windows[3].name, "7-day Fable");
        assert_eq!(
            report.windows[4].name, "weekly_scoped",
            "无模型名回退到 kind"
        );
        assert!(
            report
                .extra
                .contains(&("Extra usage".to_string(), "enabled".to_string()))
        );
        assert_eq!(report.error, None);
    }

    /// 空响应不造假窗口：该 provider 在面板上会显示成「没有窗口」。
    #[test]
    fn parse_anthropic_without_fields_yields_no_windows() {
        let report = parse_anthropic(&serde_json::json!({}));
        assert!(report.windows.is_empty());
        assert!(report.extra.is_empty());
        assert_eq!(report.error, None);
    }

    /// 不联网：本地假服务驱动一次完整的请求 → 解析往返。
    #[test]
    fn anthropic_endpoint_roundtrip_uses_local_server() {
        run_with_timeout(std::time::Duration::from_secs(30), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let body = serde_json::json!({
                    "five_hour": {"utilization": 26.0, "resets_at": "2026-11-01T00:00:00Z"},
                    "seven_day": {"utilization": 10.0}
                })
                .to_string();
                let (base, handle) = spawn_json_server(vec![(200, body)]).await;
                let client = reqwest::Client::new();
                let report = fetch_anthropic_at(&client, &base, "token").await;
                handle.abort();

                assert_eq!(report.provider, "anthropic");
                assert_eq!(report.error, None);
                assert_eq!(report.windows.len(), 2);
                assert_eq!(report.windows[0].percent, 26.0);
            });
        });
    }

    /// 非 2xx：错误消息里带状态码与响应体摘要。
    #[test]
    fn endpoint_errors_surface_status_and_body() {
        run_with_timeout(std::time::Duration::from_secs(30), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let (base, handle) =
                    spawn_json_server(vec![(401, "{\"error\":\"bad token\"}".to_string())]).await;
                let client = reqwest::Client::new();
                let report = fetch_anthropic_at(&client, &base, "token").await;
                handle.abort();

                let error = report.error.expect("应报错");
                assert!(error.contains("401"), "{error}");
                assert!(error.contains("bad token"), "{error}");
                assert!(report.windows.is_empty());
            });
        });
    }
}

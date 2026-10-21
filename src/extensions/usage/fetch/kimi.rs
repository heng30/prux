//! Kimi for Coding 的配额端点：`GET https://api.kimi.com/coding/v1/usages`。
//!
//! 后端在不同时期发过两种形状：带 `window` 描述的 `limits[]`，以及按 `limit_5h` / `limit_7d` /
//! `limit_30d` 命名的 `usages` 映射。两种都解析，窗口名由时长统一推出来。

use super::{
    ProviderReport, QuotaWindow,
    common::{as_f64, clamp_percent, parse_reset_ms, used_limit_percent, window_label},
    token_for,
};
use crate::utils::http::get_json;
use reqwest::Client;
use serde_json::Value;

/// Kimi for Coding 的配额端点。
pub(super) const KIMI_USAGE_URL: &str = "https://api.kimi.com/coding/v1/usages";

/// Kimi for Coding：5 小时 / 周 / 月窗口。
pub(super) async fn fetch_kimi(client: &Client) -> Option<ProviderReport> {
    let token = match token_for("kimi-coding").await {
        Ok(Some(token)) => token,
        Ok(None) => return None,
        Err(err) => return Some(ProviderReport::failed("kimi-coding", err)),
    };
    Some(fetch_kimi_at(client, KIMI_USAGE_URL, &token).await)
}

/// [`fetch_kimi`] 的可注入端点版本（测试用）。
pub(super) async fn fetch_kimi_at(client: &Client, url: &str, token: &str) -> ProviderReport {
    let headers = vec![("Authorization".to_string(), format!("Bearer {token}"))];
    match get_json(client, url, &headers).await {
        Ok(json) => {
            let mut report = ProviderReport::new("kimi-coding");
            report.windows = parse_kimi(&json);
            if report.windows.is_empty() {
                report.error = Some("no quota windows returned".to_string());
            }
            report
        }
        Err(err) => ProviderReport::failed("kimi-coding", err),
    }
}

/// 解析 Kimi 配额响应：新版 `limits[]`（带 `window` 描述）与旧版 `usages` 映射都接受。
pub(super) fn parse_kimi(json: &Value) -> Vec<QuotaWindow> {
    let mut windows = Vec::new();

    if let Some(limits) = json.get("limits").and_then(Value::as_array) {
        for limit in limits {
            let minutes = limit.get("window").and_then(|window| {
                let duration = window.get("duration").and_then(as_f64)?;
                let unit = window
                    .get("timeUnit")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_ascii_uppercase();
                Some(if unit.contains("HOUR") {
                    duration * 60.0
                } else if unit.contains("DAY") {
                    duration * 1440.0
                } else {
                    duration
                })
            });
            let fallback = format!("Limit {}", windows.len() + 1);
            // `window_label` 按秒分档，这里先把手里的分钟换算回秒
            let name = minutes
                .map(|minutes| window_label(minutes * 60.0))
                .unwrap_or(fallback);
            let detail = limit.get("detail").unwrap_or(limit);
            if let Some(window) = kimi_window(detail, &name) {
                windows.push(window);
            }
        }
    }

    if let Some(usages) = json.get("usages").and_then(Value::as_object) {
        // 显式按键名排序：窗口顺序不该取决于 JSON 对象的内部存储顺序
        // （`serde_json` 的 `Map` 在不同 feature 下可能是 BTreeMap 或 IndexMap）。
        let mut entries: Vec<(&String, &Value)> = usages.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (key, value) in entries {
            let minutes = match key.as_str() {
                "limit_5h" => Some(300.0),
                "limit_7d" => Some(10080.0),
                "limit_30d" => Some(43200.0),
                _ => None,
            };
            let name = minutes
                .map(|minutes| window_label(minutes * 60.0))
                .unwrap_or_else(|| key.clone());
            if let Some(window) = kimi_window(value, &name) {
                windows.push(window);
            }
        }
    }

    windows
}

/// 解析 Kimi 单个窗口行：`percentage` 优先，其次 `used`/`limit`，再次 `used_ratio`。
pub(super) fn kimi_window(raw: &Value, name: &str) -> Option<QuotaWindow> {
    let limit = raw.get("limit").and_then(as_f64);
    let used = raw.get("used").and_then(as_f64).or_else(|| {
        match (limit, raw.get("remaining").and_then(as_f64)) {
            (Some(limit), Some(remaining)) => Some((limit - remaining).max(0.0)),
            _ => None,
        }
    });
    let percent = raw
        .get("percentage")
        .and_then(as_f64)
        .map(clamp_percent)
        .or_else(|| used_limit_percent(used.unwrap_or(0.0), limit.unwrap_or(0.0)))
        .or_else(|| {
            raw.get("used_ratio")
                .and_then(as_f64)
                .map(|ratio| clamp_percent(ratio * 100.0))
        })?;
    Some(QuotaWindow {
        name: name.to_string(),
        percent,
        resets_at: ["resetTime", "resetAt", "reset_time"]
            .iter()
            .find_map(|key| raw.get(*key).and_then(parse_reset_ms)),
    })
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::test_support::run_with_timeout;
    use crate::test_support::spawn_json_server;

    /// 新版 `limits[]` 与旧版 `usages` 映射都解析，窗口名由时长推出。
    #[test]
    fn parse_kimi_accepts_both_shapes() {
        let json = serde_json::json!({
            "limits": [
                {"window": {"duration": 5, "timeUnit": "HOUR"}, "detail": {"used": 5, "limit": 10}},
                {"window": {"duration": 7, "timeUnit": "DAY"}, "detail": {"percentage": 30.0}}
            ],
            "usages": {
                "limit_5h": {"used_ratio": 0.4},
                "limit_30d": {"used": 1, "limit": 4, "reset_time": 1_700_000_000}
            }
        });
        let windows = parse_kimi(&json);
        assert_eq!(windows.len(), 4);
        // `limits[]` 按给定顺序，`usages` 映射按 key 名显式排序（limit_30d 在 limit_5h 前）
        assert_eq!(windows[0].name, "5-hour");
        assert_eq!(windows[0].percent, 50.0);
        assert_eq!(windows[1].name, "Weekly");
        assert_eq!(windows[1].percent, 30.0);
        assert_eq!(windows[2].name, "Monthly", "旧版 usages 映射: limit_30d");
        assert_eq!(windows[2].percent, 25.0);
        assert_eq!(windows[2].resets_at, Some(1_700_000_000_000));
        assert_eq!(windows[3].name, "5-hour", "旧版 usages 映射: limit_5h");
        assert_eq!(windows[3].percent, 40.0);
    }

    /// 返回体没有窗口时明确报错，而不是画一个空面板。
    #[test]
    fn kimi_without_windows_reports_error() {
        run_with_timeout(std::time::Duration::from_secs(30), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let (base, handle) = spawn_json_server(vec![(200, "{}".to_string())]).await;
                let client = reqwest::Client::new();
                let report = fetch_kimi_at(&client, &base, "token").await;
                handle.abort();

                assert_eq!(report.error.as_deref(), Some("no quota windows returned"));
            });
        });
    }
}

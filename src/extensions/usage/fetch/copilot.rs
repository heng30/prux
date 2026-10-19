//! GitHub Copilot 的配额来源：内部 token 端点 `GET copilot_internal/v2/token`。
//!
//! 这个端点本来是用来换 Copilot token 的，响应里顺便带了 `limited_user_quotas`（各功能的
//! 已用/上限）与 `limited_user_reset_date`。认证用的是 **GitHub token**（存在 OAuth 凭据的
//! `refresh` 字段里），不是 Copilot token 本身。

use super::{
    ProviderReport, QuotaWindow,
    common::{as_f64, humanize_key, parse_reset_ms, used_limit_percent},
};
use crate::{
    core::{auth, model_resolver},
    utils::http::get_json,
};
use reqwest::Client;
use serde_json::Value;

/// Copilot 内部 token 端点路径（响应里带 `limited_user_quotas`）。
pub(super) const COPILOT_TOKEN_PATH: &str = "/copilot_internal/v2/token";

/// Copilot：从内部 token 端点读各功能的已用/上限与重置日期。
pub(super) async fn fetch_copilot(client: &Client) -> Option<ProviderReport> {
    let cred = auth::read_oauth_credential("github-copilot")?;
    // Copilot 的 `refresh` 存的是 GitHub token，内部 token 端点要的正是它。
    let github_token = cred.refresh.clone();
    if github_token.trim().is_empty() {
        return None;
    }
    let domain = cred
        .enterprise_url
        .clone()
        .unwrap_or_else(|| "github.com".to_string());
    Some(fetch_copilot_at(client, &domain, &github_token).await)
}

/// [`fetch_copilot`] 的可注入端点版本（测试用）。
pub(super) async fn fetch_copilot_at(
    client: &Client,
    domain: &str,
    github_token: &str,
) -> ProviderReport {
    let url = format!("https://api.{domain}{COPILOT_TOKEN_PATH}");
    let mut headers = vec![("Authorization".to_string(), format!("token {github_token}"))];
    // 网关头（User-Agent / Editor-Version / Copilot-Integration-Id）来自模型目录，
    // 与 provider 层请求 Copilot 时用的是同一份，不在这里另写死一份。
    headers.extend(model_resolver::provider_headers("github-copilot"));
    match get_json(client, &url, &headers).await {
        Ok(json) => parse_copilot(&json),
        Err(err) => ProviderReport::failed("github-copilot", err),
    }
}

/// 解析 Copilot 内部 token 响应。
pub(super) fn parse_copilot(json: &Value) -> ProviderReport {
    let mut report = ProviderReport::new("github-copilot");

    if let Some(sku) = json.get("sku").and_then(Value::as_str) {
        report.extra.push(("Plan".to_string(), sku.to_string()));
    }

    let reset = json.get("limited_user_reset_date").and_then(parse_reset_ms);

    if let Some(quotas) = json.get("limited_user_quotas").and_then(Value::as_object) {
        for (name, value) in quotas {
            let Some(object) = value.as_object() else {
                continue;
            };
            let used = object.get("used").and_then(as_f64).unwrap_or(0.0);
            let limit = object.get("limit").and_then(as_f64).unwrap_or(0.0);
            let Some(percent) = used_limit_percent(used, limit) else {
                continue;
            };
            report.windows.push(QuotaWindow {
                name: humanize_key(name),
                percent,
                resets_at: reset,
            });
            report.extra.push((
                humanize_key(name),
                format!("{} / {} used", used as u64, limit as u64),
            ));
        }
    }

    report
}

#[cfg(test)]
mod tests {

    use super::*;

    /// 各功能的已用/上限转成窗口，上限为 0 的不画条。
    #[test]
    fn parse_copilot_reads_quotas_and_reset_date() {
        let json = serde_json::json!({
            "sku": "copilot_pro",
            "limited_user_reset_date": "2026-11-01",
            "limited_user_quotas": {
                "chat": {"used": 5, "limit": 10},
                "premium_requests": {"used": 0, "limit": 0}
            }
        });
        let report = parse_copilot(&json);
        assert_eq!(report.windows.len(), 1, "limit=0 的窗口不画条");
        assert_eq!(report.windows[0].name, "Chat");
        assert_eq!(report.windows[0].percent, 50.0);
        assert_eq!(report.windows[0].resets_at, Some(1_793_491_200_000));
        assert!(
            report
                .extra
                .contains(&("Plan".to_string(), "copilot_pro".to_string()))
        );
    }
}

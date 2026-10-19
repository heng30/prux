//! `usage` 解析层的共用工具：数值 / 百分比归一化、重置时间解析、窗口名与键名格式化。
//!
//! 这些函数是各 provider 解析器（[`super`] 下的兄弟模块）的共同底座，全部是纯函数。

use crate::utils::time;
use serde_json::Value;

/// JSON 数值或数值字符串 → `f64`（protobuf 的 int64 会序列化成字符串）。
pub(super) fn as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
}

/// JSON 值 → 展示文本（数字去掉多余的 `.0`）。
pub(super) fn value_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    value.as_f64().map(|number| {
        if number.fract() == 0.0 {
            format!("{}", number as i64)
        } else {
            format!("{number}")
        }
    })
}

/// 百分比裁剪到 `0.0..=100.0`。
pub(super) fn clamp_percent(percent: f64) -> f32 {
    if percent.is_finite() {
        percent.clamp(0.0, 100.0) as f32
    } else {
        0.0
    }
}

/// 比例或百分比 → 百分比：`<= 1` 视为比例（`0.42` → `42%`），否则按百分比处理。
pub(super) fn ratio_percent(value: f64) -> f32 {
    if value <= 1.0 {
        clamp_percent(value * 100.0)
    } else {
        clamp_percent(value)
    }
}

/// `used / limit` → 百分比；`limit` 非正时返回 `None`（端点没给上限，画不出进度条）。
pub(super) fn used_limit_percent(used: f64, limit: f64) -> Option<f32> {
    if !limit.is_finite() || limit <= 0.0 {
        return None;
    }
    Some(clamp_percent((used.max(0.0) / limit) * 100.0))
}

/// 从对象里挑一个百分比字段（`usage_percent` 类按 0..100，`usage_ratio` 类按 0..1）。
pub(super) fn percent_from_object(object: &serde_json::Map<String, Value>) -> Option<f32> {
    for key in ["usage_percent", "used_percent", "percent_used"] {
        if let Some(value) = object.get(key).and_then(as_f64) {
            return Some(clamp_percent(value));
        }
    }
    for key in ["usage", "utilization", "usage_ratio", "used_ratio"] {
        if let Some(value) = object.get(key).and_then(as_f64) {
            return Some(ratio_percent(value));
        }
    }
    let used = object.get("used").and_then(as_f64);
    let limit = object
        .get("limit")
        .or_else(|| object.get("max"))
        .and_then(as_f64);
    if let (Some(used), Some(limit)) = (used, limit) {
        return used_limit_percent(used, limit);
    }
    let remaining = object.get("remaining").and_then(as_f64);
    if let (Some(remaining), Some(limit)) = (remaining, limit) {
        return used_limit_percent(limit - remaining, limit);
    }
    None
}

/// 从对象里挑一个重置时间字段。
pub(super) fn resets_at_from_object(object: &serde_json::Map<String, Value>) -> Option<u64> {
    [
        "resets_at",
        "reset_at",
        "resetsAt",
        "resetAt",
        "reset_time",
        "resetTime",
        "nextResetTime",
    ]
    .iter()
    .find_map(|key| object.get(*key).and_then(parse_reset_ms))
}

/// 重置时刻 → Unix 毫秒。
///
/// 端点给出的形状很杂：RFC3339 字符串（可能带时区偏移）、epoch 秒 / 毫秒（数字或数字字符串）、
/// 纯日期（`2026-11-01`）。一律归一成毫秒；解析不出来返回 `None`。
pub(super) fn parse_reset_ms(value: &Value) -> Option<u64> {
    if let Some(text) = value.as_str() {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return None;
        }
        if let Some(epoch) = trimmed.parse::<f64>().ok().and_then(epoch_to_ms) {
            return Some(epoch);
        }
        if let Some(ms) = time::parse_timestamp_ms(trimmed) {
            return Some(ms);
        }
        // 纯日期：补成当天 UTC 零点
        let date_only = format!("{trimmed}T00:00:00Z");
        return time::parse_timestamp_ms(&date_only);
    }
    value.as_f64().and_then(epoch_to_ms)
}

/// epoch 秒 / 毫秒 → 毫秒（大于 1e12 的按毫秒算；非正数视为未知）。
pub(super) fn epoch_to_ms(epoch: f64) -> Option<u64> {
    if !epoch.is_finite() || epoch <= 0.0 {
        return None;
    }
    let ms = if epoch > 1.0e12 {
        epoch
    } else {
        epoch * 1000.0
    };
    Some(ms as u64)
}

/// 秒数 → 窗口名（`18000` → `5-hour`，`604800` → `Weekly`，其余按天/小时取整）。
pub(super) fn window_label(seconds: f64) -> String {
    let minutes = seconds / 60.0;
    match minutes {
        300.0 => "5-hour".to_string(),
        10080.0 => "Weekly".to_string(),
        m if (43200.0..=44640.0).contains(&m) => "Monthly".to_string(),
        m if m >= 1440.0 && m % 1440.0 == 0.0 => format!("{}d", m / 1440.0),
        m if m >= 60.0 && m % 60.0 == 0.0 => format!("{}h", m / 60.0),
        m if m >= 1.0 => format!("{}m", m),
        _ => format!("{}s", seconds),
    }
}

/// `premium_requests` / `chat` 这类下划线键 → `Premium requests` / `Chat`。
pub(super) fn humanize_key(key: &str) -> String {
    let mut out = String::with_capacity(key.len());
    let mut upper = true;
    for ch in key.chars() {
        if ch == '_' || ch == '-' {
            out.push(' ');
            upper = true;
        } else if upper {
            out.extend(ch.to_uppercase());
            upper = false;
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {

    use super::*;

    /// 百分比裁剪、比例归一化、`used/limit` 换算的边界。
    #[test]
    fn percent_helpers_clamp_and_normalize() {
        assert_eq!(clamp_percent(150.0), 100.0);
        assert_eq!(clamp_percent(-5.0), 0.0);
        assert_eq!(clamp_percent(f64::NAN), 0.0);
        // 比例 vs 百分比
        assert_eq!(ratio_percent(0.42), 42.0);
        assert_eq!(ratio_percent(42.0), 42.0);
        assert_eq!(ratio_percent(1.0), 100.0);
        // used/limit
        assert_eq!(used_limit_percent(5.0, 10.0), Some(50.0));
        assert_eq!(used_limit_percent(5.0, 0.0), None, "无上限画不出进度条");
        assert_eq!(used_limit_percent(-1.0, 10.0), Some(0.0));
    }

    /// RFC3339、epoch 秒 / 毫秒（数字或字符串）、纯日期都归一成毫秒。
    #[test]
    fn epoch_and_reset_times_normalize_to_ms() {
        assert_eq!(
            parse_reset_ms(&serde_json::json!(1_700_000_000)),
            Some(1_700_000_000_000)
        );
        assert_eq!(
            parse_reset_ms(&serde_json::json!(1_700_000_000_000u64)),
            Some(1_700_000_000_000),
            "大于 1e12 按毫秒算"
        );
        assert_eq!(
            parse_reset_ms(&serde_json::json!("1700000000")),
            Some(1_700_000_000_000),
            "数字字符串按 epoch 秒"
        );
        assert_eq!(
            parse_reset_ms(&serde_json::json!("2026-11-01T00:00:00Z")),
            Some(1_793_491_200_000)
        );
        assert_eq!(
            parse_reset_ms(&serde_json::json!("2026-11-01T08:00:00+08:00")),
            Some(1_793_491_200_000),
            "带时区偏移的写法按偏移换算，不能当成本地钟点"
        );
        assert_eq!(
            parse_reset_ms(&serde_json::json!("2026-11-01")),
            Some(1_793_491_200_000),
            "纯日期补成当天零点"
        );
        assert_eq!(parse_reset_ms(&serde_json::json!(0)), None);
        assert_eq!(parse_reset_ms(&serde_json::json!("")), None);
        assert_eq!(parse_reset_ms(&serde_json::json!("not a time")), None);
        assert_eq!(parse_reset_ms(&serde_json::Value::Null), None);
    }

    /// 秒数 → 窗口名的常见档位。
    #[test]
    fn window_labels_cover_common_durations() {
        assert_eq!(window_label(18_000.0), "5-hour");
        assert_eq!(window_label(604_800.0), "Weekly");
        assert_eq!(window_label(2_592_000.0), "Monthly");
        assert_eq!(window_label(86_400.0), "1d");
        assert_eq!(window_label(7_200.0), "2h");
        assert_eq!(window_label(90.0), "1.5m");
        assert_eq!(window_label(30.0), "30s");
    }

    /// 百分比字段优先于比例字段，其次用 `used`/`limit`、`remaining`/`limit` 推算。
    #[test]
    fn percent_from_object_prefers_explicit_percent_fields() {
        let object = serde_json::json!({"usage_percent": 12.0, "used_ratio": 0.5});
        assert_eq!(percent_from_object(object.as_object().unwrap()), Some(12.0));
        let object = serde_json::json!({"used_ratio": 0.5});
        assert_eq!(percent_from_object(object.as_object().unwrap()), Some(50.0));
        let object = serde_json::json!({"used": 3, "limit": 4});
        assert_eq!(percent_from_object(object.as_object().unwrap()), Some(75.0));
        let object = serde_json::json!({"remaining": 1, "limit": 4});
        assert_eq!(percent_from_object(object.as_object().unwrap()), Some(75.0));
        let object = serde_json::json!({"nothing": 1});
        assert_eq!(percent_from_object(object.as_object().unwrap()), None);
    }

    /// 下划线 / 连字符键名转成标题式展示文本。
    #[test]
    fn humanize_key_splits_separators() {
        assert_eq!(humanize_key("premium_requests"), "Premium Requests");
        assert_eq!(humanize_key("chat-messages"), "Chat Messages");
        assert_eq!(humanize_key("sku"), "Sku");
    }
}

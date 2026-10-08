//! 共享的用量结构解析与成本计算

use super::{Cost, Usage};
use serde_json::{Value, json};

/// 两份用量逐项相加（对均为 `None` 的可选字段保持 `None`）：
/// 用于把嵌套/分类器开销并入调用方结果，或合并同一账本内的多笔开销。
pub fn combine_usage(a: &Usage, b: &Usage) -> Usage {
    Usage {
        input: a.input.saturating_add(b.input),
        output: a.output.saturating_add(b.output),
        cache_read: a.cache_read.saturating_add(b.cache_read),
        cache_write: a.cache_write.saturating_add(b.cache_write),
        cache_write_1h: match (a.cache_write_1h, b.cache_write_1h) {
            (None, None) => None,
            (x, y) => Some(x.unwrap_or(0).saturating_add(y.unwrap_or(0))),
        },
        reasoning: match (a.reasoning, b.reasoning) {
            (None, None) => None,
            (x, y) => Some(x.unwrap_or(0).saturating_add(y.unwrap_or(0))),
        },
        total_tokens: a.total_tokens.saturating_add(b.total_tokens),
        cost: Cost {
            input: a.cost.input + b.cost.input,
            output: a.cost.output + b.cost.output,
            cache_read: a.cost.cache_read + b.cost.cache_read,
            cache_write: a.cost.cache_write + b.cost.cache_write,
            total: a.cost.total + b.cost.total,
        },
    }
}

/// 每百万 token 价格、分层 tier、Anthropic 1h cache write 双倍计费。
pub fn compute_cost(usage: &mut Usage, cost_cfg: Option<&Value>) {
    let Some(cost) = cost_cfg else {
        usage.cost = Cost::default();
        return;
    };

    let input_tokens = usage.input + usage.cache_read + usage.cache_write;
    let mut rates = cost;
    let mut matched_threshold: f64 = -1.0;
    if let Some(tiers) = cost.get("tiers").and_then(|v| v.as_array()) {
        for tier in tiers {
            let above = tier
                .get("inputTokensAbove")
                .and_then(|v| v.as_f64())
                .unwrap_or(-1.0);
            if input_tokens as f64 > above && above > matched_threshold {
                rates = tier;
                matched_threshold = above;
            }
        }
    }

    let i_price = cost_rate(rates, "input");
    let o_price = cost_rate(rates, "output");
    let cr_price = cost_rate(rates, "cacheRead");
    let cw_price = cost_rate(rates, "cacheWrite");

    let long_write = usage.cache_write_1h.unwrap_or(0);
    let short_write = usage.cache_write.saturating_sub(long_write);
    let i = usage.input as f64 / 1e6 * i_price;
    let o = usage.output as f64 / 1e6 * o_price;
    let cr = usage.cache_read as f64 / 1e6 * cr_price;
    let cw = (cw_price * short_write as f64 + i_price * 2.0 * long_write as f64) / 1e6;
    usage.cost = Cost {
        input: i,
        output: o,
        cache_read: cr,
        cache_write: cw,
        total: i + o + cr + cw,
    };
}

/// OpenAI 服务层级定价倍数。`fast` 是 priority 的新叫法，
/// GPT-6 系列即使请求 priority 也会返回 `fast`，落到默认值就会按标准价计费。
pub fn service_tier_cost_multiplier(model_id: &str, service_tier: Option<&str>) -> f64 {
    match service_tier {
        Some("flex") => 0.5,
        Some("priority") | Some("fast") => {
            if model_id == "gpt-5.5" {
                2.5
            } else {
                2.0
            }
        }
        _ => 1.0,
    }
}

/// 按服务层级缩放已算好的成本（1x 时不动）。
pub fn apply_service_tier_cost(usage: &mut Usage, model_id: &str, service_tier: Option<&str>) {
    let multiplier = service_tier_cost_multiplier(model_id, service_tier);
    if multiplier == 1.0 {
        return;
    }

    usage.cost.input *= multiplier;
    usage.cost.output *= multiplier;
    usage.cost.cache_read *= multiplier;
    usage.cost.cache_write *= multiplier;
    usage.cost.total =
        usage.cost.input + usage.cost.output + usage.cost.cache_read + usage.cost.cache_write;
}

/// 流式 JSON 解析：尝试完整解析，失败则尝试部分解析
pub(crate) fn parse_streaming_json(s: &str) -> Value {
    if s.trim().is_empty() {
        return json!({});
    }
    if let Ok(v) = serde_json::from_str::<Value>(s) {
        return v;
    }
    // 部分解析：找到最大前缀的平衡括号
    partial_parse(s).unwrap_or(json!({}))
}

/// 从后往前逐步截断，返回最长的可完整解析 JSON 前缀；没有任何前缀能解析时为 `None`。
fn partial_parse(s: &str) -> Option<Value> {
    // 尝试递增截断找到可解析前缀
    let chars: Vec<char> = s.chars().collect();
    for cut in (1..=chars.len()).rev() {
        let candidate: String = chars[..cut].iter().collect();
        if let Ok(v) = serde_json::from_str::<Value>(&candidate) {
            return Some(v);
        }
    }
    None
}

/// 从 cost 对象取指定费率字段；字段缺失或不是数值时按 0.0 计。
fn cost_rate(cost: &Value, key: &str) -> f64 {
    cost.get(key).and_then(|v| v.as_f64()).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cost_uses_usage_split_and_tiers() {
        let mut usage = Usage {
            input: 500,
            output: 200,
            cache_read: 100,
            cache_write: 50,
            cache_write_1h: Some(20),
            total_tokens: 850,
            cost: Cost::default(),
            reasoning: None,
        };
        let cost = json!({
            "input": 1.0,
            "output": 2.0,
            "cacheRead": 0.5,
            "cacheWrite": 0.25,
            "tiers": [{
                "inputTokensAbove": 400,
                "input": 0.5,
                "output": 1.0,
                "cacheRead": 0.2,
                "cacheWrite": 0.1
            }]
        });
        compute_cost(&mut usage, Some(&cost));
        assert!((usage.cost.input - 0.00025).abs() < 1e-12);
        assert!((usage.cost.output - 0.0002).abs() < 1e-12);
        // tier 生效，1h write = 2 * tier.input 单价
        assert!((usage.cost.cache_write - (0.1 * 30.0 + 0.5 * 2.0 * 20.0) / 1e6).abs() < 1e-12);
        // 无 cost 配置 → 清零
        compute_cost(&mut usage, None);
        assert_eq!(usage.cost.total, 0.0);
    }

    /// pi #10034：GPT-6 系列返回 `service_tier: "fast"`（priority 的新叫法），
    /// 未识别就会落到 1x 默认值。
    #[test]
    fn service_tier_multiplier_matches_pi() {
        assert_eq!(
            service_tier_cost_multiplier("gpt-6-luna", Some("fast")),
            2.0
        );
        assert_eq!(
            service_tier_cost_multiplier("gpt-6-luna", Some("priority")),
            2.0
        );
        assert_eq!(service_tier_cost_multiplier("gpt-5.5", Some("fast")), 2.5);
        assert_eq!(service_tier_cost_multiplier("gpt-5.5", Some("flex")), 0.5);
        assert_eq!(
            service_tier_cost_multiplier("gpt-6-luna", Some("default")),
            1.0
        );
        assert_eq!(service_tier_cost_multiplier("gpt-6-luna", None), 1.0);

        let mut usage = Usage {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write: 1_000_000,
            cache_write_1h: None,
            total_tokens: 4_000_000,
            cost: Cost::default(),
            reasoning: None,
        };
        let cost = json!({ "input": 2.0, "output": 10.0, "cacheRead": 0.1, "cacheWrite": 2.5 });
        compute_cost(&mut usage, Some(&cost));
        apply_service_tier_cost(&mut usage, "gpt-6-luna", Some("fast"));
        assert!((usage.cost.input - 4.0).abs() < 1e-12);
        assert!((usage.cost.output - 20.0).abs() < 1e-12);
        assert!((usage.cost.cache_read - 0.2).abs() < 1e-12);
        assert!((usage.cost.cache_write - 5.0).abs() < 1e-12);
        assert!((usage.cost.total - 29.2).abs() < 1e-12);

        // 标准层级：成本不变
        let mut std_usage = usage.clone();
        std_usage.cost = Cost::default();
        compute_cost(&mut std_usage, Some(&cost));
        let before = std_usage.cost.total;
        apply_service_tier_cost(&mut std_usage, "gpt-6-luna", Some("auto"));
        assert!((std_usage.cost.total - before).abs() < 1e-12);
    }
}

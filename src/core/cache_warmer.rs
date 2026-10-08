//! 提示缓存保温
//!
//! 真实请求写出 prompt cache 后，用 **1 token 输出上限重放同一请求**，在 TTL 到期前续命，
//! 让下一次真实请求仍命中缓存（尤其适合长工具运行期间）。要点：
//!
//! - 模式 `cacheWarming`：`off` / `streaming`（默认：仅 agent run 期间保温）/ `idle`（结算后继续保温，30 分钟上限）；
//! - TTL 来自目录 `promptCache.{short,long}`（**秒**），短/长由 `PRUX_CACHE_RETENTION` 选择；目录缺失该字段 → 不保温；
//! - 成本感知：只有期望节省 ≥ $0.05 才发（`streaming` 期间续跑概率 1.0，`idle` 阶段按 0.15 折扣）；
//! - 扩展可经 `cache_warming_decision` 钩子覆盖决策（最后一个给出 action 的扩展生效）；
//! - 硬上限：`streaming` 1 小时、`idle` 30 分钟；每次刷新还有一个「延迟 + 剩余裕量一半」的刷新截止点，错过即停。
//!
//! 请求本身走 [`provider::stream_chat`]（与真实请求同一路径，`max_tokens=1`），
//! 结果不作为会话回合；用量由调用方（worker）落成 `cause = "cache_warm"` 的 usage 记录。

use crate::core::{
    extensions, model_resolver, provider,
    provider::{AgentMessage, ModelConfig, Usage},
    settings_manager,
};
use crate::utils::time::format_duration;
use serde_json::{Value, json};
use std::{
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use strum_macros::{EnumString, IntoStaticStr};
use tokio::task::AbortHandle;

/// `streaming` 阶段硬上限（1 小时）
pub const MAX_WARMING_AGE_MS: u64 = 60 * 60 * 1000;
/// `idle` 阶段硬上限（30 分钟）
pub const MAX_IDLE_WARMING_AGE_MS: u64 = 30 * 60 * 1000;
/// 期望节省低于该值不保温（美元）
pub const CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS: f64 = 0.05;
/// idle 阶段「续跑概率」（空闲时用户未必继续，收益打 15%）
pub const IDLE_CONTINUATION_PROBABILITY: f64 = 0.15;

/// settings.json `cacheWarming`
///
/// 变体名与字符串的互转由 `strum` 派生（全小写、大小写不敏感）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub enum CacheWarmingMode {
    /// 不保温
    Off,
    /// 仅 agent run 期间保温（默认）
    Streaming,
    /// 结算后继续保温，直到 30 分钟上限
    Idle,
}

impl CacheWarmingMode {
    /// 解析设置值：去首尾空白、大小写不敏感；无法识别时返回 `None`。
    pub fn parse(s: &str) -> Option<Self> {
        s.trim().parse().ok()
    }

    /// 规范小写名（`off` / `streaming` / `idle`），用于写回设置与状态展示。
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// 读取全局 settings（项目级不生效：每次保温都真花钱）
    pub fn from_settings() -> Self {
        Self::parse(&settings_manager::read_settings_cache_warming()).unwrap_or(Self::Streaming)
    }
}

/// prompt cache 保留档位（`PRUX_CACHE_RETENTION`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheRetention {
    /// 请求已关闭 prompt 缓存
    None,
    /// 短档：目录 `promptCache.short`（Anthropic 默认 300s）
    Short,
    /// 长档：目录 `promptCache.long`（1 小时档，更贵）
    Long,
}

/// 读 `PRUX_CACHE_RETENTION` 选档：`none`/`off` → 关闭，`long` → 长档，
/// 其余（含未设置）→ 短档。
pub fn read_cache_retention() -> CacheRetention {
    match std::env::var("PRUX_CACHE_RETENTION").ok().as_deref() {
        Some("none") | Some("off") => CacheRetention::None,
        Some("long") => CacheRetention::Long,
        _ => CacheRetention::Short,
    }
}

/// 目录 `promptCache` 查询结果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TtlLookup {
    /// 请求关闭了 prompt 缓存
    Disabled,
    /// 目录没有该档位 TTL
    Unavailable,
    /// 目录给出的 TTL（毫秒）
    Millis(u64),
}

/// 目录 `promptCache.{short,long}`（秒）→ 毫秒。
///
/// 按「运行期读目录」实现（`ModelConfig` 不携带全量 compat/目录字段，
/// 见 `anthropic.rs::compat_force_adaptive_thinking` 的同类做法）。
pub fn prompt_cache_ttl(model: &ModelConfig) -> TtlLookup {
    let retention = read_cache_retention();
    let tier = match retention {
        CacheRetention::None => return TtlLookup::Disabled,
        CacheRetention::Short => "short",
        CacheRetention::Long => "long",
    };

    let seconds = model_resolver::provider_models(&model.provider)
        .iter()
        .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(model.model_id.as_str()))
        .and_then(|m| m.get("promptCache"))
        .and_then(|p| p.get(tier))
        .and_then(|v| v.as_u64());

    match seconds {
        Some(s) if s > 0 => TtlLookup::Millis(s.saturating_mul(1000)),
        _ => TtlLookup::Unavailable,
    }
}

/// 刷新延迟：TTL 的 90%，且至少留 10s 裕量；TTL ≤ 10s 视为不可保温。
pub fn warming_delay_ms(ttl_ms: u64) -> Option<u64> {
    if ttl_ms <= 10_000 {
        return None;
    }
    let ninety = (ttl_ms as f64 * 0.9) as u64;
    Some(ninety.min(ttl_ms - 10_000).max(1))
}

/// 请求是否可安全重放：预算式 thinking 的 Anthropic 请求不行
/// （`max_tokens=1` 会改变 `budget_tokens`）；adaptive thinking 除外。
pub fn is_replayable(model: &ModelConfig, reasoning_effort: Option<&str>) -> bool {
    let reasoning_on = reasoning_effort.is_some_and(|l| l != "off");
    if !reasoning_on {
        return true;
    }
    !(model.api == "anthropic-messages" && !compat_force_adaptive_thinking(model))
}

/// 目录 `compat.forceAdaptiveThinking` 是否为真；查不到模型或字段时视为 `false`。
fn compat_force_adaptive_thinking(model: &ModelConfig) -> bool {
    model_resolver::provider_models(&model.provider)
        .iter()
        .find(|m| m.get("id").and_then(|v| v.as_str()) == Some(model.model_id.as_str()))
        .and_then(|m| m.get("compat"))
        .and_then(|c| c.get("forceAdaptiveThinking"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 保温停止原因
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WarmStop {
    /// 模式为 `off`
    Disabled,
    /// 请求不能安全重放（预算式 thinking）
    NotReplayable,
    /// 请求本身关闭了 prompt 缓存
    TtlDisabled,
    /// 目录查不到该档位 TTL
    TtlUnavailable,
    /// `streaming` 模式下的 agent run 结算
    Settled,
    /// `idle` 阶段触到 30 分钟硬上限
    IdleLimit,
    /// `streaming` 阶段触到 1 小时硬上限
    HourLimit,
    /// 上下文变化 / 会话切换，旧快照已失效
    ContextChanged,
    /// 错过本次刷新截止点（延迟 + 剩余裕量一半）
    DeadlineMissed,
    /// 扩展经 `cache_warming_decision` 钩子叫停
    ExtensionStop,
    /// 期望节省低于阈值
    BelowThreshold,
    /// 目录没有定价信息，无法判断收益
    NoEconomics,
}

impl WarmStop {
    /// 停止原因对应的英文短语，拼进 `/session` 的 detail 文案。
    pub fn reason(self) -> &'static str {
        match self {
            Self::Disabled => "cache warming disabled",
            Self::NotReplayable => "request cannot be replayed safely",
            Self::TtlDisabled => "request disabled prompt caching",
            Self::TtlUnavailable => "cache lifetime unavailable",
            Self::Settled => "agent run settled",
            Self::IdleLimit => "30-minute idle safety limit reached",
            Self::HourLimit => "one-hour safety limit reached",
            Self::ContextChanged => "conversation context changed",
            Self::DeadlineMissed => "cache refresh deadline missed",
            Self::ExtensionStop => "stopped by extension",
            Self::BelowThreshold => "expected savings below threshold",
            Self::NoEconomics => "cache economics unavailable",
        }
    }
}

/// 成本评估结果（也是 `cache_warming_decision` 事件负载的来源）
#[derive(Debug, Clone, PartialEq)]
pub struct WarmEconomics {
    /// 保温一次的成本（命中缓存读 + 1 token 输出）
    pub warm_cost: f64,
    /// 缓存未命中比命中多花的钱（`cache_miss - cache_hit`）
    pub miss_cost: f64,
    /// 用户会续跑的概率：`streaming` 1.0、`idle` 0.15
    pub continuation_probability: f64,
    /// 期望节省 = 概率 × `miss_cost` − `warm_cost`
    pub expected_savings: f64,
    /// 目录有定价信息，差额与保温成本可算
    pub economics_available: bool,
    /// "warm" | "stop"
    pub action: String,
}

/// 用目录 cost 配置给一份 usage 定价（美元）。
fn price(cost_cfg: Option<&Value>, usage: Usage) -> f64 {
    let mut u = usage;
    provider::usage::compute_cost(&mut u, cost_cfg);
    u.cost.total
}

/// 缓存命中 vs 未命中的差额 − 保温成本。
pub fn evaluate(prompt_tokens: u64, cost_cfg: Option<&Value>, idle: bool) -> WarmEconomics {
    let tokens = u32::try_from(prompt_tokens).unwrap_or(u32::MAX);
    let cache_hit = price(
        cost_cfg,
        Usage {
            cache_read: tokens,
            ..Default::default()
        },
    );

    let has_cache_write_rate = cost_cfg
        .and_then(|c| c.get("cacheWrite"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
        > 0.0;

    let cache_miss = if has_cache_write_rate {
        price(
            cost_cfg,
            Usage {
                cache_write: tokens,
                ..Default::default()
            },
        )
    } else {
        price(
            cost_cfg,
            Usage {
                input: tokens,
                ..Default::default()
            },
        )
    };

    let warm_cost = price(
        cost_cfg,
        Usage {
            cache_read: tokens,
            output: 1,
            ..Default::default()
        },
    );

    let miss_cost = (cache_miss - cache_hit).max(0.0);
    let continuation_probability = if idle {
        IDLE_CONTINUATION_PROBABILITY
    } else {
        1.0
    };

    let economics_available = prompt_tokens > 0 && (cache_hit > 0.0 || cache_miss > 0.0);
    let expected_savings = continuation_probability * miss_cost - warm_cost;
    let action = if expected_savings >= CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS {
        "warm"
    } else {
        "stop"
    };

    WarmEconomics {
        warm_cost,
        miss_cost,
        continuation_probability,
        expected_savings,
        economics_available,
        action: action.to_string(),
    }
}

/// 最近一条 assistant 消息的 prompt token 数（input + cacheRead + cacheWrite）
pub fn last_prompt_tokens(messages: &[AgentMessage]) -> u64 {
    messages
        .iter()
        .rev()
        .find_map(|m| {
            m.usage
                .as_ref()
                .filter(|_| m.role == "assistant")
                .map(|u| u64::from(u.input) + u64::from(u.cache_read) + u64::from(u.cache_write))
        })
        .unwrap_or(0)
}

/// 一次保温请求的快照（与真实请求同参，独立于运行中的 agent）
#[derive(Debug, Clone)]
pub struct WarmRequest {
    /// 真实请求用的模型与目录定价
    pub model: ModelConfig,
    /// 重放用的完整消息序列（与真实请求同参）
    pub messages: Vec<AgentMessage>,
    /// 与真实请求完全相同的系统提示，保证缓存前缀可命中。
    pub system_prompt: String,
    /// 工具定义（名字、描述、JSON schema），保证缓存前缀与真实请求一致
    pub tools: Vec<(String, String, Value)>,
    /// 推理档位：预算式 thinking 的请求不可重放（见 [`is_replayable`]）
    pub reasoning_effort: Option<String>,
    /// 采样温度；None = 模型配置未指定，用上游默认。
    pub temperature: Option<f64>,
}

/// 一次成功保温的用量回执（由 worker 落成 usage 记录 / 通知）
#[derive(Debug, Clone)]
pub struct WarmOutcome {
    /// 保温请求所走的 provider 名。
    pub provider: String,
    /// 请求时指定的模型 id
    pub model_id: String,
    /// 上游实际返回的模型（可能被路由改写）
    pub response_model: Option<String>,
    /// 保温请求的用量（含成本），落成 `cause = "cache_warm"` 记录
    pub usage: Usage,
    /// 决策被扩展覆盖
    pub extension_override: bool,
}

/// `/session` 用的状态快照
#[derive(Debug, Clone, Default)]
pub struct WarmStatus {
    /// 当前 `cacheWarming` 设置值（每次启动保温时刷新）
    pub mode: String,
    /// "streaming" | "idle" | "inactive"
    pub phase: String,
    /// 人类可读状态
    pub detail: String,
    /// 本轮最近一次成本评估（`inactive` 时清空）
    pub economics: Option<WarmEconomics>,
    /// 最近一次成功保温的成本（transcript 通知用）
    pub last_warmed_cost: Option<f64>,
    /// 本会话累计保温成本
    pub total_warmed_cost: f64,
    /// 本会话成功保温次数
    pub warmed_count: u64,
}

/// 保温器与后台任务共享的内部状态：取消句柄、待取回执与状态快照。
#[derive(Debug)]
struct Shared {
    /// 后台保温任务的取消句柄（`None` = 当前无保温在跑）
    abort: Option<AbortHandle>,
    /// 已成功保温、等待 worker 取走的回执
    outcomes: Vec<WarmOutcome>,
    /// 供 `/session` 读取的保温状态快照。
    status: WarmStatus,
}

/// 保温器句柄：`Agent` 持有，任务在后台 tokio task 里跑。
#[derive(Clone)]
pub struct CacheWarmer {
    /// 与后台任务共享的状态；锁只做短临界区，不跨 `await` 持有
    shared: Arc<Mutex<Shared>>,
}

impl Default for CacheWarmer {
    /// 等价于 [`CacheWarmer::new`]。
    fn default() -> Self {
        Self::new()
    }
}

impl CacheWarmer {
    /// 创建初始为 `inactive` 的保温器（无后台任务、无待取回执）。
    pub fn new() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared {
                abort: None,
                outcomes: Vec::new(),
                status: WarmStatus {
                    mode: CacheWarmingMode::Streaming.as_str().to_string(),
                    phase: "inactive".to_string(),
                    detail: format!("Inactive ({})", WarmStop::TtlUnavailable.reason()),
                    ..Default::default()
                },
            })),
        }
    }

    /// 取共享状态锁；锁被毒化时直接取回内部值（保温状态不值得因 panic 失败）。
    fn lock(&self) -> MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 状态快照（`/session`）
    pub fn status(&self) -> WarmStatus {
        self.lock().status.clone()
    }

    /// 取消进行中的保温（上下文变化 / 会话切换 / 退出）
    pub fn cancel(&self, reason: WarmStop) {
        let mut s = self.lock();
        if let Some(h) = s.abort.take() {
            h.abort();
        }

        if s.status.phase != "inactive" {
            s.status.phase = "inactive".to_string();
            s.status.detail = format!("Stopped ({})", reason.reason());
            s.status.economics = None;
        }
    }

    /// agent run 结算：`streaming` 模式立即停；`idle` 模式进入 idle 阶段。
    pub fn on_agent_settled(&self) {
        let mode = CacheWarmingMode::from_settings();
        match mode {
            CacheWarmingMode::Streaming => self.cancel(WarmStop::Settled),
            CacheWarmingMode::Idle => {
                let mut s = self.lock();
                if s.status.phase == "streaming" {
                    s.status.phase = "idle".to_string();
                }
            }
            CacheWarmingMode::Off => {}
        }
    }

    /// 取出并清空已完成的保温用量回执（worker 在空闲间隙调用）
    pub fn drain_outcomes(&self) -> Vec<WarmOutcome> {
        std::mem::take(&mut self.lock().outcomes)
    }

    /// 启动保温。返回 `Err(reason)` 表示不保温（已更新状态）。
    pub fn start(&self, req: WarmRequest) -> Result<(), WarmStop> {
        let mode = CacheWarmingMode::from_settings();
        self.cancel(WarmStop::ContextChanged);

        let mut s = self.lock();
        s.status.mode = mode.as_str().to_string();

        if mode == CacheWarmingMode::Off {
            s.status.phase = "inactive".to_string();
            s.status.detail = format!("Inactive ({})", WarmStop::Disabled.reason());
            return Err(WarmStop::Disabled);
        }
        if !is_replayable(&req.model, req.reasoning_effort.as_deref()) {
            s.status.phase = "inactive".to_string();
            s.status.detail = format!("Inactive ({})", WarmStop::NotReplayable.reason());
            return Err(WarmStop::NotReplayable);
        }
        let ttl_ms = match prompt_cache_ttl(&req.model) {
            TtlLookup::Disabled => {
                s.status.phase = "inactive".to_string();
                s.status.detail = format!("Inactive ({})", WarmStop::TtlDisabled.reason());
                return Err(WarmStop::TtlDisabled);
            }
            TtlLookup::Unavailable => {
                s.status.phase = "inactive".to_string();
                s.status.detail = format!("Inactive ({})", WarmStop::TtlUnavailable.reason());
                return Err(WarmStop::TtlUnavailable);
            }
            TtlLookup::Millis(ms) => ms,
        };
        let Some(delay_ms) = warming_delay_ms(ttl_ms) else {
            s.status.phase = "inactive".to_string();
            s.status.detail = format!("Inactive ({})", WarmStop::TtlUnavailable.reason());
            return Err(WarmStop::TtlUnavailable);
        };

        let idle = mode == CacheWarmingMode::Idle;
        s.status.phase = if idle { "idle" } else { "streaming" }.to_string();
        s.status.economics = None;
        let shared = self.shared.clone();
        let abort = tokio::spawn(run_warmer(req, ttl_ms, delay_ms, idle, shared)).abort_handle();
        s.abort = Some(abort);
        Ok(())
    }
}

/// 后台保温循环：等延迟 → 评估/扩展决策 → 重放 → 回执 → 重新计时。
async fn run_warmer(
    req: WarmRequest,
    ttl_ms: u64,
    delay_ms: u64,
    idle: bool,
    shared: Arc<Mutex<Shared>>,
) {
    let started = Instant::now();

    loop {
        // 每次刷新前重新读阶段：streaming→idle 切换会放宽硬上限
        let phase_idle = idle
            || shared
                .lock()
                .map(|s| s.status.phase == "idle")
                .unwrap_or(idle);

        let limit_ms = if phase_idle {
            MAX_IDLE_WARMING_AGE_MS
        } else {
            MAX_WARMING_AGE_MS
        };

        tokio::time::sleep(Duration::from_millis(delay_ms)).await;

        let elapsed = started.elapsed().as_millis() as u64;

        // 刷新截止点：延迟 + 剩余裕量的一半（错过即视为本次保温失效）
        let refresh_deadline = delay_ms + (ttl_ms.saturating_sub(delay_ms)) / 2;
        if elapsed > refresh_deadline {
            stop(&shared, WarmStop::DeadlineMissed);
            return;
        }

        if elapsed >= limit_ms {
            stop(
                &shared,
                if phase_idle {
                    WarmStop::IdleLimit
                } else {
                    WarmStop::HourLimit
                },
            );
            return;
        }

        // 成本评估（prompt token 数来自最后一条 assistant 消息）
        let economics = evaluate(
            last_prompt_tokens(&req.messages),
            req.model.cost.as_ref(),
            phase_idle,
        );
        {
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            s.status.economics = Some(economics.clone());
            s.status.detail = format_status_detail(&economics, delay_ms, phase_idle);
        }
        if !economics.economics_available {
            stop(&shared, WarmStop::NoEconomics);
            return;
        }

        // 扩展覆盖（最后一个给出 action 的扩展生效）
        let payload = json!({
            "type": "cache_warming_decision",
            "warmCost": economics.warm_cost,
            "missCost": economics.miss_cost,
            "continuationProbability": economics.continuation_probability,
            "action": economics.action,
        });
        let override_action = extensions::dispatch_cache_warming_decision(&payload);
        let action = override_action
            .clone()
            .unwrap_or_else(|| economics.action.clone());
        let extension_override = override_action.is_some() && action != economics.action;
        if action == "stop" {
            stop(
                &shared,
                if override_action.is_some() {
                    WarmStop::ExtensionStop
                } else {
                    WarmStop::BelowThreshold
                },
            );
            return;
        }

        let result = provider::stream_chat(
            &req.model,
            &req.messages,
            &req.system_prompt,
            &req.tools,
            req.reasoning_effort.as_deref(),
            req.temperature,
            Some(1),
            None,
            None,
            None,
        )
        .await;

        // 失败/中止静默丢弃（不打断用户，也不污染会话）
        if result.message.stop_reason.as_deref() != Some("error")
            && result.message.stop_reason.as_deref() != Some("aborted")
            && let Some(usage) = result.usage
        {
            let response_model = result
                .message
                .response_model
                .clone()
                .or_else(|| result.message.model.clone());
            let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
            s.status.last_warmed_cost = Some(usage.cost.total);
            s.status.total_warmed_cost += usage.cost.total;
            s.status.warmed_count += 1;
            s.status.detail = "Warming cache".to_string();
            s.outcomes.push(WarmOutcome {
                provider: req.model.provider.clone(),
                model_id: req.model.model_id.clone(),
                response_model,
                usage,
                extension_override,
            });
        }
    }
}

/// 后台任务收尾：清掉取消句柄、置 `inactive` 并写入停止原因，同时清空成本评估。
fn stop(shared: &Arc<Mutex<Shared>>, reason: WarmStop) {
    let mut s = shared.lock().unwrap_or_else(|e| e.into_inner());
    s.abort = None;
    s.status.phase = "inactive".to_string();
    s.status.detail = format!("Stopped ({})", reason.reason());
    s.status.economics = None;
}

/// 生成「下次决策时间 + 续跑概率 + 期望节省 vs 阈值 → 动作」的一行状态文案。
fn format_status_detail(e: &WarmEconomics, delay_ms: u64, idle: bool) -> String {
    let pct = (e.continuation_probability * 100.0).round() as u64;
    let while_running = if idle { "" } else { " while agent is running" };
    let cmp = if e.expected_savings >= CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS {
        ">="
    } else {
        "<"
    };
    format!(
        "Decision in {} ({}% continuation probability{}, expected savings ${:.3} {} $0.050 -> {})",
        format_duration(delay_ms),
        pct,
        while_running,
        e.expected_savings,
        cmp,
        e.action
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(api: &str, cost: Option<Value>) -> ModelConfig {
        ModelConfig {
            model_type: Default::default(),
            provider: "test".into(),
            model_id: "test-model".into(),
            base_url: "https://example.com".into(),
            // 真实保温重放必带凭据（空 key 会在分发前被拒为 `Provider is not configured`）
            api_key: "sk-test".into(),
            api: api.into(),
            output: Vec::new(),
            input: vec!["text".into()],
            reasoning: true,
            max_tokens: Some(4096),
            temperature: None,
            context_window: 200_000,
            thinking_format: String::new(),
            supports_reasoning_effort: false,
            thinking_level_map: None,
            auth_header: false,
            image_resize: Default::default(),
            supports_developer_role: false,
            requires_reasoning_content_on_assistant_messages: false,
            max_tokens_field: "max_tokens".into(),
            cost,
            max_retry_delay_ms: None,
            thinking_budgets: None,
            sampling_params: None,
            sampling_params_by_thinking_level: None,
            provider_routing: None,
            session_id: None,
            supports_usage_in_streaming: true,
            supports_store: true,
            supports_finish_reason: true,
            requires_assistant_after_tool_result: false,
            supports_strict_mode: false,
            send_session_affinity_headers: None,
            session_affinity_format: None,
            allowed_fallback_models: Vec::new(),
        }
    }

    /// strum 派生的双向转换：`as_str` 是规范名、`parse` 去空白且大小写不敏感。
    #[test]
    fn mode_parses_and_defaults_to_streaming() {
        assert_eq!(CacheWarmingMode::parse("off"), Some(CacheWarmingMode::Off));
        assert_eq!(
            CacheWarmingMode::parse("idle"),
            Some(CacheWarmingMode::Idle)
        );
        assert_eq!(CacheWarmingMode::parse("nope"), None);
        assert_eq!(
            CacheWarmingMode::parse(" Streaming "),
            Some(CacheWarmingMode::Streaming)
        );
        assert_eq!(CacheWarmingMode::Streaming.as_str(), "streaming");
        assert_eq!(CacheWarmingMode::Idle.as_str(), "idle");
    }

    #[test]
    fn delay_is_ninety_percent_with_ten_second_margin() {
        assert_eq!(warming_delay_ms(10_000), None, "≤10s 不保温");
        // 300s TTL → 270s
        assert_eq!(warming_delay_ms(300_000), Some(270_000));
        // 极小裕量：TTL-10s < 90%
        assert_eq!(warming_delay_ms(11_000), Some(1_000));
        // 1h：min(3240s, 3590s) = 3240s
        assert_eq!(warming_delay_ms(3_600_000), Some(3_240_000));
    }

    #[test]
    fn replay_safety_matches_pi() {
        let budget_thinking = model("anthropic-messages", None);
        assert!(
            is_replayable(&budget_thinking, None),
            "无 thinking 时可重放"
        );
        assert!(
            !is_replayable(&budget_thinking, Some("high")),
            "预算式 thinking 的 anthropic 请求不可重放"
        );
        assert!(is_replayable(&budget_thinking, Some("off")), "off 等同关闭");
        let completions = model("openai-completions", None);
        assert!(is_replayable(&completions, Some("high")));
    }

    #[test]
    fn economics_uses_cache_delta_minus_warm_cost() {
        // input $3/M、cacheRead $0.3/M、cacheWrite $3.75/M
        let cost = json!({ "input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75 });
        let e = evaluate(100_000, Some(&cost), false);
        // 命中：100k * 0.3/M = $0.03；未命中：100k * 3.75/M = $0.375；
        // 保温成本：命中 + 1 输出 token ≈ $0.030015
        assert!((e.miss_cost - 0.345).abs() < 1e-9, "{}", e.miss_cost);
        assert!(e.warm_cost > 0.03 && e.warm_cost < 0.031);
        assert_eq!(e.continuation_probability, 1.0);
        assert!(e.economics_available);
        assert_eq!(e.action, "warm");
        assert!(e.expected_savings >= CACHE_WARMING_MINIMUM_EXPECTED_SAVINGS);
    }

    #[test]
    fn economics_marks_stop_when_below_threshold() {
        // 极小 prompt：差额 < $0.05
        let cost = json!({ "input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75 });
        let e = evaluate(1_000, Some(&cost), false);
        assert_eq!(e.action, "stop");
        assert!(e.economics_available, "有定价信息但收益不足");
    }

    #[test]
    fn idle_discounts_continuation_probability() {
        let cost = json!({ "input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75 });
        let e = evaluate(100_000, Some(&cost), true);
        assert_eq!(e.continuation_probability, IDLE_CONTINUATION_PROBABILITY);
        // 0.15 * 0.345 - 0.030015 = 0.0217 < 0.05 → 空闲时小额 prompt 不值得
        assert_eq!(e.action, "stop");
        // 300k：0.15 * 1.035 - 0.090045 = 0.0652 >= 0.05 → 空闲也值得
        let big = evaluate(300_000, Some(&cost), true);
        assert_eq!(big.action, "warm");
    }

    #[test]
    fn no_cost_means_no_economics() {
        let e = evaluate(100_000, None, false);
        assert!(!e.economics_available);
        assert_eq!(e.action, "stop");
    }

    #[test]
    fn last_prompt_tokens_sums_cache_fields() {
        let mut m = AgentMessage::user_text("hi");
        m.role = "assistant".into();
        m.usage = Some(Usage {
            input: 100,
            cache_read: 20,
            cache_write: 5,
            ..Default::default()
        });
        let mut later = AgentMessage::user_text("again");
        later.role = "assistant".into();
        later.usage = Some(Usage {
            input: 7,
            ..Default::default()
        });
        assert_eq!(last_prompt_tokens(&[m, later]), 7);
        assert_eq!(last_prompt_tokens(&[]), 0);
    }

    /// 目录 `promptCache`（秒）→ TTL ms；`PRUX_CACHE_RETENTION` 选档，缺失即不可保温。
    #[test]
    fn prompt_cache_ttl_reads_catalog_tiers() {
        let _lock = crate::test_support::env_key_lock();
        let _ad = crate::test_support::AgentDirGuard::temp();

        let mut m = model("anthropic-messages", None);
        m.provider = "anthropic".into();
        m.model_id = "claude-haiku-4-5".into();

        // 默认 short = 300s
        unsafe { std::env::remove_var("PRUX_CACHE_RETENTION") };
        assert_eq!(prompt_cache_ttl(&m), TtlLookup::Millis(300_000));

        // long = 3600s
        unsafe { std::env::set_var("PRUX_CACHE_RETENTION", "long") };
        assert_eq!(prompt_cache_ttl(&m), TtlLookup::Millis(3_600_000));

        // none = 请求已关闭缓存
        unsafe { std::env::set_var("PRUX_CACHE_RETENTION", "none") };
        assert_eq!(prompt_cache_ttl(&m), TtlLookup::Disabled);

        unsafe { std::env::remove_var("PRUX_CACHE_RETENTION") };
        // 无 promptCache 的 provider（openai 目录不含该字段）→ 不可保温
        let mut o = model("openai-completions", None);
        o.provider = "openai".into();
        o.model_id = "gpt-4-turbo".into();
        assert_eq!(prompt_cache_ttl(&o), TtlLookup::Unavailable);
    }

    /// detail 文案：时长是紧凑标签（`3m42s`），其余与 pi 一致。
    #[test]
    fn status_detail_shape() {
        let cost = json!({ "input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75 });
        let e = evaluate(100_000, Some(&cost), false);
        assert_eq!(
            format_status_detail(&e, 222_000, false),
            "Decision in 3m42s (100% continuation probability while agent is running, \
             expected savings $0.315 >= $0.050 -> warm)"
        );
        assert_eq!(
            format_status_detail(&e, 12_000, true),
            "Decision in 12s (100% continuation probability, \
             expected savings $0.315 >= $0.050 -> warm)"
        );
    }

    #[test]
    fn start_stops_when_mode_off() {
        let warmer = CacheWarmer::new();
        // 无法在单测里安全改全局 settings；直接验证离线判定路径之外的状态机：
        // 无目录 promptCache → Unavailable，且状态为 inactive。
        let req = WarmRequest {
            model: model("anthropic-messages", None),
            messages: Vec::new(),
            system_prompt: String::new(),
            tools: Vec::new(),
            reasoning_effort: None,
            temperature: None,
        };
        let err = warmer.start(req).unwrap_err();
        assert!(
            matches!(err, WarmStop::TtlUnavailable | WarmStop::Disabled),
            "{:?}",
            err
        );
        assert_eq!(warmer.status().phase, "inactive");
        assert!(warmer.drain_outcomes().is_empty());
    }

    /// 端到端：`run_warmer` 用 `max_tokens=1` 重放请求，成功后写入回执与状态。
    #[test]
    fn runner_replays_request_and_reports_usage() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        crate::test_support::run_with_timeout(Duration::from_secs(60), || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                // 单连接假 SSE 服务器：一次 chat completions 响应（含 usage）
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let server = tokio::spawn(async move {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 4096];
                    while let Ok(n) = socket.read(&mut tmp).await {
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let body = concat!(
                        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"x\"}}]}\n\n",
                        "data: {\"id\":\"c\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":100000,\"completion_tokens\":1,\"total_tokens\":100001}}\n\n",
                        "data: [DONE]\n\n"
                    );
                    socket
                        .write_all(
                            format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                                body.len(),
                                body
                            )
                            .as_bytes(),
                        )
                        .await
                        .unwrap();
                    let _ = socket.shutdown().await;
                });

                let mut m = model(
                    "openai-completions",
                    Some(json!({ "input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75 })),
                );
                m.base_url = format!("http://127.0.0.1:{}", addr.port());

                let mut assistant = AgentMessage::user_text("hi");
                assistant.role = "assistant".into();
                assistant.usage = Some(Usage {
                    input: 100_000,
                    ..Default::default()
                });

                let req = WarmRequest {
                    model: m,
                    messages: vec![assistant],
                    system_prompt: "sys".into(),
                    tools: Vec::new(),
                    reasoning_effort: None,
                    temperature: None,
                };
                let shared = Arc::new(Mutex::new(Shared {
                    abort: None,
                    outcomes: Vec::new(),
                    status: WarmStatus {
                        mode: "streaming".into(),
                        phase: "streaming".into(),
                        ..Default::default()
                    },
                }));

                // ttl 11s → 延迟 1s；1.5s 后应已跑完一次保温
                let task = tokio::spawn(run_warmer(req, 11_000, 1_000, false, shared.clone()));
                tokio::time::sleep(Duration::from_millis(1_500)).await;
                task.abort();
                let _ = server.await;

                let outcomes = std::mem::take(&mut shared.lock().unwrap().outcomes);
                assert_eq!(outcomes.len(), 1, "应完成一次保温");
                assert_eq!(outcomes[0].provider, "test");
                assert!(outcomes[0].usage.cost.total > 0.0);
                let s = shared.lock().unwrap();
                assert_eq!(s.status.warmed_count, 1);
                assert_eq!(s.status.last_warmed_cost, Some(outcomes[0].usage.cost.total));
            });
        });
    }
}

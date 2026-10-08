//! 底栏扩展：三种布局，对应扩展名 `footer(normal)` / `footer(minimal)` / `footer(rich)`
//!
//! - `normal`：两行——行 1 工作目录(git 分支)，行 2 上下文/token 统计 + 模型名；
//! - `minimal`：只显示 normal 的第二行（模型所在行）；
//! - `rich`：行 1 分支+模型+上下文（左）、token 速率+in/out+费用+计划模式+交互数+耗时（右），
//!   行 2 目录名（左）+工具调用计数（右），带任务计时与 token 速率采样状态机。
//!
//! footer 扩展互斥：同一时刻至多一个 footer 启用（注册/面板切换时自动禁用
//! 其他 footer），对应"选中一个自动取消其他"的单一底栏覆盖语义。

pub mod minimal;
pub mod normal;
pub mod rich;

pub(crate) use super::{ACCENT, DIM, ERROR, MUTED, SUCCESS, WARNING};
pub use minimal::MinimalFooter;
pub use normal::NormalFooter;
pub use rich::RichFooter;

use crate::{
    core::{
        extensions::{FooterCtx, FooterLine, FooterSpan},
        provider::AgentMessage,
    },
    extensions::{
        FOOTER_FACTORIES, FooterFactory, PRIORITY_FOOTER_MINIMAL, PRIORITY_FOOTER_NORMAL,
        PRIORITY_FOOTER_RICH,
    },
    utils::{
        display::{display_width, truncate_display},
        glyphs::{DEF_INPUT_TOKENS, DEF_OUTPUT_TOKENS},
    },
};
use std::{path::Path, sync::Arc};

// ---- 自声明工厂：linkme 分布式切片，priority 值大者先注册（见 [`crate::extensions`]）。
// footer 互斥“最后注册的启用者生效”：三者 priority 依次递增，保证注册序
// rich → minimal → normal，最终默认启用 `footer(normal)`（与历史注释意图一致）。

/// rich 底栏的注册工厂，priority 决定注册序与互斥默认启用项。
#[linkme::distributed_slice(FOOTER_FACTORIES)]
static FOOTER_RICH_FACTORY: FooterFactory = FooterFactory {
    priority: PRIORITY_FOOTER_RICH,
    make: || Arc::new(RichFooter::new()),
};

/// minimal 底栏的注册工厂。
#[linkme::distributed_slice(FOOTER_FACTORIES)]
static FOOTER_MINIMAL_FACTORY: FooterFactory = FooterFactory {
    priority: PRIORITY_FOOTER_MINIMAL,
    make: || Arc::new(MinimalFooter::new()),
};

/// normal 底栏的注册工厂。
#[linkme::distributed_slice(FOOTER_FACTORIES)]
static FOOTER_NORMAL_FACTORY: FooterFactory = FooterFactory {
    priority: PRIORITY_FOOTER_NORMAL,
    make: || Arc::new(NormalFooter::new()),
};

/// footer 成本统计口径（normal / minimal / rich 共用）。
///
/// 除 assistant 外，还要累加**携带 usage 的 toolResult**：扩展经 `ctx.execute_tool()`
/// 嵌套调用（如子代理 `reportUsage`）的开销挂在调用方的工具结果上，
/// 不累加就等于把这部分成本从会话成本里漏掉。摘要类消息
/// （`compactionSummary` / `branchSummary`）由会话投影带上条目 usage，一并计入。
/// `latest_cache_hit_rate` 只由 assistant 决定（工具结果的命中率不代表上下文）。
pub(crate) struct UsageScan {
    /// 累计输入 token 数（不含缓存读写的计费项）。
    pub tok_in: u64,
    /// 累计输出 token 数。
    pub tok_out: u64,
    /// 累计从 prompt 缓存读取的 token 数。
    pub cache_read: u64,
    /// 累计写入 prompt 缓存的 token 数。
    pub cache_write: u64,
    /// 累计成本（美元，来自各条 usage 的 cost.total）。
    pub cost: f64,
    /// 最近一条 assistant 的缓存命中率（百分比）；无 assistant 或 prompt 为空时为 None。
    pub latest_cache_hit_rate: Option<f64>,
}

/// 把 token 数格式化成 footer 用的短串：<1000 原数，千/百万级用 `1.2k`/`34k`/`1.2M`/`34M`。
pub(crate) fn format_tokens(count: u64) -> String {
    if count < 1000 {
        count.to_string()
    } else if count < 10_000 {
        format!("{:.1}k", count as f64 / 1000.0)
    } else if count < 1_000_000 {
        format!("{}k", (count as f64 / 1000.0).round() as u64)
    } else if count < 10_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else {
        format!("{}M", (count as f64 / 1_000_000.0).round() as u64)
    }
}

/// 替换绝对路径中的的`HOME`前缀为`~`
pub(crate) fn format_cwd(cwd: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if home.is_empty() {
        return cwd.to_string();
    }
    let p = Path::new(cwd);
    let h = Path::new(&home);
    if let Ok(rel) = p.strip_prefix(h) {
        if rel.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", rel.display());
    }
    cwd.to_string()
}

/// 行 1（normal）：工作目录（~ 化）+ (git 分支)，dim 单行
pub(crate) fn pwd_line(ctx: &FooterCtx) -> FooterLine {
    let mut pwd = format_cwd(ctx.cwd);
    if let Some(branch) = ctx.git_branch {
        pwd = format!("{} ({})", pwd, branch);
    }
    truncate_to_width(&pwd, ctx.width)
}

/// 向统计行追加一个 span（`key` 为（主题键，缺省色））。
#[inline(always)]
fn push_span(stats: &mut Vec<FooterSpan>, key: (&'static str, &'static str), text: &str) {
    stats.push(FooterSpan::new(key.0, key.1, text.to_string()));
}

/// 路由后缀：` → <物理模型>[:<级别>]`；未路由时为空串。normal/minimal/rich 共用。
pub(crate) fn routed_suffix(ctx: &FooterCtx) -> String {
    let Some(model) = ctx.routed_model else {
        return String::new();
    };

    match ctx.routed_thinking_level {
        Some(level) => format!(" → {model}:{level}"),
        None => format!(" → {model}"),
    }
}

/// 行 2（normal/minimal 共用）：左侧 token/缓存/费用/上下文统计 + 右侧模型名。
/// minimal footer 只渲染本行（模型所在行）。
pub(crate) fn stats_line(ctx: &FooterCtx) -> FooterLine {
    let UsageScan {
        tok_in,
        tok_out,
        cache_read,
        cache_write,
        cost,
        latest_cache_hit_rate,
    } = scan_usage(ctx.messages);

    // 左侧 stats：↑in ↓out R W CH% $cost context%/window
    let mut stats: Vec<FooterSpan> = Vec::new();
    if tok_in > 0 {
        push_span(
            &mut stats,
            DIM,
            &format!("{DEF_INPUT_TOKENS}{}", format_tokens(tok_in)),
        );
        push_span(&mut stats, DIM, " ");
    }
    if tok_out > 0 {
        push_span(
            &mut stats,
            DIM,
            &format!("{DEF_OUTPUT_TOKENS}{}", format_tokens(tok_out)),
        );
        push_span(&mut stats, DIM, " ");
    }
    if cache_read > 0 {
        push_span(&mut stats, DIM, &format!("R{}", format_tokens(cache_read)));
        push_span(&mut stats, DIM, " ");
    }
    if cache_write > 0 {
        push_span(&mut stats, DIM, &format!("W{}", format_tokens(cache_write)));
        push_span(&mut stats, DIM, " ");
    }
    if (cache_read > 0 || cache_write > 0)
        && let Some(rate) = latest_cache_hit_rate
    {
        push_span(&mut stats, DIM, &format!("CH{:.1}%", rate));
        push_span(&mut stats, DIM, " ");
    }
    if cost > 0.0 {
        push_span(&mut stats, SUCCESS, &format!("${:.3}", cost));
        push_span(&mut stats, DIM, " ");
    }

    // 上下文：>90 error、>70 warning、否则无色（dim 由外层）
    let window = format_tokens(ctx.context_window as u64);
    let pct_display = format!("{:.1}%/{}", ctx.context_percent, window);
    let (pct_key, pct_fb) = if ctx.context_percent > 90.0 {
        ERROR
    } else if ctx.context_percent > 70.0 {
        WARNING
    } else {
        DIM
    };
    stats.push(FooterSpan::new(pct_key, pct_fb, pct_display));

    // 右侧：模型名（dim，右对齐，最少 2 空格间隔）；无模型时右侧留空。
    // reasoning 模型显示 `model:thinkingLevel`，level 缺省按 off。
    let model = match (ctx.model, ctx.model_reasoning) {
        (Some(m), true) => format!(
            "{}:{}{}",
            m,
            ctx.thinking_level.unwrap_or("off"),
            routed_suffix(ctx)
        ),
        (Some(m), false) => format!("{}{}", m, routed_suffix(ctx)),
        (None, _) => String::new(),
    };

    let stats_w: usize = stats.iter().map(|s| display_width(&s.text)).sum();
    let min_pad = 2usize;
    let avail_right = ctx.width.saturating_sub(stats_w + min_pad);
    let model_display = if !model.is_empty() {
        if display_width(&model) > avail_right {
            truncate_display(&model, avail_right.max(1)).0
        } else {
            model
        }
    } else {
        String::new()
    };
    let mut line: FooterLine = stats;
    let pad_w = ctx
        .width
        .saturating_sub(stats_w + display_width(&model_display))
        .max(1);
    if !model_display.is_empty() {
        line.push(FooterSpan::new(DIM.0, DIM.1, " ".repeat(pad_w)));
        line.push(FooterSpan::new(DIM.0, DIM.1, model_display));
    }
    truncate_spans(line, ctx.width)
}

/// 汇总全部消息的 usage：累加输入/输出/缓存读写的 token 与费用，
/// 并取最后一条 assistant 的缓存命中率（缓存读 / 总 prompt token）。
/// 嵌套工具结果与摘要只计入总量，不参与命中率。
pub(crate) fn scan_usage(messages: &[AgentMessage]) -> UsageScan {
    let mut acc = UsageScan {
        tok_in: 0,
        tok_out: 0,
        cache_read: 0,
        cache_write: 0,
        cost: 0.0,
        latest_cache_hit_rate: None,
    };

    for msg in messages {
        let Some(u) = &msg.usage else { continue };
        match msg.role.as_str() {
            "assistant" => {
                acc.tok_in += u.input as u64;
                acc.tok_out += u.output as u64;
                acc.cache_read += u.cache_read as u64;
                acc.cache_write += u.cache_write as u64;
                acc.cost += u.cost.total;
                let prompt_tokens = u.input as u64 + u.cache_read as u64 + u.cache_write as u64;
                acc.latest_cache_hit_rate = if prompt_tokens > 0 {
                    Some(u.cache_read as f64 / prompt_tokens as f64 * 100.0)
                } else {
                    None
                };
            }
            // 嵌套工具调用 / 摘要：只计总量，不影响缓存命中率
            "toolResult" | "compactionSummary" | "branchSummary" => {
                acc.tok_in += u.input as u64;
                acc.tok_out += u.output as u64;
                acc.cache_read += u.cache_read as u64;
                acc.cache_write += u.cache_write as u64;
                acc.cost += u.cost.total;
            }
            _ => {}
        }
    }
    acc
}

/// 单行 dim 文本截断
pub(crate) fn truncate_to_width(text: &str, width: usize) -> FooterLine {
    let (t, _) = truncate_display(text, width);
    vec![FooterSpan::new(DIM.0, DIM.1, t)]
}

/// 按显示宽度截断 span 行
pub(crate) fn truncate_spans(line: FooterLine, width: usize) -> FooterLine {
    let mut out: FooterLine = Vec::new();
    let mut used = 0usize;
    for span in line {
        let w = display_width(&span.text);
        if used >= width {
            break;
        }
        if used + w <= width {
            out.push(span);
            used += w;
        } else {
            let room = width - used;
            let (t, _) = truncate_display(&span.text, room);
            if !t.is_empty() {
                out.push(FooterSpan::new(span.key, span.fallback, t));
            }
            used = width;
        }
    }
    out
}

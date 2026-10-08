//! Mermaid → Unicode 盒图文本（基于 mermaid-text 库）。
//!
//! 由 `mermaid-text` 统一渲染 graph/flowchart、sequenceDiagram、gantt、pie、
//! erDiagram 等图表类型，输出为确定性 Unicode 盒图（无需浏览器/图片协议）。
//!
//! 宽度约束：
//! - 调用方传入最大输出行宽（终端列数），经 [`RenderOptions::max_width`] 交给
//!   库做渐进式紧凑（减小层间/节点间距、折行长标签）。
//! - `max_width_strict` 开启硬预算：若最窄配置下最宽行仍超出请求宽度，返回
//!   `TooWide` 错误，本模块将其视为"无法渲染"返回 `None`，由调用方回退为源码
//!   展示——保证最终输出的任何一行都不超过宽度上限。
//!
//! 高度紧凑：
//! - 库的压缩管线只按**宽度**逐档选择间距，没有高度维度——窄而深的图（如
//!   `graph TD` 长链）在默认间距（层距 6 格）下宽度很容易满足预算，结果是一张
//!   很高的图。本模块因此先显式以最紧凑档位渲染（`gaps_override (1, 0)`，
//!   Sugiyama 后端层距压到 3 格基线），只要宽度硬预算放得下就采纳；只有紧凑
//!   档位超宽（需要折行兜底）时才退回库的常规压缩管线。
//!
//! 无法解析 / 空输入 / 不支持的图类型同样返回 `None`。

use mermaid_text::{RenderOptions, render_with_options};

/// 渲染 mermaid 源码为 Unicode 文本图，并限制最大行宽。
///
/// - `src`：mermaid 源码（含头部行，如 `graph TD` / `sequenceDiagram` / `gantt`）。
/// - `width`：最大输出行宽（显示列）。`None` 表示不限制。
///
/// 返回渲染文本；超宽（严格预算下仍无法压缩）或解析失败/为空时返回 `None`，
/// 调用方应回退为源码展示。
pub fn render_mermaid(src: &str, width: Option<usize>) -> Option<String> {
    // 高度优先：先尝试最紧凑档位。库的压缩管线只按宽度选档，窄而深的图会在
    // 默认层距（6 格）下"宽度刚好满足"就停，导致高度很高；显式 gaps_override
    // 直接以最小间距渲染。紧凑档位能通过宽度硬预算时，其行数必然不高于任何
    // 非紧凑档位（层距最小且不折行），高度只降不升。
    let tight = RenderOptions {
        max_width: width,
        max_width_strict: true,
        gaps_override: Some((1, 0)),
        ..Default::default()
    };
    if let Ok(out) = render_with_options(src, &tight) {
        return Some(out);
    }

    // 紧凑档位超宽（含需要标签折行的情况）：退回库的常规管线（逐档压缩 + 折行）。
    let opts = RenderOptions {
        max_width: width,
        max_width_strict: true,
        ..Default::default()
    };
    render_with_options(src, &opts).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(src: &str) -> Option<String> {
        render_mermaid(src, Some(80))
    }

    #[test]
    fn renders_simple_graph_td() {
        let src = "graph TD\n  A[Start] --> B{Check}\n  B -->|yes| C[End]";
        let out = render(src).unwrap();
        assert!(out.contains("Start"), "{out}");
        assert!(out.contains("Check"), "{out}");
        assert!(out.contains("End"), "{out}");
        assert!(out.contains("yes"), "{out}");
    }

    #[test]
    fn renders_chain_edges_and_lr() {
        let out = render("graph LR\n  A[Build] --> B[Test] --> C[Deploy]").unwrap();
        for s in ["Build", "Test", "Deploy"] {
            assert!(out.contains(s), "{s} missing: {out}");
        }
    }

    #[test]
    fn empty_or_unsupported_returns_none() {
        assert!(render("").is_none());
        assert!(render("%% only a comment").is_none());
    }

    #[test]
    fn compact_preferred_over_tall_default() {
        // 窄而深的 TD 链：宽度在默认间距下也放得下（管线会停在默认档），
        // 但高度很高。render_mermaid 应优先采用最紧凑档位，行数显著少于默认渲染。
        let mut src = String::from("graph TD\n");
        for i in 0..=7 {
            if i > 0 {
                src.push_str(&format!(
                    "  n{}[节点{}] --> n{}[节点{}]\n",
                    i - 1,
                    i - 1,
                    i,
                    i
                ));
            }
        }
        let tight = render(&src).unwrap();
        let natural = render_with_options(
            &src,
            &RenderOptions {
                max_width: Some(80),
                max_width_strict: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(
            tight.lines().count() < natural.lines().count(),
            "紧凑档位应更矮: tight={} natural={}\n{tight}\n---\n{natural}",
            tight.lines().count(),
            natural.lines().count()
        );
        assert!(tight.contains("节点7"), "{tight}");
    }

    #[test]
    fn strict_width_falls_back_on_too_wide() {
        // 超过硬预算的图应返回 None（调用方回退源码），不得输出超宽行。
        let src = "graph LR\n  A[非常非常非常非常非常非常长的节点标签] --> B[另一个非常非常非常非常长的标签]";
        assert!(render_mermaid(src, Some(20)).is_none(), "窄预算应失败");
        // 无宽度限制时应能渲染
        assert!(render_mermaid(src, None).is_some());
    }

    #[test]
    fn renders_sequence_diagram() {
        let src = "sequenceDiagram\n    participant 用户\n    participant 系统\n    用户->>系统: 发送请求\n    系统->>用户: 返回响应";
        let out = render(src).unwrap();
        // 宽字符（CJK/韩文/emoji）按显示宽度占位，参与者名/消息连续无额外空格
        for expect in ["用户", "系统", "发送请求", "返回响应"] {
            assert!(out.contains(expect), "{expect} missing: {out}");
        }
    }

    #[test]
    fn sequence_participant_alias_and_implicit() {
        let out = render("sequenceDiagram\nparticipant A as Alice\nA->>Bob: hi").unwrap();
        assert!(out.contains("Alice"), "{out}");
        assert!(out.contains("Bob"), "{out}");
        assert!(out.contains("hi"), "{out}");
    }

    #[test]
    fn gantt_renders_title_sections_tasks() {
        let src = "gantt\n    title 项目计划\n    section 阶段1\n    任务A :a1, 2024-01-01, 30d\n    section 阶段2\n    任务B :a2, after a1, 20d";
        let out = render(src).unwrap();
        assert!(out.contains("Gantt: 项目计划"), "{out}");
        assert!(out.contains("阶段1"), "{out}");
        assert!(out.contains("阶段2"), "{out}");
        assert!(out.contains("任务A"), "{out}");
        assert!(out.contains("任务B"), "{out}");
    }
}

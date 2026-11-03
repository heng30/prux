//! 维护轮（maintenance）：拿现有条目做一次 reconcile，决定每条的 keep / adapt / retire。
//!
//! 抽取只会往里加，不会收拾：反复抽取后同一件事会写成几条近义句子，早期猜错的事实会一直留着。
//! 维护轮把一份作用域里的有效条目（含证据）交给模型，要求**每个 id 恰好一个决定**：
//!
//! | 决定 | 含义 | 落盘效果 |
//! |------|------|----------|
//! | `keep` | 仍然成立且不自相矛盾 | 不动 |
//! | `adapt` | 大意对但太含糊/混了两件事/证据不准 | 换掉 `content`（可带新证据与置信度） |
//! | `retire` | 已不成立、太泛、或被另一条完全覆盖 | `active = false`，不再参与召回 |
//!
//! 判定是**全有或全无**：返回里缺 id、重复 id、给了不在清单里的 id、`adapt` 却没给正文——
//! 任何一条不满足就整批失败、一个字节都不改（半套决定会让记忆图处在谁也说不清的状态）。
//!
//! 这里只有纯逻辑（拼提示 / 解析 / 校验 / 落图）；请求本身在 [`super::run_maintenance`]。

use super::graph::{MemoryEntry, MemoryGraph, MemoryScope};
use crate::utils::{glyphs::DEF_ELLIPSIS, time::now_iso, truncate::truncate_chars};
use serde::Deserialize;
use strum_macros::{EnumString, IntoStaticStr};

/// 维护请求的系统提示。
pub(super) const SYSTEM: &str = "You reconcile a coding agent's durable memory store. Reply with \
a JSON array and nothing else. Judge each entry against its evidence; never invent entries.";

/// 提示里列出的条目上限（再多就分不清谁覆盖谁，且会挤掉证据）。
pub(super) const MAX_ENTRIES: usize = 60;

/// 单条正文 / 证据在提示里的字符上限。
const PREVIEW_CHARS: usize = 300;

/// 请求的输出 token 上限（每条决定都很短，60 条也够）。
pub(super) const MAX_TOKENS: u32 = 2048;

/// 一条维护决定。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Decision {
    /// 目标条目 id。
    pub(super) id: String,
    /// 要做什么。
    pub(super) action: Action,
    /// `adapt` 的新正文；`keep` / `retire` 时为 `None`。
    pub(super) content: Option<String>,
    /// `adapt` 的新证据；缺省表示保留原证据。
    pub(super) evidence: Option<String>,
    /// `adapt` 的新置信度（0.0–1.0）；缺省表示不动。
    pub(super) confidence: Option<f32>,
}

/// 维护决定的三档动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub(super) enum Action {
    /// 保持原样。
    Keep,
    /// 改写正文（换掉含糊/混装的表述）。
    Adapt,
    /// 退役（不再成立 / 太泛 / 被别的条目覆盖）。
    Retire,
}

/// 一条落盘结果统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct Applied {
    /// 保持原样的条数。
    pub(super) kept: usize,
    /// 被改写的条数。
    pub(super) adapted: usize,
    /// 被退役的条数。
    pub(super) retired: usize,
}

impl Applied {
    /// 是否真的改动了什么（改写 / 退役）。
    pub(super) fn changed(&self) -> bool {
        self.adapted > 0 || self.retired > 0
    }
}

/// 模型回复里的原始决定（宽松反序列化）。
#[derive(Debug, Deserialize)]
struct RawDecision {
    /// 目标条目 id。
    #[serde(default)]
    id: String,
    /// 动作字符串。
    #[serde(default)]
    action: String,
    /// 新正文。
    #[serde(default)]
    content: String,
    /// 新证据。
    #[serde(default)]
    evidence: String,
    /// 新置信度。
    #[serde(default)]
    confidence: Option<f32>,
}

/// 待 reconcile 的一条条目（跨两份存储，`scope` 只用于提示里的分组展示）。
pub(super) struct Candidate {
    /// 这条属于哪份存储。
    pub(super) scope: MemoryScope,
    /// 条目本身。
    pub(super) entry: MemoryEntry,
}

/// 拼维护提示词：规则 + 每条可见字段 + 证据。
pub(super) fn build_prompt(candidates: &[Candidate]) -> String {
    let mut prompt = String::from(
        "Reconcile the memory entries below. Return a JSON array with EXACTLY one element per \
         entry id and no others. Each element is an object with:\n\
         - \"id\": the entry id, copied verbatim\n\
         - \"action\": \"keep\", \"adapt\" or \"retire\"\n\
         - \"content\": required for \"adapt\": the rewritten single self-contained sentence; \
         omit it otherwise\n\
         - \"evidence\": optional for \"adapt\": a new evidence string, only when the old one is \
         wrong\n\
         - \"confidence\": optional for \"adapt\": 0.0-1.0, lower it when the evidence only \
         partially supports the sentence\n\n\
         Rules:\n\
         - \"keep\" when the entry still holds and says exactly one checkable thing.\n\
         - \"adapt\" when it is vague (\"the project uses a bundler\"), mixes two facts into one \
         sentence, contradicts its own evidence, or uses identifiers that no longer match it. \
         Keep the original meaning; do not add facts that the entry or its evidence do not show.\n\
         - \"retire\" when it is no longer true, states nothing checkable, or is fully covered by \
         another entry that you keep. When two entries say the same thing, retire the weaker one.\n\
         - Entries without evidence may be kept when they are plainly true; retire them when they \
         are guesses or unverifiable.\n\
         - Every id must appear exactly once. Never invent ids. Do not merge two entries into one.\n\
         - Reply with the array only.\n",
    );

    let mut last_scope = None;
    for candidate in candidates.iter().take(MAX_ENTRIES) {
        if last_scope != Some(candidate.scope) {
            prompt.push_str(&format!("\n## {} entries\n", candidate.scope.as_label()));
            last_scope = Some(candidate.scope);
        }
        let entry = &candidate.entry;
        prompt.push_str(&format!(
            "- id: {}\n  category: {}\n  trust: {:?}\n  confidence: {:.2}\n  content: {}\n",
            entry.id,
            entry.category.as_label(),
            entry.trust,
            entry.confidence,
            one_line(&entry.content),
        ));
        match &entry.evidence {
            Some(evidence) => {
                prompt.push_str(&format!("  evidence: {}\n", one_line(evidence)));
            }
            None => prompt.push_str("  evidence: (none recorded)\n"),
        }
    }
    prompt
}

/// 压成单行并截断（多行正文会把一条记忆拆成几行，读起来分不清边界）。
fn one_line(text: &str) -> String {
    truncate_chars(
        &super::recall::single_line(text),
        PREVIEW_CHARS,
        Some(DEF_ELLIPSIS),
        true,
    )
}

/// 解析并校验模型回复。
///
/// 返回 `Err` 时调用方**不得**改动任何条目：缺 id、重复 id、未知 id、未知动作、
/// `adapt` 缺正文，都视为整批失败。
pub(super) fn parse_response(text: &str, known: &[String]) -> Result<Vec<Decision>, String> {
    let array = super::extract::first_json_array(text)
        .ok_or_else(|| "no JSON array in the reply".to_string())?;
    let raw: Vec<RawDecision> =
        serde_json::from_str(array).map_err(|err| format!("malformed decisions: {err}"))?;

    let mut out: Vec<Decision> = Vec::with_capacity(raw.len());
    for item in raw {
        let id = item.id.trim().to_string();
        if id.is_empty() {
            return Err("a decision has an empty id".to_string());
        }
        if !known.contains(&id) {
            return Err(format!("decision for unknown id {id}"));
        }
        if out.iter().any(|seen| seen.id == id) {
            return Err(format!("duplicate decision for {id}"));
        }
        let action: Action = item
            .action
            .trim()
            .to_ascii_lowercase()
            .parse()
            .map_err(|_| format!("unknown action {:?} for {id}", item.action))?;
        let content = item.content.trim();
        if action == Action::Adapt && content.is_empty() {
            return Err(format!("adapt decision for {id} has no content"));
        }
        let evidence = item.evidence.trim();
        out.push(Decision {
            id,
            action,
            content: (action == Action::Adapt).then(|| content.to_string()),
            evidence: (!evidence.is_empty()).then(|| evidence.to_string()),
            confidence: item.confidence.map(|c| c.clamp(0.0, 1.0)),
        });
    }

    let missing: Vec<&String> = known
        .iter()
        .filter(|id| !out.iter().any(|decision| &decision.id == *id))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "{} entr{} not decided: {}",
            missing.len(),
            if missing.len() == 1 { "y" } else { "ies" },
            missing
                .iter()
                .map(|id| id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Ok(out)
}

/// 把决定落到一份图上：`adapt` 换正文/证据/置信度，`retire` 退役，`keep` 不动。
///
/// 不在本图里的 id 被忽略（决定来自合并清单，另一份存储的条目由那一份处理）。
pub(super) fn apply(graph: &mut MemoryGraph, decisions: &[Decision]) -> Applied {
    let mut applied = Applied::default();
    for decision in decisions {
        match decision.action {
            Action::Keep => {
                if graph.get(&decision.id).is_some() {
                    applied.kept += 1;
                }
            }
            Action::Adapt => {
                let Some(entry) = graph.get_mut(&decision.id) else {
                    continue;
                };
                if let Some(content) = &decision.content {
                    entry.content = content.clone();
                }
                if let Some(evidence) = &decision.evidence {
                    entry.evidence = Some(evidence.clone());
                }
                if let Some(confidence) = decision.confidence {
                    entry.confidence = confidence;
                }
                entry.updated_at = now_iso();
                entry.refresh_search_text();
                applied.adapted += 1;
            }
            Action::Retire => {
                if graph.retire_memory(&decision.id) {
                    applied.retired += 1;
                }
            }
        }
    }
    applied
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::memory::graph::MemoryCategory;

    /// 造一条带 id 的候选。
    fn entry(id: &str, content: &str, evidence: Option<&str>) -> Candidate {
        let mut entry = MemoryEntry::new(MemoryCategory::Fact, content);
        entry.id = id.to_string();
        entry.evidence = evidence.map(str::to_string);
        Candidate {
            scope: MemoryScope::Project,
            entry,
        }
    }

    /// 造一份含若干条记忆的图。
    fn graph_with(ids: &[&str]) -> MemoryGraph {
        let mut graph = MemoryGraph::new();
        for id in ids {
            let mut entry = MemoryEntry::new(MemoryCategory::Fact, format!("body of {id}"));
            entry.id = (*id).to_string();
            graph.add_memory(entry);
        }
        graph
    }

    #[test]
    fn prompt_lists_every_candidate_with_evidence() {
        let candidates = vec![
            entry("mem_a", "uses pnpm", Some("user: always pnpm here")),
            entry("mem_b", "no evidence", None),
        ];
        let prompt = build_prompt(&candidates);
        assert!(prompt.contains("id: mem_a"));
        assert!(prompt.contains("evidence: user: always pnpm here"));
        assert!(prompt.contains("evidence: (none recorded)"));
        assert!(prompt.contains("exactly once"));
    }

    #[test]
    fn parse_accepts_one_decision_per_id() {
        let known = vec!["mem_a".to_string(), "mem_b".to_string()];
        let text = r#"Here you go:
            [{"id":"mem_a","action":"adapt","content":"uses pnpm for installs"},
             {"id":"mem_b","action":"retire"}]"#;
        let decisions = parse_response(text, &known).unwrap();
        assert_eq!(decisions.len(), 2);
        assert_eq!(decisions[0].action, Action::Adapt);
        assert_eq!(
            decisions[0].content.as_deref(),
            Some("uses pnpm for installs")
        );
        assert_eq!(decisions[1].action, Action::Retire);
        assert!(decisions[1].content.is_none());
    }

    #[test]
    fn parse_rejects_incomplete_or_inconsistent_decisions() {
        let known = vec!["mem_a".to_string(), "mem_b".to_string()];
        for (text, needle) in [
            (r#"[{"id":"mem_a","action":"keep"}]"#, "not decided"),
            (
                r#"[{"id":"mem_a","action":"keep"},{"id":"mem_a","action":"retire"},{"id":"mem_b","action":"keep"}]"#,
                "duplicate decision",
            ),
            (
                r#"[{"id":"mem_a","action":"keep"},{"id":"mem_b","action":"keep"},{"id":"mem_zz","action":"keep"}]"#,
                "unknown id",
            ),
            (
                r#"[{"id":"mem_a","action":"adapt"},{"id":"mem_b","action":"keep"}]"#,
                "has no content",
            ),
            (
                r#"[{"id":"mem_a","action":"delete"},{"id":"mem_b","action":"keep"}]"#,
                "unknown action",
            ),
            ("no json here", "no JSON array"),
        ] {
            let err = parse_response(text, &known).unwrap_err();
            assert!(err.contains(needle), "错误里应含 {needle:?}，实际 {err:?}");
        }
    }

    #[test]
    fn apply_rewrites_retires_and_counts() {
        let mut graph = graph_with(&["mem_a", "mem_b", "mem_c"]);
        let decisions = vec![
            Decision {
                id: "mem_a".to_string(),
                action: Action::Adapt,
                content: Some("rewritten".to_string()),
                evidence: Some("evidence".to_string()),
                confidence: Some(0.4),
            },
            Decision {
                id: "mem_b".to_string(),
                action: Action::Retire,
                content: None,
                evidence: None,
                confidence: None,
            },
            Decision {
                id: "mem_c".to_string(),
                action: Action::Keep,
                content: None,
                evidence: None,
                confidence: None,
            },
        ];
        let applied = apply(&mut graph, &decisions);
        assert_eq!(
            applied,
            Applied {
                kept: 1,
                adapted: 1,
                retired: 1
            }
        );
        let adapted = graph.get("mem_a").unwrap();
        assert_eq!(adapted.content, "rewritten");
        assert_eq!(adapted.evidence.as_deref(), Some("evidence"));
        assert!((adapted.confidence - 0.4).abs() < f32::EPSILON);
        assert!(
            graph
                .get("mem_a")
                .unwrap()
                .searchable_text()
                .contains("rewritten")
        );
        assert!(!graph.get("mem_b").unwrap().active, "retire 后不再有效");
        assert!(graph.get("mem_b").unwrap().superseded_by.is_none());
        assert!(graph.get("mem_c").unwrap().active);
        assert_eq!(graph.active_memories().count(), 2);
    }
}

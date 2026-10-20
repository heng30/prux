//! 词法召回：BM25 打分、最近条目排序与注入块格式化。
//!
//! prux 没有 embedding 栈（见 `migrations/migration-jcode-v0.94.0.md` §4.15），
//! 因此召回是**纯词法**的：查询先经 [`crate::utils::text::terms`] 切成词元
//! （中日韩按二字组合展开），再按 BM25 在有效条目上打分。
//!
//! 这里只有纯函数：拿条目、给排序、拼文本。读盘在 [`super::store`]，注入时机在
//! [`super`] 的钩子里。

use super::graph::{MemoryCategory, MemoryEntry, MemoryScope};
use crate::utils::text;
use std::collections::HashMap;

/// 查询最多取用的词元个数（防止超长查询把打分拖慢）。
pub(super) const MAX_QUERY_TERMS: usize = 12;

/// 单个词元在一条记忆里计数的上限。
const MAX_TERM_HITS: usize = 12;

/// BM25 的词频饱和参数。
const BM25_K1: f64 = 1.2;

/// BM25 的长度归一化参数。
const BM25_B: f64 = 0.75;

/// 注入块的标题行（同时充当「这是自动注入」的标记）。
const BLOCK_HEADER: &str = "[memory] Durable notes recalled from earlier sessions. They may be stale — verify before trusting.";

/// 注入块里分类小节的顺序与标题。
const SECTION_ORDER: &[(MemoryCategory, &str)] = &[
    (MemoryCategory::Correction, "Corrections"),
    (MemoryCategory::Fact, "Facts"),
    (MemoryCategory::Preference, "Preferences"),
    (MemoryCategory::Entity, "Entities"),
];

/// 一条带分数与归属的召回结果。
#[derive(Debug, Clone)]
pub(super) struct Ranked {
    /// 条目本体。
    pub(super) entry: MemoryEntry,
    /// 所属存储。
    pub(super) scope: MemoryScope,
    /// BM25 分数（或「最近」排序用的综合分）。
    pub(super) score: f64,
}

/// 查询切词；空向量表示查询里没有可检索的关键词。
pub(super) fn query_terms(query: &str) -> Vec<String> {
    text::terms(query, MAX_QUERY_TERMS)
}

/// BM25 召回：返回按分数降序的前 `limit` 条（只含命中至少一个词元的条目）。
///
/// 排序是确定性的：分数降序 → 更新时间降序 → id 升序，因此同样的图与查询必得同样的
/// 文本，注入块不会因为哈希顺序而每轮抖动（那会把 prompt cache 打掉）。
pub(super) fn rank(
    entries: &[(MemoryScope, &MemoryEntry)],
    query: &str,
    limit: usize,
) -> Vec<Ranked> {
    if limit == 0 {
        return Vec::new();
    }
    let terms = query_terms(query);
    if terms.is_empty() || entries.is_empty() {
        return Vec::new();
    }

    let docs: Vec<(MemoryScope, &MemoryEntry, String, f64)> = entries
        .iter()
        .map(|(scope, entry)| {
            let haystack = entry.searchable_text();
            let len = haystack.chars().count() as f64;
            (*scope, *entry, haystack, len)
        })
        .collect();

    let total = docs.len() as f64;
    let avg_len = docs.iter().map(|(_, _, _, len)| *len).sum::<f64>() / total;

    // 词元 → 文档频率
    let mut doc_freq: HashMap<&str, usize> = HashMap::new();
    for term in &terms {
        let df = docs
            .iter()
            .filter(|(_, _, haystack, _)| haystack.contains(term.as_str()))
            .count();
        doc_freq.insert(term.as_str(), df);
    }

    let mut ranked: Vec<Ranked> = Vec::new();
    for (scope, entry, haystack, len) in &docs {
        let mut score = 0.0;
        for term in &terms {
            let tf = count_hits(haystack, term);
            if tf == 0 {
                continue;
            }
            let df = *doc_freq.get(term.as_str()).unwrap_or(&0) as f64;
            let idf = (1.0 + (total - df + 0.5) / (df + 0.5)).ln();
            let norm = BM25_K1 * (1.0 - BM25_B + BM25_B * len / avg_len.max(1.0));
            score += idf * (tf as f64 * (BM25_K1 + 1.0)) / (tf as f64 + norm);
        }
        if score <= 0.0 {
            continue;
        }
        ranked.push(Ranked {
            entry: (*entry).clone(),
            scope: *scope,
            score,
        });
    }

    sort_ranked(&mut ranked);
    ranked.truncate(limit);
    ranked
}

/// 无查询词时的「最近/最重要」条目（按 [`MemoryEntry::memory_score`] 排序）。
pub(super) fn recent(entries: &[(MemoryScope, &MemoryEntry)], limit: usize) -> Vec<Ranked> {
    let mut ranked: Vec<Ranked> = entries
        .iter()
        .map(|(scope, entry)| Ranked {
            entry: (*entry).clone(),
            scope: *scope,
            score: entry.memory_score(),
        })
        .filter(|ranked| ranked.score > 0.0)
        .collect();
    sort_ranked(&mut ranked);
    ranked.truncate(limit);
    ranked
}

/// 稳定的排序：分数降序 → 更新时间降序 → id 升序。
fn sort_ranked(ranked: &mut [Ranked]) {
    ranked.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| b.entry.updated_ms().cmp(&a.entry.updated_ms()))
            .then_with(|| a.entry.id.cmp(&b.entry.id))
    });
}

/// 一个词元在文本里出现的次数（封顶 [`MAX_TERM_HITS`]）。
fn count_hits(haystack: &str, term: &str) -> usize {
    if term.is_empty() {
        return 0;
    }
    haystack.matches(term).take(MAX_TERM_HITS).count()
}

/// 拼注入块（无命中返回 `None`）。
///
/// 块内容是**确定性**的：同样的条目序列必得同样的字符串。超出 `budget_chars` 时按
/// 顺序截断（整条丢弃，不切半句）。
pub(super) fn format_block(items: &[Ranked], budget_chars: usize) -> Option<String> {
    if items.is_empty() || budget_chars == 0 {
        return None;
    }

    let mut body = String::new();
    let mut used = BLOCK_HEADER.chars().count();

    for (category, title) in SECTION_ORDER {
        let mut section = String::new();
        for item in items.iter().filter(|item| item.entry.category == *category) {
            let line = format!("- {}\n", single_line(&item.entry.content));
            let cost = line.chars().count() + title.len() + 4;
            if used + cost > budget_chars {
                break;
            }
            used += cost;
            section.push_str(&line);
        }
        if !section.is_empty() {
            body.push_str("\n## ");
            body.push_str(title);
            body.push('\n');
            body.push_str(&section);
        }
    }

    if body.is_empty() {
        return None;
    }
    Some(format!("{BLOCK_HEADER}\n{body}"))
}

/// 把正文压成一行（注入块里每条只占一行，换行会打乱列表结构）。
pub(super) fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条项目作用域的记忆。
    fn entry(category: MemoryCategory, content: &str) -> (MemoryScope, MemoryEntry) {
        (MemoryScope::Project, MemoryEntry::new(category, content))
    }

    #[test]
    fn bm25_prefers_entries_with_more_query_terms() {
        let a = entry(MemoryCategory::Fact, "repo uses pnpm for install");
        let b = entry(MemoryCategory::Fact, "repo uses cargo for builds");
        let c = entry(MemoryCategory::Fact, "unrelated note about colors");
        let items = [a, b, c];
        let refs: Vec<(MemoryScope, &MemoryEntry)> =
            items.iter().map(|(scope, entry)| (*scope, entry)).collect();

        let ranked = rank(&refs, "pnpm install", 10);
        assert_eq!(ranked.len(), 1, "只有命中词元的条目入选");
        assert!(ranked[0].entry.content.contains("pnpm"));
        assert!(ranked[0].score > 0.0);
    }

    #[test]
    fn bm25_is_deterministic_for_equal_scores() {
        let a = entry(MemoryCategory::Fact, "alpha beta");
        let b = entry(MemoryCategory::Fact, "alpha beta");
        let refs = vec![(a.0, &a.1), (b.0, &b.1)];
        let first = rank(&refs, "alpha beta", 2);
        let second = rank(&refs, "alpha beta", 2);
        let ids = |r: &[Ranked]| r.iter().map(|x| x.entry.id.clone()).collect::<Vec<_>>();
        assert_eq!(ids(&first), ids(&second), "同分时按 id 稳定排序");
    }

    #[test]
    fn bm25_ignores_stop_word_only_and_empty_queries() {
        let a = entry(MemoryCategory::Fact, "anything");
        let refs = vec![(a.0, &a.1)];
        assert!(rank(&refs, "the and of", 5).is_empty());
        assert!(rank(&refs, "", 5).is_empty());
        assert!(rank(&refs, "anything", 0).is_empty(), "limit 0 直接空");
    }

    #[test]
    fn cjk_queries_match_via_bigrams() {
        let a = entry(
            MemoryCategory::Preference,
            "这个仓库用 make check 而不是 cargo test",
        );
        let b = entry(MemoryCategory::Fact, "unrelated");
        let refs = vec![(a.0, &a.1), (b.0, &b.1)];
        let ranked = rank(&refs, "仓库检查", 5);
        // 「仓库」是原文子串，「检查」不是（原文是 make check），至少命中一个即入选
        assert!(
            ranked
                .iter()
                .any(|r| r.entry.content.contains("make check"))
        );
    }

    #[test]
    fn recent_orders_by_memory_score_then_recency() {
        let mut a = entry(MemoryCategory::Fact, "old").1;
        a.updated_at = "2020-01-01T00:00:00.000Z".to_string();
        let b = entry(MemoryCategory::Correction, "new correction").1;
        let refs = vec![(MemoryScope::Project, &a), (MemoryScope::Global, &b)];
        let ranked = recent(&refs, 10);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].entry.content, "new correction", "纠正类权重最高");
        assert_eq!(ranked[0].scope, MemoryScope::Global);
    }

    #[test]
    fn block_is_stable_and_respects_budget() {
        let a = entry(MemoryCategory::Fact, "fact one");
        let b = entry(MemoryCategory::Correction, "correction one");
        let mut ranked = vec![
            Ranked {
                entry: b.1.clone(),
                scope: b.0,
                score: 2.0,
            },
            Ranked {
                entry: a.1.clone(),
                scope: a.0,
                score: 1.0,
            },
        ];
        sort_ranked(&mut ranked);

        let block = format_block(&ranked, 8000).unwrap();
        assert!(block.starts_with("[memory]"));
        assert!(block.contains("## Corrections"), "小节按固定顺序: {block}");
        assert!(block.contains("- correction one"));
        assert!(block.contains("## Facts"));
        assert_eq!(block, format_block(&ranked, 8000).unwrap(), "两次同文本");

        assert_eq!(format_block(&ranked, 0), None, "预算 0 关闭注入");
        assert_eq!(format_block(&[], 8000), None);
        let tiny = format_block(&ranked, 20);
        assert!(tiny.is_none(), "放不下任何一条时返回 None: {tiny:?}");
    }

    #[test]
    fn block_flattens_multiline_content() {
        let a = entry(MemoryCategory::Fact, "line one\nline two").1;
        let ranked = vec![Ranked {
            entry: a,
            scope: MemoryScope::Project,
            score: 1.0,
        }];
        let block = format_block(&ranked, 8000).unwrap();
        assert!(block.contains("- line one line two"), "{block}");
    }
}

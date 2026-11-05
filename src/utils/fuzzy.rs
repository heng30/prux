//! 模糊匹配
//!
//! 匹配要求：查询字符按顺序出现在文本中（不必连续）。分数越低越优：
//! 连续匹配奖励、词边界奖励、靠后位置惩罚、完全一致额外大奖。
//! 数字/字母连读（如 "v4" / "4v"）二次尝试交换匹配。
//!
//! 另带按**编辑距离**判定的近似匹配：[`edit_distance`] 与 [`closest_match`]（后者先试归一化
//! 相等，再用距离兑“是不是拼错了”），供键名/标识符的 typo 提示使用。

/// 计算 query 对 text 的匹配分；不匹配返回 None。分数越低表示匹配质量越高
fn fuzzy_match_score(query_lower: &[char], text_lower: &[char]) -> Option<f64> {
    if query_lower.is_empty() {
        return Some(0.0);
    }
    if query_lower.len() > text_lower.len() {
        return None;
    }

    let mut query_index = 0usize;
    let mut score = 0.0f64;
    let mut last_match_index = -1isize;
    let mut consecutive_matches = 0i32;

    for (i, &c) in text_lower.iter().enumerate() {
        if query_index >= query_lower.len() {
            break;
        }
        if c == query_lower[query_index] {
            let is_word_boundary =
                i == 0 || matches!(text_lower[i - 1], ' ' | '\t' | '-' | '_' | '.' | '/' | ':');

            if last_match_index == i as isize - 1 {
                consecutive_matches += 1;
                score -= (consecutive_matches as f64) * 5.0;
            } else {
                consecutive_matches = 0;
                if last_match_index >= 0 {
                    score += (i as isize - last_match_index - 1) as f64 * 2.0;
                }
            }

            if is_word_boundary {
                score -= 10.0;
            }
            score += i as f64 * 0.1;

            last_match_index = i as isize;
            query_index += 1;
        }
    }

    if query_index < query_lower.len() {
        return None;
    }

    if query_lower == text_lower {
        score -= 100.0;
    }

    Some(score)
}

/// 主匹配；不中且为「字母+数字」或「数字+字母」连读时交换两段再试一次（+5 惩罚）。
pub fn fuzzy_match(query: &str, text: &str) -> Option<f64> {
    let query_lower: Vec<char> = query.to_lowercase().chars().collect();
    let text_lower: Vec<char> = text.to_lowercase().chars().collect();

    let primary = fuzzy_match_score(&query_lower, &text_lower);
    if let Some(s) = primary {
        return Some(s);
    }

    // 数字/字母 swap：如 "v4" <-> "4v"
    let qs: String = query_lower.iter().collect();
    let alpha: String = qs.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let digits: String = qs.chars().skip_while(|c| c.is_ascii_alphabetic()).collect();
    if alpha.is_empty() || digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }

    let swapped: Vec<char> = format!("{}{}", digits, alpha).chars().collect();
    fuzzy_match_score(&swapped, &text_lower).map(|s| s + 5.0)
}

/// 按空格/斜杠分词：所有 token 都必须匹配，总分为各 token 分数之和，升序返回。
pub fn fuzzy_filter<'a, T>(
    items: &'a [T],
    query: &str,
    get_text: impl Fn(&'a T) -> &str,
) -> Vec<&'a T> {
    let tokens: Vec<String> = query
        .trim()
        .split([' ', '/'])
        .filter(|t| !t.is_empty())
        .map(|t| t.to_string())
        .collect();

    if tokens.is_empty() {
        return items.iter().collect();
    }

    let mut results: Vec<(&T, f64)> = Vec::new();
    for item in items {
        let text = get_text(item);
        let mut total = 0.0;
        let mut ok = true;
        for tok in &tokens {
            match fuzzy_match(tok, text) {
                Some(s) => total += s,
                None => {
                    ok = false;
                    break;
                }
            }
        }
        if ok {
            results.push((item, total));
        }
    }

    // 稳定排序保持同分时的原始相对顺序（pi 用稳定 sort）
    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    results.into_iter().map(|(item, _)| item).collect()
}

/// 路径候选打分：文件名完全一致 > 文件名开头 > 文件名子串 > 仅完整路径子串，
/// 命中时目录额外 +10；不命中返回 0。分数越高越靠前。
pub fn path_score(path: &str, query: &str, is_dir: bool) -> i32 {
    let lower_path = path.to_lowercase();
    let lower_query = query.to_lowercase();
    let name = lower_path.rsplit('/').next().unwrap_or(&lower_path);

    let mut score = if lower_query.is_empty() {
        1
    } else if name == lower_query {
        100
    } else if name.starts_with(&lower_query) {
        80
    } else if name.contains(&lower_query) {
        50
    } else if lower_path.contains(&lower_query) {
        30
    } else {
        0
    };

    if is_dir && score > 0 {
        score += 10;
    }
    score
}

/// 两串的 Levenshtein 距离（键名 / 短标识符这类短串够用，两行 DP）。
pub fn edit_distance(left: &str, right: &str) -> usize {
    let right: Vec<char> = right.chars().collect();
    let mut prev: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];

    for (i, left_char) in left.chars().enumerate() {
        current[0] = i + 1;
        for (j, right_char) in right.iter().enumerate() {
            let cost = usize::from(left_char != *right_char);
            current[j + 1] = (prev[j] + cost).min(prev[j + 1] + 1).min(current[j] + 1);
        }
        std::mem::swap(&mut prev, &mut current);
    }

    prev[right.len()]
}

/// 候选表里与 `query` 最接近的一项；“最近”先看归一化相等，再看编辑距离。
///
/// 先按 [`normalize_key`]（小写、去连字符）精确匹配：`theme_gui` / `ThemeGui` 这类写法差异
/// 不该靠距离猜；否则取编辑距离 ≤ `max_distance` 的最小者，并列时取字典序在前者（输出稳定）。
/// 没有足够接近的候选时返回 `None`。
pub fn closest_match<'a>(
    query: &str,
    candidates: &[&'a str],
    max_distance: usize,
) -> Option<&'a str> {
    let target = normalize_key(query);
    if let Some(exact) = candidates.iter().find(|c| normalize_key(c) == target) {
        return Some(*exact);
    }

    candidates
        .iter()
        .filter_map(|candidate| {
            let distance = edit_distance(&target, &normalize_key(candidate));
            (distance <= max_distance).then_some((distance, *candidate))
        })
        .min_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(right.1)))
        .map(|(_, found)| found)
}

/// 键名归一化：小写并去掉连字符（`auto-compact` → `autocompact`）。
fn normalize_key(key: &str) -> String {
    key.to_ascii_lowercase().replace('-', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_match_ranks_before_substring() {
        // /lo: login 首字符即命中（词边界 -10），clone 的 l 在中间
        let login = fuzzy_match("lo", "login").unwrap();
        let clone = fuzzy_match("lo", "clone").unwrap();
        assert!(
            login < clone,
            "login({login}) should rank above clone({clone})"
        );
    }

    #[test]
    fn substring_requires_in_order() {
        // "lo" 需 l 在 o 前按序出现
        assert!(fuzzy_match("lo", "login").is_some());
        assert!(fuzzy_match("lo", "clone").is_some());
        // l 在 o 之后 -> 不中（alphabetical 中 o 先于 l 出现，但需连续顺序……ole 中 o 前 l 后）
        assert!(fuzzy_match("lo", "ahol").is_none());
    }

    #[test]
    fn exact_match_wins_big() {
        let exact = fuzzy_match("model", "model").unwrap();
        let prefix = fuzzy_match("m", "model").unwrap();
        assert!(exact < prefix);
    }

    #[test]
    fn filter_sorts_all_tokens() {
        let names = ["login", "clone", "model", "export"];
        let got: Vec<&str> = fuzzy_filter(&names, "lo", |s| s)
            .into_iter()
            .copied()
            .collect();
        // login(0 词边界) 在 clone 前面；model/export 不含 lo
        assert_eq!(got, vec!["login", "clone"]);
    }

    #[test]
    fn edit_distance_counts_insertions_deletions_and_substitutions() {
        assert_eq!(edit_distance("theme", "theme"), 0);
        assert_eq!(edit_distance("theme", "themes"), 1);
        assert_eq!(edit_distance("theme", "thme"), 1);
        assert_eq!(edit_distance("theme", "thime"), 1);
        assert_eq!(edit_distance("", "abc"), 3);
    }

    /// 归一化相等优先于编辑距离（`theme_gui` 不会被猜成 `theme`）；超阈值不给建议。
    #[test]
    fn closest_match_prefers_normalized_equality_then_distance() {
        let candidates = ["theme", "theme-gui", "sidebar-gui", "compactReserveTokens"];

        assert_eq!(
            closest_match("theme_gui", &candidates, 2),
            Some("theme-gui")
        );
        assert_eq!(closest_match("ThemeGui", &candidates, 2), Some("theme-gui"));
        assert_eq!(
            closest_match("sidebar_gui", &candidates, 2),
            Some("sidebar-gui")
        );
        assert_eq!(
            closest_match("compactReserveToken", &candidates, 2),
            Some("compactReserveTokens")
        );
        assert_eq!(closest_match("zzzzzzzzz", &candidates, 2), None);
        // 阈值由调用方决定：距离 1 的候选在 max_distance = 0 时不给
        assert_eq!(closest_match("themes", &candidates, 0), None);
    }

    /// 打分优先级：文件名完全匹配=100 > 开头=80 > 子串=50 > 仅完整路径子串=30；目录 +10。
    #[test]
    fn score_ranks_filenames_before_paths() {
        assert_eq!(path_score("main.rs", "main.rs", false), 100);
        assert_eq!(path_score("main.rs", "main", false), 80);
        assert_eq!(path_score("xmain.rs", "main", false), 50);
        assert_eq!(path_score("src/main", "src", false), 30);
        assert!(path_score("main.rs", "main", false) > path_score("xmain.rs", "main", false));
        assert!(path_score("xmain.rs", "main", false) > path_score("src/main", "src", false));
        assert_eq!(path_score("adir", "ad", true), 90);
        // 不命中 = 0；空查询每条都算命中（排序只剩层级/长度/字典序）
        assert_eq!(path_score("main.rs", "zzz", false), 0);
        assert_eq!(path_score("main.rs", "", false), 1);
    }
}

//! 模糊匹配
//!
//! 匹配要求：查询字符按顺序出现在文本中（不必连续）。分数越低越优：
//! 连续匹配奖励、词边界奖励、靠后位置惩罚、完全一致额外大奖。
//! 数字/字母连读（如 "v4" / "4v"）二次尝试交换匹配。

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
}

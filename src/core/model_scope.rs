//! scope pattern 解析。
//!
//! 输入是模型 pattern 列表（`--models` CLI / settings 的 `enabledModels`）：
//! - 含 glob 字符（`*`/`?`/`[`）的 pattern：对 `provider/id` 与裸 id 做 nocase
//!   glob 匹配（`*` 跨 `/`，`[...]` 支持区间与 `!`/`^` 取反）；
//! - 不含 glob 字符的 pattern：先精确匹配（`provider/id` / 裸 id，nocase，
//!   歧义视为不匹配），再退化为 id/name 包含匹配（偏好非日期版本），
//!   最后尝试剥离 `:thinkingLevel` 后缀递归解析。
//!
//! 每个匹配携带可选的 thinking level（`pattern:level` 后缀）。无匹配产出
//! no-match 诊断；非法 thinking 后缀产出 warning 诊断并照常解析（对齐 pi
//! scope 模式的 `allowInvalidThinkingLevelFallback`）。

use crate::cli::args::VALID_THINKING_LEVELS;

/// scope 中的一个模型：canonical `provider/id` + 可选 thinking level
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedModel {
    /// 提供方标识（canonical 路径 `/` 前一段）。
    pub provider: String,
    /// 模型 id，不含 provider 前缀。
    pub id: String,
    /// pattern `:level` 后缀指定的思考档位；未指定为 None。
    pub thinking: Option<String>,
}

impl ScopedModel {
    /// 拼出 canonical `provider/id` 路径。
    pub fn canonical(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// 可用模型目录条目（name 仅用于非 glob 的包含匹配）
#[derive(Debug, Clone)]
pub struct AvailableModel {
    /// 提供方标识（canonical 路径 `/` 前一段）。
    pub provider: String,
    /// 模型 id，不含 provider 前缀。
    pub id: String,
    /// 展示名，仅用于非 glob pattern 的包含匹配。
    pub name: String,
}

impl AvailableModel {
    /// 拼出 canonical `provider/id` 路径。
    pub fn canonical(&self) -> String {
        format!("{}/{}", self.provider, self.id)
    }
}

/// 解析诊断
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeDiagnostic {
    /// pattern 未匹配任何模型
    NoMatch { pattern: String },
    /// pattern 带非法 thinking 后缀（仍按前缀解析，thinking 忽略）
    InvalidThinkingLevel { pattern: String },
}

/// 解析结果：scoped 有序列表（去重保留首个）+ 诊断
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResolveResult {
    /// 解析出的模型列表，按 pattern 顺序去重（保留首次出现的 thinking）。
    pub scoped: Vec<ScopedModel>,
    /// 解析诊断：无匹配或非法 thinking 后缀。
    pub diagnostics: Vec<ScopeDiagnostic>,
}

/// 日期版本 id 形如 `claude-sonnet-4-5-20250929`（尾部 `-YYYYMMDD`）
fn is_dated_id(id: &str) -> bool {
    let Some(dash) = id.rfind('-') else {
        return false;
    };
    let tail = &id[dash + 1..];
    tail.len() == 8 && tail.chars().all(|c| c.is_ascii_digit())
}

/// pattern 是否含 glob 字符（决定走 glob 分支还是 parseModelPattern 分支）
pub fn has_glob_chars(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?') || pattern.contains('[')
}

/// 解析 pattern 列表为 scope
pub fn resolve_model_scope(patterns: &[String], available: &[AvailableModel]) -> ResolveResult {
    let mut result = ResolveResult::default();
    for pattern in patterns {
        if has_glob_chars(pattern) {
            resolve_glob_pattern(pattern, available, &mut result);
        } else {
            resolve_single_pattern(pattern, available, &mut result);
        }
    }
    result
}

/// glob 分支：`pattern[:level]`，对 whole `provider/id` 与裸 id 匹配（nocase）
fn resolve_glob_pattern(pattern: &str, available: &[AvailableModel], result: &mut ResolveResult) {
    let (glob, thinking) = strip_thinking_suffix(pattern);

    // 先整串精确匹配（罕见；一个同时是某模型 id 的 glob 字面量）
    if let Some(id) = exact_canonical_or_id(&glob, available) {
        push_scoped(result, id, thinking);
        return;
    }

    let mut matched = false;
    for m in available {
        let full = m.canonical();
        if glob_match(&glob, &full) || glob_match(&glob, &m.id) {
            push_scoped(result, &m.canonical(), thinking.clone());
            matched = true;
        }
    }

    if !matched {
        result.diagnostics.push(ScopeDiagnostic::NoMatch {
            pattern: pattern.to_string(),
        });
    }
}

/// 非 glob 分支：对齐 pi `parseModelPattern`（精确 → 包含匹配 → `:level` 后缀递归）
fn resolve_single_pattern(pattern: &str, available: &[AvailableModel], result: &mut ResolveResult) {
    let (model, diag) = parse_single_pattern(pattern, available);
    if let Some(d) = diag {
        result.diagnostics.push(d);
    }

    if let Some(m) = model {
        push_scoped(result, &m.canonical(), m.thinking);
    } else if result.diagnostics.is_empty() {
        // parse_single_pattern 未命中且无诊断（如非法后缀且无兜底）：补 no-match
        result.diagnostics.push(ScopeDiagnostic::NoMatch {
            pattern: pattern.to_string(),
        });
    }
}

/// 单 pattern 解析：返回 (模型, 诊断)。诊断仅由非法 thinking 后缀产生。
fn parse_single_pattern(
    pattern: &str,
    available: &[AvailableModel],
) -> (Option<ScopedModel>, Option<ScopeDiagnostic>) {
    // 精确匹配（canonical / provider+id / 裸 id，nocase，歧义不匹配）
    if let Some(id) = find_exact(pattern, available) {
        // find_exact 的所有返回路径都来自 AvailableModel::canonical()（"provider/id"），必然含 '/'，split 不会失败；
        // expect 分支仅防御不变量，触发即意味着 find_exact 被人改动后违反了返回约定。
        let (provider, model_id) =
            split_canonical(&id).expect("find_exact must return canonical provider/id");
        return (
            Some(ScopedModel {
                provider,
                id: model_id,
                thinking: None,
            }),
            None,
        );
    }

    // 退化：id/name 包含匹配（偏好非日期版本，排序取高）
    if let Some(id) = fuzzy_single_match(pattern, available) {
        // 同上：fuzzy_single_match 只返回 canonical() 构造的字符串，必然含 '/'。
        let (provider, model_id) =
            split_canonical(&id).expect("fuzzy match must return canonical provider/id");
        return (
            Some(ScopedModel {
                provider,
                id: model_id,
                thinking: None,
            }),
            None,
        );
    }

    // 无匹配：剥离最后一个 `:` 后缀递归（模型 id 可能含冒号）
    let Some(last_colon) = pattern.rfind(':') else {
        return (None, None);
    };
    let prefix = &pattern[..last_colon];
    let suffix = &pattern[last_colon + 1..];

    let (model, _) = parse_single_pattern(prefix, available);
    if let Some(m) = model {
        if VALID_THINKING_LEVELS.contains(&suffix) {
            return (
                Some(ScopedModel {
                    thinking: Some(suffix.to_string()),
                    ..m
                }),
                None,
            );
        }

        return (
            Some(m),
            Some(ScopeDiagnostic::InvalidThinkingLevel {
                pattern: pattern.to_string(),
            }),
        );
    }
    (None, None)
}

/// 整串精确匹配 `provider/id` 或裸 id（nocase，歧义不匹配）；返回 canonical `provider/id`。
fn find_exact(pattern: &str, available: &[AvailableModel]) -> Option<String> {
    let trimmed = pattern.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_lowercase();

    let canonical: Vec<&AvailableModel> = available
        .iter()
        .filter(|m| m.canonical().to_lowercase() == lower)
        .collect();
    if canonical.len() == 1 {
        return Some(canonical[0].canonical());
    }
    if canonical.len() > 1 {
        return None;
    }

    if let Some((provider, id)) = trimmed.split_once('/') {
        let provider = provider.trim();
        let id = id.trim();
        if !provider.is_empty() && !id.is_empty() {
            let provider_matches: Vec<&AvailableModel> = available
                .iter()
                .filter(|m| {
                    m.provider.to_lowercase() == provider.to_lowercase()
                        && m.id.to_lowercase() == id.to_lowercase()
                })
                .collect();
            if provider_matches.len() == 1 {
                return Some(provider_matches[0].canonical());
            }
            if provider_matches.len() > 1 {
                return None;
            }
        }
    }

    let id_matches: Vec<&AvailableModel> = available
        .iter()
        .filter(|m| m.id.to_lowercase() == lower)
        .collect();
    if id_matches.len() == 1 {
        return Some(id_matches[0].canonical());
    }
    None
}

/// canonical 或裸 id 的整串精确匹配（glob 分支用；返回 id 字符串）
fn exact_canonical_or_id<'a>(pattern: &str, available: &'a [AvailableModel]) -> Option<&'a str> {
    let lower = pattern.trim().to_lowercase();
    let canonical: Vec<&AvailableModel> = available
        .iter()
        .filter(|m| m.canonical().to_lowercase() == lower)
        .collect();
    if canonical.len() == 1 {
        return Some(&canonical[0].id);
    }
    if canonical.len() > 1 {
        return None;
    }
    let id_matches: Vec<&AvailableModel> = available
        .iter()
        .filter(|m| m.id.to_lowercase() == lower)
        .collect();
    if id_matches.len() == 1 {
        return Some(&id_matches[0].id);
    }
    None
}

/// 包含匹配：id 或 name 含 pattern（nocase）。命中多个时偏好非日期版本，其余按 id 降序取首个。
fn fuzzy_single_match(pattern: &str, available: &[AvailableModel]) -> Option<String> {
    let lower = pattern.to_lowercase();
    let mut matches: Vec<&AvailableModel> = available
        .iter()
        .filter(|m| m.id.to_lowercase().contains(&lower) || m.name.to_lowercase().contains(&lower))
        .collect();
    if matches.is_empty() {
        return None;
    }

    matches.sort_by(|a, b| {
        let a_alias = !is_dated_id(&a.id);
        let b_alias = !is_dated_id(&b.id);
        b_alias.cmp(&a_alias).then_with(|| b.id.cmp(&a.id))
    });
    Some(matches[0].canonical())
}

/// 去重追加（保留首个出现的顺序与 thinking）
fn push_scoped(result: &mut ResolveResult, canonical: &str, thinking: Option<String>) {
    let Some((provider, id)) = split_canonical(canonical) else {
        return;
    };

    if result.scoped.iter().any(|s| s.canonical() == canonical) {
        return;
    }
    result.scoped.push(ScopedModel {
        provider,
        id,
        thinking,
    });
}

/// 按第一个 `/` 拆出 (provider, id)；不含 `/` 时返回 `None`。
fn split_canonical(canonical: &str) -> Option<(String, String)> {
    canonical
        .split_once('/')
        .map(|(p, m)| (p.to_string(), m.to_string()))
}

/// 剥离合法 thinking 后缀（`pattern:level`）；模型 id 含冒号时后缀非法则不剥离。
fn strip_thinking_suffix(pattern: &str) -> (String, Option<String>) {
    if let Some(colon) = pattern.rfind(':') {
        let suffix = &pattern[colon + 1..];
        if VALID_THINKING_LEVELS.contains(&suffix) {
            return (pattern[..colon].to_string(), Some(suffix.to_string()));
        }
    }
    (pattern.to_string(), None)
}

/// glob 匹配（nocase，`*` 跨段，`?` 单字符，`[...]` 字符类支持区间与取反）。未闭合的 `[` 视为字面字符。
pub fn glob_match(pattern: &str, text: &str) -> bool {
    glob_match_chars(
        &pattern.chars().collect::<Vec<char>>(),
        &text.chars().collect::<Vec<char>>(),
    )
}

/// 逐字符 glob 匹配：`?` 匹配单字符、`*` 可跨 `/`、`[...]` 为字符类；
/// 字母不区分大小写（按 ASCII 忽略）。
fn glob_match_chars(p: &[char], t: &[char]) -> bool {
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<usize> = None; // 最近一次 `*` 的 pattern 下标
    let mut star_ti = 0usize; // 尝试回溯时的 text 下标

    while ti < t.len() {
        if pi < p.len() && p[pi] == '?' {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '[' && has_closing_bracket(p, pi) {
            let (matched, next) = match_char_class(p, pi, t[ti]);
            if matched {
                pi = next;
                ti += 1;
            } else if let Some(s) = star {
                pi = s + 1;
                star_ti += 1;
                ti = star_ti;
            } else {
                return false;
            }
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if pi < p.len() && p[pi].eq_ignore_ascii_case(&t[ti]) {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }

    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// 判断 `p[start]`（应为 `[`）之后是否存在闭合 `]`；无则调用方把 `[` 当字面字符。
fn has_closing_bracket(p: &[char], start: usize) -> bool {
    p[start + 1..].contains(&']')
}

/// 解析 `[abc]`/`[a-z]`/`[!abc]`；返回 (是否命中, 消费到的下标)
fn match_char_class(p: &[char], start: usize, c: char) -> (bool, usize) {
    let mut i = start + 1;
    let mut negate = false;
    if i < p.len() && (p[i] == '!' || p[i] == '^') {
        negate = true;
        i += 1;
    }

    let cc = c.to_ascii_lowercase();
    let mut matched = false;
    while i < p.len() && p[i] != ']' {
        if i + 2 < p.len() && p[i + 1] == '-' && p[i + 2] != ']' {
            let lo = p[i].to_ascii_lowercase();
            let hi = p[i + 2].to_ascii_lowercase();
            if lo <= cc && cc <= hi {
                matched = true;
            }
            i += 3;
        } else {
            if p[i].eq_ignore_ascii_case(&c) {
                matched = true;
            }
            i += 1;
        }
    }

    // 有闭合 `]`（调用方已保证）→ 消费它
    let next = if i < p.len() { i + 1 } else { i };
    (matched != negate, next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avail(provider: &str, id: &str) -> AvailableModel {
        AvailableModel {
            provider: provider.to_string(),
            id: id.to_string(),
            name: id.to_string(),
        }
    }

    fn catalog() -> Vec<AvailableModel> {
        vec![
            avail("deepseek", "deepseek-v4-pro"),
            avail("deepseek", "deepseek-flash"),
            avail("deepseek", "deepseek-v4-flash-vision-exp"),
            avail("anthropic", "claude-sonnet-4-5"),
            avail("anthropic", "claude-sonnet-4-5-20250929"),
            avail("ollama", "llama3.1"),
            avail("ollama", "llama3.1:8b"), // 冒号在模型 id 内部
        ]
    }

    #[test]
    fn glob_matches_full_and_bare_ids() {
        let r = resolve_model_scope(&["claude-*".to_string()], &catalog());
        let ids: Vec<&str> = r.scoped.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["claude-sonnet-4-5", "claude-sonnet-4-5-20250929"],
            "裸 id glob 应匹配"
        );
        assert!(r.diagnostics.is_empty());
    }

    #[test]
    fn glob_provider_prefix() {
        let r = resolve_model_scope(&["deepseek/*".to_string()], &catalog());
        assert_eq!(r.scoped.len(), 3);
        assert!(r.scoped.iter().all(|s| s.provider == "deepseek"));
        // dedupe：同一模型不会被两次加入
        let r2 = resolve_model_scope(
            &[
                "deepseek/*".to_string(),
                "deepseek/deepseek-v4-pro".to_string(),
            ],
            &catalog(),
        );
        assert_eq!(r2.scoped.len(), 3);
    }

    #[test]
    fn nocase_glob() {
        let r = resolve_model_scope(&["CLAUDE-SONNET-*".to_string()], &catalog());
        assert_eq!(r.scoped.len(), 2, "glob 应忽略大小写");
    }

    #[test]
    fn glob_no_match_diagnostic() {
        let r = resolve_model_scope(&["gpt-*".to_string()], &catalog());
        assert!(r.scoped.is_empty());
        assert_eq!(
            r.diagnostics,
            vec![ScopeDiagnostic::NoMatch {
                pattern: "gpt-*".to_string()
            }]
        );
    }

    #[test]
    fn exact_provider_id() {
        let r = resolve_model_scope(&["anthropic/claude-sonnet-4-5".to_string()], &catalog());
        assert_eq!(r.scoped.len(), 1);
        assert_eq!(r.scoped[0].canonical(), "anthropic/claude-sonnet-4-5");
        assert_eq!(r.scoped[0].thinking, None);
    }

    #[test]
    fn exact_bare_id_case_insensitive() {
        let r = resolve_model_scope(&["DEEPSEEK-FLASH".to_string()], &catalog());
        assert_eq!(r.scoped.len(), 1);
        assert_eq!(r.scoped[0].id, "deepseek-flash");
    }

    #[test]
    fn fuzzy_prefers_alias_over_dated() {
        // 包含匹配：claude-sonnet 命中 alias 与 dated，偏好 alias
        let r = resolve_model_scope(&["claude-sonnet".to_string()], &catalog());
        assert_eq!(r.scoped.len(), 1);
        assert_eq!(r.scoped[0].id, "claude-sonnet-4-5");
        assert!(r.diagnostics.is_empty());
    }

    #[test]
    fn thinking_suffix_valid() {
        let r = resolve_model_scope(&["deepseek-v4-pro:high".to_string()], &catalog());
        assert_eq!(r.scoped.len(), 1);
        assert_eq!(r.scoped[0].thinking.as_deref(), Some("high"));
    }

    #[test]
    fn thinking_suffix_invalid_warns_but_resolves() {
        let r = resolve_model_scope(&["deepseek-v4-pro:ultra".to_string()], &catalog());
        assert_eq!(r.scoped.len(), 1, "非法后缀仍按前缀解析");
        assert_eq!(r.scoped[0].thinking, None);
        assert_eq!(
            r.diagnostics,
            vec![ScopeDiagnostic::InvalidThinkingLevel {
                pattern: "deepseek-v4-pro:ultra".to_string()
            }]
        );
    }

    #[test]
    fn colon_in_model_id_kept() {
        // ollama llama3.1:8b：后缀 8b 非法 → 递归前缀 llama3.1 命中
        let r = resolve_model_scope(&["llama3.1:8b".to_string()], &catalog());
        assert_eq!(r.scoped.len(), 1);
        assert_eq!(r.scoped[0].id, "llama3.1:8b");
        assert!(
            r.diagnostics.is_empty(),
            "模型 id 内冒号不算非法后缀：{:?}",
            r.diagnostics
        );
    }

    #[test]
    fn glob_with_thinking_suffix() {
        let r = resolve_model_scope(&["deepseek/*:high".to_string()], &catalog());
        assert_eq!(r.scoped.len(), 3);
        assert!(
            r.scoped
                .iter()
                .all(|s| s.thinking.as_deref() == Some("high"))
        );
    }

    #[test]
    fn glob_char_class() {
        // 字符类示例：deepseek 新目录 id（deepseek-flash / deepseek-v4-pro）
        assert!(glob_match("deepseek-[fv]*", "deepseek-flash"));
        assert!(glob_match("deepseek-[fv]*", "deepseek-v4-pro"));
        assert!(!glob_match("deepseek-[fv]*", "anthropic/claude-sonnet-4-5"));
        assert!(glob_match("deepseek-[!v]*", "deepseek-flash"));
        assert!(!glob_match("deepseek-[!v]*", "deepseek-v4-pro"));
        assert!(glob_match("deepseek-v4-[0-9]*", "deepseek-v4-4x"));
        assert!(glob_match("llama[0-9]*", "llama3.1"));
    }

    #[test]
    fn glob_question_mark_and_star_span() {
        assert!(glob_match("deepseek-v4-???", "deepseek-v4-pro"));
        assert!(!glob_match("deepseek-v4-????", "deepseek-v4-pro"));
        assert!(glob_match("deepseek/*-flash*", "deepseek/deepseek-flash"));
        assert!(glob_match("deepseek/*", "deepseek/deepseek-v4-pro"));
        assert!(!glob_match("deepseek/*", "anthropic/claude"));
    }
}

/// 单条 gitignore 风格规则：匹配模式、是否取反、是否仅匹配目录。
#[derive(Clone)]
pub struct IgnoreRule {
    /// 已剥去 `!` 前缀与尾部 `/` 的 gitignore 匹配模式
    pub pattern: String,
    /// 是否由 `!` 取反：true 时命中该规则表示重新纳入
    pub negated: bool,
    /// 是否仅匹配目录（原模式以 `/` 结尾），非目录直接跳过
    pub dir_only: bool,
}

/// 按 gitignore 语义从后往前求值的一组规则（后规则优先）。
#[derive(Clone, Default)]
pub struct IgnoreMatcher {
    /// 按文件出现顺序收集的规则，求值时从后往前（后规则优先）
    pub rules: Vec<IgnoreRule>,
}

/// gitignore 风格的段匹配：* 段内任意、? 单字符，均不跨 /
pub fn glob_segment(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();

    /// 递归匹配：模式字符逐个消耗文本字符，`*` 回溯尝试所有剩余长度
    #[stacksafe::stacksafe]
    fn go(p: &[char], t: &[char]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some('*') => (0..=t.len()).any(|i| go(&p[1..], &t[i..])),
            Some('?') => !t.is_empty() && go(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && go(&p[1..], &t[1..]),
        }
    }
    go(&p, &t)
}

/// 路径 glob：支持 **（跨 /，含零层）、* 与 ?（不跨 /）
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let pat: Vec<&str> = pattern.split('/').filter(|s| !s.is_empty()).collect();
    let txt: Vec<&str> = text.split('/').filter(|s| !s.is_empty()).collect();

    /// 递归匹配路径段：`**` 可吞掉任意多层目录（含零层），其余段走段内匹配
    #[stacksafe::stacksafe]
    fn go(pat: &[&str], txt: &[&str]) -> bool {
        if pat.is_empty() {
            return txt.is_empty();
        }
        if pat[0] == "**" {
            (0..=txt.len()).any(|i| go(&pat[1..], &txt[i..]))
        } else {
            !txt.is_empty() && glob_segment(pat[0], txt[0]) && go(&pat[1..], &txt[1..])
        }
    }
    go(&pat, &txt)
}

/// 规则匹配：无 / 的模式按任意层级 basename 匹配；否则相对根路径匹配
pub fn rule_matches(rule: &IgnoreRule, text: &str) -> bool {
    if rule.pattern.contains('/') {
        glob_match(&rule.pattern, text)
    } else {
        text.split('/').any(|seg| glob_segment(&rule.pattern, seg))
    }
}

/// 后规则优先（gitignore 语义）：从后往前找首个匹配，命中 negated 规则则不忽略
pub fn ig_ignores(ig: &IgnoreMatcher, text: &str, is_dir: bool) -> bool {
    for rule in ig.rules.iter().rev() {
        if rule.dir_only && !is_dir {
            continue;
        }
        if rule_matches(rule, text) {
            return !rule.negated;
        }
    }
    false
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn glob_matching() {
        assert!(glob_segment("*.md", "SKILL.md"));
        assert!(!glob_segment("*.md", "SKILL.txt"));
        assert!(glob_match("skills/*/SKILL.md", "skills/demo/SKILL.md"));
        assert!(!glob_match("skills/*/SKILL.md", "skills/a/b/SKILL.md"));
        assert!(glob_match("skills/**/SKILL.md", "skills/a/b/SKILL.md"));
        assert!(glob_match("**/SKILL.md", "a/b/SKILL.md"));
        assert!(glob_match("demo?", "demo2"));
        assert!(!glob_match("demo?", "demo"));
    }
}

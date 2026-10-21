//! 规则匹配工具：通配匹配，以及一条路径的全部匹配拼写。
//!
//! 匹配语义（策略契约，与上游一致）：`*` 匹配**零个或多个任意字符**（**包括** `/`
//! 与其它分隔符），`?` 匹配恰好一个字符；不支持 `**`（与 `*` 等价），也不支持字符类
//! `[...]`。路径展开与词法归一化在 [`crate::utils::paths`]。

use crate::utils::paths::{absolutize, expand_home};
use std::path::Path;

/// 通配匹配：`pattern` 是否匹配 `text`。
///
/// `*` 跨分隔符贪婪匹配、`?` 匹配单个字符；其余字符按字面比较（区分大小写）。
/// 采用带回溯指针的迭代实现，避免为 `*` 递归爆栈；最坏 `O(len(text) * len(pattern))`。
pub(super) fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    // 最近一个 `*` 的位置与它当时对齐到的 text 下标（回溯点）。
    let mut star: Option<usize> = None;
    let mut star_ti = 0usize;

    while ti < t.len() {
        match p.get(pi) {
            Some('*') => {
                star = Some(pi);
                star_ti = ti;
                pi += 1;
            }
            Some('?') => {
                pi += 1;
                ti += 1;
            }
            Some(c) if *c == t[ti] => {
                pi += 1;
                ti += 1;
            }
            _ => {
                // 失配：退回最近的 `*`，让它多吞一个字符。
                let Some(sp) = star else {
                    return false;
                };
                pi = sp + 1;
                star_ti += 1;
                ti = star_ti;
            }
        }
    }

    // 文本耗尽后，模式剩余部分必须全是 `*`。
    while p.get(pi) == Some(&'*') {
        pi += 1;
    }
    pi == p.len()
}

/// 一条路径的**全部匹配拼写**：原文、家目录展开、绝对形式、canonical（符号链接解析）形式。
///
/// 去重后按稳定顺序返回。canonical 形式要求目标存在（`canonicalize` 会失败），
/// 因此不存在的路径只贡献前几种拼写——这与上游一致：deny 靠在**磁盘上**的文件判，
/// 拼写怎么变都拦得住，但前提是那个文件真的在。
pub(super) fn canonical_forms(path: &str, cwd: &Path) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |s: String| {
        if !out.contains(&s) {
            out.push(s);
        }
    };

    push(path.to_string());
    let expanded = expand_home(path);
    if expanded != path {
        push(expanded);
    }

    let base = if Path::new(path).is_absolute() {
        Path::new(path).to_path_buf()
    } else {
        cwd.join(path)
    };
    push(
        absolutize(&base.to_string_lossy(), cwd)
            .to_string_lossy()
            .to_string(),
    );

    // 符号链接：原文与绝对形式各自解析一次（相对路径解析结果通常相同，去重会合并）。
    for candidate in [Path::new(path).to_path_buf(), base] {
        if let Ok(real) = candidate.canonicalize() {
            push(real.to_string_lossy().to_string());
        }
    }
    if let Ok(real) = cwd.canonicalize() {
        push(absolutize(path, &real).to_string_lossy().to_string());
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_crosses_separators() {
        assert!(wildcard_match("src/*", "src/a/b/c.rs"));
        assert!(wildcard_match("*", "anything at all"));
        assert!(wildcard_match("*.env", "src/.env"));
        assert!(!wildcard_match("*.env", "src/.env.local"));
        assert!(wildcard_match("*.env.*", "src/.env.local"));
        assert!(wildcard_match("git status", "git status"));
        assert!(!wildcard_match("git status", "git status -s"));
    }

    #[test]
    fn question_mark_is_single_char() {
        assert!(wildcard_match("a?c", "abc"));
        assert!(!wildcard_match("a?c", "ac"));
        assert!(!wildcard_match("a?c", "abbc"));
    }

    #[test]
    fn wildcard_backtracks() {
        assert!(wildcard_match("a*b*c", "axxbyyc"));
        assert!(!wildcard_match("a*b*c", "axxbyy"));
        assert!(wildcard_match("*a*b", "za/b"));
    }

    #[test]
    fn forms_include_relative_and_absolute() {
        let cwd = Path::new("/work/proj");
        let forms = canonical_forms("src/a.rs", cwd);
        assert!(forms.contains(&"src/a.rs".to_string()));
        assert!(forms.contains(&"/work/proj/src/a.rs".to_string()));
    }
}

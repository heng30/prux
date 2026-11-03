//! 提交信息的生成：`/pr run` 有未提交改动时，先让模型看一眼暂存区写一条真的提交信息。
//!
//! 不直接沿用会话标题——标题是用户的第一句话（往往是「帮我看看这个」），和这批改动经常
//! 对不上，进历史就是垃圾。写不出来（没选模型、请求失败、输出为空）时调用方回落到
//! [`fallback`]：交付不该因为写不出信息而停下。
//!
//! 输出格式要求得很死（一行 subject + 可选 body），因为 [`normalize`] 只做机械清洗：
//! 模型爱加的 `Subject:` 前缀、代码围栏、引号都剥掉，剩下的原样进 `git commit -m`。

use crate::utils::truncate::{truncate_chars, truncate_chars_within};

/// 生成请求的系统提示。
pub(super) const SYSTEM: &str = "You write git commit messages for staged changes. Reply with \
     the message only: one subject line of at most 72 characters in the imperative mood, then \
     optionally a blank line and up to three short body lines saying why. No `Subject:` prefix, \
     no code fences, no quotes, no commentary.";

/// 一次生成请求的输出上限（一条 subject + 几行 body）。
pub(super) const MAX_TOKENS: u32 = 400;

/// 喂给模型的改动摘要字符上限。
pub(super) const OUTLINE_CHARS: usize = 6000;

/// subject 的字符上限（超了硬截断：一行太长的历史读不下去）。
pub(super) const SUBJECT_CHARS: usize = 72;

/// body 的字符上限（多出来的行没有信息量，只会撑长历史）。
pub(super) const BODY_CHARS: usize = 500;

/// 写不出提交信息时的兜底文案：扩展名 + 会话标题。
pub(super) fn fallback(title: &str) -> String {
    format!("prux: {title}")
}

/// 拼提示：会话标题说明「这批改动是为了什么」，改动摘要说明「改了什么」。
pub(super) fn build_prompt(outline: &str, title: &str) -> String {
    format!(
        "The session that produced these changes was about:\n{title}\n\nStaged changes:\n{outline}"
    )
}

/// 把模型输出规整成能直接交给 `git commit -m` 的信息；没有可用的 subject 时返回 `None`。
///
/// 只做清洗，不改写：剥掉首行的 `Subject:` / `Commit message:` / `提交信息：` 标签与引号，
/// 丢掉代码围栏，首行当 subject（超长截断），其余非空行压成 body（超长截断）。
pub(super) fn normalize(text: &str) -> Option<String> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.starts_with("```"))
        .collect();
    let first = lines.iter().position(|line| !line.is_empty())?;
    let subject = strip_labels(lines[first]);
    let subject = subject
        .trim_matches(|c| c == '"' || c == '\'' || c == '`' || c == '“' || c == '”')
        .trim();
    if subject.is_empty() {
        return None;
    }

    let mut out = truncate_chars_within(subject, SUBJECT_CHARS, "…");
    let body = lines[first + 1..]
        .iter()
        .copied()
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let body = truncate_chars(&body, BODY_CHARS, None, true);
    if !body.is_empty() {
        out.push_str("\n\n");
        out.push_str(&body);
    }
    Some(out)
}

/// 去掉首行可能带的 `Subject:` 一类标签；没有标签时原样返回。
fn strip_labels(line: &str) -> &str {
    const LABELS: &[&str] = &[
        "Subject:",
        "subject:",
        "Commit message:",
        "commit message:",
        "Message:",
        "message:",
        "提交信息：",
        "提交信息:",
    ];
    for label in LABELS {
        if let Some(rest) = line.strip_prefix(label) {
            return rest.trim();
        }
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_subject_survives_untouched() {
        assert_eq!(
            normalize("Add the pr extension\n").as_deref(),
            Some("Add the pr extension")
        );
    }

    #[test]
    fn labels_fences_and_quotes_are_stripped() {
        assert_eq!(
            normalize("```\nSubject: \"Fix the flaky test\"\n```").as_deref(),
            Some("Fix the flaky test")
        );
        assert_eq!(
            normalize("提交信息：修复轮询超时\n\n1. 先等一个周期").as_deref(),
            Some("修复轮询超时\n\n1. 先等一个周期")
        );
    }

    #[test]
    fn long_output_is_truncated_to_one_readable_subject_plus_body() {
        let long = "x".repeat(SUBJECT_CHARS * 2);
        let message = normalize(&format!("{long}\n\n{}", "y".repeat(BODY_CHARS * 2))).unwrap();
        let (subject, body) = message.split_once("\n\n").unwrap();
        assert_eq!(subject.chars().count(), SUBJECT_CHARS, "subject 不超上限");
        assert_eq!(body.chars().count(), BODY_CHARS, "body 不超上限");
    }

    #[test]
    fn empty_or_noise_output_yields_none() {
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("```\n\n```"), None);
        assert_eq!(normalize("\"   \""), None);
    }
}

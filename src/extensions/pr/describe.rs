//! PR 标题与正文的生成：`/pr run` 开 PR 前让模型看一眼分支上的全部改动写一份。
//!
//! 不拿会话标题当 PR 标题——那是用户的第一句话（`@a.c 实现归并排序`、`帮我看看这个`），
//! 同一个仓库开多个 PR 时标题全不一样也全看不出改了什么，评审的人第一眼就该知道这批改动
//! 干了什么。标题与正文都是一次请求的产物，写不出来（没选模型、请求失败、输出为空、
//! JSON 不合规）才回落会话标题 + 固定说明。
//!
//! 系统提示词用**自己的那一份**（[`SYSTEM`]），不拿会话的系统提示词：后者写着工具协议与
//! 工具清单（read / bash / edit …），而一次性请求没有 `tools` 参数，模型会把工具调用块
//! 当普通文本输出，PR 标题就变成一堆 `<｜DSML｜ invoke …>`。
//!
//! **要 JSON 不要「首行标题 + 空行正文」**：按行切分的解析一旦模型多写一句前言、
//! 少一个空行、把标题写进代码块，标题就悄悄变成半句话，而没有人会去核对。一个
//! `{"title", "body"}` 对象要么解析成功，要么失败回落，没有中间地带。

use crate::utils::truncate::{truncate_chars, truncate_chars_within};
use serde::Deserialize;

/// 生成 PR 描述的系统提示（独立一份，不用会话的系统提示词）。
///
/// 会话提示词里写着工具协议与工具清单，而一次性请求没有 `tools` 参数，模型会对着
/// 它写工具调用块——那就是一份 `<｜DSML｜ invoke …>` 的垃圾标题 / 正文。这里只要 JSON。
pub(super) const SYSTEM: &str = r#"Reply with one JSON object and nothing else:
{"title": "...", "body": "..."} — title is one line, body is the pull request body in Markdown.
There are no tools available in this request: do not emit tool calls or XML tags, and do not run
checks, tests, builds or git commands."#;

/// 一次生成请求的输出上限（标题 + 几段正文）。
pub(super) const MAX_TOKENS: u32 = 1200;

/// 喂给模型的分支改动摘要字符上限。
pub(super) const OUTLINE_CHARS: usize = 12000;

/// 标题的字符上限（超了硬截断：一行读不完的标题等于没有标题）。
pub(super) const TITLE_CHARS: usize = 72;

/// 正文的字符上限（PR 正文不是散文，撑长只会没人读）。
pub(super) const BODY_CHARS: usize = 2000;

/// 模型回复的形状（`body` 缺省为空串，`title` 缺失或为空则整份丢弃）。
#[derive(Deserialize)]
struct RawPr {
    /// PR 标题（一行）。
    title: String,
    /// PR 正文（Markdown）；缺失按空正文处理。
    #[serde(default)]
    body: String,
}

/// 拼提示：会话标题说明「这批改动是为了什么」，分支改动摘要说明「改了什么」。
///
/// 末句把两个约束钉死：只回一个 JSON 对象，不要顺手跑检查 / 测试 / git 命令；正文用
/// Markdown 正文，不要复述 diff。
pub(super) fn build_prompt(outline: &str, title: &str) -> String {
    format!(
        "The session that produced these changes was about:\n{title}\n\nAll changes on this \
         branch:\n{outline}\n\nWrite the pull request for it. Reply with one JSON object and \
         nothing else: {{\"title\": \"...\", \"body\": \"...\"}} — title is one line (at most \
         {TITLE_CHARS} characters), body is the pull request body in Markdown (what changed, why, \
         and what a reviewer should look at). Describe the change itself — do not paste the diff, \
         do not emit tool calls or XML tags (there are no tools in this request), and do not run \
         any checks, tests, builds or git commands."
    )
}

/// 解析模型输出为 `(标题, 正文)`：取第一个 `{` 到最后一个 `}`，反序列化失败即 `None`。
///
/// 代码围栏不用特殊处理（` ```json ` 落在切片外面）；标题为空同样返回 `None`，
/// 否则调用方会开出一个没有名字的 PR。
pub(super) fn parse(text: &str) -> Option<(String, String)> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    let raw: RawPr = serde_json::from_str(text.get(start..=end)?).ok()?;
    let title = raw.title.trim();
    if title.is_empty() {
        return None;
    }
    Some((
        truncate_chars_within(title, TITLE_CHARS, "…"),
        truncate_chars(raw.body.trim(), BODY_CHARS, None, true),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_json_object_yields_the_title_and_the_body() {
        let text = "{\"title\": \"Add merge sort\", \"body\": \"## 改动\\n- 新增归并排序\\n\"}";
        let (title, body) = parse(text).unwrap();
        assert_eq!(title, "Add merge sort");
        assert_eq!(body, "## 改动\n- 新增归并排序");
    }

    #[test]
    fn fences_and_chatter_around_the_object_are_tolerated() {
        let (title, body) = parse(
            "Here is the PR:\n```json\n{\"title\": \"修复轮询超时\", \"body\": \"先等一个周期再读状态\"}\n```\n",
        )
        .unwrap();
        assert_eq!(title, "修复轮询超时");
        assert_eq!(body, "先等一个周期再读状态");
    }

    #[test]
    fn a_title_only_object_still_parses_with_an_empty_body() {
        assert_eq!(parse(r#"{"title": "Add pr descriptions"}"#).unwrap().1, "");
    }

    #[test]
    fn broken_or_empty_output_is_rejected_instead_of_guessed() {
        assert_eq!(parse(""), None);
        assert_eq!(
            parse("Add merge sort\n\nbody"),
            None,
            "非 JSON 不再被猜成标题"
        );
        assert_eq!(parse(r#"{"title": "  ", "body": "x"}"#), None);
        assert_eq!(parse(r#"{"body": "x"}"#), None, "缺 title");
        assert_eq!(parse(r#"{"title": "x", "body": 42}"#), None, "类型不对");
    }

    #[test]
    fn long_output_is_truncated_to_the_documented_limits() {
        let long = "x".repeat(TITLE_CHARS * 2);
        let text = format!(
            r#"{{"title": "{long}", "body": "{}"}}"#,
            "y".repeat(BODY_CHARS * 2)
        );
        let (title, body) = parse(&text).unwrap();
        assert_eq!(title.chars().count(), TITLE_CHARS);
        assert_eq!(body.chars().count(), BODY_CHARS);
    }
}

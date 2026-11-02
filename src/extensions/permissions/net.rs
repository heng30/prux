//! 从一次工具调用里抽出「它会连哪些主机」。
//!
//! 只认 `http://` / `https://` 开头的绝对 URL：裸域名（`example.com`）与其它协议
//! （`ftp://`、`git+ssh://`）不参与域名层判定——它们的主机语义不可靠，
//! 猜错只会拦住不相干的东西。
//!
//! 抽法按来源分两路：bash / powershell 走命令文本（每条被扫描出的语句各抽一次，
//! 因此 `$(curl https://x)` 也逃不掉）；其它工具递归遍历入参 JSON 里的字符串值，
//! MCP 把 URL 塞在 `arguments.url` 这种深层位置时同样能命中。

use serde_json::Value;
use url::Url;

/// `Value` 递归遍历深度上限：入参是模型给的 JSON，深到离谱时宁可少抽也不爆栈。
const MAX_DEPTH: usize = 8;

/// 命令行文本里切词用的分隔符（引号、括号、重定向、管道、语句分隔都算）。
const WORD_DELIMS: &[char] = &[
    '\'', '"', '`', '(', ')', '<', '>', ';', '|', '&', '\n', '\r', '\t', ' ',
];

/// 从一段命令行文本里抽出所有绝对 URL（保序、去重）。
pub(super) fn urls_in_command(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split(WORD_DELIMS) {
        let Some(url) = url_in_word(word) else {
            continue;
        };
        if !out.iter().any(|seen| seen == &url) {
            out.push(url);
        }
    }
    out
}

/// 从一个词里截出 URL：取最早出现的 `http(s)://` 到词尾，去掉尾部标点。
///
/// 这样 `--url=https://x/y`、`curl(-sL https://x)` 这类前缀包装也能命中。
fn url_in_word(word: &str) -> Option<String> {
    let lowered = word.to_ascii_lowercase();
    let start = ["http://", "https://"]
        .iter()
        .filter_map(|scheme| lowered.find(scheme))
        .min()?;
    let url = word[start..].trim_end_matches(['.', ',', ')', ']', '}', ';', ':']);
    // 只剩 scheme 说明这不是 URL（如文档里写的 "https://"），别拿去解析。
    (url.len() > 8 && Url::parse(url).is_ok()).then(|| url.to_string())
}

/// 从工具入参 JSON 里递归抽出所有绝对 URL（只看字符串值）。
pub(super) fn urls_in_args(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    collect(value, 0, &mut out);
    out
}

/// `urls_in_args` 的递归体：把含 `://` 的字符串交给 [`urls_in_command`] 切。
fn collect(value: &Value, depth: usize, out: &mut Vec<String>) {
    if depth > MAX_DEPTH {
        return;
    }
    match value {
        Value::String(text) if text.contains("://") => {
            for url in urls_in_command(text) {
                if !out.iter().any(|seen| seen == &url) {
                    out.push(url);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect(item, depth + 1, out);
            }
        }
        Value::Object(map) => {
            for item in map.values() {
                collect(item, depth + 1, out);
            }
        }
        _ => {}
    }
}

/// 一条 URL 的主机拼写候选：主机名（小写，IPv6 带方括号），有端口时再补 `host:port`。
///
/// 两种拼写都参与匹配，因此 `"api.example.com"` 与 `"localhost:8080"` 都能写。
/// URL 解析不出来（手工拼的字符串）时返回空。
pub(super) fn host_forms(url: &str) -> Vec<String> {
    let Ok(parsed) = Url::parse(url) else {
        return Vec::new();
    };
    let Some(host) = parsed.host_str() else {
        return Vec::new();
    };
    let mut forms = vec![host.to_string()];
    if let Some(port) = parsed.port() {
        forms.push(format!("{host}:{port}"));
    }
    forms
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extracts_urls_from_a_command_line() {
        assert_eq!(
            urls_in_command("curl -sSL --url=https://API.Example.com/x?y=1 | jq ."),
            vec!["https://API.Example.com/x?y=1"]
        );
        // 引号与括号不是 URL 的一部分
        assert_eq!(
            urls_in_command("echo (http://a.test/p) 'https://b.test/q'"),
            vec!["http://a.test/p", "https://b.test/q"]
        );
        // 同一段命令里出现两次只算一次
        assert_eq!(
            urls_in_command("curl http://a.test && curl http://a.test"),
            vec!["http://a.test"]
        );
    }

    #[test]
    fn ignores_non_http_and_empty_scheme() {
        assert!(urls_in_command("git clone git@github.com:a/b.git").is_empty());
        assert!(urls_in_command("ftp://files.test/x").is_empty());
        assert!(urls_in_command("see https://").is_empty());
    }

    #[test]
    fn extracts_nested_args_urls() {
        let args = json!({
            "action": "call",
            "arguments": {"url": "https://api.internal/v1", "nested": ["http://other.test"]},
            "note": "no url here",
        });
        assert_eq!(
            urls_in_args(&args),
            vec!["https://api.internal/v1", "http://other.test"]
        );
    }

    #[test]
    fn host_forms_are_lowercase_with_optional_port() {
        assert_eq!(
            host_forms("https://API.Example.COM:8443/x"),
            vec!["api.example.com", "api.example.com:8443"]
        );
        assert_eq!(
            host_forms("http://LocalHost:8080"),
            vec!["localhost", "localhost:8080"]
        );
        assert_eq!(
            host_forms("https://[::1]:9000/x"),
            vec!["[::1]", "[::1]:9000"]
        );
        assert!(host_forms("not a url").is_empty());
    }
}

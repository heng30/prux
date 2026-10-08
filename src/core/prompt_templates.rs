// 提示词模版

use crate::{
    PROJECT_SCOPE_NAME,
    utils::{display::normalize_newlines, mime::strip_bom, paths::resolve_path},
};
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
};

/// 从带 frontmatter 的 Markdown 文件加载的提示词模板，供斜杠命令调用。
#[derive(Debug, Clone)]
pub struct PromptTemplate {
    /// 模板名（文件名去 `.md` 扩展名），斜杠命令按它匹配。
    pub name: String,
    /// 简述，取自 frontmatter，缺省时取正文首行前 60 字。
    pub description: String,
    /// 参数提示（frontmatter `argument-hint`）；未提供为 None。
    pub argument_hint: Option<String>,
    /// 模板正文（frontmatter 之后），替换 $1/$@ 等占位符后发送。
    pub content: String,
    /// 模板来源文件路径。
    pub file_path: String,
}

/// 最近一次加载产生的告警（frontmatter 不合法等）；交互启动/重载时取走提示。
static LOAD_WARNINGS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

/// 惰性初始化的全局告警槽，避免静态 Mutex 的 const 初始化限制。
fn warnings_slot() -> &'static Mutex<Vec<String>> {
    LOAD_WARNINGS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 取走最近一次 `load_prompt_templates` 产生的告警（不重复提示）。
pub fn take_load_warnings() -> Vec<String> {
    std::mem::take(&mut *warnings_slot().lock().unwrap())
}

/// 一次加载的结果：模板 + 诊断告警。
#[derive(Debug, Default)]
pub struct LoadedPromptTemplates {
    /// 本次加载到的模板，同名只保留先出现的一个。
    pub templates: Vec<PromptTemplate>,
    /// frontmatter 不合法等诊断信息，供调用方提示。
    pub warnings: Vec<String>,
}

/// 校验 frontmatter 是否是 YAML 解析器会拒绝的形态（解析失败应报告为
/// resource warning，而不是静默忽略）。无 YAML 依赖，只捕获会令 YAML 报错的
/// 常见形态：未加引号的标量里出现 `: `（嵌套映射）、引号/流式集合不配对。
fn frontmatter_error(yaml: &str) -> Option<String> {
    for raw_line in yaml.lines() {
        // 缩进行/列表项/注释：合法的多行形态，不校验
        if raw_line.starts_with(' ')
            || raw_line.starts_with('\t')
            || raw_line.trim_start().starts_with('-')
            || raw_line.trim_start().starts_with('#')
        {
            continue;
        }
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((_key, value)) = line.split_once(':') else {
            // 顶层裸标量：YAML 可解析为字符串文档，不算错误
            continue;
        };
        let value = value.trim();
        if value.is_empty() {
            continue;
        }
        let first = value.as_bytes()[0] as char;
        if first == '"' || first == '\'' {
            // 引号标量：只要后面出现过闭合引号就算合法（允许行尾注释）
            if !value[1..].contains(first) {
                return Some(format!("unterminated quoted value: {line}"));
            }
            continue;
        }
        if first == '[' {
            if !value.contains(']') {
                return Some(format!("unterminated flow sequence: {line}"));
            }
            continue;
        }
        if first == '{' {
            if !value.contains('}') {
                return Some(format!("unterminated flow mapping: {line}"));
            }
            continue;
        }
        if value.contains(": ") {
            return Some(format!("unquoted value contains a nested mapping: {line}"));
        }
    }
    None
}

/// 解析 frontmatter，返回 (description, argument-hint, body)。
/// frontmatter 块存在但不合法时返回 Err（由加载方转为 resource warning 并跳过该模板）。
fn parse_frontmatter(content: &str) -> Result<(Option<String>, Option<String>, String), String> {
    let normalized = normalize_newlines(strip_bom(content));
    if !normalized.starts_with("---") {
        return Ok((None, None, normalized.trim_start().to_string()));
    }
    let Some(end) = normalized[3..].find("\n---") else {
        return Ok((None, None, normalized));
    };
    let end = 3 + end;
    let yaml = &normalized[3..end];
    if let Some(err) = frontmatter_error(yaml) {
        return Err(err);
    }
    let body = normalized[end + 4..].trim().to_string();
    let mut description = None;
    let mut argument_hint = None;
    for line in yaml.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("description:") {
            description = Some(strip_yaml_quotes(v.trim()));
        } else if let Some(v) = line.strip_prefix("argument-hint:") {
            argument_hint = Some(strip_yaml_quotes(v.trim()));
        }
    }
    Ok((description, argument_hint, body))
}

/// 去掉 YAML 标量两端配对的单/双引号，并 trim 空白；未配对时原样返回。
fn strip_yaml_quotes(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let b = s.as_bytes();
        if (b[0] == b'"' || b[0] == b'\'') && b[s.len() - 1] == b[0] {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// bash 风格参数切分（支持单双引号）
pub fn parse_command_args(args_string: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in args_string.chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                current.push(c);
            }
        } else if c == '"' || c == '\'' {
            quote = Some(c);
        } else if c.is_whitespace() {
            if !current.is_empty() {
                args.push(std::mem::take(&mut current));
            }
        } else {
            current.push(c);
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// 把全部参数用空格拼成单个字符串（对应 `$@` / `$ARGUMENTS`）。
fn all_args(args: &[String]) -> String {
    args.join(" ")
}

/// $1/$2/$@/$ARGUMENTS/${N:-default}/${@:N}/${@:N:L}
pub fn substitute_args(content: &str, args: &[String]) -> String {
    let chars: Vec<char> = content.chars().collect();
    let mut out = String::new();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '$' || i + 1 >= chars.len() {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        match chars[i + 1] {
            '{' => {
                let Some(close_rel) = chars[i + 2..].iter().position(|c| *c == '}') else {
                    out.push(chars[i]);
                    i += 1;
                    continue;
                };
                let close = i + 2 + close_rel;
                let expr: String = chars[i + 2..close].iter().collect();
                if let Some((target, default)) = expr.split_once(":-") {
                    let value = match target {
                        "@" | "ARGUMENTS" => all_args(args),
                        _ => target
                            .parse::<usize>()
                            .ok()
                            .and_then(|n| args.get(n.saturating_sub(1)))
                            .cloned()
                            .unwrap_or_default(),
                    };
                    out.push_str(if value.is_empty() { default } else { &value });
                    i = close + 1;
                    continue;
                }
                if let Some(rest) = expr.strip_prefix("@:") {
                    let mut parts = rest.split(':');
                    let start = parts
                        .next()
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(1)
                        .saturating_sub(1);
                    let len = parts.next().and_then(|v| v.parse::<usize>().ok());
                    let slice = match len {
                        Some(len) => args.get(start..start.saturating_add(len)).unwrap_or(&[]),
                        None => args.get(start..).unwrap_or(&[]),
                    };
                    out.push_str(&slice.join(" "));
                    i = close + 1;
                    continue;
                }
                out.push(chars[i]);
                i += 1;
            }
            '@' => {
                out.push_str(&all_args(args));
                i += 2;
            }
            c if c.is_ascii_digit() => {
                let mut n = c.to_digit(10).unwrap_or(0) as usize;
                let mut j = i + 2;
                while j < chars.len() && chars[j].is_ascii_digit() {
                    n = n * 10 + chars[j].to_digit(10).unwrap_or(0) as usize;
                    j += 1;
                }
                if n > 0
                    && let Some(v) = args.get(n - 1)
                {
                    out.push_str(v);
                }
                i = j;
            }
            'A' if chars[i + 1..].starts_with(&['A', 'R', 'G', 'U', 'M', 'E', 'N', 'T', 'S']) => {
                out.push_str(&all_args(args));
                i += 10; // $ + ARGUMENTS
            }
            _ => {
                out.push(chars[i]);
                i += 1;
            }
        }
    }
    out
}

/// frontmatter 未给 description 时的兜底：取正文首个非空行，超过 60 字截断加省略号。
fn description_from_body(body: &str) -> String {
    let first = body.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let mut desc: String = first.chars().take(60).collect();
    if first.chars().count() > 60 {
        desc.push_str("...");
    }
    desc
}

/// 读取并解析单个模板文件（frontmatter + 正文），模板名取文件名去扩展名。
/// 文件不可读或 frontmatter 不合法时返回 Err（附原因）。
fn load_template_from_file(path: &Path) -> Result<PromptTemplate, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("failed to read prompt template file: {e}"))?;
    let (description, argument_hint, body) = parse_frontmatter(&content)?;
    let name = path
        .file_stem()
        .and_then(|n| n.to_str())
        .unwrap_or("template")
        .to_string();
    let description = description
        .filter(|d| !d.trim().is_empty())
        .unwrap_or_else(|| description_from_body(&body));
    Ok(PromptTemplate {
        name,
        description,
        argument_hint,
        content: body,
        file_path: path.to_string_lossy().to_string(),
    })
}

/// 扫描目录下所有 `.md` 模板并追加到 `templates`（同名只保留先出现者），
/// 解析失败则把诊断信息推入 `warnings`。目录不可读时静默返回。
fn load_templates_from_dir(
    dir: &Path,
    templates: &mut Vec<PromptTemplate>,
    warnings: &mut Vec<String>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("md") || !path.is_file() {
            continue;
        }
        match load_template_from_file(&path) {
            Ok(t) if !templates.iter().any(|x| x.name == t.name) => templates.push(t),
            Ok(_) => {}
            Err(e) => warnings.push(format!(
                "Invalid prompt template frontmatter in {}: {e}",
                path.display()
            )),
        }
    }
}

/// 默认目录：agentDir/prompts、cwd/${PROJECT_SCOPE_NAME}/prompts；显式路径可为文件或目录
pub fn load_prompt_templates(
    cwd: &str,
    agent_dir: &Path,
    prompt_paths: &[String],
    include_defaults: bool,
) -> Vec<PromptTemplate> {
    let loaded =
        load_prompt_templates_with_diagnostics(cwd, agent_dir, prompt_paths, include_defaults);
    *warnings_slot().lock().unwrap() = loaded.warnings;
    loaded.templates
}

/// 同 [`load_prompt_templates`]，但同时返回诊断告警（供测试/调用方直接消费）。
pub fn load_prompt_templates_with_diagnostics(
    cwd: &str,
    agent_dir: &Path,
    prompt_paths: &[String],
    include_defaults: bool,
) -> LoadedPromptTemplates {
    let mut templates = Vec::new();
    let mut warnings = Vec::new();
    if include_defaults {
        load_templates_from_dir(&agent_dir.join("prompts"), &mut templates, &mut warnings);
        load_templates_from_dir(
            &PathBuf::from(cwd).join(PROJECT_SCOPE_NAME).join("prompts"),
            &mut templates,
            &mut warnings,
        );
    }

    for raw in prompt_paths {
        let path = resolve_path(raw, cwd);
        if path.is_dir() {
            load_templates_from_dir(&path, &mut templates, &mut warnings);
        } else if path.extension().and_then(|e| e.to_str()) == Some("md") && path.is_file() {
            match load_template_from_file(&path) {
                Ok(t) if !templates.iter().any(|x| x.name == t.name) => templates.push(t),
                Ok(_) => {}
                Err(e) => warnings.push(format!(
                    "Invalid prompt template frontmatter in {}: {e}",
                    path.display()
                )),
            }
        }
    }

    LoadedPromptTemplates {
        templates,
        warnings,
    }
}

/// 若文本以 /<template-name> 开头，替换为模板内容；否则原样返回
pub fn expand_prompt_template(text: &str, templates: &[PromptTemplate]) -> String {
    if !text.starts_with('/') {
        return text.to_string();
    }

    let Some((name, rest)) = text[1..].split_once(char::is_whitespace) else {
        let name = &text[1..];
        if let Some(t) = templates.iter().find(|t| t.name == name) {
            return t.content.clone();
        }
        return text.to_string();
    };

    let args_string = rest.trim_start();
    if let Some(t) = templates.iter().find(|t| t.name == name) {
        let args = parse_command_args(args_string);
        return substitute_args(&t.content, &args);
    }

    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_error_flags_invalid_and_accepts_valid() {
        assert!(frontmatter_error("\ndescription: Broken: unquoted colon").is_some());
        assert!(frontmatter_error("\ndescription: \"unterminated").is_some());
        assert!(frontmatter_error("\ndescription: [a, b").is_some());
        assert!(frontmatter_error("\ndescription: ok\nargument-hint: \"<x>\"").is_none());
        assert!(frontmatter_error("\ndescription: https://example.com").is_none());
        assert!(frontmatter_error("\ndescription: \"ok\" # comment").is_none());
        assert!(frontmatter_error("\ndescription: [a, b] # comment").is_none());
        assert!(frontmatter_error("\n- item one\n- item two").is_none());
    }

    #[test]
    fn reports_malformed_frontmatter_and_keeps_valid_siblings() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("invalid.md"),
            "---\ndescription: Broken: unquoted colon\n---\nDo something.\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("valid.md"), "Valid prompt content.").unwrap();
        let loaded = load_prompt_templates_with_diagnostics(
            dir.path().to_str().unwrap(),
            dir.path(),
            &[dir.path().to_string_lossy().to_string()],
            false,
        );
        assert_eq!(loaded.templates.len(), 1, "{:?}", loaded.templates);
        assert_eq!(loaded.templates[0].name, "valid");
        assert_eq!(loaded.warnings.len(), 1, "{:?}", loaded.warnings);
        assert!(loaded.warnings[0].contains("invalid.md"));
    }

    #[test]
    fn parses_bash_args() {
        assert_eq!(
            parse_command_args("one \"two words\" 'three'"),
            vec!["one", "two words", "three"]
        );
    }

    #[test]
    fn substitutes_template_args() {
        let out = substitute_args(
            "hi $1 ${2:-default} $@ ${ARGUMENTS:-none} ${@:2} ${@:2:1}",
            &["a".into(), "b".into(), "c".into()],
        );
        assert_eq!(out, "hi a b a b c a b c b c b");
    }

    #[test]
    fn expands_template() {
        let t = PromptTemplate {
            name: "review".into(),
            description: String::new(),
            argument_hint: None,
            content: "review $1".into(),
            file_path: String::new(),
        };
        assert_eq!(
            expand_prompt_template("/review src/main.rs", &[t]),
            "review src/main.rs"
        );
        assert_eq!(expand_prompt_template("hello", &[]), "hello");
    }
}

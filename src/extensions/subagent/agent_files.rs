//! agent 文件的读写支持：`/agents eject|enable|disable`。
//!
//! - **读取**（判断某个文件是不是已禁用）走真正的 frontmatter 解析器
//!   （[`parse_agent_file`]），不另写一套可能与之漂移的判断；
//! - **编辑**是**逐行**的：解析后重新序列化会重排键序、丢掉注释与引号风格，
//!   而 agent 文件是给用户手写的。因此只在 frontmatter 块内增删那一行，其余字节原样保留。
//!
//! 无法改写的写法（如 `enabled: False`、值被引号包住）返回"未改动"，
//! 由调用方如实报告失败，而不是宣称改成功。

use super::{
    agent_types::{self, GLOBAL_AGENT_DIR, PROJECT_AGENT_DIR},
    types::{AgentSource, AgentType},
};
use crate::{PROJECT_SCOPE_NAME, core::settings_manager};
use std::path::{Path, PathBuf};

/// 逐行 `enabled: false`（忽略行尾空白/CR）。
fn is_enabled_false(line: &str) -> bool {
    line.trim_end_matches(['\r', '\n']).trim_end() == "enabled: false"
}

/// frontmatter 块的行切片。
struct Frontmatter<'a> {
    /// frontmatter 各行（含首尾 `---`）。
    lines: Vec<&'a str>,
    /// 闭合 `---` 的行号
    close_idx: usize,
}

/// 切出 frontmatter 块（首行必须是 `---`，且存在闭合行）。
fn split_frontmatter(content: &str) -> Option<Frontmatter<'_>> {
    let mut lines: Vec<&str> = content.split_inclusive('\n').collect();
    if lines.is_empty() {
        lines = vec![content];
    }
    if lines.first()?.trim_end() != "---" {
        return None;
    }
    let close_idx = lines
        .iter()
        .enumerate()
        .skip(1)
        .find(|(_, l)| l.trim_end() == "---")
        .map(|(i, _)| i)?;
    Some(Frontmatter { lines, close_idx })
}

/// `disable` 的结果（区分真实改动与无操作，便于如实报告）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisableOutcome {
    /// 本次成功写入，文件被新禁用
    Disabled,
    /// 文件本已处于禁用状态，未做任何改动
    AlreadyDisabled,
    /// 缺少 frontmatter，无处插入禁用标记
    NoFrontmatter,
}

/// 写入 `enabled: false`（插在 frontmatter 第一行之后）。
pub fn disable_in_content(content: &str) -> (String, DisableOutcome) {
    let Some(block) = split_frontmatter(content) else {
        return (content.to_string(), DisableOutcome::NoFrontmatter);
    };

    if is_disabled_content(content) {
        return (content.to_string(), DisableOutcome::AlreadyDisabled);
    }

    let mut lines = block.lines.clone();
    let eol = if lines[0].ends_with("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    lines.insert(1, "enabled: false");

    let mut out = String::new();
    for (i, l) in lines.iter().enumerate() {
        out.push_str(l);
        // 插入行原本不带换行：补一个（沿用块内换行风格）
        if i == 1 && !l.ends_with('\n') {
            out.push_str(eol);
        }
    }
    (out, DisableOutcome::Disabled)
}

/// 移除 `enabled: false`（块内任意位置；返回 `false` = 本来就没有）。
pub fn enable_in_content(content: &str) -> (String, bool) {
    let Some(block) = split_frontmatter(content) else {
        return (content.to_string(), false);
    };
    let kept: Vec<&str> = block
        .lines
        .iter()
        .enumerate()
        .filter(|(i, l)| !(*i > 0 && *i < block.close_idx && is_enabled_false(l)))
        .map(|(_, l)| *l)
        .collect();
    if kept.len() == block.lines.len() {
        return (content.to_string(), false);
    }
    (kept.concat(), true)
}

/// 文件内容是否处于禁用态（走真正的解析器）。
pub fn is_disabled_content(content: &str) -> bool {
    agent_types::parse_agent_file(content, "probe", AgentSource::Global, Path::new("probe.md"))
        .map(|ty| !ty.enabled)
        .unwrap_or(false)
}

/// 序列化一个类型为完整的 `.md`（eject 写入用）。
pub fn serialize_agent_file(ty: &AgentType) -> String {
    let mut fields: Vec<String> = Vec::new();
    fields.push(format!("name: {}", ty.name));
    fields.push(format!("description: {}", quote_yaml(&ty.description)));
    if ty.display_name != ty.name {
        fields.push(format!("display_name: {}", ty.display_name));
    }
    if let Some(color) = &ty.color {
        fields.push(format!("color: {}", quote_yaml(color)));
    }
    fields.push(format!(
        "tools: {}",
        format_tools_field(ty.tools.as_deref())
    ));
    if let Some(model) = &ty.model {
        fields.push(format!("model: {model}"));
    }
    if let Some(thinking) = &ty.thinking {
        fields.push(format!("thinking: {thinking}"));
    }
    if let Some(max_turns) = ty.max_turns {
        fields.push(format!("max_turns: {max_turns}"));
    }
    if let Some(persist) = ty.persist_session {
        fields.push(format!("persist_session: {persist}"));
    }
    if let Some(dir) = &ty.session_dir {
        fields.push(format!("session_dir: {dir}"));
    }
    if let Some(disallowed) = &ty.disallowed_tools {
        fields.push(format!("disallowed_tools: {}", disallowed.join(", ")));
    }
    // `allowed_subagents`：`Some([])` = `all`（开启嵌套但不限型）；
    // `Some(list)` = 白名单；`None` = 不开启嵌套（不写）。
    if let Some(nested) = &ty.allowed_subagents {
        fields.push(format!(
            "allowed_subagents: {}",
            if nested.is_empty() {
                "all".to_string()
            } else {
                nested.join(", ")
            }
        ));
    }
    // 扩展作用域：`extensions: false` = 不带扩展工具；列表 = 白名单。
    if ty.extensions_none {
        fields.push("extensions: false".to_string());
    } else if let Some(exts) = &ty.extensions {
        fields.push(format!("extensions: {}", exts.join(", ")));
    }
    if !ty.exclude_extensions.is_empty() {
        fields.push(format!(
            "exclude_extensions: {}",
            ty.exclude_extensions.join(", ")
        ));
    }
    // 技能：`Some(None)` = `skills: false`；`Some(Some(list))` = 预载列表。
    match &ty.skills {
        Some(None) => fields.push("skills: false".to_string()),
        Some(Some(list)) => fields.push(format!("skills: {}", list.join(", "))),
        None => {}
    }
    if let Some(memory) = ty.memory {
        fields.push(format!("memory: {}", memory.as_str()));
    }
    if let Some(isolation) = &ty.isolation {
        fields.push(format!("isolation: {isolation}"));
    }
    if let Some(isolated) = ty.isolated {
        fields.push(format!("isolated: {isolated}"));
    }
    if let Some(output) = ty.output_transcript {
        fields.push(format!("output_transcript: {output}"));
    }
    if let Some(bg) = ty.run_in_background {
        fields.push(format!("run_in_background: {bg}"));
    }
    if let Some(inherit) = ty.inherit_context {
        fields.push(format!("inherit_context: {inherit}"));
    }
    fields.push(format!("prompt_mode: {}", ty.prompt_mode.as_str()));
    if !ty.enabled {
        fields.push("enabled: false".to_string());
    }
    format!(
        "---\n{}\n---\n\n{}\n",
        fields.join("\n"),
        ty.system_prompt.trim_end()
    )
}

/// `tools:` 字段：None = `all`，空 = `none`，否则 CSV。
fn format_tools_field(tools: Option<&[String]>) -> String {
    match tools {
        None => "all".to_string(),
        Some([]) => "none".to_string(),
        Some(list) => list.join(", "),
    }
}

/// YAML 字符串加引号：不带引号的标量会被 `:`/`#` 静默截断（上游踩过的坑）。
fn quote_yaml(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
}

/// eject 的写入路径（`project = true` 写项目级，否则写全局）。
pub fn eject_path(name: &str, cwd: &str, project: bool) -> PathBuf {
    let dir = if project {
        Path::new(cwd)
            .join(PROJECT_SCOPE_NAME)
            .join(PROJECT_AGENT_DIR)
    } else {
        settings_manager::agent_dir().join(GLOBAL_AGENT_DIR)
    };
    dir.join(format!("{}.md", safe_stem(name)))
}

/// 定位某个类型已存在的 `.md`（先看加载来源，再按发现优先级探测）。
pub fn find_agent_file(ty: &AgentType, cwd: &str) -> Option<PathBuf> {
    if let Some(path) = &ty.source_path
        && path.is_file()
    {
        return Some(path.clone());
    }
    let candidates = [
        Path::new(cwd)
            .join(PROJECT_SCOPE_NAME)
            .join(PROJECT_AGENT_DIR),
        settings_manager::agent_dir().join(GLOBAL_AGENT_DIR),
    ];
    candidates
        .iter()
        .map(|dir| dir.join(format!("{}.md", safe_stem(&ty.name))))
        .find(|p| p.is_file())
}

/// 文件名安全化：仅允许 `[A-Za-z0-9._-]`，其余替换为 `-`（避免路径穿越）。
fn safe_stem(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();

    // 去掉首尾的 `-`/`.`：`../../etc/passwd` 变成 `etc-passwd`，且不会留下纯点段
    let cleaned = cleaned.trim_matches(['-', '.']).to_string();
    if cleaned.is_empty() {
        "agent".to_string()
    } else {
        cleaned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::subagent::types::PromptMode;
    use crate::test_support::AgentDirGuard;

    #[test]
    fn disable_and_enable_are_line_wise_and_preserve_everything_else() {
        let original = "---\n# my notes\ndescription: \"Scout: find things\"\ntools: read, grep\n---\n\nBody # not a comment\n";
        let (disabled, outcome) = disable_in_content(original);
        assert_eq!(outcome, DisableOutcome::Disabled);
        assert!(
            disabled.starts_with("---\nenabled: false\n# my notes\n"),
            "{disabled}"
        );
        assert!(
            disabled.contains("description: \"Scout: find things\""),
            "{disabled}"
        );
        assert!(
            disabled.ends_with("Body # not a comment\n"),
            "正文不得改动: {disabled}"
        );
        assert!(is_disabled_content(&disabled));

        // 幂等：已禁用再禁用 → already-disabled 且内容不变
        let (again, outcome) = disable_in_content(&disabled);
        assert_eq!(outcome, DisableOutcome::AlreadyDisabled);
        assert_eq!(again, disabled);

        // 启用：删掉那一行，其余保留
        let (enabled, changed) = enable_in_content(&disabled);
        assert!(changed);
        assert_eq!(enabled, original, "启用后应还原为原文件");
        let (_, changed) = enable_in_content(&enabled);
        assert!(!changed, "本来就没禁用 → 未改动");

        // 无 frontmatter
        let (same, outcome) = disable_in_content("just text\n");
        assert_eq!(outcome, DisableOutcome::NoFrontmatter);
        assert_eq!(same, "just text\n");
    }

    #[test]
    fn disable_recognizes_enabled_false_at_any_position_and_refuses_odd_spellings() {
        // 块内任意位置都算已禁用（与加载侧一致）
        let mid = "---\ndescription: x\nenabled: false # off\ntools: none\n---\nbody\n";
        assert!(is_disabled_content(mid), "带行尾注释也视为禁用");
        // 大小写变体：加载侧（大小写不敏感）认它是禁用；但逐行编辑只认小写无引号写法，
        // 因此删除会**如实报告未改动**（上游同款 best-effort 语义）
        let odd = "---\nenabled: False\n---\nbody\n";
        assert!(is_disabled_content(odd), "加载侧是大小写不敏感的");
        let (_, changed) = enable_in_content(odd);
        assert!(!changed, "无法改写的写法必须报告未改动，而不是谎报");
        let (same, outcome) = disable_in_content(odd);
        assert_eq!(outcome, DisableOutcome::AlreadyDisabled);
        assert_eq!(same, odd);

        // 引号包住的值同理
        let quoted = "---\nenabled: \"false\"\n---\nbody\n";
        let (_, changed) = enable_in_content(quoted);
        assert!(!changed);
    }

    #[test]
    fn serialize_round_trips_through_the_loader() {
        let ty = AgentType {
            name: "auditor".to_string(),
            display_name: "Auditor".to_string(),
            description: "Audit: things #loud".to_string(),
            color: Some("teal".to_string()),
            tools: Some(vec!["read".to_string(), "grep".to_string()]),
            model: Some("anthropic/claude-haiku-4-5".to_string()),
            thinking: Some("high".to_string()),
            max_turns: Some(12),
            prompt_mode: PromptMode::Append,
            enabled: true,
            persist_session: Some(true),
            session_dir: None,
            disallowed_tools: Some(vec!["bash".to_string()]),
            allowed_subagents: Some(vec!["Explore".to_string()]),
            memory: Some(crate::extensions::subagent::types::MemoryScope::Project),
            isolated: Some(false),
            isolation: Some("worktree".to_string()),
            extensions: None,
            extensions_none: true,
            exclude_extensions: vec!["proxy".to_string()],
            skills: Some(Some(vec!["alpha".to_string()])),
            output_transcript: Some(false),
            run_in_background: Some(false),
            inherit_context: Some(true),
            system_prompt: "You audit things.".to_string(),
            source: AgentSource::Builtin,
            source_path: None,
            ignored_fields: Vec::new(),
        };
        let text = serialize_agent_file(&ty);
        let parsed = agent_types::parse_agent_file(
            &text,
            "fallback",
            AgentSource::Global,
            Path::new("auditor.md"),
        )
        .expect("eject 产物必须能被加载器读回");
        assert_eq!(parsed.name, "auditor");
        assert_eq!(
            parsed.description, "Audit: things #loud",
            "特殊字符需加引号"
        );
        assert_eq!(parsed.display_name, "Auditor");
        assert_eq!(parsed.color.as_deref(), Some("teal"));
        assert_eq!(parsed.tools.as_ref().unwrap().len(), 2);
        assert_eq!(parsed.max_turns, Some(12));
        assert_eq!(parsed.prompt_mode, PromptMode::Append);
        assert_eq!(
            parsed.disallowed_tools.as_ref().unwrap(),
            &vec!["bash".to_string()]
        );
        assert_eq!(parsed.run_in_background, Some(false));
        assert_eq!(parsed.inherit_context, Some(true));
        assert_eq!(parsed.system_prompt, "You audit things.");
        // eject 不丢字段：嵌套/记忆/隔离/扩展作用域/技能都要往返
        assert_eq!(
            parsed.allowed_subagents.as_ref().unwrap(),
            &vec!["Explore".to_string()],
            "allowed_subagents 应往返"
        );
        assert_eq!(
            parsed.memory,
            Some(crate::extensions::subagent::types::MemoryScope::Project)
        );
        assert_eq!(parsed.isolation.as_deref(), Some("worktree"));
        assert!(parsed.extensions_none, "extensions: false 应往返");
        assert_eq!(parsed.exclude_extensions, vec!["proxy".to_string()]);
        assert_eq!(
            parsed.skills,
            Some(Some(vec!["alpha".to_string()])),
            "skills 预载列表应往返"
        );

        // `allowed_subagents: all` 的往返（Some([]) → 写 `all` → 读回 Some([])）
        let mut all_nested = ty.clone();
        all_nested.allowed_subagents = Some(Vec::new());
        let text = serialize_agent_file(&all_nested);
        assert!(text.contains("allowed_subagents: all"), "{text}");
        let parsed = agent_types::parse_agent_file(
            &text,
            "fallback",
            AgentSource::Global,
            Path::new("auditor.md"),
        )
        .unwrap();
        assert_eq!(parsed.allowed_subagents, Some(Vec::new()));

        // tools: none / all 的两种含义
        let mut none = ty.clone();
        none.tools = Some(Vec::new());
        assert!(serialize_agent_file(&none).contains("tools: none"));
        let mut all = ty.clone();
        all.tools = None;
        assert!(serialize_agent_file(&all).contains("tools: all"));
        // 禁用态会被写出
        let mut off = ty.clone();
        off.enabled = false;
        assert!(serialize_agent_file(&off).contains("enabled: false"));
    }

    #[test]
    fn eject_paths_are_scoped_and_sanitized() {
        let _ad = AgentDirGuard::temp();
        let global = eject_path("Explore", "/tmp/proj", false);
        assert!(
            global.starts_with(settings_manager::agent_dir().join(GLOBAL_AGENT_DIR)),
            "{}",
            global.display()
        );
        assert_eq!(global.file_name().unwrap(), "Explore.md");

        let project = eject_path("../../etc/passwd", "/tmp/proj", true);
        assert!(
            project.starts_with(Path::new("/tmp/proj").join(crate::PROJECT_SCOPE_NAME)),
            "不得越出项目目录: {}",
            project.display()
        );
        assert_eq!(project.file_name().unwrap(), "etc-passwd.md");

        // 定位：source_path 失效时按发现优先级探测（全局）
        let global_dir = settings_manager::agent_dir().join(GLOBAL_AGENT_DIR);
        std::fs::create_dir_all(&global_dir).unwrap();
        let file = global_dir.join("auditor.md");
        std::fs::write(&file, "---\ndescription: x\n---\nbody\n").unwrap();
        let mut ty = AgentType {
            name: "auditor".to_string(),
            display_name: "auditor".to_string(),
            description: "x".to_string(),
            color: None,
            tools: None,
            model: None,
            thinking: None,
            max_turns: None,
            prompt_mode: PromptMode::Replace,
            enabled: true,
            persist_session: None,
            session_dir: None,
            disallowed_tools: None,
            allowed_subagents: None,
            memory: None,
            isolated: None,
            isolation: None,
            extensions: None,
            extensions_none: false,
            exclude_extensions: Vec::new(),
            skills: None,
            output_transcript: None,
            run_in_background: None,
            inherit_context: None,
            system_prompt: String::new(),
            source: AgentSource::Builtin,
            source_path: Some(PathBuf::from("/nonexistent/stale.md")),
            ignored_fields: Vec::new(),
        };
        assert_eq!(find_agent_file(&ty, "/tmp/proj"), Some(file.clone()));
        ty.source_path = Some(file.clone());
        assert_eq!(find_agent_file(&ty, "/tmp/proj"), Some(file));
    }
}

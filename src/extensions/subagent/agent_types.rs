//! agent 类型的发现、frontmatter 解析、默认三类型与类型解析。
//!
//! 两个来源：
//! 1. 项目级 `<cwd>/PROJECT_SCOPE_NAME/agents/`（**受项目信任门控**）
//! 2. 全局级 `<agent_dir()>/extensions/agent/`
//!
//! 覆盖规则：项目 > 全局 > 内嵌默认；同层内后加载者胜并告警。
//! 每次 spawn 现扫目录（不做缓存），因此改 `.md` 立刻生效。

use super::{
    prompt::{
        EXPLORE_DESCRIPTION, EXPLORE_PROMPT, GENERAL_PURPOSE_DESCRIPTION, PLAN_DESCRIPTION,
        PLAN_PROMPT,
    },
    types::{AgentSource, AgentType, MemoryScope, PromptMode},
};
use crate::{
    PROJECT_SCOPE_NAME, core,
    core::{project_trust, settings_manager},
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// 全局 agent 目录名（挂在 `<agent_dir()>/extensions` 下）。
pub const GLOBAL_AGENT_DIR: &str = "extensions/agent";
/// 项目 agent 目录名（挂在 `<cwd>/PROJECT_SCOPE_NAME/` 下）。
pub const PROJECT_AGENT_DIR: &str = "agents";
/// 出现时忽略并一次性告警，而不是静默降级成别的行为。
pub const UNSUPPORTED_FIELDS: &[&str] = &[];

/// 发现过程中的告警（供调用方去重后通知用户）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryWarning {
    /// 关联文件/目录；None = 与具体文件无关（如未信任）
    pub source: Option<PathBuf>,
    /// 人类可读的告警文本。
    pub message: String,
}

/// 发现结果。
pub struct Roster {
    /// 发现的全部类型（含 `enabled: false` 的）。
    pub types: Vec<AgentType>,
    /// 发现期告警（由调用方按 (来源, 原因) 去重展示）。
    pub warnings: Vec<DiscoveryWarning>,
    /// 致命解析错误（仅在 `DiscoveryOptions::strict` 下产生）
    pub errors: Vec<String>,
}

impl Roster {
    /// 启用中的类型（`/agents types` 与工具描述清单用）。
    pub fn enabled(&self) -> impl Iterator<Item = &AgentType> {
        self.types.iter().filter(|t| t.enabled)
    }
}

/// 按名字解析代理类型时的失败原因：不存在、被禁用或仅有大小写歧义。
#[derive(Debug)]
pub enum ResolveError {
    /// 没有任何同名类型
    Unknown,
    /// 同名类型被 `enabled: false` 禁用
    Disabled,
    /// 多个类型只差大小写
    Ambiguous(Vec<String>),
}

/// 内嵌默认三类型。
pub fn default_types() -> Vec<AgentType> {
    vec![
        AgentType {
            name: "general-purpose".to_string(),
            display_name: "Agent".to_string(),
            description: GENERAL_PURPOSE_DESCRIPTION.to_string(),
            color: None,
            tools: None, // None = 全部工具（核心会再剔除子代理工具）
            model: None,
            thinking: None,
            max_turns: None,
            prompt_mode: PromptMode::Append, // 父级孪生：追加桥接段而非替换
            enabled: true,
            persist_session: None,
            session_dir: None,
            system_prompt: String::new(),
            source: AgentSource::Builtin,
            source_path: None,
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
            ignored_fields: Vec::new(),
        },
        AgentType {
            name: "Explore".to_string(),
            display_name: "Explore".to_string(),
            description: EXPLORE_DESCRIPTION.to_string(),
            color: None,
            tools: Some(read_only_tools()),
            model: None,
            thinking: None,
            max_turns: None,
            prompt_mode: PromptMode::Replace,
            enabled: true,
            persist_session: None,
            session_dir: None,
            system_prompt: EXPLORE_PROMPT.to_string(),
            source: AgentSource::Builtin,
            source_path: None,
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
            ignored_fields: Vec::new(),
        },
        AgentType {
            name: "Plan".to_string(),
            display_name: "Plan".to_string(),
            description: PLAN_DESCRIPTION.to_string(),
            color: None,
            tools: Some(read_only_tools()),
            model: None,
            thinking: None,
            max_turns: None,
            prompt_mode: PromptMode::Replace,
            enabled: true,
            persist_session: None,
            session_dir: None,
            system_prompt: PLAN_PROMPT.to_string(),
            source: AgentSource::Builtin,
            source_path: None,
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
            ignored_fields: Vec::new(),
        },
    ]
}

/// 只读工具的默认集合（read/bash/grep/find/ls）。
fn read_only_tools() -> Vec<String> {
    ["read", "bash", "grep", "find", "ls"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// 发现期的配置开关（来自 `subagent.json`）。
#[derive(Debug, Clone, Copy, Default)]
pub struct DiscoveryOptions {
    /// 解析失败是否致命（false = 跳过 + 告警）
    pub strict: bool,
    /// 是否不注册内嵌默认三类型
    pub disable_defaults: bool,
}

/// 发现全部 agent 类型（默认 → 全局 → 项目，后者覆盖前者）。
pub fn discover(cwd: &str) -> Roster {
    discover_with(cwd, DiscoveryOptions::default())
}

/// 按配置发现（`strict` 下坏文件收集到 `errors`；`disable_defaults` 下不注册默认三类型）。
pub fn discover_with(cwd: &str, opts: DiscoveryOptions) -> Roster {
    let mut warnings: Vec<DiscoveryWarning> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut entries: Vec<AgentType> = if opts.disable_defaults {
        Vec::new()
    } else {
        default_types()
    };

    let cwd_path = PathBuf::from(cwd);
    let agent_dir = settings_manager::agent_dir();

    load_dir(
        &agent_dir.join(GLOBAL_AGENT_DIR),
        AgentSource::Global,
        &mut entries,
        &mut warnings,
        &mut errors,
        opts.strict,
    );

    let project_dir = cwd_path.join(PROJECT_SCOPE_NAME).join(PROJECT_AGENT_DIR);
    if project_dir.is_dir() {
        if project_trust::is_project_trusted(&cwd_path, &agent_dir) {
            load_dir(
                &project_dir,
                AgentSource::Project,
                &mut entries,
                &mut warnings,
                &mut errors,
                opts.strict,
            );
        } else {
            warnings.push(DiscoveryWarning {
                source: Some(project_dir),
                message: "project agent definitions ignored: project is not trusted".to_string(),
            });
        }
    }

    let types = dedupe_by_name(entries, &mut warnings);

    // 文件里写了本版本不支持的字段：忽略并告警（不是静默降级）
    for ty in types.iter().filter(|t| t.source != AgentSource::Builtin) {
        for field in &ty.ignored_fields {
            warnings.push(DiscoveryWarning {
                source: ty.source_path.clone(),
                message: format!(
                    "agent type {:?}: frontmatter field {field:?} is not supported in this version and was ignored",
                    ty.name
                ),
            });
        }
    }

    Roster {
        types,
        warnings,
        errors,
    }
}

/// 同名合并：保留最后出现的一个（后加载者胜），被覆盖者记一条告警。
fn dedupe_by_name(entries: Vec<AgentType>, warnings: &mut Vec<DiscoveryWarning>) -> Vec<AgentType> {
    let mut out: Vec<AgentType> = Vec::new();
    for ty in entries {
        let existing = out
            .iter_mut()
            .find(|e| e.name.eq_ignore_ascii_case(&ty.name));

        match existing {
            Some(slot) => {
                let prev_source = slot
                    .source_path
                    .as_ref()
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| slot.source.as_str().to_string());

                let message = format!(
                    "agent type {:?} from {} overrides the definition from {}",
                    ty.name,
                    ty.source_path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| ty.source.as_str().to_string()),
                    prev_source,
                );

                warnings.push(DiscoveryWarning {
                    source: ty.source_path.clone(),
                    message,
                });
                *slot = ty;
            }
            None => out.push(ty),
        }
    }
    out
}

/// 读取一个目录下的 `*.md`（按文件名字典序，保证同层覆盖结果确定）。
fn load_dir(
    dir: &Path,
    source: AgentSource,
    out: &mut Vec<AgentType>,
    warnings: &mut Vec<DiscoveryWarning>,
    errors: &mut Vec<String>,
    strict: bool,
) {
    if !dir.is_dir() {
        return;
    }

    let Ok(read) = std::fs::read_dir(dir) else {
        warnings.push(DiscoveryWarning {
            source: Some(dir.to_path_buf()),
            message: format!("cannot read agent directory: {}", dir.display()),
        });
        return;
    };

    let mut files: Vec<PathBuf> = read
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("md"))
        .collect();
    files.sort();

    for path in files {
        let fallback = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("agent")
            .to_string();

        match std::fs::read_to_string(&path) {
            Ok(content) => match parse_agent_file(&content, &fallback, source, &path) {
                Ok(ty) => out.push(ty),
                Err(err) if strict => errors.push(format!("{}: {err}", path.display())),
                Err(err) => warnings.push(DiscoveryWarning {
                    source: Some(path),
                    message: err,
                }),
            },
            Err(err) => warnings.push(DiscoveryWarning {
                source: Some(path),
                message: format!("cannot read agent file: {err}"),
            }),
        }
    }
}

/// frontmatter 值（手写 mini YAML 子集，零新依赖）。
#[derive(Debug, Clone, PartialEq)]
enum FmValue {
    /// 标量字符串，解析后仍保留原样供 trim 处理。
    Str(String),
    /// 布尔标量，如 frontmatter 中的 true / false。
    Bool(bool),
    /// 整数标量，用于端口、步数等数值配置项。
    Num(i64),
    /// 列表值，来自逗号分隔串或内联数组语法。
    List(Vec<String>),
}

impl FmValue {
    /// 取字符串值（去掉首尾空白后非空）；`Num` 转成十进制串，其余变体为 `None`。
    fn as_str(&self) -> Option<String> {
        match self {
            FmValue::Str(s) if !s.trim().is_empty() => Some(s.trim().to_string()),
            FmValue::Num(n) => Some(n.to_string()),
            _ => None,
        }
    }

    /// 取布尔值：`Bool` 直取，字符串接受 true/yes/on 与 false/no/off（大小写不敏感），其余为 `None`。
    fn as_bool(&self) -> Option<bool> {
        match self {
            FmValue::Bool(b) => Some(*b),
            FmValue::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "yes" | "on" => Some(true),
                "false" | "no" | "off" => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    /// 取非负整数：`Num` 需 ≥0，字符串按十进制解析，失败为 `None`。
    fn as_u32(&self) -> Option<u32> {
        match self {
            FmValue::Num(n) if *n >= 0 => Some(*n as u32),
            FmValue::Str(s) => s.trim().parse::<u32>().ok(),
            _ => None,
        }
    }

    /// 逗号分隔列表或内联数组 → 字符串列表。
    fn as_list(&self) -> Vec<String> {
        match self {
            FmValue::List(l) => l.clone(),
            FmValue::Str(s) => s
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect(),
            FmValue::Num(n) => vec![n.to_string()],
            FmValue::Bool(_) => Vec::new(),
        }
    }
}

/// 展开 `tools:` 里的 `ext:<扩展>` / `ext:<扩展>/<工具>` 选择器。
///
/// 返回 `(展开后的列表, 未知选择器)`。未知选择器**保留原样**（核心收窄时自然不匹配，
/// 由调用方一次性告警），这样拼错不会静默变成"全部工具"。
///
/// 展开依赖进程内注册表（`core::extensions::registered()`）：因此只有**已启用**的扩展
/// 能被选择器命中，与"child 只可能用到已启用扩展"一致。
pub fn expand_ext_selectors(list: &[String]) -> (Vec<String>, Vec<String>) {
    let mut out: Vec<String> = Vec::new();
    let mut unknown: Vec<String> = Vec::new();
    let registry = core::extensions::registered();

    for item in list {
        let trimmed = item.trim();
        let Some(rest) = trimmed.strip_prefix("ext:") else {
            out.push(trimmed.to_string());
            continue;
        };
        let (ext_name, tool_name) = match rest.split_once('/') {
            Some((e, t)) => (e.trim(), Some(t.trim())),
            None => (rest.trim(), None),
        };
        let Some(ext) = registry
            .iter()
            .find(|e| e.name().eq_ignore_ascii_case(ext_name))
        else {
            unknown.push(trimmed.to_string());
            out.push(trimmed.to_string());
            continue;
        };
        let names: Vec<String> = ext.tools().into_iter().map(|t| t.name).collect();

        match tool_name {
            None => {
                if names.is_empty() {
                    unknown.push(trimmed.to_string());
                    out.push(trimmed.to_string());
                } else {
                    out.extend(names);
                }
            }
            Some(want) => {
                if let Some(hit) = names.iter().find(|n| n.eq_ignore_ascii_case(want)) {
                    out.push(hit.clone());
                } else {
                    unknown.push(trimmed.to_string());
                    out.push(trimmed.to_string());
                }
            }
        }
    }
    out.dedup();
    (out, unknown)
}

/// 按名字查找类型（**忽略 `enabled`**）：`/agents enable|disable|eject` 需要能操作
/// 一个当前被禁用的类型（[`resolve`] 会返回 `Disabled`，那些操作就得先能找到它）。
pub fn find_any<'a>(types: &'a [AgentType], name: &str) -> Option<&'a AgentType> {
    types
        .iter()
        .rev()
        .find(|t| t.name.eq_ignore_ascii_case(name.trim()))
}

/// 解析一个 agent `.md`。
pub fn parse_agent_file(
    content: &str,
    fallback_name: &str,
    source: AgentSource,
    path: &Path,
) -> Result<AgentType, String> {
    let (fm, body) = split_frontmatter(content)
        .ok_or_else(|| "missing YAML frontmatter (expected a leading --- block)".to_string())?;
    let map = parse_frontmatter(fm);

    let raw_name = map
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback_name.to_string());

    if raw_name.contains(':') {
        return Err(format!(
            "agent name {:?} is invalid: ':' is reserved for plugin-scoped identifiers",
            raw_name
        ));
    }

    let display_name = map
        .get("display_name")
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| raw_name.clone());

    let tools = parse_tools(&map);
    let (extensions, extensions_none) = parse_extensions(&map);
    let skills = parse_skills(&map);
    let allowed_subagents = parse_allowed_subagents(&map);

    let prompt_mode = map
        .get("prompt_mode")
        .and_then(|v| v.as_str())
        .and_then(|s| PromptMode::parse(&s))
        .unwrap_or(PromptMode::Replace);

    let as_bool = |key: &str| map.get(key).and_then(|v| v.as_bool());
    let list = |key: &str| map.get(key).map(|v| v.as_list());

    Ok(AgentType {
        name: raw_name,
        display_name,
        description: map
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or_else(|| map.get("name").and_then(|v| v.as_str()).unwrap_or_default()),
        color: map.get("color").and_then(|v| v.as_str()),
        tools,
        model: map.get("model").and_then(|v| v.as_str()),
        thinking: map.get("thinking").and_then(|v| v.as_str()),
        max_turns: map.get("max_turns").and_then(|v| v.as_u32()),
        prompt_mode,
        enabled: map.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true),
        persist_session: map.get("persist_session").and_then(|v| v.as_bool()),
        session_dir: map.get("session_dir").and_then(|v| v.as_str()),
        system_prompt: body.trim().to_string(),
        source,
        source_path: Some(path.to_path_buf()),
        disallowed_tools: list("disallowed_tools").filter(|l| !l.is_empty()),
        allowed_subagents,
        memory: map
            .get("memory")
            .and_then(|v| v.as_str())
            .and_then(|s| MemoryScope::parse(&s)),
        isolated: as_bool("isolated"),
        isolation: map
            .get("isolation")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        extensions,
        extensions_none,
        exclude_extensions: list("exclude_extensions").unwrap_or_default(),
        skills,
        output_transcript: as_bool("output_transcript"),
        run_in_background: as_bool("run_in_background"),
        inherit_context: as_bool("inherit_context"),
        ignored_fields: UNSUPPORTED_FIELDS
            .iter()
            .filter(|k| map.contains_key(**k))
            .map(|k| (*k).to_string())
            .collect(),
    })
}

/// `tools:`：`*`/`all` 与缺省等价于"全部"（None）；`none` 与空列表等价于"无"。
fn parse_tools(map: &HashMap<String, FmValue>) -> Option<Vec<String>> {
    match map.get("tools") {
        None => None,
        Some(v) => {
            let list = v.as_list();
            let lowered: Vec<String> = list.iter().map(|s| s.to_ascii_lowercase()).collect();
            if lowered.iter().any(|s| s == "*" || s == "all") || lowered.is_empty() {
                if lowered.iter().any(|s| s == "none") {
                    Some(Vec::new())
                } else {
                    None
                }
            } else if lowered.iter().any(|s| s == "none") {
                Some(Vec::new())
            } else {
                Some(list)
            }
        }
    }
}

/// `extensions:` 三态：`false`（无扩展）/ 列表（只用这些）/ 缺省（全部）。
/// 返回 `(白名单, extensions_none)`。
fn parse_extensions(map: &HashMap<String, FmValue>) -> (Option<Vec<String>>, bool) {
    let none = matches!(map.get("extensions"), Some(FmValue::Bool(false)));
    let list = match map.get("extensions") {
        Some(FmValue::Bool(true)) | None | Some(FmValue::Bool(false)) => None,
        Some(v) => {
            let l = v.as_list();
            if l.is_empty() { None } else { Some(l) }
        }
    };
    (list, none)
}

/// `skills:` 三态：`false` / 列表 / 缺省（继承）。
fn parse_skills(map: &HashMap<String, FmValue>) -> Option<Option<Vec<String>>> {
    match map.get("skills") {
        None | Some(FmValue::Bool(true)) => None,
        Some(FmValue::Bool(false)) => Some(None),
        Some(v) => {
            let l = v.as_list();
            if l.is_empty() {
                Some(None)
            } else {
                Some(Some(l))
            }
        }
    }
}

/// `allowed_subagents:`：缺省/`false` = 不开启嵌套；
/// `true`/`all`/`*` = 开启且不限型（归一成空列表）；
/// 列表 = 开启且限定这些类型。
fn parse_allowed_subagents(map: &HashMap<String, FmValue>) -> Option<Vec<String>> {
    match map.get("allowed_subagents") {
        None | Some(FmValue::Bool(false)) => None,
        Some(FmValue::Bool(true)) => Some(Vec::new()),
        Some(v) => {
            let l = v.as_list();
            let is_all = l.iter().any(|s| {
                let s = s.trim();
                s == "*" || s.eq_ignore_ascii_case("all")
            });
            // `all`/`*` = 开启且不限型；列表 = 开启且限定这些类型
            Some(if is_all { Vec::new() } else { l })
        }
    }
}

/// 切分 frontmatter 与正文（容忍 BOM）。
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let (first, rest) = content.split_once('\n')?;
    if first.trim() != "---" {
        return None;
    }

    // 空 frontmatter：`---` 与闭合符相邻
    if let Some(after) = rest.strip_prefix("---") {
        let body = after.strip_prefix('\n').unwrap_or(after);
        return Some(("", body));
    }

    let (fm, after) = rest.split_once("\n---")?;
    // `after` 以闭合行剩余部分开头（通常为空或以换行继续）
    let body = after.strip_prefix('\n').unwrap_or(after);
    Some((fm, body))
}

/// 解析 frontmatter 行：仅支持顶层 `key: value`（不解析嵌套块/多行标量）。
fn parse_frontmatter(fm: &str) -> HashMap<String, FmValue> {
    let mut map = HashMap::new();
    for raw in fm.lines() {
        if raw.starts_with(' ') || raw.starts_with('\t') {
            continue;
        }
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().replace('-', "_").to_ascii_lowercase();
        if key.is_empty() {
            continue;
        }
        map.insert(key, parse_value(value.trim()));
    }
    map
}

/// 把 frontmatter 单个标量文本解析为 `FmValue`：识别 `[a, b]` 列表、真/假字面量与整数，其余按字符串去引号。
fn parse_value(raw: &str) -> FmValue {
    let raw = strip_inline_comment(raw);
    if raw.len() >= 2 && raw.starts_with('[') && raw.ends_with(']') {
        let inner = &raw[1..raw.len() - 1];
        return FmValue::List(
            inner
                .split(',')
                .map(|s| unquote(s.trim()))
                .filter(|s| !s.is_empty())
                .collect(),
        );
    }
    match raw.to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" => return FmValue::Bool(true),
        "false" | "no" | "off" => return FmValue::Bool(false),
        _ => {}
    }
    if let Ok(n) = raw.parse::<i64>() {
        return FmValue::Num(n);
    }
    FmValue::Str(unquote(raw))
}

/// 去掉未加引号值里的行内注释（` #` 之后）。
fn strip_inline_comment(raw: &str) -> &str {
    if raw.starts_with('"') || raw.starts_with('\'') {
        return raw;
    }
    match raw.find(" #") {
        Some(i) => raw[..i].trim_end(),
        None => raw,
    }
}

/// 去掉成对的首尾引号（`"` 或 `'`）；未加引号则原样返回。
fn unquote(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let b = s.as_bytes();
        let q = b[0];
        if (q == b'"' || q == b'\'') && b[s.len() - 1] == q {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// 按名字解析类型（大小写不敏感）。
pub fn resolve<'a>(types: &'a [AgentType], name: &str) -> Result<&'a AgentType, ResolveError> {
    let matches: Vec<&AgentType> = types
        .iter()
        .filter(|t| t.name.eq_ignore_ascii_case(name.trim()))
        .collect();
    match matches.len() {
        0 => Err(ResolveError::Unknown),
        1 if !matches[0].enabled => Err(ResolveError::Disabled),
        1 => Ok(matches[0]),
        _ => Err(ResolveError::Ambiguous(
            matches.iter().map(|t| t.name.clone()).collect(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "prux-subagent-types-{tag}-{}",
            crate::utils::time::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn frontmatter_splits_and_tolerates_bom() {
        let (fm, body) = split_frontmatter("\u{feff}---\nname: x\n---\nBODY\n").unwrap();
        assert!(fm.contains("name: x"));
        assert_eq!(body.trim(), "BODY");
        assert!(split_frontmatter("no frontmatter").is_none());
    }

    #[test]
    fn values_parse_arrays_bools_numbers_quotes() {
        let fm = parse_frontmatter(
            "name: auditor\ntools: [read, grep]\nenabled: false\nmax_turns: 30\ncolor: \"#8B5CF6\"\ndescription: A, B\n",
        );
        assert_eq!(
            fm["tools"],
            FmValue::List(vec!["read".into(), "grep".into()])
        );
        assert_eq!(fm["enabled"], FmValue::Bool(false));
        assert_eq!(fm["max_turns"], FmValue::Num(30));
        assert_eq!(fm["color"], FmValue::Str("#8B5CF6".into()));
        assert_eq!(fm["description"].as_list(), vec!["A", "B"]);
    }

    #[test]
    fn tools_comma_list_and_all_none() {
        let p = PathBuf::from("/tmp/x.md");
        let mk = |t: &str| {
            let content = format!("---\nname: a\ntools: {t}\n---\nbody");
            parse_agent_file(&content, "a", AgentSource::Project, &p).unwrap()
        };
        assert_eq!(
            mk("read, grep, find").tools,
            Some(vec!["read".into(), "grep".into(), "find".into()])
        );
        assert_eq!(
            mk("[read, grep]").tools,
            Some(vec!["read".into(), "grep".into()])
        );
        assert_eq!(mk("all").tools, None);
        assert_eq!(mk("*").tools, None);
        assert_eq!(mk("none").tools, Some(vec![]));
    }

    #[test]
    fn name_with_colon_is_rejected() {
        let p = PathBuf::from("/tmp/x.md");
        let err = parse_agent_file(
            "---\nname: plugin:agent\n---\nbody",
            "x",
            AgentSource::Global,
            &p,
        )
        .unwrap_err();
        assert!(err.contains("reserved"), "{err}");
    }

    #[test]
    fn defaults_apply_when_fields_absent() {
        let p = PathBuf::from("/tmp/x.md");
        let ty = parse_agent_file("---\n---\nbody", "myagent", AgentSource::Global, &p).unwrap();
        assert_eq!(ty.name, "myagent");
        assert_eq!(ty.display_name, "myagent");
        assert_eq!(ty.tools, None);
        assert_eq!(ty.prompt_mode, PromptMode::Replace);
        assert!(ty.enabled);
    }

    #[test]
    fn resolve_is_case_insensitive_and_reports_ambiguity() {
        let mut types = default_types();
        assert!(resolve(&types, "explore").is_ok());
        assert!(resolve(&types, "EXPLORE").is_ok());
        assert!(matches!(
            resolve(&types, "nope"),
            Err(ResolveError::Unknown)
        ));

        types[0].enabled = false;
        assert!(matches!(
            resolve(&types, "general-purpose"),
            Err(ResolveError::Disabled)
        ));

        let mut dup = default_types();
        dup.push(AgentType {
            name: "EXPLORE".to_string(),
            ..dup[1].clone()
        });
        assert!(matches!(
            resolve(&dup, "explore"),
            Err(ResolveError::Ambiguous(_))
        ));
    }

    #[test]
    fn project_overrides_global_and_warns() {
        let agent_home = tmpdir("home");
        let _guard = AgentDirGuard::set(&agent_home);
        let cwd = tmpdir("cwd");

        let global = agent_home.join(GLOBAL_AGENT_DIR);
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("audit.md"),
            "---\nname: audit\ndescription: from global\n---\nglobal body",
        )
        .unwrap();

        let project = cwd.join(PROJECT_SCOPE_NAME).join(PROJECT_AGENT_DIR);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("audit.md"),
            "---\nname: audit\ndescription: from project\n---\nproject body",
        )
        .unwrap();

        // 未信任：项目目录被忽略并告警
        let roster = discover(&cwd.to_string_lossy());
        let audit = resolve(&roster.types, "audit").unwrap();
        assert_eq!(audit.description.trim(), "from global");
        assert_eq!(audit.system_prompt, "global body");
        assert!(
            roster
                .warnings
                .iter()
                .any(|w| w.message.contains("not trusted")),
            "{:?}",
            roster.warnings
        );

        // 信任后：项目覆盖全局并告警
        project_trust::set_project_trust(&cwd, &agent_home, true);
        let roster = discover(&cwd.to_string_lossy());
        let audit = resolve(&roster.types, "audit").unwrap();
        assert_eq!(audit.description.trim(), "from project");
        assert_eq!(audit.system_prompt, "project body");
        assert!(
            roster
                .warnings
                .iter()
                .any(|w| w.message.contains("overrides")),
            "{:?}",
            roster.warnings
        );
    }

    #[test]
    fn broken_file_is_skipped_with_warning() {
        let agent_home = tmpdir("broken");
        let _guard = AgentDirGuard::set(&agent_home);
        let cwd = tmpdir("broken-cwd");
        let global = agent_home.join(GLOBAL_AGENT_DIR);
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(global.join("bad.md"), "no frontmatter here").unwrap();

        let roster = discover(&cwd.to_string_lossy());
        assert!(roster.types.iter().all(|t| t.name != "bad"));
        assert!(
            roster
                .warnings
                .iter()
                .any(|w| w.message.contains("missing YAML frontmatter")),
            "{:?}",
            roster.warnings
        );
    }

    /// `ext:` 选择器：展开成该扩展的工具名；未知选择器原样保留（不静默变"全部"）。
    #[test]
    fn ext_selectors_expand_against_the_registry() {
        let _auth = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // 本测试会 `set_extension_enabled("subagent", false)` → on_enabled_changed(false)
        // → manager::reset_all()：必须与触碰 manager 全局态的测试串行。
        let _g = crate::extensions::subagent::test_lock();
        crate::core::extensions::register_extension(crate::extensions::subagent::Subagent);
        crate::core::extensions::set_extension_enabled("subagent", true);

        // 整个扩展：展开成它的全部工具
        let (expanded, unknown) = expand_ext_selectors(&["ext:subagent".to_string()]);
        assert!(expanded.contains(&"Agent".to_string()), "{expanded:?}");
        assert!(
            expanded.contains(&"steer_subagent".to_string()),
            "{expanded:?}"
        );
        assert!(unknown.is_empty(), "{unknown:?}");

        // 单个工具 + 混写普通工具名
        let (expanded, unknown) =
            expand_ext_selectors(&["read".to_string(), "ext:subagent/Agent".to_string()]);
        assert_eq!(expanded, vec!["read".to_string(), "Agent".to_string()]);
        assert!(unknown.is_empty());

        // 扩展不存在 / 工具不存在 → 原样保留 + 回报
        let (expanded, unknown) =
            expand_ext_selectors(&["ext:nope".to_string(), "ext:subagent/nope".to_string()]);
        assert_eq!(
            expanded,
            vec!["ext:nope".to_string(), "ext:subagent/nope".to_string()]
        );
        assert_eq!(unknown.len(), 2, "{unknown:?}");

        crate::core::extensions::set_extension_enabled("subagent", false);
        crate::core::extensions::unregister_extension("subagent");
    }

    /// `extensions:` 三态与 `skills:` 三态的解析。
    #[test]
    fn parse_extensions_and_skills_tristates() {
        let agent_home = tmpdir("tristate");
        let _guard = AgentDirGuard::set(&agent_home);
        let cwd = tmpdir("tristate-cwd");
        let global = agent_home.join(GLOBAL_AGENT_DIR);
        std::fs::create_dir_all(&global).unwrap();

        std::fs::write(
            global.join("a.md"),
            "---\nname: a\ndescription: d\nextensions: false\nskills: false\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            global.join("b.md"),
            "---\nname: b\ndescription: d\nextensions: ext-one, ext-two\nskills: alpha, beta\nexclude_extensions: noisy\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            global.join("c.md"),
            "---\nname: c\ndescription: d\n---\nbody",
        )
        .unwrap();

        let roster = discover(&cwd.to_string_lossy());
        let by = |n: &str| roster.types.iter().find(|t| t.name == n).unwrap();

        let a = by("a");
        assert!(
            a.extensions_none,
            "`extensions: false` 应置 extensions_none"
        );
        assert!(a.extensions.is_none());
        assert_eq!(a.skills, Some(None), "`skills: false` = 三态里的不继承");
        assert!(a.ignored_fields.is_empty(), "{:?}", a.ignored_fields);

        let b = by("b");
        assert!(!b.extensions_none);
        assert_eq!(
            b.extensions.as_ref().unwrap(),
            &vec!["ext-one".to_string(), "ext-two".to_string()]
        );
        assert_eq!(b.exclude_extensions, vec!["noisy".to_string()]);
        assert_eq!(
            b.skills,
            Some(Some(vec!["alpha".to_string(), "beta".to_string()])),
            "具名列表 = 预载这些技能"
        );

        let c = by("c");
        assert!(c.extensions.is_none() && !c.extensions_none && c.skills.is_none());
        // `allowed_subagents` 已成为真支持：解析成白名单、不再进 ignored_fields
        std::fs::write(
            global.join("d.md"),
            "---\nname: d\ndescription: d\nallowed_subagents: all\n---\nbody",
        )
        .unwrap();
        std::fs::write(
            global.join("e.md"),
            "---\nname: e\ndescription: e\nallowed_subagents: Explore, plan\nmemory: project\n---\nbody",
        )
        .unwrap();
        let roster = discover(&cwd.to_string_lossy());
        let d = roster.types.iter().find(|t| t.name == "d").unwrap();
        assert!(d.ignored_fields.is_empty(), "{:?}", d.ignored_fields);
        assert_eq!(
            d.allowed_subagents,
            Some(Vec::new()),
            "all = 开启嵌套且不限型"
        );
        let e = roster.types.iter().find(|t| t.name == "e").unwrap();
        assert_eq!(
            e.allowed_subagents.as_ref().unwrap(),
            &vec!["Explore".to_string(), "plan".to_string()]
        );
        assert_eq!(e.memory, Some(MemoryScope::Project), "memory 已解析");
    }

    #[test]
    fn enabled_false_disables_a_default_type() {
        let agent_home = tmpdir("disable");
        let _guard = AgentDirGuard::set(&agent_home);
        let cwd = tmpdir("disable-cwd");
        let global = agent_home.join(GLOBAL_AGENT_DIR);
        std::fs::create_dir_all(&global).unwrap();
        std::fs::write(
            global.join("plan.md"),
            "---\nname: Plan\nenabled: false\n---\n",
        )
        .unwrap();

        let roster = discover(&cwd.to_string_lossy());
        assert!(matches!(
            resolve(&roster.types, "Plan"),
            Err(ResolveError::Disabled)
        ));
    }
}

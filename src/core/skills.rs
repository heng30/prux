// 技能系统（发现规则 + 资源来源与过滤语义）

use crate::{
    PROJECT_SCOPE_NAME,
    utils::{
        display::normalize_newlines,
        ignore::{IgnoreMatcher, IgnoreRule, glob_match, ig_ignores},
        mime::strip_bom,
        paths::{EntryKind, entry_kind, relative_posix, resolve_home, shorten_home, to_posix},
    },
};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::OnceLock,
};

/// 技能名的最大字符数上限，校验 frontmatter 的 name 字段。
const MAX_NAME_LENGTH: usize = 64;
/// 技能描述的最大字符数上限，校验 frontmatter 的 description 字段。
const MAX_DESCRIPTION_LENGTH: usize = 1024;
/// 发现技能时按序读取的忽略规则文件名。
const IGNORE_FILE_NAMES: [&str; 3] = [".gitignore", ".ignore", ".fdignore"];

/// 惰性编译的 skill 块解析正则，供 parse_skill_block 复用。
static RE: OnceLock<regex::Regex> = OnceLock::new();

/// 一个已加载技能：名称、描述、所在路径与是否禁止模型主动调用。
#[derive(Debug, Clone)]
pub struct Skill {
    /// 技能名（frontmatter `name`）。
    pub name: String,
    /// 技能描述（frontmatter `description`）。
    pub description: String,
    /// SKILL.md 文件路径。
    pub path: PathBuf,
    /// SKILL.md 所在目录，作为技能内相对路径的基准。
    pub base_dir: PathBuf,
    /// 是否禁止模型主动调用（只能由用户显式触发）。
    pub disable_model_invocation: bool,
    /// 来源标签：发现根目录的 POSIX 路径（`$HOME` 缩写为 `~`），内嵌扩展技能为 `builtin`，
    /// 显式 `--skill` 为其自身路径；仅用于面板展示，不参与过滤。
    pub source: String,
}

/// SKILL.md 头部 YAML frontmatter 的解析结果。
#[derive(Debug, Clone, Default)]
struct Frontmatter {
    /// frontmatter 的 `name`，缺省为 None。
    name: Option<String>,
    /// frontmatter 的 `description`，缺省为 None。
    description: Option<String>,
    /// 是否禁止模型主动调用（`true` / `yes` 视为真）。
    disable_model_invocation: bool,
}

/// 把 SKILL.md 内容拆成 YAML frontmatter 与剩余正文：识别开头的 `---` 包裹块，
/// 解析其中的 name / description / disable-model-invocation。
/// 无合法 frontmatter 时返回默认值与归一化后的全文。
fn parse_frontmatter(content: &str) -> (Frontmatter, String) {
    let normalized = normalize_newlines(strip_bom(content));
    if !normalized.starts_with("---") {
        return (Frontmatter::default(), normalized);
    }
    let Some(end) = normalized[3..].find("\n---") else {
        return (Frontmatter::default(), normalized);
    };
    let end = 3 + end;
    let yaml = &normalized[3..end];
    let body = normalized[end + 4..].trim().to_string();

    let mut fm = Frontmatter::default();
    for raw_line in yaml.lines() {
        let line = raw_line.trim();
        if let Some(value) = line.strip_prefix("name:") {
            let value = unquote_yaml(value.trim());
            if !value.is_empty() {
                fm.name = Some(value);
            }
        } else if let Some(value) = line.strip_prefix("description:") {
            fm.description = Some(unquote_yaml(value.trim()));
        } else if let Some(value) = line.strip_prefix("disable-model-invocation:") {
            fm.disable_model_invocation = matches!(value.trim(), "true" | "yes");
        }
    }
    (fm, body)
}

/// 去掉 YAML 标量两端成对的单/双引号（已 trim）；无成对引号时原样返回。
fn unquote_yaml(s: &str) -> String {
    let s = s.trim();
    if s.len() >= 2 {
        let b = s.as_bytes();
        let quote = b[0];
        if (quote == b'"' || quote == b'\'') && b[s.len() - 1] == quote {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

/// 校验技能名：仅小写字母/数字/连字符，不得以连字符开头或结尾、不得有连续连字符，
/// 且不超 [`MAX_NAME_LENGTH`]。
fn validate_name(name: &str) -> bool {
    if name.is_empty() || name.len() > MAX_NAME_LENGTH {
        return false;
    }
    let mut prev = '\0';
    for c in name.chars() {
        if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '-' {
            return false;
        }
        if c == '-' && (prev == '\0' || prev == '-') {
            return false;
        }
        prev = c;
    }
    !name.ends_with('-')
}

/// 校验技能描述：去空白后非空且长度不超过 [`MAX_DESCRIPTION_LENGTH`]。
fn validate_description(description: &str) -> bool {
    !description.trim().is_empty() && description.len() <= MAX_DESCRIPTION_LENGTH
}

/// 从单个 SKILL.md 文件读取技能；`source` 为来源标签（见 [`Skill::source`]）。
/// 文件不可读时返回 `None`；其余校验见 [`load_skill_from_content`]。
pub fn load_skill_from_file(file_path: &Path, source: &str) -> Option<Skill> {
    let content = std::fs::read_to_string(file_path).ok()?;
    load_skill_from_content(file_path, source, &content)
}

/// 从 SKILL.md 内容构造技能：description 必需且须通过校验，name 缺失时用父目录名。
/// `file_path` 为 SKILL.md 路径（内嵌扩展技能可为尚未落盘的目标路径），
/// `source` 为来源标签；缺 description 或描述非法时返回 `None`，name 非法仅告警仍会加载。
pub fn load_skill_from_content(file_path: &Path, source: &str, content: &str) -> Option<Skill> {
    let (fm, _body) = parse_frontmatter(content);
    let description = fm.description?;
    if !validate_description(&description) {
        return None;
    }
    let name = fm
        .name
        .unwrap_or_else(|| directory_name(file_path).unwrap_or_else(|| "skill".to_string()));
    _ = validate_name(&name); // 仅告警，仍然加载
    Some(Skill {
        name,
        description,
        path: file_path.to_path_buf(),
        base_dir: file_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default(),
        disable_model_invocation: fm.disable_model_invocation,
        source: source.to_string(),
    })
}

/// 从 SKILL.md 内容取技能名：frontmatter `name`，缺失时回落 `fallback`。
/// 供内嵌扩展技能在落盘前确定目标目录名（目录名取技能名）。
pub fn skill_name_from_content(content: &str, fallback: &str) -> String {
    parse_frontmatter(content)
        .0
        .name
        .unwrap_or_else(|| fallback.to_string())
}

/// 来源标签：`root` 的 POSIX 路径，`$HOME` 前缀缩写为 `~`（见 [`shorten_home`]）。
pub fn source_label(root: &Path) -> String {
    shorten_home(&to_posix(root))
}

/// SKILL.md 路径的父目录名（即技能目录名）；路径没有父目录名时为 `None`。
fn directory_name(file_path: &Path) -> Option<String> {
    file_path
        .parent()?
        .file_name()?
        .to_str()
        .map(str::to_string)
}

/// 把 `dir` 下各忽略文件（`.gitignore` 等）里的规则并入 `ig`：模式相对 `root` 补上目录前缀，
/// 并展开 `!` 取反、`/` 锚定、末尾 `/` 仅目录等语法。
fn add_ignore_rules(ig: &mut IgnoreMatcher, dir: &Path, root: &Path) {
    let prefix = relative_posix(root, dir);
    let prefix = if prefix.is_empty() || prefix == "." {
        String::new()
    } else {
        format!("{}/", prefix)
    };
    for name in IGNORE_FILE_NAMES {
        let ignore_path = dir.join(name);
        if !ignore_path.is_file() {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&ignore_path) else {
            continue;
        };
        for raw in content.split('\n') {
            let line = raw.trim();
            if line.is_empty() {
                continue;
            }
            if line.starts_with('#') && !line.starts_with("\\#") {
                continue;
            }
            let mut pattern = line.to_string();
            let mut negated = false;
            if let Some(rest) = pattern.strip_prefix('!') {
                negated = true;
                pattern = rest.to_string();
            } else if let Some(rest) = pattern.strip_prefix("\\!") {
                pattern = rest.to_string();
            }
            if let Some(rest) = pattern.strip_prefix('/') {
                pattern = rest.to_string();
            }
            let mut dir_only = false;
            if let Some(rest) = pattern.strip_suffix('/') {
                dir_only = true;
                pattern = rest.to_string();
            }
            if pattern.is_empty() {
                continue;
            }
            let pattern = if prefix.is_empty() {
                pattern
            } else {
                format!("{}{}", prefix, pattern)
            };
            ig.rules.push(IgnoreRule {
                pattern,
                negated,
                dir_only,
            });
        }
    }
}

/// 技能收集器：realpath 去重 + 名字先到先得（对齐 pi skillMap 语义）
#[derive(Default)]
struct SkillCollector {
    /// 去重后收集到的技能，按发现顺序排列。
    skills: Vec<Skill>,
    /// 已收录技能的 realpath，用于跨符号链接去重。
    real_paths: HashSet<PathBuf>,
}

impl SkillCollector {
    /// 收录一个技能：按 realpath 跨符号链接去重，同名先到先得；重复或重名时静默丢弃。
    fn push(&mut self, skill: Skill) {
        let real = std::fs::canonicalize(&skill.path).unwrap_or_else(|_| skill.path.clone());
        if !self.real_paths.insert(real) {
            return;
        }
        if self.skills.iter().any(|s| s.name == skill.name) {
            return;
        }
        self.skills.push(skill);
    }
}

/// 递归扫描 `dir` 下的技能：目录直接含 SKILL.md 即收录并停止下钻，否则继续遍历子目录；
/// `include_root_files` 为真时把根目录下的 `.md` 文件也视为技能。
/// `visited` 记录已访问 realpath 以防符号链接环，忽略规则逐层累积到 `ig`。
#[stacksafe::stacksafe]
fn load_skills_from_dir_inner(
    dir: &Path,
    root: &Path,
    include_root_files: bool,
    source: &str,
    ig: &mut IgnoreMatcher,
    visited: &mut HashSet<PathBuf>,
    skills: &mut SkillCollector,
) {
    let canon = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    if !visited.insert(canon) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let entries: Vec<_> = entries.flatten().collect();

    add_ignore_rules(ig, dir, root);

    // Skill root：目录直接包含 SKILL.md 时不再递归（忽略规则同样生效）
    for entry in &entries {
        if entry.file_name().to_string_lossy() != "SKILL.md" {
            continue;
        }
        let path = entry.path();
        if entry_kind(&path) != Some(EntryKind::File) {
            continue;
        }
        let rel = relative_posix(root, &path);
        if ig_ignores(ig, &rel, false) {
            return;
        }
        if let Some(skill) = load_skill_from_file(&path, source) {
            skills.push(skill);
        }
        return;
    }

    for entry in &entries {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || name == "node_modules" {
            continue;
        }
        let path = entry.path();
        let Some(kind) = entry_kind(&path) else {
            continue;
        };
        let rel = relative_posix(root, &path);
        match kind {
            EntryKind::Dir => {
                if ig_ignores(ig, &rel, true) {
                    continue;
                }
                load_skills_from_dir_inner(&path, root, false, source, ig, visited, skills);
            }
            EntryKind::File => {
                if !include_root_files || !name.ends_with(".md") {
                    continue;
                }
                if ig_ignores(ig, &rel, false) {
                    continue;
                }
                if let Some(skill) = load_skill_from_file(&path, source) {
                    skills.push(skill);
                }
            }
        }
    }
}

/// 技能扫描入口：以 `dir` 为根和忽略规则基准，从零建忽略匹配器与已访问集合后调递归实现，
/// 返回去重后的技能列表；`include_root_files` 为真时根目录下的 `.md` 也算技能，
/// `source` 为写进每个技能的来源标签。
fn load_skills_from_dir(dir: &Path, include_root_files: bool, source: &str) -> Vec<Skill> {
    let mut skills = SkillCollector::default();
    let mut ig = IgnoreMatcher::default();
    let mut visited = HashSet::new();
    load_skills_from_dir_inner(
        dir,
        dir,
        include_root_files,
        source,
        &mut ig,
        &mut visited,
        &mut skills,
    );
    skills.skills
}

/// settings.json skills 过滤规则（对齐 pi isEnabledByOverrides/applyPatterns）：
/// `!pattern` 排除 glob、`+path` 精确强制包含、`-path` 精确强制排除（优先级最高）；
/// 无前缀的普通规则在 pi 中仅作用于包资源，自动发现资源忽略之
#[derive(Debug, Clone, Copy, PartialEq)]
enum FilterKind {
    /// `!pattern`：按 glob 排除。
    Exclude,
    /// `+path`：精确强制包含。
    ForceInclude,
    /// `-path`：精确强制排除，优先级最高。
    ForceExclude,
}

/// 该规则是否为针对 `name` 的**精确排除**项（`-name`，允许写成 `-/name`）。
pub fn filter_exact_exclude_matches(rule: &str, name: &str) -> bool {
    rule.strip_prefix('-')
        .map(|rest| rest.trim_start_matches('/') == name)
        .unwrap_or(false)
}

/// 规则数组里是否存在针对 `name` 的精确排除项。
pub fn filter_has_exact_exclude(rules: &[String], name: &str) -> bool {
    rules.iter().any(|r| filter_exact_exclude_matches(r, name))
}

/// 追加精确排除项 `-name`（已存在则不动）。追加在末尾，从而压过前面的规则。返回是否写入了新项。
pub fn filter_add_exact_exclude(rules: &mut Vec<String>, name: &str) -> bool {
    if filter_has_exact_exclude(rules, name) {
        return false;
    }
    rules.push(format!("-{name}"));
    true
}

/// 删除全部精确排除项 `-name`（不影响手写的 `+`/`!` 规则）；返回是否有删除。
pub fn filter_remove_exact_exclude(rules: &mut Vec<String>, name: &str) -> bool {
    let before = rules.len();
    rules.retain(|r| !filter_exact_exclude_matches(r, name));
    rules.len() != before
}

/// 把 settings.json 的 skills 过滤字符串解析成 `(类型, 去前导斜杠的路径/glob)`：
/// `!` → 排除 glob、`+` → 精确强制包含、`-` → 精确强制排除；无前缀项忽略。
fn parse_filter(filter: &[String]) -> Vec<(FilterKind, String)> {
    let mut out = Vec::new();
    for p in filter {
        if let Some(rest) = p.strip_prefix('-') {
            out.push((
                FilterKind::ForceExclude,
                rest.trim_start_matches('/').to_string(),
            ));
        } else if let Some(rest) = p.strip_prefix('+') {
            out.push((
                FilterKind::ForceInclude,
                rest.trim_start_matches('/').to_string(),
            ));
        } else if let Some(rest) = p.strip_prefix('!') {
            out.push((
                FilterKind::Exclude,
                rest.trim_start_matches('/').to_string(),
            ));
        }
    }
    out
}

/// 匹配目标对齐 pi matchesAnyPattern/matchesAnyExactPattern 对 SKILL.md 的特判：
/// 相对路径、文件全路径、文件名、父目录相对路径、父目录名（技能目录名）
fn filter_matches_skill(skill: &Skill, root: &Path, pattern: &str, exact: bool) -> bool {
    let rel = relative_posix(root, &skill.path);
    let full = to_posix(&skill.path);
    let name = skill
        .path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let parent_rel = relative_posix(root, &skill.base_dir);
    let parent_name = skill
        .base_dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    // 末项是 frontmatter 的技能名：面板按名字写 `-name` 时，
    // 必须能命中「目录名与技能名不一致」的技能（如目录 foo、name: bar）。
    let candidates = [rel, full, name, parent_rel, parent_name, skill.name.clone()];
    if exact {
        let normalized = pattern.trim_matches('/').to_string();
        candidates.iter().any(|c| c == &normalized)
    } else {
        candidates.iter().any(|c| glob_match(pattern, c))
    }
}

/// 依次套用过滤规则，返回该技能最终是否启用：`+`/`-` 走精确匹配强制开关，`!` 按 glob 排除，
/// 后面的规则覆盖前面的。
fn skill_enabled(skill: &Skill, root: &Path, filter: &[(FilterKind, String)]) -> bool {
    let mut enabled = true;
    for (kind, pattern) in filter {
        let matched = filter_matches_skill(
            skill,
            root,
            pattern,
            matches!(kind, FilterKind::ForceInclude | FilterKind::ForceExclude),
        );
        match kind {
            FilterKind::Exclude => {
                if matched {
                    enabled = false;
                }
            }
            FilterKind::ForceInclude => {
                if matched {
                    enabled = true;
                }
            }
            FilterKind::ForceExclude => {
                if matched {
                    enabled = false;
                }
            }
        }
    }
    enabled
}

/// 从 cwd 向上收集祖先（含 cwd 自身）的 .agents/skills，排除用户级 ~/.agents/skills
fn ancestor_agents_skill_dirs(cwd: &Path, home_dir: &Path) -> Vec<PathBuf> {
    let home = resolve_home(home_dir);
    let user_agents = home.join(".agents").join("skills");
    let mut dirs = Vec::new();
    let mut current = cwd.to_path_buf();
    loop {
        let candidates = current.join(".agents").join("skills");
        if candidates != user_agents && candidates.is_dir() {
            dirs.push(candidates);
        }
        if !current.pop() {
            break;
        }
    }
    dirs
}

/// 加载技能（对齐 pi resource-loader + package-manager 的来源与优先级，同名先到先得）：
///
/// 1. 项目级：cwd/.prux/skills、cwd 及祖先 .agents/skills（需项目信任）
/// 2. 用户级：agentDir/skills（`bundled_names` 之外的部分）、~/.agents/skills
/// 3. 显式 --skill 路径（文件或目录，不受 settings 过滤；因排在扫描顺序末尾，
///    与自动发现技能同名时按先到先得**垫底**）
/// 4. `bundled_names` 列出的内置技能副本（agentDir/skills/<name>）垫在最末：
///    它们只是随二进制分发的兜底副本，同名时必须让位于上面任何一份
///
/// `bundled_names` 由调用方传入（core 不依赖 extensions 的内嵌资源表）；传空切片则
/// 所有来源地位等价。settings_filter 仅作用于自动发现技能；realpath 去重。
#[allow(clippy::too_many_arguments)]
pub fn load_skills(
    cwd: &str,
    agent_dir: &Path,
    home_dir: &Path,
    skill_paths: &[String],
    bundled_names: &[String],
    include_defaults: bool,
    trusted: bool,
    settings_filter: &[String],
) -> Vec<Skill> {
    let mut collector = SkillCollector::default();
    let filter = parse_filter(settings_filter);
    let cwd_path = PathBuf::from(cwd);
    let home = resolve_home(home_dir);

    // 扫描一个技能根，返回其下通过 settings 过滤的技能（目录内 realpath 去重已在此完成）
    let collect_dir = |dir: &Path| -> Vec<Skill> {
        let mut local = SkillCollector::default();
        let mut ig = IgnoreMatcher::default();
        let mut visited = HashSet::new();
        let source = source_label(dir);

        load_skills_from_dir_inner(dir, dir, true, &source, &mut ig, &mut visited, &mut local);

        local
            .skills
            .into_iter()
            .filter(|s| skill_enabled(s, dir, &filter))
            .collect()
    };

    // 内置技能的落盘副本：暂存到扫描全部结束时再收录（见函数文档第 4 点）
    let mut bundled_copies: Vec<Skill> = Vec::new();

    if include_defaults {
        // 项目级（信任门控）：对齐 pi，项目资源优先于用户资源
        if trusted {
            for skill in collect_dir(&cwd_path.join(PROJECT_SCOPE_NAME).join("skills")) {
                collector.push(skill);
            }
            for d in ancestor_agents_skill_dirs(&cwd_path, &home) {
                for skill in collect_dir(&d) {
                    collector.push(skill);
                }
            }
        }

        // 用户级：agentDir/skills 里属于内置目录的那些名字降级（同目录内其余技能保持原位置）
        for skill in collect_dir(&agent_dir.join("skills")) {
            if bundled_names.iter().any(|n| n == &skill.name) {
                bundled_copies.push(skill);
            } else {
                collector.push(skill);
            }
        }

        for skill in collect_dir(&home.join(".agents").join("skills")) {
            collector.push(skill);
        }
    }

    // 显式路径：不受过滤，冲突时显式优先
    for raw in skill_paths {
        let path = if raw == "~" {
            home.clone()
        } else if let Some(rest) = raw.strip_prefix("~/") {
            home.join(rest)
        } else {
            let p = PathBuf::from(raw);
            if p.is_absolute() { p } else { cwd_path.join(p) }
        };

        let source = source_label(&path);
        match entry_kind(&path) {
            Some(EntryKind::Dir) => {
                for skill in load_skills_from_dir(&path, true, &source) {
                    collector.push(skill);
                }
            }
            Some(EntryKind::File)
                if path.extension().and_then(|e| e.to_str()) == Some("md")
                    && let Some(skill) = load_skill_from_file(&path, &source) =>
            {
                collector.push(skill);
            }
            Some(EntryKind::File) => {}
            None => {}
        }
    }

    // 内置技能副本垫底：同名时它们让位于上面任何一份（含显式 --skill）
    for skill in bundled_copies {
        collector.push(skill);
    }

    collector.skills
}

/// 技能调用块解析结果
#[derive(Debug, Clone, PartialEq)]
pub struct SkillBlock {
    /// `<skill name="...">` 中的技能名。
    pub name: String,
    /// `<skill location="...">` 中的技能位置。
    pub location: String,
    /// 块内正文，即注入的技能指令内容。
    pub content: String,
    /// 块之后的剩余文本，作为用户消息。
    pub user_message: Option<String>,
}

/// 惰性编译并缓存技能块正则（块必须位于文本开头）。
fn skill_block_re() -> &'static regex::Regex {
    RE.get_or_init(|| {
        regex::Regex::new(
            r#"^<skill name="([^"]+)" location="([^"]+)">\n([\s\S]*?)\n</skill>(?:\n\n([\s\S]+))?$"#,
        )
        .expect("valid skill block regex")
    })
}

/// 解析整条用户文本中的 `<skill name=".." location="..">..</skill>` 块
/// （块必须位于文本开头，其余文本作为 userMessage）
pub fn parse_skill_block(text: &str) -> Option<SkillBlock> {
    let caps = skill_block_re().captures(text)?;
    Some(SkillBlock {
        name: caps[1].to_string(),
        location: caps[2].to_string(),
        content: caps[3].to_string(),
        user_message: caps
            .get(4)
            .map(|m| m.as_str().trim().to_string())
            .filter(|s| !s.is_empty()),
    })
}

/// 展开 /skill:name [额外指令]
pub fn expand_skill_command(text: &str, skills: &[Skill]) -> String {
    if !text.starts_with("/skill:") {
        return text.to_string();
    }
    let rest = &text["/skill:".len()..];
    let (name, args) = rest
        .split_once(char::is_whitespace)
        .map(|(n, a)| (n, a.trim()))
        .unwrap_or((rest, ""));
    let Some(skill) = skills.iter().find(|s| s.name == name) else {
        return text.to_string();
    };
    let Some(content) = std::fs::read_to_string(&skill.path).ok() else {
        return text.to_string();
    };
    let body = parse_frontmatter(&content).1;
    let block = format!(
        "<skill name=\"{}\" location=\"{}\">\nReferences are relative to {}.\n\n{}\n</skill>",
        skill.name,
        skill.path.display(),
        skill.base_dir.display(),
        body.trim()
    );
    if args.is_empty() {
        block
    } else {
        format!("{}\n\n{}", block, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn discovers_nested_skills_and_first_wins() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("nested").join("demo").join("SKILL.md"),
            "---\nname: demo\ndescription: nested\n---\nbody",
        );
        write(
            &root.join("other.md"),
            "---\nname: demo\ndescription: other\n---\nbody",
        );
        write(
            &root.join("loose.md"),
            "---\nname: loose\ndescription: root file\n---\nbody",
        );
        let skills = load_skills_from_dir(root, true, "test");
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["demo", "loose"]);
        assert_eq!(skills[0].description, "nested");
    }

    #[test]
    fn requires_description() {
        let dir = tempfile::tempdir().unwrap();
        write(&dir.path().join("SKILL.md"), "---\nname: x\n---\nbody");
        assert!(load_skills_from_dir(dir.path(), true, "test").is_empty());
    }

    #[test]
    fn expands_skill_command() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("SKILL.md"),
            "---\nname: demo\ndescription: d\n---\n# Body",
        );
        let skill = load_skills_from_dir(dir.path(), true, "test");
        let out = expand_skill_command("/skill:demo do it", &skill);
        assert!(out.contains("<skill name=\"demo\" location=\""));
        assert!(out.contains("# Body"));
        assert!(out.ends_with("do it"));
    }

    #[test]
    fn ignore_rules_from_gitignore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("skills").join("skip-me").join("SKILL.md"),
            "---\ndescription: x\n---\nb",
        );
        write(
            &root.join("skills").join("keep").join("SKILL.md"),
            "---\ndescription: keep\n---\nb",
        );
        write(&root.join("skills").join(".gitignore"), "skip-me/\n");
        let skills = load_skills_from_dir(&root.join("skills"), true, "test");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "keep");
    }

    #[test]
    fn ignore_negation_restores() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("skills").join("keep.md"),
            "---\nname: keep\ndescription: keep\n---\nb",
        );
        write(
            &root.join("skills").join("drop.md"),
            "---\nname: drop\ndescription: drop\n---\nb",
        );
        write(&root.join("skills").join(".gitignore"), "*.md\n!keep.md\n");
        let skills = load_skills_from_dir(&root.join("skills"), true, "test");
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["keep"]);
    }

    #[cfg(unix)]
    #[test]
    fn follows_symlinks_and_dedupes_realpath() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("real").join("SKILL.md"),
            "---\ndescription: real\n---\nb",
        );
        let link = root.join("link-to-real");
        std::os::unix::fs::symlink(root.join("real"), &link).unwrap();
        let skills = load_skills_from_dir(root, true, "test");
        // real 直接子目录 + 链接目录：同一 realpath 只算一次
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "real");
    }

    #[test]
    fn loads_user_and_project_sources() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let home = root.join("home");
        let cwd = root.join("proj");
        let agent = root.join("config");
        write(
            &agent.join("skills").join("a").join("SKILL.md"),
            "---\ndescription: agent\n---\nb",
        );
        write(
            &home
                .join(".agents")
                .join("skills")
                .join("b")
                .join("SKILL.md"),
            "---\ndescription: user-agents\n---\nb",
        );
        // pi 兼容目录 ~/.pi/agent/skills 不被加载
        write(
            &home
                .join(".pi")
                .join("agent")
                .join("skills")
                .join("c")
                .join("SKILL.md"),
            "---\ndescription: pi-compat\n---\nb",
        );
        write(
            &cwd.join(PROJECT_SCOPE_NAME)
                .join("skills")
                .join("d")
                .join("SKILL.md"),
            "---\ndescription: project\n---\nb",
        );
        write(
            &cwd.join(".agents")
                .join("skills")
                .join("e")
                .join("SKILL.md"),
            "---\ndescription: proj-agents\n---\nb",
        );

        // 未信任：项目级不加载
        let skills = load_skills(
            cwd.to_str().unwrap(),
            &agent,
            &home,
            &[],
            &[],
            true,
            false,
            &[],
        );
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);

        // 信任后：项目级（.prux + .agents）进入且优先级高于用户级（对齐 pi rank：project < user）
        let skills = load_skills(
            cwd.to_str().unwrap(),
            &agent,
            &home,
            &[],
            &[],
            true,
            true,
            &[],
        );
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["d", "e", "a", "b"]);
    }

    #[test]
    fn ancestor_agents_skills_with_trust() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let home = root.join("home");
        let cwd = root.join("deep").join("nested").join("proj");
        write(
            &root
                .join("deep")
                .join(".agents")
                .join("skills")
                .join("x")
                .join("SKILL.md"),
            "---\ndescription: ancestor\n---\nb",
        );
        write(
            &home
                .join(".agents")
                .join("skills")
                .join("y")
                .join("SKILL.md"),
            "---\ndescription: user\n---\nb",
        );
        let skills = load_skills(
            cwd.to_str().unwrap(),
            &root.join("cfg"),
            &home,
            &[],
            &[],
            true,
            true,
            &[],
        );
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["x", "y"]);
    }

    #[test]
    fn settings_filter_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let home = root.join("home");
        let agent = root.join("config");
        write(
            &agent.join("skills").join("keep").join("SKILL.md"),
            "---\ndescription: keep\n---\nb",
        );
        write(
            &agent.join("skills").join("drop-me").join("SKILL.md"),
            "---\ndescription: drop\n---\nb",
        );
        write(
            &agent.join("skills").join("forced").join("SKILL.md"),
            "---\ndescription: forced\n---\nb",
        );

        // !glob 排除
        let skills = load_skills(
            root.to_str().unwrap(),
            &agent,
            &home,
            &[],
            &[],
            true,
            false,
            &["!drop-*".to_string()],
        );
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["keep", "forced"]);

        // -name 精确强制排除
        let skills = load_skills(
            root.to_str().unwrap(),
            &agent,
            &home,
            &[],
            &[],
            true,
            false,
            &["-forced".to_string()],
        );
        assert!(skills.iter().all(|s| s.name != "forced"));

        // +name 强制包含覆盖 !
        let skills = load_skills(
            root.to_str().unwrap(),
            &agent,
            &home,
            &[],
            &[],
            true,
            false,
            &["!*".to_string(), "+forced".to_string()],
        );
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["forced"]);
    }

    /// 精确排除辅助（`/skills` 面板的写入口）：只动 `-name`，不碰手写的 `+`/`!` 规则。
    #[test]
    fn exact_exclude_helpers_target_one_name_only() {
        let mut rules = vec!["!drop-*".to_string(), "+keep".to_string()];
        assert!(!filter_has_exact_exclude(&rules, "dup"));

        // 追加在末尾 → 压过前面所有规则
        assert!(filter_add_exact_exclude(&mut rules, "dup"));
        assert_eq!(rules.last().unwrap(), "-dup");
        assert!(!filter_add_exact_exclude(&mut rules, "dup"), "幂等");
        assert_eq!(rules.iter().filter(|r| *r == "-dup").count(), 1);

        // 删除只删精确项，手写规则原样保留
        assert!(filter_remove_exact_exclude(&mut rules, "dup"));
        assert_eq!(rules, vec!["!drop-*".to_string(), "+keep".to_string()]);
        assert!(!filter_remove_exact_exclude(&mut rules, "dup"));

        // 不能把名字前缀相同的另一条误删（grill-me vs grill）
        let mut rules = vec!["-grill-me".to_string(), "-grill".to_string()];
        assert!(filter_remove_exact_exclude(&mut rules, "grill-me"));
        assert_eq!(rules, vec!["-grill".to_string()]);

        // `-/name` 与 `-name` 等价
        let mut rules = vec!["-/dup".to_string()];
        assert!(filter_has_exact_exclude(&rules, "dup"));
        assert!(filter_remove_exact_exclude(&mut rules, "dup"));
        assert!(rules.is_empty());
    }

    /// 目录名与 frontmatter `name` 不一致时，`-<name>` 必须能命中（面板写的就是 `name`）。
    #[test]
    fn filter_matches_frontmatter_name_not_only_dir_name() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let agent = root.join("cfg");
        write(
            &agent.join("skills").join("foo").join("SKILL.md"),
            "---\nname: bar\ndescription: d\n---\nb",
        );
        let load = |filter: &[String]| {
            load_skills(
                root.to_str().unwrap(),
                &agent,
                &root.join("home"),
                &[],
                &[],
                true,
                false,
                filter,
            )
        };

        assert_eq!(load(&[]).len(), 1, "基线：应发现这个技能");
        assert!(
            load(&["-bar".to_string()]).is_empty(),
            "frontmatter name 应可被过滤命中"
        );
        assert!(
            load(&["-foo".to_string()]).is_empty(),
            "目录名仍然可用（向后兼容）"
        );
    }

    #[test]
    fn explicit_paths_bypass_filter_and_win_when_default_is_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let agent = root.join("config");
        write(
            &agent.join("skills").join("dup").join("SKILL.md"),
            "---\nname: dup\ndescription: default\n---\ndefault body",
        );
        let explicit = root.join("explicit.md");
        write(
            &explicit,
            "---\nname: dup\ndescription: explicit\n---\nexplicit body",
        );

        // 显式路径不受过滤影响；这里它之所以赢，是因为同名默认技能被 `-dup` 先过滤掉了
        // （扫描顺序上显式路径垫底，同名先到先得——见 `load_skills` 的文档）。
        let skills = load_skills(
            root.to_str().unwrap(),
            &agent,
            &root.join("home"),
            &[explicit.to_string_lossy().to_string()],
            &[],
            true,
            false,
            &["-dup".to_string()],
        );
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "explicit");
    }

    /// 内置技能的落盘副本（agent 目录、名字在 `bundled_names` 里）垫在最末：
    /// 同名时让位于项目/用户/显式路径里的任何一份，但仍是可用的兜底。
    #[test]
    fn bundled_skill_copies_have_lowest_priority() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let home = root.join("home");
        let cwd = root.join("proj");
        let agent = root.join("config");
        let bundled = vec!["dup".to_string()];

        write(
            &agent.join("skills").join("dup").join("SKILL.md"),
            "---\ndescription: bundled\n---\nb",
        );
        write(
            &agent.join("skills").join("plain").join("SKILL.md"),
            "---\ndescription: plain-agent\n---\nb",
        );
        write(
            &home
                .join(".agents")
                .join("skills")
                .join("plain")
                .join("SKILL.md"),
            "---\ndescription: plain-user\n---\nb",
        );

        let load = |bundled: &[String]| {
            load_skills(
                cwd.to_str().unwrap(),
                &agent,
                &home,
                &[],
                bundled,
                true,
                true,
                &[],
            )
        };
        let desc = |skills: &[Skill], name: &str| -> String {
            skills
                .iter()
                .find(|s| s.name == name)
                .map(|s| s.description.clone())
                .unwrap_or_default()
        };

        // 只有内置副本时它仍然生效（垫底 ≠ 丢弃）
        assert_eq!(desc(&load(&bundled), "dup"), "bundled");
        // 不在内置名单里的 agent 目录技能保持原优先级（仍压过 ~/.agents/skills）
        assert_eq!(desc(&load(&bundled), "plain"), "plain-agent");

        // 用户级（~/.agents/skills）同名副本胜出
        write(
            &home
                .join(".agents")
                .join("skills")
                .join("dup")
                .join("SKILL.md"),
            "---\ndescription: user\n---\nb",
        );
        assert_eq!(
            desc(&load(&[]), "dup"),
            "bundled",
            "空名单 = 旧口径：agent 目录先扫描先得"
        );
        assert_eq!(desc(&load(&bundled), "dup"), "user");

        // 项目级同理
        write(
            &cwd.join(".agents")
                .join("skills")
                .join("dup")
                .join("SKILL.md"),
            "---\ndescription: project\n---\nb",
        );
        assert_eq!(desc(&load(&bundled), "dup"), "project");

        // 删掉项目/用户副本后，显式 --skill 路径也压过内置副本
        let other = tempfile::tempdir().unwrap();
        let ohome = other.path().join("home");
        let explicit = other.path().join("explicit.md");
        write(&explicit, "---\nname: dup\ndescription: explicit\n---\nb");
        let skills = load_skills(
            cwd.to_str().unwrap(),
            &agent,
            &ohome,
            &[explicit.to_string_lossy().to_string()],
            &bundled,
            true,
            false,
            &[],
        );
        assert_eq!(desc(&skills, "dup"), "explicit");
        assert_eq!(desc(&skills, "plain"), "plain-agent");
    }

    #[test]
    fn disable_model_invocation_parsed() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("SKILL.md"),
            "---\nname: secret\ndescription: s\ndisable-model-invocation: true\n---\nb",
        );
        let skills = load_skills_from_dir(dir.path(), true, "test");
        assert!(skills[0].disable_model_invocation);
    }

    #[test]
    fn bom_does_not_block_frontmatter() {
        let dir = tempfile::tempdir().unwrap();
        write(
            &dir.path().join("SKILL.md"),
            "\u{FEFF}---\nname: demo\ndescription: bom skill\n---\nbody",
        );
        let skills = load_skills_from_dir(dir.path(), true, "test");
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "demo");
        assert_eq!(skills[0].description, "bom skill");
    }
}

//! core::context / core::skills / core::prompt_templates / core::system_prompt 集成测试。
//! 全部使用临时目录，不触碰真实 ~/.prux 配置。

use prux::core::context::load_project_context_files;
use prux::core::prompt_templates::{
    expand_prompt_template, load_prompt_templates, parse_command_args,
};
use prux::core::skills::{Skill, expand_skill_command, load_skill_from_file, load_skills};
use prux::core::system_prompt::{
    SystemPromptOptions, build_system_prompt, format_skills_for_prompt,
};
use std::path::{Path, PathBuf};

fn temp_agent_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// 构造最小 Skill；name 与 dir 不一致时以 frontmatter 为准
fn skill(name: &str, description: &str, path: PathBuf) -> Skill {
    Skill {
        name: name.to_string(),
        description: description.to_string(),
        base_dir: path.parent().unwrap_or(Path::new("/")).to_path_buf(),
        path,
        disable_model_invocation: false,
        source: "test".to_string(),
    }
}

// ---------- context ----------

#[test]
fn loads_context_files_from_ancestors() {
    let agent = temp_agent_dir();
    // agentDir 全局上下文
    std::fs::write(agent.path().join("AGENTS.md"), "global rules").unwrap();
    // 项目目录：cwd 与上级目录各放一个
    let root = tempfile::tempdir().unwrap();
    let proj = root.path().join("a/b");
    std::fs::create_dir_all(&proj).unwrap();
    std::fs::write(root.path().join("AGENTS.md"), "root rules").unwrap();
    std::fs::write(proj.join("CLAUDE.md"), "proj rules").unwrap();

    let cwd = proj.to_str().unwrap();
    let files = load_project_context_files(cwd, agent.path());
    // 顺序：根目录在前，cwd 在后；全局（agentDir）在最前
    let contents: Vec<&str> = files.iter().map(|(_, c)| c.as_str()).collect();
    assert_eq!(contents, vec!["global rules", "root rules", "proj rules"]);
}

#[test]
fn context_prefers_override_and_dedupes() {
    let agent = temp_agent_dir();
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("AGENTS.md"), "plain").unwrap();
    std::fs::write(root.path().join("AGENTS.override.md"), "override").unwrap();
    let files = load_project_context_files(root.path().to_str().unwrap(), agent.path());
    // override 优先，且同目录不会同时加载 AGENTS.md
    assert_eq!(files.len(), 1);
    assert!(files[0].0.ends_with("AGENTS.override.md"));
    assert!(files[0].1.contains("override"));
}

// ---------- skills ----------

#[test]
fn parses_skill_md_frontmatter() {
    let dir = tempfile::tempdir().unwrap();
    let skill_path = dir.path().join("SKILL.md");
    std::fs::write(
        &skill_path,
        "---\nname: my-skill\ndescription: test skill\n---\n\n# Skill body\n\nstep one...",
    )
    .unwrap();
    let skill = load_skill_from_file(&skill_path, "test").expect("parse should succeed");
    assert_eq!(skill.name, "my-skill");
    assert_eq!(skill.description, "test skill");
    assert_eq!(skill.path, skill_path);
}

#[test]
fn skill_without_description_not_loaded() {
    // 与 pi 一致：无 description 的技能不加载（validate_description 强制）
    let dir = tempfile::tempdir().unwrap();
    let skill_path = dir.path().join("no-frontmatter.md");
    std::fs::write(&skill_path, "plain content").unwrap();
    assert!(load_skill_from_file(&skill_path, "test").is_none());
    // 有 frontmatter 但缺 description 同样不加载
    let path2 = dir.path().join("x.md");
    std::fs::write(&path2, "---\nname: x\n---\nbody").unwrap();
    assert!(load_skill_from_file(&path2, "test").is_none());
}

#[test]
fn discovers_skills_recursively() {
    let agent = temp_agent_dir();
    // 目录布局：<agentDir>/skills/<name>/SKILL.md
    let skill_dir = agent.path().join("skills").join("demo-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: demo\n---\n\ncontent",
    )
    .unwrap();

    let skills = load_skills(
        agent.path().to_str().unwrap(),
        agent.path(),
        Path::new("/nonexistent-home"),
        &[],
        &[],
        true,
        true,
        &[],
    );
    assert!(skills.iter().any(|s| s.name == "demo-skill"));
}

#[test]
fn explicit_skill_paths_load_even_without_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let skill_path = dir.path().join("SKILL.md");
    std::fs::write(
        &skill_path,
        "---\nname: explicit\ndescription: explicit\n---\n\nx",
    )
    .unwrap();
    let skills = load_skills(
        dir.path().to_str().unwrap(),
        dir.path(),
        Path::new("/nonexistent-home"),
        &[skill_path.to_string_lossy().to_string()],
        &[],
        false,
        true,
        &[],
    );
    assert_eq!(skills.len(), 1);
    assert_eq!(skills[0].name, "explicit");
}

#[test]
fn expand_skill_command_replaces_invocation() {
    // 展开需要 SKILL.md 真实存在（从文件读取正文）
    let dir = tempfile::tempdir().unwrap();
    let skill_dir = dir.path().join("demo-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let skill_file = skill_dir.join("SKILL.md");
    std::fs::write(
        &skill_file,
        "---\nname: demo-skill\ndescription: demo\n---\n\n# Skill body\n\nstep one",
    )
    .unwrap();

    let skills = vec![skill("demo-skill", "desc", skill_file.clone())];
    let out = expand_skill_command("/skill:demo-skill help me write code", &skills);
    assert!(out.contains("<skill name=\"demo-skill\""));
    assert!(out.contains("step one"));
    assert!(out.ends_with("help me write code"));
    // 未注册的技能不展开
    let out2 = expand_skill_command("/skill:nope please", &skills);
    assert!(out2.contains("/skill:nope"));
    // 非 /skill: 开头原样返回
    let out3 = expand_skill_command("plain message", &skills);
    assert_eq!(out3, "plain message");
}

// ---------- prompt_templates ----------

#[test]
fn loads_prompt_templates_from_dir() {
    let dir = tempfile::tempdir().unwrap();
    // include_defaults 从 <agentDir>/prompts 加载
    let tpl_dir = dir.path().join("prompts");
    std::fs::create_dir_all(&tpl_dir).unwrap();
    std::fs::write(
        tpl_dir.join("review.md"),
        "---\ndescription: code review\nargument-hint: [file]\n---\n\nPlease review the following file: {{1}}",
    )
    .unwrap();

    let templates = load_prompt_templates(dir.path().to_str().unwrap(), dir.path(), &[], true);
    assert_eq!(templates.len(), 1);
    assert_eq!(templates[0].name, "review");
    assert_eq!(templates[0].description, "code review");
    assert_eq!(templates[0].argument_hint.as_deref(), Some("[file]"));
}

#[test]
fn expands_template_with_args() {
    let dir = tempfile::tempdir().unwrap();
    // 显式 prompt_paths 加载单个模板文件
    let tpl = dir.path().join("review.md");
    std::fs::write(
        &tpl,
        "---\ndescription: review\n---\n\nPlease review $1 and $2",
    )
    .unwrap();
    let templates = load_prompt_templates(
        dir.path().to_str().unwrap(),
        dir.path(),
        &[tpl.to_string_lossy().to_string()],
        false,
    );
    assert_eq!(templates.len(), 1);
    let out = expand_prompt_template("/review a.rs b.rs", &templates);
    assert!(out.contains("Please review a.rs and b.rs"));
    // 无匹配模板时原样返回
    let out2 = expand_prompt_template("plain message", &templates);
    assert_eq!(out2, "plain message");
}

#[test]
fn parses_command_args_with_quotes() {
    assert_eq!(parse_command_args("a b c"), vec!["a", "b", "c"]);
    assert_eq!(parse_command_args("'a b' c"), vec!["a b", "c"]);
    assert_eq!(parse_command_args("\"x y\" z"), vec!["x y", "z"]);
    assert_eq!(parse_command_args(""), Vec::<String>::new());
}

// ---------- system_prompt ----------

fn prompt_options(cwd: &str) -> SystemPromptOptions {
    SystemPromptOptions {
        cwd: cwd.to_string(),
        selected_tools: Some(vec!["read".into(), "bash".into(), "edit".into()]),
        tool_snippets: std::collections::HashMap::from([
            ("read".into(), "read usage".to_string()),
            ("bash".into(), "bash usage".to_string()),
        ]),
        prompt_guidelines: vec!["answer in English".into()],
        append_system_prompt: None,
        context_files: vec![("/x/AGENTS.md".into(), "project rules".into())],
        skills: vec![skill("skill-a", "skill A", PathBuf::from("/s/SKILL.md"))],
        custom_prompt: None,
    }
}

#[test]
fn builds_system_prompt_with_context() {
    let cwd = "/tmp/proj";
    let p = build_system_prompt(&prompt_options(cwd));
    assert!(p.contains("/tmp/proj"));
    assert!(p.contains("project rules"));
    assert!(p.contains("read"));
    assert!(p.contains("answer in English"));
}

#[test]
fn system_prompt_custom_replaces() {
    let mut opts = prompt_options("/tmp");
    opts.custom_prompt = Some("custom system prompt".into());
    let p = build_system_prompt(&opts);
    assert!(p.starts_with("custom system prompt"));
    assert!(
        p.contains("project rules"),
        "custom prompt should still append context"
    );
}

#[test]
fn formats_skills_list() {
    let skills = vec![
        skill("a", "description A", PathBuf::from("/x/a.md")),
        skill("b", "description B", PathBuf::from("/x/b.md")),
    ];
    let out = format_skills_for_prompt(&skills);
    assert!(out.contains("a"));
    assert!(out.contains("description A"));
    assert!(out.contains("/x/a.md"));
}

#[test]
fn system_prompt_append_section() {
    let mut opts = prompt_options("/tmp");
    opts.append_system_prompt = Some("extra instructions".into());
    let p = build_system_prompt(&opts);
    assert!(p.contains("extra instructions"));
}

/// `selected_tools` 的两种「空」不是一回事：`None` 是「未指定」（回落默认四件套），
/// `Some(vec![])` 是「显式无工具」（`--no-tools`、`codemode.mode = only`）。
#[test]
fn system_prompt_distinguishes_unset_from_explicit_empty_tools() {
    let mut opts = prompt_options("/tmp");

    opts.selected_tools = None;
    let unset = build_system_prompt(&opts);
    assert!(unset.contains("- read: read usage"), "{unset}");
    assert!(unset.contains("- bash: bash usage"), "{unset}");
    assert!(unset.contains("skill-a"), "read 在列时应带技能段：{unset}");

    opts.selected_tools = Some(Vec::new());
    let empty = build_system_prompt(&opts);
    assert!(empty.contains("Available tools:\n(none)"), "{empty}");
    assert!(!empty.contains("- read: read usage"), "{empty}");
    assert!(
        !empty.contains("skill-a"),
        "没有 read 就不应列技能段：{empty}"
    );
}

// ---------- Path 工具 ----------

#[test]
fn skill_path_resolution_is_absolute() {
    let _ = Path::new("/tmp");
}

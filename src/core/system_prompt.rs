//! 系统提示词

use crate::{
    APP_NAME,
    core::{
        self,
        settings_manager::{DEFAULT_TOOL_NAMES, agent_dir},
        skills::Skill,
    },
    extensions::mcp,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

/// 系统提示词的构建输入
pub struct SystemPromptOptions {
    /// 当前工作目录（写入提示词尾部）
    pub cwd: String,
    /// 列进 "Available tools" 的工具名。
    ///
    /// `None` = 调用方未指定 → 回落到 [`DEFAULT_TOOL_NAMES`]；
    /// `Some(vec![])` = 显式声明「本次没有直接可调的工具」（如 `--no-tools`、
    /// `codemode.mode = only` 摘掉全部直接声明后），此时该段渲染为 `(none)`。
    /// 两种情况的区别就是「未指定」与「显式为空」——合并成一个空 `Vec` 会让前者被后者静默覆盖。
    pub selected_tools: Option<Vec<String>>,
    /// 工具名 → 一句话描述，只有在此表中的工具才会列进 "Available tools"
    pub tool_snippets: HashMap<String, String>,
    /// 用户/项目追加的行为准则
    pub prompt_guidelines: Vec<String>,
    /// 追加到提示词末尾的额外系统提示（如 SDK 注入）
    pub append_system_prompt: Option<String>,
    /// 项目上下文文件：(路径, 内容)，渲染为 `<project_instructions>` 段
    pub context_files: Vec<(String, String)>,
    /// 可供模型自动调用的技能列表
    pub skills: Vec<Skill>,
    /// 自定义系统提示词；为 `Some` 时整体替换默认提示词
    pub custom_prompt: Option<String>,
}

/// 组装完整系统提示：有 `custom_prompt` 时整体替换默认提示词，否则用默认模板，
/// 两者都会追加 `append_system_prompt`、项目上下文文件与技能列表。
pub fn build_system_prompt(options: &SystemPromptOptions) -> String {
    let prompt_cwd = options.cwd.replace('\\', "/");
    let append_section = options
        .append_system_prompt
        .as_ref()
        .map(|s| format!("\n\n{}", s))
        .unwrap_or_default();

    if let Some(custom_prompt) = &options.custom_prompt {
        return build_custom_system_prompt(
            options,
            prompt_cwd,
            append_section,
            custom_prompt.clone(),
        );
    }

    build_default_system_prompt(options, prompt_cwd, append_section)
}

/// 把可见技能渲染成 `<available_skills>` XML 段（名称 / 描述 / 路径）；无可见技能时返回空串。
// disable-model-invocation 的技能不进入系统提示（仅可通过 /skill:name 显式调用）
pub fn format_skills_for_prompt(skills: &[Skill]) -> String {
    let visible: Vec<_> = skills
        .iter()
        .filter(|s| !s.disable_model_invocation)
        .collect();
    if visible.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        "\n\nThe following skills provide specialized instructions for specific tasks.".to_string(),
        "Use the read tool to load a skill's file when the task matches its description.".to_string(),
        "When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.".to_string(),
        String::new(),
        "<available_skills>".to_string(),
    ];

    for skill in visible {
        lines.push("  <skill>".to_string());
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push(format!(
            "    <location>{}</location>",
            escape_xml(&skill.path.to_string_lossy())
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

/// 程序文档描述段（README/docs/examples 路径 + 主题映射 bullet）。
///
/// 默认系统提示词拼入它；当某已启用扩展声明 `suppress_documentation` 时整段跳过。
/// `doc-helper` 的 `/help` 注入时复用**同一个**函数，保证两处文字不漂移。
pub(crate) fn documentation_section() -> String {
    format!(
        "{APP_NAME} documentation (read only when the user asks about {APP_NAME} itself, its SDK, agents, themes, skills, or TUI):
- Main documentation: {}
- Additional docs: {}
- Examples: {} (custom tools, SDK)
- When reading {APP_NAME} docs or examples, resolve docs/... under Additional docs and examples/... under Examples, not the current working directory
- When asked about: themes (docs/themes.md), skills (docs/skills.md), subagents (docs/extensions/subagent.md), prompt templates (docs/prompt-templates.md), TUI components (docs/tui.md), keybindings (docs/keybindings.md), SDK integrations (docs/sdk.md), custom providers (docs/custom-provider.md), adding models (docs/models.md), environment variables (docs/environment-variables.md)
- When working on {APP_NAME} topics, read the docs and examples, and follow .md cross-references before implementing
- Always read {APP_NAME} .md files completely and follow links to related docs (e.g., tui.md for TUI API details)",
        readme_path(),
        docs_path(),
        examples_path()
    )
}

/// 渲染 "Available tools" 段要用哪些工具名：`None` 回落到 [`DEFAULT_TOOL_NAMES`]，
/// `Some(list)` 原样返回（空列表即「显式无工具」）。
fn prompt_tool_names(options: &SystemPromptOptions) -> Vec<String> {
    match &options.selected_tools {
        Some(names) => names.clone(),
        None => DEFAULT_TOOL_NAMES.iter().map(|s| s.to_string()).collect(),
    }
}

/// 以用户自定义提示词为基底，追加系统提示、项目上下文文件、技能段与当前工作目录。
fn build_custom_system_prompt(
    options: &SystemPromptOptions,
    prompt_cwd: String,
    append_section: String,
    custom_prompt: String,
) -> String {
    let has_read = prompt_tool_names(options).iter().any(|t| t == "read");
    let mut prompt = custom_prompt;
    prompt.push_str(&append_section);
    if !options.context_files.is_empty() {
        prompt.push_str("\n\n<project_context>\n\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for (file_path, content) in &options.context_files {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                file_path, content
            ));
        }
        prompt.push_str("</project_context>\n");
    }
    if has_read && !options.skills.is_empty() {
        prompt.push_str(&format_skills_for_prompt(&options.skills));
    }
    prompt.push_str(&format!("\nCurrent working directory: {}\n", prompt_cwd));
    prompt.push_str(&mcp_servers_section());
    prompt
}

/// 按内置模板拼默认系统提示：工具清单 + 行为准则 + 文档段 + 项目上下文 + 技能 + cwd。
///
/// 工具清单为空时回落到 read/bash/edit/write；准则会去重，并视扩展声明决定是否拼文档段。
fn build_default_system_prompt(
    options: &SystemPromptOptions,
    prompt_cwd: String,
    append_section: String,
) -> String {
    // 工具清单：`None` 回落标准默认集；`Some` 原样（空列表 = 显式无工具，渲染 `(none)`）
    let tools = prompt_tool_names(options);

    let visible_tools: Vec<&String> = tools
        .iter()
        .filter(|t| options.tool_snippets.contains_key(*t))
        .collect();

    let tools_list = if visible_tools.is_empty() {
        "(none)".to_string()
    } else {
        visible_tools
            .iter()
            .map(|t| format!("- {}: {}", t, options.tool_snippets.get(*t).unwrap()))
            .collect::<Vec<_>>()
            .join("\n")
    };

    let mut guidelines_list: Vec<String> = Vec::new();
    let mut guidelines_set = HashSet::new();

    let has_bash = tools.iter().any(|t| t == "bash");
    let has_grep = tools.iter().any(|t| t == "grep");
    let has_find = tools.iter().any(|t| t == "find");
    let has_ls = tools.iter().any(|t| t == "ls");

    if has_bash && !has_grep && !has_find && !has_ls {
        add_guideline(
            "Use bash for file operations like ls, rg, find",
            &mut guidelines_list,
            &mut guidelines_set,
        );
    }

    for guideline in &options.prompt_guidelines {
        let normalized = guideline.trim();
        if !normalized.is_empty() {
            add_guideline(normalized, &mut guidelines_list, &mut guidelines_set);
        }
    }

    add_guideline(
        "Be concise in your responses",
        &mut guidelines_list,
        &mut guidelines_set,
    );
    add_guideline(
        "Show file paths clearly when working with files",
        &mut guidelines_list,
        &mut guidelines_set,
    );

    let guidelines = guidelines_list
        .iter()
        .map(|g| format!("- {}", g))
        .collect::<Vec<_>>()
        .join("\n");

    let mut prompt = format!(
        "You are an expert coding assistant operating inside {APP_NAME}, a coding agent harness. You help users by reading files, executing commands, editing code, and writing new files.

Available tools:
{tools_list}

In addition to the tools above, you may have access to other custom tools depending on the project.

Guidelines:
{guidelines}"
    );

    // 文档描述段：默认拼入；任一已启用扩展声明 `suppress_documentation` 时整段跳过
    // （系统提示精简，帮助文档改由该扩展的按需命令提供，见 `doc-helper`）。
    if !core::extensions::documentation_suppressed() {
        prompt.push_str("\n\n");
        prompt.push_str(&documentation_section());
    }

    prompt.push_str(&append_section);

    if !options.context_files.is_empty() {
        prompt.push_str("\n\n<project_context>\n\n");
        prompt.push_str("Project-specific instructions and guidelines:\n\n");
        for (file_path, content) in &options.context_files {
            prompt.push_str(&format!(
                "<project_instructions path=\"{}\">\n{}\n</project_instructions>\n\n",
                file_path, content
            ));
        }
        prompt.push_str("</project_context>\n");
    }

    let has_read = tools.iter().any(|t| t == "read");
    if has_read && !options.skills.is_empty() {
        prompt.push_str(&format_skills_for_prompt(&options.skills));
    }

    prompt.push_str(&format!("\nCurrent working directory: {}", prompt_cwd));
    prompt.push_str(&mcp_servers_section());

    prompt
}

/// `mcp_servers` 段（前面带两个换行）；没东西可列时返回空串。
fn mcp_servers_section() -> String {
    match mcp::servers_section() {
        Some(section) => format!("\n\n{section}"),
        None => String::new(),
    }
}

/// 转义 XML 文本里的 `& < > " '`，避免技能名/描述破坏提示词结构。
fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// 追加一条行为准则；已存在（按 `guidelines_set` 判重）则忽略。
fn add_guideline(g: &str, guidelines_list: &mut Vec<String>, guidelines_set: &mut HashSet<String>) {
    if guidelines_set.contains(g) {
        return;
    }
    guidelines_set.insert(g.to_string());
    guidelines_list.push(g.to_string());
}

/// 安装/数据目录（`agent_dir`，README 与 docs/examples 所在的根）。
fn install_dir() -> PathBuf {
    agent_dir()
}

/// 安装目录下 README.md 的路径字符串。
fn readme_path() -> String {
    install_dir()
        .join("README.md")
        .to_string_lossy()
        .to_string()
}

/// 安装目录下 docs/ 的路径（`docs_path()` 与扩展描述里的文档指引共用）。
pub(crate) fn docs_dir() -> PathBuf {
    install_dir().join("docs")
}

/// 安装目录下 docs/ 的路径字符串。
fn docs_path() -> String {
    docs_dir().to_string_lossy().to_string()
}

/// 安装目录下 examples/ 的路径字符串。
fn examples_path() -> String {
    install_dir().join("examples").to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 最小系统提示选项：只带 cwd，其余段留空。
    fn options() -> SystemPromptOptions {
        SystemPromptOptions {
            cwd: "/tmp/project".to_string(),
            selected_tools: None,
            tool_snippets: Default::default(),
            prompt_guidelines: Vec::new(),
            append_system_prompt: None,
            context_files: Vec::new(),
            skills: Vec::new(),
            custom_prompt: None,
        }
    }

    /// 2.17：已启用的 MCP 服务器里，工具不直接声明的那些出现在系统提示的 `mcp_servers` 段；
    /// 没有这类服务器（或 MCP 扩展没启用）时整段不出现。
    #[test]
    fn mcp_servers_section_lists_indirect_servers() {
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _dir = crate::test_support::AgentDirGuard::temp();
        let _home = crate::test_support::HomeGuard::temp();

        // 只注册 mcp 扩展：`register_all()` 会连带注册启动横幅，干扰其它用例的全局状态
        crate::core::extensions::register_extension(crate::extensions::mcp::Mcp);
        crate::core::extensions::set_extension_enabled("mcp", true);
        let paths =
            crate::extensions::mcp::config::ConfigPaths::discover(std::path::Path::new("."));

        // 默认 exposure（direct）：不进这段
        crate::extensions::mcp::config::add_server(
            &paths,
            crate::extensions::mcp::config::ConfigScope::User,
            "direct-one",
            crate::extensions::mcp::config::ServerEntry {
                command: Some("echo".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let prompt = build_system_prompt(&options());
        assert!(!prompt.contains("<mcp_servers>"), "{prompt}");

        // codemode exposure：进这段
        crate::extensions::mcp::config::add_server(
            &paths,
            crate::extensions::mcp::config::ConfigScope::User,
            "docs-server",
            crate::extensions::mcp::config::ServerEntry {
                command: Some("echo".to_string()),
                exposure: Some(crate::extensions::mcp::config::McpExposure::Codemode),
                description: Some("Documentation lookup".to_string()),
                ..Default::default()
            },
        )
        .unwrap();
        let prompt = build_system_prompt(&options());
        assert!(
            prompt.contains("<mcp_servers>\nMCP servers whose tools are not declared to you. Call the tools of `codemode` servers from codemode scripts.\n- docs-server (codemode): Documentation lookup\n</mcp_servers>"),
            "{prompt}"
        );
        assert!(!prompt.contains("direct-one"), "{prompt}");

        // MCP 扩展禁用后整段消失
        crate::core::extensions::set_extension_enabled("mcp", false);
        let prompt = build_system_prompt(&options());
        assert!(!prompt.contains("<mcp_servers>"), "{prompt}");
    }
}

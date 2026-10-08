//! 默认三类型的提示词、子代理上下文桥接段、`Agent` 工具的模型面描述。

use super::{
    agent_types::GLOBAL_AGENT_DIR, config::ToolDescriptionMode, manager, types::AgentType,
};
use crate::{
    PROJECT_SCOPE_NAME,
    core::{
        project_trust::is_project_trusted,
        settings_manager::agent_dir,
        skills::{self, load_skills},
    },
    extensions::skills::bundled_skill_names,
};
use std::path::{Path, PathBuf};

/// `Available tools` 段里 `Agent` 工具的单行片段。
pub const AGENT_TOOL_SNIPPET: &str = "Delegate a sub-task to an isolated sub-agent and either wait for it or run it in the background";

/// `general-purpose` 的工具描述
pub const GENERAL_PURPOSE_DESCRIPTION: &str = "General-purpose agent for researching complex questions, searching for code, and executing multi-step tasks. Use it for open-ended work that benefits from an isolated context window.";

/// `Explore` 的工具描述
pub const EXPLORE_DESCRIPTION: &str = "Fast read-only search agent for locating code. Use it to find files by pattern, grep for \
     symbols or keywords, or answer \"where is X defined / which files reference Y.\" Do NOT use \
     it for code review, design-doc auditing, cross-file consistency checks, or open-ended \
     analysis. When calling, specify search breadth: \"quick\" for a single targeted lookup, \
     \"medium\" for moderate exploration, or \"very thorough\" to search across multiple \
     locations and naming conventions.";

/// `Plan` 的工具描述
pub const PLAN_DESCRIPTION: &str = "Software architect agent for designing implementation plans. Use this when you need to plan \
     the implementation strategy for a task. Returns step-by-step plans, identifies critical \
     files, and considers architectural trade-offs.";

/// `Explore` 的独立系统提示（`prompt_mode: replace`）。
pub const EXPLORE_PROMPT: &str = r#"# CRITICAL: READ-ONLY MODE - NO FILE MODIFICATIONS
You are a file search specialist. You excel at thoroughly navigating and exploring codebases.
Your role is EXCLUSIVELY to search and analyze existing code. You do NOT have access to file editing tools.

You are STRICTLY PROHIBITED from:
- Creating new files
- Modifying existing files
- Deleting files
- Moving or copying files
- Creating temporary files anywhere, including /tmp
- Using redirect operators (>, >>, |) or heredocs to write to files
- Running ANY commands that change system state

Use bash ONLY for read-only operations: ls, git status, git log, git diff, find, cat, head, tail.

# Tool Usage
- Use the find tool for file pattern matching (NOT the bash find command)
- Use the grep tool for content search (NOT bash grep/rg command)
- Use the read tool for reading files (NOT bash cat/head/tail)
- Use bash ONLY for read-only operations
- Make independent tool calls in parallel for efficiency
- Adapt search approach based on thoroughness level specified

# Output
- Use absolute file paths in all references
- Report findings as regular messages
- Do not use emojis
- Be thorough and precise"#;

/// `Plan` 的独立系统提示（`prompt_mode: replace`）。
pub const PLAN_PROMPT: &str = r#"# CRITICAL: READ-ONLY MODE - NO FILE MODIFICATIONS
You are a software architect and planning specialist.
Your role is EXCLUSIVELY to explore the codebase and design implementation plans.
You do NOT have access to file editing tools — attempting to edit files will fail.

You are STRICTLY PROHIBITED from:
- Creating new files
- Modifying existing files
- Deleting files
- Moving or copying files
- Creating temporary files anywhere, including /tmp
- Using redirect operators (>, >>, |) or heredocs to write to files
- Running ANY commands that change system state

# Planning Process
1. Understand requirements
2. Explore thoroughly (read files, find patterns, understand architecture)
3. Design solution based on your assigned perspective
4. Detail the plan with step-by-step implementation strategy

# Requirements
- Consider trade-offs and architectural decisions
- Identify dependencies and sequencing
- Anticipate potential challenges
- Follow existing patterns where appropriate

# Tool Usage
- Use the find tool for file pattern matching (NOT the bash find command)
- Use the grep tool for content search (NOT bash grep/rg command)
- Use the read tool for reading files (NOT bash cat/head/tail)
- Use bash ONLY for read-only operations

# Output Format
- Use absolute file paths
- Do not use emojis
- End your response with:

### Critical Files for Implementation
List 3-5 files most critical for implementing this plan:
- /absolute/path/to/file - [Brief reason]"#;

/// `general-purpose`（父级孪生）在 `append` 模式追加的桥接段。
///
/// 直接沿用上游措辞：工具名与之完全一致（read/edit/write/find/grep/bash）。
pub const SUB_AGENT_BRIDGE: &str = r#"<sub_agent_context>
You are operating as a sub-agent invoked to handle a specific task.
- Use the read tool instead of cat/head/tail
- Use the edit tool instead of sed/awk
- Use the write tool instead of echo/heredoc
- Use the find tool instead of bash find/ls for file search
- Use the grep tool instead of bash grep/rg for content search
- Make independent tool calls in parallel
- Use absolute file paths
- Do not use emojis
- Be concise but complete
- You have no user to ask: decide, act, and report. End with your final answer.
</sub_agent_context>"#;

/// `<active_agent>` 标签。`replace` 模式放在提示最前，`append` 模式跟在桥接段之后。
pub fn active_agent_tag(name: &str) -> String {
    let escaped: String = name
        .chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            other => other.to_string(),
        })
        .collect();
    format!("<active_agent name=\"{escaped}\"/>")
}

/// 追加到主 agent 系统提示 Guidelines 段的子代理用法要点。
pub fn agent_tool_guidelines() -> Vec<String> {
    let mut lines = vec![
        "Prefer doing short, context-light work yourself; delegate only when the sub-task benefits \
         from an isolated context window."
            .to_string(),
        "The Agent tool runs detached by default: it returns an agent id immediately and you are \
         notified when it finishes — never poll or guess its result, and never wait on it just to \
         keep the turn alive. Pass run_in_background: false when you need the result in this turn; \
         to run several agents concurrently, issue several Agent calls in the same message \
         (foreground calls in one message run in parallel)."
            .to_string(),
        "Trust but verify: a sub-agent's summary describes what it intended to do. When it writes \
         or edits code, check the actual changes before reporting the work as done."
            .to_string(),
    ];

    // `isolation` 同理：关掉时既不进 schema、也不占提示词
    if manager::config().worktree_isolation {
        lines.push(ISOLATION_GUIDELINE.to_string());
    }

    // `schedule` 只在真的可用时才提（关掉时既不进 schema、也不占提示词）
    if manager::config().schedule {
        lines.push(SCHEDULE_GUIDELINE.to_string());
    }
    lines
}

/// `isolation` 的提示词条目（自定义模板的 `{{isolationGuideline}}` 复用同一段，
/// 保证“关掉就不提”与默认描述一致）。
pub(crate) const ISOLATION_GUIDELINE: &str = "Use isolation: \"worktree\" to give the agent its own git worktree (safe parallel \
file modifications); leave it unset, or pass \"off\", for none. The worktree is \
removed when the agent finishes; if it made changes, they are committed to a branch and the branch is named in the result.";

/// `schedule` 的提示词条目（同上）。
pub(crate) const SCHEDULE_GUIDELINE: &str = "Use `schedule` only when the user explicitly asked for scheduled / recurring / \
delayed execution (e.g. \"every Monday\", \"in an hour\"). Don't auto-schedule from vague intent like \"monitor X\" \u{2014} \
run once now or ask.";

/// `Agent` 工具描述模板：`{{typeList}}` 在运行时替换为可用类型清单。
const AGENT_TOOL_DESCRIPTION: &str = r#"Launch a new agent to handle complex, multi-step tasks autonomously. Each agent type has specific capabilities and tools available to it.

Available agent types and the tools they have access to:
{{typeList}}

Custom agents can be defined in .prux/agents/<name>.md (project) or the global agents directory — they are picked up automatically. Project-level agents override global ones. Creating a .md file with the same name as a default agent overrides it.

When using the Agent tool, specify a subagent_type parameter to select which agent type to use.

## When not to use

If the target is already known, use a direct tool — read for a known path, grep/find for a specific symbol or string. Reserve this tool for open-ended questions that span the codebase, or tasks that match an available agent type.

## Usage notes

- Always include a short (3-5 word) description summarizing what the agent will do (shown in UI).
- The call detaches by default: it returns the agent id immediately and you are notified when it finishes — do NOT sleep, poll, or predict its result. Use get_subagent_result with wait: true if you must block on a specific agent.
- Pass run_in_background: false to block and get the agent's final output inline. When you launch multiple agents for independent work, send them in a single message with multiple tool uses so they run concurrently.
- When the agent is done, it returns a single message back to you. The result is not visible to the user — to show the user, send a text message with a concise summary.
- Trust but verify: an agent's summary describes what it intended to do, not necessarily what it did. When an agent writes or edits code, check the actual changes before reporting the work as done.
- Use resume with an agent id to continue a previous agent's work. A new (non-resume) Agent call starts a fresh agent with no memory of prior runs, so the prompt must be self-contained.
- Use steer_subagent to send mid-run messages to a running agent.
- Clearly tell the agent whether you expect it to write code or just to do research (search, file reads, etc.), since it is not aware of the user's intent.
- Use model as an exact "provider/modelId". Use thinking to control the extended thinking level.
- Use inherit_context if the agent needs the parent conversation history.

## Writing the prompt

Brief the agent like a smart colleague who just walked into the room — it hasn't seen this conversation, doesn't know what you've tried, doesn't understand why this task matters.
- Explain what you're trying to accomplish and why.
- Describe what you've already learned or ruled out.
- Give enough context about the surrounding problem that the agent can make judgment calls rather than just following a narrow instruction.
- If you need a short response, say so ("report in under 200 words").
- Lookups: hand over the exact command. Investigations: hand over the question — prescribed steps become dead weight when the premise is wrong.

Terse command-style prompts produce shallow, generic work.

**Never delegate understanding.** Don't write "based on your findings, fix the bug" or "based on the research, implement it." Those phrases push synthesis onto the agent instead of doing it yourself. Write prompts that prove you understood: include file paths, line numbers, what specifically to change."#;

/// 精简版描述
const AGENT_TOOL_DESCRIPTION_COMPACT: &str = r#"Launch an autonomous agent for complex, multi-step tasks. Agent types:

{{typeList}}

Custom agents: .prux/agents/<name>.md (project) or the global agents directory.

Notes:
- description: 3-5 words (shown in UI). Prompts must be self-contained — the agent has not seen this conversation.
- Parallel work: one message, multiple Agent calls — they run concurrently.
- Subagents run detached by default; you are notified when one completes. Pass run_in_background: false only when your very next action depends on the result and nothing else could usefully happen meanwhile. Never fabricate or predict a pending agent's result — if the user asks before the notification arrives, say it is still running.
- The result is not shown to the user — summarize it for them. Verify an agent's claimed code changes before reporting work as done.
- resume continues a previous agent by ID; steer_subagent messages a running one."#;

/// 自定义描述模板的两个位置（项目优先，其次全局）。
pub fn custom_description_paths(cwd: &str) -> [PathBuf; 2] {
    [
        Path::new(cwd)
            .join(PROJECT_SCOPE_NAME)
            .join("agent-tool-description.md"),
        agent_dir().join("agent-tool-description.md"),
    ]
}

/// 自定义模板里的可替换变量（未知占位符原样保留）。
fn template_vars(cwd: &str) -> [(&'static str, String); 2] {
    [
        (
            "projectAgentsDir",
            Path::new(cwd)
                .join(PROJECT_SCOPE_NAME)
                .join("agents")
                .display()
                .to_string(),
        ),
        (
            "globalAgentsDir",
            agent_dir().join(GLOBAL_AGENT_DIR).display().to_string(),
        ),
    ]
}

/// 自定义模板替换；未知占位符原样保留并返回其名字（调用方一次性告警）。
///
/// 占位符：
/// - `{{typeList}}`：完整类型清单（含工具）
/// - `{{compactTypeList}}`：每型一行（上游的紧凑列表）
/// - `{{agentDir}}` / `{{projectAgentsDir}}` / `{{globalAgentsDir}}`：目录
/// - `{{isolationGuideline}}` / `{{scheduleGuideline}}`：对应功能开启时展开成 `\n- <guideline>`，关掉时为空
pub fn render_custom_description(
    template: &str,
    types: &[AgentType],
    cwd: &str,
) -> (String, Vec<String>) {
    let type_list = type_list_text(types);
    let compact_type_list = compact_type_list_text(types);
    let agent_dir = agent_dir().join(GLOBAL_AGENT_DIR).display().to_string();
    let cfg = manager::config();
    let isolation_guideline = if cfg.worktree_isolation {
        format!("\n- {ISOLATION_GUIDELINE}")
    } else {
        String::new()
    };
    let schedule_guideline = if cfg.schedule {
        format!("\n- {SCHEDULE_GUIDELINE}")
    } else {
        String::new()
    };
    let vars = template_vars(cwd);
    let mut unknown: Vec<String> = Vec::new();
    let mut out = String::new();
    let mut rest = template;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return (out, unknown);
        };
        let name = after[..end].trim();
        let replacement = match name {
            "typeList" => Some(type_list.as_str()),
            "compactTypeList" => Some(compact_type_list.as_str()),
            "agentDir" => Some(agent_dir.as_str()),
            "isolationGuideline" => Some(isolation_guideline.as_str()),
            "scheduleGuideline" => Some(schedule_guideline.as_str()),
            _ => vars
                .iter()
                .find(|(k, _)| *k == name)
                .map(|(_, v)| v.as_str()),
        };

        match replacement {
            Some(value) => out.push_str(value),
            None => {
                out.push_str(&format!("{{{{{name}}}}}"));
                if !unknown.iter().any(|u| u == name) {
                    unknown.push(name.to_string());
                }
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    (out, unknown)
}

/// 紧凑类型清单：每型一行 `- name: <第一句>`
fn compact_type_list_text(types: &[AgentType]) -> String {
    let enabled: Vec<&AgentType> = types.iter().filter(|t| t.enabled).collect();
    if enabled.is_empty() {
        return "(none configured)".to_string();
    }

    enabled
        .iter()
        .map(|t| format!("- {}: {}", t.name, first_sentence(&t.description)))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 取描述的第一句（到 `.`/`。`/换行为止）。
fn first_sentence(text: &str) -> String {
    let text = text.trim();
    let end = text
        .find(['.', '\n', '\u{3002}'])
        .map(|i| i + 1)
        .unwrap_or(text.len());
    text[..end].trim().to_string()
}

/// 类型清单文本（full/compact 共用）。
fn type_list_text(types: &[AgentType]) -> String {
    let enabled: Vec<&AgentType> = types.iter().filter(|t| t.enabled).collect();
    if enabled.is_empty() {
        return "(none configured)".to_string();
    }
    let mut list = String::new();
    for (i, t) in enabled.iter().enumerate() {
        if i > 0 {
            list.push('\n');
        }
        list.push_str(&format!("- {}: {}", t.name, t.description));
        list.push_str(&format!("\n  Tools: {}", t.tools_label()));
    }
    list
}

/// 描述模式的产物：文本 + 需要一次性告警的问题。
pub enum AgentToolDescription {
    /// 正常生成
    Text(String),
    /// 自定义文件缺失/为空 → 回退 full，附带告警文案
    FallbackToFull { text: String, warning: String },
    /// 自定义模板渲染完成，但有未知占位符（文案只报一次）
    TextWithWarning { text: String, warning: String },
}

/// 预载技能正文：按名字的技能发现路径里找，
/// 找到就把正文包成 `<skill name=… location=…>` 块（复用核心的 `/skill:` 展开器，
/// 保证与父级调用技能时的呈现一致），找不到的名字回报给调用方做一次性告警。
///
/// 返回 `(注入段, 未找到的名字)`；无预载时返回空串。
pub fn preload_skills(names: &[String], cwd: &str) -> (String, Vec<String>) {
    if names.is_empty() {
        return (String::new(), Vec::new());
    }
    let agent_dir = agent_dir();
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("/"));
    let trusted = is_project_trusted(Path::new(cwd), &agent_dir);
    let skills = load_skills(
        cwd,
        &agent_dir,
        &home,
        &[],
        &bundled_skill_names(),
        true,
        trusted,
        &[],
    );

    let mut blocks: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for name in names {
        let request = format!("/skill:{name}");
        let block = skills::expand_skill_command(&request, &skills);
        if block == request {
            missing.push(name.clone());
        } else {
            blocks.push(block);
        }
    }

    if blocks.is_empty() {
        return (String::new(), missing);
    }
    (
        format!(
            "<preloaded_skills>\n{}\n</preloaded_skills>",
            blocks.join("\n\n")
        ),
        missing,
    )
}

/// 生成 `Agent` 工具描述（填充可用类型清单）。
///
/// 在 `Extension::tools()` 被调用时求值：因此改 `.md` 后描述可能滞后到下次 rebuild，
/// 但 spawn 本身每次现扫、不滞后。
pub fn agent_tool_description(types: &[AgentType]) -> String {
    AGENT_TOOL_DESCRIPTION.replace("{{typeList}}", &type_list_text(types))
}

/// 按配置模式生成描述（`custom` 读取 `agent-tool-description.md`，缺失/为空回退 full 并告警）。
pub fn agent_tool_description_for(
    types: &[AgentType],
    mode: ToolDescriptionMode,
    cwd: &str,
) -> AgentToolDescription {
    match mode {
        ToolDescriptionMode::Full => AgentToolDescription::Text(agent_tool_description(types)),
        ToolDescriptionMode::Compact => AgentToolDescription::Text(
            AGENT_TOOL_DESCRIPTION_COMPACT.replace("{{typeList}}", &type_list_text(types)),
        ),
        ToolDescriptionMode::Custom => {
            for path in custom_description_paths(cwd) {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };

                let text = text.trim();
                if text.is_empty() {
                    return AgentToolDescription::FallbackToFull {
                        text: agent_tool_description(types),
                        warning: format!(
                            "tool_description = custom but {} is empty — using the full description",
                            path.display()
                        ),
                    };
                }

                let (rendered, unknown) = render_custom_description(text, types, cwd);
                if unknown.is_empty() {
                    return AgentToolDescription::Text(rendered);
                }

                return AgentToolDescription::TextWithWarning {
                    text: rendered,
                    warning: format!(
                        "agent-tool-description.md: unknown placeholder(s) left as-is: {}",
                        unknown.join(", ")
                    ),
                };
            }

            AgentToolDescription::FallbackToFull {
                text: agent_tool_description(types),
                warning: "tool_description = custom but no agent-tool-description.md found \
                          (looked in .prux/ and the global agents directory) — using the full description"
                    .to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extensions::subagent::types::{AgentSource, PromptMode};

    fn ty(name: &str) -> AgentType {
        AgentType {
            name: name.to_string(),
            display_name: name.to_string(),
            description: "Does things.".to_string(),
            color: None,
            tools: Some(vec!["read".to_string()]),
            model: None,
            thinking: None,
            max_turns: None,
            prompt_mode: PromptMode::Replace,
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
        }
    }

    #[test]
    fn description_lists_enabled_types_only() {
        let mut disabled = ty("hidden");
        disabled.enabled = false;
        let d = agent_tool_description(&[ty("Explore"), disabled]);
        assert!(d.contains("- Explore: Does things."));
        assert!(d.contains("Tools: read"));
        assert!(!d.contains("hidden"));
        assert!(!d.contains("{{typeList}}"));
    }

    #[test]
    fn description_handles_empty_roster() {
        let d = agent_tool_description(&[]);
        assert!(d.contains("(none configured)"));
    }

    #[test]
    fn compact_and_custom_modes() {
        use super::super::config::ToolDescriptionMode as M;
        let types = vec![ty("Explore")];
        let AgentToolDescription::Text(compact) =
            agent_tool_description_for(&types, M::Compact, "/tmp/p")
        else {
            panic!("compact 不应回退");
        };
        assert!(compact.contains("- Explore: Does things."), "{compact}");
        assert!(
            !compact.contains("## Writing the prompt"),
            "compact 应短得多"
        );

        // custom 无文件 → 回退 full + 告警
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();
        let _ad = crate::test_support::AgentDirGuard::temp();
        match agent_tool_description_for(&types, M::Custom, &cwd) {
            AgentToolDescription::FallbackToFull { text, warning } => {
                assert!(text.contains("## Writing the prompt"));
                assert!(
                    warning.contains("no agent-tool-description.md"),
                    "{warning}"
                );
            }
            _ => panic!("不应有自定义文件"),
        }

        // 项目级自定义文件：占位符替换 + 未知占位符保留并告警
        let project_dir = dir.path().join(crate::PROJECT_SCOPE_NAME);
        std::fs::create_dir_all(&project_dir).unwrap();
        std::fs::write(
            project_dir.join("agent-tool-description.md"),
            "TYPES:\n{{typeList}}\nPROJECT={{projectAgentsDir}}\nDIR={{agentDir}}\nCOMPACT:\n{{compactTypeList}}\nISO:{{isolationGuideline}}\nSCHED:{{scheduleGuideline}}\nMYSTERY={{nope}}",
        )
        .unwrap();
        match agent_tool_description_for(&types, M::Custom, &cwd) {
            AgentToolDescription::TextWithWarning { text, warning } => {
                assert!(text.contains("- Explore: Does things."), "{text}");
                assert!(text.contains("PROJECT="), "{text}");
                // 上游占位符名也展开
                assert!(
                    !text.contains("{{agentDir}}") && text.contains("DIR="),
                    "{text}"
                );
                assert!(
                    text.contains("- Explore: Does things."),
                    "compact 也列出类型: {text}"
                );
                assert!(
                    text.contains("- Use isolation:"),
                    "isolation 开启时展开: {text}"
                );
                assert!(
                    text.contains("- Use `schedule`"),
                    "schedule 开启时展开: {text}"
                );
                assert!(text.ends_with("MYSTERY={{nope}}"), "{text}");
                assert!(warning.contains("nope"), "{warning}");
            }
            _ => panic!("应渲染自定义模板"),
        }
    }

    #[test]
    fn preload_skills_reads_bodies_and_reports_missing() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_string_lossy().to_string();

        // 用户级技能：<agent_dir>/skills/<name>/SKILL.md
        let skill_dir = crate::core::settings_manager::agent_dir()
            .join("skills")
            .join("review-helper");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: review-helper\ndescription: Helps with reviews\n---\n\nBODY OF SKILL\n",
        )
        .unwrap();

        let (section, missing) = preload_skills(&["review-helper".to_string()], &cwd);
        assert!(section.starts_with("<preloaded_skills>"), "{section}");
        assert!(section.contains("BODY OF SKILL"), "{section}");
        assert!(section.contains("name=\"review-helper\""), "{section}");
        assert!(section.contains("location="), "{section}");
        assert!(missing.is_empty(), "{missing:?}");

        // 找不到的名字：不注入、单独回报
        let (section, missing) =
            preload_skills(&["review-helper".to_string(), "nope".to_string()], &cwd);
        assert!(section.contains("BODY OF SKILL"));
        assert_eq!(missing, vec!["nope".to_string()]);

        // 全找不到 → 空段（不产生空标签）
        let (section, missing) = preload_skills(&["nope".to_string()], &cwd);
        assert!(section.is_empty());
        assert_eq!(missing.len(), 1);
        // 无预载 → 不做任何发现（空输入短路）
        assert_eq!(preload_skills(&[], &cwd), (String::new(), Vec::new()));
    }

    #[test]
    fn active_agent_tag_escapes_name() {
        assert_eq!(
            active_agent_tag("a\"b<c>"),
            "<active_agent name=\"a&quot;b&lt;c&gt;\"/>"
        );
    }
}

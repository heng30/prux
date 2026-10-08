//! `--verbose` 启动报告：在会话区列出本次启动实际加载的资源。
//!
//! 启动资源清单（Context / Skills / Prompts / Extensions），
//! 并补上真正决定行为的启动项（模型、会话文件、工具集）。
//! 数据采集 [`collect`] 与渲染 [`render`] 分离：渲染是纯函数，便于单测。
//!
//! 刻意**不列主题清单**
//! 主题数量随用户配置增长，列表噪音大于信息量；自定义主题仍在 `/settings` 与 `/theme` 里可见。

use crate::{
    core::{agent_session::Agent, extensions, prompt_templates::PromptTemplate},
    modes::interactive::app::SysSpan,
    utils::paths::shorten_home,
};

/// 启动报告的数据来源（与渲染分离，测试可直接构造）
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StartupFacts {
    /// provider 名（如 anthropic）。
    pub provider: String,
    /// 本次生效的模型 id。
    pub model_id: String,
    /// 推理级别（off/low/medium/high 等）；None = 模型不支持推理或未启用。
    pub thinking: Option<String>,
    /// 会话文件路径；None = 未持久化（--no-session / 打开失败）
    pub session_file: Option<String>,
    /// 本次会话启用的工具名（已排序去重）。
    pub tools: Vec<String>,
    /// 上下文文件路径（按加载顺序，顺序影响系统提示拼接）
    pub context_files: Vec<String>,
    /// (技能名, SKILL.md 路径)
    pub skills: Vec<(String, String)>,
    /// (模板名, 文件路径)
    pub prompts: Vec<(String, String)>,
    /// 已启用扩展名（已排序去重）。
    pub extensions: Vec<String>,
}

/// 采集启动报告数据
pub fn collect(agent: &Agent, prompts: &[PromptTemplate]) -> StartupFacts {
    let mut skills: Vec<(String, String)> = agent
        .skills
        .iter()
        .map(|s| (s.name.clone(), shorten_home(&s.path.to_string_lossy())))
        .collect();
    skills.sort();

    let mut prompt_list: Vec<(String, String)> = prompts
        .iter()
        .map(|p| (p.name.clone(), shorten_home(&p.file_path)))
        .collect();
    prompt_list.sort();

    let mut tools: Vec<String> = agent
        .tools
        .iter()
        .map(|(name, _, _)| name.clone())
        .collect();
    tools.sort();

    let mut extensions: Vec<String> = extensions::registered()
        .iter()
        .map(|e| e.name().to_string())
        .collect();
    extensions.sort();
    extensions.dedup();

    StartupFacts {
        provider: agent.model.provider.clone(),
        model_id: agent.model.model_id.clone(),
        thinking: agent.thinking_level.clone(),
        session_file: agent
            .session
            .as_ref()
            .and_then(|s| s.get_session_file())
            .map(|p| shorten_home(&p.to_string_lossy())),
        tools,
        context_files: agent
            .rebuild_ctx
            .context_files
            .iter()
            .map(|(path, _)| shorten_home(path))
            .collect(),
        skills,
        prompts: prompt_list,
        extensions,
    }
}

/// 渲染为一条多行系统消息的 span 序列（空小节不输出）
pub fn render(facts: &StartupFacts) -> Vec<SysSpan> {
    let mut spans: Vec<SysSpan> = Vec::new();

    let model = match &facts.thinking {
        Some(t) => format!("{}/{} (thinking: {})", facts.provider, facts.model_id, t),
        None => format!("{}/{}", facts.provider, facts.model_id),
    };
    section(&mut spans, "Model", None, &[model]);

    match &facts.session_file {
        Some(path) => section(&mut spans, "Session", None, std::slice::from_ref(path)),
        None => section(
            &mut spans,
            "Session",
            None,
            &["(not persisted)".to_string()],
        ),
    }

    section(
        &mut spans,
        "Tools",
        Some(facts.tools.len()),
        &[facts.tools.join(", ")],
    );

    section(
        &mut spans,
        "Context",
        Some(facts.context_files.len()),
        &facts.context_files,
    );

    let skills: Vec<String> = facts
        .skills
        .iter()
        .map(|(name, path)| format!("{} ({})", name, path))
        .collect();
    section(&mut spans, "Skills", Some(skills.len()), &skills);

    let prompts: Vec<String> = facts
        .prompts
        .iter()
        .map(|(name, path)| format!("/{} ({})", name, path))
        .collect();
    section(&mut spans, "Prompts", Some(prompts.len()), &prompts);

    section(
        &mut spans,
        "Extensions",
        Some(facts.extensions.len()),
        &[facts.extensions.join(", ")],
    );

    spans
}

/// 单个小节：`[标题] (数量)` + 缩进条目，每行一个
fn section(spans: &mut Vec<SysSpan>, title: &str, count: Option<usize>, lines: &[String]) {
    if lines.iter().all(|l| l.trim().is_empty()) {
        return;
    }

    let head = match count {
        Some(n) => format!("[{}] ({})\n", title, n),
        None => format!("[{}]\n", title),
    };
    spans.push(SysSpan {
        fg: Some("accent".to_string()),
        bold: true,
        text: head,
    });

    for line in lines {
        spans.push(SysSpan::fg("dim", &format!("  {}\n", line)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> StartupFacts {
        StartupFacts {
            provider: "deepseek".into(),
            model_id: "deepseek-v4-pro".into(),
            thinking: Some("high".into()),
            session_file: Some("/sessions/a.jsonl".into()),
            tools: vec!["bash".into(), "read".into()],
            context_files: vec!["/proj/AGENTS.md".into()],
            skills: vec![("codebase-design".into(), "/p/skills/cd/SKILL.md".into())],
            prompts: vec![("review".into(), "/p/prompts/review.md".into())],
            extensions: vec!["footer".into(), "plan-mode".into()],
        }
    }

    fn text(facts: &StartupFacts) -> String {
        render(facts).iter().map(|s| s.text.clone()).collect()
    }

    #[test]
    fn renders_all_sections() {
        let out = text(&facts());
        assert!(
            out.contains("[Model]\n  deepseek/deepseek-v4-pro (thinking: high)\n"),
            "{out}"
        );
        assert!(out.contains("[Session]\n  /sessions/a.jsonl\n"), "{out}");
        assert!(out.contains("[Tools] (2)\n  bash, read\n"), "{out}");
        assert!(out.contains("[Context] (1)\n  /proj/AGENTS.md\n"), "{out}");
        assert!(
            out.contains("[Skills] (1)\n  codebase-design (/p/skills/cd/SKILL.md)\n"),
            "{out}"
        );
        assert!(
            out.contains("[Prompts] (1)\n  /review (/p/prompts/review.md)\n"),
            "{out}"
        );
        assert!(!out.contains("[Themes]"), "启动报告不再列主题清单: {out}");
        assert!(
            out.contains("[Extensions] (2)\n  footer, plan-mode\n"),
            "{out}"
        );
    }

    #[test]
    fn marks_missing_session_and_skips_empty_sections() {
        let f = StartupFacts {
            provider: "p".into(),
            model_id: "m".into(),
            ..StartupFacts::default()
        };
        let out = text(&f);
        assert!(out.contains("[Session]\n  (not persisted)\n"), "{out}");
        assert!(!out.contains("[Skills]"), "{out}");
        assert!(!out.contains("[Context]"), "{out}");
        assert!(!out.contains("[Extensions]"), "{out}");
        assert!(!out.contains("[Tools]"), "{out}");
    }

    #[test]
    fn collect_maps_agent_resources() {
        let _ad = crate::test_support::AgentDirGuard::temp();

        let entry = crate::core::model_resolver::find_model("deepseek", "deepseek-flash").unwrap();
        let agent = Agent::new(
            entry,
            "/tmp".to_string(),
            crate::core::agent_session::ToolSelection::Default(vec![
                "read".to_string(),
                "bash".to_string(),
            ]),
            Vec::new(),
            Some("high".to_string()),
            None,
            None,
            vec![("/tmp/AGENTS.md".to_string(), "ctx".to_string())],
            None,
            None,
            false,
            vec![crate::core::skills::Skill {
                name: "demo".to_string(),
                description: "d".to_string(),
                path: "/tmp/skills/demo/SKILL.md".into(),
                base_dir: "/tmp/skills/demo".into(),
                disable_model_invocation: false,
                source: "/tmp/skills".to_string(),
            }],
        )
        .unwrap();

        let prompts = vec![PromptTemplate {
            name: "review".to_string(),
            description: String::new(),
            argument_hint: None,
            content: String::new(),
            file_path: "/tmp/prompts/review.md".to_string(),
        }];
        let facts = collect(&agent, &prompts);

        assert_eq!(facts.provider, "deepseek");
        assert_eq!(facts.model_id, "deepseek-flash");
        assert_eq!(facts.thinking.as_deref(), Some("high"));
        assert_eq!(facts.session_file, None, "未传会话 → 无持久化文件");
        // 其他测试可能残留已注册扩展（全局注册表）：只断言内置工具在内
        assert!(facts.tools.contains(&"bash".to_string()));
        assert!(facts.tools.contains(&"read".to_string()));
        assert_eq!(facts.context_files, vec!["/tmp/AGENTS.md"]);
        assert_eq!(
            facts.skills,
            vec![("demo".to_string(), "/tmp/skills/demo/SKILL.md".to_string())]
        );
        assert_eq!(
            facts.prompts,
            vec![("review".to_string(), "/tmp/prompts/review.md".to_string())]
        );
        // 扩展注册表由 main 注册（单测进程内为空）：只断言采集不报错且无重复
        let mut sorted = facts.extensions.clone();
        sorted.dedup();
        assert_eq!(facts.extensions, sorted);
    }

    /// 对齐 pi 0.99：启动报告不再列主题清单（自定义主题仍在 /settings、/theme 可见）
    #[test]
    fn startup_report_omits_theme_list() {
        let out = text(&facts());
        assert!(!out.contains("[Themes]"), "{out}");
        assert!(out.contains("[Extensions]"), "{out}");
    }
}

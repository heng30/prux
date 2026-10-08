//! cli::args 集成测试：clap derive 解析的面覆盖。
//! 覆盖：模型/会话/工具/资源选项、@文件与消息分流、子命令、旧式多字符短选项、可选值参数。

use prux::cli::args::{Args, Command};

fn parse(args: &[&str]) -> Args {
    let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    Args::parse_from_args(&v).expect("parse should succeed")
}

#[test]
fn parses_model_options() {
    let a = parse(&[
        "--provider",
        "anthropic",
        "--model",
        "claude:high",
        "--api-key",
        "sk-test",
        "--thinking",
        "medium",
        "--models",
        "a,b,c",
    ]);
    assert_eq!(a.provider.as_deref(), Some("anthropic"));
    assert_eq!(a.model.as_deref(), Some("claude:high"));
    assert_eq!(a.api_key.as_deref(), Some("sk-test"));
    assert_eq!(a.thinking.as_deref(), Some("medium"));
    assert_eq!(a.models, vec!["a", "b", "c"]);
}

#[test]
fn parses_session_options() {
    let a = parse(&[
        "-c",
        "--session",
        "abc",
        "--fork",
        "def",
        "--session-dir",
        "/tmp/s",
        "--session-id",
        "xyz",
        "--no-session",
        "-n",
        "demo",
    ]);
    assert!(a.continue_session);
    assert_eq!(a.session.as_deref(), Some("abc"));
    assert_eq!(a.fork.as_deref(), Some("def"));
    assert_eq!(a.session_dir.as_deref(), Some("/tmp/s"));
    assert_eq!(a.session_id.as_deref(), Some("xyz"));
    assert!(a.no_session);
    assert_eq!(a.name.as_deref(), Some("demo"));
}

#[test]
fn parses_tool_options() {
    let a = parse(&[
        "-t",
        "read,bash",
        "--exclude-tools",
        "grep",
        "--no-builtin-tools",
    ]);
    assert_eq!(a.tools, vec!["read", "bash"]);
    assert_eq!(a.exclude_tools, vec!["grep"]);
    assert!(a.no_builtin_tools);

    let b = parse(&["--no-tools"]);
    assert!(b.no_tools);

    // --no-extensions：禁用所有扩展（含内置），与工具开关互相独立
    let c = parse(&["--no-extensions"]);
    assert!(c.no_extensions);
    assert!(!c.no_tools && !c.no_builtin_tools);
}

#[test]
fn parses_resource_options() {
    let a = parse(&[
        "--skill",
        "skill-a",
        "--skill",
        "skill-b",
        "--prompt-template",
        "tpl.md",
        "--theme",
        "dark.json",
        "--use-theme",
        "dark",
        "--no-skills",
        "--no-prompt-templates",
        "--no-themes",
        "--no-context-files",
    ]);
    assert_eq!(a.skill, vec!["skill-a", "skill-b"]);
    assert_eq!(a.prompt_template, vec!["tpl.md"]);
    assert_eq!(a.theme, vec!["dark.json"]);
    assert_eq!(a.use_theme.as_deref(), Some("dark"));
    assert!(a.no_skills && a.no_prompt_templates && a.no_themes && a.no_context_files);
}

#[test]
fn parses_other_options() {
    let a = parse(&[
        "--system-prompt",
        "sys",
        "--append-system-prompt",
        "a1",
        "--append-system-prompt",
        "a2",
        "--verbose",
        "--offline",
    ]);
    assert_eq!(a.system_prompt.as_deref(), Some("sys"));
    assert_eq!(a.append_system_prompt, vec!["a1", "a2"]);
    assert!(a.verbose && a.offline);
}

#[test]
fn parses_files_and_messages() {
    let a = parse(&["@prompt.md", "hello", "@img.png", "world"]);
    assert_eq!(a.file_args(), vec!["prompt.md", "img.png"]);
    assert_eq!(a.messages(), vec!["hello", "world"]);
}

#[test]
fn parses_approve_merge() {
    assert_eq!(parse(&["-a"]).approve_option(), Some(true));
    assert_eq!(parse(&["--no-approve"]).approve_option(), Some(false));
    assert_eq!(parse(&[]).approve_option(), None);
}

#[test]
fn parses_optional_values() {
    assert_eq!(parse(&["--list-models"]).list_models.as_deref(), Some(""));
    assert_eq!(
        parse(&["--list-models", "claude"]).list_models.as_deref(),
        Some("claude")
    );
    let e = parse(&["--export", "a.json"]);
    assert_eq!(e.export_in(), Some("a.json"));
    assert_eq!(e.export_out(), None);
    let e2 = parse(&["--export", "a.json", "out.html"]);
    assert_eq!(e2.export_in(), Some("a.json"));
    assert_eq!(e2.export_out(), Some("out.html"));
}

#[test]
fn parses_subcommands() {
    let a = parse(&["auth", "anthropic"]);
    match a.command {
        Some(Command::Auth { args }) => {
            assert_eq!(args, vec!["anthropic".to_string()]);
        }
        other => panic!("expected Auth, got {:?}", other),
    }
}

#[test]
fn rejects_bad_input() {
    let v: Vec<String> = vec!["--thinking".to_string(), "bogus".to_string()];
    assert!(Args::parse_from_args(&v).is_err());

    let v2: Vec<String> = vec!["--mode".to_string(), "json".to_string()];
    assert!(Args::parse_from_args(&v2).is_err()); // --mode 已移除，按未知参数报错

    let v3: Vec<String> = vec!["--no-such-flag".to_string()];
    assert!(Args::parse_from_args(&v3).is_err());
}

#[test]
fn help_and_version_flags_exist() {
    // -h/--help/-v/--version 由 clap 捕获（返回 DisplayHelp/DisplayVersion 错误），不应被当作未知参数
    let h = Args::parse_from_args(&["--help".to_string()]);
    assert!(h.is_err(), "--help should be captured by clap");
    let v = Args::parse_from_args(&["-v".to_string()]);
    assert!(v.is_err(), "-v should be captured by clap");
}

#[test]
fn auth_subcommand_collects_full_args() {
    let a = parse(&[
        "auth",
        "check",
        "--provider",
        "deepseek",
        "--json",
        "--credentials",
        "--no-refresh",
    ]);
    match a.command {
        Some(Command::Auth { args }) => {
            assert_eq!(
                args,
                vec![
                    "check".to_string(),
                    "--provider".to_string(),
                    "deepseek".to_string(),
                    "--json".to_string(),
                    "--credentials".to_string(),
                    "--no-refresh".to_string(),
                ]
            );
        }
        other => panic!("expected Auth, got {:?}", other),
    }
    // print-bearer-token 带 --min-expiry
    let b = parse(&[
        "auth",
        "print-bearer-token",
        "--provider",
        "opencode",
        "--min-expiry",
        "30m",
    ]);
    match b.command {
        Some(Command::Auth { args }) => {
            assert_eq!(
                args,
                vec![
                    "print-bearer-token".to_string(),
                    "--provider".to_string(),
                    "opencode".to_string(),
                    "--min-expiry".to_string(),
                    "30m".to_string(),
                ]
            );
        }
        other => panic!("expected Auth, got {:?}", other),
    }
}

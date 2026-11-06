//! 命令行参数解析
//!
//! 解析完全由 clap 完成，以及 @文件 与 消息 的位置参数分流。

use crate::{
    APP_NAME,
    core::{
        self,
        extensions::{CliFlagDef, ExtensionSubcommand},
    },
};
use clap::{ArgAction, CommandFactory, FromArgMatches, Parser, Subcommand, error::ErrorKind};
use std::collections::HashSet;

/// 允许的思考等级取值（off…max），用于 clap 参数校验与错误提示。
pub const VALID_THINKING_LEVELS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// 思考级别的文案：一句话描述 + 估算思考量（`/thinking` 面板的副文案与右侧提示），
/// 键为 [`VALID_THINKING_LEVELS`] 中的取值；估算为空串表示该级别没有估算值。
const THINKING_LEVEL_TEXTS: [(&str, &str, &str); 7] = [
    ("off", "No reasoning", ""),
    ("minimal", "Very brief reasoning", "~1k tokens"),
    ("low", "Light reasoning", "~2k tokens"),
    ("medium", "Moderate reasoning", "~8k tokens"),
    ("high", "Deep reasoning", "~16k tokens"),
    ("xhigh", "Extra-high reasoning", "~32k tokens"),
    ("max", "Maximum reasoning", ""),
];

/// [`VALID_THINKING_LEVELS`] 中某级别的（一句话描述, 估算思考量）；
/// 估算为空串表示该级别没有估算值，未登记的级别返回两个空串。
pub fn thinking_level_text(level: &str) -> (&'static str, &'static str) {
    THINKING_LEVEL_TEXTS
        .iter()
        .find(|(name, _, _)| *name == level)
        .map(|(_, desc, tokens)| (*desc, *tokens))
        .unwrap_or_default()
}

/// [`VALID_THINKING_LEVELS`] 中某级别的完整描述（估算思考量以括号附在后面，
/// 如 `Light reasoning (~2k tokens)`）；未登记的级别返回空串。
pub fn thinking_level_description(level: &str) -> String {
    let (desc, tokens) = thinking_level_text(level);
    if tokens.is_empty() {
        desc.to_string()
    } else {
        format!("{desc} ({tokens})")
    }
}

/// 内置（非扩展注入）的子命令。
///
/// 扩展子命令（`prux mcp`、`prux acp`）不在这里：声明与入口都归扩展，
/// 只有**已启用**扩展的子命令才会出现在 `prux --help` 里。
#[derive(Debug, Clone, Subcommand)]
pub enum Command {
    /// Print credentials or check provider readiness
    #[command(disable_help_flag = true)]
    Auth {
        /// 原样透传给 auth 子命令的剩余参数，不含子命令名本身。
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

/// 命令行入口
///
/// 用法: prux [OPTIONS] [@文件...] [消息...]
#[derive(Debug, Clone, Parser)]
#[command(
    name = APP_NAME,
    version,
    disable_version_flag = true,
    about = concat!(env!("CARGO_PKG_NAME"), " - AI coding assistant with read, bash, edit, write tools"),
    long_about = concat!(concat!(env!("CARGO_PKG_NAME"), " - AI coding assistant with read, bash, edit, write tools\nuse @ prefix to include file contents in the message: ", concat!(env!("CARGO_PKG_NAME"), " @prompt.md @image.png \"answer this question\""))),
    disable_help_subcommand = true
)]
pub struct Args {
    // ---------- 子命令 ----------
    /// 本次要执行的子命令；为 None 时按普通对话模式运行。
    #[command(subcommand)]
    pub command: Option<Command>,

    // ---------- 模型选项 ----------
    /// Provider (anthropic, openai, google, etc.)
    #[arg(long)]
    pub provider: Option<String>,
    /// Model pattern or ID (supports provider/id and optional :<thinking>)
    #[arg(long)]
    pub model: Option<String>,
    /// API key (overrides the environment variable)
    #[arg(long = "api-key", value_name = "KEY")]
    pub api_key: Option<String>,
    /// Thinking level: off, minimal, low, medium, high, xhigh, max
    #[arg(long, value_parser = parse_thinking_level, value_name = "LEVEL")]
    pub thinking: Option<String>,
    /// Comma-separated model list (used for Ctrl+P cycling)
    #[arg(long, value_delimiter = ',', value_name = "PATTERNS")]
    pub models: Vec<String>,
    /// List available models (optional search term)
    #[arg(
        long = "list-models",
        num_args = 0..=1,
        default_missing_value = "",
        value_name = "SEARCH"
    )]
    pub list_models: Option<String>,

    // ---------- 会话选项 ----------
    /// Continue the most recent session
    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,
    /// Browse and pick a session to resume
    #[arg(short = 'r', long)]
    pub resume: bool,
    /// Use the given session file or partial UUID
    #[arg(long, value_name = "PATH|ID")]
    pub session: Option<String>,
    /// Fork the given session file or partial UUID into a new session
    #[arg(long, value_name = "PATH|ID")]
    pub fork: Option<String>,
    /// Session storage directory
    #[arg(long = "session-dir", value_name = "DIR")]
    pub session_dir: Option<String>,
    /// Use an exact project session ID
    #[arg(long = "session-id", value_name = "ID")]
    pub session_id: Option<String>,
    /// Ephemeral session (not persisted)
    #[arg(long = "no-session")]
    pub no_session: bool,
    /// Set the session display name
    #[arg(short = 'n', long, value_name = "NAME")]
    pub name: Option<String>,

    // ---------- 工具选项 ----------
    /// Allowlist of tool names or patterns (*) to enable, comma-separated;
    /// accepts +name / -name to add or remove from the default selection;
    /// keeps MCP tools unless an entry starts with mcp__
    #[arg(short = 't', long, value_delimiter = ',', value_name = "LIST")]
    pub tools: Vec<String>,
    /// Denylist of tool names or patterns (*) to disable, comma-separated;
    /// applies to all tools, MCP tools included
    #[arg(
        short = 'x',
        long = "exclude-tools",
        value_delimiter = ',',
        value_name = "LIST"
    )]
    pub exclude_tools: Vec<String>,
    /// Disable all tools by default
    #[arg(long = "no-tools")]
    pub no_tools: bool,
    /// Disable built-in tools by default
    #[arg(long = "no-builtin-tools")]
    pub no_builtin_tools: bool,
    /// Disable built-in MCP support: no servers connect and no MCP tools
    #[arg(long = "no-mcp")]
    pub no_mcp: bool,
    /// Disable every extension (including built-in ones) for this run
    #[arg(long = "no-extensions")]
    pub no_extensions: bool,

    // ---------- 资源选项 ----------
    /// Load a skill file or directory (repeatable)
    #[arg(long, value_name = "PATH")]
    pub skill: Vec<String>,
    /// Disable skill discovery and loading
    #[arg(long = "no-skills")]
    pub no_skills: bool,
    /// Load a prompt template file or directory (repeatable)
    #[arg(long = "prompt-template", value_name = "PATH")]
    pub prompt_template: Vec<String>,
    /// Disable prompt template discovery
    #[arg(long = "no-prompt-templates")]
    pub no_prompt_templates: bool,
    /// Load a theme file or directory (repeatable)
    #[arg(long, value_name = "PATH")]
    pub theme: Vec<String>,
    /// Set the initial interactive theme
    #[arg(long = "use-theme", value_name = "NAME")]
    pub use_theme: Option<String>,
    /// Disable theme discovery
    #[arg(long = "no-themes")]
    pub no_themes: bool,
    /// Disable AGENTS.md / CLAUDE.md context file discovery
    #[arg(long = "no-context-files")]
    pub no_context_files: bool,

    // ---------- 其他选项 ----------
    /// Show version
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    pub version: (),
    /// Replace the default system prompt
    #[arg(long = "system-prompt", value_name = "TEXT")]
    pub system_prompt: Option<String>,
    /// Append to the system prompt (repeatable)
    #[arg(long = "append-system-prompt", value_name = "TEXT")]
    pub append_system_prompt: Vec<String>,
    /// Launch the interactive TUI instead of the desktop GUI (prux-gui only)
    #[cfg(all(feature = "gui", not(windows)))]
    #[arg(long)]
    pub tui: bool,
    /// List loaded startup resources (model/session/tools/context/skills/prompts/themes/extensions)
    #[arg(long)]
    pub verbose: bool,
    /// Disable startup network operations (equivalent to PRUX_OFFLINE=1)
    #[arg(long)]
    pub offline: bool,
    /// Trust project resources (skip the trust prompt)
    #[arg(short = 'a', long)]
    pub approve: bool,
    /// Do not trust project resources
    #[arg(long = "no-approve")]
    pub no_approve: bool,
    /// Export a session to HTML: --export <session> [out]
    #[arg(
        long,
        num_args = 1..=2,
        value_names = ["SESSION", "OUT"]
    )]
    pub export: Vec<String>,

    // ---------- 位置参数 ----------
    /// File arguments with @ prefix and message text
    #[arg(value_name = "@FILES|MESSAGES")]
    pub input: Vec<String>,
}

/// 解析结果：命令行参数 + 扩展 flag 值
pub struct ParsedArgs {
    /// clap 解析出的选项集合，供后续分发与读取。
    pub args: Args,
    /// 扩展 flag：flag 名 → bool 值（仅已启用扩展注入的 flag）
    pub extension_flags: Vec<(String, bool)>,
    /// 带值扩展 flag（`--name <value>`）：`(名字, 值)`
    pub extension_flag_values: Vec<(String, String)>,
    /// 扩展注入的 CLI 子命令：`(子命令名, 其余参数)`。命中时 clap 不参与解析。
    pub extension_subcommand: Option<(String, Vec<String>)>,
}

/// 校验思考级别（非法级别给出诊断而非报错）
fn parse_thinking_level(s: &str) -> Result<String, String> {
    if VALID_THINKING_LEVELS.contains(&s) {
        Ok(s.to_string())
    } else {
        Err(format!(
            "Invalid thinking level \"{}\". Valid values: {}",
            s,
            VALID_THINKING_LEVELS.join(", ")
        ))
    }
}

impl Args {
    /// 从原始 argv 解析参数
    pub fn parse_from_env() -> Result<Args, clap::Error> {
        let raw: Vec<String> = std::env::args().skip(1).collect();
        Args::try_parse_from(std::iter::once(APP_NAME.to_string()).chain(raw))
    }

    /// 解析并注入已启用扩展声明的 CLI flag（--plan 等）。
    ///
    /// 仅 [`crate::core::extensions::registered`]（已启用且当前模式可用）的扩展
    /// flag 会注入 clap：未启用扩展的 flag 不出现，`--xxx` 按未知参数报错。
    /// 与内置参数或扩展间同名冲突时报错退出（保持 clap 严格语义）。
    pub fn parse_with_extension_flags() -> Result<ParsedArgs, clap::Error> {
        let raw: Vec<String> = std::env::args().skip(1).collect();
        let mut cmd = Self::command();

        // 收集已启用扩展的 flag 声明（保留扩展名供报错/分发）
        let mut declared: Vec<(String, CliFlagDef)> = Vec::new();
        for ext in core::extensions::registered() {
            for flag in ext.cli_flags() {
                declared.push((ext.name().to_string(), flag));
            }
        }

        // 冲突检测：与内置 long 名冲突 / 扩展间同名冲突 → 报错退出
        let builtin: HashSet<String> = cmd
            .get_arguments()
            .filter_map(|a| a.get_long().map(String::from))
            .collect();
        let mut seen: HashSet<&'static str> = HashSet::new();

        for (ext, flag) in &declared {
            if builtin.contains(flag.name) {
                return Err(clap::Error::raw(
                    ErrorKind::InvalidValue,
                    format!(
                        "CLI flag conflict: extension \"{ext}\" declares --{} which conflicts with a built-in argument",
                        flag.name
                    ),
                ));
            }
            if !seen.insert(flag.name) {
                return Err(clap::Error::raw(
                    ErrorKind::InvalidValue,
                    format!(
                        "CLI flag conflict: multiple extensions declare --{}",
                        flag.name
                    ),
                ));
            }
        }

        // 动态追加扩展 flag：bool 开关用 SetTrue，声明了 takes_value 的用 Set 收一个值
        for flag in &declared {
            cmd = cmd.arg(Self::extension_flag_arg(&flag.1));
        }

        // 扩展子命令注入成**真正的 clap 子命令**：这样 `prux --help` 的 Commands 段
        // 才会列出它们，且只列出已启用扩展声明的那些。
        let subcommands = core::extensions::declared_subcommands();
        let builtin_subs: HashSet<String> = cmd
            .get_subcommands()
            .map(|c| c.get_name().to_string())
            .collect();
        let mut seen_sub: HashSet<&'static str> = HashSet::new();
        for (ext, sub) in &subcommands {
            if builtin_subs.contains(sub.name) {
                return Err(clap::Error::raw(
                    ErrorKind::InvalidValue,
                    format!(
                        "CLI subcommand conflict: extension \"{ext}\" declares `{}` which conflicts with a built-in subcommand",
                        sub.name
                    ),
                ));
            }

            if !seen_sub.insert(sub.name) {
                return Err(clap::Error::raw(
                    ErrorKind::InvalidValue,
                    format!(
                        "CLI subcommand conflict: multiple extensions declare `{}`",
                        sub.name
                    ),
                ));
            }

            cmd = cmd.subcommand(Self::extension_subcommand_cmd(sub));
        }

        let mut extension_flags: Vec<(String, bool)> = Vec::new();
        let mut extension_flag_values: Vec<(String, String)> = Vec::new();
        let mut matches =
            cmd.try_get_matches_from(std::iter::once(APP_NAME.to_string()).chain(raw))?;

        // 命中的是扩展子命令：整段摘出来再交给 `Args`，否则 derive 会为未知子命令报错。
        let extension_subcommand = match matches.subcommand_name() {
            Some(name) if subcommands.iter().any(|(_, s)| s.name == name) => {
                let (name, sub_matches) = matches.remove_subcommand().expect("just matched");
                let rest: Vec<String> = sub_matches
                    .get_many::<String>("args")
                    .map(|values| values.cloned().collect())
                    .unwrap_or_default();
                Some((name, rest))
            }
            _ => None,
        };

        let mut args = Args::from_arg_matches(&matches)?;
        args.normalize();

        for (_, flag) in declared {
            if flag.takes_value {
                if let Some(value) = matches.get_one::<String>(flag.name) {
                    extension_flag_values.push((flag.name.to_string(), value.clone()));
                }
            } else {
                extension_flags.push((flag.name.to_string(), matches.get_flag(flag.name)));
            }
        }

        Ok(ParsedArgs {
            args,
            extension_flags,
            extension_flag_values,
            extension_subcommand,
        })
    }

    /// 把一个扩展 flag 声明变成 clap 参数：`takes_value` 决定收不收值。
    ///
    /// `--plan` 这类开关是 `SetTrue`；`--subagents-workflow-file <path>` 这类是
    /// `Set` + `num_args(1)`（单测直接构造命令验证，不必真的去改进程 argv）。
    fn extension_flag_arg(flag: &CliFlagDef) -> clap::Arg {
        let arg = clap::Arg::new(flag.name)
            .long(flag.name)
            .help(flag.description);

        if flag.takes_value {
            arg.action(ArgAction::Set).num_args(1)
        } else {
            arg.action(ArgAction::SetTrue)
        }
    }

    /// 把一个扩展子命令声明变成 clap 子命令：参数全部原样收下，`-h` 交给扩展自己解释
    /// （各扩展的帮助详略不同，例如 `mcp` 要列出所有子操作）。
    fn extension_subcommand_cmd(sub: &ExtensionSubcommand) -> clap::Command {
        clap::Command::new(sub.name)
            .about(sub.description)
            .disable_help_flag(true)
            .arg(
                clap::Arg::new("args")
                    .num_args(0..)
                    .trailing_var_arg(true)
                    .allow_hyphen_values(true),
            )
    }

    /// 从给定参数列表解析
    pub fn parse_from_args(args: &[String]) -> Result<Args, clap::Error> {
        let mut parsed = Args::try_parse_from(
            std::iter::once(APP_NAME.to_string()).chain(args.iter().cloned()),
        )?;

        parsed.normalize();
        Ok(parsed)
    }

    /// 解析后的归一化：`value_delimiter` 会把 `--models a,` 拆出一个空元素，
    /// 留着一个空 pattern 会让模型 scope 里多出一项什么都匹配不了的条目，故丢弃
    fn normalize(&mut self) {
        self.models.retain(|m| !m.is_empty());
    }

    /// @ 前缀的文件参数
    pub fn file_args(&self) -> Vec<String> {
        self.input
            .iter()
            .filter(|s| s.starts_with('@') && s.len() > 1)
            .map(|s| s[1..].to_string())
            .collect()
    }

    /// 非 @ 前缀的消息文本
    pub fn messages(&self) -> Vec<String> {
        self.input
            .iter()
            .filter(|s| !s.starts_with('@') || s.len() <= 1)
            .cloned()
            .collect()
    }

    /// 合并 --approve / --no-approve 为 Option<bool>
    pub fn approve_option(&self) -> Option<bool> {
        match (self.approve, self.no_approve) {
            (true, _) => Some(true),
            (_, true) => Some(false),
            _ => None,
        }
    }

    /// --export 的输入会话路径
    pub fn export_in(&self) -> Option<&str> {
        self.export.first().map(|s| s.as_str())
    }

    /// --export 的输出路径（可选）
    pub fn export_out(&self) -> Option<&str> {
        self.export.get(1).map(|s| s.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_flags_are_booleans_unless_declared_otherwise() {
        let cmd = Args::command().arg(Args::extension_flag_arg(&CliFlagDef {
            name: "plan",
            description: "plan mode",
            takes_value: false,
        }));
        let m = cmd
            .try_get_matches_from(["prux", "--plan"])
            .expect("bool flag parses");
        assert!(m.get_flag("plan"));

        let cmd = Args::command().arg(Args::extension_flag_arg(&CliFlagDef {
            name: "subagents-workflow-file",
            description: "run a workflow file",
            takes_value: true,
        }));
        let m = cmd
            .try_get_matches_from(["prux", "--subagents-workflow-file", "/tmp/wf.js"])
            .expect("value flag parses");
        assert_eq!(
            m.get_one::<String>("subagents-workflow-file")
                .map(String::as_str),
            Some("/tmp/wf.js")
        );
        // 带值 flag 不给值 → 报错（不是静默当开关）
        let cmd = Args::command().arg(Args::extension_flag_arg(&CliFlagDef {
            name: "wf-file",
            description: "d",
            takes_value: true,
        }));
        assert!(cmd.try_get_matches_from(["prux", "--wf-file"]).is_err());
    }

    fn parse(args: &[&str]) -> Args {
        let v: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        Args::parse_from_args(&v).expect("parse ok")
    }

    #[test]
    fn parses_resource_arrays() {
        let a = parse(&[
            "--skill",
            "a",
            "--skill",
            "b",
            "--prompt-template",
            "p.md",
            "--theme",
            "t.json",
            "--use-theme",
            "t",
        ]);
        assert_eq!(a.skill, vec!["a", "b"]);
        assert_eq!(a.prompt_template, vec!["p.md"]);
        assert_eq!(a.theme, vec!["t.json"]);
        assert_eq!(a.use_theme.as_deref(), Some("t"));
    }

    #[test]
    fn parses_session_id_and_models() {
        let a = parse(&["--session-id", "abc", "--models", "a,b"]);
        assert_eq!(a.session_id.as_deref(), Some("abc"));
        assert_eq!(a.models, vec!["a", "b"]);
    }

    #[test]
    fn models_drops_empty_patterns() {
        // clap 的 value_delimiter 为 "a," 产生 ["a", ""]，空 pattern 必须被丢掉
        assert_eq!(parse(&["--models", "a,"]).models, vec!["a"]);
        assert_eq!(parse(&["--models", "a,,b"]).models, vec!["a", "b"]);
        assert_eq!(parse(&["--models", ","]).models, Vec::<String>::new());
        assert_eq!(parse(&["--models", "a,b"]).models, vec!["a", "b"]);
    }

    #[test]
    fn parses_messages_and_files() {
        let a = parse(&["@prompt.md", "hello", "@img.png", "world"]);
        assert_eq!(a.file_args(), vec!["prompt.md", "img.png"]);
        assert_eq!(a.messages(), vec!["hello", "world"]);
    }

    #[test]
    #[cfg(all(feature = "gui", not(windows)))]
    fn parses_tui_flag() {
        assert!(parse(&["--tui"]).tui);
        assert!(!parse(&[]).tui);
    }

    #[test]
    fn parses_export_two_args() {
        let a = parse(&["--export", "a.json", "out.html"]);
        assert_eq!(a.export_in(), Some("a.json"));
        assert_eq!(a.export_out(), Some("out.html"));
        let b = parse(&["--export", "a.json"]);
        assert_eq!(b.export_in(), Some("a.json"));
        assert_eq!(b.export_out(), None);
    }

    #[test]
    fn parses_list_models_optional_value() {
        let a = parse(&["--list-models"]);
        assert_eq!(a.list_models.as_deref(), Some(""));
        let b = parse(&["--list-models", "claude"]);
        assert_eq!(b.list_models.as_deref(), Some("claude"));
    }

    #[test]
    fn parses_approve_and_no_approve() {
        assert_eq!(parse(&["-a"]).approve_option(), Some(true));
        assert_eq!(parse(&["--no-approve"]).approve_option(), Some(false));
        assert_eq!(parse(&[]).approve_option(), None);
    }

    #[test]
    fn rejects_invalid_thinking_level() {
        let v: Vec<String> = vec!["--thinking".to_string(), "bogus".to_string()];
        assert!(Args::parse_from_args(&v).is_err());
    }

    #[test]
    fn parses_auth_subcommand() {
        let v: Vec<String> = vec!["auth".to_string(), "anthropic".to_string()];
        let a = Args::parse_from_args(&v).expect("auth parse");
        match &a.command {
            Some(Command::Auth { args }) => {
                assert_eq!(args, &vec!["anthropic".to_string()]);
            }
            other => panic!("unexpected: {:?}", other),
        }
    }
}

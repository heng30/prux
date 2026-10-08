//! CLI 参数解析示例（cli::args，clap derive）
//! 运行：cargo run --example cli_demo
//! 演示：@文件分流、消息收集、可选值参数、子命令、旧式短选项兼容。

use prux::cli::args::{Args, Command};

fn parse(label: &str, raw: &[&str]) {
    let v: Vec<String> = raw.iter().map(|s| s.to_string()).collect();
    match Args::parse_from_args(&v) {
        Ok(a) => {
            println!("{label}");
            println!("  files:    {:?}", a.file_args());
            println!("  messages: {:?}", a.messages());
            println!(
                "  provider: {:?}  model: {:?}  thinking: {:?}",
                a.provider, a.model, a.thinking
            );
            println!("  tools: {:?}", a.tools);
            if a.no_tools {
                println!("  no_tools: true");
            }
            if let Some(cmd) = &a.command
                && let Command::Auth { args } = cmd
            {
                println!("  subcommand: auth args={args:?}");
            }
            println!();
        }
        Err(e) => println!("{label}\n  parse failed: {e}\n"),
    }
}

fn main() {
    println!("== clap derive argument parsing demo ==\n");

    parse("@file + message", &["@prompt.md", "refactor this"]);
    parse(
        "provider and thinking level",
        &["--provider", "anthropic", "--model", "claude:high", "hello"],
    );
    parse(
        "legacy multi-char short options",
        &["-nt", "-nc", "-xt", "bash"],
    );
    parse("optional value --list-models", &["--list-models", "claude"]);
    parse("two export values", &["--export", "s.json", "out.html"]);
    parse("subcommand", &["auth", "anthropic"]);
    parse("invalid thinking level", &["--thinking", "bogus"]);
    parse("unknown argument", &["--no-such-flag"]);
}

<p align="center">
  <img src="https://raw.githubusercontent.com/heng30/prux/main/icon.png" alt="prux coding agent" width=30%>
</p>

<p align="center">
  <strong>A minimal terminal coding agent</strong>
</p>

<p align="center">
    <a href="https://github.com/heng30/prux/releases"><img src="https://img.shields.io/github/v/release/heng30/prux" alt="Release"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT.svg" alt="License"></a>
    <a href="Platform"><img src="https://img.shields.io/badge/Platform-Linux%20%7C%20Windows%20%7C%20macOS-blue.svg" alt="Platform"></a>
    <a href="https://doc.rust-lang.org/edition-guide/rust-2024/"><img src="https://img.shields.io/badge/Rust-2024_edition-orange" alt="Rust 2024"></a>
    <a href="Stars"><img src="https://img.shields.io/github/stars/heng30/prux?style=social" alt="Stars"></a>
</p>

<p align="center">
  <a href="README.md">English</a> | <a href="README.zh-CN.md">简体中文</a>
</p>

> ⚠️NOTE: So far only `opencode-go` and `deepseek` have been tested.

## Introduction

`prux` is a minimal terminal coding agent. The core stays small and focused, and its capabilities are extended through extensions, skills, prompt templates, themes, and customizable keybindings. It is a Rust rewrite of [@earendil-works/pi-coding-agent](https://github.com/earendil-works/pi), staying aligned with upstream in behavior and wording for the most part.

<p align="center">
  <img src="https://raw.githubusercontent.com/heng30/prux/main/screenshot/image.gif" alt="image demo" width="45%">
  <img width="2%">
  <img src="https://raw.githubusercontent.com/heng30/prux/main/screenshot/code.gif" alt="code demo" width="45%">
</p>

## Features

- **Interactive TUI**: a full-screen terminal interface with themes and customizable keybindings. Requires stdin/stdout to be a TTY.
- **Built-in tools**: `read` / `bash` / `edit` / `write` / `grep` / `find` / `ls`, covering common coding tasks.
- **Multiple providers**: supports API providers such as Anthropic, OpenAI, and DeepSeek, with custom and virtual model routing.
- **Session management**: session persistence, `/resume` restoration, forking, and tree navigation.
- **Context compaction**: `/compact` compacts long contexts, with branch summaries.
- **Extension system**: implement the `Extension` trait in Rust to provide tools, commands, keybindings, CLI flags, and event hooks.
- **Built-in extensions**: MCP, subagents, codemode (QuickJS sandbox orchestration), skills, scheduled tasks, plan mode, and more.

## Installation and build

Install the released binary from crates.io (requires a Rust toolchain):

```bash
cargo install prux
```

Or build from source:

```bash
cargo build --release
./target/release/prux --version
```

Common commands:

```bash
make          # cargo build --release
make build    # compile only (debug)
make debug    # cargo run
make check    # cargo check
cargo test    # full test suite
cargo clippy  # static checks
```

## Quick start

```bash
cd /path/to/project
prux
```

On the first run, configure a provider with `/login` inside the TUI, or set the environment variable before launching:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
prux
```

Settings are written to `~/.prux/` (the agent directory, overridable with `$PRUX_AGENT_DIR`): credentials in `auth.json`, global settings in `settings.json`, and so on.

## Non-TUI entry points

Besides conversational mode, the following commands do not require a TTY:

- `prux auth`: credential management
- `prux mcp`: MCP server management
- `prux --export`: export a session as HTML
- `prux --list-models`: list available models

## Documentation

The complete documentation is written in Chinese and lives in [`assets/docs/`](assets/docs/). It is embedded at compile time and synced to the agent directory at runtime. Start at [`assets/docs/index.md`](assets/docs/index.md), and see [`assets/docs/quickstart.md`](assets/docs/quickstart.md) to get up and running.

## Contributing from source

Development conventions, module layering, and build details are in [`AGENTS.md`](AGENTS.md). The upstream alignment cadence is recorded in `sync.md` and [`migrations/`](migrations/).

### Troubleshooting

Windows limitations: under `Git Bash` on Windows, prux goes through the legacy console API used by `crossterm`, which implements neither the kitty keyboard protocol nor bracketed paste. `Shift+Enter` therefore cannot be told apart from a plain `Enter` (use `Ctrl+J` to insert a newline), and pasted text arrives as ordinary keystrokes, so a multi-line paste may be submitted line by line. Running inside Windows Terminal (ANSI/VT support) is recommended.

## License

MIT, see [`LICENSE`](LICENSE).

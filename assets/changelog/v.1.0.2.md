## English

### What prux is

`prux` is a minimal terminal coding agent, rewritten in Rust from
[@earendil-works/pi-coding-agent](https://github.com/earendil-works/pi). The core stays small and
focused; capabilities are added through extensions, skills, prompt templates, themes, and
customizable keybindings. It runs as an interactive full-screen TUI and requires stdin/stdout to be
a TTY — there is no RPC/JSON event-stream mode.

### Main features

- **Interactive TUI** — full-screen terminal interface with themes, a footer panel system,
  customizable keybindings, and a `/` command palette with subcommand completion.
- **Built-in tools** — `read` / `bash` / `edit` / `write` / `grep` / `find` / `ls`, covering common
  coding tasks, plus a `tool_search` extension that lets the model discover deferred tools.
- **Multiple providers** — Anthropic, OpenAI, and DeepSeek style API providers, credentials managed
  via `/login` or `auth.json`, with custom providers and virtual model routing on top.
- **Session management** — persistence, `/resume` restoration, forking, tree navigation, and HTML
  export (`prux --export`).
- **Context compaction** — `/compact` for long contexts, with branch summaries.
- **Extension system** — implement the `Extension` trait in Rust (or ship a plugin) to add tools,
  commands, keybindings, CLI flags, event hooks, footer layouts, and banners.
- **Built-in extensions** — MCP client (OAuth 2.1, lazy connections, metadata caching), sub-agents
  with custom agent types, codemode (orchestrate tools from JavaScript in a QuickJS sandbox), skills,
  task tracking, plan mode, goal mode, rewind, notify hooks, hang detection, loop detection, web
  access, a local OpenAI-compatible proxy gateway, and a virtual "plan on strong model, implement on
  cheap model" router.
- **Customization** — Agent Skills, prompt templates, themes, model definitions, settings, and
  keybindings; startup state lives in `~/.prux/` (overridable with `$PRUX_AGENT_DIR`).
- **Non-TUI entry points** — `prux auth`, `prux mcp`, `prux --export <session>`, and
  `prux --list-models` work without a terminal.

### Installation

```bash
cargo build --release
./target/release/prux --version
```

```bash
make          # cargo build --release
make build    # compile only (debug)
make debug    # cargo run
make check    # cargo check
cargo test    # full test suite
cargo clippy  # static checks
```

Set a provider key before the first launch, or configure one with `/login` inside the TUI:

```bash
export ANTHROPIC_API_KEY=sk-ant-...
cd /path/to/project
prux
```

## 中文

### prux 是什么

`prux` 是一个极简的终端编码助手，是 [@earendil-works/pi-coding-agent](https://github.com/earendil-works/pi) 的 Rust 重写。
核心保持小而专注，能力通过扩展、技能、提示模板、主题和可定制键位来叠加。
它以**交互式全屏 TUI** 运行，要求 stdin/stdout 均为 TTY，不提供 RPC / JSON 事件流等非交互模式。

### 主要功能

- **交互式 TUI**：全屏终端界面，支持主题、底栏（footer）布局体系、可定制键位，以及带子命令补全
  的 `/` 命令面板。
- **内置工具**：`read` / `bash` / `edit` / `write` / `grep` / `find` / `ls` 覆盖常见编码任务；另有
  `tool_search` 扩展，让模型按需发现延迟加载的工具。
- **多提供商**：支持 Anthropic、OpenAI、DeepSeek 风格的 API 提供商，凭据经 `/login` 或 `auth.json`
  管理，并可在其上叠加自定义提供商与虚拟模型路由。
- **会话管理**：会话持久化、`/resume` 恢复、分支（fork）、树状导航，以及 HTML 导出
  （`prux --export`）。
- **上下文压缩**：用 `/compact` 压缩长上下文，支持分支摘要。
- **扩展系统**：用 Rust 实现 `Extension` trait（或以插件形式分发），即可提供工具、命令、快捷键、
  CLI flag、事件钩子、底栏布局与欢迎横幅。
- **内置扩展**：MCP 客户端（OAuth 2.1、懒连接、元数据缓存）、自定义 agent 类型的子代理、codemode
  （在 QuickJS 沙箱里用 JavaScript 编排工具）、技能、任务跟踪、plan mode、goal 模式、rewind、
  notify 钩子、卡死检测、死循环检测、网页访问，以及本地 OpenAI 兼容代理网关和「强模型规划、
  廉价模型实现」的虚拟模型路由。
- **定制能力**：Agent Skills、提示模板、主题、模型定义、设置与键位；启动状态存放于 `~/.prux/`
  （可用 `$PRUX_AGENT_DIR` 覆盖）。
- **非 TUI 入口**：`prux auth`、`prux mcp`、`prux --export <session>`、`prux --list-models` 无需终端
  即可使用。

### 安装

```bash
cargo build --release
./target/release/prux --version
```

```bash
make          # cargo build --release
make build    # 仅编译（debug）
make debug    # cargo run
make check    # cargo check
cargo test    # 全量测试
cargo clippy  # 静态检查
```

首次启动前设置提供商密钥，或在 TUI 内用 `/login` 配置：

```bash
export ANTHROPIC_API_KEY=sk-ant-...
cd /path/to/project
prux
```

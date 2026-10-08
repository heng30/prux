# prux 文档

prux 是一个极简的终端编码助手：核心保持小而专注，通过扩展（extensions）、技能（skills）、提示模板（prompt templates）、主题（themes）和可定制的键位来扩展能力。

prux 以**交互式 TUI** 方式运行：启动时要求 stdin/stdout 均为 TTY，否则直接报错退出。它没有 rpc / JSON 事件流等非交互运行模式。

## 快速开始

安装后，在项目目录运行：

```bash
prux
```

认证方式：用 `/login` 配置订阅或 API key 提供商，或在启动前设置环境变量（如 `DEEPSEEK_API_KEY`）。完整首次运行流程见 [快速开始](quickstart.md)。

## 开始使用

- [会话（Sessions）](sessions.md) - 会话管理、分支（fork）、树状导航与 `/resume`、`-c`。
- [设置（Settings）](settings.md) - 全局与项目级设置（`~/.prux/settings.json`）。
- [键位（Keybindings）](keybindings.md) - 默认快捷键与自定义键位（`keybindings.json`）。
- [上下文压缩（Compaction）](compaction.md) - `/compact` 上下文压缩与分支摘要。

## 定制

- [扩展（Extensions）](extensions.md) - 用 Rust 实现的 `Extension` trait，提供工具、命令、快捷键、CLI flag 与事件钩子。
- [虚拟模型（Virtual models）](virtual-models.md) - 扩展注册的虚拟模型：每次请求按需路由到不同物理模型 + thinking 级别。
- [子代理（Sub-agents）](extensions/subagent.md) - `subagent` 扩展：子代理运行环境与隔离、agent 类型与 frontmatter 键、`SubagentWorkflow` 编排、设置键及其关系、界面与定时任务。
- [codemode](extensions/codemode.md) - `codemode` 扩展：在 QuickJS 沙箱里用一段 JavaScript 编排工具调用、脚本 API、首行选项与限制。
- [MCP](extensions/mcp.md) - `mcp` 扩展：接入外部 MCP 服务器的配置发现与字段、`mcp` 工具、各种 `mcp.json` 配置例子、生命周期与缓存、OAuth 2.1、命令。
- [技能（Skills）](skills.md) - Agent Skills，按需复用的能力（`/skill:name`）。
- [提示模板（Prompt templates）](prompt-templates.md) - 由斜杠命令展开的可复用提示。
- [主题（Themes）](themes.md) - 内置与自定义终端主题（`/theme`）。
- [模型（Models）](models.md) - 为受支持的提供商 API 添加模型条目（`/model`）。
- [自定义提供商（Custom providers）](custom-provider.md) - 实现自定义 API 与 OAuth 流程。

## 编程接口

- [SDK](sdk.md) - 以 Rust 库形式嵌入其他应用、构建自定义界面或自动化流水线。
- [TUI 组件（TUI）](tui.md) - 扩展构建自定义终端界面。

## 参考

- [环境变量（Environment variables）](environment-variables.md) - prux 进程配置与会话元数据（对 bash 工具可用）。
- [出站 HTTP 代理（HTTP Proxy）](http-proxy.md) - `HTTP_PROXY` / `NO_PROXY` 直连配置（注意与入站的 [proxy 扩展](extensions.md) 方向相反）。

# 快速开始（Quickstart）

本文带你从启动 prux 到完成第一次有用的会话。

## 安装与启动

prux 是编译成二进制的本地程序，从源码构建后直接运行，不支持 npm 安装或在线更新：

```bash
cd /path/to/prux
cargo build --release
./target/release/prux --version
```

然后在你想让 prux 工作的项目目录启动它：

```bash
cd /path/to/project
prux
```

prux 的对话模式仅支持交互式 TUI，要求 stdin 与 stdout 都是终端（TTY）；在管道、脚本或重定向环境下直接启动对话会报错退出。非 TUI 的入口另有 `prux auth` / `prux mcp` / `--export` / `--list-models`，它们不需要 TTY。

## 认证

prux 通过两种方式获取 API key：

- **方式一：`/login` 子命令式登录**：在 TUI 内运行 `/login`，选择 provider 并输入 API key，key 会写入 `~/.prux/auth.json`（即 agent 目录下的 `auth.json`，agent 目录可用 `$PRUX_AGENT_DIR` 覆盖）。用 `/logout` 可移除已存 key。
- **方式二：环境变量**：在启动前设置对应 provider 的 API key 环境变量，例如：

```bash
export ANTHROPIC_API_KEY=sk-ant-...
prux
```

模型与第三方 provider 的配置见 [models.md](models.md) 与 [custom-provider.md](custom-provider.md)；环境变量见 [environment-variables.md](environment-variables.md)。

## 第一次会话

启动 prux 后，输入请求并按回车：

```text
总结这个仓库，并告诉我如何运行它的检查。
```

默认情况下，prux 给模型四个工具：

- `read` - 读取文件
- `write` - 创建或覆盖文件
- `edit` - 修改文件
- `bash` - 运行 shell 命令

`read` / `write` / `edit` / `bash` 是默认启用的四个内置工具；此外还内置了 `grep`、`find`、`ls`（以及 Windows 上的 `powershell`），默认**不启用**，可用 `-t/--tools`、`--exclude-tools`、`--no-tools`、`--no-builtin-tools` 或 settings 的 `defaultTools` 开关。prux 运行在当前工作目录，可以修改该目录下的文件。如果想方便回滚，建议配合 git 或其他检查点工作流。工具选择详见下文「常用 CLI 选项」。

## 给 prux 项目指令

prux 启动时会加载上下文文件。在项目里添加 `AGENTS.md` 告诉它如何在该项目工作：

```markdown
# 项目指令

- 改完代码后运行 `cargo check`。
- 不要在本地跑生产迁移。
- 保持回复简洁。
```

prux 会加载：

- `~/.prux/AGENTS.md` 全局指令
- 父目录与当前目录的 `AGENTS.md` 或 `CLAUDE.md`；若某目录含 `AGENTS.override.md`，则用它替代该目录下的 `AGENTS.md` / `CLAUDE.md`

项目级上下文文件需要先信任项目才会加载（见 `/trust`）。修改上下文文件后重启 prux，或在 TUI 里运行 `/reload`。

## 常见尝试

### 引用文件

在编辑器里输入 `@` 进行模糊搜索，或在命令行传文件：

```bash
prux @README.md "总结这个"
prux @src/app.ts @src/app.test.ts "一起审查这两个文件"
```

图片或文本可用 Ctrl+V 粘贴（终端支持时），图片也可拖入。

### 运行 shell 命令

在交互模式下：

```text
!npm run lint
```

命令输出会发送给模型。用 `!!command` 运行命令但不把输出加入模型上下文。

### 切换模型

用 `/model` 或 Ctrl+L 为当前会话选择模型。用 `/thinking` 选择当前会话的思考级别，Shift+Tab 循环思考级别，Ctrl+P / Shift+Ctrl+P 循环切换模型。模型与 provider 选择详见 [models.md](models.md)。

### 之后继续

会话会自动保存：

```bash
prux -c                  # 继续最近的会话
prux -r                  # 浏览并挑选历史会话
prux --name "my task"    # 启动时设置会话显示名
prux --session <path|id> # 打开指定会话
```

在 prux 内用 `/resume`、`/new`、`/tree`、`/fork`、`/clone` 管理会话，详见 [sessions.md](sessions.md)。

### 导出会话

用 `/export` 把当前会话导出为 HTML 或 JSONL，或用 CLI 直接导出：

```bash
prux --export <session> [out.html]
```

### 常用 CLI 选项

```text
--model <id>            模型（支持 provider/id 及 :<thinking>）
--provider <id>         provider
--api-key <key>         覆盖环境变量的 API key
--thinking <level>      思考级别：off/minimal/low/medium/high/xhigh/max
--models <a,b,...>      Ctrl+P 循环用的模型列表
--list-models [搜索词]   列出可用模型
-t, --tools <列表>       工具白名单（逗号分隔）
--exclude-tools <列表>   排除指定工具
--no-tools              默认禁用全部工具
--no-builtin-tools      默认禁用内置工具
--no-extensions         本次运行禁用所有扩展（含内置）
--skill <path>          加载 skill（可重复）
--no-skills             关闭 skill 发现与加载
--prompt-template <path> 加载 prompt 模板（可重复）
--no-prompt-templates   关闭 prompt 模板发现
--theme / --use-theme   加载或指定初始主题
--no-themes             关闭主题发现
--offline               关闭启动网络操作（等价 PRUX_OFFLINE=1）
-a, --approve           信任项目资源（跳过信任询问）
--no-approve            不信任项目资源
--export <session> [out] 导出会话为 HTML
--system-prompt <TEXT>  替换默认系统提示
--append-system-prompt <TEXT> 追加到系统提示（可重复）
--verbose               启动时在会话区列出已加载资源（模型/会话/工具/上下文文件/技能/模板/扩展）
--no-context-files      禁用上下文文件发现
-n, --name <NAME>       启动时设置会话显示名
--session-id <ID>       打开/创建指定完整 v7 UUID 的会话
--session-dir <DIR>     会话存储目录
--no-session            临时会话（不落盘）
--fork <path|id>        把指定会话 fork 成新会话
-v, --version           显示版本
```

另有子命令：`prux auth`（打印凭据 / 检查 provider 就绪）与 `prux mcp`（MCP 服务器增删改查与登录，见 [extensions/mcp.md](extensions/mcp.md)）；完整列表用 `prux --help`。

模型相关见 [models.md](models.md)；主题见 [themes.md](themes.md)；skill 见 [skills.md](skills.md)；prompt 模板见 [prompt-templates.md](prompt-templates.md)。

## 下一步

- [index.md](index.md) - 文档总览
- [settings.md](settings.md) - 全局与项目配置
- [keybindings.md](keybindings.md) - 快捷键与自定义
- [models.md](models.md) - 认证与模型配置
- [environment-variables.md](environment-variables.md) - 环境变量
- [tui.md](tui.md) - TUI 界面与斜杠命令
- [sessions.md](sessions.md) - 会话管理
- [compaction.md](compaction.md) - 上下文压缩
- [extensions.md](extensions.md) - 扩展系统
- [skills.md](skills.md) - skills
- [prompt-templates.md](prompt-templates.md) - prompt 模板
- [themes.md](themes.md) - 主题
- [sdk.md](sdk.md) - Rust 库形式编程接口

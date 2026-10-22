<p align="center">
  <img src="https://raw.githubusercontent.com/heng30/prux/main/icon.png" alt="prux coding agent" width=30%>
</p>

<p align="center">
  <strong>极简的终端编码助手</strong>
</p>

<p align="center">
    <a href="https://github.com/heng30/prux/releases"><img src="https://img.shields.io/github/v/release/heng30/prux" alt="Release"></a>
    <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT.svg" alt="License"></a>
    <a href="Platform"><img src="https://img.shields.io/badge/Platform-Linux%20%7C%20Windows%20%7C%20macOS-blue.svg" alt="Platform"></a>
    <a href="https://doc.rust-lang.org/edition-guide/rust-2024/"><img src="https://img.shields.io/badge/Rust-2024_edition-orange" alt="Rust 2024"></a>
    <a href="Stars"><img src="https://img.shields.io/github/stars/heng30/prux?style=social" alt="Stars"></a>
</p>

<p align="center">
  <a href="README.md">English</a> | <a href="README.zh-CN.md">简体中文</a> | <a href="https://www.bilibili.com/video/BV1usp46nECj">介绍视频</a>
</p>

> ⚠️注意：目前仅测试了`opencode-go`和`deepseek`。

## 简介

`prux` 是一个极简的终端编码助手。核心保持小而专注，通过扩展、技能、提示模板、主题和可定制键位来扩展能力。它是 [@earendil-works/pi-coding-agent](https://github.com/earendil-works/pi) 的 Rust 重写，大部分行为和文案上与上游保持一致。

<p align="center">
  <img src="https://raw.githubusercontent.com/heng30/prux/main/screenshot/image.gif" alt="image demo" width="45%">
  <img width="2%">
  <img src="https://raw.githubusercontent.com/heng30/prux/main/screenshot/code.gif" alt="code demo" width="45%">
</p>

## 特性

- **交互式 TUI**：全屏终端界面，支持主题、快捷键自定义。要求 stdin/stdout 均为 TTY。
- **内置工具**：`read` / `bash` / `edit` / `write` / `grep` / `find` / `ls`，覆盖常见编码任务。
- **多提供商**：支持 Anthropic、OpenAI、DeepSeek 等 API 提供商，可自定义与虚拟模型路由。
- **会话管理**：会话保存、`/resume` 恢复、分支（fork）与树状导航。
- **上下文压缩**：`/compact` 压缩长上下文，支持分支摘要。
- **扩展系统**：用 Rust 实现 `Extension` trait，提供工具、命令、快捷键、CLI flag 与事件钩子。
- **内置扩展**：MCP、子代理（subagent）、codemode（QuickJS 沙箱编排）、技能、定时任务、plan mode 等。

## 安装与构建

从 crates.io 安装发布版二进制（需要 Rust 工具链）：

```bash
cargo install prux
```

或从源码构建：

```bash
cargo build --release
./target/release/prux --version
```

常用命令：

```bash
make          # cargo build --release
make build    # 仅编译（debug）
make debug    # cargo run
make check    # cargo check
cargo test    # 全量测试
cargo clippy  # 静态检查
```

## 快速开始

```bash
cd /path/to/project
prux
```

首次运行需要在 TUI 内用 `/login` 配置提供商，或在启动前设置环境变量：

```bash
export ANTHROPIC_API_KEY=sk-ant-...
prux
```

设置写入 `~/.prux/`（agent 目录，可用 `$PRUX_AGENT_DIR` 覆盖）：认证 `auth.json`、全局设置 `settings.json` 等。

## 非 TUI 入口

除对话模式外，以下命令不需要 TTY：

- `prux auth`：认证管理
- `prux mcp`：MCP 服务器管理
- `prux --export`：导出会话为 HTML
- `prux --list-models`：列出可用模型

## 文档

完整中文文档位于 [`assets/docs/`](assets/docs/)，编译期内嵌并在运行时同步到 agent 目录。入口见 [`assets/docs/index.md`](assets/docs/index.md)，快速上手见 [`assets/docs/quickstart.md`](assets/docs/quickstart.md)。

## 从源码参与贡献

开发约定、模块分层与构建细节见 [`AGENTS.md`](AGENTS.md)。上游对齐节奏记录在 `sync.md` 与 [`migrations/`](migrations/)。

### 排错

Windows 平台限制：在 Windows 的 `Git Bash` 上经由 `crossterm` 使用的 legacy 控制台 API 运行，该 API 既不支持 kitty 键盘协议，也不支持 bracketed paste。因此 `Shift+Enter` 无法与普通 `Enter` 区分（换行请用 `Ctrl+J`），粘贴的文本会作为普通按键输入，多行粘贴可能被逐行提交。建议在 Windows `Terminal`（启用 ANSI/VT）中使用。

`computer` 扩展（操作真实桌面）的平台限制：

- **Linux 仅支持 Wayland，不支持 X11**：后端按 Wayland 会话实现，没有 `WAYLAND_DISPLAY` 时所有 `computer_*` 工具都报“没有可用后端”。
- **输入要合成器支持虚拟输入协议**：sway / Hyprland / river / wayfire 等 wlroots 系可以；GNOME 与 KDE 不实现 `zwp_virtual_keyboard_v1` / `zwlr_virtual_pointer_v1`，点击与打字会失败并说明原因。
- **截图需要 `grim` 或可用的 `xcap`**（`grim` 在 PATH 上时优先用它）。
- **窗口列表需要 `swaymsg`**（sway/wlroots 的 IPC 树）：没有它时 `computer_windows` 报错，其余工具照常可用。窗口截图截的是“该窗口矩形那块屏幕”，被遮挡的部分截到的是遮挡物；`minimize` 直接报错，因为 Wayland 把可见性交给合成器。
- **指针位置不可知**：Wayland 不暴露全局指针坐标，截图回执里的 `cursor: unknown` 是正常的。
- **无障碍取决于应用**：需要 `at-spi2-core` 在跑，GTK/Qt 应用通常可用，Electron 应用需要 `--force-renderer-accessibility`。
- **Windows**：截图与窗口枚举走 `xcap`，无障碍走 UI Automation（无需额外授权）。高 DPI 缩放会让图内像素与屏幕像素错位，按坐标点击时优先用截图返回的 `from_frame`。
- **macOS**：截图需要「屏幕录制」授权，输入与无障碍需要「辅助功能」授权。窗口动作（关窗 / 最小化 / 最大化）用 ⌘ W / ⌘ M / ⌃⌘ F 模拟，不是系统 API，会被应用自己的快捷键设置影响。
- **Windows 与 macOS 上 `select_text` 与 `get_caret` 尚未接线**：前者明确报错，后者报告“不提供光标位置”。
- **子代理不能调用 `computer_*`**：真实鼠标键盘不能靠“派个子代理”绕过权限。另外该扩展只在 Dev / Creator / 全部模式下出现在 `/extension` 面板，极简模式里看不到它。

## 许可证

MIT，见 [`LICENSE`](LICENSE)。

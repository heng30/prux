# 交互终端界面（TUI）

prux 以全屏终端界面（TUI）运行，用于读写会话、执行斜杠命令、切换模型与主题。本文按 prux 实际实现说明屏幕布局、各区域组件与交互方式。

## 启动

运行 `prux` 即进入 TUI。项目首次运行时若未信任，会先显示信任确认（见[设置](settings.md)）。TUI 中的键盘动作都可通过 `keybindings.json` 自定义（扩展自己注册的键位除外，它们不进配置表），完整动作表见[键位绑定](keybindings.md)。

## 屏幕布局

垂直方向自上而下分为五个区域：

```
┌─────────────────────────────────────────────┐
│  消息区（历史 + 流式输出，占满剩余高度）      │
│  ┌─ user ──┐  ┌─ assistant ──┐              │
│  └─────────┘  └──────────────┘              │
│  待发送区（有排队消息时才出现）              │
│  状态栏（1 行：忙碌 spinner / 临时提示）      │
│  ─── 输入框上边框 ───                        │
│  输入框（多行编辑器 + 候选列表）             │
│  ─── 输入框下边框 ───                        │
│  底栏（footer 扩展渲染，无扩展时不显示）      │
└─────────────────────────────────────────────┘
```

- **消息区**：占满剩余高度，右侧有滚动条。
- **待发送区**：存在排队中的 follow-up / steer 消息时占行，否则不占布局。
- **状态栏**：固定 1 行，见[状态栏](#状态栏与忙碌指示)。
- **输入区**：编辑会话输入；激活选择器/面板时被其占据。
- **底栏**：由 footer 扩展渲染（如显示模型、token 统计），无扩展注册时不显示。

## 消息区

消息区按时间线渲染会话历史，分为几种块：

- **用户消息**：带背景色块的 Markdown。
- **助手消息**：Markdown 正文，无背景。
- **思考块（thinking）**：斜体灰字，可折叠；折叠时显示 `Thinking...`，用 `app.thinking.toggle`（`ctrl+t`）显示/隐藏。
- **工具调用块**：进行中/成功/失败分别染色，下方展示工具输出与耗时；`toolResult` 就地更新对应块。
- **摘要块**：`[compaction]` / `[branch]` 压缩摘要。
- **技能调用块**：折叠显示 `[skill] name (Ctrl+O/Alt+click to expand)`。
- **系统消息**：带时间戳与级别（Info/Success/Warning/Error），不进入 LLM 上下文。

**折叠与展开**：`app.tools.expand`（`ctrl+o`）或在输入为空时按 `tab` 切换「全部展开」，展开后显示工具/技能的详细内容；
全局开关会**覆盖**所有非 thinking 折叠块（丢弃它们的逐块状态），thinking 由 `app.thinking.toggle`（`ctrl+t`）单独控制。
**Alt+左键**点击任一折叠块（摘要 / 技能调用 / thinking run / 工具输出）可单独展开或折叠该块；裸左键仍用于文本选择。

**滚动**：`shift+home` 滚到顶、`shift+end` 滚到底（跟随最新）、`shift+pageUp` / `shift+pageDown` 按视口高度翻页；滚轮滚动消息区（指针在消息区内才生效）。

## 输入框

多行编辑器，支持光标移动、词级移动、历史浏览、kill-ring（`ctrl+w/u/k` 删除、`ctrl+y` 粘贴最近一条、`alt+y` 循环更旧的条目）、fish 风格撤销（`ctrl+-`）。详细信息见[键位绑定](keybindings.md)。

- **换行 / 提交**：`shift+enter`（`ctrl+j`）换行，`enter` 提交。编辑器首行以 `!` 开头时为 bash 模式，边框染 `bashMode` 主题色，提交后直接执行 shell 命令；`!!command` 同样执行但输出不加入模型上下文。
- **粘贴**：`ctrl+v` 粘贴文本；大段文本（超过 10 行或 1000 字符）折叠为 `[paste #N ...]` marker，提交时展开，避免撑满输入框。
- **外部编辑器**：`ctrl+g` 用外部编辑器打开当前输入；命令取 `settings.externalEditor`，未设置时依次回退 `$VISUAL`、`$EDITOR`、`vi`。
- **候选列表**：输入 `/` 弹出斜杠命令候选，输入 `@` 弹出文件候选；带子命令的命令（如 `/goal`）在命令名后敲空格再弹出子命令候选（空格后的第一个词在面板内过滤，出现第二个空白即关闭）。三种候选都显示在输入框边框下方（选中项居中，底部附页码与移动提示）。`tab` 只补全；`enter` 在命令/子命令候选上补全并执行，在 `@` 文件候选上只补全不提交。候选最大展示行数可在 `/settings` 调整。
- **历史浏览**：上/下方向键在历史间环形浏览（草稿 ↔ 最新 ↔ … ↔ 最早 ↔ 草稿），回到草稿时恢复进入浏览前的文本与光标。

## 斜杠命令

输入 `/` 后按 `tab` 或从候选列表中选择。未知命令不报错，作为普通用户输入处理（技能/模板展开）。命令如下（内置命令由 prux 静态注册，`download` / `update` 等由已启用扩展提供），其中标注「面板」的在输入区打开选择器：

| 命令 | 说明 |
| `quit` / `q` | 退出 TUI |
| `compact [instructions]` | 压缩上下文；可带自定义摘要指令 |
| `copy` | 复制最后一条助手消息到剪贴板 |
| `new` | 新建会话 |
| `hotkeys` | 打印键位绑定摘要 |
| `thinking [level]` | 设置思考级别（无参数打开面板；级别：off/minimal/low/medium/high/xhigh/max） |
| `session [index]` | 显示当前会话信息，或按序号切换会话 |
| `changelog` | 显示当前版本的更新日志（编译期内嵌，非会话内容） |
| `resume` | 打开会话选择器（面板） |
| `history [term]` | 打开 prompt 历史搜索面板（实时过滤；`@clear` / `@clear-all` 删除历史） |
| `tree` | 显示会话树（只读文本视图，无交互式导航） |
| `label <entryId> [label]` / `name [name]` | 设置树节点标签 / 重命名会话 |
| `fork` / `clone` | 派生 / 复制当前会话 |
| `share` | 分享会话（导出 + gist 上传） |
| `export [out]` | 导出会话（`.html` / `.jsonl`） |
| `theme [name]` | 切换主题（无参数打开面板） |
| `reload` | 重载键位、扩展、技能、提示模板、主题与上下文文件 |
| `settings` | 打开设置选择器（面板） |
| `model [id]` | 选择模型（无参数打开面板） |
| `scoped-models` | 启用/禁用 `Ctrl+P` 循环名单里的模型（面板） |
| `import <path.jsonl>` | 导入会话 |
| `login` / `logout` | 存储 / 移除 provider 的 API key |
| `extension` | 启用 / 禁用扩展（面板） |
| `trust` | 信任当前项目 |
| `bug [description]` | 报告 bug：收集脱敏环境/模型/扩展/设置与会话诊断，可选附带 transcript 或模型摘要，导出 zip 到当前目录 |
| `debug` | 显示运行状态汇总（不在 `/` 候选列表中，需手输） |
| `dock` | 切换停靠面板（状态栏上方显示扩展内容，如 `/todos`） |
| `download [fd\|rg]` | 下载/安装外部工具（`fd` / `rg`）；无参数切换 downloader 的 dock 段显隐（面板未开时一并打开面板）。启动只检测并提示缺失，不自动下载。由 `downloader` 扩展提供 |
| `update check` / `update open` | `check` 异步检查 GitHub 最新 release，有新版本时在聊天区输出当前版本、新版本与下载地址（便于复制）；`open` 直接用浏览器打开最新 release 下载页（不比较版本）。由 `update-check` 扩展提供 |

扩展可以注册自己的斜杠命令与快捷键；`reload` 会应用其启用状态。

## 选择面板

`/model`、`/theme`、`/thinking`、`/login`、`/extension`、`/resume` 与 `/settings` 都在输入区打开模态选择器（键盘独占，多级菜单用栈管理）。通用交互：

- 顶部为可搜索的过滤框（模糊匹配），可 `ctrl+v` 粘贴。
- `up` / `down`（及扩展导航键 `ctrl+j/k/g/d/u`）移动选中，`pageUp` / `pageDown` 翻页。
- `enter` 确认，`escape` / `ctrl+c` 取消（多级菜单逐级返回）。
- 列表项下方显示描述，右侧附页码。

各面板特例：

- **模型面板（`/model`）**：按 provider 分组，当前使用的模型置顶，`tab` 在 all / scoped 之间切换；选中后经 worker 切换模型并回执刷新 footer 徽标。
- **主题面板（`/theme`）**：列出可用主题，当前主题置顶，选中即切换并持久化到 settings（见[主题](themes.md)）。
- **思考面板（`/thinking`）**：选择思考级别。
- **登录面板（`/login`）**：认证方式（账号 / API key）→ provider → 输入 key 或 OAuth（OAuth 面板中 `ctrl+click` 授权 URL 用浏览器打开，URL 行即手动粘贴输入框）。
- **扩展面板（`/extension`）**：checkbox 列表，`space` 切换启用，`tab` 切换扩展模式，`enter` 进入详情。
- **会话选择器（`/resume`）**：搜索过滤会话，`ctrl+n` 切换命名过滤、`ctrl+p` 切换路径、`ctrl+s` 循环排序、`ctrl+r` 重命名、`ctrl+d` 删除（二次确认）。

## 设置选择器（/settings）

`/settings` 打开设置选择器，可搜索。`enter` / `space` 循环切换值，含：Cache warming、Autocomplete 最大展示行数、History 条数、Wheel 行数、Auto-compact、Retry、steering 模式、follow-up 模式、Hide thinking、语法高亮、mermaid、latex、图片相关项、默认项目信任方式等；theme 与 thinking 级别只读展示（改由 `/theme`、`/thinking`）。改动写盘，见[设置](settings.md)。

## 状态栏与忙碌指示

消息区与输入框之间的 1 行：

- **忙碌**：braille/线条 spinner 帧 + 状态文本（`Working...` / `Compacting...` / `Sharing session...` 等），帧推进 80ms。网络重试不在状态栏显示 `Retrying...`，进度只在输出区提示行呈现。
- **临时提示**：`ctrl+x` 复制、OAuth 等一次性反馈以 muted 单行显示，2 秒后消失。
- **信任确认**：项目未信任时以 warning 色逐行显示确认。

## 停靠面板（dock）

状态栏上方的可滚动面板，用于常显扩展状态（如 plan-mode 的 todo 进度）。`/dock` 切换显隐；面板打开时若没有任何扩展提供内容则提示。

内容由已启用扩展经 `dock_lines()` 提供（含扩展自带的标题行），所有提供非空内容的扩展按注册顺序平铺、段间空行分割，整体一个滚动视口（上限 14 行，超出由鼠标滚轮滚动）。空内容自动隐藏。`downloader` 扩展提供「Downloader」段（`fd` / `rg` 的状态点 + 版本 + 位置），但**该段默认隐藏**且只在下载时自动显示，因此面板默认仍为空；裸 `/download` 切换本段显隐（面板未开时一并打开面板），`/dock` 切换整个面板。

- **`/todos`**：在面板中显示 / 隐藏 plan 段，面板未显示时一并打开；跨会话默认收起。
- **`/download`**：在面板中显示 / 隐藏 downloader 段（**默认隐藏**），面板未显示时一并打开面板；与 `/todos` 同一套“段可见性”语义。
- **`/dock`**：手动切换面板开关；无任何扩展段可见时提示（downloader 段被 `/download` 隐藏且无其它段时就会为空）。
- **自动显示**：扩展在"有内容要展示"时经 `ShowDock` 请求打开面板（`plan-mode` 进入执行、`goal` 激活、子代理派发、工作流开跑），无需手动 `/dock`；同一时刻只排一条请求。
- **自动隐藏**：子代理/工作流是"这一轮的事"——它们全部结束后，用户下一次提交输入（空闲 Enter、忙碌 steer、Alt+Enter follow-up）会把已完成的代理记录从面板隐去；面板随之自动隐藏，不必手动 `/dock`。仍有在跑的代理/工作流时不隐去（用户还在等结果）。隐去只是不再占位，记录与完成通知卡片照旧可查（`/agents`、`@handle` resume）。
- 自定义内容提供方式见[扩展](extensions.md)。

## 底栏（footer）

底栏由 footer 扩展渲染，可显示当前模型、thinking 级别、git 分支、上下文使用率、token 统计等。无扩展注册时整行不显示、不占布局。自定义方式见[扩展](extensions.md)。

## 鼠标交互

- 滚轮滚动消息区——**只在指针位于消息区内时生效**：指针在输入框、状态栏、底栏上时滚轮不带动输出区（停靠面板内滚面板，覆盖层内滚覆盖层）。
- 左键拖动在消息区扩展选择文本；拖动到视口边缘自动跨屏滚动。
- 中键：把选中文本复制到系统剪贴板并粘贴进输入框；无选中时连续粘贴同一内容。
- `ctrl+click`：在 OAuth 登录面板中打开授权 URL / 验证 URI。
- `ctrl+click`：在消息区打开 markdown 链接（`[文本](url)`、`<url>`）。支持 OSC8 超链接的终端
  （kitty / ghostty / wezterm / iTerm2 / VS Code 终端 / Windows Terminal 等）也可直接用
  `cmd/ctrl+click` 由终端打开；两种方式互补，不依赖终端能力。
- `alt+click`：折叠/展开点击到的块（thinking / 工具调用 / 技能调用 / 摘要）。
- 点击滚动条或拖动其滑块跳转。

## 退出与中断

- `escape` 中断当前任务（`app.interrupt`）。
- `ctrl+c` 清空输入；500ms 内第二次 `ctrl+c` 退出。
- 输入框为空时 `ctrl+d` 退出，`ctrl+z` 挂起到后台。
- `/quit` 直接退出。

## 说明

- 未实现 fullscreen 模式，`tui.altScreen.*` 动作不列入。
- 选择器/面板在输入区渲染；`/agents` 等少数界面走独立的覆盖层渲染路径。
- `/reload` 会重读 `keybindings.json` 并即时生效，无需重启。
- tree / scoped-models 选择器动作（`app.tree.*`、`app.models.*`）未实现。

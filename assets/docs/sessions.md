# 会话（Sessions）

prux 把对话保存为会话（session），这样你可以继续之前的工作、从更早的轮次分支（fork/clone），或回头查看之前走过的路径。每个会话是一个 JSONL 文件，内部以树形结构组织。

## 会话存储

会话默认自动保存到 `~/.prux/sessions/`（agent 目录，见 [environment-variables.md](environment-variables.md) 的 `PRUX_AGENT_DIR`），并按工作目录分子目录存放。会话子目录名由工作目录路径编码而来：去掉开头的 `/`，把 `/` 和 `:` 替换为 `-`，再包上 `--…--`：

```bash
~/proj            →  ~/.prux/sessions/--home-you-proj--
/data/Code/app    →  ~/.prux/sessions/--data-Code-app--
```

每个会话是一个独立文件，文件名形如 `<ISO时间戳>Z_<uuid7>.jsonl`（时间戳里 `:` 与 `.` 已被换成 `-`，例如 `2025-01-01T10-00-00-000Z_0193f1b2-…jsonl`）。会话存储目录可按以下优先级覆盖：

1. `--session-dir <DIR>`（命令行最高优先）
2. `$PRUX_CODING_AGENT_SESSION_DIR`
3. 默认派生的 `<agent_dir>/sessions/--<cwd>--`

`--session-dir` 与环境变量指向的是会话文件的**直接存放目录**（不再按 cwd 嵌套）。

常用启动参数：

```bash
prux -c                    # 继续最近一次会话
prux -r                    # 启动时浏览并选择历史会话
prux --no-session          # 临时会话模式，不落盘
prux -n "my task"          # 启动时为会话设置显示名
prux --session <path|id>   # 打开指定会话文件或部分 UUID
prux --session-id <id>     # 打开/创建指定完整 v7 UUID 的项目会话
prux --fork <path|id>      # 把某会话文件/部分 UUID fork 成新会话
prux --export <sess> [out] # 把会话导出为 HTML
```

交互模式下用 `/session` 查看当前会话信息：会话名、文件路径、会话 ID、消息总数、工具调用数、token（输入/输出/总）与成本。

## 会话文件格式

会话文件是 JSONL（每行一个 JSON 对象）。首行是 header，之后每行是一条 mutation（会话变更记录）：

```jsonl
{"kind":"header","version":4,"id":"0193f1b2-…","cwd":"/home/you/proj","createdAt":1720000000000}
{"kind":"entry","lane":"main","type":"message","id":"0193f1b3-…","seq":1,"timestamp":1720000000000,"message":{"role":"user","content":[{"type":"text","text":"hi"}]}}
{"kind":"lane","lane":"main","leafId":"0193f1b3-…","seq":2}
{"kind":"fact","fact":"name","name":"my session","seq":3}
```

四种行：`entry`（内容）、`record`（运行记录）、`lane`（只改叶子指针）、`fact`（元数据）。根条目的 `parentId` 会被省略（不写 `null`）。

- **header**：`kind=header`、`version=4`，记录会话 `id`、`cwd`、`createdAt`；fork 出的会话还会带 `parentSessionId` 指向源会话。
- **entry**：消息（`type=message`）、模型切换（`model_change`）、思考级别（`thinking_level_change`）、活动工具（`active_tools_change`）、压缩（`compaction`）、分支摘要（`branch_summary`）、自定义条目（`custom`）、上下文编辑（`context_edit`）等。每个 entry 有 `id`/`seq`/`timestamp`/`parentId`（根节点省略该键），靠 `parentId` 形成树；`main` lane 的 `leaf` 指针标记当前活动位置。
- **context_edit**（append-only 上下文编辑）：`{"type":"context_edit","targetId":"<条目 id>","replacement":null}` 表示在**模型上下文**中省略该条目，`replacement={"content":[…块…]}` 表示只替换其内容；**不改写历史**，原始 transcript 仍可被 UI / `/bug` / fork 完整看到。恢复重试、溢出恢复会自动为被放弃的 attempt 追加省略；扩展也可在 `turn_end` / `agent_before_settle` 边界追加（含 retain-none 压缩：只留摘要、不保留前置条目）。
- **fact**：会话名（`fact=name`）与标签（`fact=label`）等元数据，不进入消息树。
- **record**：记录运行/步骤的 lane record，供 `/session` 统计与审计，不参与会话树。

恢复会话时，prux 沿 `parentId` 链从当前 leaf 回溯构建上下文消息；压缩后的上下文以「压缩摘要 + 保留尾部」开头。

## 交互命令

| 命令 | 说明 |
|------|------|
| `/resume` | 打开会话选择器，浏览/切换历史会话（也支持 `/session <序号>` 直接切换） |
| `/session` | 显示当前会话信息（文件、ID、消息数、token、成本） |
| `/session <n>` | 按序号切换会话（序号 1..N，按最近修改排序） |
| `/new` | 开始全新会话 |
| `/name <name>` | 设置当前会话显示名 |
| `/tree` | 以文本形式把当前会话树打印到消息区（节点含类型、标签与 ID） |
| `/label <entryId> [label]` | 给指定条目设置/清除标签 |
| `/fork` | 把当前会话整体 fork 成新会话 |
| `/clone` | 把当前活动分支复制为新的会话 |
| `/compact [instructions]` | 压缩较早上下文；详见 [compaction.md](compaction.md) |
| `/export [file]` | 把当前会话导出为 HTML |
| `/share` | 把当前会话导出为 HTML 并以私有 GitHub gist 上传分享 |
| `/import <path>` | 导入一个会话 JSONL 文件 |
| `/bug [description]` | 导出 bug 报告 zip（`report.json` / `diagnostics.json` / 可选 `session.jsonl`、`summary.md`）到当前目录 |

## 崩溃记录（crashes.json）

未捕获的 panic 会追加一条记录到 `~/.prux/crashes.json`（最多保留 5 条）。下次启动时若存在
7 天内的未提示记录，会在消息区提醒一次并指向 `/bug`；`/bug` 导出时会把这些崩溃记录写进
`diagnostics.json`，成功导出后清空文件。

## 恢复与删除会话（/resume）

`/resume` 打开会话选择器；`prux -r` 在启动时也会打开选择器（非交互/仅一个会话时直接取最近那个，否则用简化的 ↑/↓、j/k、Enter、Esc 选择）。选择器特性：

- 输入即搜索：支持 `re:<正则>`、`"精确短语"` 与空白分隔的 fuzzy 匹配；`Ctrl+V`/`Ctrl+Shift+V` 粘贴到搜索框
- `Tab`：切换范围（当前项目 ↔ 全部项目）
- `Ctrl+P`：切换路径显示
- `Ctrl+S`：循环排序（threaded → recent → relevance）
- `Ctrl+N`：只显示已命名会话
- `Ctrl+R`：重命名选中会话
- `Ctrl+D`：删除选中会话，回车确认（优先用 `trash` CLI，失败则直接 `unlink`）
- `Ctrl+Backspace`：查询为空时直接进入删除确认

选择器会显示当前打开的会话，并在左侧用 `✓` 标记（已打开的会话不可选中，方向键会自动跳过）；空会话（无用户消息）不显示（当前会话除外）；fork 出的会话按其 `parentSessionId` 在列表中以树形缩进显示（默认 threaded 排序）。打开时默认选中**第一个非当前会话**（渐进加载/树重排期间持续跟随，用户首次导航后不再自动跳转）。方向键选择（`Home`/`End` 跳首/末项，`Ctrl+G` 跳到当前打开会话的下一条会话）、`Enter` 恢复、`Esc`/`Ctrl+C` 取消。相关键位可在 [keybindings.md](keybindings.md) 的 `app.session.*` 中自定义。

## 命名会话

用 `/name <name>` 给会话设置人类可读的名称：

```text
/name Refactor auth module
```

启动时用 `-n` / `--name` 设置：

```bash
prux --name "Refactor auth module"
prux --name "CI audit" "Review this build failure"
```

已命名会话在 `/resume` 与 `prux -r` 里更容易找到（可用 `Ctrl+N` 只看命名会话）。会话名以 `fact=name` 形式持久化在会话文件内。

## /tree

`/tree` 把当前会话树以文本打印到消息区：每个节点标注类型（`msg/user`、`msg/assistant`、`model`、`compact`、`thinking` 等）、标签（若有）与条目 ID，最多展示 40 个节点。

prux 的 `/tree` 是**只读的树形文本视图**：它展示会话结构，不提供交互式树导航。要给条目打标签请用 `/label`。若想在会话内部继续不同路径，用 `/fork` 或 `/clone` 复制出新的会话文件。

## /fork 与 /clone

| 特性 | `/fork` | `/clone` |
|------|---------|----------|
| 输出 | 新会话文件 | 新会话文件 |
| 复制范围 | 整个会话树（含所有分支、标签、会话名） | 当前活动分支（从 leaf 回溯到 root） |
| 典型用途 | 从一个旧会话整体出发做新工作 | 复制当前进度，继续前先留个备份 |

两个命令都会新建独立会话文件；新会话的 header 记录 `parentSessionId` 指向源会话，因此 `/resume` 列表里能看到 fork 出的会话挂在其父会话之下。分支的 lane record（运行/步骤记录）不会被复制。

命令行等价操作：

```bash
prux --fork <path|id>     # fork 一个会话文件/部分 UUID 到新会话
```

`--fork`/`--session` 接受文件路径或部分 UUID。`--session` 在当前项目目录找不到时会全局搜索其它项目，命中则询问是否把它 fork 进当前目录（非交互环境默认拒绝）；`--fork` 只在当前会话目录内查找。`--session-id` 接受完整 v7 UUID，未命中时用该 ID 新建会话。注意 `--fork` 不能与 `--no-session`/`--session`/`--resume`/`--continue` 同时使用，`--session-id` 也不能与 `--session`/`--resume`/`--continue` 同时使用。

## 会话损坏与修复

会话文件损坏时 prux 会尽量恢复而不是静默丢数据：

- 末尾半截 JSON（torn tail）自动截断并保留此前有效内容。
- 中间某行损坏但可跳过（如重复 ID、垃圾行），提供「抢救」选项：跳过损坏行、重排 seq、保留后缀。
- 引用断裂等不可救的损坏，提供「截断」选项：只保留首个损坏行之前的有效前缀。

交互模式下，打开/继续一个中间行损坏的会话会在 TUI 弹出修复确认面板，选择抢救或截断；两种修复在写回前都会先把原文件备份为 `.jsonl.bak`。不可修复的损坏（header 缺失/版本不符等）则直接报错。

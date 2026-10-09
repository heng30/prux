# 设置（settings.json）

prux 的配置是单个 JSON 文件。

配置文件位于 agent 目录：

- Linux/macOS：`~/.prux/settings.json`（或 `$PRUX_AGENT_DIR/settings.json`）

交互式修改常用项用 `/settings`。切换模型（`/model`、`Ctrl+P`）会自动写回 `defaultProvider`/`defaultModel`，切换思考级别（`/thinking`、`Shift+Tab`）会自动写回 `defaultThinkingLevel`，切换主题（`/theme`）会写回 `theme`，`/extension` 面板会写回 `disabledExtensions`/`enabledExtensions`/`extensionMode`——重启后这些选择都会恢复，无需手动编辑。

手动编辑 `settings.json` 后：`/reload` 会重读键位、扩展、技能、提示模板、主题、上下文文件，以及 `hideThinkingBlock`、`autocompleteMaxVisible`、`fullscreenWheelScrollLines`、`historyMaxEntries` 四项设置；其余设置在启动或每回合读取，修改需重启 prux。带 BOM 的 JSON 可正常解析；文件损坏时启动会打印警告（不会静默忽略），且不会静默回写覆盖：读取会回退到最近一次成功读取的配置，并尽力从损坏原文恢复完整的顶层键值对（破坏点之后残缺的条目除外）；交互模式下首次写盘会弹确认窗，选择 `Overwrite` 则以「已恢复的配置 + 本次改动」重写（不会只剩本次改动的键），选择 `Cancel` 则保留原文件、不写入；非交互模式则直接拒绝覆写并报错。修复或删除损坏文件后写入即恢复（删除视为重置），未知/新增键在写入时原样保留。

## 项目级设置与信任

prux 只有全局一个配置入口，项目级 `.prux/settings.json` 用于覆盖 `defaultTools` 与扩展启停（`disabledExtensions` / `enabledExtensions`，见 [extensions.md](extensions.md)），不参与其它设置的合并。

目录含 `.prux/settings.json`、`.prux` 资源、`.agents/skills` 或 `SYSTEM.md` 等项目资源且无已保存决策时，交互启动会询问是否信任该项目。信任决策写入 agent 目录的 `trust.json`（路径 → true/false），`/trust` 可保存当前项目（含父目录）的决策，或清空 `trust.json` 全部已保存决策。没有可用决策时，回退到全局 `settings.json` 的 `defaultProjectTrust`；`--approve`/`-a` 或 `--no-approve` 可单次覆盖。信任后才加载项目级 `settings.json`（`defaultTools`、`disabledExtensions`/`enabledExtensions`）、skills、context 文件等资源；未信任时项目级 `settings.json` 直接忽略。

启动解析出的信任（含 `-a`/`--no-approve`）是**本次运行**的最终结论：后续项目资源门控（项目级 skills/context 文件、子代理的 agents/workflows/agent-memory/config）都以它为准，运行期间不再重读 `trust.json`。它与面板的「仅本次」选项一样只作用于当前进程；「仅本次」不落盘，`/trust` 只落盘、本次不生效（重启 prux 后生效）。

## 所有设置

### 模型与思考

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `defaultProvider` | string | - | 启动 provider（如 `"deepseek"`、`"anthropic"`、`"openai"`；Ctrl+P 切换或 `/model` 选择后自动写回）。provider 列表见 [models.md](models.md) |
| `defaultModel` | string | - | 启动模型 ID（同上自动写回）。优先级：`--provider`/`--model` > `PRUX_MODEL` > 本设置 > 内置默认模型 |
| `defaultThinkingLevel` | string | - | 启动思考级别：`"off"`、`"minimal"`、`"low"`、`"medium"`、`"high"`、`"xhigh"`、`"max"`；超出模型支持范围会被钳制。Shift+Tab 循环或 `/thinking` 修改后自动写回 |
| `enabledModels` | string[] | - | `Ctrl+P` 循环名单（同 `--models` 格式）；由 `/scoped-models` 面板 Ctrl+S 写入（`/model` 切到名单外模型时会自动追加该模型），缺失/空数组 = 全部启用 |

### 界面与显示

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `theme` | string | `"system"` | 主题名：内置 `"system"`（默认，按终端上报的配色推导）、`"dark"`/`"light"`、`"auto"`（按终端背景在 dark/light 间选择）或自定义主题。主题来源见 [themes.md](themes.md) |
| `hideThinkingBlock` | boolean | `false` | 隐藏输出中的思考块（`/settings` → Hide thinking） |
| `syntaxHighlight` | boolean | `true` | 代码块语法高亮（syntect）开关（`/settings` → Syntax highlight） |
| `mermaid` | boolean | `true` | Mermaid 代码块渲染为 Unicode 流程图；关闭时按普通代码块展示（`/settings` → Mermaid） |
| `latex` | boolean | `true` | `$...$` / `$$...$$` 公式渲染为 Unicode；关闭时保留原始文本（`/settings` → LaTeX） |
| `autocompleteMaxVisible` | number | `5` | 补全下拉最多可见项（3-20；`/settings` → Autocomplete max items）。条目描述过长时自动折行（单条至多 2 行，续行与描述列对齐，仍超出以 `…` 收尾），折行后总行数可超过该值 |
| `historyMaxEntries` | number | `500` | 当前项目 prompt 历史在磁盘上保留的条数（`0` = 不落盘；`/settings` → History max entries，档位 0/100/250/500/1000）。调小后**立即截断**内存历史 |
| `fullscreenWheelScrollLines` | number \| `"auto"` | `"auto"` | 鼠标滚轮每格移动的行数（1-100）；`"auto"` 按滚动速度加速（单格一行，快速连续滚动最多十行）（`/settings` → Wheel scroll lines） |
| `quietStartup` | boolean \| `"header"` | `"header"` | 启动横幅与资源清单怎么显示（`/settings` → Quiet startup）：`false` 两者都显示，`"header"` 只显示横幅（版本与按键提示）、不输出资源清单，`true` 两者都不显示。`--verbose` 强制两者都显示 |
| `externalEditor` | string | `$VISUAL` → `$EDITOR` → `vi` | `Ctrl+G` 外部编辑器命令，优先于环境变量。需要带参数时写整条命令，如 `"code --wait"` |
| `showImages` | boolean | `false` | 在支持图形协议的终端里内联显示图片（消息附件与工具结果），默认关闭时图片块**整块不渲染**（`/settings` → Show images）。见下方「内联图片」小节 |
| `showCacheMissNotices` | boolean | `false` | 显示缓存相关提示（保温、未命中）与 provider 恢复诊断（`/settings` → Cache miss notices） |

### 提示缓存

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `cacheWarming` | string | `"off"` | 提示缓存保温（`/settings` → Cache warming）：真实请求写出 cache 后用 1 token 请求重放续命，让下一次请求仍命中缓存。`"off"`（缺省/非法值）完全不保温、`"streaming"` 只在 agent run 期间保温（硬上限 1 小时）、`"idle"` 结算后继续保温（硬上限 30 分钟）。期望节省不足 $0.05 时不发（每次保温都真花钱）；只读全局文件，项目级设置不生效 |

### 项目信任

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `defaultProjectTrust` | string | `"ask"` | 无扩展或已保存决策时的回退信任策略：`"ask"`、`"always"`、`"never"`（`/settings` → Default project trust）。仅全局文件读取 |

### 消息投递

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `steeringMode` | string | `"one-at-a-time"` | 流式时发送 steering 消息：`"one-at-a-time"`（发一条等回应）或 `"all"`（一次全发）（`/settings` → Steering mode） |
| `followUpMode` | string | `"one-at-a-time"` | 排队 follow-up 消息的发送方式：同上（`/settings` → Follow-up mode） |
| `exposeSessionEnvironment` | boolean | `true` | 向 shell 工具环境注入会话变量：`PRUX_SESSION_ID`、`PRUX_SESSION_FILE`、`PRUX_PROVIDER`、`PRUX_MODEL`、`PRUX_REASONING_LEVEL`（`/settings` → Expose session env） |

### 上下文压缩

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `autoCompact` | boolean | `true` | 自动上下文压缩开关（`/settings` → Auto-compact）。手动压缩用 `/compact`，详见 [compaction.md](compaction.md) |
| `compactReserveTokens` | number | `16384` | 为 LLM 回复预留的 token 数（`/settings` → Compact reserve tokens，档位 4096/8192/16384/32768/65536）。也兼容嵌套写法 `compaction.reserveTokens`，还可按模型覆盖（`compaction.modelOverrides`） |
| `compactKeepRecentTokens` | number | `20000` | 保留（不参与摘要）的最近 token 数（`/settings` → Compact keep recent tokens，档位 8000/12000/20000/40000/80000）。同样兼容 `compaction.keepRecentTokens` 与按模型覆盖 |

### 自动重试

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `retry.enabled` | boolean | `true` | 对瞬时错误启用 agent 级自动重试（`/settings` → Retry） |
| `retry.maxRetries` | number | `3` | agent 级最大重试次数（`/settings` → Retry max attempts，档位 0/1/2/3/5/10；`0` = 不重试） |
| `retry.baseDelayMs` | number | `2000` | 指数退避的基准延迟（2s、4s、8s）；无 `/settings` 入口，只能手改 |

```json
{
  "retry": {
    "enabled": true,
    "maxRetries": 3,
    "baseDelayMs": 2000
  }
}
```

### 图片

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `images.autoResize` | boolean | `true` | 发送前自动缩放图片（`@file` 附件、剪贴板粘贴、`read` 读取、工具返回的图片）（`/settings` → Images auto resize） |
| `images.blockImages` | boolean | `false` | 阻止一切图片发送给 LLM（`/settings` → Images block）：`@图片` 引用与剪贴板粘贴不收集附件，`read` 读取与工具返回的图片被替换为文本占位 |

### Shell

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `shellPath` | string | - | 自定义 shell 路径（支持前导 `~` 展开） |
| `shellCommandPrefix` | string | - | 每条 bash 命令的前缀（如 `"shopt -s expand_aliases"`） |

### 工具

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `defaultTools` | string[] | `["read", "bash", "edit", "write"]` | 启动时启用的内置工具；可选 `read`、`bash`、`edit`、`write`、`grep`、`find`、`ls`、`powershell`（Windows）。扩展与 SDK 自定义工具不受影响。`/settings` → Built-in tools 子面板可勾选（只处理内置工具，写入 `+name`/`-name` 增量） |

```json
{
  "defaultTools": ["read", "bash", "edit", "write", "grep", "find", "ls"]
}
```

项目级 `.prux/settings.json` 的 `defaultTools` 数组**替换**全局数组（含空数组 = 无内置工具但保留扩展工具）。项目列表全是 `+`/`-` 条目时改为在全局列表上增删（只增删，不替换）。CLI 的 `--tools`（allowlist，支持 `*` 通配；全为 `+`/`-` 条目时改为在默认选择上增删）、`--no-tools`（全关）、`--no-builtin-tools`（只关内置）、`--exclude-tools`（过滤，支持通配）、`--no-mcp`（关掉内置 MCP 支持）会覆盖本设置。注意 `--tools` 不再顺带挡掉 MCP 工具，除非条目以 `mcp__` 开头（或用 `--exclude-tools` / `--no-mcp` 明确关掉）。

`/settings` → Built-in tools 子面板只处理内置工具，写入的是 `+name` / `-name` 增量（例如关掉 `read`、开上 `grep` 得到 `["-read", "+grep"]`）：手写的纯工具名列表不会被覆盖，条目清空时删除该键（回落默认工具集）。面板反映的是**全局 + 项目**合并后的有效状态，项目层若用纯工具名替换，面板的切换不会改变有效工具集。变更在 `/reload` 或重启后生效。

### 采样

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `temperature` | number | - | temperature 覆盖，优先级高于模型的 `samplingParams`（`/settings` → Temperature，档位 default/0/0.2/0.5/0.7/1；选 `default` 即删除该键） |

### 扩展

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `extensionMode` | string | `"all"` | 扩展模式，四档：`"minimal"`、`"dev"`、`"creator"`、`"all"`（级别 minimal < dev = creator < all，扩展在「级别低于当前模式」或「恰好声明为当前模式」时可用）。`/extension` 面板 Tab 按 `all → dev → creator → minimal → all` 循环并写盘 |
| `disabledExtensions` | string[] | `[]` | 禁用的扩展名列表（默认启用的扩展未列出即启用）；`/extension` 面板关闭某个扩展后写盘。扩展机制见 [extensions.md](extensions.md) |
| `enabledExtensions` | string[] | `[]` | 显式启用的扩展名列表，仅对声明“默认关闭”的扩展（如 `plan-mode`、`goal`）有意义：需在此列出（或经 `/extension` 面板开启）才生效；默认启用的扩展不写此键 |

### 更新检查

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `skipUpdateVersion` | string | - | 用户在 `update-check` 提示中选 `Don't ask again` 时自动写回的版本号（不带前导 `v`）。只抑制**该版本**的提示，之后发布的更新版本仍会提示；删除该键即可重新接收所有版本提示。检查在启动时异步进行，`--offline`/`PRUX_OFFLINE` 会跳过 |

### 技能与主题来源

| 设置 | 类型 | 默认 | 说明 |
|------|------|------|------|
| `skills` | string[] | `[]` | 自动发现技能的**过滤规则**（不是加载目录），仅带前缀的规则生效：`!pattern` 按 glob 排除、`+path` 精确强制包含、`-path` 精确强制排除（优先级最高）；无前缀项被忽略。详见 [skills.md](skills.md) |
| `themes` | string[] | `[]` | 附加主题来源：目录（按 `<name>.json` 查找）或单个主题 JSON 文件路径，支持 `~` 展开。与 agent 目录 `themes/`、项目 `.prux/themes/` 共同构成主题查找候选。详见 [themes.md](themes.md) |

```json
{
  "skills": ["!brave-*", "+path/to/keep"],
  "themes": ["~/.prux/custom-themes", "~/.prux/dracula.json"]
}
```

### 内联图片（`showImages`）

开启后，消息里的图片（`Ctrl+V` 粘贴的附件）与工具结果里的图片（如 `read` 读取的截图）会按终端图形协议直接画在消息区；关闭（默认）时图片块**整块不渲染**——既不预留行，也不留 `[image]` 占位行。

- **终端能力探测只在启动时做一次**，且只在 `showImages` 为真时做：查询会读写 stdin，必须早于 TUI 接管输入。运行中通过 `/settings` 打开时不再查询，改按环境变量判定（`TERM_PROGRAM` / `KITTY_WINDOW_ID` / `WEZTERM_PANE` / `ITERM_SESSION_ID` 等），单元格像素尺寸用默认值——重启一次能得到更准的协议与尺寸。
- **tmux 内用 sixel（裸 sixel）**：tmux 3.4+ 编译了 sixel 支持（`#{sixel_support}` 为 `1`）时用 **sixel**——把真实像素交给外层终端画，这是 tmux 里唯一能看清图片的路（halfblocks 的分辨率只有「列数 × 行数×2」个采样点，60 列的图约 60×50 个色块，怎么调滤镜都是糊的）。
  必须是**裸** sixel：库的 tmux 路径会把 sixel 包进 DCS passthrough（`\x1bPtmux;…`），实测（tmux 3.6a + foot）会被 tmux 当普通文本打到屏幕上、把界面刷花并阻塞写入（界面假死），所以 prux 构造 picker 时会临时隐藏 tmux 身份，让库走非 tmux 的裸 sixel 路径。
  tmux 转发 sixel 仍受 `allow-passthrough` 管辖（没开只会把序列吞掉、图片区一片空白），所以探测到没开时 prux 会显式打开**当前 pane** 的这个选项（不动全局配置）。拿不到 sixel 支持时回落 halfblocks（纯文本输出，复用器不拦）。
  **单元格像素尺寸取 pane 的真实值**（pane 像素尺寸 ÷ 单元格数，等价于 tmux 的 `#{client_cell_width}`）：sixel 是按像素画的，用库的默认 10×20 会让图只占预留区左上角一小块、右下留一大片空。终端不上报像素尺寸时回落默认值。
- **协议**：kitty（kitty / ghostty / WezTerm / Warp）、iTerm2、sixel，其余终端回落到 halfblocks（半个方块字符 + 真彩色，任何终端都能显示，保真度低）。用 `PRUX_IMAGE_PROTOCOL=kitty|iterm2|sixel|halfblocks|none` 可强制指定，`none` 等于关闭内联图片。
- **宽度上限 60 列**（对齐 pi 的 `imageWidthCells` 默认），高度上限是同宽正方形换算出的行数；图片按比例缩放（**比盒子小的图不放大**，保持原始像素尺寸，与协议库的 `Resize::Fit` 口径一致），滚动时按可见行裁剪（半出视口只画可见部分）。图片盒子的像素尺寸不可能是单元格的整数倍，库会把不足一格的余量补成**透明**像素（不显色但占位置）；这段补边已经接近一行时，prux 就不再额外补空行——否则多张图片之间会空出两行。需要补行时它沿用所在块的背景色（用户消息要保证色块连续）。
- **编码是异步的**：渲染线程只查缓存，未命中就登记后台任务、先画 `[image]` 占位；解码 + 缩放 + 协议编码（一张 1726×778 的截图约 0.3~1s）在后台线程做完后把界面标脏，下一帧换成真图。所以发送/粘贴图片不会再卡住界面。
- 会话查看器（`/session` 的全屏覆盖层走另一条渲染管线）仍显示 `[image]` 占位；扩展渲染器接管的工具块也不参与图片绘制。

## 完整示例

```json
{
  "defaultProvider": "deepseek",
  "defaultModel": "deepseek-v4-pro",
  "defaultThinkingLevel": "medium",
  "defaultProjectTrust": "ask",
  "theme": "dark",
  "hideThinkingBlock": false,
  "syntaxHighlight": true,
  "mermaid": true,
  "latex": true,
  "autocompleteMaxVisible": 5,
  "historyMaxEntries": 500,
  "fullscreenWheelScrollLines": "auto",
  "externalEditor": "code --wait",
  "showImages": false,
  "showCacheMissNotices": false,
  "cacheWarming": "off",
  "defaultTools": ["read", "bash", "edit", "write", "grep", "find", "ls"],
  "temperature": 0.7,
  "steeringMode": "one-at-a-time",
  "followUpMode": "one-at-a-time",
  "exposeSessionEnvironment": true,
  "autoCompact": true,
  "compactReserveTokens": 16384,
  "compactKeepRecentTokens": 20000,
  "retry": {
    "enabled": true,
    "maxRetries": 3,
    "baseDelayMs": 2000
  },
  "images": {
    "autoResize": true,
    "blockImages": false
  },
  "enabledModels": [],
  "extensionMode": "all",
  "disabledExtensions": [],
  "enabledExtensions": [],
  "skills": [],
  "themes": []
}
```

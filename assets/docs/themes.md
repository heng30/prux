# 主题（Themes）

主题是定义 TUI 颜色的 JSON 文件。prux 从主题目录与配置加载颜色变量，用于渲染交互式界面与 `/export` 的 HTML 导出。

## 目录

- [位置](#位置)
- [内置主题](#内置主题)
- [选择主题](#选择主题)
- [创建自定义主题](#创建自定义主题)
- [主题格式](#主题格式)
- [颜色键](#颜色键)
- [颜色值](#颜色值)
- [提示](#提示)

## 位置

prux 依次从以下位置加载主题（名称或路径）：

1. **显式路径**：若 `--theme` 参数指向 `.json` 文件，直接加载该文件。
2. **项目主题**：`<cwd>/.prux/themes/<name>.json`。
3. **配置目录**：`settings.json` 的 `themes` 数组（可含文件或目录，`~` 会展开）。
4. **agent 目录**：`~/.prux/themes/<name>.json`（或 `$PRUX_AGENT_DIR/themes/<name>.json`）。
5. **内置兜底**：`agent_dir/themes/dark.json`，最后回退到内嵌的 `dark` / `light`。

找不到目标主题文件时自动回退到内置 `dark`（仅当主题名就是 `light` 时才回退 `light`，与终端背景无关）。

`--no-themes` 关闭主题发现（`/theme` 面板不再列出配置目录/项目中发现的主题）。

## 内置主题

prux 内嵌两个主题，无需落盘即可使用（自 0.99.1 起采用 pi 修订后的 OKHSL 调色板，值写成 `okhsl()`）：

- **`dark`**
- **`light`**

此外还有由终端配色推导的 **`system`** 主题，它是**默认主题**：从终端上报的
前景/背景/16 色 ANSI 色板（OSC 10/11/4）推导整套色板；终端不上报配色时改用
**索引兜底档**（`generate_indexed_theme_colors`：彩色 token 用 ANSI 槽位、中性色走 SGR 2），
**不再**回落到内置 `dark` / `light`。

`extra-themes` 扩展（默认启用）会在启动时把一批额外主题按「内容幂等」写入 `~/.prux/themes`：

| 主题文件 | 主题名 |
|----------|--------|
| `catppuccin-mocha.json` | `catppuccin-mocha` |
| `cyberpunk.json` | `cyberpunk` |
| `dracula.json` | `dracula` |
| `everforest.json` | `everforest` |
| `gruvbox.json` | `gruvbox` |
| `midnight-ocean.json` | `midnight-ocean` |
| `nord.json` | `nord` |
| `ocean-breeze.json` | `ocean-breeze` |
| `rose-pine.json` | `rose-pine` |
| `synthwave.json` | `synthwave` |
| `tokyo-night.json` | `tokyo-night` |

（主题名取 JSON 里的 `name` 字段，这些内置额外主题都用小写 slug，`/theme` 里显示的也是它。）

同步规则：

- 目标不存在 → 写入；已存在（无论内容是否一致）→ 跳过不覆盖。
- 禁用该扩展时，只删除内容与内嵌完全一致的文件；被外部修改过的文件保留，避免误删你的定制。

## 选择主题

在 `/settings` 面板、`/theme` 面板，或用 `/theme <name>` 命令切换。切换成功后写入 `settings.json` 的 `theme` 字段，重启后恢复。

默认键位 `ctrl+shift+t`（`app.theme.cycle`）循环下一个主题。

### 默认主题

在 `settings.json` 中设置默认主题：

```json
{
  "theme": "my-theme"
}
```

未设置时默认 `system`（终端配色推导；终端不上报配色时改用索引兜底档）。设置为 `auto` 会按终端背景自动解析：检测到亮背景（`COLORFGBG` 背景色号 > 8）用 `light`，否则用 `dark`。

### 初始主题（仅本次运行）

用 `--use-theme <name>` 指定本次运行的初始主题，不改变已保存的设置：

```bash
prux --use-theme light
```

`--theme <名字或 .json 路径>` 指定主题（值为主题名，或以 `.json` 结尾时当显式文件路径）；它**不接受目录**，且优先级最低（`--use-theme` > `settings.theme` > `--theme`），重复给出时只用第一个。`--export` 导出 HTML 时同样以 `--use-theme` / `settings.theme` / `--theme` 决定配色。

选择其他主题后立即生效并正常保存。

## 创建自定义主题

1. 创建主题文件：

```bash
mkdir -p ~/.prux/themes
vim ~/.prux/themes/my-theme.json
```

2. 定义颜色（prux 只读取它需要的键，未定义的键走代码内默认值，无需填满全部）：

```json
{
  "name": "my-theme",
  "vars": {
    "primary": "#00aaff",
    "muted2": "#808080"
  },
  "colors": {
    "accent": "primary",
    "border": "primary",
    "success": "#00ff00",
    "error": "#ff0000",
    "warning": "#ffff00",
    "muted": "muted2",
    "dim": "#666666",
    "text": "",
    "userMessageBg": "#2d2d30",
    "userMessageText": "",
    "toolPendingBg": "#1e1e2e",
    "toolSuccessBg": "#1e2e1e",
    "toolErrorBg": "#2e1e1e",
    "toolTitle": "primary",
    "toolOutput": "",
    "mdCode": "#00ffff",
    "mdCodeBlock": "",
    "syntaxKeyword": "primary",
    "syntaxString": "#00ff00",
    "thinkingText": "muted2"
  }
}
```

3. 用 `/theme my-theme` 或 `/settings` 选择。

**重载：** prux 没有自动热重载，编辑主题文件后执行 `/reload` 使改动生效。

## 主题格式

prux 读取主题 JSON 的五个字段：

- `name`（可选）— 主题显示名（`/theme` 面板与顶部展示的就是它）。
- `appearance`（可选）— `"dark"` 或 `"light"`，声明主题明暗外观；缺省时先从 `vars` 里的颜色推导，再回退到终端当前的明暗。
- `vars`（可选）— 可复用的颜色，供 `colors` 引用。
- `colors`（可选）— 实际渲染用到的颜色；与 `vars` 合并进同一张表，`colors` 覆盖同名项。
- `export`（可选）— `/export` HTML 导出的配色（见 [颜色键](#颜色键)）。

`$schema` 字段用于编辑器自动补全与校验，非必需。prux 不校验「必需 token」数量——它只读取渲染用到的键，未定义的键回退到代码内默认色。

## 颜色键

prux 渲染器实际读取的颜色键如下（这张表就是渲染器用到的键；未列出的键只当作 `vars` 可引用项）。`""` 表示使用终端默认色。

> 「默认」列是**代码内兜底色**，仅在主题（含内置主题）未声明该键时生效；
> 内置 `dark`/`light` 自 0.99.1 起改用 pi 的 OKHSL 调色板，不再等于下表的值。

### 核心界面

| 键 | 用途 | 默认 |
|----|------|------|
| `accent` | 主强调色（logo、选中项、光标） | `#8abeb7` |
| `border` | 边框 | `#5f87ff` |
| `success` | 成功状态 | `#b5bd68` |
| `error` | 错误状态 | `#cc6666` |
| `warning` | 警告状态 | `#d8a657` |
| `muted` | 次要文本 | `#808080` |
| `dim` | 三级文本（提示、次要标签、系统消息 info 级） | `#666666` |
| `text` | 默认文本（通常 `""`） | 终端默认 |
| `thinkingText` | 思考块文本 | `#808080` |
| `bashMode` | 输入框处于 bash 模式（行首 `!`）时的边框色 | `#7f9f7f` |
| `scrollbarThumb` | 全屏滚动条滑块 | `#6a6a78` |

### 系统消息

系统消息（命令输出、提示、报错）按级别取 pi 的状态键；主题未定义时用代码内兜底色。

| 键 | 用途 | 默认 |
| `dim` | info 级（普通提示） | `#666666` |
| `success` | success 级 | `#b5bd68` |
| `warning` | warning 级（加粗） | `#d8a657` |
| `error` | error 级（加粗） | `#cc6666` |

### 消息与工具块

| 键 | 用途 | 默认 |
|----|------|------|
| `userMessageBg` | 用户消息背景 | `#343541` |
| `userMessageText` | 用户消息文本 | `#d4d4d4` |
| `customMessageBg` | 扩展消息背景 | `#2d2838` |
| `customMessageText` | 扩展消息文本 | `#d4d4d4` |
| `customMessageLabel` | 扩展消息标签 | `#9575cd` |
| `toolPendingBg` | 工具框（进行中） | `#282832` |
| `toolSuccessBg` | 工具框（成功） | `#283228` |
| `toolErrorBg` | 工具框（错误） | `#3c2828` |
| `toolTitle` | 工具标题 | `#d4d4d4` |
| `toolOutput` | 工具输出文本 | `#808080` |
| `toolDiffAdded` | `edit` 工具 diff 的新增行 | `#98c379` |
| `toolDiffRemoved` | `edit` 工具 diff 的删除行 | `#e06c75` |
| `toolDiffContext` | `edit` 工具 diff 的上下文行 | `#6a737d` |

### Markdown

| 键 | 用途 | 默认 |
|----|------|------|
| `mdHeading` | 标题 | `#f0c674` |
| `mdLink` | 链接文本 | `#81a2be` |
| `mdLinkUrl` | 链接 URL | `#666666` |
| `mdCode` | 行内代码 | `#8abeb7` |
| `mdCodeBlock` | 代码块正文（公式渲染也用它，pi 无公式专用 token） | `#b5bd68` |
| `mdCodeBlockBorder` | 代码块围栏 | `#808080` |
| `mdQuote` | 引用文本 | `#808080` |
| `mdQuoteBorder` | 引用边框 | `#808080` |
| `mdHr` | 分隔线 | `#808080` |
| `mdListBullet` | 列表圆点 | `#8abeb7` |

### 语法高亮

代码块语法高亮用 `syntax*` 键（默认值见内置主题，未定义的 scope 回退到 `mdCodeBlock` 色）：

| 键 | 用途 |
|----|------|
| `syntaxComment` | 注释 |
| `syntaxKeyword` | 关键字 |
| `syntaxFunction` | 函数名 |
| `syntaxVariable` | 变量 |
| `syntaxString` | 字符串 |
| `syntaxNumber` | 数字 |
| `syntaxType` | 类型 |
| `syntaxOperator` | 运算符 |
| `syntaxPunctuation` | 标点 |

### HTML 导出（可选）

`export` 段控制 `/export` 输出的 HTML 配色。省略时从 `userMessageBg` 推导：

```json
{
  "export": {
    "pageBg": "#18181e",
    "cardBg": "#1e1e24",
    "infoBg": "#3c3728"
  }
}
```

## 颜色值

prux 支持五种值格式：

| 格式 | 示例 | 说明 |
|------|------|------|
| Hex | `"#ff0000"` / `"#f00"` | 6 位或 3 位十六进制 RGB |
| 色彩空间函数 | `"oklch(60% 0.15 250)"` / `"okhsl(232 54% 67%)"` | OKLCH / OKHSL，转成 sRGB 渲染（内置主题即用此格式） |
| 变量 | `"primary"` | 引用 `vars` / `colors` 中的条目（递归解析，最多 4 层） |
| 256 色索引 | `"ansi:196"` 或数字 `196` | 降级为 256 色序列 |
| 默认 | `""` | 终端默认色 |

prux 按终端能力输出颜色：真彩终端发 24-bit 序列（`38;2`/`48;2`），否则降级为
256 色近似（`38;5`/`48;5`，立方体 + 灰阶加权）。终端能力探测与覆盖见
[环境变量](environment-variables.md) 的 `PRUX_TRUE_COLOR`。

## 提示

- **暗色终端**：使用明亮、高饱和、高对比的颜色。
- **亮色终端**：使用更深、更柔和、低对比的颜色。
- **配色和谐**：先用基础调色板（Nord、Gruvbox、Tokyo Night 等，仓库 `assets/themes/` 已有）定义到 `vars`，再统一引用。
- **测试**：用不同消息类型、工具状态、Markdown 内容与长换行文本验证主题。

## 说明

- 主题目录为 `~/.prux/themes`（或 `$PRUX_AGENT_DIR/themes`），项目主题为 `.prux/themes/`。
- prux **不校验必需 token**：只读取渲染用到的键，其余忽略并回退默认。
- 颜色值支持 hex（`#rgb` / `#rrggbb`）、`oklch()` / `okhsl()`、变量引用、`ansi:<n>` 与空串。
- 无自动热重载：编辑主题后需 `/reload`。
- `extra-themes` 扩展会把内置额外主题同步进 `themes` 目录；禁用扩展时按内容一致删除，不误删外部定制。
- 未消费的键不生效（如 `searchMatch*`、thinking 边框色 `thinkingOff`~`thinkingMax`、`scrollbarTrack` 等仅存在于内置主题 JSON 中，渲染器不消费；`selectedBg` / `thinkingMax` 仅用于 `/export` 回退）。

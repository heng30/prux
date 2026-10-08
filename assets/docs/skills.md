# 技能（Skills）

技能是自包含的能力包，按需加载。一个技能提供针对特定任务的专用工作流、环境搭建说明、辅助脚本与参考文档。prux 遵循 [Agent Skills](https://agentskills.io/specification) 的目录形态（目录 + `SKILL.md`），实现其中所需的那部分机制。

> **安全：** 技能可以指示模型执行任意操作，也可能携带模型会运行的脚本。使用前请先审阅技能内容。

## 技能位置

prux 从以下位置加载技能（扫描按此顺序进行，同名技能先到先得，见「发现与加载规则」）：

**用户级（全局）：**
- `~/.prux/skills` —— agent 目录下的默认位置（agent 目录可用 `$PRUX_AGENT_DIR` 覆盖，见 [environment-variables.md](environment-variables.md)）
- `~/.agents/skills` —— 与其他 agent harness（Claude Code 等）共享的兼容目录

**项目级（仅当项目受信任后加载，见 [settings.md](settings.md) 的信任说明）：**
- `<cwd>/.prux/skills`
- cwd 及其祖先目录中的 `.agents/skills`（向上直到文件系统根；用户级 `~/.agents/skills` 自身不在此列，它作为用户级来源单独加载）

**CLI 显式路径：**
- `--skill <path>` —— 可重复指定文件或 `.md`（单文件技能）或目录（整目录技能），不受信任门控与 settings 过滤影响；`--no-skills` 关闭默认自动发现后，显式 `--skill` 仍然生效

发现规则：

- 每个技能根目录下，**直接包含 `SKILL.md` 的目录**即是一个技能（`SKILL.md` 所在目录为技能目录，找到后不再深入递归）。
- 目录树的更深处同样适用该规则：任意包含 `SKILL.md` 的嵌套目录都会成为一个技能。
- 技能根目录**顶层的散 `.md` 文件**：frontmatter 合法且 `description` 非空时，作为单文件技能加载。
- 嵌套层级的散 `.md` 文件不会被当作技能；不像是技能的 markdown 一律静默忽略。
- 扫描过程尊重技能根目录内及沿途的 `.gitignore` / `.ignore` / `.fdignore`（支持 `!` 取反），可用来排除技能候选。
- 对同一路径的重复发现（如符号链接）按真实路径去重，只算一次。

## 工作方式

1. 启动时 prux 扫描技能位置，提取每个技能的 `name`、`description` 与 `location`。
2. 系统提示词注入 `<available_skills>` 列表（每个技能含 name/description/location；仅当当前工具集包含 `read` 且存在技能时注入；`disable-model-invocation` 的技能被过滤掉，见下）。
3. 当任务与某技能描述匹配时，模型用 `read` 工具读取该技能的 `SKILL.md` 全文并遵循其中的指引（模型不一定会主动加载；可用提示词引导，或直接 `/skill:name` 强制注入）。
4. 技能正文中的相对路径一律相对技能目录解析。

这是渐进式披露：只有名称与描述常驻上下文，完整指令按需加载。

## 技能命令

所有已发现技能自动注册为 `/skill:name` 命令候选（默认开启，无需配置）。在输入框键入 `/skill:` 会列出全部技能及其描述。

```
/skill:pdf-tools            # 加载并执行该技能
/skill:pdf-tools extract    # 带参数加载
```

提交时 prux 把命令展开为一个 `<skill name=".." location="..">` 消息块：块内含该技能的 markdown 正文，并预置一行 `References are relative to <技能目录>`；命令后的参数附在块之后，作为一条普通用户消息文本（注意：斜杠文本提交后会整体小写化，参数也会被小写，见 [prompt-templates.md](prompt-templates.md)）。展开后的消息在 TUI 中默认折叠为一行 `[skill] name (Ctrl+O/Alt+click to expand)`，用 Ctrl+O 全局展开，或 Alt+左键单独展开该块查看正文。

未知的斜杠命令（不以已注册命令开头）不会被拦截，而是按普通用户输入处理，同样会经过技能/模板展开。

### `/skills` 面板（启停与安装）

`/skills` 打开技能管理面板：列出全部技能（含尚未落盘的**扩展技能**），空格切换、`Ctrl+S` 应用、回车看详情、`Esc` 关闭并丢弃未保存改动。

- **启停**：唯一真源是 `settings.json` 的 `skills` 过滤数组。关闭一个技能 = 在数组末尾追加 `-<name>`（因此压过前面所有规则）；开启 = 删除精确的 `-<name>` 项。面板**不会**改动你手写的 `+path` / `!glob` 规则；若某个技能是被这类规则排除的，行尾会标 `[filtered]`（面板开关改不动它）。
- **扩展技能**：随二进制分发的技能（`assets/skills/**`，编译期内嵌）默认**不落盘、不进系统提示**。在面板里开启会「检查并创建」到 `agent_dir()/skills/<name>/**`（已存在的文件不覆盖，保护你的修改）；关闭只写 `-<name>`，**不**删除磁盘副本，因此内置副本会持续遮蔽 `~/.agents/skills` 里的同名技能。
- **行尾注记**：`[not installed]` 扩展技能尚未落盘；`[model-off]` 该技能带 `disable-model-invocation`，即使启用也不会进 `<available_skills>`，只能 `/skill:<name>` 手动触发；`[filtered]` 被 `settings.json` 里非精确项规则排除。
- `Ctrl+S` 写盘后立即重扫技能并重建系统提示/工具链；勾选态以重扫结果回填，不会出现“显示已开但实际没加载”。

## 目录结构

一个技能是含 `SKILL.md` 的目录，其余内容自由组织：

```
my-skill/
├── SKILL.md              # 必填：frontmatter + 指令正文
├── scripts/              # 辅助脚本
│   └── process.sh
├── references/           # 按需读取的详细文档
│   └── api-reference.md
└── assets/
    └── template.json
```

`SKILL.md` 由 YAML frontmatter（见下）加 markdown 指令正文组成：

````markdown
---
name: my-skill
description: 该技能做什么、何时使用。写具体一些。
---

# My Skill

## Setup

首次使用前执行一次：

```bash
cd /path/to/my-skill && ./scripts/setup.sh
```

## Usage

```bash
./scripts/process.sh <input>
```

详细参数见 [reference guide](references/api-reference.md)。
````

正文与资源一律用相对路径（相对技能目录）。

## Frontmatter 字段

prux 只解析下面三个字段，其余字段（如 `license`、`compatibility`、`metadata`、`allowed-tools`）一律忽略：

| 字段 | 必填 | 说明 |
|------|------|------|
| `name` | 否 | 技能名。缺省时取 `SKILL.md` 所在目录名。形式要求：1-64 字符、仅小写字母/数字/连字符、不以连字符开头或结尾、无连续连字符。形式非法**不阻止加载**（宽松处理） |
| `description` | 是 | 技能的用途与触发时机。非空且 ≤1024 字节（UTF-8，中文按字节算）；缺失、为空或超长时该技能**不被加载** |
| `disable-model-invocation` | 否 | `true` / `yes` 时该技能从系统提示词中隐藏（模型无法自动感知），只能通过 `/skill:name` 显式调用 |

### 名称规则

- 1-64 个字符
- 仅小写字母、数字、连字符
- 不以连字符开头或结尾，无连续连字符

合法：`pdf-processing`、`data-analysis`、`code-review`
非法：`PDF-Processing`、`-pdf`、`pdf--processing`

prux **不要求** name 与所在目录同名（跨工具共享的技能目录往往如此），缺省时以目录名兜底。

### 描述最佳实践

description 决定模型何时加载该技能，越具体越好。

好：
```yaml
description: 从 PDF 提取文本与表格、填写 PDF 表单、合并多个 PDF。处理 PDF 文档时使用。
```

差：
```yaml
description: 处理 PDF。
```

## 发现与加载规则

| 情况 | 结果 |
|------|------|
| 目录含 `SKILL.md`，frontmatter 合法且 `description` 非空 | 加载为技能 |
| `SKILL.md` 无 frontmatter 或缺少 `description` | 不加载（静默，无告警） |
| 顶层散 `.md` 有技能 frontmatter 且 `description` 非空 | 加载为单文件技能 |
| 其它不是技能的 markdown | 静默忽略 |
| `name` 超过 64 字符或含非法字符 | 仍加载（宽松） |
| `description` 为空或超过 1024 字节 | 不加载 |
| frontmatter 含未知字段 | 忽略 |
| 同名冲突（来自不同位置） | 保留先发现者，无告警 |
| 符号链接指向同一技能目录 | realpath 去重，只加载一次 |

同名冲突的保留顺序即扫描顺序：项目级 `.prux/skills` → 祖先 `.agents/skills`（由近及远）→ `~/.prux/skills` → `~/.agents/skills` → `--skill` 显式路径。显式路径同样遵循先到先得，只是不受信任门控与 settings 过滤。

## settings.json 过滤

`settings.json` 的 `skills` 数组是**自动发现技能的过滤规则**，不是加载目录。仅作用于自动发现的技能，`--skill` 显式路径不受影响：

| 规则 | 语义 | 匹配方式 |
|------|------|----------|
| `!pattern` | 排除 | glob 通配 |
| `+path` | 强制包含（可覆盖前面的 `!`） | 精确（忽略首尾 `/`） |
| `-path` | 强制排除（可覆盖 `+`） | 精确（忽略首尾 `/`） |

规则按数组顺序逐条应用、后者覆盖前者；无前缀的普通条目不产生效果（被忽略）。每条规则的匹配目标是：技能文件的相对路径、文件全路径、文件名、父目录相对路径、父目录名（技能目录名），命中任意一个即视为匹配。

```json
{
  "skills": ["!brave-*", "-secret-tool", "+keep-this"]
}
```

## 示例

```
pdf-tools/
├── SKILL.md
├── scripts/
│   └── extract.sh
└── references/
    └── api.md
```

**SKILL.md：**

````markdown
---
name: pdf-tools
description: 用 Python + pdfplumber 提取 PDF 文本与表格。处理 PDF 文档时使用。
---

# PDF Tools

## Setup

```bash
cd /path/to/pdf-tools && python3 -m venv .venv && ./.venv/bin/pip install pdfplumber
```

## Extract Text

```bash
./scripts/extract.sh input.pdf
```

输出写入 `out/`，详见 [api.md](references/api.md)。
````

## 行为细节

- 技能目录默认位置为 agent 目录：`~/.prux/skills`（`$PRUX_AGENT_DIR/skills`）；另有共享兼容目录 `~/.agents/skills`。
- 不加载 npm 包 / `package.json` 中的技能资源。
- frontmatter 只解析 `name`、`description`、`disable-model-invocation` 三个字段，其余（`license`、`compatibility`、`metadata`、`allowed-tools`）忽略。
- `description` 缺失或超过 1024 字节时技能直接不加载。
- 顶层散 `.md` 在**所有**技能根目录（含 `.agents/skills`）都可作为单文件技能。
- 无 `enableSkillCommands` 配置开关：技能命令候选默认开启、不可关闭。
- 同名冲突无告警输出，仅保留先发现者。
- `/reload` 会重新扫描技能并重建系统提示词（新增或修改技能文件后无需重启）。

# 提示模板（Prompt Templates）

提示模板（prompt templates）是可复用的一段 Markdown 片段，提交时会按模板名展开成完整的提示。模板文件名（不含 `.md`）即命令名：`review.md` 对应 `/review`。

## 加载位置

prux 从以下位置加载模板（均只扫描目录的直接子文件，不递归）：

- **全局：** agent 目录下的 `prompts/`，即 `~/.prux/prompts/*.md`（agent 目录可用 `$PRUX_AGENT_DIR` 覆盖）。
- **项目：** 当前目录下的 `.prux/prompts/*.md`。该目录属于「项目资源」，存在时会触发启动时的项目信任询问；与上下文文件/技能不同，模板加载本身不受信任门控。
- **CLI：** `--prompt-template <path>` 可重复指定，路径可为单个 `.md` 文件或目录；存在性会在启动时校验（不存在的路径打印警告）。

用 `--no-prompt-templates` 关闭上述全局/项目目录的自动发现；显式传入的 `--prompt-template` 路径不受影响，仍然加载。

重名模板只保留最先加载的一个（同名字段按 全局 → 项目 → CLI 显式路径 的顺序去重）。编辑或新增模板后，用 `/reload` 即可从磁盘重新加载，无需重启。

## 格式

```markdown
---
description: Review staged git changes
---
Review the staged changes (`git diff --cached`). Focus on:
- Bugs and logic errors
- Security issues
- Error handling gaps
```

- 文件名成为命令名：`review.md` 即 `/review`。
- `description` 可选。缺省时取正文的第一个非空行（超过 60 字符才截断到 60 字符并追加 `...`）。
- `argument-hint` 可选，用于提示期望的参数。prux 会解析并保留该字段，但当前界面不展示。
- frontmatter 以 `---` 开头、以换行 `---` 结束；无 frontmatter 时整篇内容即正文。

## 触发方式

在输入区键入 `/name` 或 `/name [参数]` 并回车。提交的 `/` 开头文本会先匹配内置或已启用的扩展斜杠命令：

- **命中命令** → 执行该命令，模板不展开。因此模板名不要与现有斜杠命令重名（如 `reload`、`model` 等）。
- **未命中命令** → 先展开内嵌的 `/skill:name` 技能引用，再尝试按模板名展开；命中即替换为模板内容，未命中则原样作为普通用户消息发送（不报错）。

```
/review                           # 展开 review.md
/component Button                 # 带单个参数展开
/component Button "click handler" # 带多个参数
```

注意：斜杠文本提交后会整体小写化再分发，模板名与参数都会被小写化。建议模板文件一律使用小写文件名（`Foo.md` 无法通过 `/Foo` 命中）。

## 参数

模板内容支持位置参数、默认值与简单切片：

- `$1`、`$2`、…：位置参数
- `$@` 或 `$ARGUMENTS`：全部参数以空格连接
- `${1:-default}`：参数 1 存在且非空时用它，否则用 `default`
- `${@:-default}` 或 `${ARGUMENTS:-default}`：全部参数存在且非空时用全部，否则用 `default`
- `${@:N}`：从第 N 个参数（1 起）到结尾
- `${@:N:L}`：从第 N 个参数起取 L 个

参数按 bash 风格切分，支持单引号/双引号包裹含空格的参数。

```markdown
---
description: Create a component
---
Create a React component named $1 with features: $@
```

默认值适合可选参数：

```markdown
Summarize the current state in ${1:-7} bullet points.
```

用法：`/component Button "onClick handler" "disabled support"`。

## 加载规则

- `prompts/` 目录发现**不递归**：子目录里的模板不会自动加载，需用 `--prompt-template <目录>` 显式指定（该目录同样只扫描直接子文件）。

## 限制与细节

- `/` 自动补全只列出内置命令、扩展命令与 `/skill:name` 技能，**不列出提示模板**；模板通过提交未知斜杠命令的方式触发，而非下拉选择。因此 `argument-hint` 当前仅在模板记录中保留，界面暂不展示。
- 斜杠文本提交后整体小写化（见「触发方式」）。
- 项目级模板加载不受信任门控；`/reload` 可从磁盘重载模板。

相关文档：[快速开始](quickstart.md) · [设置](settings.md) · [技能](skills.md) · [扩展](extensions.md)

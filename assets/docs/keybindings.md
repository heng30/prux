# 键位绑定（Keybindings）

所有键盘快捷键可通过 `keybindings.json` 自定义（扩展自己注册的键位除外，见文末说明）。

配置文件位于 agent 目录：

- Linux/macOS：`~/.prux/keybindings.json`（或 `$PRUX_AGENT_DIR/keybindings.json`）

每个动作（action）可绑定一个或多个键。键 id 按命名空间分组（`tui.editor.*`、`tui.input.*`、`tui.select.*`、`app.*`）。修改配置后可用 `/reload` 即时生效（也重新注册扩展）。

## 键格式

`modifier+key`，修饰符 `ctrl` / `shift` / `alt` / `super`（可组合），键为：

- **字母：** `a-z`
- **数字：** `0-9`
- **特殊键：** `escape`(`esc`)、`enter`(`return`)、`tab`、`space`、`backspace`、`delete`、`insert`、`home`、`end`、`pageUp`、`pageDown`、`up`、`down`、`left`、`right`
- **功能键：** `f1`-`f12`
- **符号：** `` ` ``、`-`、`=`、`[`、`]`、`\`、`;`、`'`、`,`、`.`、`/` 等

组合示例：`ctrl+shift+x`、`alt+ctrl+x`、`ctrl+shift+alt+x`、`super+k`、`ctrl+1`。

`super` 绑定需要终端上报该修饰键（通常依赖 Kitty 键盘协议）。

## 所有动作与默认键

### 编辑器编辑

| 动作 id | 默认键 | 说明 |
| `tui.editor.cursorUp` | `up` | 光标上移（顶部浏览历史） |
| `tui.editor.cursorDown` | `down` | 光标下移（底部浏览历史） |
| `tui.editor.historyPrevious` | *(无)* | 显式浏览上一条历史 |
| `tui.editor.historyNext` | *(无)* | 显式浏览下一条历史 |
| `tui.editor.cursorLeft` | `left`, `ctrl+b` | 光标左移 |
| `tui.editor.cursorRight` | `right`, `ctrl+f` | 光标右移 |
| `tui.editor.cursorWordLeft` | `alt+left`, `ctrl+left`, `alt+b` | 词左移 |
| `tui.editor.cursorWordRight` | `alt+right`, `ctrl+right`, `alt+f` | 词右移 |
| `tui.editor.cursorLineStart` | `home`, `ctrl+home`, `ctrl+a` | 行首 |
| `tui.editor.cursorLineEnd` | `end`, `ctrl+end`, `ctrl+e` | 行尾 |
| `tui.editor.pageUp` | `pageUp`, `ctrl+pageUp` | 编辑器翻页 |
| `tui.editor.pageDown` | `pageDown`, `ctrl+pageDown` | 编辑器翻页 |
| `tui.editor.deleteCharBackward` | `backspace`, `shift+backspace` | 删除前一字符 |
| `tui.editor.deleteCharForward` | `delete`, `shift+delete`, `ctrl+d` | 删除后一字符 |
| `tui.editor.deleteWordBackward` | `ctrl+w`, `alt+backspace`, `ctrl+backspace` | 删除前一词 |
| `tui.editor.deleteWordForward` | `alt+d`, `alt+delete` | 删除后一词 |
| `tui.editor.deleteToLineStart` | `ctrl+u` | 删到行首 |
| `tui.editor.deleteToLineEnd` | `ctrl+k` | 删到行尾 |
| `tui.editor.yank` | `ctrl+y` | 粘贴最近一条删除的文本（kill-ring） |
| `tui.editor.yankPop` | `alt+y`, `alt+shift+y` | 循环替换为 kill-ring 里更旧的条目 |
| `tui.editor.undo` | `ctrl+-`, `ctrl+shift+-` | 撤销 |

### 输入

| 动作 id | 默认键 | 说明 |
| `tui.input.newLine` | `shift+enter`, `ctrl+j` | 换行 |
| `tui.input.submit` | `enter` | 提交输入 |
| `tui.input.tab` | `tab` | Tab / 补全 |
| `tui.input.copy` | `ctrl+shift+c` | 复制选中文本（`ctrl+c` 被 `app.clear` 占用） |

### 选择器（面板 / /settings / /resume）

| 动作 id | 默认键 | 说明 |
| `tui.select.up` | `up` | 上移选中 |
| `tui.select.down` | `down` | 下移选中 |
| `tui.select.first` | `home` | 跳到首项 |
| `tui.select.last` | `end` | 跳到末项 |
| `tui.select.pageUp` | `pageUp` | 上翻页 |
| `tui.select.pageDown` | `pageDown` | 下翻页 |
| `tui.select.confirm` | `enter` | 确认 |
| `tui.select.cancel` | `escape`, `ctrl+c` | 取消 |

### 应用动作

| 动作 id | 默认键 | 说明 |
| `app.interrupt` | `escape` | 中断当前任务 |
| `app.clear` | `ctrl+c` | 清空编辑器（500ms 内第二次退出） |
| `app.exit` | `ctrl+d` | 编辑器空时退出 |
| `app.suspend` | `ctrl+z` | 挂起到后台 |
| `app.thinking.cycle` | `shift+tab` | 循环 thinking 级别 |
| `app.model.cycleForward` | `ctrl+p` | 循环下一个模型 |
| `app.model.cycleBackward` | `shift+ctrl+p` | 循环上一个模型 |
| `app.model.select` | `ctrl+l` | 打开模型选择器 |
| `app.tools.expand` | `ctrl+o` | 折叠/展开工具输出 |
| `app.thinking.toggle` | `ctrl+t` | 显示/隐藏思考块 |
| `app.theme.cycle` | `ctrl+shift+t` | 循环切换主题 |
| `app.editor.external` | `ctrl+g` | 外部编辑器 |
| `app.message.copy` | `ctrl+x` | 复制最后一条助手消息；在 `/login`、`/mcp login` 的登录面板里改为复制授权 URL（长 URL 折行后无法整段选中） |
| `app.editor.copy` | `alt+c` | 复制整个输入框内容 |
| `app.message.followUp` | `alt+enter` | 排队 follow-up 消息 |
| `app.message.dequeue` | `alt+up` | 取回排队消息 |
| `app.message.scrollToTop` | `shift+home` | 消息区滚动到顶部 |
| `app.message.scrollToBottom` | `shift+end` | 消息区滚动到底部（跟随最新） |
| `app.message.pageUp` | `shift+pageUp` | 消息区向上翻一页 |
| `app.message.pageDown` | `shift+pageDown` | 消息区向下翻一页 |
| `app.clipboard.pasteImage` | `ctrl+v` | 粘贴（文本 fallback） |

### 会话选择器（/resume）

| 动作 id | 默认键 | 说明 |
| `app.session.toggleNamedFilter` | `ctrl+n` | 切换命名会话过滤 |
| `app.session.togglePath` | `ctrl+p` | 切换路径显示 |
| `app.session.toggleSort` | `ctrl+s` | 循环排序方式 |
| `app.session.rename` | `ctrl+r` | 重命名会话 |
| `app.session.delete` | `ctrl+d` | 删除会话 |
| `app.session.deleteNoninvasive` | `ctrl+backspace` | 查询为空时直接删除 |
| `app.session.next` | `ctrl+g` | 跳到当前会话的下一条会话（/resume） |

## 自定义配置

创建 `keybindings.json`：

```json
{
  "tui.editor.historyPrevious": "ctrl+p",
  "tui.editor.historyNext": "ctrl+n",
  "tui.editor.deleteWordBackward": ["ctrl+w", "alt+backspace"]
}
```

- 每个动作可绑定单个键或数组
- 用户配置覆盖默认；空数组 `[]` 禁用该动作
- 同一键绑定多个动作会产生冲突，启动时打印警告（来自用户配置之间的冲突；用户配置与默认键冲突不警告）

### Emacs 示例

```json
{
  "tui.editor.historyPrevious": "ctrl+p",
  "tui.editor.historyNext": "ctrl+n",
  "tui.editor.cursorLeft": ["left", "ctrl+b"],
  "tui.editor.cursorRight": ["right", "ctrl+f"],
  "tui.editor.cursorWordLeft": ["alt+left", "alt+b"],
  "tui.editor.cursorWordRight": ["alt+right", "alt+f"],
  "tui.editor.deleteCharForward": ["delete", "ctrl+d"],
  "tui.editor.deleteCharBackward": ["backspace", "ctrl+h"],
  "tui.input.newLine": ["shift+enter", "ctrl+j"]
}
```

### Vim 示例

```json
{
  "tui.editor.cursorUp": ["up", "alt+k"],
  "tui.editor.cursorDown": ["down", "alt+j"],
  "tui.editor.cursorLeft": ["left", "alt+h"],
  "tui.editor.cursorRight": ["right", "alt+l"],
  "tui.editor.cursorWordLeft": ["alt+left", "alt+b"],
  "tui.editor.cursorWordRight": ["alt+right", "alt+w"]
}
```

## 说明

- 未实现 fullscreen 模式，`tui.altScreen.*` 动作不列入；tree / scoped-models 选择器动作（`app.tree.*`、`app.models.*`）未实现。
- 扩展注册的键位（如 `plan-mode` 的 `alt+p` 切换 plan 模式、`goal`、`subagent` 等）不进 `keybindings.json` 配置体系，无法用该文件重绑或禁用；它们随扩展启用状态生效。
- 选择器内额外支持 Ctrl+J/K/G/D/U 导航（未列入配置表）。
- 列表导航首尾环绕：候选面板（`/`、`@`）、`/model` `/theme` `/thinking` 等模态面板、`/settings`、扩展设置、`/resume` 会话选择器与 `/agents` 覆盖层均在边界回绕（首项 ↑ → 末项，末项 ↓ → 首项）。
  输入框的提示历史同样环形（草稿 ↔ 最新 ↔ … ↔ 最早 ↔ 草稿）：草稿 ↑ → 最新一条，最早一条 ↑ → 回绕到草稿，草稿 ↓ → 回绕到最早一条，最新一条 ↓ → 回到草稿。
- `tui.input.copy` 默认 `ctrl+shift+c`（`ctrl+c` 被 `app.clear` 占用）；`tui.editor.deleteWordBackward` 额外含 `ctrl+backspace`（兼容旧行为）。
- 用户配置在启动时加载，修改后可用 `/reload` 即时生效。

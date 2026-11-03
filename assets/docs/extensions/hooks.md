# hooks（外部命令生命周期钩子）

`hooks` 把 prux 的四个生命周期时机暴露成**外部命令**：你在 `settings.json` 里写几条 shell
命令，prux 在该拦的时候把上下文交给它们（JSON 走 stdin），按它们的表态继续、拦下来或者改写。

它和扩展系统里的 Rust 钩子不是一回事：那些要重新编译，这里改配置就生效。适合做项目级的检查
（提交前跑 lint）、危险命令的门槛、以及「跑完测试再收工」这类闭环。

**默认不启用**，且仅在 `Dev` / `Creator` 模式（及 `All`）下可用。在 `/extension` 面板开启，
或写 `settings.json`：

```json
{ "enabledExtensions": ["hooks"] }
```

`/hooks` 打印当前生效的钩子清单与配置错误。

## 配置

`settings.json` 的 `hooks` 段，键是事件名，值是分组数组：

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "bash|write",
        "hooks": [
          { "type": "command", "command": "./scripts/check.sh", "timeout": 10 }
        ]
      }
    ]
  }
}
```

| 字段 | 含义 |
|---|---|
| `matcher` | 工具名正则（大小写不敏感）。省略、空串或 `*` = 匹配所有工具 |
| `hooks[].type` | 目前只支持 `command`（缺省即 `command`） |
| `hooks[].command` | 交给 shell 执行的命令串（Unix `sh -c`，Windows `cmd /C`） |
| `hooks[].timeout` | 等待上限（秒），缺省 10；超时连子进程一起杀掉，按「没有表态」处理 |

配置来源与其它 `settings.json` 键一致：全局 `agent_dir()/settings.json`，项目
`<项目>/.prux/settings.json` **仅在项目受信任时**参与（同一事件名下项目配置整体覆盖全局）。
解析不了的条目（未知事件名、坏正则、空命令、非 `command` 类型）只会被跳过并在 `/hooks` 里
列出原因，不会让整份配置失效。

## 事件

| 事件 | 时机 | 能做什么 |
|---|---|---|
| `PreToolUse` | 工具调用前 | `deny` 拦下、`ask` 弹确认面板、`updatedInput` 改写入参 |
| `PostToolUse` | 工具调用后 | 用 `reason` 覆盖交给模型的工具结果，或 `additionalContext` 追加一段 |
| `UserPromptSubmit` | 用户提交输入时 | `deny` 拒绝这次提交（聊天区给理由），`additionalContext` 并进提示词 |
| `Stop` | 一回合即将结算时 | `deny` + `reason` 要求再跑一轮，`reason` 作为新一轮的用户消息 |

同一事件下命中的分组按书写顺序执行；`PreToolUse` 上任何一条给出 `deny` 就立即停止，后面的
钩子不再执行（它们面对的是不会发生的调用）。

## 输入

每条钩子的 stdin 是一行 JSON：

```json
{
  "hook_event_name": "PreToolUse",
  "session_id": "0192f0…",
  "cwd": "/path/to/project",
  "tool_name": "bash",
  "tool_input": { "command": "cargo test" }
}
```

| 字段 | 出现时机 |
|---|---|
| `hook_event_name` | 总是 |
| `session_id` | 总是（会话尚未落盘时为空串） |
| `cwd` | 总是（钩子的工作目录，同时导出为环境变量 `PRUX_PROJECT_DIR`） |
| `tool_name` / `tool_input` | `PreToolUse`、`PostToolUse` |
| `tool_response` | `PostToolUse`（工具结果文本，超长截断） |
| `prompt` | `UserPromptSubmit`（用户这一轮的原文，超长截断） |
| `stop_hook_active` | `Stop`：`true` 表示上一轮结算也是本钩子要求续跑的 |

## 输出

看退出码：

| 退出码 | 含义 |
|---|---|
| `0` | 成功。stdout 若是 JSON 按下面的协议解释；`UserPromptSubmit` 下非 JSON 的 stdout 整段当作追加上下文 |
| `2` | 阻止。理由取 stderr（空则取 stdout） |
| 其它 | 没有表态；stderr 只在终端提示一次「hook failed」，不进模型上下文 |

退出码 `0` 的 JSON：

```json
{
  "decision": "block",
  "reason": "run `cargo fmt` first",
  "hookSpecificOutput": {
    "permissionDecision": "ask",
    "permissionDecisionReason": "this deletes build artifacts",
    "additionalContext": "the repo uses pnpm, not npm",
    "updatedInput": { "command": "pnpm test" }
  }
}
```

| 字段 | 效果 |
|---|---|
| `decision` | `block` = 阻止；`approve` = 明确放行（只用来抵消同批次里的 `ask`） |
| `reason` | 阻止 / 确认的理由；`PostToolUse` 下它成为交给模型的工具结果 |
| `hookSpecificOutput.permissionDecision` | `allow` / `ask` / `deny`，与 `decision` 同义，写法二选一 |
| `hookSpecificOutput.permissionDecisionReason` | 同上，作为理由 |
| `hookSpecificOutput.additionalContext` | 追加的文本：`PostToolUse` 附在工具结果后，`UserPromptSubmit` 并进提示词 |
| `hookSpecificOutput.updatedInput` | 改写工具入参（对象，整体替换），仅 `PreToolUse` 有意义 |

多条钩子的结果合并：意向取最严的（`deny` > `ask` > `allow` > 无表态），理由跟着最严的那条走，
追加的上下文按执行顺序拼接。

## 例子

写文件前跑格式化检查，不合格就拦下：

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "write|edit",
        "hooks": [
          {
            "command": "cargo fmt --check || { echo 'run cargo fmt first' >&2; exit 2; }"
          }
        ]
      }
    ]
  }
}
```

干完活必须测试通过才收工（`Stop` 返回 `block` 时，理由会作为新一轮的用户消息）：

```json
{
  "hooks": {
    "Stop": [
      { "hooks": [{ "command": "./scripts/must-pass.sh", "timeout": 300 }] }
    ]
  }
}
```

`must-pass.sh` 里在检查失败时 `echo "tests failed, fix them" >&2; exit 2` 即可。

## 注意

- **钩子是同步等待的**：`PreToolUse` / `PostToolUse` / `UserPromptSubmit` 会占住这次调用直到
  命令退出或超时，别在里面跑整轮测试；长时间检查留给 `Stop`（并把 `timeout` 调大）。
- **`PostToolUse` 撤不回已经发生的调用**：`block` 只改交给模型看的结果，文件已经写了、命令已经跑了。
- **`Stop` 会连续续跑**：同一个回合内最多续跑 5 次，超过就停下并在聊天区提示——通常意味着钩子的
  判定条件写错了。用户主动中断（`Esc` / `Ctrl+C`）、有排队输入、子代理结算时都不会触发续跑。
- **`ask` 需要交互界面**：非交互运行、面板超时或按 `Esc` 一律按拒绝处理。
- 钩子的 stdout 不会自动进入模型上下文（`UserPromptSubmit` 的纯文本 stdout 除外）。

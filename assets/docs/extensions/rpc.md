# RPC 模式（`prux rpc`）

`prux rpc` 把 prux 跑成一个**长期子进程**，用 stdin / stdout 上的「一行一条 JSON」控制。
适合：别的语言写的客户端、IDE 插件、自建界面、需要进程隔离的自动化。

用的是 `bootstrap()` 装配好的那个 Agent——模型链、工具表、会话文件、已启用扩展与 TUI 里
完全一致，客户端和 prux 看到的是同一份 `agent_dir()`。

```bash
prux rpc                 # 服务到 stdin 关闭
prux rpc --help          # 一行用法
```

子命令只在本扩展启用后存在（`/extension` 面板开启，或 settings.json 的
`enabledExtensions` 里加 `"rpc"`）。普通 CLI 参数照常生效（工作目录、模型、工具、会话），
但 **`@文件` 参数不会被接受**：要发提示词请走 `prompt` 命令。

## 记录家族

| 方向 | 记录 | 用途 |
|---|---|---|
| stdin | 命令 | 提问、查状态、改配置、管会话 |
| stdout | `response` | 报告一条命令成功与否，附带命令数据 |
| stdout | 会话事件 | 流式输出、消息、工具、压缩、重试等活动 |
| 双向 | 扩展 UI 记录 | 扩展要用户作答时与客户端往来 |

命令与响应按 `id` 对齐：命令带 `id` 时，响应原样带回同一个 `id`。命令处理是并发的，
**不要用响应顺序推断对应关系**。会话事件通常没有 `id`。

```json
{"id":"req-1","type":"get_state"}
{"id":"req-1","type":"response","command":"get_state","success":true,"data":{}}
```

失败的响应只有一个 `success:false` 与 `error` 文本：

```json
{"id":"req-2","type":"response","command":"set_model","success":false,"error":"Model not found: invalid/model"}
```

JSON 解析失败会回一条没有 `id` 的解析响应：

```json
{"type":"response","command":"parse","success":false,"error":"Failed to parse command: invalid JSON"}
```

## 分帧

严格 JSONL：一条记录一个完整 JSON 对象，以 LF（`\n`）结尾。**按字节流切行**，
不要用会认 `U+2028` / `U+2029` 的行读取器（它们在 JSON 字符串里合法）。stdout 只放协议记录，
诊断与应用日志在 stderr。

## 生命周期

`prompt` 的响应表示**提示词已被接受**，不是模型工作已结束：

```json
{"id":"req-2","type":"prompt","message":"看看这个仓库"}
{"id":"req-2","type":"response","command":"prompt","success":true,"data":{"disposition":"started"}}
```

`data.disposition` 取值：

- `started`：开了一轮新回合；
- `queued`：回合正在跑，消息排进队列（`streamingBehavior` 为 `steer` 或 `followUp` 时）；
- `handled`：被扩展斜杠命令认领，没有起回合（不要等 `agent_settled`）。

回合没跑完时发 `prompt` 又没给 `streamingBehavior`，会直接回错：
`Agent is already processing. Specify streamingBehavior ('steer' or 'followUp') to queue the message.`

回合结束后等 `agent_settled` 才算真正空闲：重试、溢出恢复、压缩、steer、follow-up 都可能接在后面。
发提示词前先开始读事件，否则可能错过很快的完成。

**关停**：关闭 stdin。进程会把在途的 bash 与 OAuth 刷新收尾完再退出。
不处理 SIGTERM——客户端应当「关 stdin 再等」。

## 命令

### 提问

| 命令 | 字段 | 响应数据 |
|---|---|---|
| `prompt` | `message`、`images?`、`streamingBehavior?`（`steer`/`followUp`） | `{disposition}` |
| `steer` | `message`、`images?` | `{disposition:"queued"}` |
| `follow_up` | `message`、`images?` | `{disposition:"queued"}` |
| `abort` | — | — |
| `clear_queue` | — | `{steering:[], followUp:[]}` |
| `new_session` | `parentSession?` | `{cancelled:false}` |

`steer` 排在下一轮请求之前注入；`follow_up` 等本轮结算后再起一轮。
`prompt` 里以 `/` 开头的文本会先试扩展斜杠命令，再展开技能命令（`/skill:名`）与提示词模板；
命中的斜杠命令回 `handled`，不会再发进模型。`steer` 与 `follow_up` **不接受**扩展斜杠命令
（回错 `Extension command "/名" cannot be queued. ...`），要执行命令请走 `prompt`。
面板类命令在 RPC 里没有画面：能出文本的照常回执，要开面板的回一句「这里做不到」。

### 状态

| 命令 | 响应数据 |
|---|---|
| `get_state` | `{model, thinkingLevel, isStreaming, isCompacting, steeringMode, followUpMode, sessionFile, sessionId, sessionName, autoCompactionEnabled, messageCount, pendingMessageCount}` |
| `get_messages` | `{messages:[...]}` |
| `get_session_stats` | `{sessionFile, sessionId, userMessages, assistantMessages, toolCalls, toolResults, totalMessages, tokens:{input,output,cacheRead,cacheWrite,total}, cost, contextUsage}` |
| `get_last_assistant_text` | `{text}` |
| `get_commands` | `{commands:[{name, description, source}]}`（`source` = `extension` / `prompt` / `skill`） |

`get_state` 的 `pendingMessageCount` 是 steer 与 follow-up 两条队列里尚未投递的条数。

### 模型与思考档位

| 命令 | 字段 | 响应数据 |
|---|---|---|
| `set_model` | `provider`、`modelId` | 模型对象 |
| `cycle_model` | — | `{model, thinkingLevel, isScoped}` 或 `null` |
| `get_available_models` | — | `{models:[{provider,id,name}]}` |
| `set_thinking_level` | `level` | — |
| `cycle_thinking_level` | — | `{level}` 或 `null` |
| `get_available_thinking_levels` | — | `{levels:[]}` |

模型对象只含 `provider` / `id` / `name` / `contextWindow` / `reasoning` / `input`，**不含凭据**。
`set_model` 会写进 settings.json 的默认模型；`cycle_model` 只改本次会话。
`cycle_model` 在有 `--models` / `settings.enabledModels` scope 时严格按 scope 顺序走；
scope 条目带思考档位时一并切换。

### 队列模式、压缩与重试

| 命令 | 字段 | 响应数据 |
|---|---|---|
| `set_steering_mode` | `mode`：`all` / `one-at-a-time` | — |
| `set_follow_up_mode` | `mode`：`all` / `one-at-a-time` | — |
| `compact` | `customInstructions?` | 压缩结果 JSON，没压时 `null` |
| `set_auto_compaction` | `enabled` | — |
| `set_auto_retry` | `enabled` | — |
| `abort_retry` | — | — |

队列模式与两个开关都会写回 settings.json。`abort_retry` 是幂等的空操作：
重试退避在回合内部，`abort` 已经把整轮停掉了。

### bash

| 命令 | 字段 | 响应数据 |
|---|---|---|
| `bash` | `command`、`excludeFromContext?` | `{output, exitCode, cancelled, truncated, fullOutputPath}` |
| `abort_bash` | — | — |

`bash` 不占命令流：结果自己回来。它**只执行并回结果，不写进会话上下文**
（因此 `excludeFromContext` 目前没有区别）。同一时刻只跟踪一个由 RPC 起的 bash，`abort_bash` 杀它。

### 会话

| 命令 | 字段 | 响应数据 |
|---|---|---|
| `switch_session` | `sessionPath` | `{cancelled:false}` |
| `fork` | `entryId` | `{text, cancelled:false}` |
| `clone` | — | `{cancelled:false}` |
| `get_fork_messages` | — | `{messages:[{entryId,text}]}` |
| `get_entries` | `since?` | `{entries:[...], leafId}` |
| `get_tree` | — | `{tree:[...], leafId}` |
| `set_session_name` | `name` | — |
| `export_html` | `outputPath?` | `{path}` |

`fork` 从指定条目分叉出一条新会话并切过去，`text` 是该条目的正文；`clone` 复制当前叶子所在的
活动分支。`get_entries` 的 `since` 传条目 id，返回它之后的条目；id 不存在直接报错。
`set_session_name` 不接受空名字。`export_html` 不给路径时按会话工作目录落一个
`prux-session-<会话文件名或会话 id>.html`。

## 扩展 UI 记录

扩展要用户作答时，stdout 会多出一类记录：

```json
{"type":"extension_ui_request","id":"ui-3","method":"select","title":"选一个","options":["a","b"]}
{"type":"extension_ui_response","id":"ui-3","value":"a"}
```

| method | 额外字段 | 客户端答复 |
|---|---|---|
| `select` | `title`、`options` | `{"value": "<选中的选项文本或下标>"}` 或 `{"cancelled":true}` |
| `notify` | `message`、`notifyType`（`info`/`warning`/`error`） | 不需要答复 |
| `setStatus` | `statusKey`、`statusText` | 不需要答复 |

扩展发起的问答：

- 通用选择面板（含权限门控）→ `select`；
- `ask_user_question` 的问卷逐题翻成 `select`：**多选表达不了**，那几题作为未作答返回，
  备注里点名是哪几题；
- 通知 / 状态栏 / toast → `notify` 或 `setStatus`；
- 覆盖层面板、停靠面板、扩展设置面板、自定义卡片在 RPC 里没有画面：覆盖层当场按关闭回传
  （否则扩展的工具调用会一直等），其余丢弃。斜杠命令要开面板时回一句「这里做不到」。

## 并发边界

`prompt`（回合）与 `compact` 会独占 Agent。这段时间里只有这几条命令接得下：

`steer`、`follow_up`、`abort`、`clear_queue`、`abort_bash`，以及带 `streamingBehavior`
的 `prompt`（等价于 steer / follow-up）。

其余命令（含 `get_state`）会回一条
`Agent is already processing; \`<命令>\` is unavailable until the run settles.`——
客户端该等 `agent_settled` 再发，或者在提示词之前就发。

## 最小客户端

```python
import json, subprocess

p = subprocess.Popen(["prux", "rpc"], stdin=subprocess.PIPE, stdout=subprocess.PIPE)
p.stdin.write(json.dumps({"id": "p1", "type": "prompt", "message": "你好"}).encode() + b"\n")
p.stdin.flush()

while line := p.stdout.readline():
    record = json.loads(line)
    if record.get("type") == "message_update":
        event = record["assistantMessageEvent"]
        if event.get("type") == "text_delta":
            print(event["delta"], end="", flush=True)
    elif record.get("type") == "agent_settled":
        break

p.stdin.close()
p.wait()
```

# 环境变量（Environment Variables）

prux 以三种方式使用环境变量：

- `PRUX_*` 变量用于配置 prux 进程本身。
- prux 会设置进程标记，让子进程识别 prux 是发起它们的 agent。
- 由 LLM 可调用的 shell 工具执行的命令会收到 `PRUX_*` 会话变量，描述当前会话。

Provider 的 API key 变量在 [models.md](models.md) 中说明。

## 进程标记

CLI 入口在启动时设置两个进程标记（子进程会继承）：

- `AI_AGENT=prux`：通用标记，供工具识别 prux 是发起进程的 agent。
- `PRUX_CODING_AGENT=true`：prux 专用标记，供子进程检测它们运行在 prux 内。

两者均非会话专属，也不是嵌入 SDK 时自动设置的。

## Shell 工具会话环境

由 `bash` 工具执行的命令会收到当前 prux 会话状态：

| 变量 | 说明 |
|------|------|
| `PRUX_SESSION_ID` | 当前会话 ID |
| `PRUX_SESSION_FILE` | 当前会话 JSONL 文件的绝对路径；临时会话不设置 |
| `PRUX_PROVIDER` | 当前选中的模型 provider |
| `PRUX_MODEL` | 当前选中的模型 ID |
| `PRUX_REASONING_LEVEL` | 当前生效的推理级别：`off`、`minimal`、`low`、`medium`、`high`、`xhigh` 或 `max` |

这些值在每条命令启动时解析，切换模型或调整推理级别会立即影响下一条 shell 命令，无需重启 prux。`PRUX_PROVIDER`/`PRUX_MODEL` 标识 prux 选中的模型，而非路由器内部可能选择的其它上游模型。

被问及当前模型或 provider 时，应读取这些变量，而非从系统提示词推断：

```bash
printf '%s/%s\n' "$PRUX_PROVIDER" "$PRUX_MODEL"
printf 'reasoning=%s session=%s\n' "$PRUX_REASONING_LEVEL" "$PRUX_SESSION_ID"
```

会话持久时可直接检查会话文件：

```bash
if [ -n "$PRUX_SESSION_FILE" ]; then
  tail -n 1 "$PRUX_SESSION_FILE"
fi
```

这些变量只注入 LLM 可调用的 `bash` 工具，不会注入用户输入的 `!` / `!!` 命令。是否注入由设置项 `exposeSessionEnvironment` 控制（见 [settings.md](settings.md)）；关闭时 prux 会清除继承的同名变量，避免嵌套 prux 进程暴露过期的父会话元数据。

## prux 进程配置

以下变量由 prux 自身读取：

| 变量 | 说明 |
|------|------|
| `PRUX_AGENT_DIR` | 覆盖配置目录；默认 `~/.prux` |
| `PRUX_CODING_AGENT_SESSION_DIR` | 覆盖会话存储目录；优先级低于 `--session-dir` |
| `PRUX_OFFLINE` | 禁用启动时联网操作（工具同步等）；等价于 `--offline`；值为 `1`/`true`/`yes` 视为真 |
| `PRUX_PROVIDER` | 覆盖默认 provider（优先级：`--provider` > 该变量 > settings） |
| `PRUX_MODEL` | 覆盖默认模型（优先级：`--model` > 该变量 > settings） |
| `PRUX_CACHE_RETENTION` | 选择扩展的 provider 提示缓存（Anthropic 语义）档位：`long` 用 1 小时长期缓存，`none`/`off` 关闭缓存，其余（含未设置）用目录里的短档 TTL |
| `PRUX_HYPERLINKS` | 覆盖 OSC 8 超链接探测，取值 `1`/`0`/`auto` |
| `PRUX_TERMINAL_COLORS` | 设为 `0`/`false`/`no` 关闭启动时的终端配色查询（OSC 10/11/4），此时 `system` 主题改用索引兜底色 |
| `PRUX_IMAGE_PROTOCOL` | 强制内联图片的图形协议，取值 `kitty`/`iterm2`/`sixel`/`halfblocks`/`none`（`none`/`0` 等于关闭内联图片）；仅在 `showImages` 开启时有意义，详见 [settings.md](settings.md) 的「内联图片」 |
| `PRUX_TRUE_COLOR` | 覆盖真彩色探测，取值 `1`/`0`/`auto` |
| `VISUAL`, `EDITOR` | `externalEditor` 未设置时的外部编辑器回退 |
| `HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY` | 出站 HTTP 代理与直连配置（大小写变体均可，小写优先）；规则见 [出站 HTTP 代理（http-proxy.md）](http-proxy.md) |

### Provider API key 与 OAuth

Provider 凭据可通过环境变量提供，例如 `ANTHROPIC_API_KEY`、`OPENAI_API_KEY`、`DEEPSEEK_API_KEY` 等（映射见 [models.md](models.md)）。其中：

- anthropic 额外支持 `ANTHROPIC_AUTH_TOKEN`、`ANTHROPIC_OAUTH_TOKEN` 两个回退来源。
- 凭据解析优先级：`--api-key` > `auth.json` > `models.json`（`apiKey` 值解析，支持 `$ENV` 引用） > 环境变量。

企业 OAuth 场景可覆盖主机：

- GitHub Copilot：`COPILOT_OAUTH_HOST` 或 `GITHUB_COPILOT_OAUTH_HOST`
- Kimi：`KIMI_CODE_OAUTH_HOST` 或 `KIMI_OAUTH_HOST`

### 说明

`bash` 工具会注入上文所列的 `PRUX_*` 会话变量；`powershell` 工具不会注入这些变量。

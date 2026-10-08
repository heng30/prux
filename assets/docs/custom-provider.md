# 自定义供应商（Custom Providers）

prux 的「供应商（provider）」是「协议端点 + 一组模型条目」的集合：内置供应商来自随程序发布的模型目录，
可通过 `/login` 登录（见 [quickstart.md](quickstart.md)）；自定义供应商则由用户在
`~/.prux/models.json`（可用 `PRUX_AGENT_DIR` 覆盖配置目录，见
[environment-variables.md](environment-variables.md)）里声明，用于：

- **代理 / 网关**：把内置供应商的请求指向公司代理或 API 网关
- **自托管端点**：Ollama、LM Studio、vLLM、llama.cpp 等本地推理服务
- **OpenAI 兼容第三方 API**：任何实现了内置协议的非标准供应商

## 模型目录：models-store.json

prux 的模型条目按 provider 合并自三层（低 → 高）：

1. **静态基线**：随程序内嵌的模型目录（`assets/models/models.all.json`，由
   `scripts/sync-models.py` 按 pi 的发布布局同步），始终可用
2. **动态缓存 `models-store.json`**：远程模型目录的只读缓存（请求 `/api/models/providers/<id>`），
   按 `(type, id)` 覆盖/追加基线模型
3. **用户配置 `models.json`**：自定义供应商与覆盖的唯一入口，每次读取实时生效

`models-store.json` 与 `auth.json` 同目录（`~/.prux/` 或 `$PRUX_AGENT_DIR`），按 provider
存放 `{ "models": [...], "checkedAt": ..., "etag": ..., "lastModified": ... }`。它由 prux 自动管理：

- 打开 `/model` 面板（对该面板列出的每个已配置供应商）与 `/login` 成功后都会后台刷新远程目录
- 请求携带 ETag：304 只推进 `checkedAt` 不重写模型；404/501 清空该 provider 动态层（基线仍可用）
- 启动时会清理「无 auth.json 凭据」的残留条目
- **不要手动编辑**：它只服务于已 `/login` 的内置供应商

自定义供应商的模型请全部写进 `models.json`——那是唯一稳定、可版本化的来源。

## 覆盖已有供应商

只换端点、保留全部原模型：

```json
{
  "providers": {
    "anthropic": { "baseUrl": "https://proxy.example.com" },
    "openai": {
      "baseUrl": "https://ai-gateway.corp.com/openai",
      "headers": { "X-Corp-Auth": "$CORP_AUTH_TOKEN" }
    }
  }
}
```

带 `models` 时按 id upsert（同名整体替换、新 id 追加、provider 级 `baseUrl`/`compat` 合并进所有模型）。
完整字段表与值解析见 [models.md](models.md)。

## 注册新供应商

给一个全新名字声明 `baseUrl` + `api` + `models`（Ollama / vLLM / LM Studio 这类 OpenAI 兼容端点
每个模型只需 `id`，`apiKey` 用占位值如 `"ollama"`）：

```json
{
  "providers": {
    "my-llm": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "apiKey": "$MY_LLM_API_KEY",
      "models": [
        {
          "id": "my-llm-large",
          "name": "My LLM Large",
          "reasoning": true,
          "input": ["text", "image"],
          "contextWindow": 200000,
          "maxTokens": 16384,
          "cost": { "input": 3.0, "output": 15.0, "cacheRead": 0.3, "cacheWrite": 3.75 }
        }
      ]
    }
  }
}
```

- **移除**：删除该 provider 条目即可，下次读取即失效，没有独立的 unregister 概念
- **命名冲突**：新名字不能与内置 provider 同名（同名即覆盖语义）
- **覆盖内置同名模型**：同 id 条目整体替换；只改个别字段用 `modelOverrides`（见 [models.md](models.md)）

## 认证与可用性

自定义供应商不能 `/login`（登录面板只列内置注册表）。请求层的 API key 解析顺序：

1. `--api-key` 命令行 flag
2. `auth.json` 的 `key`（`/login` 写入；值同样支持 `!cmd` / `$ENV` 解析）
3. `auth.json` 的 OAuth `access`（内置订阅型供应商）
4. `models.json` 的 provider 级 `apiKey`（值解析后）
5. 内置环境变量映射（`OPENAI_API_KEY`、`DEEPSEEK_API_KEY` 等，仅内置供应商）

因此自定义供应商的凭据来源是 `models.json` 的 `apiKey`（或其 `!cmd`/`$ENV` 形式）。配置了可解析
`apiKey` 的 provider 会被视为「已配置」，出现在 `/model` 面板与 Ctrl+P 循环（`auth.json` 凭据之外
的第二种配置来源）；未配置任何凭据的 provider 不会出现在模型面板。详见 [models.md](models.md) 的
「可用性」一节与 [keybindings.md](keybindings.md)。

> OAuth：prux 不为任意自定义供应商提供 `/login` OAuth。需要 SSO/订阅登录时，要么使用内置注册表
> 供应商（再经 `models.json` 覆盖 baseUrl 走企业代理），要么在网关侧完成认证后下发 API key。

## API 类型

`api` 决定流式协议实现（provider 级缺省，可在模型条目级覆盖）。prux 实现的协议：

| api | 说明 |
|-----|------|
| `openai-completions` | OpenAI Chat Completions，兼容性最好（Ollama/vLLM/LM Studio/大部分网关） |
| `openai-responses` | OpenAI Responses |
| `anthropic-messages` | Anthropic Messages（认证缺省走 `x-api-key`；`sk-ant-oat` 开头的 OAuth token 改走 `Authorization: Bearer` + `user-agent`/`x-app` 头；兼容端点也会收到 `anthropic-version`、会话亲和 `x-session-affinity` 等头） |
| `google-generative-ai` | Google Generative AI |

`azure-openai-responses`、`mistral-conversations`、`google-vertex`、`bedrock-converse-stream`、
`cloudflare-*` 等**未在协议层实现**：填了不支持的 `api` 值会直接报 `unsupported api type`。绝大多数
第三方 API 走 `openai-completions` 即可。

## 供应商字段：compat 与 quirk

模型的 `compat` 对象描述协议差异。`models.json` 的 provider 级 `compat` 会深合并进该 provider 的
**chat 条目**（已存在的 image / classifier 条目不合并；`models` 数组里新声明的条目无差别合并），
模型条目自身的 `compat` 胜出。prux 实际读取并生效的键：

| compat 键 | 生效协议 | 作用 |
|------|------|------|
| `supportsDeveloperRole` | completions / responses | `false` 时推理模型的系统消息用 `system` 而非 `developer`（Ollama 等常用 `false`） |
| `requiresReasoningContentOnAssistantMessages` | completions | 历史 assistant 消息需回传 reasoning content（DeepSeek 系） |
| `supportsStore` | completions | `false` 时不默认发 `store:false` |
| `supportsUsageInStreaming` | completions | 请求带 `stream_options.include_usage`（缺省 true） |
| `supportsFinishReason` | completions | 流式事件是否携带 finish_reason |
| `requiresAssistantAfterToolResult` | completions | 工具结果后需补一条 assistant 消息 |
| `supportsStrictMode` | completions | 工具启用 strict JSON schema |
| `maxTokensField` | completions | 输出 token 字段名：`max_completion_tokens`（缺省）/ `max_tokens` |
| `thinkingFormat` | completions | 思考参数形态（见下） |
| `supportsReasoningEffort` | completions | provider 是否接受显式 `reasoning_effort`。缺省时按 provider/baseUrl 探测（grok/zai/moonshot/together/cloudflare-gateway/nvidia/ant-ling 为 `false`，其余 `true`） |
| `openRouterRouting` / `vercelGatewayRouting` | completions | 把该对象原样放进请求体顶层 `provider` 字段（OpenRouter/Vercel 网关路由）；两者同时给出时只用 `openRouterRouting` |
| `supportsOpenAIGrammarTools` | completions | 按 OpenAI 语法约束工具（grammar）处理（缺省 false） |
| `allowedFallbackModels` | anthropic-messages | Anthropic 服务端 fallback 白名单（目录字段 `{provider, model, cost?}` 列表）：非空时请求体带 `fallbacks` 并开启服务端 fallback beta；计费时若服务端返回的模型命中列表且有 `cost`，按该条目的 `cost` 计，否则回落请求模型自身 cost |
| `allowEmptySignature` | anthropic-messages | 保留空 signature 的 thinking 块重放（Fireworks / Vercel 等兼容端点） |
| `forceAdaptiveThinking` | anthropic-messages | 上游要求 adaptive thinking（`thinking.type: adaptive`） |
| `supportsTemperature` | anthropic-messages | `false` 时不发 temperature（缺省 true） |
| `supportsEagerToolInputStreaming` | anthropic-messages | 控制 fine-grained beta 头（缺省 true） |
| `sendSessionAffinityHeaders` | anthropic-messages / completions | 是否下发会话亲和头；缺省视为「是否 openrouter」（见下） |
| `sessionAffinityFormat` | anthropic-messages / completions / responses | 会话亲和头形态：`openai` / `openai-nosession` / `openrouter`；缺省按 provider/baseUrl 探测（openrouter → `openrouter`，其余 `openai`） |

`thinkingFormat`（openai-completions 的扩展思考形态，与模型级 `thinkingLevelMap` 配合）。**缺省/空**时会按
provider/baseUrl 自动探测：`deepseek`→`deepseek`、`zai`→`zai`、`together`→`together`、`ant-ling`→`ant-ling`、
`openrouter`→`openrouter`，其余→`openai`。级别到参数值的映射（含 **`off` 键缺失 vs `off: null`** 的区别）
见 [models.md](models.md) 的「thinkingLevelMap 与 off 的语义」一节：

| 值（及探测结果） | 请求行为 |
|------|------|
| `deepseek` | `thinking.type: enabled/disabled`（`off: null` 时不发 disabled）；启用且 `supportsReasoningEffort` 时附 `reasoning_effort` |
| `zai` | `thinking.type: enabled(clear_thinking:false)/disabled`（`off: null` 时同前）；附 `reasoning_effort`（同上） |
| `qwen` | 顶层 `enable_thinking: true/false`（DashScope 式）；附 `reasoning_effort`（同上） |
| `qwen-chat-template` | `chat_template_kwargs: { enable_thinking, preserve_thinking: true }` |
| `openrouter` | `reasoning.effort`：启用取映射值，关闭取 `off` 映射值或 `"none"`（`off: null` 时关闭不发） |
| `together` | `reasoning.enabled: true/false`；附 `reasoning_effort`（同上） |
| `baseten` | 仅 `reasoning_effort`：启用取映射值，关闭取 `off` 映射值 |
| `ant-ling` | 仅启用且 `thinkingLevelMap` 有显式映射时发 `reasoning.effort` |
| `string-thinking` | `thinking` 为字符串：启用取映射值，关闭取 `off` 映射值或 `"none"` |
| `chat-template` | 不注入思考字段（由模板/网关自行处理） |
| `openai`（或空/其它） | `reasoning_effort`（仅当 `supportsReasoningEffort`）：启用取映射值，关闭仅当 `off` 为字符串才发 |

会话亲和（prompt cache / 路由）请求头的规则（对齐 pi 1.0.2）。会话 id 是当前会话的持久 id，
随请求下发；`openrouter` 格式只发 `x-session-id`，`anthropic-messages` 发 `x-session-affinity`，
`openai` 格式发 `session_id` + `x-client-request-id`（completions 额外补 `x-session-affinity`），
`openai-nosession` 格式去掉 `session_id`（opencode 目录即此档）：

| 情况 | 行为 |
|------|------|
| `anthropic-messages` / `openai-completions` 且 `sendSessionAffinityHeaders` 为 `false`（或缺省且非 openrouter） | 不发会话亲和头 |
| `anthropic-messages` / `openai-completions` 且开关为 `true`（或默认 openrouter） | 按 `sessionAffinityFormat` 下发 |
| `openai-responses` | 只要有会话 id 就按格式下发（无开关） |
| opencode / opencode-go（provider 或 baseUrl 命中 opencode.ai） | 额外发 `x-opencode-session` + `x-opencode-client`（所有协议） |

注意：远程目录/模型 JSON 里还带着一批 prux **当前不读取**的 compat 键（如 `cacheControlFormat`、
`chatTemplateArgs`、`supportsStrictTools` 等）——它们只是被原样透传，配置自定义供应商时
**不要依赖** 这些键生效，以上表为准。

其余模型条目字段（`name`、`reasoning`、`input`、`cost`、`contextWindow`、`maxTokens`、
`samplingParams`、`headers`、`thinkingLevelMap`）及 provider 级 `headers` / `authHeader` /
`modelOverrides` 的完整说明见 [models.md](models.md)。模型条目里的 `headers` 只对声明它的模型生效；
provider 级 `headers` 对所有模型生效，值解析失败的条目会被跳过。

## 上下文溢出与自动恢复

请求超过上下文窗口时，prux 会自动压缩对话并重试一次（详见 [compaction.md](compaction.md)）。
触发条件是对最终 assistant 消息的 `errorMessage` 做内置子串匹配（Anthropic `prompt is too long`、
OpenAI `context window`、`context_length_exceeded`、`too many tokens` 等），并先排除
`rate limit` / `throttling` / `too many requests` / `service unavailable` 等限流文案避免误判。

想让自定义供应商的溢出错误也能自动压缩恢复，应让服务端/网关的错误文案命中上述通用短语（最稳妥是
包含 `context_length_exceeded`）。未命中的溢出错误只会按普通失败结束本轮。

## SDK：直接构造模型

配置文件之外，Rust 库可直接构造 `ModelConfig` 并绕过 `models-store.json`（示例见
[sdk.md](sdk.md)「自定义模型」一节）。`ModelConfig` 的字段即上文模型条目字段的扁平化视图；
协议路由同样由 `api` 字段决定。

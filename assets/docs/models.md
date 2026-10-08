# 自定义模型（models.json）

在 `~/.prux/models.json`（可用 `PRUX_AGENT_DIR` 覆盖目录）添加第三方 provider 与模型
（Ollama、vLLM、LM Studio、代理等），或给内置 provider 换 `baseUrl`、合并模型、加请求头。

## 最小示例

本地模型（Ollama / LM Studio / vLLM）每个模型只需 `id`：

```json
{
  "providers": {
    "my-ollama": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "apiKey": "ollama",
      "models": [
        { "id": "llama3.1:8b" },
        { "id": "qwen2.5-coder:7b" }
      ]
    }
  }
}
```

文件每次使用实时读取：打开 `/model` 或重新发起请求即生效，无需重启。

## 完整示例

```json
{
  "providers": {
    "my-ollama": {
      "baseUrl": "http://localhost:11434/v1",
      "api": "openai-completions",
      "apiKey": "ollama",
      "compat": {
        "supportsDeveloperRole": false,
        "supportsReasoningEffort": false
      },
      "models": [
        {
          "id": "llama3.1:8b",
          "name": "Llama 3.1 8B (Local)",
          "reasoning": false,
          "input": ["text"],
          "contextWindow": 128000,
          "maxTokens": 32000,
          "cost": { "input": 0, "output": 0, "cacheRead": 0, "cacheWrite": 0 }
        }
      ]
    }
  }
}
```

## 支持的 API

| API | 说明 |
|-----|------|
| `openai-completions` | OpenAI Chat Completions（兼容性最好） |
| `openai-responses` | OpenAI Responses |
| `anthropic-messages` | Anthropic Messages |
| `google-generative-ai` | Google Generative AI |
| `openrouter-images` | OpenRouter 图片生成（一次性非流式，见下方「图片生成」） |
| `typesafe-system-one` | TypeSafe System One 分类器（一次性非流式，见下方「分类器」） |

`api` 可在 provider 级设置（默认为该 provider 全部模型），也可在每个模型上单独覆盖。
表里只有内置实现；扩展还可以为自己的 `api` 名注册协议实现（见
[extensions.md](extensions.md#协议实现图片与分类器)），此时 `api` 可以是任意自定义名。

### 图片生成

目录里 `type: "image"` 的条目（如 openrouter 的 FLUX / `google/gemini-3-pro-image`）
只用于图片生成，不会出现在 `/model` 选择器里；它们的 `api` 在目录里**显式写成** `openrouter-images`
（prux 没有按 `type` 推断的默认：缺 `api` 的条目一律当 `openai-completions`，发图片请求会报
`does not support image generation`），也可以是扩展注册的图片协议实现。
调用方式见 [sdk.md](sdk.md#图片生成)：`find_model_of_type(provider, id, ModelType::Image)`
取条目 → `model_config_from_entry` → `provider::generate_images`。

`models.json` 里的自定义条目也可以声明 `"type": "image"` 与 `"output"`（原样透传），
`api` 为 `openrouter-images` 或扩展注册的实现即可走同一条路径。

### 分类器

目录里 `type: "classifier"` 的条目只用于结构化分类，不会出现在 `/model` 选择器里；
它们的 `api` 在目录里**显式写成** `typesafe-system-one`（TypeSafe 的 System One 协议，
OpenRouter / TypeSafe / Vercel AI Gateway / OpenCode Zen 共用，只是 `baseUrl` 不同，
请求发到 `<baseUrl>/systemone`；内置 `typesafe` provider 的 `jev-latest` 用
`TYPESAFE_API_KEY` 或 `/login typesafe` 的 API key），也可以是扩展注册的分类器实现。
调用方式见 [sdk.md](sdk.md#分类器)：`find_model_of_type(provider, id, ModelType::Classifier)`
取条目 → `model_config_from_entry` → `provider::classify`。

`models.json` 里的自定义条目也可以声明 `"type": "classifier"`，
只要 `api` 是 `typesafe-system-one` 或扩展注册的实现即可走同一条路径。

### 虚拟模型

虚拟模型（`api = prux-virtual`）**不写在 models.json**：它们由扩展/ SDK 注册，
每次请求路由到一个物理模型 + thinking 级别，并像普通模型一样出现在 `/model` 里。
`prux-virtual` 不是可配置的协议实现——未经路由直接发给它（如 `/proxy` 网关）会报
`unsupported api type: prux-virtual`。用法见 [virtual-models.md](virtual-models.md)。

## Provider 字段

| 字段 | 说明 |
|------|------|
| `baseUrl` | API 端点。自定义 provider 必须给出（或模型级给出）；覆盖内置 provider 时替换全部模型的 baseUrl（换代理） |
| `api` | API 类型（见上表）。**只作用于同一个 `providers.<p>.models` 数组里未自带 `api` 的条目**，不会改写内置 provider 的基线模型 |
| `apiKey` | API key 配置，支持四类值（见下方「值解析」）。省略时使用 `/login` 凭据、`--api-key` 或内置环境变量 |
| `headers` | 自定义请求头（同样支持值解析） |
| `authHeader` | `true` 时自动加 `Authorization: Bearer <apiKey>` |
| `compat` | provider 兼容性覆盖，合并进该 provider 的 **chat 条目**（已存在的 image / classifier 条目不变；`models` 数组里新声明的条目无差别合并）（如 Ollama 不认 `developer` 角色时设 `supportsDeveloperRole: false`） |
| `models` | 模型定义数组 |
| `modelOverrides` | 按模型 id 覆盖内置/已合并模型的字段（`name`、`reasoning`、`input`、`cost`、`contextWindow`、`maxTokens`、`samplingParams`、`headers`、`compat` 等） |

## Model 字段

| 字段 | 缺省 | 说明 |
|------|------|------|
| `id` | —（必填） | 模型标识（传给 API） |
| `name` | `id` | 展示名 |
| `api` | provider 的 `api` | 模型级 API 覆盖 |
| `reasoning` | `false` | 支持扩展思考 |
| `input` | `[]` | 输入类型 `["text"]` / `["text", "image"]`（不写则视为空，`/model` 面板不显示 `· text` 标记） |
| `type` | `chat` | 模型用途：`chat` / `image` / `classifier`（图片生成见下方「图片生成」，分类器见下方「分类器」） |
| `output` | `[]` | 输出模态，仅图片模型使用（`["image"]` 或 `["image", "text"]`，后者允许同时返回文本） |
| `contextWindow` | `128000` | 上下文窗口（token） |
| `maxTokens` | `16384` | 最大输出 token（`0` 表示不限制） |
| `inputLimits` | 见说明 | 请求/图片入参上限：`maxRequestBytes`（单次请求序列化后的字节上限）、`images.maxPerMessage` / `images.maxPerRequest`（张数）、`images.resize.maxWidth` / `maxHeight`（缩放后像素，缺省 2000×2000）、`maxBytes`（base64 载荷字节，缺省约 4.5 MB）、`jpegQuality`（缺省 80） |
| `samplingParams` | — | 原样合并进每个请求体的采样参数（如 `min_p`、`top_k`）；只补请求体未设的键，不覆盖显式 `temperature`/`max_tokens`/thinking 等 |
| `samplingParamsByThinkingLevel` | — | 按**有效思考级别**覆盖采样参数：键为 `off`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max`，值为该级别的参数对象（见下方「按思考级别配采样参数」） |
| `cost` | 全零 | 每百万 token 价格 |
| `headers` | — | 模型级请求头（覆盖 provider 级同名头） |
| `thinkingLevelMap` | — | 思考级别映射（`off`…`max` → provider 值）。键缺失=支持该级别并回退为级别名；`null`=不支持该级别；字符串=该级别的 provider 值（见下方「thinkingLevelMap 与 off 的语义」） |

输出 token 的字段名不是模型字段，而是 `compat.maxTokensField`（`max_completion_tokens` 缺省 / `max_tokens`），见 [custom-provider.md](custom-provider.md#供应商字段compat-与-quirk)。

### 按思考级别配采样参数

`samplingParamsByThinkingLevel` 只在 OpenAI 兼容协议（`openai-completions` / `openai-responses`）上生效，
让同一个模型在不同思考档位用不同采样参数：

```json
{
  "id": "qwen-thinking-model",
  "reasoning": true,
  "samplingParams": { "temperature": 1.0, "top_p": 0.95 },
  "samplingParamsByThinkingLevel": {
    "off":  { "temperature": 0.7, "top_p": 0.8 },
    "high": { "top_k": 20 }
  }
}
```

键是 **prux 的思考级别**（不是 `thinkingLevelMap` 里的 provider 值）。取值时先把当前级别按模型能力钳制
（`low` 被声明为不支持时落到最近的可用级别），再按「模型 `samplingParams` → 该级别的覆盖项」合并，
后写胜；级别未列出的键继承模型默认值。**级别覆盖项的键会覆盖请求体里的同名显式字段**（包括 `temperature`），
与 pi 一致；`settings.json` 的 `temperature` 是用户级覆盖，优先级最高，不被级别覆盖项改写。
`modelOverrides` 里的同名字段按级别、按键合并。

### thinkingLevelMap 与 off 的语义

`thinkingLevelMap` 把 prux 的思考级别（`off`…`max`）映射到 provider 参数值。每个键有三种状态，
其中 **`off` 键缺失** 与 **`off: null`** 的含义完全不同：

| `off` 的写法 | 含义 | `thinking=off` 时 |
|--------------|------|-------------------|
| **键缺失** | 未声明 → 视为「支持 off」，关闭值回退为级别名 | 级别保持 `off`，请求按 `thinkingFormat` 发关闭表达（如 `thinking.type: "disabled"`） |
| **`off: null`** | 显式声明「不支持关闭」 | 钳制到最近可用级别（通常是 `low`），请求仍开启思考 |
| `off: "none"`（或其它字符串） | 支持 off，关闭值为该字符串 | 级别保持 `off`，请求发该关闭值（如 `reasoning_effort: "none"`） |

其余级别同理：`off`…`high` 键缺失视为支持，`xhigh`/`max` 必须显式映射；`null` 表示不支持；
字符串表示该级别的 provider 值。

实际生效示例（prux 会将级别钳制到模型支持范围，再据此构造请求）：

| 模型 | `thinkingLevelMap.off` | 设为 `off` 后实际级别 | 请求 |
|------|------------------------|---------------------------|------|
| `deepseek/deepseek-flash` | 键缺失 | `off` | `thinking: { "type": "disabled" }` |
| `opencode-go/deepseek-v4.1-flash` | `null` | `low` | `thinking: { "type": "enabled" }` + `reasoning_effort: "low"` |
| `opencode-go/hy3` | `"none"` | `off` | `reasoning_effort: "none"` |
| `opencode-go/mimo-v2.5` | 无 `thinkingLevelMap`（`off` 键缺失） | `off` | 不发思考参数（缺省 openai 风格在无 `off` 映射时不发关闭值），网关默认开启 → 仍会思考 |

因此「设置 off 后仍输出思考」通常有两种原因：

1. 目录把 `off` 声明为 **`null`**（如 `opencode-go/deepseek-v4.1-flash`）——级别被钳制到 `low`，思考实际上仍开启；
2. 模型本身没有可关闭的映射（如 `mimo-v2.5`）——虽然级别是 `off`，但参数层发不出关闭值，只能依赖网关默认。

要强制关闭第 2 类模型，可在 `models.json` 的 `modelOverrides` 里补上 `thinkingLevelMap.off`（如 `"none"`），
前提是上游接受该关闭值。请求参数的具体形态由 `compat.thinkingFormat` 决定，见
[custom-provider.md](custom-provider.md) 的「`thinkingFormat`」一节。

## 覆盖内置 provider

只换代理不改模型：

```json
{
  "providers": {
    "anthropic": {
      "baseUrl": "https://my-proxy.example.com/v1"
    }
  }
}
```

合并自定义模型（按 `id` upsert：同名替换、新 id 追加）：

```json
{
  "providers": {
    "deepseek": {
      "baseUrl": "https://my-proxy.example.com",
      "models": [{ "id": "my-custom-model", "contextWindow": 64000 }]
    }
  }
}
```

模型的缺省字段继承顺序（**只在同 `type` 的条目间**）：同 id 已有条目 → 同 `api` 的条目 → （chat 才有）`openai-completions` 的条目 → 该类型第一个条目；此外 provider 级 `baseUrl` 作用于全部类型、provider 级 `api` 只作为同数组内新条目的回退值。

## 值解析

`apiKey` 与 `headers` 的值支持四种形式：

- **shell 命令**：以 `!` 开头时整值作为命令执行，取 stdout（trim）
  ```json
  "apiKey": "!security find-generic-password -ws 'anthropic'"
  ```
- **环境变量**：`$ENV_VAR` 或 `${ENV_VAR}`；插值可嵌在更长的字面量里
  ```json
  "apiKey": "$MY_API_KEY"
  "apiKey": "${KEY_PREFIX}_${KEY_SUFFIX}"
  ```
  `$FOO_BAR` 是变量 `FOO_BAR`；想让 `BAR` 成为字面量用 `${FOO}_BAR`。缺失的环境变量使该值无法解析。
- **转义**：`$$` 输出字面 `$`；`$!` 输出字面 `!`（不触发命令）
- **字面量**：直接使用（如 `"sk-..."`、Ollama 的占位 `"ollama"`）

模型名带冒号（如 `llama3.1:8b`）不会被误判为 thinking 级别：只有 `off`/`minimal`/`low`/`medium`/`high`/`xhigh`/`max` 后缀才会被当作 thinking 参数。

## 可用性

- 凭据来源判定顺序：`auth.json` 凭据 > `models.json` 的 provider 级 `apiKey` > 内置环境变量。
  前一个来源存在时，后面的不再参与判定。
- 配置了 `apiKey` 的 provider 会出现在 `/model` 面板与 Ctrl+P 循环（`auth.json` 凭据之外的第二种配置来源）。
- `apiKey` 是 shell 命令时恒视为已配置（可用性检查不执行命令）；环境变量引用需要变量存在。
  **声明了 `apiKey` 但引用缺失时**该 provider 视为未配置，**不会**回退到内置环境变量。
- 内置环境变量（`DEEPSEEK_API_KEY`、`OPENAI_API_KEY` 等，见 `api_key_env_var` 映射）存在且非空时，
  对应 provider 同样视为已配置并出现在 `/model`（`environment` 认证来源）；仅在 `models.json`
  未给该 provider 声明 `apiKey` 时生效。
- **本地免鉴权端点也要写占位 `apiKey`**（如 `"ollama"`）：未解析到任何凭据的请求会被拒为
  `Provider is not configured: <provider>`（对齐 pi），不会发出一个空 key 的请求。
- `/login` API key 列表的认证来源三态：`✓ stored`（auth.json 已存）、`✓ env (VAR)`（仅环境变量）、
  `✓ stored · env (VAR)`（两者并存）、`• not configured`（未配置）。
- 未配置任何凭据（`auth.json`、`models.json`、环境变量均无）时，启动提示运行 `/login`。

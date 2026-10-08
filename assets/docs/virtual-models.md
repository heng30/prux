# 虚拟模型（Virtual models）

虚拟模型（virtual model）是一个可被选中的模型，它在**每次请求**时挑选一个物理模型。
用来按任务、成本或对话状态路由：比如让路由器把简单问题发给小模型、难题发给大模型，
而用户只选一个模型。

虚拟模型由扩展注册（见 [扩展](extensions.md)），随后像普通模型一样出现在 `/model`、
`--model`、scoped models 与设置里。它可以挂在任何 provider 下，包括已有物理模型的
provider（如 `deepseek/auto`）。

本文讲的“路由”只针对 **chat 型**虚拟模型；同一套注册面还能声明 image / classifier 的
操作型条目（见下方「操作型条目」）。

## 选择与派发

虚拟模型选定一个模型 + 一个 thinking 级别；路由器为每个请求把这一对映射到物理的一对：

```
选中（虚拟模型, 虚拟级别）  ->  派发（物理模型, 物理级别）
router/auto:high           ->  anthropic/claude-opus-4-8:medium
```

虚拟 thinking 级别只是路由器的**输入**，含义由路由器自行解释，不必对应推理预算。

prux 把「选择」与「派发」分得很清楚：

| | 选择 | 派发 |
|---|---|---|
| 记录在 | `model_change` 与 `thinking_level_change` 条目 | 每条 assistant 消息的 `provider` / `api` / `model` / `thinkingLevel` |
| 可见为 | `agent.model`、`/model`、`--list-models`、footer 左侧 | 每条响应的 assistant 消息 |

provider 层只收到物理模型；assistant 消息记的是物理模型，因此跨物理模型重放一段对话
与手动切换模型后重放完全一样。`/session` 按**物理模型**分列成本，footer 在选中模型
之后显示实际路由（`auto:high → gpt-5.5:medium`）。

上下文占用按「最近一次成功响应所用物理模型」的窗口计算；还没有任何响应时用虚拟模型
声明的窗口（未声明按 0 = 未知）。压缩同样如此：虚拟选择在阈值压缩时先路由，再按
**路由到的**那个模型判断并压缩，路由本身保持不变。

## 操作型条目（image / classifier）

`VirtualModelDefinition::operation(provider, id, name, type, api)` 注册的是**操作型条目**：
它不参与路由（没有路由器），只是给目录添一条可调用的 image / classifier 模型——
`api` 指向内置实现（`openrouter-images` / `typesafe-system-one`）或随条目给出的扩展实现
（`with_image_impl()` / `with_classifier_impl()`；也有不带条目的 `Extension::image_apis()` /
`classifier_apis()`，见 [extensions.md](extensions.md#协议实现图片与分类器)）。`with_base_url` /
`with_cost` / `with_output` 提供请求基地址、单价与输出模态。

对操作型条目调用路由（`resolve_route`）会以错误结束（"is not a chat model"）。

## 注册虚拟模型

虚拟模型由扩展提供：实现 [扩展](extensions.md) 的 `Extension::virtual_models()`，
返回 `VirtualModelDefinition`。定义里带一个路由器（`VirtualModelRouter`）。

```rust
use prux::core::virtual_models::{
    ModelRoute, ModelRouteRequest, VirtualModelDefinition, VirtualModelRouter,
};
use std::sync::Arc;

struct Router;

impl VirtualModelRouter for Router {
    fn route<'a>(&'a self, request: ModelRouteRequest<'a>) -> BoxFuture<'a, Result<ModelRoute>> {
        Box::pin(async move {
            // 工具续跑与重试留在处理本轮的那个模型上（保住 prompt 缓存与 thinking 签名）
            let sticky = request.failed.as_ref().map(|f| (&f.provider, &f.model_id))
                .or_else(|| request.previous.as_ref().map(|p| (&p.provider, &p.model_id)));
            if request.reason != ModelRouteReason::User
                && let Some((provider, model_id)) = sticky
            {
                return Ok(ModelRoute {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                    thinking_level: Some("medium".into()),
                    state: None,
                });
            }
            let (model_id, level) = if request.thinking_level == Some("high") {
                ("claude-opus-4-8", "medium")
            } else {
                ("claude-haiku-4-5", "medium")
            };
            Ok(ModelRoute {
                provider: "anthropic".into(),
                model_id: model_id.into(),
                thinking_level: Some(level.into()),
                state: None,
            })
        })
    }
}

impl Extension for MyExtension {
    fn name(&self) -> &str { "my-router" }
    fn tools(&self) -> Vec<ExtensionTool> { Vec::new() }
    fn virtual_models(&self) -> Vec<VirtualModelDefinition> {
        vec![VirtualModelDefinition::new("router", "auto", "Auto", Arc::new(Router))
            .with_thinking_levels(["low", "high"])
            .with_limits(200_000, 32_000)]
    }
}
```

要点：

- `provider` 是模型挂载的 provider id，可以是任何 id。挂在有物理模型的 provider 上时，
  该 provider 有凭据才可用；挂在一个没有 provider 使用的 id 上时**始终可用**。
- `id` 不应与同 provider 的物理模型同名。若同名，虚拟模型**隐藏**物理条目（目录刷新
  后新出现的同名物理条目也照样被隐藏）。
- `thinking_levels` 列出可选级别，缺省 `["off"]`；空列表/非法值会被过滤。
- `context_window` / `max_tokens` 在首次响应前展示；未设置为 0（未知）。
- `input` 列出可选输入模态，缺省 `text` + `image`。
- 注册与扩展同生命周期：扩展被 `/extension` 面板禁用、`--no-extensions` 关闭或模式
  不匹配时，它提供的虚拟模型立即从目录与路由中消失，无需重新注册。
  （SDK 也可不经扩展直接调用 `virtual_models::register(def, "")` 注册。）

## 路由请求

`route(request)` 在**每次**使用该虚拟模型的请求前运行，返回 `{provider, model_id,
thinking_level, state?}`。目标必须是目录里存在、且其 provider 有凭据的物理模型。虚拟
模型不能再路由到另一个虚拟模型。prux 会把返回的 thinking 级别**钳制**到目标模型支持的
范围。

| 字段 | 含义 |
|---|---|
| `selected` | 被选中的虚拟模型（不是路由目标） |
| `thinking_level` | 选中的虚拟 thinking 级别 |
| `reason` | 为什么发这个请求，见下 |
| `previous` | `messages` 里最近一次成功响应命中的物理模型与级别 |
| `failed` | 仅 `retry`：失败尝试的物理模型、级别与错误信息（该响应已从 `messages` 移除）。路由本身失败时为 `None` |
| `state` | 该会话分支上最近一次返回的路由器状态，见下 |
| `messages` | 本次请求的完整对话（含 system 消息） |
| `abort` | 当前运行的取消信号（`Arc<AtomicBool>`） |

| `reason` | 请求 |
|---|---|
| `user` | 用户写下一条消息之后的第一个请求（含 steer / follow-up） |
| `continuation` | agent 循环里的其它请求，如工具结果或扩展消息之后 |
| `retry` | 请求失败后的自动重试，含上下文溢出后的压缩重试 |
| `direct` | agent 循环之外的请求，如压缩摘要或扩展直接调用 |

`continuation` 返回 `previous`、`retry` 返回 `failed` 能保住 prompt 缓存与 thinking 签名。
跨轮换模型是允许的，但会丢一次 prompt 缓存。重试也可以换模型，例如 `failed.error_message`
表明供应商过载或上下文溢出。

`route()` 返回错误、或返回一个不存在/无凭据的模型时，该请求以 `stopReason=error` 的
assistant 消息结束（消息名指向虚拟模型本身）。

## 保留路由状态

`route()` 可以顺带返回 `state`。prux 把它存到会话分支上（custom 条目
`prux.virtual-model-state`），下次请求作为 `request.state` 传回。用于对话记录里没有的
决策，比如分类器结果或路由阶段：

```rust
// 阶段保持：plan 阶段用强模型，build 阶段换成便宜模型
let state = request.state.clone().unwrap_or(json!({ "phase": "plan" }));
let model_id = if state["phase"] == "plan" { "claude-opus-4-8" } else { "claude-haiku-4-5" };
ModelRoute { provider: "anthropic".into(), model_id: model_id.into(),
             thinking_level: Some("medium".into()), state: Some(state) }
```

- `state` 必须可 JSON 序列化。返回 `None` 或原样返回 `request.state` 表示保持当前状态。
- 返回与当前状态**不同的**对象时会在请求发出前先落库；与当前状态相等则不写条目。
  只在状态真的变化时返回新对象。请求随后失败也不会撤掉状态。
- 状态跟随会话树：fork 与 `/tree` 导航看到各自分支的状态，且能在压缩中存活。
- `direct` 请求没有状态，返回的 `state` 会被忽略。

路由器可以调用别的模型，例如用 [`provider::classify`](models.md) 搭配 `type: classifier` 条目；
这会在本轮第一个 token 之前增加延迟。

## 开箱用例：`jet` 扩展

`jet` 是内置扩展（**默认不启用**）里唯一的虚拟模型消费者：一个"强模型规划、便宜模型
实现"的路由器，移植自 pi 的示例插件 `examples/extensions/jev-router.ts`。它把上面几节
的能力凑成一个可直接用的形态：

- 注册 `jet/auto`（provider `jet`），按**会话相位**路由：规划档用 `planning`
  （分类器把题面判为复杂时用 `planningComplex`），本轮第一次成功的 `edit`/`write`
  之后同轮切到 `implementation` 档并留在那里——每个会话最多切一次模型。
- 三档各自带 **provider + 模型 id**，可以来自不同 provider（`jet.json` 的 `router`
  小节形如 `{"planning": {"provider": "...", "model": "..."}, ...}`）。
- 相位用 `state` 落盘（custom 条目 `prux.virtual-model-state`），跟着会话树走、能扛压缩；
  沿用同一档时不重复落状态条目（只换 provider 也仍算同一档，按模型 id 认）。
- 顺序：`/jet router --provider <p> --planning <id> --implementation <id>`
  （分属不同 provider 时再加 `--planning-provider` / `--planning-complex-provider` /
  `--implementation-provider`）→ `/jet set <classifier-provider> <classifier-model-id>`
  → 在 `/model` 面板选中 `jet/auto`。
- 路由目标不完整或解析不到时**不注册**：`jet/auto` 不会出现在 `/model` 里。
- 分类器调用的用量不计入会话成本（与 pi 一致）。

完整语义、配置 schema 与命令清单见 [extensions.md](extensions.md) 的 `jet` 一节。

## 与 pi 的差异（已知且刻意）

- **示例插件变成了内置扩展**：pi 的 `examples/extensions/jev-router.ts` 在 prux 里是内置
  扩展 `jet`（默认不启用，见上节）；路由逻辑写在 `Extension::virtual_models()` 里
  （本页示例即完整形态）。
- **`unregisterVirtualModel(provider, id)` 不单独暴露**：虚拟模型随扩展整体注册/注销
  （`virtual_models::unregister` / `unregister_owner` 供 SDK 与测试用）。
- **SDK 直接注册的「仅有虚拟模型」provider 不进已配置列表**：扩展注册的虚拟模型会让
  一个全新 provider 自动可用；不经扩展注册时，请先把该 provider 写进 `models.json`
  （带 `apiKey`），否则 `/model` 不会列出它。
- **`/proxy` 不路由虚拟模型**：网关按请求模型直接 `stream_chat`，虚拟条目会报
  `unsupported api type: prux-virtual`。虚拟模型只服务 agent 循环与扩展直调。
- **`/resume` 不恢复模型选择**：这是 prux 既有行为（不按 `model_change` 恢复），虚拟
  选择同样只随显式 `/model` 切换。
- 没有 `signal`（用 `abort`）、没有 `onPayload` / `onResponse` / `maxRetries` 选项，
  与 provider 层其它入口一致。

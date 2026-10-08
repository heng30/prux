# SDK

prux 提供 Rust 库形式的编程接口，可嵌入其他应用、构建自定义界面或自动化流程。

**典型用途：**
- 构建自定义 UI（TUI、Web、桌面）
- 把 agent 能力集成进现有应用
- 自动化流水线
- 程序化测试 agent 行为

## 依赖

在 `Cargo.toml` 中引入：

```toml
[dependencies]
prux = { path = "/path/to/prux" }
tokio = { version = "1", features = ["full"] }
# 虚拟模型路由器签名用 BoxFuture（见下文「虚拟模型」）
futures-util = "0.3"
```

## 快速开始

```rust
use prux::core::agent_session::{Agent, ToolSelection};
use prux::core::model_resolver::find_model;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // 从模型目录查找模型（与 CLI 相同的配置来源）
    let model_entry = find_model("opencode-go", "deepseek-v4-flash")?;

    let mut agent = Agent::new(
        model_entry,
        std::env::current_dir()?.to_string_lossy().to_string(),
        ToolSelection::tools(vec!["read".into(), "bash".into(), "edit".into(), "write".into()]),
        Vec::new(),           // exclude_tools（--exclude-tools）
        Some("high".into()),  // thinking 级别
        None,                  // 自定义系统提示（None = 默认）
        None,                  // append system prompt
        Vec::new(),            // AGENTS.md 上下文文件
        None,                  // 会话（None = 无持久化）
        None,                  // API key 覆盖
        false,                 // offline
        Vec::new(),            // skills (name, description, path)
    )?;

    let reply = agent.prompt("当前目录有什么文件？").await?;
    println!("{}", reply);
    Ok(())
}
```

## 核心模块

| 模块 | 说明 |
|------|------|
| `core::agent_session::Agent` | 主入口：`new()` 构建，`prompt()` 发送消息并执行工具循环，`switch_model()` 切换模型，`maybe_compact()` 上下文压缩 |
| `core::provider` | 模型配置（`ModelConfig`）、消息类型（`AgentMessage`/`ContentBlock`）、流式请求（`stream_chat`）、非流式（`simple_completion`） |
| `core::session_manager::Session` | JSONL v4 会话：`create`/`open`/`continue_recent`、`append_message`、`build_context_messages`、压缩 entry |
| `core::tools` | 内置工具：`execute_read/write/edit/bash/grep/find/ls`（可独立调用） |
| `core::model_resolver` + `core::settings_manager` | `find_model`/`list_models`，读取 `~/.prux`（或 `PRUX_AGENT_DIR`）配置与目录；凭据读取另有 `core::auth`（`read_auth_key` 等） |
| `core::compaction` | token 估算、`serialize_conversation`、摘要 prompt 构建、文件清单统计 |
| `core::export_html` | 会话导出为 HTML（复用内嵌 HTML 模板） |
| `core::skills` / `core::prompt_templates` | SKILL.md 发现与 `/skill:name`、prompt template 发现与展开 |
| `cli::args` / `modes::interactive` | CLI 参数解析、交互 TUI（CLI 内部使用） |

## 事件流（流式输出）

`Agent` 支持注入事件 sink，接收与 TUI 模式相同的内核事件：

```rust
use prux::core::agent_session::Agent;

let mut agent = /* ... */;
// JsonSink = Box<dyn FnMut(Value) + Send>；内部包 Arc+Mutex，用 attach_json_sink 挂接
agent.attach_json_sink(Box::new(|event: serde_json::Value| {
    if let Some(ev) = event.get("assistantMessageEvent") {
        if ev.get("type").and_then(|t| t.as_str()) == Some("text_delta") {
            if let Some(d) = ev.get("delta").and_then(|d| d.as_str()) {
                print!("{}", d);
            }
        }
    }
}));

let reply = agent.prompt("流式输出示例").await?;
```

事件类型：`agent_start` / `turn_start` / `message_start` / `message_update`（`text_delta`、`thinking_delta`、`toolcall_delta` 等）/ `message_end` / `tool_execution_start|end` / `turn_end` / `agent_end`。

## 会话持久化

```rust
use prux::core::session_manager::Session;

// 创建新会话（写入 <agent_dir>/sessions/--<cwd 编码>--）
let mut session = Session::create(cwd, None, true)?;

// 继续最近会话（-c 行为）
let session = Session::continue_recent(cwd, None, true)?;

// 打开指定会话
let session = Session::open("/path/to/session.jsonl")?;

// 恢复上下文消息（沿 parentId 链回溯）
let messages = session.build_context_messages();
```

## 直接调用工具

```rust
use prux::core::tools;

let result = tools::execute_read("Cargo.toml", None, None, cwd).await?;
let result = tools::execute_bash("cargo --version", None, cwd).await?;
let result = tools::execute_grep("fn main", Some("src"), None, false, false, None, None, cwd).await?;
```

## 自定义模型

`ModelConfig` 可以直接构造（不依赖模型目录），但字段有 30 多个且**没有 `Default`**：
实际用法是从目录条目转（`model_config_from_entry`），或按需列全字段。

```rust
use prux::core::model_resolver::{find_model, model_config_from_entry};

// 推荐：从目录条目转，字段与缺省继承都由目录负责
let entry = find_model("opencode-go", "deepseek-v4-flash")?;
let model = model_config_from_entry(&entry, None, None);
```

直接手写字面量时（如自建一个目录里没有的模型）需给全字段：

```rust
use prux::core::provider::ModelConfig;

let model = ModelConfig {
    provider: "my-provider".into(),
    model_id: "my-model".into(),
    base_url: "https://example.com/v1".into(),
    api_key: std::env::var("MY_API_KEY").unwrap_or_default(),
    ..model_config_from_entry(&find_model("opencode-go", "deepseek-v4-flash")?, None, None)
};
```

## 模型目录

列目录里的模型（`CatalogModel { provider, id, name, kind }`）：

```rust
use prux::core::model_resolver::{
    list_all_available_models_of_type, list_all_models, list_models, list_models_of_type,
};
use prux::core::provider::ModelType;

// 单个 provider（chat / 指定类型）
let chat = list_models("anthropic");
let images = list_models_of_type("openrouter", ModelType::Image);

// 全部 provider（含未配置凭据的）
let every = list_all_models();
let classifiers = list_all_models_of_type(ModelType::Classifier);

// 只含已配置凭据的 provider（等价于 pi 的 Models.getAllAvailable()）
let usable = list_all_available_models_of_type(ModelType::Chat);
```

需要完整字段（`api`、上下文窗口、计费单价等）时，用 `find_model_of_type(provider, id, kind)`
取条目再 `model_config_from_entry(&entry, None, None)` 转成请求用的 `ModelConfig`。

## 图片生成

图片模型取自目录的 `type: "image"` 条目（`api` 为 `openrouter-images`，
或扩展注册的图片协议实现，见 [extensions.md](extensions.md#协议实现图片与分类器)），
与 chat 共用同一份 provider 凭据。调用是一次性的（非流式），**失败不抛错**——
`stop_reason` 为 `"error"` 时看 `error_message`；输出里的图片是 base64，落盘/展示由调用方自理。

```rust
use prux::core::model_resolver::{find_model_of_type, model_config_from_entry};
use prux::core::provider::{ImageContent, ModelType, generate_images};

let entry = find_model_of_type("openrouter", "google/gemini-3-pro-image", ModelType::Image)?;
let model = model_config_from_entry(&entry, None, None);
let result = generate_images(&model, &[ImageContent::Text { text: "a red circle".into() }]).await;

for block in &result.output {
    match block {
        ImageContent::Text { text } => println!("{text}"),
        ImageContent::Image { data, mime_type } => {
            // data 是 base64（不含 data: 前缀）
            println!("{mime_type}, {} bytes(base64)", data.len());
        }
    }
}
```

- 列可用图片模型：`list_models_of_type(provider, ModelType::Image)`。
- 同一上游 ID 可能同时有 chat 与 image 条目（如 `google/gemini-3-pro-image`），
  按类型取条目必须用 `find_model_of_type`，否则可能拿到 chat 那条。
- 完整可运行示例：`cargo run --example image_gen -- --prompt "..."`。

## 分类器

分类器模型取自目录的 `type: "classifier"` 条目（`api` 为 `typesafe-system-one`，
或扩展注册的分类器协议实现）。内置的 `typesafe` provider 自带 `jev-latest`
（`TYPESAFE_API_KEY` / `/login typesafe`）。一次调用可问多道题，题型为 `choice`（选择）、
`score`（评分）、`bool`（判断，线上以 TypeSafe 的 `noul` 表示）；答案按问题 id 返回。
同样是**一次性非流式 + 失败不抛错**（看 `stop_reason` / `error_message`）；
`usage` 按目录单价计费，**答案格式不对的请求也会把它带回来**（请求已经发出并计费）。

```rust
use prux::core::model_resolver::{find_model_of_type, model_config_from_entry};
use prux::core::provider::{
    BoolCriteria, ClassifierAnswer, ClassifierContext, ClassifierQuestion, ModelType, classify,
};
use std::collections::BTreeMap;

let entry = find_model_of_type("openrouter", "~typesafe/jev-latest", ModelType::Classifier)?;
let model = model_config_from_entry(&entry, None, None);
let context = ClassifierContext {
    state: serde_json::json!({ "message": "The change works perfectly, thanks." }),
    questions: BTreeMap::from([
        ("approved".to_string(), ClassifierQuestion::Bool {
            instructions: "Does the user approve?".into(),
            criteria: BoolCriteria { yes: "Approval".into(), no: "No approval".into() },
        }),
    ]),
};

let result = classify(&model, &context).await;
match result.answers.get("approved") {
    Some(ClassifierAnswer::Bool { probability }) => println!("approve probability: {probability}"),
    _ => println!("failed: {}", result.error_message.unwrap_or(result.stop_reason)),
}
```

- 列可用分类器模型：`list_models_of_type(provider, ModelType::Classifier)`。
- 扩展也可以在运行时注册分类器模型（`VirtualModelDefinition::operation` + `with_classifier_impl`），
  见 [extensions.md](extensions.md#协议实现图片与分类器)。
- `probabilities` 只包含服务返回的选项，不保证覆盖 `criteria` 里的全部选项；
  不属于 `criteria` 的键也会原样带回来（服务端行为）。
- 完整可运行示例：`cargo run --example classify -- --text "..."`。

## 虚拟模型

不经扩展注册一个虚拟模型（SDK 直注册）：用 `virtual_models::register` 挂上定义，
再像普通模型一样用 `find_model` 取条目建 Agent。路由器每次请求返回物理模型 + thinking
级别，`state` 可挂在会话分支上传递。完整语义与限制见 [virtual-models.md](virtual-models.md)。

```rust
use prux::core::virtual_models::{
    self, ModelRoute, ModelRouteRequest, VirtualModelDefinition, VirtualModelRouter,
};
use prux::core::agent_session::{Agent, ToolSelection};
use prux::core::model_resolver::find_model;
use std::sync::Arc;

struct Router;
impl VirtualModelRouter for Router {
    fn route<'a>(
        &'a self,
        _r: ModelRouteRequest<'a>,
    ) -> futures_util::future::BoxFuture<'a, prux::error::Result<ModelRoute>> {
        Box::pin(async {
            Ok(ModelRoute {
                provider: "deepseek".into(),
                model_id: "deepseek-v4-pro".into(),
                thinking_level: Some("medium".into()),
                state: None,
            })
        })
    }
}

virtual_models::register(
    VirtualModelDefinition::new("router", "auto", "Auto", Arc::new(Router))
        .with_thinking_levels(["low", "high"]),
    virtual_models::STANDALONE_OWNER, // 无归属 = 不随扩展启停过滤
);
let entry = find_model("router", "auto")?;
let mut agent = Agent::new(entry, cwd, ToolSelection::default(), vec![], None, None, None, vec![], None, None, false, vec![])?;
```

> SDK 直注册的、**没有任何物理模型**的 provider 不会自动出现在「已配置 provider」
> 列表里；要让它出现在 `/model`，先把该 provider 写进 `models.json`（带 `apiKey`）。

## 示例：最小自动化

```rust
use prux::core::agent_session::{Agent, ToolSelection};
use prux::core::model_resolver::find_model;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let entry = find_model("opencode-go", "deepseek-v4-flash")?;
    let cwd = std::env::current_dir()?.to_string_lossy().to_string();

    let mut agent = Agent::new(
        entry,
        cwd,
        ToolSelection::tools(vec!["bash".into()]),
        Vec::new(),  // exclude_tools
        None,        // thinking_level
        None,        // system_prompt
        None,        // append_system_prompt
        Vec::new(),  // context_files
        None,        // session
        None,        // api_key_override
        false,       // offline
        Vec::new(),  // skills
    )?;

    let tasks = ["cargo --version", "rustc --version", "git log --oneline -3"];
    for task in tasks {
        let reply = agent.prompt(&format!("执行 {} 并总结输出", task)).await?;
        println!("--- {} ---\n{}", task, reply);
    }
    Ok(())
}
```

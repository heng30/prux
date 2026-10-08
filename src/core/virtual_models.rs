//! 虚拟模型（virtual models）：一种可选择的目录条目，每次请求路由到一个**物理模型**。
//!
//! 模型选择（`model_change` 条目 / `agent.model` / 各视图）可以指向一个虚拟模型；
//! 路由之后的每一步只看见物理模型：provider 层按物理模型发请求，assistant 消息记录物理模型。
//! 虚拟模型永远不会到达 provider 层。
//!
//! 虚拟模型属于某个 provider id，但不是该 provider 的物理模型：注册表单独持有它们，
//! 需要时（[`crate::core::model_resolver`] 的目录视图）与物理条目合并。
//! 因此任何 provider（包括已有物理模型的 provider）都可以挂若干虚拟模型。

use crate::{
    cli::args::VALID_THINKING_LEVELS,
    core::{
        extensions, model_resolver,
        provider::{
            AgentMessage, ModelConfig, ModelType,
            api_impls::{
                self, ClassifierApiImpl, ImageApiImpl, RegisteredClassifierApi, RegisteredImageApi,
            },
        },
        session_manager::Session,
        session_v4::Entry,
    },
    error::{Error, Result},
};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock, atomic::AtomicBool},
};
use strum_macros::IntoStaticStr;

/// 不属于任何扩展的虚拟模型（SDK 直接注册）：不随扩展启停过滤，始终可用。
pub const STANDALONE_OWNER: &str = "";

/// 虚拟模型目录条目的 api 标识；未经路由的请求落到它上面即报错。
pub const VIRTUAL_MODEL_API: &str = concat!(env!("CARGO_PKG_NAME"), "-virtual");

/// 会话分支上记录路由状态的 custom 条目类型。
pub const VIRTUAL_MODEL_STATE_ENTRY: &str = concat!(env!("CARGO_PKG_NAME"), ".virtual-model-state");

/// 虚拟模型的路由回调：为单次请求挑选物理模型与 thinking 级别。
pub trait VirtualModelRouter: Send + Sync {
    /// 解析一次请求。返回错误即该请求以错误响应结束。
    fn route<'a>(&'a self, request: ModelRouteRequest<'a>) -> BoxFuture<'a, Result<ModelRoute>>;
}

impl<F> VirtualModelRouter for F
where
    F: for<'a> Fn(ModelRouteRequest<'a>) -> BoxFuture<'a, Result<ModelRoute>> + Send + Sync,
{
    fn route<'a>(&'a self, request: ModelRouteRequest<'a>) -> BoxFuture<'a, Result<ModelRoute>> {
        self(request)
    }
}

/// 请求被路由的原因。
///
/// 字符串形式（`user` / `continuation` / `retry` / `direct`）由 `strum` 派生。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, IntoStaticStr)]
#[serde(rename_all = "lowercase")]
#[strum(serialize_all = "lowercase")]
pub enum ModelRouteReason {
    /// 用户写下一条消息之后的第一个请求（prompt / steer / follow-up）。
    User,
    /// agent 循环里的其它请求，例如工具结果或扩展消息之后。
    Continuation,
    /// 请求失败后的自动重试（含上下文溢出后的压缩重试）。
    Retry,
    /// agent 循环之外的请求，例如压缩摘要或扩展直接调用。
    Direct,
}

impl ModelRouteReason {
    /// 事件与文档用的字符串形式（`user` / `continuation` / `retry` / `direct`）。
    pub fn as_str(self) -> &'static str {
        self.into()
    }
}

/// 一条已发生的响应所命中的物理模型与 thinking 级别。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteTarget {
    /// 物理模型的 provider id。
    pub provider: String,
    /// 物理模型 id。
    pub model_id: String,
    /// 该响应实际使用的 thinking 级别；None 表示供应商未上报。
    pub thinking_level: Option<String>,
}

/// `retry` 请求对应的失败尝试（该响应已从 `messages` 里移除）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FailedRoute {
    /// 失败请求命中的物理 provider id。
    pub provider: String,
    /// 失败请求命中的物理模型 id。
    pub model_id: String,
    /// 失败请求使用的 thinking 级别；None 表示未记录。
    pub thinking_level: Option<String>,
    /// provider 给出的错误文本；None 表示无错误信息。
    pub error_message: Option<String>,
    /// 失败响应的 stop reason（通常为 `"error"`）。
    pub stop_reason: Option<String>,
}

/// 路由回调收到的单次请求描述。
pub struct ModelRouteRequest<'a> {
    /// 被选中的虚拟模型（**不是**路由目标）。
    pub selected: &'a ModelConfig,
    /// 选中的 thinking 级别；其含义由路由器自行解释。
    pub thinking_level: Option<&'a str>,
    /// 该请求被路由的原因。
    pub reason: ModelRouteReason,
    /// `messages` 里最近一次成功响应命中的物理模型与级别。
    pub previous: Option<RouteTarget>,
    /// 仅 `retry` 请求有值：失败尝试的物理模型、级别与错误信息。
    pub failed: Option<FailedRoute>,
    /// 该会话分支上最近一次由路由器返回的状态；`direct` 请求为 None。
    pub state: Option<&'a Value>,
    /// 本次请求的完整对话（含 system 消息）。
    pub messages: &'a [AgentMessage],
    /// 当前运行的取消信号；None 表示无取消通道。
    pub abort: Option<Arc<AtomicBool>>,
}

/// 路由回调返回的目标：物理模型 + thinking 级别 + 可选的新路由器状态。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelRoute {
    /// 目标物理模型的 provider id。
    pub provider: String,
    /// 目标物理模型 id。
    pub model_id: String,
    /// 目标 thinking 级别；None 表示沿用请求里选中的级别。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<String>,
    /// 新的路由器状态；None 表示保持当前状态（`direct` 请求忽略它）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<Value>,
}

/// 宿主校验通过后的路由结果：解析好的物理模型 + 钳制后的级别 + 新状态。
#[derive(Debug, Clone)]
pub struct ResolvedRoute {
    /// 目标物理模型配置（含本次请求的凭据 / 会话标志）。
    pub model: ModelConfig,
    /// 钳制到目标模型能力范围的 thinking 级别。
    pub thinking_level: Option<String>,
    /// 路由器返回的新状态；None 表示保持当前状态。
    pub state: Option<Value>,
}

/// 虚拟模型定义（扩展注册用）。
///
/// chat 型：每次请求由 [`VirtualModelRouter`] 路由到物理模型（[`VirtualModelDefinition::new`]）。
/// image / classifier 型（[`VirtualModelDefinition::operation`]）：不参与路由，只作为目录条目，
/// 调用时按 [`VirtualModelDefinition::api`] 分发到内置协议或扩展注册的实现
/// （见 [`crate::core::provider::api_impls`]）。
pub struct VirtualModelDefinition {
    /// 挂载的 provider id：可以是已有物理模型的 provider，也可以是全新 id。
    pub provider: String,
    /// 模型 id；与同 provider 的物理模型同名时隐藏后者。
    pub id: String,
    /// 展示名（`/model` 选择器）。
    pub name: String,
    /// 模型类型：chat 型可路由，image / classifier 型是目录条目。
    pub model_type: ModelType,
    /// 协议实现标识：chat 型默认 [`VIRTUAL_MODEL_API`]（未经路由即报错），
    /// image / classifier 型由扩展指定（内置 `openrouter-images` / `typesafe-system-one`
    /// 或扩展注册的实现）。
    pub api: String,
    /// 请求基地址；空串表示由协议实现自行决定。
    pub base_url: String,
    /// 单价表（目录 `cost` 形状）；None 表示未声明，按 0 计费。
    pub cost: Option<Value>,
    /// 可被选择的 thinking 级别列表；空列表按 `["off"]` 处理。
    pub thinking_levels: Vec<String>,
    /// 首次响应前展示的上下文窗口；0 表示未知。
    pub context_window: u32,
    /// 首次响应前展示的单次输出上限；0 表示未知。
    pub max_tokens: u32,
    /// 可选择的输入模态列表；空列表按 `["text", "image"]` 处理。
    pub input: Vec<String>,
    /// 输出模态列表（目录 `output`，图片模型用）；空表示只有文本。
    pub output: Vec<String>,
    /// 路由回调；非 chat 型为 None（没有物理模型可选）。
    pub router: Option<Arc<dyn VirtualModelRouter>>,
    /// 条目自带的图片生成实现；Some 时 [`register`] 按 [`Self::api`] 登记进
    /// [`api_impls`]（只有 `ModelType::Image` 条目生效）。
    pub image_impl: Option<Arc<dyn ImageApiImpl>>,
    /// 条目自带的分类器实现；与 [`Self::image_impl`] 同口径，只有 `ModelType::Classifier` 条目生效。
    pub classifier_impl: Option<Arc<dyn ClassifierApiImpl>>,
}

impl VirtualModelDefinition {
    /// 构造 chat 虚拟模型定义（输入模态默认 text+image、thinking 级别默认 off）。
    pub fn new(
        provider: impl Into<String>,
        id: impl Into<String>,
        name: impl Into<String>,
        router: Arc<dyn VirtualModelRouter>,
    ) -> Self {
        VirtualModelDefinition {
            provider: provider.into(),
            id: id.into(),
            name: name.into(),
            model_type: ModelType::Chat,
            api: VIRTUAL_MODEL_API.to_string(),
            base_url: String::new(),
            cost: None,
            thinking_levels: Vec::new(),
            context_window: 0,
            max_tokens: 0,
            input: Vec::new(),
            output: Vec::new(),
            router: Some(router),
            image_impl: None,
            classifier_impl: None,
        }
    }

    /// 构造 image / classifier 操作型定义：不参与路由，只有 `api` 与目录元数据。
    pub fn operation(
        provider: impl Into<String>,
        id: impl Into<String>,
        name: impl Into<String>,
        model_type: ModelType,
        api: impl Into<String>,
    ) -> Self {
        VirtualModelDefinition {
            provider: provider.into(),
            id: id.into(),
            name: name.into(),
            model_type,
            api: api.into(),
            base_url: String::new(),
            cost: None,
            thinking_levels: Vec::new(),
            context_window: 0,
            max_tokens: 0,
            input: Vec::new(),
            output: Vec::new(),
            router: None,
            image_impl: None,
            classifier_impl: None,
        }
    }

    /// 设置请求基地址（操作型条目用；chat 型由路由目标决定）。
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// 设置单价表（目录 `cost` 形状，图片 / 分类器的计费来源）。
    pub fn with_cost(mut self, cost: Value) -> Self {
        self.cost = Some(cost);
        self
    }

    /// 设置输出模态列表（图片模型用，如 `["image"]` / `["image", "text"]`）。
    pub fn with_output(mut self, output: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.output = output.into_iter().map(Into::into).collect();
        self
    }

    /// 设置可选择的 thinking 级别列表。
    pub fn with_thinking_levels(
        mut self,
        levels: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.thinking_levels = levels.into_iter().map(Into::into).collect();
        self
    }

    /// 设置上下文窗口与单次输出上限（均为 0 表示未知）。
    pub fn with_limits(mut self, context_window: u32, max_tokens: u32) -> Self {
        self.context_window = context_window;
        self.max_tokens = max_tokens;
        self
    }

    /// 设置可选择的输入模态列表。
    pub fn with_input(mut self, input: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.input = input.into_iter().map(Into::into).collect();
        self
    }

    /// 挂上条目自带的图片生成实现：`api` 名即 [`Self::api`]，不必再在
    /// [`crate::core::extensions::Extension::image_apis`] 里写第二遍。
    pub fn with_image_impl(mut self, implementation: Arc<dyn ImageApiImpl>) -> Self {
        self.image_impl = Some(implementation);
        self
    }

    /// 挂上条目自带的分类器实现：`api` 名即 [`Self::api`]，其余同 [`Self::with_image_impl`]。
    pub fn with_classifier_impl(mut self, implementation: Arc<dyn ClassifierApiImpl>) -> Self {
        self.classifier_impl = Some(implementation);
        self
    }

    /// 归一化后的 thinking 级别列表：过滤非法值，空则回退 `["off"]`。
    pub fn resolved_thinking_levels(&self) -> Vec<String> {
        let mut levels: Vec<String> = self
            .thinking_levels
            .iter()
            .map(|l| l.trim().to_string())
            .filter(|l| VALID_THINKING_LEVELS.contains(&l.as_str()))
            .collect();

        if levels.is_empty() {
            levels.push("off".to_string());
        }

        levels
    }

    /// 归一化后的输入模态列表：空则回退 text+image。
    pub fn resolved_input(&self) -> Vec<String> {
        if self.input.is_empty() {
            vec!["text".to_string(), "image".to_string()]
        } else {
            self.input.clone()
        }
    }
}

/// 注册表条目：定义 + 归属扩展名（空 = SDK 直接注册、始终可用）。
struct RegisteredVirtualModel {
    /// 虚拟模型定义。
    definition: Arc<VirtualModelDefinition>,
    /// 注册它的扩展名；[`STANDALONE_OWNER`] 表示无归属。
    owner: String,
}

/// 全局注册表：`(provider, id)` → 条目。
fn registry() -> &'static Mutex<BTreeMap<(String, String), RegisteredVirtualModel>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<(String, String), RegisteredVirtualModel>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// 是否虚拟模型的 api 标识（未经路由的请求据此拒绝）。
pub fn is_virtual_api(api: &str) -> bool {
    api == VIRTUAL_MODEL_API
}

/// 注册 / 替换一个虚拟模型；同 `(provider, id)` 覆盖旧定义。
///
/// 条目自带的 [`VirtualModelDefinition::image_impl`] / [`VirtualModelDefinition::classifier_impl`]
/// 一并按 [`VirtualModelDefinition::api`] 登记进 [`api_impls`]，与条目同属 `owner`：
/// 随扩展启停过滤，并随 [`unregister_owner`] / [`clear`] 一起下线。
/// 同名 `api` 以最后一次注册为准（[`crate::core::extensions::register_extension_arc`]
/// 先注册虚拟模型、再注册 `image_apis()`，因此后者能覆盖前者）。
pub fn register(definition: VirtualModelDefinition, owner: impl Into<String>) {
    let owner = owner.into();
    let api = definition.api.clone();

    // 类型不匹配的自带实现（如 chat 型挂了 image_impl）不登记，避免污染 `api` 命名空间。
    let image_impl = match definition.model_type {
        ModelType::Image => definition.image_impl.clone(),
        _ => None,
    };

    let classifier_impl = match definition.model_type {
        ModelType::Classifier => definition.classifier_impl.clone(),
        _ => None,
    };

    let key = (definition.provider.clone(), definition.id.clone());
    registry().lock().unwrap().insert(
        key,
        RegisteredVirtualModel {
            definition: Arc::new(definition),
            owner: owner.clone(),
        },
    );

    // 锁外登记：两个注册表互不嵌套，避免锁序问题。
    if let Some(implementation) = image_impl {
        api_impls::register_image_apis(
            owner.clone(),
            vec![RegisteredImageApi {
                api: api.clone(),
                implementation,
            }],
        );
    }

    if let Some(implementation) = classifier_impl {
        api_impls::register_classifier_apis(
            owner,
            vec![RegisteredClassifierApi {
                api,
                implementation,
            }],
        );
    }
}

/// 注销单个虚拟模型；返回是否实际移除。
///
/// 不触碰该条目的自带实现（同名 `api` 可能还被另一个条目或 `image_apis()` 注册着）；
/// 一并下线走 [`unregister_owner`]。
pub fn unregister(provider: &str, id: &str) -> bool {
    registry()
        .lock()
        .unwrap()
        .remove(&(provider.to_string(), id.to_string()))
        .is_some()
}

/// 注销某个扩展注册的全部虚拟模型（扩展注销 / 测试自清理用）。
///
/// 该扩展名下经 [`register`] 登记 / [`crate::core::extensions::Extension::image_apis`]
/// 声明的图片与分类器实现同时下线。
pub fn unregister_owner(owner: &str) {
    if owner == STANDALONE_OWNER {
        return;
    }

    registry()
        .lock()
        .unwrap()
        .retain(|_, entry| entry.owner != owner);

    api_impls::unregister_owner(owner);
}

/// 清空注册表与其中自带的实现（测试用）。
pub fn clear() {
    registry().lock().unwrap().clear();
    api_impls::clear();
}

/// 条目是否可用：无归属者恒可用，其余随扩展启用状态 / 扩展模式动态过滤。
fn is_entry_active(entry: &RegisteredVirtualModel) -> bool {
    entry.owner == STANDALONE_OWNER || extensions::is_extension_active(&entry.owner)
}

/// 当前可用的虚拟模型定义列表（按 provider、id 排序）。
pub fn active_definitions() -> Vec<Arc<VirtualModelDefinition>> {
    registry()
        .lock()
        .unwrap()
        .values()
        .filter(|e| is_entry_active(e))
        .map(|e| e.definition.clone())
        .collect()
}

/// 查找当前可用的虚拟模型定义。
pub fn find_active(provider: &str, id: &str) -> Option<Arc<VirtualModelDefinition>> {
    let reg = registry().lock().unwrap();
    let entry = reg.get(&(provider.to_string(), id.to_string()))?;
    is_entry_active(entry).then(|| entry.definition.clone())
}

/// 该 `(provider, id)` 是否为当前可用的虚拟模型。
pub fn is_active_virtual(provider: &str, id: &str) -> bool {
    find_active(provider, id).is_some()
}

/// provider 的可用虚拟模型 `(id, name)` 列表（全部类型），按 id 排序。
pub fn list_for_provider(provider: &str) -> Vec<(String, String)> {
    active_definitions()
        .into_iter()
        .filter(|d| d.provider == provider)
        .map(|d| (d.id.clone(), d.name.clone()))
        .collect()
}

/// provider 的指定类型虚拟模型 `(id, name)` 列表，按 id 排序。
pub fn list_for_provider_of_type(provider: &str, kind: ModelType) -> Vec<(String, String)> {
    active_definitions()
        .into_iter()
        .filter(|d| d.provider == provider && d.model_type == kind)
        .map(|d| (d.id.clone(), d.name.clone()))
        .collect()
}

/// 仅有虚拟模型（目录里没有任何物理条目）的 provider 列表。
///
/// 只统计**扩展注册**的虚拟模型：SDK 直接注册（[`STANDALONE_OWNER`]）的条目在
/// `/model` 等目录里仍可见（只要该 provider 已配置/已知），但不因此把一个全新 provider
/// 塞进「已配置 provider」列表——否则任何进程内的临时注册都会泄露到 UI 目录。
pub fn virtual_only_providers() -> Vec<String> {
    let mut out: Vec<String> = registry()
        .lock()
        .unwrap()
        .values()
        .filter(|e| is_entry_active(e))
        .filter(|e| e.owner != STANDALONE_OWNER)
        .filter(|e| model_resolver::provider_models(&e.definition.provider).is_empty())
        .map(|e| e.definition.provider.clone())
        .collect();
    out.sort();
    out.dedup();
    out
}

/// 从消息历史里找出**最近一次成功响应**命中的物理模型与 thinking 级别。
///
/// 失败与中止的响应（含路由本身失败、消息仍指向虚拟模型的情况）被跳过。
pub fn previous_route(messages: &[AgentMessage]) -> Option<RouteTarget> {
    for m in messages.iter().rev() {
        if m.role != "assistant" {
            continue;
        }
        if matches!(m.stop_reason.as_deref(), Some("error") | Some("aborted")) {
            continue;
        }
        if m.api.as_deref().is_some_and(is_virtual_api) {
            continue;
        }
        let (Some(provider), Some(model_id)) = (m.provider.as_deref(), m.model.as_deref()) else {
            continue;
        };
        return Some(RouteTarget {
            provider: provider.to_string(),
            model_id: model_id.to_string(),
            thinking_level: m.thinking_level.clone(),
        });
    }
    None
}

/// 读取会话当前分支上某个虚拟模型最近一次存储的路由状态。
pub fn read_state(session: &Session, provider: &str, model_id: &str) -> Option<Value> {
    let leaf = session.get_leaf_id()?;
    let entries = session
        .state
        .find_entries_on_branch(leaf, None, None, false)
        .ok()?;

    for entry in entries {
        let Entry::Custom {
            custom_type, data, ..
        } = entry
        else {
            continue;
        };

        if custom_type != VIRTUAL_MODEL_STATE_ENTRY {
            continue;
        }

        let Some(data) = data else { continue };
        if data.get("provider").and_then(Value::as_str) == Some(provider)
            && data.get("modelId").and_then(Value::as_str) == Some(model_id)
        {
            return data.get("state").cloned();
        }
    }
    None
}

/// 把路由器返回的新状态写进会话分支（custom 条目），返回条目 id。
pub fn append_state(
    session: &mut Session,
    provider: &str,
    model_id: &str,
    state: &Value,
) -> String {
    session.append_custom_entry(
        VIRTUAL_MODEL_STATE_ENTRY,
        Some(serde_json::json!({
            "provider": provider,
            "modelId": model_id,
            "state": state,
        })),
    )
}

/// 解析一次路由：查找路由器、调用它、校验目标并钳制 thinking 级别。
///
/// `api_key_override` / `session_id` 透传给物理模型解析（与
/// [`model_resolver::configured_model`] 同口径）。
pub async fn resolve_route(
    request: ModelRouteRequest<'_>,
    api_key_override: Option<String>,
    session_id: Option<String>,
) -> Result<ResolvedRoute> {
    resolve_route_with(request, |provider, id| {
        resolve_physical(provider, id, api_key_override.clone(), session_id.clone())
    })
    .await
}

/// [`resolve_route`] 的可注入版本：`resolve` 负责把 `(provider, id)` 解析成带凭据的物理模型。
pub async fn resolve_route_with(
    request: ModelRouteRequest<'_>,
    resolve: impl Fn(&str, &str) -> Result<ModelConfig>,
) -> Result<ResolvedRoute> {
    let provider = request.selected.provider.clone();
    let id = request.selected.model_id.clone();
    let name = format!("Virtual model {provider}/{id}");

    let definition = find_active(&provider, &id)
        .ok_or_else(|| Error::msg(format!("{name} is not registered.")))?;

    if definition.model_type != ModelType::Chat {
        return Err(Error::msg(format!(
            "{name} is not a chat model (type: {}).",
            definition.model_type.as_str()
        )));
    }

    let router = definition
        .router
        .clone()
        .ok_or_else(|| Error::msg(format!("{name} has no router.")))?;

    let requested_level = request.thinking_level.map(str::to_string);
    let route = router.route(request).await?;

    let target = resolve(&route.provider, &route.model_id).map_err(|e| {
        Error::msg(format!(
            "{name} routed to {}/{}, {e}",
            route.provider, route.model_id
        ))
    })?;

    if is_virtual_api(&target.api) {
        return Err(Error::msg(format!(
            "{name} routed to {}/{}, which is not a physical model.",
            route.provider, route.model_id
        )));
    }

    let level = route
        .thinking_level
        .clone()
        .or(requested_level)
        .unwrap_or_else(|| "off".to_string());

    // 路由目标必须支持该级别：钳制到目标模型的能力范围
    let thinking_level = target.clamp_thinking_level(&level);

    Ok(ResolvedRoute {
        model: target,
        thinking_level: Some(thinking_level),
        state: route.state,
    })
}

/// 默认的物理模型解析：目录解析 + 必需凭据校验。
///
/// 只取 **chat 条目**：路由目标必须是 chat 物理模型，同 id 的 image / classifier
/// 条目（如 OpenRouter 同 id 兼有的 image 条目）不能当目标，否则路由看着成功、
/// 发请求时才报 `unsupported api type`。
pub fn resolve_physical(
    provider: &str,
    id: &str,
    api_key_override: Option<String>,
    session_id: Option<String>,
) -> Result<ModelConfig> {
    let model = model_resolver::configured_model_of_type(
        provider,
        id,
        ModelType::Chat,
        api_key_override,
        session_id,
    )?;

    if is_virtual_api(&model.api) {
        return Err(Error::msg("which is not a physical model"));
    }

    if model.api_key.trim().is_empty() {
        return Err(Error::msg(format!(
            "which has no credentials. Run /login {} to store one.",
            model.provider
        )));
    }

    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        provider::{ContentBlock, ModelType},
        session_manager::Session,
    };

    /// 构造一个最小可用的物理模型配置（测试用，无网络）。
    fn physical(provider: &str, id: &str) -> ModelConfig {
        ModelConfig {
            model_type: ModelType::Chat,
            image_resize: crate::utils::image::ImageResizeLimits::default(),
            allowed_fallback_models: Vec::new(),
            provider: provider.into(),
            model_id: id.into(),
            base_url: "http://127.0.0.1:1".into(),
            api_key: "sk-test".into(),
            api: "openai-completions".into(),
            output: Vec::new(),
            input: vec!["text".into()],
            reasoning: false,
            max_tokens: Some(1024),
            temperature: None,
            context_window: 128_000,
            thinking_format: String::new(),
            supports_reasoning_effort: false,
            thinking_level_map: None,
            auth_header: false,
            supports_developer_role: false,
            requires_reasoning_content_on_assistant_messages: false,
            supports_usage_in_streaming: true,
            supports_store: true,
            supports_finish_reason: true,
            requires_assistant_after_tool_result: false,
            max_tokens_field: "max_tokens".into(),
            supports_strict_mode: false,
            send_session_affinity_headers: None,
            session_affinity_format: None,
            cost: None,
            session_id: None,
            max_retry_delay_ms: None,
            sampling_params: None,
            sampling_params_by_thinking_level: None,
            provider_routing: None,
            thinking_budgets: None,
        }
    }

    /// 构造一个「选中」的虚拟模型配置（api 为虚拟标识）。
    fn virtual_selected(provider: &str, id: &str) -> ModelConfig {
        let mut m = physical(provider, id);
        m.api = VIRTUAL_MODEL_API.into();
        m.api_key = String::new();
        m
    }

    /// 假图片实现：只为验证「chat 型条目上挂的实现不登记」，调用必错。
    struct UnusedImages;

    impl ImageApiImpl for UnusedImages {
        fn generate<'a>(
            &'a self,
            _model: &'a ModelConfig,
            _input: &'a [crate::core::provider::ImageContent],
        ) -> BoxFuture<'a, Result<crate::core::provider::AssistantImages>> {
            Box::pin(async { Err(Error::msg("unused")) })
        }
    }

    /// 静态路由器：固定返回一个目标。
    struct FixedRouter {
        /// 目标 provider。
        provider: String,
        /// 目标模型 id。
        model_id: String,
        /// 目标 thinking 级别。
        level: Option<String>,
        /// 返回给宿主的新状态。
        state: Option<Value>,
    }

    impl VirtualModelRouter for FixedRouter {
        fn route<'a>(&'a self, _r: ModelRouteRequest<'a>) -> BoxFuture<'a, Result<ModelRoute>> {
            Box::pin(async move {
                Ok(ModelRoute {
                    provider: self.provider.clone(),
                    model_id: self.model_id.clone(),
                    thinking_level: self.level.clone(),
                    state: self.state.clone(),
                })
            })
        }
    }

    fn register_fixed(
        provider: &str,
        id: &str,
        target_provider: &str,
        target_id: &str,
        level: Option<&str>,
    ) {
        register(
            VirtualModelDefinition::new(
                provider,
                id,
                "Auto",
                Arc::new(FixedRouter {
                    provider: target_provider.into(),
                    model_id: target_id.into(),
                    level: level.map(str::to_string),
                    state: None,
                }),
            )
            .with_thinking_levels(["low", "high"]),
            STANDALONE_OWNER,
        );
    }

    #[test]
    fn definitions_normalize_levels_and_input() {
        let d = VirtualModelDefinition::new(
            "router",
            "auto",
            "Auto",
            Arc::new(FixedRouter {
                provider: "a".into(),
                model_id: "b".into(),
                level: None,
                state: None,
            }),
        );
        assert_eq!(d.resolved_thinking_levels(), vec!["off".to_string()]);
        assert_eq!(d.resolved_input(), vec!["text", "image"]);

        let d = d
            .with_thinking_levels(["low", "bogus", "high"])
            .with_input(["text"]);
        assert_eq!(d.resolved_thinking_levels(), vec!["low", "high"]);
        assert_eq!(d.resolved_input(), vec!["text"]);
    }

    #[test]
    fn registry_lists_and_unregisters() {
        // 每个测试用独立的 provider 名：注册表是进程级全局状态，并行测试共享。
        register_fixed(
            "router-listing",
            "auto",
            "deepseek",
            "deepseek-v4-pro",
            None,
        );
        assert!(is_active_virtual("router-listing", "auto"));
        assert_eq!(
            list_for_provider("router-listing"),
            vec![("auto".to_string(), "Auto".to_string())]
        );
        assert!(find_active("router-listing", "missing").is_none());
        assert!(unregister("router-listing", "auto"));
        assert!(!is_active_virtual("router-listing", "auto"));
        assert!(!unregister("router-listing", "auto"));
    }

    /// pi #9962：已保存的默认模型属于「扩展注册的 provider」且该 provider 已存凭据时，
    /// 新会话必须解析出它（旧 bug：忽略保存值，或提示没有可用模型）。
    ///
    /// prux 口径：扩展用 [`Extension::virtual_models`] 注册的原生 provider 条目 +
    /// auth.json 凭据 + settings.defaultProvider/defaultModel → 启动解析与目录都要命中。
    #[test]
    fn saved_default_model_resolves_for_extension_registered_provider() {
        use crate::core::extensions::{
            Extension, ExtensionMode, ExtensionTool, register_extension, set_extension_mode,
            unregister_extension,
        };

        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _ek = crate::test_support::env_key_lock();
        unsafe {
            std::env::remove_var("PRUX_PROVIDER");
            std::env::remove_var("PRUX_MODEL");
        }
        set_extension_mode(ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        struct NativeProvider;
        impl Extension for NativeProvider {
            fn name(&self) -> &str {
                "native-provider-ext"
            }
            fn tools(&self) -> Vec<ExtensionTool> {
                Vec::new()
            }
            fn virtual_models(&self) -> Vec<VirtualModelDefinition> {
                vec![VirtualModelDefinition::new(
                    "ext-native",
                    "ext-auto",
                    "Ext Auto",
                    Arc::new(FixedRouter {
                        provider: "deepseek".into(),
                        model_id: "deepseek-v4-pro".into(),
                        level: None,
                        state: None,
                    }),
                )]
            }
        }

        register_extension(NativeProvider);
        crate::core::auth::write_auth_key("ext-native", "sk-ext").unwrap();
        crate::core::settings_manager::write_default_model_and_provider("ext-native", "ext-auto")
            .unwrap();

        // 新会话的启动解析：必须拿到保存的扩展 provider 模型
        let settings = crate::core::settings_manager::read_settings();
        let (provider, model_id, _) =
            crate::core::model_resolver::resolve_provider_model(None, None, &settings).unwrap();
        assert_eq!(
            (provider.as_str(), model_id.as_str()),
            ("ext-native", "ext-auto")
        );

        // 目录里能查到它，且解析成可用的 ModelConfig（走虚拟模型 api）
        let entry = crate::core::model_resolver::find_model("ext-native", "ext-auto").unwrap();
        assert_eq!(entry.id, "ext-auto");
        let config = crate::core::model_resolver::model_config_from_entry(
            &entry,
            None,
            Some("sess-ext".to_string()),
        );
        assert!(crate::core::virtual_models::is_virtual_api(&config.api));

        // 已配置凭据的 provider 列表也要包含它（否则 UI 会当成未登录）
        assert!(
            crate::core::auth::list_configured_providers()
                .iter()
                .any(|p| p == "ext-native"),
            "扩展 provider 带凭据时应列入已配置"
        );

        assert!(unregister_extension("native-provider-ext"));
        crate::core::auth::remove_auth("ext-native").ok();
        crate::core::settings_manager::write_default_model_and_provider(
            "deepseek",
            "deepseek-v4-pro",
        )
        .ok();
    }

    #[test]
    fn bundled_impl_is_ignored_for_chat_definitions() {
        // 无归属（SDK 直注册）的条目恒可用：若 chat 型条目的实现被登记，这里就能查到。
        register(
            VirtualModelDefinition::new(
                "router-bundled",
                "auto",
                "Auto",
                Arc::new(FixedRouter {
                    provider: "a".into(),
                    model_id: "b".into(),
                    level: None,
                    state: None,
                }),
            )
            .with_image_impl(Arc::new(UnusedImages)),
            STANDALONE_OWNER,
        );
        assert!(api_impls::image_impl(VIRTUAL_MODEL_API).is_none());
        assert!(unregister("router-bundled", "auto"));
    }

    /// 路由目标只按 chat 条目解析：只有 image 条目的 id 不能当物理目标，
    /// 回归：曾经不限类型的解析会把它当目标放行，发请求时才报协议不支持。
    #[test]
    fn resolve_physical_rejects_non_chat_entries() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let dir = crate::core::settings_manager::agent_dir();
        std::fs::write(
            dir.join("models.json"),
            serde_json::json!({"providers":{
                "route-img":{"apiKey":"sk-test","baseUrl":"http://127.0.0.1:1","models":[
                    {"id":"m1","name":"M1","type":"image","api":"openrouter-images","output":["image"]}
                ]},
                "route-chat":{"apiKey":"sk-test","baseUrl":"http://127.0.0.1:1","models":[
                    {"id":"m1","name":"M1","type":"chat","api":"openai-completions"}
                ]}
            }})
            .to_string(),
        )
        .unwrap();

        let err = resolve_physical("route-img", "m1", None, None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
        // chat 条目照旧可解析为路由目标
        assert_eq!(
            resolve_physical("route-chat", "m1", None, None)
                .unwrap()
                .model_type,
            ModelType::Chat
        );
    }

    #[tokio::test]
    async fn resolve_route_validates_target_and_clamps_level() {
        register_fixed("router-clamp", "auto", "physical", "m1", Some("max"));

        // 目标带 thinking 支持（high 可用，max 不可用）→ 钳制到 high。
        let mut target = physical("physical", "m1");
        target.reasoning = true;
        target.thinking_level_map = Some(
            [("high", Some("high".to_string())), ("max", None)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
        );
        let selected = virtual_selected("router-clamp", "auto");
        let route = resolve_route_with(
            ModelRouteRequest {
                selected: &selected,
                thinking_level: Some("high"),
                reason: ModelRouteReason::User,
                previous: None,
                failed: None,
                state: None,
                messages: &[],
                abort: None,
            },
            |p, id| {
                assert_eq!((p, id), ("physical", "m1"));
                Ok(target.clone())
            },
        )
        .await
        .unwrap();
        assert_eq!(route.thinking_level.as_deref(), Some("high"));
        assert_eq!(route.model.model_id, "m1");
    }

    #[tokio::test]
    async fn resolve_route_rejects_virtual_target_and_missing_registration() {
        register_fixed("router-reject", "auto", "router-reject", "other", None);
        let selected = virtual_selected("router-reject", "auto");
        let e = resolve_route_with(
            ModelRouteRequest {
                selected: &selected,
                thinking_level: None,
                reason: ModelRouteReason::User,
                previous: None,
                failed: None,
                state: None,
                messages: &[],
                abort: None,
            },
            |p, id| {
                let mut m = physical(p, id);
                m.api = VIRTUAL_MODEL_API.into();
                Ok(m)
            },
        )
        .await
        .unwrap_err();
        assert!(e.to_string().contains("not a physical model"), "{e}");

        let unregistered = virtual_selected("router-reject", "nope");
        let e = resolve_route_with(
            ModelRouteRequest {
                selected: &unregistered,
                thinking_level: None,
                reason: ModelRouteReason::User,
                previous: None,
                failed: None,
                state: None,
                messages: &[],
                abort: None,
            },
            |p, id| Ok(physical(p, id)),
        )
        .await
        .unwrap_err();
        assert!(e.to_string().contains("is not registered"), "{e}");
    }

    #[test]
    fn previous_route_skips_failed_and_virtual_messages() {
        let mut failed = AgentMessage::user_text("");
        failed.role = "assistant".into();
        failed.stop_reason = Some("error".into());
        failed.provider = Some("physical".into());
        failed.model = Some("bad".into());

        let mut ok = AgentMessage::user_text("");
        ok.role = "assistant".into();
        ok.provider = Some("physical".into());
        ok.model = Some("good".into());
        ok.thinking_level = Some("high".into());
        ok.content = vec![ContentBlock::Text {
            text: "hi".into(),
            text_signature: None,
        }];

        let target = previous_route(&[failed.clone(), ok.clone()]).unwrap();
        assert_eq!(target.model_id, "good");
        assert_eq!(target.thinking_level.as_deref(), Some("high"));

        // 只有失败响应 → 无 previous
        assert!(previous_route(&[failed]).is_none());

        // 虚拟模型自己的（失败的路由）响应也不算物理 previous
        let mut virt = ok.clone();
        virt.api = Some(VIRTUAL_MODEL_API.into());
        assert!(previous_route(&[virt]).is_none());
    }

    #[test]
    fn state_roundtrips_through_session_branch() {
        let dir = std::env::temp_dir().join(format!("prux-vm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = Session::create("/tmp", Some(dir.clone()), true).unwrap();
        assert!(read_state(&s, "router", "auto").is_none());

        append_state(
            &mut s,
            "router",
            "auto",
            &serde_json::json!({"phase": "plan"}),
        );
        assert_eq!(
            read_state(&s, "router", "auto"),
            Some(serde_json::json!({"phase": "plan"}))
        );
        assert!(read_state(&s, "router", "other").is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}

//! `image-route` 扩展：虚拟模型 `image-route/auto`——请求里带图就给能看图的模型，
//! 不带图就给基础模型。
//!
//! 用途：会话跑在**不支持图片输入**的模型上（便宜模型多数如此），偶尔贴一张截图时
//! 不必手动 `/model` 换过去再换回来。选中 `image-route/auto` 后：
//!
//! - 本次请求（含历史）里出现任何图片块 → 路由到 `image` 档；
//! - 没有图片 → 路由到 `base` 档。
//!
//! 判定按**整份请求**而不是「最后一条用户消息」：图片留在历史里时，后续每一次请求
//! 仍然要把它发给 provider，基础模型一样会失败（或被静默丢图），所以只要上下文里有图，
//! 这一轮就交给能看图的模型。
//!
//! **配置**：`agent_dir()/extensions/image-route.json`，经 `/image-route` 写入——`base`
//! 必填、`image` 可省（省略时等同 `base`），两项都要 `provider` + `model`。配置不完整就
//! **不注册**虚拟模型（`/model` 里不会出现半成品条目）。
//!
//! **默认不启用**，声明 `Dev` / `Creator` 两个模式。

use super::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_IMAGE_ROUTE, command_arg};
use crate::{
    core::{
        config_cache::ConfigCache,
        extensions::{
            CommandHost, Extension, ExtensionCommand, ExtensionMode, ExtensionTool, ModelRoute,
            ModelRouteRequest, SubcommandDef, UiNotifyLevel, VirtualModelDefinition,
            VirtualModelRouter,
        },
        provider::{AgentMessage, ContentBlock, ModelType},
        settings_manager::agent_dir,
        virtual_models,
    },
    error::Result,
    extensions::util::{check_physical, str_field, thinking_intersection, tighter_u32},
    modes::tui::handlers::register_host_command,
};
use futures_util::future::BoxFuture;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};

/// 扩展名（注册名，也是 `/extension` 面板里的名字）。
const EXT: &str = "image-route";

/// 斜杠命令名。
const CMD: &str = "image-route";

/// 虚拟模型挂载的 provider id。
const VIRTUAL_PROVIDER: &str = "image-route";

/// 虚拟模型 id。
const VIRTUAL_ID: &str = "auto";

/// 虚拟模型的展示名（`/model` 选择器）。
const VIRTUAL_NAME: &str = "Auto (Image)";

/// 配置文件名（位于 `agent_dir()/extensions/` 下）。
const CONFIG_FILE: &str = "image-route.json";

/// 配置目录子路径。
const CONFIG_SUBDIR: &str = "extensions";

/// 配置版本戳。
const CONFIG_VERSION: i64 = 1;

/// 配置文件读写锁（锁序恒为「RwLock → 缓存 Mutex」）。
static CONFIG_LOCK: RwLock<()> = RwLock::new(());

/// `image-route.json` 的读缓存；只在 [`CONFIG_LOCK`] 作用域内读写。
static CONFIG_CACHE: ConfigCache = ConfigCache::new();

/// `/image-route` 子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "base",
        description: "No-image target: /image-route base <provider> <model-id>",
    },
    SubcommandDef {
        name: "image",
        description: "Image target: /image-route image <provider> <model-id>",
    },
    SubcommandDef {
        name: "reset",
        description: "Clear the configuration and unregister image-route/auto",
    },
];

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static IMAGE_ROUTE_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_IMAGE_ROUTE,
    make: || -> Arc<dyn Extension> { Arc::new(ImageRoute) },
};

/// 一个路由档位：provider + 模型 id。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Target {
    /// 目标模型所属的 provider id；空串 = 未配置。
    provider: String,
    /// 目标模型 id；空串 = 未配置。
    model: String,
}

impl Target {
    /// provider 与模型 id 都非空。
    fn is_complete(&self) -> bool {
        !self.provider.trim().is_empty() && !self.model.trim().is_empty()
    }
}

/// `image-route.json` 的完整配置（两档，缺省全空）。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct ImageRouteConfig {
    /// 不带图的请求走这一档。
    base: Target,
    /// 带图的请求走这一档；未配置时等同 [`Self::base`]。
    image: Target,
}

impl ImageRouteConfig {
    /// 配置是否可用：`base` 必填。
    fn is_complete(&self) -> bool {
        self.base.is_complete()
    }

    /// 带图请求的目标：`image` 未配置时退回 `base`。
    fn image_target(&self) -> &Target {
        if self.image.is_complete() {
            &self.image
        } else {
            &self.base
        }
    }
}

/// 配置文件路径：`agent_dir()/extensions/image-route.json`
fn config_path() -> PathBuf {
    agent_dir().join(CONFIG_SUBDIR).join(CONFIG_FILE)
}

/// 取一个档位小节（`{provider, model}`）；缺节 / 非对象 / 非字符串都回落空串。
fn target_field(value: Option<&Value>) -> Target {
    Target {
        provider: str_field(value, "provider"),
        model: str_field(value, "model"),
    }
}

/// 缺省 + 文件覆盖合并（纯函数，便于测试）。
fn merge_config(from_file: Option<&Value>) -> ImageRouteConfig {
    let base = from_file.and_then(|v| v.get("base"));
    let image = from_file.and_then(|v| v.get("image"));
    ImageRouteConfig {
        base: target_field(base),
        image: target_field(image),
    }
}

/// 配置 → JSON（camelCase 键；未配置的项写成空串，不删键）。
fn to_value(cfg: &ImageRouteConfig) -> Value {
    json!({
        "configVersion": CONFIG_VERSION,
        "base": { "provider": cfg.base.provider, "model": cfg.base.model },
        "image": { "provider": cfg.image.provider, "model": cfg.image.model },
    })
}

/// 从磁盘加载配置；文件缺失 / 缺键 / 版本戳不符时回写缺省（损坏文件不覆盖）。
fn load() -> ImageRouteConfig {
    let path = config_path();
    let from_file = CONFIG_CACHE.read_object(&CONFIG_LOCK, &path);
    let cfg = merge_config(from_file.as_ref());

    // 存在但读不成对象（损坏）→ 不覆盖；其余情况补齐缺键。
    let should_write = match &from_file {
        None => !path.exists(),
        Some(file) => {
            ["base", "image"].iter().any(|k| file.get(k).is_none())
                || file.get("configVersion").and_then(Value::as_i64) != Some(CONFIG_VERSION)
        }
    };
    if should_write {
        write_config(&cfg);
    }
    cfg
}

/// 写盘并回填缓存（camelCase 键）。写失败不报错：调用方是命令路径与配置补全，落不了盘只影响下次读取；
/// 落盘失败同样不回填缓存，避免缓存住「只在内存」的值。
fn write_config(cfg: &ImageRouteConfig) {
    _ = CONFIG_CACHE.write_object(&CONFIG_LOCK, &config_path(), to_value(cfg));
}

/// 读-改-写：`f` 在磁盘现值上施加改动，返回写回后的配置。
fn modify(f: impl FnOnce(&mut ImageRouteConfig)) -> ImageRouteConfig {
    let mut cfg = load();
    f(&mut cfg);
    write_config(&cfg);
    cfg
}

/// 请求（含历史）里是否有图片块。
fn request_has_image(messages: &[AgentMessage]) -> bool {
    messages.iter().any(|m| {
        m.content
            .iter()
            .any(|block| matches!(block, ContentBlock::Image { .. }))
    })
}

/// `image-route` 的虚拟模型路由器：按请求里有没有图挑档位。
struct ImageRouter {
    /// 配置快照（注册时取，改配置会重新注册）。
    cfg: ImageRouteConfig,
}

impl ImageRouter {
    /// 本次请求走哪一档：有图 → `image` 档，无图 → `base` 档。
    fn target_for<'a>(&'a self, messages: &[AgentMessage]) -> &'a Target {
        if request_has_image(messages) {
            self.cfg.image_target()
        } else {
            &self.cfg.base
        }
    }
}

impl VirtualModelRouter for ImageRouter {
    /// 解析一次请求：见 [`ImageRouter::target_for`]。
    fn route<'a>(&'a self, request: ModelRouteRequest<'a>) -> BoxFuture<'a, Result<ModelRoute>> {
        Box::pin(async move {
            let target = self.target_for(request.messages);
            Ok(ModelRoute {
                provider: target.provider.clone(),
                model_id: target.model.clone(),
                thinking_level: request.thinking_level.map(str::to_string),
                // 无状态：档位完全由本次请求的上下文决定，不落会话分支
                state: None,
            })
        })
    }
}

/// 按当前配置组装虚拟模型定义；配置不完整或目标解析不到时返回原因（此时**不注册**）。
fn build_definition(cfg: &ImageRouteConfig) -> std::result::Result<VirtualModelDefinition, String> {
    if !cfg.is_complete() {
        return Err(
            "no base target (/image-route base <provider> <model-id> first; `image` is optional)"
                .to_string(),
        );
    }

    let base = check_physical(
        &cfg.base.provider,
        &cfg.base.model,
        ModelType::Chat,
        "base",
        (VIRTUAL_PROVIDER, VIRTUAL_ID),
    )?;
    let image = if cfg.image.is_complete() {
        check_physical(
            &cfg.image.provider,
            &cfg.image.model,
            ModelType::Chat,
            "image",
            (VIRTUAL_PROVIDER, VIRTUAL_ID),
        )?
    } else {
        base.clone()
    };

    let mut targets = vec![&base];
    if image.id != base.id || image.provider != base.provider {
        targets.push(&image);
    }

    let context_window = tighter_u32(base.context_window, image.context_window);
    let max_tokens = tighter_u32(base.max_tokens, image.max_tokens);

    let router = Arc::new(ImageRouter { cfg: cfg.clone() });
    Ok(
        VirtualModelDefinition::new(VIRTUAL_PROVIDER, VIRTUAL_ID, VIRTUAL_NAME, router)
            .with_thinking_levels(thinking_intersection(&targets))
            .with_limits(context_window, max_tokens),
    )
}

/// 按当前配置同步注册：可注册则注册（覆盖旧定义），否则注销 `image-route/auto`。
fn sync_registration() {
    match build_definition(&load()) {
        Ok(definition) => virtual_models::register(definition, EXT),
        Err(_) => _ = virtual_models::unregister(VIRTUAL_PROVIDER, VIRTUAL_ID),
    }
}

/// 两档的一行摘要（`/image-route` 状态行）。
fn summary(cfg: &ImageRouteConfig) -> String {
    if !cfg.is_complete() {
        return "add a base target: /image-route base <provider> <model-id>".to_string();
    }

    if !cfg.image.is_complete() {
        return format!(
            "base {}/{} (separate image target not set: images also go to the base model)",
            cfg.base.provider, cfg.base.model
        );
    }

    format!(
        "base {}/{} · image {}/{}",
        cfg.base.provider, cfg.base.model, cfg.image.provider, cfg.image.model
    )
}

/// `image-route` 扩展。
struct ImageRoute;

impl Extension for ImageRoute {
    /// 扩展注册名（`/extension` 面板与 `/image-route` 命令共用）。
    fn name(&self) -> &str {
        EXT
    }

    /// 面板里的功能说明。
    fn description(&self) -> &str {
        "Virtual model image-route/auto: serve turns that carry images with an image-capable \
         model, everything else with the base model. Configure with /image-route base|image; \
         stored in agent_dir()/extensions/image-route.json"
    }

    /// 默认关闭：两档都要用户自己配，未配置前注册不了任何东西。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 在 Dev 与 Creator 两个模式下可用（Minimal 下不可用）。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 不提供任何模型可见工具：能力通过虚拟模型暴露。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 按配置注册 `image-route/auto`；配置不完整时返回空（不往目录里塞不可用条目）。
    fn virtual_models(&self) -> Vec<VirtualModelDefinition> {
        build_definition(&load()).into_iter().collect()
    }

    /// `/image-route`（子命令 base / image / reset）：写配置 + 立即重注册。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Configure the image-route/auto virtual model".to_string(),
            busy_safe: true, // 只碰配置与虚拟模型注册表，不锁 agent
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 接线 `/image-route` 的 TUI 执行入口。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_image_route);
    }

    /// 被启用时按当前配置补一次注册（禁用期间配置可能已变）。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            sync_registration();
        }
    }
}

/// `/image-route` 的 TUI 执行入口。
fn command_image_route(host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((sub, rest)) => (sub, rest.trim()),
        None => (arg, ""),
    };

    match sub {
        "" => show_status(host),
        "base" => set_command(host, rest, false),
        "image" => set_command(host, rest, true),
        "reset" => reset_command(host),
        other => host.push_message(
            &format!("Unknown /image-route subcommand: {other}. Use `base`, `image` or `reset`."),
            UiNotifyLevel::Warning,
        ),
    }
    false
}

/// 打印当前配置与注册状态。
fn show_status(host: &mut dyn CommandHost) {
    let cfg = load();
    match build_definition(&cfg) {
        Ok(_) => host.push_message(
            &format!("{VIRTUAL_PROVIDER}/{VIRTUAL_ID}: ready ({})", summary(&cfg)),
            UiNotifyLevel::Success,
        ),
        Err(reason) => host.push_message(
            &format!("{VIRTUAL_PROVIDER}/{VIRTUAL_ID}: not registered — {reason}"),
            UiNotifyLevel::Warning,
        ),
    }
}

/// `/image-route base|image <provider> <model-id>`：写一档配置并重注册。
fn set_command(host: &mut dyn CommandHost, rest: &str, is_image: bool) {
    let parts: Vec<&str> = rest.split_whitespace().collect();
    let label = if is_image { "image" } else { "base" };
    if parts.len() != 2 {
        host.push_message(
            &format!("Usage: /image-route {label} <provider> <model-id>"),
            UiNotifyLevel::Warning,
        );
        return;
    }

    let target = Target {
        provider: parts[0].to_string(),
        model: parts[1].to_string(),
    };
    let cfg = modify(|cfg| {
        if is_image {
            cfg.image = target;
        } else {
            cfg.base = target;
        }
    });

    match build_definition(&cfg) {
        Ok(_) => {
            sync_registration();
            host.push_message(
                &format!("{VIRTUAL_PROVIDER}/{VIRTUAL_ID}: {}", summary(&cfg)),
                UiNotifyLevel::Success,
            );
        }
        Err(reason) => {
            // 配置已写盘但还注册不了（比如 provider 打错）：如实说清楚，别让用户以为生效了
            sync_registration();
            host.push_message(
                &format!("{VIRTUAL_PROVIDER}/{VIRTUAL_ID}: saved but not registered — {reason}"),
                UiNotifyLevel::Warning,
            );
        }
    }
}

/// `/image-route reset`：清空配置并注销虚拟模型；当前正选中它时要提醒切走。
fn reset_command(host: &mut dyn CommandHost) {
    modify(|cfg| *cfg = ImageRouteConfig::default());
    sync_registration();

    host.push_message(
        &format!("{VIRTUAL_PROVIDER}/{VIRTUAL_ID}: configuration cleared (unregistered)"),
        UiNotifyLevel::Success,
    );

    // 配置清空后基档也没了，这里不猜一个模型替用户决定，只提示换回去
    let selected = host
        .current_model()
        .is_some_and(|(provider, model)| provider == VIRTUAL_PROVIDER && model == VIRTUAL_ID);
    if selected {
        host.push_message(
            &format!("switch away from {VIRTUAL_PROVIDER}/{VIRTUAL_ID} with /model"),
            UiNotifyLevel::Warning,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::{AgentMessage, ContentBlock};

    /// 一条带图片块的用户消息。
    fn image_message() -> AgentMessage {
        let mut message = AgentMessage::user_text("what is this?");
        message.content.push(ContentBlock::Image {
            data: "AAAA".to_string(),
            mime_type: "image/png".to_string(),
        });
        message
    }

    /// 完整的两档配置（测试夹具）。
    fn cfg() -> ImageRouteConfig {
        ImageRouteConfig {
            base: Target {
                provider: "openai".to_string(),
                model: "gpt-5.6-terra".to_string(),
            },
            image: Target {
                provider: "anthropic".to_string(),
                model: "claude-opus-4-8".to_string(),
            },
        }
    }

    #[test]
    fn merge_without_file_yields_empty_config() {
        let empty = merge_config(None);
        assert_eq!(empty, ImageRouteConfig::default());
        assert!(!empty.is_complete());
        assert_eq!(empty.image_target(), &Target::default());
    }

    #[test]
    fn merge_reads_both_sections_and_rejects_wrong_types() {
        let merged = merge_config(Some(&json!({
            "base": { "provider": "openai", "model": "gpt-5.6-terra" },
            "image": { "provider": 42, "model": "claude-opus-4-8" },
        })));
        assert_eq!(merged.base.provider, "openai");
        // 类型不符的键回落空串：该档视为未配置，退回 base
        assert_eq!(merged.image.provider, "");
        assert!(merged.is_complete());
        assert_eq!(merged.image_target(), &merged.base);
    }

    #[test]
    fn request_has_image_finds_blocks_anywhere_in_the_context() {
        assert!(!request_has_image(&[]));
        assert!(!request_has_image(&[AgentMessage::user_text("hi")]));
        assert!(request_has_image(&[
            AgentMessage::user_text("what is this?"),
            image_message()
        ]));
    }

    #[test]
    fn image_target_falls_back_to_base_when_unset() {
        let mut incomplete = cfg();
        incomplete.image = Target::default();
        assert_eq!(incomplete.image_target(), &incomplete.base);
    }

    #[test]
    fn route_picks_the_image_target_only_when_the_request_carries_an_image() {
        let router = ImageRouter { cfg: cfg() };

        let plain = router.target_for(&[AgentMessage::user_text("hi")]);
        assert_eq!(plain.provider, "openai");
        assert_eq!(plain.model, "gpt-5.6-terra");

        let with_image = router.target_for(&[image_message()]);
        assert_eq!(with_image.provider, "anthropic");
        assert_eq!(with_image.model, "claude-opus-4-8");
    }

    #[test]
    fn build_definition_rejects_incomplete_or_self_targets() {
        assert!(build_definition(&ImageRouteConfig::default()).is_err());

        let mut self_target = cfg();
        self_target.base = Target {
            provider: VIRTUAL_PROVIDER.to_string(),
            model: VIRTUAL_ID.to_string(),
        };
        let Err(err) = build_definition(&self_target) else {
            panic!("自指目标必须被拒");
        };
        assert!(err.contains("must not be the virtual model"), "{err}");
    }
}

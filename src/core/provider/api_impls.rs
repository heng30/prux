//! 扩展注册的协议实现（图片生成 / 分类器）。
//!
//! 内置实现只有 `openrouter-images`（图片）与 `typesafe-system-one`（分类器），
//! 扩展可以为**自定义 api 名**注册实现，目录条目（`models.json` 自定义条目，
//! 或扩展经 `Extension::virtual_models()` 注册的 image / classifier 条目）声明同名 `api` 后即走该实现。
//! 扩展自注册条目时用 [`crate::core::virtual_models::VirtualModelDefinition::with_image_impl`] /
//! `with_classifier_impl` 把实现挂在条目上（`api` 名只写一遍）；
//! [`crate::core::extensions::Extension::image_apis`] / `classifier_apis` 只用于贡献
//! 没有自注册条目的实现（覆盖内置 / 给 `models.json` 条目用）。
//!
//! 查找顺序：**扩展注册的实现优先，其次是内置实现**（同名 api 由扩展覆盖内置）；
//! 都没有时返回「协议未实现」错误（由 [`crate::core::provider::generate_images`] /
//! [`crate::core::provider::classify`] 编码进结果，不抛错）。
//!
//! 归属扩展的实现随扩展启停过滤：扩展被 `/extension` 面板禁用（或 `--no-extensions`）后，其实现立即不再参与分发。

use crate::{
    core::{
        extensions,
        provider::{
            AssistantImages, ClassifierContext, ClassifierResult, ImageContent, ModelConfig,
        },
        virtual_models::STANDALONE_OWNER,
    },
    error::Result,
};
use futures_util::future::BoxFuture;
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, OnceLock},
};

/// 扩展提供的图片生成实现（一次性非流式）。
///
/// 返回 `Err` 由调用方转成 `stop_reason == "error"` 的结果；
/// 需要「失败不抛错」的语义由 [`crate::core::provider::generate_images`] 保证。
pub trait ImageApiImpl: Send + Sync {
    /// 用 `model` 的协议/凭据发起一次图片生成请求。
    fn generate<'a>(
        &'a self,
        model: &'a ModelConfig,
        input: &'a [ImageContent],
    ) -> BoxFuture<'a, Result<AssistantImages>>;
}

/// 扩展提供的分类器实现（一次性非流式）。
pub trait ClassifierApiImpl: Send + Sync {
    /// 用 `model` 的协议/凭据跑一次分类。
    fn classify<'a>(
        &'a self,
        model: &'a ModelConfig,
        context: &'a ClassifierContext,
    ) -> BoxFuture<'a, Result<ClassifierResult>>;
}

/// 扩展注册的一条图片生成协议实现。
pub struct RegisteredImageApi {
    /// 协议实现标识（目录条目的 `api`：如 `my-provider-images`）。
    pub api: String,
    /// 实现本体。
    pub implementation: Arc<dyn ImageApiImpl>,
}

/// 扩展注册的一条分类器协议实现。
pub struct RegisteredClassifierApi {
    /// 协议实现标识（目录条目的 `api`：如 `my-provider-system-one`）。
    pub api: String,
    /// 实现本体。
    pub implementation: Arc<dyn ClassifierApiImpl>,
}

/// 注册表条目（图片）：实现 + 归属扩展名（空 = SDK 直接注册、始终可用）。
struct ImageEntry {
    /// 注册它的扩展名；[`STANDALONE_OWNER`] 表示无归属。
    owner: String,
    /// 实现本体。
    implementation: Arc<dyn ImageApiImpl>,
}

/// 注册表条目（分类器）：实现 + 归属扩展名（空 = SDK 直接注册、始终可用）。
struct ClassifierEntry {
    /// 注册它的扩展名；[`STANDALONE_OWNER`] 表示无归属。
    owner: String,
    /// 实现本体。
    implementation: Arc<dyn ClassifierApiImpl>,
}

/// 图片实现注册表：api 名 → 条目。
fn image_registry() -> &'static Mutex<BTreeMap<String, ImageEntry>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<String, ImageEntry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// 分类器实现注册表：api 名 → 条目。
fn classifier_registry() -> &'static Mutex<BTreeMap<String, ClassifierEntry>> {
    static REGISTRY: OnceLock<Mutex<BTreeMap<String, ClassifierEntry>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// 条目是否可用：无归属者恒可用，其余随扩展启用状态 / 扩展模式动态过滤。
fn is_entry_active(owner: &str) -> bool {
    owner == STANDALONE_OWNER || extensions::is_extension_active(owner)
}

/// 注册（或替换）扩展提供的图片实现；同 api 名以最后一次注册为准。
pub fn register_image_apis(owner: impl Into<String>, apis: Vec<RegisteredImageApi>) {
    let owner = owner.into();
    let mut reg = image_registry().lock().unwrap();
    for api in apis {
        reg.insert(
            api.api,
            ImageEntry {
                owner: owner.clone(),
                implementation: api.implementation,
            },
        );
    }
}

/// 注册（或替换）扩展提供的分类器实现；同 api 名以最后一次注册为准。
pub fn register_classifier_apis(owner: impl Into<String>, apis: Vec<RegisteredClassifierApi>) {
    let owner = owner.into();
    let mut reg = classifier_registry().lock().unwrap();
    for api in apis {
        reg.insert(
            api.api,
            ClassifierEntry {
                owner: owner.clone(),
                implementation: api.implementation,
            },
        );
    }
}

/// 注销某个扩展注册的全部协议实现（扩展注销 / 测试自清理用）。
pub fn unregister_owner(owner: &str) {
    if owner == STANDALONE_OWNER {
        return;
    }
    image_registry()
        .lock()
        .unwrap()
        .retain(|_, e| e.owner != owner);
    classifier_registry()
        .lock()
        .unwrap()
        .retain(|_, e| e.owner != owner);
}

/// 清空注册表。
pub fn clear() {
    image_registry().lock().unwrap().clear();
    classifier_registry().lock().unwrap().clear();
}

/// 查找当前可用的图片实现。
pub fn image_impl(api: &str) -> Option<Arc<dyn ImageApiImpl>> {
    let reg = image_registry().lock().unwrap();
    let entry = reg.get(api)?;
    is_entry_active(&entry.owner).then(|| entry.implementation.clone())
}

/// 查找当前可用的分类器实现。
pub fn classifier_impl(api: &str) -> Option<Arc<dyn ClassifierApiImpl>> {
    let reg = classifier_registry().lock().unwrap();
    let entry = reg.get(api)?;
    is_entry_active(&entry.owner).then(|| entry.implementation.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::provider::{AssistantImages, ClassifierAnswer, ClassifierResult, ModelType};

    /// 假图片实现：回一个固定文本块，证明分发确实走到了扩展。
    struct FakeImages;

    impl ImageApiImpl for FakeImages {
        fn generate<'a>(
            &'a self,
            model: &'a ModelConfig,
            _input: &'a [ImageContent],
        ) -> BoxFuture<'a, Result<AssistantImages>> {
            Box::pin(async move {
                Ok(AssistantImages {
                    api: model.api.clone(),
                    provider: model.provider.clone(),
                    model: model.model_id.clone(),
                    output: vec![ImageContent::Text {
                        text: "from extension".to_string(),
                    }],
                    response_id: None,
                    usage: None,
                    stop_reason: "stop".to_string(),
                    error_message: None,
                    timestamp: 0,
                })
            })
        }
    }

    /// 假分类器实现：回一个固定答案。
    struct FakeClassifier;

    impl ClassifierApiImpl for FakeClassifier {
        fn classify<'a>(
            &'a self,
            _model: &'a ModelConfig,
            _context: &'a ClassifierContext,
        ) -> BoxFuture<'a, Result<ClassifierResult>> {
            Box::pin(async {
                Ok(ClassifierResult {
                    api: "fake-system-one".to_string(),
                    provider: "fake".to_string(),
                    model: "fake-classifier".to_string(),
                    answers: std::collections::BTreeMap::from([(
                        "q".to_string(),
                        ClassifierAnswer::Bool { probability: 1.0 },
                    )]),
                    usage: None,
                    stop_reason: "stop".to_string(),
                    error_message: None,
                    timestamp: 0,
                })
            })
        }
    }

    /// 归属扩展已启用才查得到：未启用 / 未注册的扩展名下，实现不参与分发。
    #[test]
    fn registered_apis_follow_owner_active_state() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        let owner = "api-impls-inactive-owner";
        register_image_apis(
            owner,
            vec![RegisteredImageApi {
                api: "test-images".to_string(),
                implementation: Arc::new(FakeImages),
            }],
        );
        register_classifier_apis(
            owner,
            vec![RegisteredClassifierApi {
                api: "test-classifier".to_string(),
                implementation: Arc::new(FakeClassifier),
            }],
        );

        // 未注册 / 未启用的扩展名 → 实现不可用
        assert!(image_impl("test-images").is_none());
        assert!(classifier_impl("test-classifier").is_none());
        // 根本没注册过的 api 名同样不可用
        assert!(image_impl("no-such-api").is_none());

        unregister_owner(owner);
        clear();
    }

    /// 无归属（SDK 直接注册）的实现不会被 `unregister_owner` 清掉。
    #[test]
    fn standalone_impls_survive_owner_unregister() {
        let _g = crate::test_support::AgentDirGuard::temp();
        register_image_apis(
            STANDALONE_OWNER,
            vec![RegisteredImageApi {
                api: "standalone-images".to_string(),
                implementation: Arc::new(FakeImages),
            }],
        );
        unregister_owner(STANDALONE_OWNER);
        assert!(image_impl("standalone-images").is_some());
        clear();
    }

    /// 假扩展：image / classifier 各一条目录条目，实现随条目给出（`api` 名只写一遍）。
    /// 不使用 `image_apis()` / `classifier_apis()`。
    struct FakeImageExtension;

    impl crate::core::extensions::Extension for FakeImageExtension {
        fn name(&self) -> &'static str {
            "fake-image-extension"
        }

        fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
            Vec::new()
        }

        fn virtual_models(&self) -> Vec<crate::core::virtual_models::VirtualModelDefinition> {
            vec![
                crate::core::virtual_models::VirtualModelDefinition::operation(
                    "fake-provider",
                    "fake-image",
                    "Fake Image",
                    ModelType::Image,
                    "ext-images",
                )
                .with_base_url("https://images.example/v1")
                .with_output(["image"])
                .with_image_impl(Arc::new(FakeImages)),
                crate::core::virtual_models::VirtualModelDefinition::operation(
                    "fake-provider",
                    "fake-classifier",
                    "Fake Classifier",
                    ModelType::Classifier,
                    "ext-classifier",
                )
                .with_classifier_impl(Arc::new(FakeClassifier)),
            ]
        }
    }

    /// 端到端：扩展注册的 image 条目 + 挂在条目上的实现（`api` 名只写一遍），
    /// `generate_images` 真的走扩展实现；扩展禁用后实现立即不参与分发（回落成「协议未实现」错误）。
    #[test]
    fn extension_image_api_is_dispatched_and_follows_extension_state() {
        use crate::core::{
            extensions::{register_extension, set_extension_enabled, unregister_extension},
            model_resolver, provider,
        };

        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        let owner = "fake-image-extension";
        unregister_extension(owner);
        register_extension(FakeImageExtension);

        // 目录：扩展注册的 image 条目进 image 列表，按类型取得到
        let entry =
            model_resolver::find_model_of_type("fake-provider", "fake-image", ModelType::Image)
                .unwrap();
        assert_eq!(entry.api, "ext-images");
        assert_eq!(entry.base_url, "https://images.example/v1");
        assert_eq!(entry.output, vec!["image".to_string()]);

        let model = model_resolver::model_config_from_entry(&entry, None, None);
        // 条目自带的实现已按条目 `api` 登记，不必再写一遍
        assert!(image_impl("ext-images").is_some());
        assert!(classifier_impl("ext-classifier").is_some());
        let result = tokio_test_block(provider::generate_images(&model, &[]));
        assert_eq!(result.stop_reason, "stop", "{:?}", result.error_message);
        assert_eq!(
            result.output,
            vec![ImageContent::Text {
                text: "from extension".to_string()
            }]
        );

        // 禁用扩展：条目与实现一起下线
        set_extension_enabled(owner, false);
        assert!(image_impl("ext-images").is_none());
        assert!(classifier_impl("ext-classifier").is_none());
        let result = tokio_test_block(provider::generate_images(&model, &[]));
        assert_eq!(result.stop_reason, "error");
        assert!(
            result
                .error_message
                .as_deref()
                .is_some_and(|m| m.contains("does not support image generation")),
            "{:?}",
            result.error_message
        );

        unregister_extension(owner);
    }

    /// 在当前测试线程上跑一个 future（测试不依赖 tokio 运行时）。
    fn tokio_test_block<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(fut)
    }
}

//! Azure OpenAI（provider `azure`）：endpoint、API 版本与部署名的解析。
//!
//! Azure 的模型目录条目 `baseUrl` 为空——每个用户一个资源，地址只能在请求前解析：
//! `AZURE_OPENAI_BASE_URL` → `AZURE_OPENAI_RESOURCE_NAME`（拼 `https://<name>.openai.azure.com/openai/v1`）
//! → 目录 `baseUrl`（可由 `models.json` 覆盖），三者都缺时请求失败并提示设置哪个变量。
//!
//! 请求体的 `model` 字段发的是**部署名**而不是目录 id：
//! `AZURE_OPENAI_DEPLOYMENT_NAME_MAP`（`modelId=deployment,modelId2=deployment2`）里命中时用映射值，
//! 否则用目录 id（[`request_model_name`]）。两个协议（`azure-openai-responses` / `openai-completions`）共用这套解析。

use super::ModelConfig;
use crate::error::{Error, Result};
use std::{borrow::Cow, collections::HashMap};
use url::Url;

/// `AZURE_OPENAI_API_VERSION` 未设置时的默认 API 版本。
///
/// `v1` 是 Azure 的新版稳定接口：路径形如 `<base>/openai/v1/<endpoint>`，不需要 `api-version` 查询参数。
const DEFAULT_API_VERSION: &str = "v1";

/// 该模型是否走 Azure 的 endpoint 解析（目录里的 azure 条目 `provider` 都是 `azure`）。
pub(crate) fn is_azure(model: &ModelConfig) -> bool {
    model.provider == "azure"
}

/// 读取环境变量并去掉首尾空白；缺失或只剩空白时为 None。
fn env_value(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Azure 的 API 版本：`AZURE_OPENAI_API_VERSION`，缺失时为 [`DEFAULT_API_VERSION`]。
pub(crate) fn api_version() -> String {
    env_value("AZURE_OPENAI_API_VERSION").unwrap_or_else(|| DEFAULT_API_VERSION.to_string())
}

/// 给 endpoint 追加 `api-version` 查询参数；`v1` 接口与其它 provider 原样返回。
///
/// Azure 的旧版 API 把版本写在查询参数里，新版 `v1` 接口（路径含 `/openai/v1`）不需要。
pub(crate) fn with_api_version(model: &ModelConfig, endpoint: String) -> String {
    if !is_azure(model) {
        return endpoint;
    }
    let version = api_version();
    if version == DEFAULT_API_VERSION {
        return endpoint;
    }
    let separator = if endpoint.contains('?') { '&' } else { '?' };
    format!("{endpoint}{separator}api-version={version}")
}

/// 解析 Azure 的 base URL：环境变量 → 资源名拼默认主机 → 目录 `baseUrl`。
///
/// 解析出来的地址会做归一：Azure 主机（`*.openai.azure.com` /
/// `*.cognitiveservices.azure.com` / `*.ai.azure.com`）上省略/写错的路径统一成
/// `/openai/v1`，这样协议层只需追加 `<base>/responses` 或 `<base>/chat/completions`。
/// 三者都没有时返回错误，文案指明可设置哪些来源。
pub(crate) fn resolve_base_url(model: &ModelConfig) -> Result<String> {
    let resolved = env_value("AZURE_OPENAI_BASE_URL")
        .or_else(|| {
            env_value("AZURE_OPENAI_RESOURCE_NAME")
                .map(|name| format!("https://{name}.openai.azure.com/openai/v1"))
        })
        .or_else(|| {
            let base = model.base_url.trim().trim_end_matches('/');
            (!base.is_empty()).then(|| base.to_string())
        });

    match resolved {
        Some(base_url) => normalize_base_url(&base_url),
        None => Err(Error::msg(
            "Azure OpenAI base URL is required. Set AZURE_OPENAI_BASE_URL or \
             AZURE_OPENAI_RESOURCE_NAME, or set baseUrl for the azure provider in models.json.",
        )),
    }
}

/// 归一 Azure 的 base URL：去尾斜杠，Azure 主机上把缺省路径补成 `/openai/v1`。
///
/// 非 Azure 主机（如自建网关、`models.json` 覆盖的代理地址）只做去尾斜杠。
fn normalize_base_url(base_url: &str) -> Result<String> {
    let trimmed = base_url.trim().trim_end_matches('/');
    let mut url = Url::parse(trimmed)
        .map_err(|_| Error::msg(format!("Invalid Azure OpenAI base URL: {base_url}")))?;

    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let is_azure_host = host.ends_with(".openai.azure.com")
        || host.ends_with(".cognitiveservices.azure.com")
        || host.ends_with(".ai.azure.com");
    let path = url.path().trim_end_matches('/').to_string();

    if is_azure_host && matches!(path.as_str(), "" | "/openai" | "/openai/v1/responses") {
        url.set_path("/openai/v1");
        url.set_query(None);
    }

    Ok(url.to_string().trim_end_matches('/').to_string())
}

/// 解析 `AZURE_OPENAI_DEPLOYMENT_NAME_MAP`：`modelId=deployment` 逗号分隔，非法项跳过。
fn deployment_map(value: Option<String>) -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(value) = value else {
        return map;
    };

    for entry in value.split(',') {
        let Some((model_id, deployment)) = entry.split_once('=') else {
            continue;
        };

        let (model_id, deployment) = (model_id.trim(), deployment.trim());
        if model_id.is_empty() || deployment.is_empty() {
            continue;
        }
        map.insert(model_id.to_string(), deployment.to_string());
    }
    map
}

/// 请求体 `model` 字段该发的名字：Azure 目录 id 命中部署名映射时为映射值，否则为目录 id。
///
/// 非 azure 模型永远是目录 id（借用，不分配）。
pub(crate) fn request_model_name(model: &ModelConfig) -> Cow<'_, str> {
    if !is_azure(model) {
        return Cow::Borrowed(&model.model_id);
    }

    match deployment_map(env_value("AZURE_OPENAI_DEPLOYMENT_NAME_MAP")).remove(&model.model_id) {
        Some(deployment) => Cow::Owned(deployment),
        None => Cow::Borrowed(&model.model_id),
    }
}

/// 请求前把 azure 模型的 `baseUrl` 换成解析结果；非 azure 模型返回 `None`（原样透传）。
///
/// 返回副本而不是就地改写，调用方因此不需要为「请求期解析」引入可变状态。
/// base URL 解析失败时返回错误，由 [`super::stream_chat`] 走失败流。
pub(crate) fn resolve_if_azure(model: &ModelConfig) -> Result<Option<ModelConfig>> {
    if !is_azure(model) {
        return Ok(None);
    }

    let mut resolved = model.clone();
    resolved.base_url = resolve_base_url(model)?;
    Ok(Some(resolved))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{AgentDirGuard, env_key_lock};

    /// Azure 相关的四个环境变量名。
    const AZURE_ENV_NAMES: [&str; 4] = [
        "AZURE_OPENAI_BASE_URL",
        "AZURE_OPENAI_RESOURCE_NAME",
        "AZURE_OPENAI_API_VERSION",
        "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
    ];

    /// 测试内的环境变量暂存：清空 Azure 变量后按需设置，drop 时还原原值并释放 env 锁。
    ///
    /// 持 [`env_key_lock`] 串行，避免与其他 env 类测试互踩（`std::env` 没有线程本地版本）。
    #[must_use = "guard 必须存活到断言结束"]
    struct AzureEnvGuard {
        /// 持有的叶级 env 互斥锁，串行化同进程 env 读写直到 drop。
        _lock: std::sync::MutexGuard<'static, ()>,
        /// 清空前的环境变量原值，drop 时逐个还原；None 表示原本未设置。
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl AzureEnvGuard {
        /// 清空全部 Azure 环境变量，再设置 `pairs` 给出的键。
        fn set(pairs: &[(&'static str, &str)]) -> Self {
            let lock = env_key_lock();
            let saved: Vec<(&'static str, Option<String>)> = AZURE_ENV_NAMES
                .iter()
                .map(|n| (*n, std::env::var(n).ok()))
                .collect();
            for name in AZURE_ENV_NAMES {
                unsafe { std::env::remove_var(name) };
            }
            for (key, value) in pairs {
                unsafe { std::env::set_var(key, value) };
            }
            Self { _lock: lock, saved }
        }
    }

    impl Drop for AzureEnvGuard {
        /// 逐个还原保存的环境变量（原本未设置的重新移除），并释放持有的 env 锁。
        fn drop(&mut self) {
            for (name, value) in self.saved.drain(..) {
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }

    /// 取目录里的 azure 模型配置（`baseUrl` 为空，provider=azure）。
    fn azure_model(model_id: &str) -> ModelConfig {
        let entry = super::super::model_resolver::find_model("azure", model_id)
            .unwrap_or_else(|err| panic!("目录里应有 azure 模型 {model_id}: {err}"));
        super::super::model_resolver::model_config_from_entry(&entry, Some("sk-test".into()), None)
    }

    #[test]
    fn normalizes_azure_host_paths() {
        for (input, expected) in [
            (
                "https://res.openai.azure.com",
                "https://res.openai.azure.com/openai/v1",
            ),
            (
                "https://res.openai.azure.com/",
                "https://res.openai.azure.com/openai/v1",
            ),
            (
                "https://res.openai.azure.com/openai",
                "https://res.openai.azure.com/openai/v1",
            ),
            (
                "https://res.openai.azure.com/openai/v1/responses",
                "https://res.openai.azure.com/openai/v1",
            ),
            (
                "https://res.cognitiveservices.azure.com",
                "https://res.cognitiveservices.azure.com/openai/v1",
            ),
            // 非 Azure 主机（自建网关）：只去尾斜杠，不动路径
            (
                "https://gateway.example.com/v1/",
                "https://gateway.example.com/v1",
            ),
        ] {
            assert_eq!(normalize_base_url(input).unwrap(), expected, "{input}");
        }
        assert!(normalize_base_url("not a url").is_err());
    }

    #[test]
    fn base_url_prefers_env_then_resource_then_catalog() {
        let _ad = AgentDirGuard::temp();

        {
            let _env =
                AzureEnvGuard::set(&[("AZURE_OPENAI_BASE_URL", "https://gateway.example.com/v1/")]);
            assert_eq!(
                resolve_base_url(&azure_model("gpt-5.4")).unwrap(),
                "https://gateway.example.com/v1"
            );
        }

        {
            let _env = AzureEnvGuard::set(&[("AZURE_OPENAI_RESOURCE_NAME", "my-res")]);
            assert_eq!(
                resolve_base_url(&azure_model("gpt-5.4")).unwrap(),
                "https://my-res.openai.azure.com/openai/v1"
            );
        }

        {
            // 目录里的 Azure 条目 baseUrl 为空，没有任何来源时报错并指明可设置哪些变量
            let _env = AzureEnvGuard::set(&[]);
            let err = resolve_base_url(&azure_model("gpt-5.4"))
                .unwrap_err()
                .to_string();
            assert!(err.contains("AZURE_OPENAI_BASE_URL"), "{err}");
        }

        {
            // models.json 覆盖出来的 baseUrl 也能用
            let _env = AzureEnvGuard::set(&[]);
            let mut model = azure_model("gpt-5.4");
            model.base_url = "https://my-gateway.example.com/openai/v1".into();
            assert_eq!(
                resolve_base_url(&model).unwrap(),
                "https://my-gateway.example.com/openai/v1"
            );
        }
    }

    #[test]
    fn deployment_map_rewrites_only_azure_request_models() {
        let _ad = AgentDirGuard::temp();
        let _env = AzureEnvGuard::set(&[(
            "AZURE_OPENAI_DEPLOYMENT_NAME_MAP",
            "gpt-5.4=my-deployment, broken, =x",
        )]);

        assert_eq!(request_model_name(&azure_model("gpt-5.4")), "my-deployment");
        // 未命中的目录 id 原样下发
        assert_eq!(request_model_name(&azure_model("gpt-5-mini")), "gpt-5-mini");
        // 非 azure 模型不受部署名映射影响
        let mut other = azure_model("gpt-5.4");
        other.provider = "openai".into();
        assert_eq!(request_model_name(&other), "gpt-5.4");
    }

    #[test]
    fn api_version_query_follows_env_and_provider() {
        let _ad = AgentDirGuard::temp();

        {
            // 默认 v1：不加查询参数
            let _env = AzureEnvGuard::set(&[]);
            let model = azure_model("gpt-5.4");
            assert_eq!(
                with_api_version(
                    &model,
                    "https://x.openai.azure.com/openai/v1/responses".into()
                ),
                "https://x.openai.azure.com/openai/v1/responses"
            );
        }

        {
            let _env = AzureEnvGuard::set(&[("AZURE_OPENAI_API_VERSION", "2024-10-21")]);
            let model = azure_model("gpt-5.4");
            assert_eq!(
                with_api_version(
                    &model,
                    "https://x.openai.azure.com/openai/v1/responses".into()
                ),
                "https://x.openai.azure.com/openai/v1/responses?api-version=2024-10-21"
            );
            // 非 azure provider 不受影响
            let mut other = model.clone();
            other.provider = "openai".into();
            assert_eq!(
                with_api_version(&other, "https://api.openai.com/v1/responses".into()),
                "https://api.openai.com/v1/responses"
            );
        }
    }

    #[test]
    fn resolve_if_azure_passes_non_azure_through() {
        let _ad = AgentDirGuard::temp();
        let _env =
            AzureEnvGuard::set(&[("AZURE_OPENAI_BASE_URL", "https://gateway.example.com/v1")]);

        let mut other = azure_model("gpt-5.4");
        other.provider = "openai".into();
        assert!(resolve_if_azure(&other).unwrap().is_none());

        let resolved = resolve_if_azure(&azure_model("gpt-5.4")).unwrap().unwrap();
        assert_eq!(resolved.base_url, "https://gateway.example.com/v1");
        // 目录 id 不受影响（部署名只改请求体里的 model 字段）
        assert_eq!(resolved.model_id, "gpt-5.4");
    }
}

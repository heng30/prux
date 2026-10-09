//! 可配置认证的 provider 注册表（登录/登出 UI 用的静态清单）。
//!
//! 凭据写入 auth.json（type=api_key / type=oauth）；模型请求路径仍由 models-store.json 决定。
//! supports_api_key 的 provider 进入 “Sign in with an API key” 列表；
//! is_subscription 为 true 的 provider 进入 “Sign in with an account”（OAuth 订阅）列表。
//!
//! 协议层可用的 35 个：
//! - 排除特殊认证 provider（amazon-bedrock / google-vertex / cloudflare-*，需多字段或 AWS/ADC 凭据）
//! - 排除协议未实现 provider（mistral）
//! - radius 为网关 provider（动态模型目录），暂不实现
//! - azure 的 base_url 按环境变量/资源名在请求前解析，此处留空（见 `provider::azure`）

/// 支持登录的 provider 信息
#[derive(Debug, Clone, Copy)]
pub struct LoginProviderInfo {
    /// provider 标识（与协议层/models-store 一致，如 deepseek）
    pub id: &'static str,
    /// 展示名（登录 UI 显示，如 DeepSeek）
    pub name: &'static str,
    /// API base URL（写入 auth.json / 请求拼接用）
    pub base_url: &'static str,
    /// 是否支持 account（OAuth 订阅）登录
    pub is_subscription: bool,
    /// 是否支持 API key 登录
    pub supports_api_key: bool,
}

/// 当前支持的登录 provider 清单（登录/登出 UI 的静态来源）
pub const LOGIN_PROVIDERS: &[LoginProviderInfo] = &[
    LoginProviderInfo {
        id: "deepseek",
        name: "DeepSeek",
        base_url: "https://api.deepseek.com",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "opencode",
        name: "OpenCode Zen",
        base_url: "https://opencode.ai/zen",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "opencode-go",
        name: "OpenCode Go",
        base_url: "https://opencode.ai/zen/go",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "anthropic",
        name: "Anthropic",
        base_url: "https://api.anthropic.com",
        is_subscription: true,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "ant-ling",
        name: "Ant Ling",
        base_url: "https://api.ant-ling.com/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "azure",
        name: "Azure",
        // Azure 的地址每资源一个（AZURE_OPENAI_BASE_URL / AZURE_OPENAI_RESOURCE_NAME），无静态值
        base_url: "",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "baseten",
        name: "Baseten",
        base_url: "https://inference.baseten.co/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "cerebras",
        name: "Cerebras",
        base_url: "https://api.cerebras.ai/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "fireworks",
        name: "Fireworks",
        base_url: "https://api.fireworks.ai/inference",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "github-copilot",
        name: "GitHub Copilot",
        base_url: "https://api.individual.githubcopilot.com",
        is_subscription: true,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "google",
        name: "Google",
        base_url: "https://generativelanguage.googleapis.com/v1beta",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "groq",
        name: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "huggingface",
        name: "Hugging Face",
        base_url: "https://router.huggingface.co/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "kimi-coding",
        name: "Kimi For Coding",
        base_url: "https://api.kimi.com/coding",
        is_subscription: true,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "meta",
        name: "Meta",
        base_url: "https://api.meta.ai/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "minimax",
        name: "MiniMax",
        base_url: "https://api.minimax.io/anthropic",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "minimax-cn",
        name: "MiniMax CN",
        base_url: "https://api.minimaxi.com/anthropic",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "moonshotai",
        name: "Moonshot AI",
        base_url: "https://api.moonshot.ai/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "moonshotai-cn",
        name: "Moonshot AI CN",
        base_url: "https://api.moonshot.cn/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "nvidia",
        name: "NVIDIA",
        base_url: "https://integrate.api.nvidia.com/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "openai",
        name: "OpenAI",
        base_url: "https://api.openai.com/v1",
        is_subscription: true,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "openrouter",
        name: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        is_subscription: true,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "qwen-token-plan",
        name: "Qwen Token Plan",
        base_url: "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "qwen-token-plan-cn",
        name: "Qwen Token Plan CN",
        base_url: "https://token-plan.cn-beijing.maas.aliyuncs.com/compatible-mode/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "qwen-token-plan-individual",
        name: "Qwen Token Plan Individual",
        base_url: "https://token-plan.ap-southeast-1.maas.aliyuncs.com/compatible-mode/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "together",
        name: "Together",
        base_url: "https://api.together.ai/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "typesafe",
        name: "TypeSafe",
        base_url: "https://api.typesafe.ai/v1/",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "vercel-ai-gateway",
        name: "Vercel AI Gateway",
        base_url: "https://ai-gateway.vercel.sh",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "xai",
        name: "xAI",
        base_url: "https://api.x.ai/v1",
        is_subscription: true,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "xiaomi",
        name: "Xiaomi",
        base_url: "https://api.xiaomimimo.com/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "xiaomi-token-plan-ams",
        name: "Xiaomi Token Plan AMS",
        base_url: "https://token-plan-ams.xiaomimimo.com/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "xiaomi-token-plan-cn",
        name: "Xiaomi Token Plan CN",
        base_url: "https://token-plan-cn.xiaomimimo.com/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "xiaomi-token-plan-sgp",
        name: "Xiaomi Token Plan SGP",
        base_url: "https://token-plan-sgp.xiaomimimo.com/v1",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "zai",
        name: "Z.AI",
        base_url: "https://api.z.ai/api/coding/paas/v4",
        is_subscription: false,
        supports_api_key: true,
    },
    LoginProviderInfo {
        id: "zai-coding-cn",
        name: "Z.AI Coding CN",
        base_url: "https://open.bigmodel.cn/api/coding/paas/v4",
        is_subscription: false,
        supports_api_key: true,
    },
];

/// 查 provider id 对应的展示名；未知 id 原样返回。
pub fn provider_name(id: &str) -> String {
    LOGIN_PROVIDERS
        .iter()
        .find(|p| p.id == id)
        .map(|p| p.name.to_string())
        .unwrap_or_else(|| id.to_string())
}

/// 查 provider 的登录是否是订阅型（`/login` `[subscription]` / `[account]` 标签用）。
///
/// 未知 id 返回 false：prux 的登录清单目前把「支持 OAuth」与「订阅型」绑在同一个标志上，
/// 扩展注册的非订阅 OAuth provider 因此归为 account。
pub fn is_subscription(id: &str) -> bool {
    LOGIN_PROVIDERS
        .iter()
        .any(|p| p.id == id && p.is_subscription)
}

/// 进入 “Sign in with an API key” 列表的 provider id
pub fn api_key_provider_ids() -> Vec<&'static str> {
    LOGIN_PROVIDERS
        .iter()
        .filter(|p| p.supports_api_key)
        .map(|p| p.id)
        .collect()
}

/// 支持 OAuth 登录（订阅型）的 provider id 列表
pub fn oauth_provider_ids() -> Vec<&'static str> {
    LOGIN_PROVIDERS
        .iter()
        .filter(|p| p.is_subscription)
        .map(|p| p.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_exposes_providers() {
        assert_eq!(LOGIN_PROVIDERS.len(), 35);
        assert_eq!(provider_name("deepseek"), "DeepSeek");
        assert_eq!(provider_name("opencode"), "OpenCode Zen");
        assert_eq!(provider_name("opencode-go"), "OpenCode Go");
        assert_eq!(provider_name("anthropic"), "Anthropic");
        assert_eq!(provider_name("azure"), "Azure");
        // 新增 provider 均有展示名
        assert_eq!(provider_name("openai"), "OpenAI");
        assert_eq!(provider_name("google"), "Google");
        assert_eq!(provider_name("groq"), "Groq");
        assert_eq!(provider_name("openrouter"), "OpenRouter");
        assert_eq!(provider_name("xai"), "xAI");
        assert_eq!(provider_name("zai"), "Z.AI");
        assert_eq!(provider_name("meta"), "Meta");
        assert_eq!(provider_name("unknown"), "unknown");
    }

    #[test]
    fn subscription_supports_account_login() {
        // 对齐 pi：openai 也支持 ChatGPT 订阅登录（Sign in with ChatGPT）
        let expected = [
            "anthropic",
            "github-copilot",
            "kimi-coding",
            "openai",
            "openrouter",
            "xai",
        ];
        let mut ids = oauth_provider_ids();
        ids.sort_unstable();
        let mut exp = expected.to_vec();
        exp.sort_unstable();
        assert_eq!(ids, exp);
        // 其余 provider 均非订阅
        for p in LOGIN_PROVIDERS {
            if !expected.contains(&p.id) {
                assert!(!p.is_subscription, "{} 不应是订阅", p.id);
            }
        }
    }

    #[test]
    fn all_registry_providers_support_api_key() {
        // openai 同时支持 API key 与 ChatGPT 订阅（OAuth）
        assert_eq!(api_key_provider_ids().len(), LOGIN_PROVIDERS.len());
        for p in LOGIN_PROVIDERS {
            assert!(p.supports_api_key, "{} 应支持 API key", p.id);
        }
    }

    #[test]
    fn registry_ids_are_unique() {
        let mut ids: Vec<&str> = LOGIN_PROVIDERS.iter().map(|p| p.id).collect();
        ids.sort();
        let mut deduped = ids.clone();
        deduped.dedup();
        assert_eq!(ids, deduped, "provider id 重复");
    }
}

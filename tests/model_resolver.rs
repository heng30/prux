//! core::model_resolver 集成测试。
//!
//! find_model/list_models 从 agent_dir()/models-store.json 读取，
//! 通过线程本地 AgentDirGuard 隔离到独立临时目录（无需 set_var/锁，测试可并行）。

use prux::core::model_resolver::{
    find_model, list_models, parse_model_arg, resolve_provider_model,
};
use prux::core::settings_manager::Settings;
use prux::test_support::AgentDirGuard;

const STORE: &str = r#"{
  "test-provider": {
    "models": [
      {
        "id": "model-a",
        "name": "Model A",
        "api": "openai-completions",
        "baseUrl": "https://example.com/v1",
        "input": ["text", "image"],
        "reasoning": true,
        "contextWindow": 200000,
        "maxTokens": 8000,
        "compat": { "maxTokensField": "max_tokens", "supportsDeveloperRole": true }
      },
      {
        "id": "model-b",
        "name": "Model B",
        "api": "openai-completions",
        "baseUrl": "https://example.com/v1",
        "input": ["text"],
        "contextWindow": 128000
      }
    ]
  }
}"#;

struct EnvGuard {
    key: &'static str,
    old: Option<String>,
}

impl EnvGuard {
    fn clear(key: &'static str) -> Self {
        let old = std::env::var(key).ok();
        unsafe { std::env::remove_var(key) };
        EnvGuard { key, old }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.old {
            Some(v) => unsafe { std::env::set_var(self.key, v) },
            None => unsafe { std::env::remove_var(self.key) },
        }
    }
}

#[test]
fn model_resolver_full_flow() {
    let _ad = AgentDirGuard::temp();
    // store 文件放到当前线程的 agent_dir（Guard 的临时目录）下
    std::fs::write(
        prux::core::settings_manager::agent_dir().join("models-store.json"),
        STORE,
    )
    .unwrap();

    // find_model：完整 ID 与前缀匹配
    let entry = find_model("test-provider", "model-a").unwrap();
    assert_eq!(entry.id, "model-a");
    assert_eq!(entry.provider, "test-provider");
    assert_eq!(entry.base_url, "https://example.com/v1");
    assert!(entry.reasoning);
    assert_eq!(entry.context_window, 200_000);
    assert_eq!(entry.max_tokens, 8000);
    assert_eq!(entry.max_tokens_field, "max_tokens");
    assert!(entry.supports_developer_role);

    // 前缀匹配
    let entry2 = find_model("test-provider", "model-b").unwrap();
    assert_eq!(entry2.id, "model-b");

    // 不存在的 provider / model → Err
    assert!(find_model("nope", "model-a").is_err());
    assert!(find_model("test-provider", "model-zzz").is_err());

    // list_models 返回全部
    let listed = list_models("test-provider");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0], ("model-a".to_string(), "Model A".to_string()));
    assert!(list_models("nope").is_empty());

    // parse_model_arg 三种形式
    assert_eq!(
        parse_model_arg("provider/model:high"),
        (Some("provider".into()), "model".into(), Some("high".into()))
    );
    assert_eq!(
        parse_model_arg("model:low"),
        (None, "model".into(), Some("low".into()))
    );
    assert_eq!(parse_model_arg("model"), (None, "model".into(), None));
    // 模型 id 自带冒号：后缀不是合法 thinking 级别时整体视为模型 id
    assert_eq!(
        parse_model_arg("my-ollama/llama3.1:8b"),
        (Some("my-ollama".into()), "llama3.1:8b".into(), None)
    );
    assert_eq!(
        parse_model_arg("llama3.1:8b"),
        (None, "llama3.1:8b".into(), None)
    );
}

#[test]
fn resolve_provider_model_priority() {
    let _ad = AgentDirGuard::temp();
    std::fs::write(
        prux::core::settings_manager::agent_dir().join("models-store.json"),
        STORE,
    )
    .unwrap();
    // 清掉可能存在的 PRUX_PROVIDER / PRUX_MODEL，保证测试确定性（进程级 env，
    // 但这两个键仅本测试读写，无锁也安全）
    let _g1 = EnvGuard::clear("PRUX_PROVIDER");
    let _g2 = EnvGuard::clear("PRUX_MODEL");
    let settings = Settings {
        default_model: Some("cfg-model".into()),
        default_provider: Some("cfg-provider".into()),
        default_thinking_level: None,
        default_project_trust: None,
        theme: None,
        external_editor: None,
        skills: vec![],
        themes: vec![],
        enabled_models: vec![],
    };

    // 无 flag → settings 默认
    let (p, m, t) = resolve_provider_model(None, None, &settings).unwrap();
    assert_eq!(
        (p.as_str(), m.as_str(), t.as_deref()),
        ("cfg-provider", "cfg-model", None)
    );

    // flag 覆盖 settings
    let (p2, m2, t2) =
        resolve_provider_model(Some("flag-p"), Some("flag-m:high"), &settings).unwrap();
    assert_eq!(
        (p2.as_str(), m2.as_str(), t2.as_deref()),
        ("flag-p", "flag-m", Some("high"))
    );

    // 都无 → 回退默认 provider（deepseek）的默认模型（对齐 pi：TUI 无配置也能启动，
    // 通过 /login 提示登录，而不是拒绝启动）
    let empty = Settings {
        default_model: None,
        default_provider: None,
        ..settings
    };
    let resolved = resolve_provider_model(None, None, &empty);
    let (p3, m3, t3) = resolved.expect("无配置应回退默认 provider 模型");
    assert_eq!(p3, "deepseek");
    assert!(!m3.is_empty());
    assert_eq!(t3, None);
}

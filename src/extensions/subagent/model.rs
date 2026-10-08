//! 型号解析与 scope 检查。
//!
//! ## 为什么 fuzzy 解析在扩展这一侧
//!
//! 核心的 `SubAgentSpec.model` 要求**精确** `provider/id`（`agent_session.rs` 里就是
//! `split_once('/')` + `switch_model`，解析失败直接报错）。模型写的是给人看的
//! （`claude-haiku-4.5`、`sonnet`、`anthropic/claude-haiku-4-5-20251001`），
//! 所以"把人的写法变成规范 id"这件事落在扩展：先在这里解析成 canonical，再交给核心。
//! 核心的精确性因此不用动摇，而用户与模型的写法都能work。
//!
//! 匹配顺序：精确（大小写不敏感，且必须**有鉴权**）→ 模糊打分
//! （id/full 子串按紧密度加分，其次 name；`claude-haiku-4.5` 与 `claude-haiku-4-5` 归一化后等价）
//! → **提供方回退**（`provider/id` 没匹配上时拿裸 id 再试一遍所有提供方） → 报错并列出可用型号。
//!
//! ## scope 检查
//!
//! `scope_models` 开启时，子代理的型号要在用户的 `enabledModels`
//! （全局 settings + 项目覆盖，由核心 [`crate::core::model_scope`] 展开）之内：
//!
//! - **调用方**给的越界型号 → **拒绝**（是编排方做的一个明确选择，得让它知道，好换个选择）；
//! - **agent 文件（frontmatter）钉的**越界型号 → 告警后继续（那是用户自己写的/装的东西，信任它）。

use crate::core::{
    auth::list_configured_providers,
    model_resolver::list_models,
    model_scope::{AvailableModel, resolve_model_scope},
    settings_manager,
};
use std::sync::OnceLock;

/// 型号来自哪一层（决定越界时是拒绝还是告警）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ModelSource {
    /// 没写：继承父级
    #[default]
    Inherited,
    /// agent 文件的 frontmatter 钉的
    Frontmatter,
    /// 调用方（工具参数 / 工作流脚本）给的
    Caller,
}

/// 可用型号目录（只含配了鉴权的提供方）。
#[derive(Debug, Clone)]
struct CatalogEntry {
    /// 提供方 id（如 `anthropic`）。
    provider: String,
    /// 目录里的型号 id。
    id: String,
    /// 友好名（fuzzy 匹配也看它）。
    name: String,
}

/// 可用型号：进程内缓存一次。
///
/// 解析发生在**每次 spawn** 上（上游同样每次解析），而列目录要遍历提供方与 OAuth 凭据，所以缓存。
/// 列在一次会话里不会变（换凭据要重启），代价可接受。
fn catalog() -> Vec<CatalogEntry> {
    #[cfg(test)]
    if let Some(entries) = test_catalog() {
        return entries;
    }

    /// 可用型号目录缓存（进程内一次；换凭据需重启）。
    static C: OnceLock<Vec<CatalogEntry>> = OnceLock::new();
    C.get_or_init(|| {
        list_configured_providers()
            .iter()
            .flat_map(|p| {
                list_models(p).into_iter().map(|(id, name)| CatalogEntry {
                    provider: p.clone(),
                    id,
                    name,
                })
            })
            .collect()
    })
    .clone()
}

/// 归一化：小写 + `.`→`-`（版本号里的点与横线是同一种写法）。
fn normalize(s: &str) -> String {
    s.to_lowercase().replace('.', "-")
}

/// 解析模型输入 → canonical `provider/id`。失败时返回可读错误（含可用型号列表）。
pub(crate) fn resolve(input: &str) -> Result<String, String> {
    let input = input.trim();
    let all = catalog();
    let (exact, provider, model_id) = match input.split_once('/') {
        Some((p, m)) => (true, p, m),
        None => (false, "", input),
    };

    // 1) 精确匹配（大小写不敏感，且必须在可用目录里）
    if exact
        && all.iter().any(|m| {
            m.provider.eq_ignore_ascii_case(provider) && m.id.eq_ignore_ascii_case(model_id)
        })
    {
        let hit = all
            .iter()
            .find(|m| {
                m.provider.eq_ignore_ascii_case(provider) && m.id.eq_ignore_ascii_case(model_id)
            })
            .expect("just checked");

        return Ok(format!("{}/{}", hit.provider, hit.id));
    }

    // 2) 模糊打分
    let query = normalize(input);
    let mut best: Option<(&CatalogEntry, f64)> = None;
    for m in &all {
        let id = normalize(&m.id);
        let name = normalize(&m.name);
        let full = normalize(&format!("{}/{}", m.provider, m.id));

        let score = if id == query || full == query {
            100.0
        } else if query.is_empty() {
            0.0
        } else if id.contains(&query) || full.contains(&query) {
            // 子串命中：越"紧"越优先（`haiku` 命中 `claude-haiku-4-5` 优于命中一段长名字）
            60.0 + (query.chars().count() as f64 / id.chars().count().max(1) as f64) * 30.0
        } else if name.contains(&query) {
            40.0 + (query.chars().count() as f64 / name.chars().count().max(1) as f64) * 20.0
        } else if query
            .split([' ', '-', '/'])
            .filter(|p| !p.is_empty())
            .all(|part| {
                // 尾部的日期戳（如 20251001）是可选的：钉了日期的配置仍能匹配不带日期的目录条目
                (part.len() == 8 && part.chars().all(|c| c.is_ascii_digit()))
                    || id.contains(part)
                    || name.contains(part)
                    || m.provider.to_lowercase().contains(part)
            })
        {
            20.0
        } else {
            0.0
        };

        if score > best.map(|(_, s)| s).unwrap_or(0.0) {
            best = Some((m, score));
        }
    }

    if let Some((m, score)) = best
        && score >= 20.0
    {
        return Ok(format!("{}/{}", m.provider, m.id));
    }

    // 3) 提供方回退：`provider/id` 没匹配上时，拿裸 id 再试所有提供方
    if exact {
        return resolve(model_id);
    }

    // 4) 没有可用型号（没配提供方）时给一句更直白的话
    if all.is_empty() {
        return Err(format!(
            "cannot resolve model {input:?}: no providers with credentials are configured"
        ));
    }

    let mut list: Vec<String> = all
        .iter()
        .map(|m| format!("  {}/{}", m.provider, m.id))
        .collect();
    list.sort();

    Err(format!(
        "Model not found: {input:?}.\\n\\nAvailable models:\\n{}",
        list.join("\\n")
    ))
}

/// scope 判定结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ScopeVerdict {
    /// 在 scope 内（或没开这个开关、或没有 allowlist）
    Ok,
    /// 调用方给的越界选择 → 拒绝这次 spawn
    Refuse(String),
    /// frontmatter 钉的越界选择 → 告警后继续
    Warn(String),
}

/// 检查**已解析**的型号是否落在 `enabledModels` 展开出的 scope 内。
pub(crate) fn check_scope(
    cfg_scope: bool,
    effective: Option<&str>,
    source: ModelSource,
    label: &str,
    input: Option<&str>,
) -> ScopeVerdict {
    if !cfg_scope {
        return ScopeVerdict::Ok;
    }

    let Some(effective) = effective else {
        return ScopeVerdict::Ok;
    };

    let patterns = settings_manager::read_enabled_models();
    if patterns.is_empty() {
        return ScopeVerdict::Ok;
    }

    let available: Vec<AvailableModel> = catalog()
        .iter()
        .map(|m| AvailableModel {
            provider: m.provider.clone(),
            id: m.id.clone(),
            name: m.name.clone(),
        })
        .collect();

    let resolved = resolve_model_scope(&patterns, &available);
    if resolved.scoped.iter().any(|m| m.canonical() == effective) {
        return ScopeVerdict::Ok;
    }

    let shown = input.unwrap_or(effective);
    match source {
        ModelSource::Caller => {
            let mut list: Vec<String> = resolved.scoped.iter().map(|m| m.canonical()).collect();
            list.sort();
            list.dedup();

            ScopeVerdict::Refuse(format!(
                "Model not in scope: {shown:?}.\\n\\nAllowed models (from enabledModels):\\n{}",
                list.iter()
                    .map(|m| format!("  {m}"))
                    .collect::<Vec<_>>()
                    .join("\\n")
            ))
        }
        ModelSource::Frontmatter | ModelSource::Inherited => ScopeVerdict::Warn(format!(
            "Agent {label:?} using out-of-scope model {shown:?}"
        )),
    }
}

/// 测试用：钉住可用型号目录（否则真目录在测试环境里是空的，fuzzy/scope 都无从验证）。
#[cfg(test)]
pub(crate) fn set_catalog_for_test(entries: &[(&str, &str, &str)]) {
    let list: Vec<CatalogEntry> = entries
        .iter()
        .map(|(p, id, name)| CatalogEntry {
            provider: (*p).to_string(),
            id: (*id).to_string(),
            name: (*name).to_string(),
        })
        .collect();
    *test_catalog_slot().lock().unwrap() = Some(list);
}

/// 测试用目录覆盖槽的进程内单例句柄（惰性初始化）。
#[cfg(test)]
fn test_catalog_slot() -> &'static std::sync::Mutex<Option<Vec<CatalogEntry>>> {
    /// 测试用的目录覆盖槽（`set_catalog_for_test` 写入）。
    static S: OnceLock<std::sync::Mutex<Option<Vec<CatalogEntry>>>> = OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(None))
}

/// 取测试目录覆盖的一份拷贝；未设置时为 `None`，此时 [`catalog`] 走真实的列目录逻辑。
#[cfg(test)]
fn test_catalog() -> Option<Vec<CatalogEntry>> {
    test_catalog_slot().lock().unwrap().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_folds_dots_and_case() {
        assert_eq!(normalize("Claude-Haiku-4.5"), "claude-haiku-4-5");
    }

    #[test]
    fn scope_is_off_by_default_and_silent_without_allowlist() {
        // 开关关掉 → 一律 Ok（无论型号是什么）
        assert_eq!(
            check_scope(false, Some("x/y"), ModelSource::Caller, "a", Some("x/y")),
            ScopeVerdict::Ok
        );
        // 没给型号 → Ok
        assert_eq!(
            check_scope(true, None, ModelSource::Caller, "a", None),
            ScopeVerdict::Ok
        );
    }

    /// 目录与 `enabledModels` 都是进程级全局态 → 触碰它们的用例必须串行。
    fn pin_catalog() -> super::super::manager::TestLock {
        let guard = super::super::manager::test_lock();
        set_catalog_for_test(&[
            ("anthropic", "claude-haiku-4-5", "Claude Haiku 4.5"),
            ("anthropic", "claude-sonnet-4-5", "Claude Sonnet 4.5"),
            ("openai", "gpt-5-mini", "GPT-5 mini"),
        ]);
        guard
    }

    #[test]
    fn resolve_does_exact_then_fuzzy_then_provider_fallback() {
        let _g = pin_catalog();
        // 精确（大小写不敏感）→ 规范形态
        assert_eq!(
            resolve("anthropic/claude-haiku-4-5").unwrap(),
            "anthropic/claude-haiku-4-5"
        );
        assert_eq!(
            resolve("Anthropic/Claude-Haiku-4-5").unwrap(),
            "anthropic/claude-haiku-4-5"
        );
        // 点/横线归一化：写 `4.5` 也能命中 `4-5`
        assert_eq!(
            resolve("claude-haiku-4.5").unwrap(),
            "anthropic/claude-haiku-4-5"
        );
        // 裸 id 子串
        assert_eq!(resolve("sonnet").unwrap(), "anthropic/claude-sonnet-4-5");
        // 显示名子串
        assert_eq!(resolve("gpt-5").unwrap(), "openai/gpt-5-mini");
        // 尾部日期戳可选：钉了日期的配置仍匹配不带日期的条目
        assert_eq!(
            resolve("anthropic/claude-haiku-4-5-20251001").unwrap(),
            "anthropic/claude-haiku-4-5"
        );
        // 提供方回退：提供方写错也还能按裸 id 命中
        assert_eq!(
            resolve("openai/claude-haiku-4-5").unwrap(),
            "anthropic/claude-haiku-4-5"
        );
        // 找不到 → 报错并列出可用型号
        let err = resolve("no-such-model-xyz").unwrap_err();
        assert!(err.contains("Model not found"), "{err}");
        assert!(err.contains("anthropic/claude-haiku-4-5"), "{err}");
    }

    #[test]
    fn resolve_reports_empty_catalog_readably() {
        let _g = super::super::manager::test_lock();
        set_catalog_for_test(&[]);
        let err = resolve("whatever").unwrap_err();
        assert!(err.contains("no providers with credentials"), "{err}");
    }

    #[test]
    fn scope_refuses_callers_choice_but_only_warns_for_frontmatter() {
        let _ad = crate::test_support::AgentDirGuard::temp();
        let _g = pin_catalog();
        // 用户把 scope 限定到 haiku
        crate::core::settings_manager::write_enabled_models(Some(&[
            "anthropic/claude-haiku-4-5".to_string()
        ]))
        .unwrap();

        // 在 scope 内 → Ok
        assert_eq!(
            check_scope(
                true,
                Some("anthropic/claude-haiku-4-5"),
                ModelSource::Caller,
                "a",
                None
            ),
            ScopeVerdict::Ok
        );
        // 调用方给的越界型号 → 拒绝，并列出允许的
        match check_scope(
            true,
            Some("anthropic/claude-sonnet-4-5"),
            ModelSource::Caller,
            "a",
            Some("sonnet"),
        ) {
            ScopeVerdict::Refuse(message) => {
                assert!(message.contains("Model not in scope"), "{message}");
                assert!(
                    message.contains("claude-haiku-4-5"),
                    "要列出允许的型号: {message}"
                );
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
        // frontmatter 钉的越界型号 → 告警后继续
        match check_scope(
            true,
            Some("anthropic/claude-sonnet-4-5"),
            ModelSource::Frontmatter,
            "audit",
            None,
        ) {
            ScopeVerdict::Warn(message) => {
                assert!(message.contains("out-of-scope"), "{message}");
                assert!(message.contains("audit"), "要指出是哪个 agent: {message}");
            }
            other => panic!("expected Warn, got {other:?}"),
        }
        // 没有 allowlist → 一律 Ok
        crate::core::settings_manager::write_enabled_models(None).unwrap();
        assert_eq!(
            check_scope(
                true,
                Some("anthropic/claude-sonnet-4-5"),
                ModelSource::Caller,
                "a",
                None
            ),
            ScopeVerdict::Ok
        );
    }
}

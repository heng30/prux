//! 核心层
//!
//! 分层约定（三层架构，依赖方向单向向上）：
//! - [`provider`] 为底层（pi-ai 角色）：统一消息/模型/工具类型 + 流式调用， 禁止依赖本层其他模块。
//!
//! - [`agent_session`]、[`session_manager`]、[`compaction`]
//!   为中层（pi-agent-core 角色）：Agent 循环、状态、事件、会话历史与上下文压缩，不依赖具体业务。
//!
//! - [`tools`]、[`skills`]、[`prompt_templates`]、[`system_prompt`]、[`context`]、
//!   [`project_trust`]、[`settings_manager`]、[`model_resolver`]、[`export_html`]、
//!   [`extensions`] 为顶层业务。

pub mod agent_loop;
pub mod agent_session;
pub mod auth;
pub mod bug_report;
pub mod cache_warmer;
pub mod changelog;
pub mod compaction;
pub mod config_cache;
pub mod context;
pub mod crash_log;
pub mod doc_sync;
pub mod export_html;
pub mod extensions;
pub mod keybindings;
pub mod login_registry;
pub mod markdown_table;
pub mod model_config;
pub mod model_refresh;
pub mod model_resolver;
pub mod model_scope;
pub mod oauth;
pub mod project_trust;
pub mod prompt_history;
pub mod prompt_templates;
pub mod provider;
pub mod runtime;
pub mod session_manager;
pub mod session_v4;
pub mod settings_manager;
pub mod skills;
pub mod system_prompt;
pub mod theme_view;
pub mod tools;
pub mod tools_manager;
pub mod virtual_models;

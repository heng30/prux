//! footer 扩展接口
//!
//! - 实现 [`FooterExtension`]（`on_agent_event` 收事件、`render` 出多行带样式文本）；
//! - 用 [`super::registry::register_footer_extension`] 注册；
//! - TUI 每帧根据已注册扩展渲染底栏，不再显示内置快捷键提示。

use super::ExtensionMode;
use crate::core::provider::AgentMessage;
use serde_json::Value;
use std::path::Path;

/// 底栏中的一段带样式文本（主题键名 + 缺省色，渲染时经 `Theme::style` 着色）
#[derive(Debug, Clone)]
pub struct FooterSpan {
    /// 主题表里取色用的样式标识；与 fallback 同为空串时用默认样式
    pub key: &'static str,
    /// 主题中找不到该标识时使用的缺省色（十六进制色值）
    pub fallback: &'static str,
    /// 该片段实际显示的文本内容
    pub text: String,
}

impl FooterSpan {
    /// 构造一段底栏文本：`key` 为主题表取色标识，`fallback` 是查不到时的缺省色。
    pub fn new(key: &'static str, fallback: &'static str, text: impl Into<String>) -> Self {
        FooterSpan {
            key,
            fallback,
            text: text.into(),
        }
    }
}

/// 底栏一行 = 多段带样式文本
pub type FooterLine = Vec<FooterSpan>;

/// 渲染一帧底栏所需的上下文
pub struct FooterCtx<'a> {
    /// 底栏可用宽度（显示列数）
    pub width: usize,
    /// 工作目录
    pub cwd: &'a str,
    /// 当前模型 id
    pub model: Option<&'a str>,
    /// 当前模型是否支持 thinking
    pub model_reasoning: bool,
    /// 当前 thinking 级别（None 按 "off" 显示）
    pub thinking_level: Option<&'a str>,
    /// 虚拟模型本次请求路由到的物理模型 id；未路由（或非虚拟模型）时为 None。
    pub routed_model: Option<&'a str>,
    /// 路由后实际使用的 thinking 级别（已钳制到物理模型）；未路由或未记录时为 None。
    pub routed_thinking_level: Option<&'a str>,
    /// 当前 git 分支（无 git 仓库时为 None）
    pub git_branch: Option<&'a str>,
    /// 上下文使用率 0-100
    pub context_percent: f64,
    /// 模型上下文窗口（token）
    pub context_window: u32,
    /// 是否正在执行任务（agent 运行中）
    pub busy: bool,
    /// 在途流式输出的 token 数（tiktoken BPE 真实计数，text + thinking + toolcall 参数）
    /// usage 只在消息结束（`message_end`）时才落地，流式期间 `messages` 里的 usage 不增长；
    /// 需要实时速率的底栏用它拿到在途 token。
    pub streaming_output_tokens: u64,
    /// 消息历史（usage 累加数据源）
    pub messages: &'a [AgentMessage],
}

/// 底栏扩展接口
///
/// 扩展内部用 `Mutex` 维护统计状态（工具计数、任务计时等），
/// 事件经 [`dispatch_agent_event`] 流入 `on_agent_event`。
pub trait FooterExtension: Send + Sync {
    /// 扩展名（诊断用）
    fn name(&self) -> &str;

    /// 扩展描述（/extension 面板详情展示）
    fn description(&self) -> &str {
        ""
    }

    /// 该扩展声明的模式列表（每个声明即一个级别；`All` 为兜底隐式全有，无需写出）
    fn modes(&self) -> Vec<ExtensionMode> {
        Vec::new()
    }

    /// agent 内核事件流入（agent_start / agent_end / tool_execution_end / ...），
    fn on_agent_event(&self, _event: &Value) {}

    /// 会话历史就绪：TUI 启动（`--resume` / `-c` 等）与会话切换
    /// （`/new` `/resume` `/import` `/fork` `/clone`）时各调用一次，`messages` 为新会话的
    /// 完整历史，`session_path` 为 None 表示全新会话。
    ///
    /// footer 的事件计数是内存态，而渲染出的 usage 来自 `messages`；不在此按历史重建，
    /// 切换后计数会停留在上一个会话、与用量行对不上。默认空实现。
    fn on_session_switched(&self, _session_path: Option<&str>, _messages: &[AgentMessage]) {}

    /// 渲染底栏，返回显示行（每行由带主题样式的片段组成；TUI 按行数分配高度）
    fn render(&self, ctx: &FooterCtx) -> Vec<FooterLine>;
}

/// 解析工作目录的 git 分支（读 .git/HEAD；支持 worktree 的 gitdir: 指向）。
/// 非 git 仓库返回 None。
pub fn git_branch(cwd: &str) -> Option<String> {
    let root = Path::new(cwd);
    let git = root.join(".git");
    let git_dir = if git.is_dir() {
        git
    } else if git.is_file() {
        let content = std::fs::read_to_string(&git).ok()?;
        let p = content.trim().strip_prefix("gitdir:")?.trim();
        if p.is_empty() {
            return None;
        }
        root.join(p)
    } else {
        return None;
    };
    let head = std::fs::read_to_string(git_dir.join("HEAD")).ok()?;
    head.trim()
        .strip_prefix("ref: refs/heads/")
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_branch_parses_head() {
        let dir = std::env::temp_dir().join(format!("prux-footer-test-{}", std::process::id()));
        let git_dir = dir.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        let branch = git_branch(dir.to_str().unwrap());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(branch.as_deref(), Some("main"));
    }

    #[test]
    fn git_branch_none_outside_repo() {
        let dir = std::env::temp_dir().join(format!("prux-footer-none-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let branch = git_branch(dir.to_str().unwrap());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(branch, None);
    }

    #[test]
    fn git_branch_worktree_gitdir_file() {
        let dir = std::env::temp_dir().join(format!("prux-footer-wt-{}", std::process::id()));
        let git = dir.join(".git");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&git, "gitdir: /somewhere/else/.git/worktrees/wt\n").unwrap();
        // gitdir 指向不存在的目录 → None（解析本身成功但读 HEAD 失败）
        let branch = git_branch(dir.to_str().unwrap());
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(branch, None);
    }

    #[test]
    fn dispatch_reaches_all_extensions() {
        // registered_footers 读扩展模式（进程级共享）：持锁串行并重置环境
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::extensions::set_extension_mode(crate::core::extensions::ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        struct Spy(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        impl FooterExtension for Spy {
            fn name(&self) -> &str {
                "spy"
            }
            fn on_agent_event(&self, event: &Value) {
                // 只记录、不断言：本 footer 注册后无法注销，其它并行用例
                // （如 goal）派发的 `goal:changed` 也会到这里；在 handler 里断言
                // 会把别人的事件误判为本用例的失败。
                if let Some(t) = event.get("type").and_then(|v| v.as_str()) {
                    self.0.lock().unwrap().push(t.to_string());
                }
            }
            fn render(&self, _ctx: &FooterCtx) -> Vec<FooterLine> {
                Vec::new()
            }
        }
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        super::super::registry::register_footer_extension(Spy(seen.clone()));
        crate::core::extensions::dispatch_agent_event(&serde_json::json!({ "type": "x" }));
        assert!(
            seen.lock().unwrap().iter().any(|t| t == "x"),
            "footer 扩展应收到派发的事件"
        );
    }

    #[test]
    fn session_switched_reaches_footers_with_history() {
        // 与 dispatch_reaches_all_extensions 同一套进程级共享状态：持锁串行并重置环境
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::extensions::set_extension_mode(crate::core::extensions::ExtensionMode::All);
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();
        struct SessionSpy(std::sync::Arc<std::sync::Mutex<Vec<String>>>);
        impl FooterExtension for SessionSpy {
            fn name(&self) -> &str {
                "session-spy"
            }
            fn on_session_switched(&self, path: Option<&str>, messages: &[AgentMessage]) {
                self.0.lock().unwrap().push(format!(
                    "{}:{}",
                    path.unwrap_or("new"),
                    messages.len()
                ));
            }
            fn render(&self, _ctx: &FooterCtx) -> Vec<FooterLine> {
                Vec::new()
            }
        }
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        super::super::registry::register_footer_extension(SessionSpy(seen.clone()));

        let msgs = [AgentMessage::user_text("hi")];
        crate::core::extensions::dispatch_session_switched(Some("/tmp/s.jsonl"), &msgs);
        assert!(
            seen.lock().unwrap().iter().any(|s| s == "/tmp/s.jsonl:1"),
            "footer 应随会话切换拿到新会话路径与历史，实得 {:?}",
            seen.lock().unwrap()
        );

        // 启动恢复路径（main 在会话就绪后调用）共用同一分发
        crate::core::extensions::dispatch_footer_session(None, &[]);
        assert!(
            seen.lock().unwrap().iter().any(|s| s == "new:0"),
            "启动路径也要把历史交给 footer，实得 {:?}",
            seen.lock().unwrap()
        );
    }
}

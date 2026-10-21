//! `permissions` 扩展的端到端门控测试：真实配置文件 → 策略加载 → 工具调用判定。
//!
//! 不经过 TUI（门控逻辑本身不依赖 UI），只验证「写下去的配置到底拦不拦得住」。

use prux::core::extensions::{
    Extension, ExtensionUiRequest, set_ui_ready, submit_ui_choice, take_pending_ui,
};
use prux::core::settings_manager::agent_dir;
use prux::extensions::permissions::Permissions;
use prux::test_support::AgentDirGuard;
use serde_json::json;
use std::{fs, path::PathBuf, sync::Mutex, time::Duration};

/// 交互式测试的全局开关串行锁：`ui_ready` 与 `PRUX_AGENT_DIR` 都是进程级状态。
static UI_TEST_LOCK: Mutex<()> = Mutex::new(());

/// 进程级 agent_dir override（spawn 出去的任务读不到线程本地的 `AgentDirGuard`）。
///
/// 用完必须还原：其它测试依赖真实/线程本地路径。
struct EnvAgentDir {
    /// 还原用的上一份取值（`None` = 原本没设）。
    prev: Option<String>,
    /// 本次使用的临时目录（随结构体一起删）。
    _dir: tempfile::TempDir,
}

impl EnvAgentDir {
    /// 建一个临时 agent_dir 并 pin 到进程环境变量。
    fn temp() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("PRUX_AGENT_DIR").ok();
        // SAFETY: 由 UI_TEST_LOCK 串行化，且只在测试进程内使用。
        unsafe { std::env::set_var("PRUX_AGENT_DIR", dir.path()) };
        EnvAgentDir { prev, _dir: dir }
    }

    /// 配置目录路径。
    fn config_dir(&self) -> PathBuf {
        self._dir.path().join("extensions").join("permissions")
    }
}

impl Drop for EnvAgentDir {
    /// 还原环境变量。
    fn drop(&mut self) {
        // SAFETY: 同 `temp`，调用方持锁。
        unsafe {
            match &self.prev {
                Some(v) => std::env::set_var("PRUX_AGENT_DIR", v),
                None => std::env::remove_var("PRUX_AGENT_DIR"),
            }
        }
    }
}

/// 等队列里出现 Select 请求并返回它的 id（丢弃其它请求）。
async fn wait_select_id() -> u64 {
    for _ in 0..400 {
        if let Some(req) = take_pending_ui() {
            if let ExtensionUiRequest::Select { id, .. } = req {
                return id;
            }
            continue;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("no approval panel was queued");
}

/// 把配置写进测试 agent_dir 的全局配置位置。
fn write_config(text: &str) {
    let dir = agent_dir().join("extensions").join("permissions");
    fs::create_dir_all(&dir).expect("create config dir");
    fs::write(dir.join("config.json"), text).expect("write config");
}

/// 调一次异步工具前钩子；返回 `Some(reason)` 表示被拦下。
///
/// 测试进程里没有 TUI 事件循环（`ui_ready()` 为 false），所以命中 `ask` 的调用
/// 一定走 fail-closed 分支——这正是这些用例要断言的行为。
fn gate(tool: &str, args: serde_json::Value) -> Option<String> {
    // 这些用例断言的是「没有面板时 fail-closed」：必须拿锁并显式关掉 UI，
    // 否则并行跑的交互式用例会把 ui_ready 打开，`ask` 就会去弹面板并等下去。
    let _g = UI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    set_ui_ready(false);
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let out = rt
        .block_on(Permissions.before_tool_call_async(
            tool.to_string(),
            args,
            None,
            Vec::new(),
            json!({}),
        ))
        .expect("pre-tool hook must not error");
    out.block.then(|| out.reason.unwrap_or_default())
}

/// `path` 层拦 `.env`，放行普通源文件与 `.env.example`。
#[test]
fn path_layer_blocks_env_but_allows_sources() {
    let _ad = AgentDirGuard::temp();
    write_config(
        &json!({
            "permissionReviewLog": false,
            "permission": {
                "*": "allow",
                "path": {
                    "*": "allow",
                    "*.env": "deny",
                    "*.env.*": "deny",
                    "*.env.example": "allow"
                }
            }
        })
        .to_string(),
    );

    let blocked = gate("read", json!({"path": ".env"})).expect("read .env must be blocked");
    assert!(blocked.contains("*.env"), "{blocked}");
    assert!(blocked.contains("Blocked by permissions"), "{blocked}");

    assert!(gate("read", json!({"path": "src/main.rs"})).is_none());
    assert!(gate("read", json!({"path": ".env.example"})).is_none());
}

/// `ask` 降级为 fail-closed：拦下并给出 `/permissions allow` 指引。
#[test]
fn ask_fails_closed_with_grant_hint() {
    let _ad = AgentDirGuard::temp();
    write_config(
        &json!({
            "permissionReviewLog": false,
            "permission": {"*": "allow", "bash": {"*": "allow", "sudo *": "ask"}}
        })
        .to_string(),
    );

    let reason = gate("bash", json!({"command": "sudo ls"})).expect("sudo must be blocked");
    assert!(reason.contains("sudo *"), "{reason}");
    assert!(reason.contains("/permissions allow bash"), "{reason}");
    assert!(gate("bash", json!({"command": "ls -la"})).is_none());
}

/// bash 链与命令替换都要逐条判定。
#[test]
fn bash_chain_and_substitution_are_gated() {
    let _ad = AgentDirGuard::temp();
    write_config(
        &json!({
            "permissionReviewLog": false,
            "permission": {"*": "allow", "bash": {"*": "allow", "rm -rf /*": "deny"}}
        })
        .to_string(),
    );

    assert!(gate("bash", json!({"command": "cd /tmp && rm -rf /"})).is_some());
    assert!(gate("bash", json!({"command": "echo $(rm -rf /)"})).is_some());
    assert!(gate("bash", json!({"command": "cargo test"})).is_none());
}

/// 看透不了的包装与解析失败一律按 ask（fail-closed）。
#[test]
fn opaque_wrappers_fail_closed() {
    let _ad = AgentDirGuard::temp();
    write_config(
        &json!({
            "permissionReviewLog": false,
            "permission": {"*": "allow", "bash": {"*": "allow"}}
        })
        .to_string(),
    );

    assert!(gate("bash", json!({"command": "bash -c 'ls'"})).is_some());
    assert!(gate("bash", json!({"command": "eval \"ls\""})).is_some());
    assert!(gate("bash", json!({"command": "echo 'unterminated"})).is_some());
}

/// 全 deny 的工具表面会被从模型可见工具表里摘掉。
#[test]
fn all_deny_surface_is_hidden() {
    let _ad = AgentDirGuard::temp();
    write_config(
        &json!({
            "permissionReviewLog": false,
            "permission": {"*": "allow", "web_search": "deny", "read": "allow"}
        })
        .to_string(),
    );

    let selected = vec!["read".to_string(), "web_search".to_string()];
    let kept = Permissions.filter_tools(selected);
    assert_eq!(kept, vec!["read".to_string()]);
}

/// 有 TUI 时命中 `ask` 会弹面板；选 "Approve once" 后这次调用被放行。
#[test]
fn ask_panel_approve_once_allows_the_call() {
    let _g = UI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let ad = EnvAgentDir::temp();
    fs::create_dir_all(ad.config_dir()).expect("create config dir");
    fs::write(
        ad.config_dir().join("config.json"),
        json!({
            "permissionReviewLog": false,
            "permission": {"*": "allow", "bash": {"*": "allow", "sudo *": "ask"}}
        })
        .to_string(),
    )
    .expect("write config");

    while take_pending_ui().is_some() {}
    set_ui_ready(true);

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let outcome = rt.block_on(async {
        let handle = tokio::spawn(async {
            Permissions
                .before_tool_call_async(
                    "bash".to_string(),
                    json!({"command": "sudo ls"}),
                    None,
                    Vec::new(),
                    json!({}),
                )
                .await
                .expect("hook")
        });
        let id = wait_select_id().await;
        submit_ui_choice(id, Some("Approve once".to_string()));
        handle.await.expect("gate task")
    });

    set_ui_ready(false);
    assert!(!outcome.block, "approve once 应放行: {:?}", outcome.reason);
}

/// 选 "Approve for this session" 后同一 `(surface, pattern)` 不再弹面板。
#[test]
fn ask_panel_session_choice_is_remembered() {
    let _g = UI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let ad = EnvAgentDir::temp();
    fs::create_dir_all(ad.config_dir()).expect("create config dir");
    fs::write(
        ad.config_dir().join("config.json"),
        json!({
            "permissionReviewLog": false,
            "permission": {"*": "allow", "bash": {"*": "allow", "mkfs*": "ask"}}
        })
        .to_string(),
    )
    .expect("write config");

    while take_pending_ui().is_some() {}
    set_ui_ready(true);

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let first = rt.block_on(async {
        let handle = tokio::spawn(async {
            Permissions
                .before_tool_call_async(
                    "bash".to_string(),
                    json!({"command": "mkfs.ext4 /dev/sdz"}),
                    None,
                    Vec::new(),
                    json!({}),
                )
                .await
                .expect("hook")
        });
        let id = wait_select_id().await;
        submit_ui_choice(id, Some("Approve for this session".to_string()));
        handle.await.expect("gate task")
    });
    assert!(
        !first.block,
        "approve for this session 应放行: {:?}",
        first.reason
    );

    // 第二次同样的命令：命中会话放行表，不弹面板也不拦。
    let second = rt
        .block_on(Permissions.before_tool_call_async(
            "bash".to_string(),
            json!({"command": "mkfs.ext4 /dev/sdz"}),
            None,
            Vec::new(),
            json!({}),
        ))
        .expect("hook");
    set_ui_ready(false);
    assert!(!second.block, "会话放行后不应再拦: {:?}", second.reason);
}

/// 用户按 Esc（面板返回 None）时按拒绝处理。
#[test]
fn ask_panel_cancel_denies() {
    let _g = UI_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let ad = EnvAgentDir::temp();
    fs::create_dir_all(ad.config_dir()).expect("create config dir");
    fs::write(
        ad.config_dir().join("config.json"),
        json!({
            "permissionReviewLog": false,
            "permission": {"*": "allow", "bash": {"*": "allow", "reboot *": "ask"}}
        })
        .to_string(),
    )
    .expect("write config");

    while take_pending_ui().is_some() {}
    set_ui_ready(true);

    let rt = tokio::runtime::Runtime::new().expect("runtime");
    let outcome = rt.block_on(async {
        let handle = tokio::spawn(async {
            Permissions
                .before_tool_call_async(
                    "bash".to_string(),
                    json!({"command": "reboot now"}),
                    None,
                    Vec::new(),
                    json!({}),
                )
                .await
                .expect("hook")
        });
        let id = wait_select_id().await;
        submit_ui_choice(id, None);
        handle.await.expect("gate task")
    });

    set_ui_ready(false);
    assert!(outcome.block, "取消应当拒绝");
    let reason = outcome.reason.unwrap_or_default();
    assert!(reason.contains("reboot *"), "{reason}");
}

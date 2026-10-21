//! vet 扩展的端到端测试：验证结算边界真的跑检查、把失败喂回模型，并按三条收敛条件收手。
//!
//! 这些测试走真实的 `dispatch_boundary` 分发路径（不是直接调 `on_boundary`），
//! 因此覆盖「扩展注册 → 钩子门控 → 边界决策 → 注入消息」整条链。
//! 全局扩展注册表跨测试共享，必须用一把叶级锁串行（无嵌套 → 无锁序死锁）。

use prux::core::extensions::{
    Extension, dispatch_boundary, register_extension, set_extension_enabled, unregister_extension,
};
use prux::core::project_trust::is_project_trusted;
use prux::core::settings_manager::agent_dir;
use prux::extensions::vet::Vet;
use prux::test_support::{AgentDirGuard, CwdGuard};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    sync::{Mutex, MutexGuard, OnceLock},
};

/// 序列化对全局扩展注册表的读写。
static TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// 取序列化锁（中毒时取回内部数据继续）。
fn guard() -> MutexGuard<'static, ()> {
    TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// 把 `vet.json` 写进当前测试的临时 `agent_dir()`。
fn write_config(config: &Value) {
    let dir = agent_dir().join("extensions");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("vet.json"),
        serde_json::to_string_pretty(config).unwrap(),
    )
    .unwrap();
}

/// 注册并启用 vet；返回持有注册/启用副作用的测试现场。
struct Fixture {
    /// 临时 agent_dir 与临时项目目录的守卫，drop 时清理。
    _agent: AgentDirGuard,
    /// 临时项目目录（测试里当 cwd 用）。
    project: tempfile::TempDir,
    /// cwd override 守卫（drop 时还原）。
    _cwd: CwdGuard,
}

impl Fixture {
    /// 建一个全新环境：临时 agent_dir + 临时项目目录 + 已启用的 vet 扩展。
    fn new(config: &Value) -> Self {
        let agent = AgentDirGuard::temp();
        let project = tempfile::tempdir().unwrap();
        let cwd = CwdGuard::set(project.path());
        write_config(config);

        register_extension(Vet);
        set_extension_enabled("vet", true);

        Fixture {
            _agent: agent,
            project,
            _cwd: cwd,
        }
    }

    /// 项目目录路径。
    fn path(&self) -> &Path {
        self.project.path()
    }

    /// 记下会话 cwd（等价于 `on_session_start`）。
    fn session(&self) {
        Vet.on_session_start(&self.path().to_string_lossy(), &[], None);
    }

    /// 模拟一次写类工具调用，把本轮标脏。
    fn touch(&self) {
        Vet.before_tool_call_ext("edit", &Value::Null, None, &[], &Value::Null)
            .unwrap();
    }

    /// 触发一次主 agent 结算边界。
    fn settle(&self) -> prux::core::extensions::BoundaryOutcome {
        dispatch_boundary(&json!({
            "type": "agent_before_settle",
            "agentScope": "main",
            "outcome": "completed",
            "hasQueuedMessages": false,
        }))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        unregister_extension("vet");
    }
}

/// 结算事件里追加的消息文本（没有则空串）。
fn appended_text(outcome: &prux::core::extensions::BoundaryOutcome) -> String {
    outcome
        .append
        .iter()
        .map(|m| m.text())
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(unix)]
#[test]
fn settle_runs_checks_and_injects_the_failure_report() {
    let _l = guard();
    let fx = Fixture::new(&json!({
        "maxIterations": 2,
        "checks": [{"name": "test", "command": "echo assertion-failed >&2; exit 101"}],
    }));
    fx.session();
    fx.touch();

    let outcome = fx.settle();
    assert!(outcome.r#continue, "有阻塞失败时必须续跑");
    assert!(!outcome.end);

    let report = appended_text(&outcome);
    assert!(report.contains("[vet] project checks failed"), "{report}");
    assert!(report.contains("attempt 1 of 2"), "{report}");
    assert!(report.contains("test"), "{report}");
    assert!(
        report.contains("assertion-failed"),
        "失败输出要进报告: {report}"
    );
    assert!(report.contains("exit 101"), "{report}");
}

#[cfg(unix)]
#[test]
fn passing_checks_do_not_resume_the_turn() {
    let _l = guard();
    let fx = Fixture::new(&json!({
        "maxIterations": 2,
        "checks": [{"name": "test", "command": "echo all-good"}],
    }));
    fx.session();
    fx.touch();

    let outcome = fx.settle();
    assert!(!outcome.r#continue);
    assert!(!outcome.end);
    assert!(outcome.append.is_empty(), "通过时不该往上下文塞东西");
}

#[cfg(unix)]
#[test]
fn the_loop_stops_after_max_iterations() {
    let _l = guard();
    let fx = Fixture::new(&json!({
        "maxIterations": 1,
        "checks": [{"name": "test", "command": "exit 3"}],
    }));
    fx.session();

    // 第一次：续跑
    fx.touch();
    assert!(fx.settle().r#continue);

    // 第二次：跑满上限 → 只报告，不再续跑
    fx.touch();
    let outcome = fx.settle();
    assert!(!outcome.r#continue, "跑满 maxIterations 后必须收手");
    assert!(outcome.append.is_empty());
}

#[cfg(unix)]
#[test]
fn the_same_failure_does_not_loop_forever() {
    let _l = guard();
    let fx = Fixture::new(&json!({
        "maxIterations": 5,
        "checks": [{"name": "test", "command": "echo same-boom >&2; exit 1"}],
    }));
    fx.session();

    fx.touch();
    assert!(fx.settle().r#continue, "第一次失败应续跑");

    // 同一条失败（输出一致）再出现 → 指纹相同 → 收手，即便还有迭代额度
    fx.touch();
    let outcome = fx.settle();
    assert!(!outcome.r#continue, "同一条失败反复出现时不该继续烧 token");
}

#[cfg(unix)]
#[test]
fn turns_without_edits_skip_the_checks() {
    let _l = guard();
    let fx = Fixture::new(&json!({
        "maxIterations": 2,
        "checks": [{"name": "test", "command": "echo should-not-run"}],
    }));
    fx.session();

    // requireEdits 缺省开：本轮没调用过写类工具 → 不跑
    let outcome = fx.settle();
    assert!(!outcome.r#continue);
    assert!(outcome.append.is_empty());

    // 关掉 requireEdits 后，同一个纯问答回合也会跑检查
    let fx2 = Fixture::new(&json!({
        "requireEdits": false,
        "maxIterations": 2,
        "checks": [{"name": "test", "command": "exit 9"}],
    }));
    fx2.session();
    assert!(fx2.settle().r#continue, "关掉 requireEdits 后应照跑");
}

#[cfg(unix)]
#[test]
fn auto_run_off_disables_the_whole_settle_path() {
    let _l = guard();
    let fx = Fixture::new(&json!({
        "autoRun": false,
        "maxIterations": 2,
        "checks": [{"name": "test", "command": "exit 7"}],
    }));
    fx.session();
    fx.touch();

    let outcome = fx.settle();
    assert!(!outcome.r#continue);
    assert!(outcome.append.is_empty());
}

#[cfg(unix)]
#[test]
fn no_checks_means_no_work() {
    let _l = guard();
    let fx = Fixture::new(&json!({"maxIterations": 2, "checks": []}));
    fx.session();
    fx.touch();

    let outcome = fx.settle();
    assert!(!outcome.r#continue);
    assert!(outcome.append.is_empty());
}

#[cfg(unix)]
#[test]
fn aborted_or_queued_turns_are_left_alone() {
    let _l = guard();
    let fx = Fixture::new(&json!({
        "maxIterations": 2,
        "checks": [{"name": "test", "command": "exit 4"}],
    }));
    fx.session();
    fx.touch();

    let aborted = dispatch_boundary(&json!({
        "type": "agent_before_settle",
        "agentScope": "main",
        "outcome": "aborted",
    }));
    assert!(!aborted.r#continue, "中断后的结算不该续跑");

    let queued = dispatch_boundary(&json!({
        "type": "agent_before_settle",
        "agentScope": "main",
        "hasQueuedMessages": true,
    }));
    assert!(!queued.r#continue, "有排队用户输入时该让位");
}

#[cfg(unix)]
#[test]
fn optional_failures_are_reported_but_do_not_block() {
    let _l = guard();
    let fx = Fixture::new(&json!({
        "maxIterations": 2,
        "checks": [{"name": "slow-e2e", "command": "exit 5", "optional": true}],
    }));
    fx.session();
    fx.touch();

    let outcome = fx.settle();
    assert!(!outcome.r#continue, "可选检查失败不该阻止收工");
    assert!(outcome.append.is_empty());
}

#[cfg(unix)]
#[test]
fn an_untrusted_project_config_is_ignored() {
    let _l = guard();
    let fx = Fixture::new(&json!({}));

    // 项目里放一份「跑检查」的配置：未受信项目不该让它生效
    let project_dir = fx.path().join(prux::PROJECT_SCOPE_NAME).join("extensions");
    fs::create_dir_all(&project_dir).unwrap();
    fs::write(
        project_dir.join("vet.json"),
        r#"{"checks": [{"name": "evil", "command": "exit 1"}]}"#,
    )
    .unwrap();
    assert!(
        !is_project_trusted(fx.path(), &agent_dir()),
        "临时项目不应被视为已受信"
    );

    fx.session();
    fx.touch();

    let outcome = fx.settle();
    assert!(!outcome.r#continue, "未受信项目的检查不该被执行");
    assert!(outcome.append.is_empty());
}

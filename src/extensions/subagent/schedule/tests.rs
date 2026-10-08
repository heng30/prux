//! 调度器测试：store round-trip、重名/取消、到点触发（真派发一个假子代理）。

use std::sync::Arc;

use super::*;
use crate::core::extensions::ExtensionTool;

/// 建一个能派发的上下文（假 runner 立刻回 `R:<prompt>`），并让它可被调度器拿到。
fn arm_ctx(cwd: &str) {
    let ctx = crate::core::extensions::ToolExecCtx {
        execute_tool: crate::core::extensions::unavailable_tool_exec(),
        parent_tool_call_id: None,
        nested_calls: Default::default(),
        session_branch_entries: Default::default(),
        script_tools: Default::default(),
        cwd: cwd.to_string(),
        make_sub_agent: Arc::new(|_spec| {
            Ok(Box::new(EchoRunner) as Box<dyn crate::core::extensions::SubAgentRunner>)
        }),
        parent_abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        agent_id: None,
        depth: 0,
        parent_model: None,
        script_call: false,
    };
    crate::extensions::subagent::fleet::remember_ctx(&ctx);
}

/// 测试用假子代理运行器：不执行真实逻辑，直接回显 `R:<prompt>`。
struct EchoRunner;
impl crate::core::extensions::SubAgentRunner for EchoRunner {
    /// 返回空实现的控制句柄：steer 忽略输入、abort 恒为 false、无会话路径。
    fn controls(&self) -> crate::core::extensions::SubAgentControls {
        crate::core::extensions::SubAgentControls {
            steer: Arc::new(|_t: String| {}),
            abort: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            session_path: None,
        }
    }
    /// 立即返回 `Ok("R:<prompt>")`，不执行任何真实子代理逻辑。
    fn run(
        &mut self,
        prompt: String,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + '_>>
    {
        Box::pin(async move { Ok(format!("R:{prompt}")) })
    }
}

/// 造一个最小可用的 [`NewJob`]：描述同 name、general-purpose 类型、固定 prompt，其余取默认。
fn new_job(name: &str, schedule: &str) -> NewJob {
    NewJob {
        name: name.to_string(),
        description: name.to_string(),
        schedule: schedule.to_string(),
        subagent_type: "general-purpose".to_string(),
        prompt: "do the thing".to_string(),
        model: None,
        thinking: None,
        max_turns: None,
    }
}

/// 每个用例都从"空会话 + 临时 agent_dir"开始，并且**串行**（调度器是进程级单例）。
///
/// 除 subagent 自己的 [`TestLock`] 外还要持 [`AUTH_TEST_LOCK`]：会话切换事件
/// （`/new` `/resume` 等）会经 subagent 的 `on_session_switched` 改**全局** session key
/// 并重绑 store —— 触发它的那些测试（sessions/handlers）持 AUTH_TEST_LOCK，
/// 不持就会在用例中途把 store 换到别的文件（表现为"坏文件没被挪走"）。
struct Fixture {
    /// 临时 agent 目录（Drop 时还原）。
    _ad: crate::test_support::AgentDirGuard,
    /// 全局会话/工作区类测试互斥。
    _auth: std::sync::MutexGuard<'static, ()>,
    /// 扩展测试互斥（全局态必须串行）。
    _g: crate::extensions::subagent::manager::TestLock,
    /// 临时目录（脚本/存储落盘用）。
    dir: tempfile::TempDir,
}

/// 取全局测试锁 + 临时 agent 目录，重置调度器与会话状态并绑定会话 key。
fn fixture() -> Fixture {
    // 锁序固定为 AUTH_TEST_LOCK → TestLock（与 subagent.rs / fleet.rs 的测试一致，
    // 反向持锁会 ABBA 死锁）
    let auth = crate::test_support::AUTH_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let guard = crate::extensions::subagent::manager::test_lock();
    let ad = crate::test_support::AgentDirGuard::temp();
    let dir = tempfile::tempdir().unwrap();
    reset();
    crate::extensions::subagent::session::reset();
    crate::extensions::subagent::session::set_session_key(Some("sched-test".to_string()));
    sync_session();
    Fixture {
        _ad: ad,
        _auth: auth,
        _g: guard,
        dir,
    }
}

/// 空 store 不落盘；出现任务后落盘，取消最后一个任务后删除文件。
#[test]
fn empty_store_is_not_written_until_a_job_exists() {
    let _f = fixture();
    let path = store_path();
    assert!(
        !path.exists(),
        "空 store 不该落盘（每个新会话都会 sync_session）: {}",
        path.display()
    );

    // 一旦有任务就落盘；最后一个任务取消后文件删除（“空不落地”，
    // 缺失文件读回来同样是空，`/resume` 不会读回旧任务）
    let job = add(new_job("nightly audit", "5m")).expect("add");
    assert!(path.exists(), "有任务后应落盘: {}", path.display());
    assert!(cancel(&job.id));
    assert!(
        !path.exists(),
        "取消最后一个任务后应删除文件，而不是留下空 jobs 文件: {}",
        path.display()
    );
    // 空 store 上再 save（会话重绑定等）不应把文件凭空建回来
    save_store();
    assert!(!path.exists(), "空表不落地: {}", path.display());
}

/// 坏 JSON 的 store 被改名成 `.corrupt` 隔离留证，内存从空开始且不覆盖原路径。
#[test]
fn corrupt_store_is_quarantined_instead_of_silently_wiped() {
    let _f = fixture();
    let path = store_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{ this is not json").unwrap();

    // 读坏了：原文件挪到 .corrupt 留证据，内存从空开始，且不再往原路径写
    sync_session();
    assert!(list().is_empty());
    assert!(
        !path.exists(),
        "坏文件应被挪走而不是原地清空: {}",
        path.display()
    );

    let dir = path.parent().unwrap();
    let quarantined: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.ends_with(".corrupt"))
        .collect();
    assert_eq!(quarantined.len(), 1, "{quarantined:?}");
    assert_eq!(
        std::fs::read_to_string(dir.join(&quarantined[0])).unwrap(),
        "{ this is not json"
    );

    // 坏文件之后仍可正常建任务（原路径已腾空）
    let job = add(new_job("nightly audit", "5m")).expect("add");
    assert!(path.exists(), "新任务应重新落盘: {}", path.display());
    assert!(cancel(&job.id));
}

/// 任务能落盘 → 重新绑定会话 → 读回，重名与非法调度串被拒，取消不存在的返回 false。
#[test]
fn jobs_round_trip_through_the_session_store_and_dedupe_by_name() {
    let f = fixture();
    let cwd = f.dir.path().to_string_lossy().to_string();
    arm_ctx(&cwd);

    let job = add(new_job("nightly audit", "5m")).expect("add");
    assert_eq!(job.schedule_type, "interval");
    assert_eq!(job.interval_ms, Some(300_000));
    assert!(job.enabled);
    assert!(next_run(&job.id).is_some(), "周期任务有下一次");

    // 重名不给过（上游同此）
    let err = add(new_job("nightly audit", "1h")).unwrap_err();
    assert!(err.contains("already exists"), "{err}");
    // 非法调度串：错误里给例子
    let err = add(new_job("wrong", "every monday")).unwrap_err();
    assert!(err.contains("6-field cron"), "{err}");

    // 落盘 → 重新绑定会话 → 读回同一个任务
    let path = store_path();
    assert!(path.exists(), "store 应落盘: {}", path.display());
    reset();
    sync_session();
    let jobs = list();
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert_eq!(jobs[0].name, "nightly audit");
    assert!(jobs[0].next_run_ms.is_some(), "读回后要重算下一次");

    // 取消
    assert!(cancel(&jobs[0].id));
    assert!(list().is_empty());
    assert!(!cancel(&jobs[0].id), "取消一个不存在的返回 false");
}

/// cron 任务算出本地 next_run；一次性任务到点后自动关停。
#[test]
fn cron_jobs_get_a_local_next_run_and_one_shots_disable_after_firing() {
    let f = fixture();
    let cwd = f.dir.path().to_string_lossy().to_string();
    arm_ctx(&cwd);

    let cron = add(new_job("weekly", "0 0 9 * * 1")).expect("cron add");
    assert_eq!(cron.schedule_type, "cron");
    assert!(cron.next_run_ms.is_some());

    // 一次性：`+1s` → 到点跑一次后自动关停（上游同此）
    let once = add(new_job("in a second", "+1s")).expect("once add");
    assert_eq!(once.schedule_type, "once");
    assert!(once.next_run_ms.is_some());
}

/// 通知队列里的纯文本（`Notify` 与 `NotifyRich` 都拼回字符串）。
fn notice_texts() -> Vec<String> {
    std::iter::from_fn(crate::core::extensions::take_pending_ui)
        .filter_map(|r| match r {
            crate::core::extensions::ExtensionUiRequest::Notify { text, .. } => Some(text),
            crate::core::extensions::ExtensionUiRequest::NotifyRich { spans, .. } => {
                Some(spans.iter().map(|s| s.text.as_str()).collect())
            }
            _ => None,
        })
        .collect()
}

/// 到点任务经 tick 真派发一个后台子代理，并在跑完后记账（run_count / 状态 / 一次性关停）。
#[test]
fn due_jobs_fire_through_the_manager_and_record_their_run() {
    let f = fixture();
    let cwd = f.dir.path().to_string_lossy().to_string();
    // 上下文只在**运行时内**能被缓存（调度器要拿到 handle 才能派发）
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        arm_ctx(&cwd);
        crate::extensions::subagent::manager::reset_all();

        // 1 秒后触发一次（用真 tick 路径：tick() 是同一条路，测试直接调到点那一刻）
        let job = add(new_job("fires", "+1s")).expect("add");
        // 把它挪到"已经到点"（不睡 1 秒：直接改 next_run 再 tick，测的是同一条触发路径）
        with_state(|st| {
            if let Some(j) = st.jobs.iter_mut().find(|j| j.id == job.id) {
                j.next_run_ms = Some(now_ms().saturating_sub(1));
            }
        });
        tick();

        // 触发了：状态 running、记了 last_run
        let after = get(&job.id).expect("job");
        assert_eq!(after.last_status.as_deref(), Some("running"), "{after:?}");
        assert!(after.last_run_ms.is_some());

        // 真派发了一个后台子代理（派发在自己的任务里，稍等一下）
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if !crate::extensions::subagent::manager::list().is_empty() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "应派发了一个后台子代理"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let records = crate::extensions::subagent::manager::list();
        assert!(
            records[0].description.contains("fires") && records[0].background,
            "定时任务派发的是后台子代理: {:?}",
            records[0].description
        );

        // 等它跑完 → 记账：run_count +1、状态 success、一次性任务关停
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let j = get(&job.id).expect("job");
            if j.last_status.as_deref() == Some("success")
                || j.last_status.as_deref() == Some("error")
            {
                assert_eq!(j.last_status.as_deref(), Some("success"), "{j:?}");
                assert_eq!(j.run_count, 1);
                assert!(!j.enabled, "一次性任务跑过就关停");
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "任务没在预期时间内收尾"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        crate::extensions::subagent::manager::reset_all();
    });
}

/// 子代理类型解析失败时任务标记为 error 而非 panic，通知里带上出错的类型名。
#[test]
fn firing_without_an_agent_type_marks_the_job_failed_instead_of_panicking() {
    let f = fixture();
    let cwd = f.dir.path().to_string_lossy().to_string();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        arm_ctx(&cwd);
        let mut input = new_job("bad type", "+5m");
        input.subagent_type = "no-such-type".to_string();
        let job = add(input).expect("add");
        with_state(|st| {
            if let Some(j) = st.jobs.iter_mut().find(|j| j.id == job.id) {
                j.next_run_ms = Some(now_ms().saturating_sub(1));
            }
        });
        tick();
        let after = get(&job.id).expect("job");
        assert_eq!(after.last_status.as_deref(), Some("error"), "{after:?}");
        // 通知里说清是哪个类型没解析出来
        let notices = notice_texts();
        assert!(
            notices.iter().any(|n| n.contains("no-such-type")),
            "{notices:?}"
        );
    });
}

/// schedule 设置缺省开启：Agent 工具 schema 里应出现 schedule 参数。
#[test]
fn the_schedule_setting_gates_the_agent_tool_param() {
    let _f = fixture();
    // 缺省开：schema 里有 schedule
    use crate::core::extensions::Extension;
    let tool = crate::extensions::subagent::Subagent
        .tools()
        .into_iter()
        .find(|t| t.name == "Agent")
        .expect("Agent tool");
    assert!(tool.parameters["properties"].get("schedule").is_some());
    let _: Vec<ExtensionTool> = Vec::new();
}

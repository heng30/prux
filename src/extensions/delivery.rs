//! `delivery` 扩展：用 `/delivery run` 把改动交付出去——建分支、提交、推、开 PR，并等 CI
//! 与评审的结论回到聊天区。
//!
//! **只手动触发**：交付会写远端、产生一个可评审的 PR，这是「这版代码值得被人看」的判断，
//! 只有用户能下。挂在结算边界上自动跑，等于把每轮还没被看过的改动都推成 PR——包括坏的。
//!
//! `/delivery run` 的路径：从基线分支新建 `prux/<日期>-<时分>` → `git add -A` + commit →
//! `git push` → 已有 PR 就复用、没有就 `gh pr create` → 轮询到全绿 / 有阻塞 / 超时，把结论
//! 贴回聊天区。CI 红了就把阻塞项原样列出，你让 agent 修完再跑一次 `/delivery run`：这时
//! 已经在 `prux/` 分支上，改动提交到同一条分支，同一个 PR 被更新。
//!
//! 三条不做的线（每条都是「替用户做了决定」）：
//! - **不自动 merge**：合入是人的决定（`stage` 只到 `pr`）。
//! - **不自动交付、不自动续跑**：没有结算边界钩子，没有把 CI 失败喂回模型的循环。
//! - **不猜远端**：`origin` 不存在、`gh` 没登录、`base` 探测不出来，都停下来报原因。
//!
//! 配置在 `agent_dir()/extensions/delivery.json`（见 [`config`]）；git 在
//! [`crate::utils::git`]、`gh` 与 PR 状态解析在 [`crate::utils::gh`]，`/delivery` 命令族在
//! [`command`]。
//!
//! **默认不启用**，声明 `Dev` 模式——它会写 git 历史并推远端，只能由用户显式打开。

mod command;
mod config;

use crate::{
    core::{
        extensions::{Extension, ExtensionCommand, ExtensionHook, ExtensionMode, ExtensionTool, UiNotifyLevel},
        provider::AgentMessage,
        session_manager::Session,
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_DELIVERY,
        util::{generate_id, notify_text},
    },
    modes::tui::handlers::register_host_command,
    utils::{
        gh::{self, Verdict},
        git,
        glyphs::DEF_TRAILING_ELLIPSIS,
        paths::cwd,
        time::{now_iso, short_date},
        truncate::truncate_chars,
    },
};
use config::{Config, Stage};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

/// 约定俗成的基线分支名：`origin/HEAD` 缺失时（新仓库、没有远端）兜底，免得往 `main` 上直接提交。
const CONVENTIONAL_BASES: &[&str] = &["main", "master", "trunk"];

/// 本扩展新建分支的前缀；带这个名字的分支说明是上一次交付留下的，改动就续在它上面。
const BRANCH_PREFIX: &str = "prux/";

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static DELIVERY_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_DELIVERY,
    make: || -> Arc<dyn Extension> { Arc::new(Delivery) },
};

/// 当前会话的 cwd（`on_session_start` 记录；拿不到时回落到进程 cwd）。
static SESSION_CWD: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 本扩展的运行态（只用于 `/delivery status` 展示与标题）。
static STATE: Mutex<State> = Mutex::new(State::new());

/// 交付闸：同一时刻只允许一次交付在跑（防止连点 `/delivery run`）。
static RUNNING: AtomicBool = AtomicBool::new(false);

/// 配置告警只提示一次的开关。
static WARNED: AtomicBool = AtomicBool::new(false);

/// 会话标题（PR 标题 / 提交信息）的字符上限。
const TITLE_CHARS: usize = 72;

/// PR 正文里用户第一条消息的字符上限。
const BODY_CHARS: usize = 2000;

/// 最近一次交付的结论（`/delivery status` 展示）。
#[derive(Debug, Clone)]
struct Note {
    /// 结论一句话。
    text: String,
    /// 发生时刻。
    at: Instant,
}

/// 跨回合保留的运行态。
struct State {
    /// 会话标题（第一条用户输入的单行摘要）。
    title: Option<String>,
    /// 本会话交付过的分支。
    branch: Option<String>,
    /// 本会话交付过的 PR 号。
    pr: Option<u64>,
    /// 最近一次交付结论。
    last: Option<Note>,
}

impl State {
    /// 全零初值（`static` 初始化要求 `const`，所以不能走 `Default`）。
    const fn new() -> Self {
        State {
            title: None,
            branch: None,
            pr: None,
            last: None,
        }
    }
}

/// 取运行态；锁被毒化时取回内部数据继续用。
fn state() -> MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// 记下本次会话的 cwd。
fn remember_cwd(cwd_text: &str) {
    *SESSION_CWD.lock().unwrap_or_else(|e| e.into_inner()) = Some(PathBuf::from(cwd_text));
}

/// 当前生效的 cwd：会话记录的优先，其次进程 cwd，都拿不到时用 `.`。
fn current_cwd() -> PathBuf {
    if let Some(dir) = SESSION_CWD
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
    {
        return dir;
    }
    cwd()
}

/// 交付闸的持有凭证：drop 时自动放行。
struct RunGuard;

impl RunGuard {
    /// 抢占执行闸；已有交付在跑时返回 `None`。
    fn acquire() -> Option<RunGuard> {
        RUNNING
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .ok()
            .map(|_| RunGuard)
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
    }
}

/// 一次交付尝试的结论。
enum Step {
    /// 没什么要交付的（工作区干净且不在交付分支上 / 不是 git 仓库），静默跳过。
    Nothing,
    /// 交付完成（消息进聊天区）。
    Done(String),
    /// 停下来交回用户（CI 红、配置问题、认证、超时）。
    Stop(String),
}

/// `delivery` 扩展。
pub struct Delivery;

impl Extension for Delivery {
    /// 扩展注册名（`/extension` 面板与 `/delivery` 命令共用）。
    fn name(&self) -> &str {
        config::EXT
    }

    /// 面板里的功能说明。
    fn description(&self) -> &str {
        "Deliver on /delivery run: commit to a session branch, push, open a pull request, then \
         report the CI and review result. Never merges, never delivers on its own. \
         Config: agent_dir()/extensions/delivery.json; /delivery status|run|stage"
    }

    /// 仅在 Dev 及以上模式可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 默认不启用：会写 git 历史并推远端，属于用户显式选择的能力。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供工具：交付是用户敲 `/delivery run` 的动作，模型看不到它。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/delivery` 命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: config::CMD.to_string(),
            description: "Delivery: /delivery status|run|stage <branch|push|pr>".to_string(),
            // handler 只读写本扩展自身的配置与运行态，不锁 agent。
            busy_safe: true,
            subcommands: command::SUBCOMMANDS.to_vec(),
        }]
    }

    /// 接线 `/delivery` 执行入口。
    fn on_registered(&self) {
        register_host_command(config::EXT, config::CMD, command::handle);
    }

    /// 只订阅用户提交（拿会话标题）；交付本身没有任何自动触发点。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::UserPrompt]
    }

    /// 启用时先加载一次配置（把告警提示出去）。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            _ = config::load();
        } else {
            WARNED.store(false, Ordering::Relaxed);
        }
    }

    /// 记录会话 cwd。
    fn on_session_start(
        &self,
        cwd_text: &str,
        _messages: &[AgentMessage],
        _session: Option<&Session>,
    ) {
        remember_cwd(cwd_text);
        WARNED.store(false, Ordering::Relaxed);
    }

    /// 用户提交输入：第一条非空行留作提交信息 / PR 标题。
    fn on_user_submit(&self, text: &str, _busy: bool) {
        let title = title_of(text);
        if title.is_some() {
            let mut st = state();
            if st.title.is_none() {
                st.title = title;
            }
        }
    }
}

/// 记下最近一次结论（`/delivery status` 用）。
fn record(text: String) {
    state().last = Some(Note {
        text,
        at: Instant::now(),
    });
}

/// 会话标题：第一条用户输入压成单行并截断。
fn title_of(text: &str) -> Option<String> {
    let line = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())?
        .to_string();
    Some(truncate_chars(&line, TITLE_CHARS, None, true))
}

/// 提交信息 / PR 标题：会话标题，没有就用固定文案。
fn session_title() -> String {
    state()
        .title
        .clone()
        .filter(|title| !title.trim().is_empty())
        .unwrap_or_else(|| "prux session changes".to_string())
}

/// 新的会话分支名 `prux/<date>-<hhmm>`；重名时加短后缀。
fn fresh_branch(top: &Path) -> String {
    let stamp = now_iso();
    let date = short_date(&stamp);
    let time = stamp.get(11..16).unwrap_or("00:00").replace(':', "");
    let base = format!("{BRANCH_PREFIX}{date}-{time}");
    if !git::branch_exists(top, &base) {
        return base;
    }
    format!("{base}-{}", &generate_id()[..4])
}

/// 目标分支是不是「基线」（不许直接提交上去）。
fn on_base(top: &Path, current: &str, cfg: &Config) -> bool {
    if !cfg.base.is_empty() && current == cfg.base {
        return true;
    }
    if git::local_default_branch(top).is_some_and(|base| base == current) {
        return true;
    }
    CONVENTIONAL_BASES.contains(&current)
}

/// 交付主体：量工作区 → 建分支 → 提交 → 推 → 开 PR → 等 CI。
fn run_delivery(cfg: &config::Config) -> Result<Step, String> {
    let dir = current_cwd();
    // 不是 git 仓库就没有交付可言：静默跳过（这个扩展不该在非 git 目录里刷提示）
    let Ok(top) = git::toplevel(&dir) else {
        return Ok(Step::Nothing);
    };

    let current = git::branch(&top)?;
    if current == "HEAD" {
        return Ok(Step::Stop(
            "detached HEAD — check out a branch first".to_string(),
        ));
    }

    let has_changes = git::dirty(&top)?;
    // 工作区干净、又不在交付分支上：没有本扩展可以接手的东西（在别人的分支上提交不是它的活）
    if !has_changes && !current.starts_with(BRANCH_PREFIX) {
        return Ok(Step::Nothing);
    }

    let mut branch = current;
    if has_changes && on_base(&top, &branch, cfg) {
        branch = fresh_branch(&top);
        git::create_branch(&top, &branch)?;
    }
    if has_changes {
        let message = format!("prux: {}", session_title());
        git::commit_all(&top, &message)?;
    }
    state().branch = Some(branch.clone());

    if cfg.stage == Stage::Branch {
        return Ok(if has_changes {
            Step::Done(format!("committed to `{branch}`"))
        } else {
            Step::Nothing
        });
    }

    if crate::core::runtime::offline() {
        return Ok(Step::Stop(
            "offline mode is on — the branch is committed but not pushed".to_string(),
        ));
    }

    if let Err(err) = git::push(&top, &branch) {
        return Ok(Step::Stop(format!(
            "{err} (branch `{branch}` is committed locally)"
        )));
    }
    if cfg.stage == Stage::Push {
        return Ok(Step::Done(format!("pushed `{branch}` to origin")));
    }

    gh::auth_ok(&top)?;
    let base = if cfg.base.is_empty() {
        gh::default_branch(&top)?
    } else {
        cfg.base.clone()
    };
    let number = match gh::pr_number(&top, &branch)? {
        Some(number) => number,
        None => {
            let body = format!(
                "{}\n\n---\nDelivered by prux from this session.\n",
                session_body()
            );
            let url = gh::create_pr(&top, &branch, &base, &session_title(), &body, cfg.draft)?;
            let number = gh::pr_number(&top, &branch)?.unwrap_or(0);
            notify_text(
                &format!("delivery: opened PR #{number} {url}"),
                UiNotifyLevel::Info,
            );
            number
        }
    };
    state().pr = Some(number);
    wait_for_ci(cfg, &top, &branch)
}

/// PR 正文：用户最初那条需求的摘录。
fn session_body() -> String {
    truncate_chars(&session_title(), BODY_CHARS, None, true)
}

/// 轮询 PR 直到出结论、超时或阻塞。
///
/// `shortcut`：推完先等一个 `poll` 周期再读状态——`gh` 的检查结论可能还是上一次推送的，
/// 立刻读会把旧结论当新的。要精确到「这次推送的 head sha」得逐条比对 check run 的
/// `headSha`，等有人被误判成绿了再说。
fn wait_for_ci(cfg: &config::Config, top: &Path, branch: &str) -> Result<Step, String> {
    let started = Instant::now();
    let deadline = started + cfg.timeout;
    loop {
        std::thread::sleep(cfg.poll);
        let status = gh::pr_status(top, branch)?;
        match status.verdict {
            Verdict::Green => {
                return Ok(Step::Done(format!(
                    "PR #{} is green ({})",
                    status.number, status.url
                )));
            }
            Verdict::Blocked(failure) => {
                return Ok(Step::Stop(report(
                    &failure,
                    status.number,
                    &status.url,
                )));
            }
            Verdict::Pending(desc) => {
                if Instant::now() >= deadline {
                    return Ok(Step::Stop(format!(
                        "{desc} after {}s — giving the turn back. PR #{} {}",
                        started.elapsed().as_secs(),
                        status.number,
                        status.url
                    )));
                }
            }
        }
    }
}

/// 把阻塞项拼成交回用户的一段报告。
///
/// 事实部分来自 [`gh::Failure::details`]；「修完再跑一次 `/delivery run`」这层语义属于
/// 交付扩展，不塞进 `utils::gh`。
fn report(failure: &gh::Failure, number: u64, url: &str) -> String {
    format!(
        "PR #{number} is not ready ({url}). Fix what is listed, then /delivery run again to \
         update it.\n\n{}",
        failure.details()
    )
}

/// `/delivery run` 的同步入口（供 [`command`] 在后台线程里调用）。
pub(super) fn run_now() -> String {
    let cfg = config::load();
    let Some(guard) = RunGuard::acquire() else {
        return "a delivery is already running".to_string();
    };
    let step = run_delivery(&cfg);
    drop(guard);
    let text = match step {
        Ok(Step::Nothing) => "nothing to deliver (clean working tree)".to_string(),
        Ok(Step::Done(text) | Step::Stop(text)) => text,
        Err(err) => err,
    };
    record(text.clone());
    text
}

/// `/delivery status` 的文本。
pub(super) fn status_text() -> String {
    let cfg = config::load();
    let (branch, pr, last) = {
        let st = state();
        (st.branch.clone(), st.pr, st.last.clone())
    };
    let mut out = format!(
        "delivery: stage {} · timeout {}s · poll {}s · base {} · draft {} · only /delivery run \
         triggers it\nconfig: {}",
        cfg.stage.as_label(),
        cfg.timeout.as_secs(),
        cfg.poll.as_secs(),
        if cfg.base.is_empty() {
            "auto".to_string()
        } else {
            cfg.base.clone()
        },
        cfg.draft,
        config::config_path().display(),
    );
    out.push_str(&format!(
        "\nbranch: {} · PR: {}",
        branch.as_deref().unwrap_or("—"),
        pr.map(|n| format!("#{n}"))
            .unwrap_or_else(|| "—".to_string()),
    ));
    out.push_str(&match last {
        Some(note) => format!(
            "\nlast delivery ({}s ago): {}",
            note.at.elapsed().as_secs(),
            note.text
        ),
        None => "\nlast delivery: never".to_string(),
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::HomeGuard;

    #[test]
    fn title_comes_from_the_first_non_empty_line() {
        let _serial = serial();
        assert_eq!(
            title_of("\n\n  加个 JSON 输出  \n细节").as_deref(),
            Some("加个 JSON 输出")
        );
        assert_eq!(title_of("   \n "), None);
        let long = "x".repeat(200);
        assert_eq!(title_of(&long).unwrap().chars().count(), TITLE_CHARS);
    }

    #[test]
    fn fresh_branch_name_is_date_based_and_non_empty() {
        let _serial = serial();
        let dir = std::env::temp_dir();
        let name = fresh_branch(&dir);
        assert!(name.starts_with(BRANCH_PREFIX), "{name}");
        assert!(!name.contains(':'), "分支名不能带冒号: {name}");
        assert!(name.len() > BRANCH_PREFIX.len());
    }

    #[test]
    fn config_is_readable_from_the_extension_dir() {
        let _serial = serial();
        let _guard = HomeGuard::temp();
        let cfg = config::load();
        assert_eq!(cfg.timeout.as_secs(), config::DEFAULT_TIMEOUT_SECS);
        assert!(status_text().contains("stage pr"));
    }

    /// 交付测试串行锁：运行态是进程级单例，并发测试会互相踩。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 取串行锁。
    fn serial() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 造一个临时 git 仓库，返回它的路径；`git` 不可用时返回 `None`。
    fn temp_repo(tag: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!(
            "prux-delivery-{tag}-{}",
            generate_id()
        ));
        std::fs::create_dir_all(&dir).ok()?;
        let ok = std::process::Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?
            .success();
        if !ok {
            return None;
        }
        for (key, value) in [("user.email", "t@example.com"), ("user.name", "prux test")] {
            std::process::Command::new("git")
                .args(["config", key, value])
                .current_dir(&dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .ok()?;
        }
        std::fs::write(dir.join("README.md"), "hello\n").ok()?;
        std::process::Command::new("git")
            .args(["add", "-A"])
            .current_dir(&dir)
            .status()
            .ok()?;
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .ok()?;
        Some(dir)
    }

    #[test]
    fn commits_onto_a_fresh_branch_instead_of_the_base_branch() {
        let _serial = serial();
        let _guard = crate::test_support::AgentDirGuard::temp();
        let Some(repo) = temp_repo("commit") else {
            return; // 没有 git 的环境跳过
        };
        remember_cwd(&repo.to_string_lossy());
        std::fs::write(repo.join("work.txt"), "changes\n").unwrap();

        let mut cfg = config::merge(None);
        cfg.stage = Stage::Branch;
        config::write(&cfg).unwrap();

        let Ok(Step::Done(text)) = run_delivery(&cfg) else {
            panic!("交付失败");
        };
        assert!(text.contains("committed to `prux/"), "{text}");

        let branch = git::branch(&repo).unwrap();
        assert!(branch.starts_with(BRANCH_PREFIX), "不在基线分支上提交: {branch}");
        assert!(!git::dirty(&repo).unwrap(), "改动都已提交");
    }

    #[test]
    fn a_second_run_keeps_committing_to_the_branch_it_created() {
        let _serial = serial();
        let _guard = crate::test_support::AgentDirGuard::temp();
        let Some(repo) = temp_repo("reuse") else {
            return;
        };
        remember_cwd(&repo.to_string_lossy());

        let mut cfg = config::merge(None);
        cfg.stage = Stage::Branch;
        config::write(&cfg).unwrap();

        std::fs::write(repo.join("one.txt"), "one\n").unwrap();
        run_delivery(&cfg).unwrap();
        let first = git::branch(&repo).unwrap();

        std::fs::write(repo.join("two.txt"), "two\n").unwrap();
        run_delivery(&cfg).unwrap();
        assert_eq!(git::branch(&repo).unwrap(), first, "第二次交付不该另开分支");
        assert!(!git::dirty(&repo).unwrap());
    }

    #[test]
    fn a_clean_tree_outside_a_delivery_branch_is_silent() {
        let _serial = serial();
        let _guard = HomeGuard::temp();
        // 进程 cwd 不是 git 仓库 / 干净工作区：都该是 Nothing 而不是报错
        let cfg = config::merge(None);
        match run_delivery(&cfg) {
            Ok(Step::Nothing) => {}
            Ok(Step::Done(text) | Step::Stop(text)) => panic!("不该有交付动作: {text}"),
            Err(err) => panic!("不该报错: {err}"),
        }
    }
}

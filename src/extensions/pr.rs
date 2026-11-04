//! `pr` 扩展：用 `/pr run` / `/pr new` 把改动交付出去——建分支、提交、推、开 PR，并等 CI
//! 与评审的结论回到聊天区。
//!
//! **只手动触发**：交付会写远端、产生一个可评审的 PR，这是「这版代码值得被人看」的判断，
//! 只有用户能下。挂在结算边界上自动跑，等于把每轮还没被看过的改动都推成 PR——包括坏的。
//!
//! `/pr run` 的路径：从基线分支新建 `prux/<日期>-<时分>` → `git add -A` + commit（提交信息
//! 由模型看暂存区写，写不出来才回落会话标题）→
//! `git push` → 已有**开着**的 PR 就复用、没有（含已关闭 / 已合并）就 `gh pr create` →
//! 轮询到全绿 / 有阻塞 / 超时，把结论贴回聊天区。工作区干净但本地有没推的提交时同样会
//! 推上去开 PR——「已经 commit 了」不是停下来等的理由。CI 红了就把阻塞项原样列出，你让
//! agent 修完再跑一次 `/pr run`：这时已经在 `prux/` 分支上，改动提交到同一条分支，同一个
//! PR 被更新。
//!
//! `/pr new` 走同一条路，但**不查已有 PR、也不续用当前交付分支**：它总是从当前状态新建一条
//! `prux/` 分支，因此拿到的是一条全新的 PR。GitHub 不允许同一个 head 分支挂两条开着的 PR，
//! 「全新」只能从新分支来。工作区干净、也没有没推的提交时它什么也不做——否则就是拿同一批
//! 提交再开一条内容重复的 PR。
//!
//! 三条不做的线（每条都是「替用户做了决定」）：
//! - **不自动 merge**：合入是人的决定，它最高只到开 PR。
//! - **不自动交付、不自动续跑**：没有结算边界钩子，没有把 CI 失败喂回模型的循环。
//! - **不猜远端**：`origin` 不存在、`gh` 没登录、`base` 探测不出来，都停下来报原因。
//!
//! 配置在 `agent_dir()/extensions/pr.json`（见 [`config`]）；git 在 [`crate::utils::git`]、
//! `gh` 与 PR 状态解析在 [`crate::utils::gh`]，`/pr` 命令族在 [`command`]。
//!
//! **默认不启用**，声明 `Dev` 模式——它会写 git 历史并推远端，只能由用户显式打开。

mod command;
mod commit;
mod config;
mod describe;

use crate::{
    core::{
        auth,
        extensions::{
            Extension, ExtensionCommand, ExtensionHook, ExtensionMode, ExtensionTool, UiNotifyLevel,
        },
        model_resolver,
        provider::{self, AgentMessage},
        session_manager::Session,
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_PR,
        util::{generate_id, notify_text, session_id_of},
    },
    modes::tui::handlers::register_host_command,
    utils::{
        async_task,
        gh::{self, Verdict},
        git,
        paths::cwd,
        time::{now_iso, short_date},
        truncate::truncate_chars,
    },
};
use config::Config;
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

/// 会话标题（PR 标题 / 提交信息）的字符上限。
const TITLE_CHARS: usize = 72;

/// PR 正文里用户第一条消息的字符上限。
const BODY_CHARS: usize = 2000;

/// 当前会话的 cwd（`on_session_start` 记录；拿不到时回落到进程 cwd）。
static SESSION_CWD: Mutex<Option<PathBuf>> = Mutex::new(None);

/// 当前会话的 provider 会话 id（`on_session_start` 记录）：一次性请求要带上它，
/// 缺了它 opencode 系 provider 直接回 400（`MissingSessionID`）。
static SESSION_ID: Mutex<Option<String>> = Mutex::new(None);

/// 当前会话正在用的系统提示词（`ContextWithSystem` 记录）：生成提交信息时原样复用，
/// 用户的提交约定就写在里面。
static SESSION_SYSTEM: Mutex<Option<String>> = Mutex::new(None);

/// 本扩展的运行态（只用于 `/pr status` 展示与标题）。
static STATE: Mutex<State> = Mutex::new(State::new());

/// 交付闸：同一时刻只允许一次交付在跑（防止连点 `/pr run`）。
static RUNNING: AtomicBool = AtomicBool::new(false);

/// 配置告警只提示一次的开关。
static WARNED: AtomicBool = AtomicBool::new(false);

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static PR_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_PR,
    make: || -> Arc<dyn Extension> { Arc::new(Pr) },
};

/// 最近一次交付的结论（`/pr status` 展示）。
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

/// 记下当前会话 id；`None` 不覆盖已有值（新建会话尚未落盘时没有 id，别把可用的覆盖成空）。
pub(super) fn remember_session(id: Option<String>) {
    if let Some(id) = id {
        *SESSION_ID.lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
    }
}

/// 当前会话 id（可能为空：扩展中途启用、会话尚未落盘）。
fn session_id() -> Option<String> {
    SESSION_ID.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 当前会话的系统提示词（**原样**用）；请求还没发过时为 `None`。
fn session_system() -> Option<String> {
    SESSION_SYSTEM
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .filter(|text| !text.trim().is_empty())
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
    /// 没什么要交付的（工作区干净且没有未推送的提交 / 不是 git 仓库），静默跳过。
    Nothing,
    /// 交付完成（消息进聊天区）。
    Done(String),
    /// 停下来交回用户（CI 红、配置问题、认证、超时）。
    Stop(String),
}

/// `pr` 扩展。
pub struct Pr;

impl Extension for Pr {
    /// 扩展注册名（`/extension` 面板与 `/pr` 命令共用）。
    fn name(&self) -> &str {
        config::EXT
    }

    /// 面板里的功能说明。
    fn description(&self) -> &str {
        "Deliver on /pr run: commit to a session branch, push, open a pull request, then report \
         the CI and review result. /pr new always starts a new branch and a new pull request. \
         Never merges, never delivers on its own. \
         Config: agent_dir()/extensions/pr.json; /pr status|run|new"
    }

    /// 仅在 Dev 及以上模式可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 默认不启用：会写 git 历史并推远端，属于用户显式选择的能力。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供工具：交付是用户敲 `/pr run` 的动作，模型看不到它。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/pr` 命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: config::CMD.to_string(),
            description: "Pull request: /pr status|run|new".to_string(),
            // handler 只读写本扩展自身的配置与运行态，不锁 agent。
            busy_safe: true,
            subcommands: command::SUBCOMMANDS.to_vec(),
        }]
    }

    /// 接线 `/pr` 执行入口。
    fn on_registered(&self) {
        register_host_command(config::EXT, config::CMD, command::handle);
    }

    /// 只订阅用户提交（拿会话标题）与含 system 的 transcript（拿当前系统提示词）；
    /// 交付本身没有任何自动触发点。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::UserPrompt, ExtensionHook::ContextWithSystem]
    }

    /// 启用时先加载一次配置（把告警提示出去）。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            _ = config::load();
        } else {
            WARNED.store(false, Ordering::Relaxed);
        }
    }

    /// 记录会话 cwd 与会话 id。
    fn on_session_start(
        &self,
        cwd_text: &str,
        _messages: &[AgentMessage],
        session: Option<&Session>,
    ) {
        remember_cwd(cwd_text);
        remember_session(session.map(|s| s.session_id.clone()));
        WARNED.store(false, Ordering::Relaxed);
    }

    /// 会话切换：换成新会话 id（切回旧会话时旧 id 要被替换）。
    fn on_session_switched(&self, session_path: Option<&str>, _messages: &[AgentMessage]) {
        remember_session(session_id_of(session_path));
    }

    /// 记下本次请求的系统提示词（**不改动** `messages`）：生成提交信息时复用同一份。
    fn transform_context_with_system(
        &self,
        messages: &mut Vec<AgentMessage>,
    ) -> crate::error::Result<()> {
        if let Some(system) = messages.first().filter(|message| message.role == "system") {
            let text = system.text();
            let mut slot = SESSION_SYSTEM.lock().unwrap_or_else(|e| e.into_inner());
            if slot.as_deref() != Some(text.as_str()) {
                *slot = Some(text);
            }
        }
        Ok(())
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

/// 记下最近一次结论（`/pr status` 用）。
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

/// 提交信息用的模型（provider, model_id）；`/pr run` 时从宿主取，取不到就回落会话标题。
type Model = Option<(String, String)>;

/// 交付模式：两种子命令唯一的区别在「要不要复用开着的那条 PR」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// `/pr run`：同一条交付分支上的改动续在同一个 PR 上，已有开着的 PR 就复用它。
    Run,
    /// `/pr new`：不查已有 PR，总是从当前状态新建分支、开一条全新的 PR。
    New,
}

/// 交付主体：量工作区 → 建分支 → 提交 → 推 → 开 PR → 等 CI。
fn deliver(cfg: &Config, mode: Mode, model: Model) -> Result<Step, String> {
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
    let on_base_branch = on_base(&top, &current, cfg);

    // 工作区干净、也没有本地已提交但没推的提交：没有本扩展可以接手的东西
    // （亮在基线分支上的提交不归它推，那是直接往 main 上提交）
    let unpushed = !on_base_branch && git::unpushed(&top)?;
    let pending = has_changes || unpushed;

    // `/pr run` 在交付分支上重复跑仍然有意义（重推 + 接着等 CI）；`/pr new` 没有新东西就什么也不做，
    // 否则会拿同一批提交再开一条内容重复的 PR。
    if !pending && (mode == Mode::New || !current.starts_with(BRANCH_PREFIX)) {
        return Ok(Step::Nothing);
    }

    let mut branch = current;
    // `/pr new` 一律换新分支：GitHub 不允许同一个 head 分支挂两条开着的 PR，
    // 「全新的 PR」只能从一条新分支来。
    if mode == Mode::New || (has_changes && on_base_branch) {
        branch = fresh_branch(&top);
        git::create_branch(&top, &branch)?;
    }
    if has_changes {
        git::stage_all(&top)?;
        git::commit(&top, &commit_message(&top, model.as_ref()))?;
    }
    state().branch = Some(branch.clone());

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
    gh::auth_ok(&top)?;
    let base = if cfg.base.is_empty() {
        gh::default_branch(&top)?
    } else {
        cfg.base.clone()
    };
    let model = model.as_ref();
    let number = match mode {
        // 不查已有 PR：分支是新的，`gh pr create` 不会撞上旧的那条。
        Mode::New => open_pr(&top, &branch, &base, cfg, model)?,
        Mode::Run => match gh::pr_for_branch(&top, &branch)? {
            Some(found) if found.is_open() => found.number,
            found => {
                // 关闭 / 合并的那条不能复用：再往上推，那条 PR 也不会重新收到这些提交。
                if let Some(found) = found {
                    notify_text(
                        &format!(
                            "pr: PR #{} is {} — opening a new one",
                            found.number,
                            found.state.to_ascii_lowercase()
                        ),
                        UiNotifyLevel::Info,
                    );
                }
                open_pr(&top, &branch, &base, cfg, model)?
            }
        },
    };
    state().pr = Some(number);
    // 按 PR 号轮询，不按分支名：同一分支上的旧 PR 会干扰 `gh pr view <branch>`。
    wait_for_ci(cfg, &top, &number.to_string())
}

/// 开一条新 PR 并把号返回。标题与正文由模型写（见 [`pr_describe`]）。
fn open_pr(
    top: &Path,
    branch: &str,
    base: &str,
    cfg: &Config,
    model: Option<&(String, String)>,
) -> Result<u64, String> {
    let (title, body) = pr_describe(top, base, model);
    let url = gh::create_pr(top, branch, base, &title, &body, cfg.draft)?;
    // 号从 `gh pr create` 打印的 URL 尾巴上取：这条分支上还挂着旧 PR 时，再查一次
    // `gh pr view <branch>` 可能读到那一条。
    let number = url
        .rsplit('/')
        .next()
        .and_then(|tail| tail.parse::<u64>().ok())
        .unwrap_or(0);
    notify_text(
        &format!("pr: opened PR #{number} {url}"),
        UiNotifyLevel::Info,
    );
    Ok(number)
}

/// PR 标题与正文：让模型看一眼整条分支的改动写一份 JSON，写不出来就回落会话标题 + 固定说明。
///
/// 生成失败只提示一句警告：一个标题不准的 PR 也比交付在本地停住强。
fn pr_describe(top: &Path, base: &str, model: Option<&(String, String)>) -> (String, String) {
    let title = session_title();
    let fallback = (title.clone(), session_body());
    let Some(model) = model else {
        return fallback;
    };
    let Ok(outline) = git::branch_outline(top, base, describe::OUTLINE_CHARS) else {
        return fallback;
    };
    if outline.trim().is_empty() {
        return fallback;
    }
    let prompt = describe::build_prompt(&outline, &title);
    match one_shot(model, describe::SYSTEM, &prompt, describe::MAX_TOKENS)
        .ok()
        .and_then(|text| describe::parse(&text))
    {
        Some(pr) => pr,
        None => {
            notify_text(
                "pr: could not write the PR title and body — using the session title",
                UiNotifyLevel::Warning,
            );
            fallback
        }
    }
}

/// PR 正文的兜底：用户最初那条需求的摘录。
fn session_body() -> String {
    truncate_chars(&session_title(), BODY_CHARS, None, true)
}

/// 提交信息：让模型看一眼已暂存的改动写一条，写不出来就回落 [`commit::fallback`]。
///
/// 生成失败只提示一句警告：交付照常继续（一条烂提交信息比「交付在本地停住」轻得多）。
fn commit_message(top: &Path, model: Option<&(String, String)>) -> String {
    let title = session_title();
    let fallback = commit::fallback(&title);
    let Some(model) = model else {
        return fallback;
    };
    let Ok(outline) = git::staged_outline(top, commit::OUTLINE_CHARS) else {
        return fallback;
    };
    if outline.trim().is_empty() {
        return fallback;
    }
    let prompt = commit::build_prompt(&outline, &title);
    // 提交信息用会话当前那一份提示词：用户的提交约定（前缀、语言、篇幅）就写在里面
    let system = session_system().unwrap_or_else(|| commit::SYSTEM.to_string());
    match one_shot(model, &system, &prompt, commit::MAX_TOKENS)
        .ok()
        .and_then(|text| commit::normalize(&text))
    {
        Some(message) => message,
        None => {
            notify_text(
                "pr: could not write the commit message — using the session title",
                UiNotifyLevel::Warning,
            );
            fallback
        }
    }
}

/// 一次性模型请求：刷 OAuth → 解析模型（带会话亲和 id）→ 非流式补全。
///
/// `system` 由调用方给：提交信息用会话当前那一份（用户写的提交约定就在里面），
/// PR 描述用它自己那份。
///
/// 拿到工具调用块时返回 `Err`：一次性请求**没有** `tools` 参数，而会话系统提示词里
/// 写着工具协议与工具清单，模型会把调用块当普通文本写出来——那东西不是提交信息，
/// 也不该进 PR 正文，当失败处理比让它落地强。
fn one_shot(
    model: &(String, String),
    system: &str,
    prompt: &str,
    max_tokens: u32,
) -> Result<String, String> {
    let (provider, model_id) = model;
    let text = async_task::run_blocking(async {
        auth::ensure_oauth_valid(provider)
            .await
            .map_err(|e| format!("{e}"))?;
        let model = model_resolver::configured_model(provider, model_id, None, session_id())
            .map_err(|e| format!("{e}"))?;
        let (text, _usage) = provider::simple_completion(&model, system, prompt, Some(max_tokens))
            .await
            .map_err(|e| format!("{e}"))?;
        Ok::<String, String>(text)
    })??;
    if looks_like_tool_call(&text) {
        return Err("the model answered with a tool call".to_string());
    }
    Ok(text)
}

/// 输出里是不是带了一个工具调用块（各家标签写法不同，这里只认开头的标记）。
///
/// 不裸匹配 `invoke`：英文提交信息里正常写 “invoke the hook” 很常见，误杀只会白白
/// 回落成会话标题。
fn looks_like_tool_call(text: &str) -> bool {
    [
        "<｜DSML｜",
        "<invoke",
        "<function_calls",
        "<parameter",
        "antml:",
    ]
    .iter()
    .any(|marker| text.contains(marker))
}

/// 轮询 PR 直到出结论、超时或阻塞。
///
/// `shortcut`：推完先等一个 `poll` 周期再读状态——`gh` 的检查结论可能还是上一次推送的，
/// 立刻读会把旧结论当新的。要精确到「这次推送的 head sha」得逐条比对 check run 的
/// `headSha`，等有人被误判成绿了再说。
fn wait_for_ci(cfg: &Config, top: &Path, selector: &str) -> Result<Step, String> {
    let started = Instant::now();
    let deadline = started + cfg.timeout;
    loop {
        std::thread::sleep(cfg.poll);
        let status = gh::pr_status(top, selector)?;
        match status.verdict {
            Verdict::Green => {
                return Ok(Step::Done(format!(
                    "PR #{} is green ({})",
                    status.number, status.url
                )));
            }
            Verdict::Blocked(failure) => {
                return Ok(Step::Stop(report(&failure, status.number, &status.url)));
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
/// 事实部分来自 [`gh::Failure::details`]；「修完再跑一次 `/pr run`」这层语义属于交付
/// 扩展，不塞进 `utils::gh`。
fn report(failure: &gh::Failure, number: u64, url: &str) -> String {
    format!(
        "PR #{number} is not ready ({url}). Fix what is listed, then /pr run again to update \
         it.\n\n{}",
        failure.details()
    )
}

/// `/pr run` / `/pr new` 的同步入口（供 [`command`] 在后台线程里调用）。
///
/// 返回结论文本与通知级别：全绿用 `Success`，其余（停下 / 超时 / 没东西可交）保持中性。
pub(super) fn run_now(mode: Mode, model: Model) -> (String, UiNotifyLevel) {
    let cfg = config::load();
    let Some(guard) = RunGuard::acquire() else {
        return (
            "a /pr run is already in flight".to_string(),
            UiNotifyLevel::Info,
        );
    };
    let step = deliver(&cfg, mode, model);
    drop(guard);
    let outcome = outcome(step);
    record(outcome.0.clone());
    outcome
}

/// 交付结论 → （给用户的一句话, 通知级别）：只有全绿那条带 success 色。
fn outcome(step: Result<Step, String>) -> (String, UiNotifyLevel) {
    match step {
        Ok(Step::Nothing) => (
            "nothing to deliver (clean working tree)".to_string(),
            UiNotifyLevel::Info,
        ),
        Ok(Step::Done(text)) => (text, UiNotifyLevel::Success),
        Ok(Step::Stop(text)) => (text, UiNotifyLevel::Info),
        Err(err) => (err, UiNotifyLevel::Info),
    }
}

/// `/pr status` 的文本。
pub(super) fn status_text() -> String {
    let cfg = config::load();
    let (branch, pr, last) = {
        let st = state();
        (st.branch.clone(), st.pr, st.last.clone())
    };
    let mut out = format!(
        "pr: timeout {}s · poll {}s · base {} · draft {} · only /pr run|new triggers it\nconfig: {}",
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
            "\nlast run ({}s ago): {}",
            note.at.elapsed().as_secs(),
            note.text
        ),
        None => "\nlast run: never".to_string(),
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
        assert!(status_text().contains("only /pr run|new triggers it"));
    }

    #[test]
    fn session_id_is_kept_when_a_later_event_has_none() {
        let _serial = serial();
        let id = "2026-01-01T00-00-00_abc";
        remember_session(Some(id.to_string()));
        remember_session(session_id_of(None));
        assert_eq!(session_id().as_deref(), Some(id), "没有会话路径不该清掉 id");
        assert_eq!(
            session_id_of(Some("/sessions/2026-01-01T00-00-00_abc.jsonl")).as_deref(),
            Some(id)
        );
    }

    #[test]
    fn only_a_green_pr_is_reported_as_success() {
        let _serial = serial();
        let (text, level) = outcome(Ok(Step::Done("PR #1 is green".to_string())));
        assert_eq!(text, "PR #1 is green");
        assert_eq!(level, UiNotifyLevel::Success);
        assert_eq!(
            outcome(Ok(Step::Stop("pending after 900s".to_string()))).1,
            UiNotifyLevel::Info,
            "超时 / 阻塞不是成功"
        );
        assert_eq!(
            outcome(Ok(Step::Nothing)).1,
            UiNotifyLevel::Info,
            "没东西可交也不算成功"
        );
    }

    #[test]
    fn tool_call_blocks_are_rejected_but_the_word_invoke_is_not() {
        assert!(looks_like_tool_call("<｜DSML｜ invoke name=\"read\">"));
        assert!(looks_like_tool_call("<function_calls>"));
        assert!(looks_like_tool_call("antml:invoke"));
        assert!(
            !looks_like_tool_call("invoke the hook before the test"),
            "正文里正常出现的 invoke 不算工具调用"
        );
        assert!(!looks_like_tool_call("Add merge sort"));
    }

    #[test]
    fn the_session_system_prompt_is_reused_verbatim() {
        let _serial = serial();
        let mut system = AgentMessage::user_text("自己的提交约定：中文 + [+/-/*] 前缀");
        system.role = "system".to_string();
        let mut messages = vec![system, AgentMessage::user_text("hi")];

        Pr.transform_context_with_system(&mut messages).unwrap();

        assert_eq!(
            session_system().as_deref(),
            Some("自己的提交约定：中文 + [+/-/*] 前缀")
        );
        assert_eq!(messages.len(), 2, "只读，不该改 transcript");
        assert_eq!(messages[0].role, "system");
    }

    /// 交付测试串行锁：运行态是进程级单例，并发测试会互相踩。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 取串行锁。
    fn serial() -> MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 造一个临时 git 仓库，返回它的路径；`git` 不可用时返回 `None`。
    fn temp_repo(tag: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("prux-pr-{tag}-{}", generate_id()));
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

    /// 一次没有远端可推的交付：提交成功，`git push` 失败（结论是 Stop，不 panic）。
    fn deliver_locally(cfg: &Config) -> Step {
        deliver_with(cfg, Mode::Run)
    }

    /// 同一条临时仓库上按指定模式交付一次。
    fn deliver_with(cfg: &Config, mode: Mode) -> Step {
        match deliver(cfg, mode, None) {
            Ok(step) => step,
            Err(err) => panic!("不该报错: {err}"),
        }
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

        let cfg = config::merge(None);
        assert!(
            matches!(deliver_locally(&cfg), Step::Stop(_)),
            "没有 origin，应当停在推远端"
        );

        let branch = git::branch(&repo).unwrap();
        assert!(
            branch.starts_with(BRANCH_PREFIX),
            "不在基线分支上提交: {branch}"
        );
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
        let cfg = config::merge(None);

        std::fs::write(repo.join("one.txt"), "one\n").unwrap();
        deliver_locally(&cfg);
        let first = git::branch(&repo).unwrap();

        std::fs::write(repo.join("two.txt"), "two\n").unwrap();
        deliver_locally(&cfg);
        assert_eq!(git::branch(&repo).unwrap(), first, "第二次交付不该另开分支");
        assert!(!git::dirty(&repo).unwrap());
    }

    #[test]
    fn new_mode_always_starts_a_brand_new_branch() {
        let _serial = serial();
        let _guard = crate::test_support::AgentDirGuard::temp();
        let Some(repo) = temp_repo("new") else {
            return;
        };
        remember_cwd(&repo.to_string_lossy());
        let cfg = config::merge(None);

        std::fs::write(repo.join("one.txt"), "one\n").unwrap();
        deliver_locally(&cfg);
        let first = git::branch(&repo).unwrap();

        std::fs::write(repo.join("two.txt"), "two\n").unwrap();
        deliver_with(&cfg, Mode::New);
        let second = git::branch(&repo).unwrap();
        assert!(second.starts_with(BRANCH_PREFIX), "{second}");
        assert_ne!(second, first, "/pr new 不该落在旧交付分支上");
        assert!(!git::dirty(&repo).unwrap());
    }

    #[test]
    fn a_committed_but_unpushed_branch_is_still_delivered() {
        let _serial = serial();
        let _guard = crate::test_support::AgentDirGuard::temp();
        let Some(repo) = temp_repo("unpushed") else {
            return;
        };
        remember_cwd(&repo.to_string_lossy());
        // 用户自己在一个功能分支上已经提交了，工作区干净：此时也得推上去开 PR
        git::create_branch(&repo, "feature").unwrap();
        std::fs::write(repo.join("done.txt"), "done\n").unwrap();
        git::commit_all(&repo, "already committed").unwrap();

        let cfg = config::merge(None);
        assert!(
            matches!(deliver_locally(&cfg), Step::Stop(_)),
            "没有 origin，应当停在推远端而不是当成没东西可交付"
        );
        assert_eq!(git::branch(&repo).unwrap(), "feature", "不该另开分支");
    }

    #[test]
    fn a_clean_base_branch_with_unpushed_commits_stays_silent() {
        let _serial = serial();
        let _guard = crate::test_support::AgentDirGuard::temp();
        let Some(repo) = temp_repo("base") else {
            return;
        };
        remember_cwd(&repo.to_string_lossy());
        std::fs::write(repo.join("main.txt"), "x\n").unwrap();
        git::commit_all(&repo, "直接提交在 main").unwrap();

        let cfg = config::merge(None);
        assert!(
            matches!(deliver_locally(&cfg), Step::Nothing),
            "基线分支上的本地提交不归本扩展推（等于直接推 main）"
        );
    }

    #[test]
    fn a_non_repo_directory_is_silent() {
        let _serial = serial();
        let _guard = HomeGuard::temp();
        // 必须是显式的临时目录：`cargo test` 的 cwd 就是本仓，忘了 remember_cwd 会直接
        // 交付真实工作区（提交 + 推 origin + 开 PR）。
        let dir = std::env::temp_dir().join(format!("prux-pr-plain-{}", generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        remember_cwd(&dir.to_string_lossy());

        let cfg = config::merge(None);
        assert!(matches!(deliver_locally(&cfg), Step::Nothing));
    }
}

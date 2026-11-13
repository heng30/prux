//! `review` 扩展：**入站**评审——把一个已经在 GitHub 上的 PR / issue 拉进来审，结论可回帖。
//!
//! 与 [`crate::extensions::pr`] 方向相反：那个把本地改动**推出去**变成 PR，这个把别人的
//! PR / issue **读进来**。两者互不依赖，也都不碰对方的分支。
//!
//! 三种用法：
//!
//! | 命令 | 作用 |
//! |---|---|
//! | `/review <PR 或 issue URL>` | 取元数据 + 在 `agent_dir()` 下开一份只读副本 + 把评审简报交给 agent |
//! | `/review --comment <URL>` | 同上，并在评审结束时把**最后一条回答**作为评论发到 PR / issue |
//! | `/review status` / `/review clean` | 看当前目标与已发布的评论 / 删掉所有副本 |
//!
//! 三条硬口径：
//!
//! - **只读副本在 `agent_dir()` 里，不动用户仓库**：用 `git init` + `git fetch --depth 1`
//!   取 `refs/pull/<n>/head`，不建 worktree、不写远端引用、不切用户的分支。评审期间 agent 在
//!   副本里读文件（简报里给的是绝对路径），**不改**那份副本，更不改用户仓库。
//! - **diff 用 GitHub 自己的**：`gh pr diff` 给的是三点（merge-base）差异，本地 shallow 副本
//!   算不出来；副本只负责「看不够时读全文」。
//! - **回帖幂等靠 marker，不靠本地账本**：评论正文首行带
//!   `<!-- prux-review:v1:<hash> -->`（哈希 = 目标 + 会话 id）。投递前先按 marker 扫一遍已有
//!   评论，命中就复用它的链接、不再发；发完再扫一遍确认落地，扫不到就报 `unconfirmed` 并
//!   **拒绝重发**——宁可让人看一眼，也不要在一个 PR 下贴两条一样的评审。
//!
//! **默认不启用，且只在 `Dev` 模式可用**：会建副本、跑网络请求，`--comment` 还会对外写评论。

use crate::{
    core::{
        extensions::{
            BoundaryOutcome, CommandHost, Extension, ExtensionCommand, ExtensionHook,
            ExtensionMode, ExtensionTool, ExtensionUiRequest, SubcommandDef, UiNotifyLevel,
            request_ui,
        },
        provider::AgentMessage,
        session_manager::Session,
        settings_manager::agent_dir,
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_REVIEW, command_arg,
        util::{notify_text, session_id_of},
    },
    modes::tui::handlers::register_host_command,
    utils::{gh::GH_TIMEOUT, git, glyphs::DEF_ELLIPSIS, time::now_iso, truncate::truncate_chars},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, MutexGuard, OnceLock},
    time::Duration,
};
use strum_macros::{EnumString, IntoStaticStr};

/// 扩展名（注册名，也是 `/extension` 面板里的标识）。
const EXT: &str = "review";

/// 斜杠命令名。
const CMD: &str = "review";

/// 副本与已发布评论记录的落盘目录（相对 `agent_dir()`）。
const REVIEW_SUBDIR: &str = "extensions/review";

/// 已发布评论记录的相对路径（相对 `agent_dir()`）。
const PUBLICATION_FILE: &str = "extensions/review/publications.json";

/// 拉 PR 的 git 操作超时（大仓库 shallow fetch 也可能几十秒）。
const GIT_TIMEOUT: Duration = Duration::from_secs(300);

/// 简报里 diff 正文的字符上限：整份 diff 再大也只把开头这段放进去，
/// 其余靠 agent 自己在副本里读（`review.diff` 完整落盘）。
const PROMPT_DIFF_CHARS: usize = 24_000;

/// 简报里 issue / PR 正文的字符上限。
const PROMPT_BODY_CHARS: usize = 4_000;

/// 简报里文件清单的最大条数。
const PROMPT_FILE_LINES: usize = 60;

/// 落盘的 diff 文件上限（字节）：超过就不写文件，只在简报里说明。
const DIFF_MAX_BYTES: usize = 8 * 1024 * 1024;

/// 评论正文上限（字符）：GitHub 的评论上限是 65536 字节，这里留足余量。
const COMMENT_MAX_CHARS: usize = 60_000;

/// `/review` 的子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "status",
        description: "Show the current review target, its checkout and the published comment",
    },
    SubcommandDef {
        name: "clean",
        description: "Delete every review checkout under agent_dir()",
    },
];

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static REVIEW_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_REVIEW,
    make: || -> Arc<dyn Extension> { Arc::new(Review) },
};

/// 被评审对象的种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
enum TargetKind {
    /// 拉取请求（`<host>/<owner>/<repo>/pull/<n>`）。
    Pull,
    /// 议题（`<host>/<owner>/<repo>/issues/<n>`）。
    Issue,
}

impl TargetKind {
    /// `gh` 的子命令名（`pr` / `issue`）。
    fn gh_command(self) -> &'static str {
        match self {
            TargetKind::Pull => "pr",
            TargetKind::Issue => "issue",
        }
    }

    /// 网页路径里的段名（`pull` / `issues`）——`gh` 的子命令名与它并不相同。
    fn path_segment(self) -> &'static str {
        match self {
            TargetKind::Pull => "pull",
            TargetKind::Issue => "issues",
        }
    }
}

/// 一个被评审的 GitHub 对象。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    /// 主机名（`github.com` 或 GHES 的主机）。
    host: String,
    /// 仓库所属账号 / 组织。
    owner: String,
    /// 仓库名。
    repo: String,
    /// PR / issue 号。
    number: u64,
    /// 种类。
    kind: TargetKind,
    /// 面向人的链接（从 URL 原文取，`gh` 的返回里也可能有）。
    url: String,
}

impl Target {
    /// `owner/repo`；不是 github.com 时带上主机，即 `gh --repo` 接受的 `[HOST/]OWNER/REPO`。
    fn slug(&self) -> String {
        if self.host.eq_ignore_ascii_case("github.com") {
            format!("{}/{}", self.owner, self.repo)
        } else {
            format!("{}/{}/{}", self.host, self.owner, self.repo)
        }
    }

    /// 人类可读的标识：`owner/repo#123`。
    fn label(&self) -> String {
        format!("{}/{}#{}", self.owner, self.repo, self.number)
    }

    /// 副本目录名：文件系统安全、可读、同目标重复评审会复用同一份。
    fn key(&self) -> String {
        let raw = format!("{}-{}-{}-{}", self.host, self.owner, self.repo, self.number);
        let mut key = String::with_capacity(raw.len());
        for ch in raw.chars() {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                key.push(ch);
            } else {
                key.push('-');
            }
        }
        if self.kind == TargetKind::Issue {
            key.push_str("-issue");
        }
        key
    }

    /// 副本目录 `agent_dir()/extensions/review/<key>/`。
    fn checkout_dir(&self) -> PathBuf {
        agent_dir().join(REVIEW_SUBDIR).join(self.key())
    }

    /// 评论去重用的 marker（同一目标 + 同一会话只发一条）。
    fn marker(&self, session: Option<&str>) -> String {
        let seed = format!("{}#{}|{}", self.slug(), self.number, session.unwrap_or("-"));
        let digest = Sha256::digest(seed.as_bytes());
        let mut hex = String::with_capacity(16);
        for byte in digest.iter().take(8) {
            let _ = write!(hex, "{byte:02x}");
        }
        format!("<!-- prux-review:v1:{hex} -->")
    }
}

/// 解析 `https://<host>/<owner>/<repo>/(pull|issues)/<n>` 与 `owner/repo#<n>`。
///
/// 认不出形状时报错（不猜）；`owner/repo#<n>` 一律当 github.com 上的拉取请求。
fn parse_target(raw: &str) -> Result<Target, String> {
    let text = raw.trim();
    if text.is_empty() {
        return Err("usage: /review <PR or issue URL> [--comment]".to_string());
    }

    if !text.contains("://") {
        let (slug, number) = text
            .rsplit_once('#')
            .ok_or_else(|| format!("not a PR / issue URL: {text}"))?;
        let (owner, repo) = slug
            .split_once('/')
            .ok_or_else(|| format!("not a PR / issue URL: {text}"))?;
        let number: u64 = number
            .trim()
            .parse()
            .map_err(|_| format!("not a PR number: {number}"))?;
        return Ok(Target {
            host: "github.com".to_string(),
            owner: owner.trim().to_string(),
            repo: repo.trim().to_string(),
            number,
            kind: TargetKind::Pull,
            url: format!(
                "https://github.com/{}/{}/pull/{number}",
                owner.trim(),
                repo.trim()
            ),
        });
    }

    let rest = text
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(text)
        .trim_start_matches("www.");
    let mut parts = rest.split('/');
    let host = parts
        .next()
        .filter(|h| !h.is_empty())
        .ok_or_else(|| format!("not a PR / issue URL: {text}"))?;
    let owner = parts
        .next()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| format!("not a PR / issue URL: {text}"))?;
    let repo = parts
        .next()
        .filter(|p| !p.is_empty())
        .ok_or_else(|| format!("not a PR / issue URL: {text}"))?;
    let kind = match parts.next() {
        Some("pull") => TargetKind::Pull,
        Some("issues") => TargetKind::Issue,
        _ => return Err(format!("not a PR / issue URL: {text}")),
    };
    let number: u64 = parts
        .next()
        .and_then(|n| n.parse().ok())
        .ok_or_else(|| format!("not a PR / issue URL: {text}"))?;

    Ok(Target {
        host: host.to_string(),
        owner: owner.to_string(),
        repo: repo.to_string(),
        number,
        kind,
        url: format!(
            "https://{host}/{owner}/{repo}/{}/{number}",
            kind.path_segment()
        ),
    })
}

/// 一次评审的运行态（每个会话一份）。
#[derive(Debug, Clone)]
struct Active {
    /// 目标。
    target: Target,
    /// 是否在结算时把最后一条回答作为评论发出去。
    comment: bool,
    /// 是否已经发过（同一次会话内不再发）。
    posted: bool,
    /// 命令发起时的工作目录：`gh` / `git` 从这里跑。
    cwd: PathBuf,
}

/// 扩展的进程级状态。
#[derive(Default)]
struct State {
    /// 当前会话的评审目标；`None` 表示这次会话还没评审过东西。
    active: Option<Active>,
    /// 当前会话 id（评论 marker 的一部分）。
    session_id: Option<String>,
}

impl State {
    /// 会话切换 / 会话开始时改记会话 id；**真的换会话**时清掉上一会话的目标（marker 也随会话变）。
    ///
    /// 两个钩子的标识都走 [`session_id_of`]（会话文件名 stem），否则同一会话在「刚有文件」
    /// 那一刻看起来就像另一个会话（一个给 id、一个给 stem），刚设的目标当场就没了。
    fn reset_session(&mut self, session_id: Option<String>) {
        // `None → Some` 是「同一个新会话刚有文件」，`同值` 是同一会话又报了一次：都不是换会话。
        if let Some(id) = &session_id
            && self.session_id.as_deref().is_none_or(|old| old == id)
        {
            self.session_id = session_id;
            return;
        }
        self.session_id = session_id;
        self.active = None;
    }
}

/// 扩展状态的进程级单例。
fn state() -> &'static Mutex<State> {
    /// 扩展状态的进程级单例。
    static S: OnceLock<Mutex<State>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(State::default()))
}

/// 取状态锁；锁被毒化时取回内部值。
fn lock() -> MutexGuard<'static, State> {
    state().lock().unwrap_or_else(|e| e.into_inner())
}

/// `review` 扩展。
pub struct Review;

impl Extension for Review {
    /// 扩展名 `review`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Review an existing PR or issue in place: fetch it read-only, brief the agent, optionally comment back."
    }

    /// 默认关闭：会建副本、发网络请求，`--comment` 还会对外写评论。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 只在 `Dev` 扩展模式下可用：`--comment` 会往别人的 PR / issue 上写一条公开评论。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev]
    }

    /// 不给模型加工具：评审靠常规 `read` / `grep` / `bash`，回帖由扩展在结算时自己做。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 提供 `/review` 命令。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "/review <PR or issue URL> [--comment] — fetch it read-only and review it"
                .to_string(),
            busy_safe: false, // 会发起一轮 prompt，忙碌时按普通文本入队
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 订阅结算边界（`--comment` 时在那里回帖）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::Boundary]
    }

    /// 接线 `/review` 的 handler。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_review);
    }

    /// 会话开始：拾录会话 id（未落盘的会话此刻还没有文件，交由首次落盘时的切换钩子补记）。
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], session: Option<&Session>) {
        let id = session.and_then(|s| session_id_of(s.get_session_file().and_then(|p| p.to_str())));
        lock().reset_session(id);
    }

    /// 会话切换：改记新会话 id 并清掉上一会话的目标。
    fn on_session_switched(&self, session_path: Option<&str>, _messages: &[AgentMessage]) {
        lock().reset_session(session_id_of(session_path));
    }

    /// 关闭本扩展时丢掉当前目标：留着它的话，重新启用后下一次结算会把一份旧回答贴上去。
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            lock().active = None;
        }
    }

    /// 结算边界：`--comment` 且还没发过时，把最后一条 assistant 回答作为评论投出去。
    ///
    /// 网络请求扔到独立线程，边界本身只取文本 + 记账：`on_boundary` 跑在 agent 的异步
    /// 执行路径上，同步 `gh` 调用会把整轮结算卡住几秒。
    fn on_boundary(&self, event: &Value) -> Option<BoundaryOutcome> {
        if event.get("type").and_then(Value::as_str) != Some("agent_before_settle") {
            return None;
        }
        if event.get("agentScope").and_then(Value::as_str) != Some("main") {
            return None;
        }

        let (active, session) = {
            let st = lock();
            let active = st.active.clone().filter(|a| a.comment && !a.posted)?;
            (active, st.session_id.clone())
        };

        let report = last_assistant_text(event)?;

        {
            let mut st = lock();
            if let Some(current) = st.active.as_mut()
                && current.target == active.target
            {
                current.posted = true;
            }
        }

        std::thread::spawn(move || {
            let outcome = publish(&active, session.as_deref(), &report);
            notify_text(&format!("review: {outcome}"), UiNotifyLevel::Info);
        });

        None
    }
}

/// 从边界事件里取最后一条 assistant 消息的正文。
///
/// `None` 表示这一轮没有可发的回答（空回答、或被中断）——那就什么都不发。
fn last_assistant_text(event: &Value) -> Option<String> {
    let values = event.get("contextMessages")?.as_array()?;
    let text = values
        .iter()
        .rev()
        .filter_map(|value| serde_json::from_value::<AgentMessage>(value.clone()).ok())
        .find(|message| message.role == "assistant")
        .map(|message| message.text())?;
    let text = text.trim();
    (!text.is_empty()).then(|| truncate_chars(text, COMMENT_MAX_CHARS, None, true))
}

/// `/review` 命令入口。返回值恒为 `false`（不退出 TUI）。
fn command_review(host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (comment, rest) = split_comment_flag(arg);
    let rest = rest.trim();

    match rest {
        "" if comment => {
            host.push_message(
                "review: --comment needs a target — /review --comment <PR or issue URL>",
                UiNotifyLevel::Warning,
            );
        }
        "" | "status" => host.push_message(&status_text(), UiNotifyLevel::Info),
        "clean" => host.push_message(&clean_checkouts(), UiNotifyLevel::Info),
        other => match parse_target(other) {
            Ok(target) => start(host, target, comment),
            Err(err) => host.push_message(&format!("review: {err}"), UiNotifyLevel::Warning),
        },
    }
    false
}

/// 把参数里的 `--comment` 摘出来，返回 `(是否要回帖, 剩余参数)`。
///
/// 只认这一个开关；其它以 `-` 开头的参数原样留在剩余参数里，交由 [`parse_target`] 报错。
fn split_comment_flag(arg: &str) -> (bool, String) {
    let mut comment = false;
    let mut rest = String::new();
    for word in arg.split_whitespace() {
        if word == "--comment" {
            comment = true;
        } else {
            if !rest.is_empty() {
                rest.push(' ');
            }
            rest.push_str(word);
        }
    }
    (comment, rest)
}

/// `/review <url>`：解析 → 记住目标 → 后台取元数据与副本 → 简报交给 agent。
///
/// `gh` / `git` 都是网络操作，放独立线程跑；成功后经 [`crate::core::extensions::ExtensionUiRequest::Continuation`]
/// 把简报作为**用户消息**投出去（空闲时起一轮，忙碌时让位——不抢跑用户输入）。
fn start(host: &mut dyn CommandHost, target: Target, comment: bool) {
    let cwd = host.cwd();
    lock().active = Some(Active {
        target: target.clone(),
        comment,
        posted: false,
        cwd: cwd.clone(),
    });

    let what = if comment {
        "fetching it, the review will be posted as a comment when it settles"
    } else {
        "fetching it read-only"
    };
    host.push_message(
        &format!("review: {} ({what})", target.label()),
        UiNotifyLevel::Info,
    );

    std::thread::spawn(move || {
        match fetch(&cwd, &target) {
            Ok(fetched) => {
                let brief = brief(&target, &fetched, comment);
                request_ui(ExtensionUiRequest::Continuation {
                    message: Box::new(AgentMessage::user_text(&brief)),
                });
            }
            Err(err) => {
                // 取不到就别挂着一个「等着回帖」的目标：下次结算会把无关回答贴到 PR 上。
                let mut st = lock();
                if st.active.as_ref().map(|a| &a.target) == Some(&target) {
                    st.active = None;
                }
                drop(st);
                notify_text(
                    &format!("review: {} — {err}", target.label()),
                    UiNotifyLevel::Warning,
                );
            }
        }
    });
}

/// 取一个目标的全部离线材料：元数据 + 只读副本 + diff。
#[derive(Debug, Default)]
struct Fetched {
    /// 标题。
    title: String,
    /// 发起人登录名。
    author: String,
    /// PR / issue 正文。
    body: String,
    /// 状态（`OPEN` / `CLOSED` / `MERGED`）。
    state: String,
    /// base 分支名（issue 为仓库默认分支）。
    base: String,
    /// head 分支名（issue 时为空）。
    head: String,
    /// head 提交（issue 时为空）。
    head_oid: String,
    /// 改动文件 `路径 (+增 −删)`。
    files: Vec<String>,
    /// 增删统计。
    stats: String,
    /// GitHub 给的三点 diff（PR 才有；issue 为空）。
    diff: String,
    /// 副本目录。
    checkout: PathBuf,
}

/// 取元数据 → 建副本 → 落 diff。任一步失败即整体失败（不带着半份材料去评审）。
fn fetch(cwd: &Path, target: &Target) -> Result<Fetched, String> {
    let meta = gh_json(cwd, target)?;
    let mut fetched = Fetched {
        title: field(&meta, "title"),
        author: meta
            .get("author")
            .and_then(|a| a.get("login"))
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        body: field(&meta, "body"),
        state: field(&meta, "state"),
        base: field(&meta, "baseRefName"),
        head: field(&meta, "headRefName"),
        head_oid: field(&meta, "headRefOid"),
        files: meta
            .get("files")
            .and_then(Value::as_array)
            .map(|files| {
                files
                    .iter()
                    .take(PROMPT_FILE_LINES)
                    .map(|file| {
                        let path = file.get("path").and_then(Value::as_str).unwrap_or("?");
                        let add = file.get("additions").and_then(Value::as_u64).unwrap_or(0);
                        let del = file.get("deletions").and_then(Value::as_u64).unwrap_or(0);
                        format!("{path} (+{add} −{del})")
                    })
                    .collect()
            })
            .unwrap_or_default(),
        stats: match (
            meta.get("additions").and_then(Value::as_u64),
            meta.get("deletions").and_then(Value::as_u64),
        ) {
            (Some(add), Some(del)) => format!("+{add} −{del}"),
            _ => String::new(),
        },
        ..Fetched::default()
    };

    if target.kind == TargetKind::Issue && fetched.base.is_empty() {
        fetched.base = default_branch(cwd, target)?;
    }

    fetched.checkout = checkout(target, &fetched)?;

    if target.kind == TargetKind::Pull {
        let out = git::run(
            cwd,
            "gh",
            &[
                "pr",
                "diff",
                &target.number.to_string(),
                "--repo",
                &target.slug(),
            ],
            GH_TIMEOUT,
        )?;
        if !out.ok {
            return Err(out.failure("gh pr diff"));
        }
        fetched.diff = out.stdout;
        if fetched.diff.len() <= DIFF_MAX_BYTES {
            let path = fetched.checkout.join("review.diff");
            let _ = std::fs::write(&path, &fetched.diff);
        }
    }

    Ok(fetched)
}

/// 读目标元数据（`gh pr|issue view --json …`）。
fn gh_json(cwd: &Path, target: &Target) -> Result<Value, String> {
    let number = target.number.to_string();
    let slug = target.slug();
    let mut args: Vec<&str> = vec![target.kind.gh_command(), "view", &number, "--repo", &slug];
    let fields = if target.kind == TargetKind::Pull {
        "number,url,title,body,state,author,baseRefName,headRefName,headRefOid,files,additions,deletions"
    } else {
        "number,url,title,body,state,author"
    };
    args.push("--json");
    args.push(fields);

    let out = git::run(cwd, "gh", &args, GH_TIMEOUT)?;
    if !out.ok {
        return Err(out.failure("gh view"));
    }
    serde_json::from_str(&out.stdout).map_err(|err| format!("gh view: malformed JSON: {err}"))
}

/// 仓库默认分支（`gh repo view`）；issue 评审要它来决定副本停在哪儿。
fn default_branch(cwd: &Path, target: &Target) -> Result<String, String> {
    let out = git::run(
        cwd,
        "gh",
        &[
            "repo",
            "view",
            &target.slug(),
            "--json",
            "defaultBranchRef",
            "--jq",
            ".defaultBranchRef.name",
        ],
        GH_TIMEOUT,
    )?;
    if !out.ok {
        return Err(out.failure("gh repo view"));
    }
    let name = out.stdout.trim();
    if name.is_empty() {
        return Err("gh repo view returned no default branch".to_string());
    }
    Ok(name.to_string())
}

/// 在 `agent_dir()` 下建只读副本，检出 PR head（issue 则检出默认分支）。
///
/// 只用 `git init` + `git fetch --depth 1 <refspec>:refs/review/head` + `checkout --detach`：
/// 不建 worktree（那要写用户仓库的 `.git/worktrees`）、不碰远端引用、不切换用户的分支。
fn checkout(target: &Target, fetched: &Fetched) -> Result<PathBuf, String> {
    let dir = target.checkout_dir();
    let url = format!(
        "https://{}/{}/{}.git",
        target.host, target.owner, target.repo
    );
    let refspec = match target.kind {
        TargetKind::Pull => format!("refs/pull/{}/head", target.number),
        TargetKind::Issue => format!("refs/heads/{}", fetched.base),
    };
    prepare_checkout(&dir, &url, &refspec)?;
    Ok(dir)
}

/// 把 `dir` 变成停在 `refspec` 上的一次性副本（幂等：同一目录重复调用只是重新 fetch）。
///
/// `url` 单独传，测试才能指向本地仓库而不碰网络。
fn prepare_checkout(dir: &Path, url: &str, refspec: &str) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|err| format!("cannot create {}: {err}", dir.display()))?;

    if !dir.join(".git").exists() {
        run_git(dir, &["init", "--quiet"])?;
    }
    let remotes = run_git(dir, &["remote"])?;
    if !remotes.lines().any(|line| line.trim() == "origin") {
        run_git(dir, &["remote", "add", "origin", url])?;
    }

    let auth = git_http_auth(dir, url);
    if let Err(err) = run_git_env(
        dir,
        &[
            "fetch",
            "--quiet",
            "--depth",
            "1",
            "--no-tags",
            "origin",
            &format!("{refspec}:refs/review/head"),
        ],
        &auth,
    ) {
        return Err(if auth.is_empty() {
            format!(
                "{err}\nhint: private repository? run `gh auth login` — its token is reused for git"
            )
        } else {
            err
        });
    }
    run_git(
        dir,
        &[
            "checkout",
            "--quiet",
            "--force",
            "--detach",
            "refs/review/head",
        ],
    )?;
    Ok(())
}

/// 跑一条 `git`（在副本目录里，超时与输出全部走管道）。
fn run_git(dir: &Path, args: &[&str]) -> Result<String, String> {
    run_git_env(dir, args, &[])
}

/// 同 [`run_git`]，追加一组环境变量（HTTPS 认证走这里，不进 argv）。
fn run_git_env(dir: &Path, args: &[&str], envs: &[(&str, String)]) -> Result<String, String> {
    let out = git::run_env(dir, "git", args, envs, GIT_TIMEOUT)?;
    if !out.ok {
        return Err(out.failure(&format!("git {}", args.first().copied().unwrap_or(""))));
    }
    Ok(out.stdout)
}

/// 借 `gh` 的 token 给 `url` 所在主机配一条 git HTTPS 认证（`http.<base>.extraheader`）。
///
/// 私有仓库用 https 拉取时 git 要问用户名密码，而 TUI 里问不了；`gh` 既然能读私有仓库的元数据，
/// 就直接用它的 token。取不到（本地路径 / `gh` 未装未登录）时返回空，回到用户自己的凭证配置。
fn git_http_auth(dir: &Path, url: &str) -> Vec<(&'static str, String)> {
    let host = url
        .strip_prefix("https://")
        .and_then(|rest| rest.split('/').next())
        .filter(|host| !host.is_empty());
    let Some(host) = host else {
        return Vec::new();
    };
    let Ok(out) = git::run(
        dir,
        "gh",
        &["auth", "token", "--hostname", host],
        GH_TIMEOUT,
    ) else {
        return Vec::new();
    };
    let token = out.stdout.trim();
    if !out.ok || token.is_empty() {
        return Vec::new();
    }
    let basic = base64::Engine::encode(
        &base64::engine::general_purpose::STANDARD,
        format!("x-access-token:{token}"),
    );
    vec![
        ("GIT_CONFIG_COUNT", "1".to_string()),
        (
            "GIT_CONFIG_KEY_0",
            format!("http.https://{host}/.extraheader"),
        ),
        (
            "GIT_CONFIG_VALUE_0",
            format!("AUTHORIZATION: basic {basic}"),
        ),
    ]
}

/// 取 JSON 字符串字段（缺失 / 非字符串 → 空串）。
fn field(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// 拼交给 agent 的评审简报。
fn brief(target: &Target, fetched: &Fetched, comment: bool) -> String {
    let mut text = String::new();
    let noun = match target.kind {
        TargetKind::Pull => "PR",
        TargetKind::Issue => "issue",
    };
    _ = writeln!(
        text,
        "Review {noun} {} \"{}\" by {} — {} ({})",
        target.label(),
        fetched.title.trim(),
        fetched.author,
        target.url,
        if fetched.state.is_empty() {
            "unknown state"
        } else {
            &fetched.state
        }
    );
    _ = writeln!(text, "\nRead-only checkout: {}", fetched.checkout.display());

    if target.kind == TargetKind::Pull {
        _ = writeln!(
            text,
            "Diff (GitHub's own, {}): {}",
            if fetched.stats.is_empty() {
                "base…head".to_string()
            } else {
                fetched.stats.clone()
            },
            fetched.checkout.join("review.diff").display()
        );
        let _ = writeln!(text, "Base: {}  Head: {}", fetched.base, fetched.head);
        if !fetched.head_oid.is_empty() {
            _ = writeln!(text, "Head commit: {}", fetched.head_oid);
        }
        if !fetched.files.is_empty() {
            _ = writeln!(text, "\nFiles changed:\n- {}", fetched.files.join("\n- "));
        }
        if !fetched.diff.is_empty() {
            let diff = truncate_chars(&fetched.diff, PROMPT_DIFF_CHARS, Some(DEF_ELLIPSIS), true);
            _ = writeln!(text, "\nDiff:\n```diff\n{diff}\n```");
        } else {
            _ = writeln!(
                text,
                "\n(The diff could not be fetched; read it in the checkout with `git diff` after \
                 fetching the base branch, or ask for it explicitly.)"
            );
        }
        _ = writeln!(
            text,
            "\nHow to review:\n\
             1. Read the diff above first.\n\
             2. When the diff is not enough (callers, tests, invariants), read the real files \
             under {} — absolute paths. That copy exists only for you; never edit it.\n\
             3. Report findings in priority order. Each one: `file:line`, what actually breaks, \
             and the smallest fix. Call out missing tests and edge cases that would bite in \
             production. If the change is fine, say so plainly — do not invent findings.\n\
             4. Judge correctness, resource and lock discipline, error handling, security and \
             test coverage. Skip style nits.\n\
             Do not modify files, do not commit, do not push, do not touch the user's own checkout.",
            fetched.checkout.display()
        );
    } else {
        _ = writeln!(
            text,
            "Repository checkout (default branch {}): {}",
            fetched.base,
            fetched.checkout.display()
        );
        if !fetched.body.trim().is_empty() {
            let body = truncate_chars(&fetched.body, PROMPT_BODY_CHARS, Some(DEF_ELLIPSIS), true);
            _ = writeln!(text, "\nIssue body:\n{body}");
        }
        _ = writeln!(
            text,
            "\nHow to review:\n\
             1. Decide whether the issue is real: find the code it talks about under {} and read \
             enough of it to tell. Report `file:line` for every claim.\n\
             2. Say what is actually wrong (or that it is not reproducible / already fixed / a \
             misunderstanding), then the smallest fix in outline.\n\
             3. Call out missing reproductions and tests.\n\
             Do not modify files, do not commit, do not push.",
            fetched.checkout.display()
        );
    }

    if comment {
        _ = writeln!(
            text,
            "\nYour final message is posted verbatim as a comment on {} once this turn settles, so \
             write it for that audience: findings only, no narration of your own process, no \
             \"I read the diff first\".",
            target.url
        );
    }

    text
}

/// 结算时投递评论：先按 marker 查重，再发，再复核落地。
fn publish(active: &Active, session: Option<&str>, report: &str) -> String {
    let target = &active.target;
    let marker = target.marker(session);
    let body = format!("{marker}\n{report}");

    match find_marker(&active.cwd, target, &marker) {
        Ok(Some(url)) => {
            record(target, &marker, report, "reused", &url);
            return format!("already commented on {} — reusing {url}", target.label());
        }
        Ok(None) => {}
        Err(err) => return format!("cannot read existing comments ({err}); nothing was posted"),
    }

    match post_comment(&active.cwd, target, &body) {
        Err(err) => format!(
            "comment on {} failed: {err} — nothing was posted",
            target.label()
        ),
        Ok(url) => match find_marker(&active.cwd, target, &marker) {
            Ok(Some(verified)) => {
                record(target, &marker, report, "confirmed", &verified);
                format!("commented on {} — {verified}", target.label())
            }
            Ok(None) => {
                record(target, &marker, report, "unconfirmed", &url);
                format!(
                    "comment on {} returned {url} but a re-read could not find it — check it \
                     manually; it will not be posted again",
                    target.label()
                )
            }
            Err(err) => {
                record(target, &marker, report, "unconfirmed", &url);
                format!(
                    "comment on {} posted at {url} but verification failed ({err}) — check it \
                     manually; it will not be posted again",
                    target.label()
                )
            }
        },
    }
}

/// 按 marker 在已有评论里查一条；没有则 `Ok(None)`。
fn find_marker(cwd: &Path, target: &Target, marker: &str) -> Result<Option<String>, String> {
    let number = target.number.to_string();
    let filter = format!(".comments[] | select(.body | contains(\"{marker}\")) | (.url // .id)");
    let out = git::run(
        cwd,
        "gh",
        &[
            target.kind.gh_command(),
            "view",
            &number,
            "--repo",
            &target.slug(),
            "--json",
            "comments",
            "--jq",
            &filter,
        ],
        GH_TIMEOUT,
    )?;
    if !out.ok {
        return Err(out.failure("gh view --json comments"));
    }
    Ok(out
        .stdout
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(str::to_string))
}

/// 发一条评论，返回 `gh` 报出的链接。
fn post_comment(cwd: &Path, target: &Target, body: &str) -> Result<String, String> {
    let number = target.number.to_string();
    let out = git::run(
        cwd,
        "gh",
        &[
            "issue",
            "comment",
            &number,
            "--repo",
            &target.slug(),
            "--body",
            body,
        ],
        GH_TIMEOUT,
    )?;
    if !out.ok {
        return Err(out.failure("gh issue comment"));
    }
    Ok(out.stdout.trim().to_string())
}

/// 已发布评论的记录（一份文件，按目标 key 覆盖）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Publication {
    /// 目标 `owner/repo#n`。
    target: String,
    /// 目标链接。
    url: String,
    /// 去重 marker。
    marker: String,
    /// 结论：`confirmed` / `unconfirmed` / `reused`。
    status: String,
    /// `gh` 报出（或复核找到）的评论链接。
    comment: String,
    /// 发出时间（UTC RFC3339）。
    at: String,
    /// 正文摘要（首行，便于 `/review status` 分辨是哪一次评审）。
    summary: String,
}

/// 记一笔发布结果（写文件失败不影响评审本身，只在状态里查不到）。
fn record(target: &Target, marker: &str, report: &str, status: &str, comment: &str) {
    let path = agent_dir().join(PUBLICATION_FILE);
    let mut stored = read_publications(&path);
    let summary = truncate_chars(
        report.lines().next().unwrap_or("").trim(),
        120,
        Some(DEF_ELLIPSIS),
        true,
    );
    stored.insert(
        target.key(),
        Publication {
            target: target.label(),
            url: target.url.clone(),
            marker: marker.to_string(),
            status: status.to_string(),
            comment: comment.to_string(),
            at: now_iso(),
            summary,
        },
    );
    if let Some(parent) = path.parent() {
        _ = std::fs::create_dir_all(parent);
    }
    if let Ok(text) = serde_json::to_string_pretty(&stored) {
        _ = std::fs::write(&path, text);
    }
}

/// 读已发布评论记录的映射（文件缺失 / 损坏 → 空）。
fn read_publications(path: &Path) -> BTreeMap<String, Publication> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// `/review status` 的正文。
fn status_text() -> String {
    let (active, session) = {
        let st = lock();
        (st.active.clone(), st.session_id.clone())
    };

    let mut lines: Vec<String> = Vec::new();
    match &active {
        Some(active) => {
            let target = &active.target;
            lines.push(format!("Target: {} ({})", target.label(), target.url));
            lines.push(format!("Checkout: {}", target.checkout_dir().display()));
            lines.push(format!(
                "Comment back: {}",
                match (active.comment, active.posted) {
                    (false, _) => "off (pass --comment)".to_string(),
                    (true, false) => "on — posts when this turn settles".to_string(),
                    (true, true) => "on — already posted this session".to_string(),
                }
            ));
            lines.push(format!("Marker: {}", target.marker(session.as_deref())));
        }
        None => lines.push(
            "No review this session. Usage: /review <PR or issue URL> [--comment]".to_string(),
        ),
    }

    let stored = read_publications(&agent_dir().join(PUBLICATION_FILE));
    if !stored.is_empty() {
        lines.push("\nPublished comments:".to_string());
        for publication in stored.values() {
            lines.push(format!(
                "- {} [{}] {} {}\n  {} — {}",
                publication.target,
                publication.status,
                publication.at,
                publication.comment,
                publication.url,
                publication.summary
            ));
        }
    }
    lines.join("\n")
}

/// `/review clean`：删掉 `agent_dir()/extensions/review/` 下的全部副本，保留记录文件。
fn clean_checkouts() -> String {
    let root = agent_dir().join(REVIEW_SUBDIR);
    let entries = match std::fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(_) => return format!("review: nothing to clean ({})", root.display()),
    };

    let mut removed = 0usize;
    let mut failed: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if !path.is_dir() || name.starts_with('.') {
            continue;
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => removed += 1,
            Err(err) => failed.push(format!("{name}: {err}")),
        }
    }

    let mut text = format!(
        "review: removed {removed} checkout(s) from {}",
        root.display()
    );
    if !failed.is_empty() {
        let _ = write!(text, "\nfailed:\n- {}", failed.join("\n- "));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pull_and_issue_urls() {
        let pull = parse_target("https://github.com/acme/widget/pull/42").unwrap();
        assert_eq!(pull.owner, "acme");
        assert_eq!(pull.repo, "widget");
        assert_eq!(pull.number, 42);
        assert_eq!(pull.kind, TargetKind::Pull);
        assert_eq!(pull.slug(), "acme/widget");
        assert_eq!(pull.label(), "acme/widget#42");
        assert_eq!(pull.url, "https://github.com/acme/widget/pull/42");

        let issue = parse_target("https://github.com/acme/widget/issues/7/files").unwrap();
        assert_eq!(issue.kind, TargetKind::Issue);
        assert_eq!(issue.number, 7);
        assert_eq!(issue.url, "https://github.com/acme/widget/issues/7");
    }

    #[test]
    fn parses_short_form_and_ghes_hosts() {
        let short = parse_target("acme/widget#3").unwrap();
        assert_eq!(short.number, 3);
        assert_eq!(short.kind, TargetKind::Pull, "短形式按 PR 处理");

        let ghes = parse_target("https://ghe.example.com/acme/widget/pull/9").unwrap();
        assert_eq!(
            ghes.slug(),
            "ghe.example.com/acme/widget",
            "非 github.com 带主机"
        );
        assert!(ghes.key().contains("ghe-example-com"), "{}", ghes.key());
    }

    #[test]
    fn rejects_shapes_it_cannot_read() {
        for raw in [
            "",
            "notes",
            "https://github.com/acme/widget",
            "acme/widget#x",
        ] {
            assert!(parse_target(raw).is_err(), "{raw} 不该被当成目标");
        }
    }

    #[test]
    fn extension_is_dev_only_off_by_default_and_declares_no_tools() {
        let ext = Review;
        assert_eq!(ext.name(), "review");
        assert!(
            !ext.default_enabled(),
            "会建副本、发网络请求、可能对外写评论"
        );
        assert!(ext.tools().is_empty(), "不给模型加工具");
        assert!(ext.hooks().contains(&ExtensionHook::Boundary));
        assert_eq!(ext.commands().len(), 1);
        assert!(!ext.commands()[0].busy_safe, "会发起一轮 prompt");
        assert_eq!(
            ext.modes(),
            vec![ExtensionMode::Dev],
            "会对外写评论，只给 Dev 模式（与 pr 同口径）"
        );
    }

    #[test]
    fn comment_flag_is_split_out() {
        assert_eq!(
            split_comment_flag("--comment https://github.com/a/b/pull/1"),
            (true, "https://github.com/a/b/pull/1".to_string())
        );
        assert_eq!(
            split_comment_flag("https://github.com/a/b/pull/1"),
            (false, "https://github.com/a/b/pull/1".to_string())
        );
        assert_eq!(split_comment_flag(""), (false, String::new()));
    }

    #[test]
    fn marker_is_stable_per_target_and_session_and_differs_across_sessions() {
        let target = parse_target("acme/widget#5").unwrap();
        let first = target.marker(Some("s-1"));
        assert_eq!(first, target.marker(Some("s-1")), "同目标同会话稳定");
        assert_ne!(first, target.marker(Some("s-2")), "换会话换 marker");
        assert!(first.starts_with("<!-- prux-review:v1:"), "{first}");
        assert!(first.ends_with(" -->"), "{first}");
        assert!(!first.contains('"'), "marker 要能安全塞进 jq 过滤器");
    }

    #[test]
    fn checkout_keys_are_filesystem_safe_and_keep_kinds_apart() {
        let pull = parse_target("https://github.com/acme/widget/pull/5").unwrap();
        let issue = parse_target("https://github.com/acme/widget/issues/5").unwrap();
        assert!(
            pull.key()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
            "{}",
            pull.key()
        );
        assert_ne!(pull.key(), issue.key(), "同号不同类不能共用副本目录");
    }

    #[test]
    fn brief_carries_the_checkout_path_and_the_comment_audience() {
        let target = parse_target("acme/widget#5").unwrap();
        let fetched = Fetched {
            title: "fix the widget".to_string(),
            author: "octo".to_string(),
            state: "OPEN".to_string(),
            base: "main".to_string(),
            head: "topic".to_string(),
            diff: "--- a\n+++ b\n".to_string(),
            stats: "+2 −1".to_string(),
            files: vec!["src/lib.rs (+2 −1)".to_string()],
            checkout: PathBuf::from("/tmp/review-checkout"),
            ..Fetched::default()
        };

        let text = brief(&target, &fetched, false);
        assert!(text.contains("/tmp/review-checkout"), "{text}");
        assert!(text.contains("src/lib.rs (+2 −1)"), "{text}");
        assert!(text.contains("Base: main  Head: topic"), "{text}");
        assert!(
            !text.contains("posted verbatim"),
            "不带 --comment 时不该承诺回帖: {text}"
        );

        let with_comment = brief(&target, &fetched, true);
        assert!(
            with_comment.contains("posted verbatim as a comment"),
            "{with_comment}"
        );
    }

    /// 造一个带提交的临时仓库；`git` 不可用时返回 `None`（与 `utils::git` 的测试同口径）。
    fn temp_repo() -> Option<PathBuf> {
        /// 进程内自增，避免并行测试撞同一目录。
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "prux-review-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).ok()?;
        let run = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|status| status.success())
                .unwrap_or(false)
        };
        if !run(&["init", "-b", "main"]) {
            return None;
        }
        for (key, value) in [("user.email", "t@example.com"), ("user.name", "prux test")] {
            run(&["config", key, value]);
        }
        std::fs::write(dir.join("README.md"), "hello\n").ok()?;
        if !run(&["add", "-A"]) || !run(&["commit", "-m", "init"]) {
            return None;
        }
        Some(dir)
    }

    #[test]
    fn review_target_survives_the_session_gaining_its_file() {
        let target = parse_target("https://github.com/o/r/pull/1").unwrap();
        let active = || Active {
            target: target.clone(),
            comment: false,
            posted: false,
            cwd: PathBuf::from("/tmp"),
        };

        // 新会话：还没落盘（session_file 为空），/review 先设了目标
        let session =
            Session::create_deferred("/tmp", Some(std::env::temp_dir().join("prux-review-x")))
                .unwrap();
        Review.on_session_start("/tmp", &[], Some(&session));
        lock().active = Some(active());

        // 简报落盘：路径此刻才有，仍是同一会话
        Review.on_session_switched(Some("/tmp/2026-11-13T07-08-48-995Z_abc.jsonl"), &[]);
        assert!(lock().active.is_some(), "首次落盘不能丢掉当前目标");

        // 真的切到另一个会话才丢
        Review.on_session_switched(Some("/tmp/2026-11-13T08-00-00-000Z_def.jsonl"), &[]);
        assert!(lock().active.is_none());

        // 已有文件的会话：再报一次同一会话不算切
        lock().active = Some(active());
        Review.on_session_switched(Some("/tmp/2026-11-13T08-00-00-000Z_def.jsonl"), &[]);
        assert!(lock().active.is_some(), "同一会话重复上报不能丢掉目标");

        lock().active = Some(active());
        Review.on_session_switched(None, &[]); // /new：还没有新会话文件
        assert!(lock().active.is_none());
        lock().active = None;
    }

    #[test]
    fn git_http_auth_ignores_local_paths() {
        assert!(git_http_auth(Path::new("."), "/tmp/some/repo").is_empty());
    }

    #[test]
    fn prepare_checkout_creates_a_detached_copy() {
        let Some(source) = temp_repo() else {
            return; // 没有 git 的环境跳过
        };
        let dest = source.join("copy");

        prepare_checkout(&dest, &source.to_string_lossy(), "refs/heads/main").unwrap();
        assert!(dest.join("README.md").exists(), "工作树被检出");
        assert_eq!(
            run_git(&dest, &["rev-parse", "--abbrev-ref", "HEAD"])
                .unwrap()
                .trim(),
            "HEAD",
            "停在分离 HEAD 上，不指向任何分支"
        );

        // 幂等：同一目录再跑一次不报错（副本会被复用）
        prepare_checkout(&dest, &source.to_string_lossy(), "refs/heads/main").unwrap();
        assert_eq!(
            run_git(&dest, &["remote"]).unwrap().trim(),
            "origin",
            "重复调用不会重复添加 origin"
        );
    }

    #[test]
    fn last_assistant_text_skips_non_assistant_tail() {
        let event = serde_json::json!({
            "contextMessages": [
                {"role": "user", "content": [{"type": "text", "text": "review it"}]},
                {"role": "assistant", "content": [{"type": "text", "text": "  findings here  "}]}
            ]
        });
        assert_eq!(
            last_assistant_text(&event).as_deref(),
            Some("findings here")
        );
        assert_eq!(
            last_assistant_text(&serde_json::json!({"contextMessages": []})),
            None
        );
    }
}

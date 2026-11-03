//! `worktree` 扩展：把当前会话搬进一份 git worktree 副本里干活。
//!
//! 与 `subagent` 的 `isolation: worktree` 分工不同：那个是给**子代理**的一次性副本
//! （跑完删掉，有改动就提交成分支），本扩展管的是**主会话**——用户自己决定「接下来这段活
//! 要在隔离副本里做」，会话历史一直跟着走，退出时把副本里的改动提交成一个分支留在主仓库。
//!
//! 三条设计约束：
//!
//! 1. **只走命令，不做成工具**：工具执行期 agent 被 worker 独占，拿不到改 `cwd` 的能力；
//!    而「换个目录干活」本来就该是用户的选择，不该让模型自己偷偷搬家。
//! 2. **提交失败就不删副本**：`git commit` 缺 `user.email` 之类的失败必须如实说，并把副本
//!    留在原地——先删后报等于把用户这半天的活丢了。
//! 3. **副本是 `HEAD` 的快照**：工作目录里未提交的改动**不会**跟着过去（它们在原仓库里，
//!    一个字节没动）。进副本前会把这件事写在提示里。
//!
//! 副本落在 `agent_dir()/extensions/worktrees/<仓库名>-<名字>`：不污染用户项目目录，也不会被系统清
//! 临时目录时带走。所有 git 调用都是同步的（命令 handler 跑在 UI 线程上），副本几十 GB 的
//! 仓库 `git worktree add` 会卡住界面几秒——这是有意换来的简单：异步建副本没法把结果
//! 交回 UI 线程切目录。
//!
//! **默认不启用**，声明 `Dev` / `Creator` 两个并排模式（`Minimal` 下不可用）。

use crate::{
    core::{
        extensions::{
            CommandHost, Extension, ExtensionCommand, ExtensionMode, ExtensionTool, SubcommandDef,
            UiNotifyLevel,
        },
        settings_manager,
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_WORKTREE, command_arg},
    modes::tui::handlers::register_host_command,
    utils::{paths::short_path, time::now_ms},
};
use std::{
    path::{Path, PathBuf},
    process::Stdio,
    sync::Mutex,
};

/// 扩展名（注册名，也是 `/extension` 面板里的名字）。
const EXT: &str = "worktree";

/// 斜杠命令名（`/worktree`）。
const CMD: &str = "worktree";

/// 退出时给改动建的分支名前缀（完整名 = 前缀 + 副本名）。
const BRANCH_PREFIX: &str = concat!(env!("CARGO_PKG_NAME"), "-wt");

/// `/worktree` 子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "enter",
        description: "Create a git worktree and move this session into it: /worktree enter [name]",
    },
    SubcommandDef {
        name: "exit",
        description: "Commit the work to a branch, delete the copy, go back: /worktree exit",
    },
    SubcommandDef {
        name: "drop",
        description: "Delete the copy and throw its changes away: /worktree drop",
    },
];

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static WORKTREE_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_WORKTREE,
    make: || -> std::sync::Arc<dyn Extension> { std::sync::Arc::new(Worktree) },
};

/// 当前活动的一次副本。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Active {
    /// 进入副本前的工作目录（退出时切回这里；可能是仓库的子目录）。
    original_cwd: String,
    /// 副本根目录（`git worktree remove` 的目标）。
    path: String,
    /// 副本名（用户给的或时间戳生成），用于分支名。
    name: String,
}

/// 活动副本状态。`None` = 当前会话在主工作树里。
static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

/// `worktree` 扩展。
pub struct Worktree;

impl Extension for Worktree {
    /// 返回扩展标识常量 `EXT`（`"worktree"`）。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的说明文案。
    fn description(&self) -> &str {
        "Move this session into a git worktree copy: /worktree enter [name] creates the copy and \
         switches the working directory into it, /worktree exit commits the work to a branch, \
         deletes the copy and goes back, /worktree drop deletes it and throws the changes away. \
         The copy is a snapshot of HEAD — uncommitted changes stay in the original directory."
    }

    /// 声明 `Dev` / `Creator` 两个并排模式：`Minimal` 下不可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认不启用：换工作目录是用户显式选择的行为。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供工具：改会话目录只能由用户下命令（工具执行期拿不到 `cwd` 的写能力）。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/worktree`（`enter` / `exit` / `drop`）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Work inside a git worktree copy: /worktree [enter <name> | exit | drop]"
                .to_string(),
            busy_safe: true,
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 接线 `/worktree` 执行入口。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_worktree);
    }
}

/// `/worktree` 命令分发：`enter [name]` / `exit` / `drop` / 无参（报告状态）。
fn command_worktree(host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((head, tail)) => (head, tail.trim()),
        None => (arg, ""),
    };

    match sub {
        "" | "list" | "status" => report(host),
        "enter" => enter(host, rest),
        "exit" => exit(host, false),
        "drop" => exit(host, true),
        other => {
            let _ = other;
            host.push_message(
                "Usage: /worktree enter [name] | /worktree exit | /worktree drop",
                UiNotifyLevel::Warning,
            );
        }
    }
    false
}

/// 没有子命令时报告当前状态。
fn report(host: &mut dyn CommandHost) {
    let active = ACTIVE.lock().ok().and_then(|state| state.clone());
    match active {
        Some(active) => host.push_message(
            &format!(
                "worktree: inside copy {} (name: {}, branch on exit: {})\n  /worktree exit \
                 commits the work to a branch, /worktree drop throws it away",
                short_path(&active.path),
                active.name,
                branch_name(&active.name),
            ),
            UiNotifyLevel::Info,
        ),
        None => host.push_message(
            &format!(
                "worktree: not in a copy — /worktree enter [name] creates one under {}",
                short_path(&worktree_root().to_string_lossy())
            ),
            UiNotifyLevel::Info,
        ),
    }
}

/// `/worktree enter [name]`：建副本并把会话切进去。
fn enter(host: &mut dyn CommandHost, raw: &str) {
    if host.is_busy() {
        host.push_message(
            "Cannot change directories while a run is in progress; press Esc to interrupt first.",
            UiNotifyLevel::Warning,
        );
        return;
    }

    if let Some(active) = ACTIVE.lock().ok().and_then(|state| state.clone()) {
        host.push_message(
            &format!(
                "worktree: already inside {} — /worktree exit first",
                short_path(&active.path)
            ),
            UiNotifyLevel::Warning,
        );
        return;
    }

    let cwd = host.cwd().to_string_lossy().to_string();
    let name = if raw.is_empty() {
        format!("wt{}", now_ms())
    } else {
        raw.to_string()
    };
    if !valid_name(&name) {
        host.push_message(
            &format!("worktree: invalid name {name:?} (use letters, digits, '.', '_' or '-')"),
            UiNotifyLevel::Warning,
        );
        return;
    }

    // 严格失败：不在仓库里 / 还没有提交 / 建不出来，都直接报错，不静默退回原目录。
    let top = match git(&cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(top) => top,
        Err(err) => {
            host.push_message(&format!("worktree: {err}"), UiNotifyLevel::Warning);
            return;
        }
    };
    if let Err(err) = git(&cwd, &["rev-parse", "HEAD"]) {
        host.push_message(
            &format!("worktree: the repository has no commit yet: {err}"),
            UiNotifyLevel::Warning,
        );
        return;
    }

    let dirty = git(&cwd, &["status", "--porcelain"])
        .map(|text| !text.trim().is_empty())
        .unwrap_or(false);

    let repo = Path::new(&top)
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "repo".to_string());
    let path = worktree_root().join(format!("{repo}-{name}"));
    let path_str = path.to_string_lossy().to_string();
    if path.exists() {
        host.push_message(
            &format!(
                "worktree: {} already exists — pick another name or delete it first",
                short_path(&path_str)
            ),
            UiNotifyLevel::Warning,
        );
        return;
    }
    if path
        .parent()
        .is_some_and(|parent| std::fs::create_dir_all(parent).is_err())
    {
        host.push_message(
            &format!("worktree: could not create {}", short_path(&path_str)),
            UiNotifyLevel::Warning,
        );
        return;
    }

    if let Err(err) = git(&top, &["worktree", "add", "--detach", &path_str, "HEAD"]) {
        host.push_message(
            &format!("worktree: could not create the copy: {err}"),
            UiNotifyLevel::Warning,
        );
        return;
    }

    // 调用方的 cwd 可能在仓库子目录里：副本内的落点要保留同样的层级，否则 monorepo 包的
    // 改动范围会悄悄扩大到整个仓库。两侧都取 realpath——git 给的是解析过的路径，而 cwd
    // 可能经过符号链接进来（macOS 的 /tmp）。
    let top_real = std::fs::canonicalize(&top).unwrap_or_else(|_| Path::new(&top).to_path_buf());
    let cwd_real = std::fs::canonicalize(&cwd).unwrap_or_else(|_| Path::new(&cwd).to_path_buf());
    let work_path = match cwd_real.strip_prefix(&top_real) {
        Ok(subdir) if !subdir.as_os_str().is_empty() => {
            path.join(subdir).to_string_lossy().to_string()
        }
        _ => path_str.clone(),
    };

    if let Ok(mut state) = ACTIVE.lock() {
        *state = Some(Active {
            original_cwd: cwd,
            path: path_str.clone(),
            name: name.clone(),
        });
    }
    host.set_cwd(&work_path);

    let mut text = format!(
        "worktree: working in {} (name: {}); /worktree exit commits the work to branch {}",
        short_path(&work_path),
        name,
        branch_name(&name)
    );
    if dirty {
        text.push_str(
            "\n  note: uncommitted changes were NOT copied — they are still in the original \
             directory",
        );
    }
    host.push_message(&text, UiNotifyLevel::Success);
}

/// `/worktree exit`（`discard = false`，提交成分支）与 `/worktree drop`（`discard = true`，丢弃）。
fn exit(host: &mut dyn CommandHost, discard: bool) {
    if host.is_busy() {
        host.push_message(
            "Cannot change directories while a run is in progress; press Esc to interrupt first.",
            UiNotifyLevel::Warning,
        );
        return;
    }

    let Some(active) = ACTIVE.lock().ok().and_then(|mut state| state.take()) else {
        host.push_message(
            "worktree: not in a copy — /worktree enter [name] creates one",
            UiNotifyLevel::Warning,
        );
        return;
    };

    // 原目录不在了就不再往下走：副本一删，会话的 cwd 就连一个存在的目录都不剩。
    if !Path::new(&active.original_cwd).is_dir() {
        if let Ok(mut state) = ACTIVE.lock() {
            *state = Some(active.clone());
        }
        host.push_message(
            &format!(
                "worktree: the original working directory {} no longer exists — staying in {} \
                 (delete it by hand when you are done)",
                short_path(&active.original_cwd),
                short_path(&active.path)
            ),
            UiNotifyLevel::Warning,
        );
        return;
    }

    let copy_exists = Path::new(&active.path).exists();
    let mut branch = None;

    if copy_exists && !discard {
        let dirty = git(&active.path, &["status", "--porcelain"])
            .map(|text| !text.trim().is_empty())
            .unwrap_or(true);
        if dirty {
            match commit(&active) {
                Ok(name) => branch = Some(name),
                Err(err) => {
                    // 提交失败绝不能顺手删副本：先把状态放回去，让用户能继续待在副本里处理。
                    if let Ok(mut state) = ACTIVE.lock() {
                        *state = Some(active.clone());
                    }
                    host.push_message(
                        &format!(
                            "worktree: could not commit the changes, the copy was kept at {}: {err}",
                            short_path(&active.path)
                        ),
                        UiNotifyLevel::Warning,
                    );
                    return;
                }
            }
        }
    }

    if copy_exists
        && let Err(err) = git(
            &active.original_cwd,
            &["worktree", "remove", "--force", &active.path],
        )
    {
        // 删不掉就退一步清掉注册项，至少别让主仓库里留下一个悬空的 worktree 条目。
        _ = git(&active.original_cwd, &["worktree", "prune"]);
        host.push_message(
            &format!(
                "worktree: could not delete {} ({err}); removed from the repository's \
                 worktree list instead",
                short_path(&active.path)
            ),
            UiNotifyLevel::Warning,
        );
    }

    host.set_cwd(&active.original_cwd);

    let text = match (branch, copy_exists, discard) {
        (Some(branch), _, _) => format!(
            "worktree: left the copy, changes committed to branch {branch} in {}",
            short_path(&active.original_cwd)
        ),
        (None, true, true) => format!(
            "worktree: left the copy, its changes were discarded ({})",
            short_path(&active.path)
        ),
        (None, true, false) => "worktree: left the copy, nothing to commit".to_string(),
        (None, false, _) => format!(
            "worktree: left the copy, {} was already gone",
            short_path(&active.path)
        ),
    };
    host.push_message(&text, UiNotifyLevel::Success);
}

/// 把副本里的改动全部提交，再在副本 HEAD 上建一个分支；返回实际的分支名。
///
/// 同名分支已存在时加时间戳后缀，不覆盖上一份成果。失败**不清理任何东西**（副本原样保留）。
fn commit(active: &Active) -> Result<String, String> {
    git(&active.path, &["add", "-A"])?;
    let message = format!("prux-worktree: {}", active.name);
    // `--no-verify`：用户的钩子不该在这个临时副本里跑（与 subagent 的副本一致）。
    git(&active.path, &["commit", "--no-verify", "-m", &message])?;

    let mut branch = branch_name(&active.name);
    if git(&active.path, &["branch", &branch]).is_err() {
        branch = format!("{}-{}", branch_name(&active.name), now_ms());
        git(&active.path, &["branch", &branch])?;
    }
    Ok(branch)
}

/// 副本名 → 分支名（`prux-wt-<name>`）。
fn branch_name(name: &str) -> String {
    format!("{BRANCH_PREFIX}-{name}")
}

/// 副本根目录：`agent_dir()/extensions/worktrees`。
fn worktree_root() -> PathBuf {
    settings_manager::agent_dir()
        .join("extensions")
        .join("worktrees")
}

/// 副本名只允许 git 分支名与路径都安全的字符。
fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !name.starts_with('.')
}

/// 同步跑一条 git 命令（命令 handler 在 UI 线程上，用不了 async 版本）。
///
/// 输出一律走管道：这是 TUI 进程，子进程绝不能继承终端。
fn git(cwd: &str, args: &[&str]) -> Result<String, String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("git {} could not run: {e}", args.join(" ")))?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if stderr.is_empty() {
            format!("git {} failed ({})", args.join(" "), out.status)
        } else {
            stderr
        });
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::CommandHost;
    use std::{any::Any, path::PathBuf};

    /// 全局 [`ACTIVE`] 是进程级的：同一测试文件里的用例必须串行跑，否则会互相抢状态。
    static SERIAL: Mutex<()> = Mutex::new(());

    /// 测试用宿主：只记录 `set_cwd` 结果与提示，其余能力空实现。
    struct MockHost {
        /// 宿主当前工作目录。
        cwd: String,
        /// 收到的系统消息正文（断言提示文案用）。
        messages: Vec<String>,
        /// 是否正在流式输出（忙碌时应拒绝换目录）。
        busy: bool,
    }

    impl MockHost {
        /// 以 `cwd` 为工作目录的空闲宿主。
        fn new(cwd: &str) -> Self {
            MockHost {
                cwd: cwd.to_string(),
                messages: Vec::new(),
                busy: false,
            }
        }
    }

    impl CommandHost for MockHost {
        fn push_message(&mut self, text: &str, _level: UiNotifyLevel) {
            self.messages.push(text.to_string());
        }
        fn push_markdown(&mut self, text: &str, level: UiNotifyLevel) {
            self.push_message(text, level);
        }
        fn show_dock(&mut self) {}
        fn dock_visible(&self) -> bool {
            false
        }
        fn open_extension_settings(&mut self, _ext: &str) {}
        fn send_prompt(&mut self, _text: &str) {}
        fn current_model(&self) -> Option<(String, String)> {
            None
        }
        fn switch_model(&mut self, _provider: &str, _model_id: &str) {}
        fn set_cwd(&mut self, path: &str) {
            self.cwd = path.to_string();
        }
        fn cwd(&self) -> PathBuf {
            PathBuf::from(&self.cwd)
        }
        fn is_busy(&self) -> bool {
            self.busy
        }
        fn set_status(&mut self, _text: &str) {}
        fn last_user_message(&self) -> Option<String> {
            None
        }
        fn debug_info(&mut self) {}
        fn debug_system_prompt(&mut self) {}
        fn session_path(&self) -> Option<String> {
            None
        }
        fn transcript_excerpt(&self, _max_messages: usize, _max_chars: usize) -> String {
            String::new()
        }
        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }

    /// 造一个真仓库（`git init` + 一次提交）。git 不在时返回 None（跳过）。
    fn repo() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().ok()?;
        let cwd = dir.path().to_string_lossy().to_string();
        git(&cwd, &["init", "-q"]).ok()?;
        git(&cwd, &["config", "user.email", "t@example.com"]).ok()?;
        git(&cwd, &["config", "user.name", "t"]).ok()?;
        std::fs::write(dir.path().join("a.txt"), "one\n").ok()?;
        git(&cwd, &["add", "-A"]).ok()?;
        git(&cwd, &["commit", "-qm", "init"]).ok()?;
        Some(dir)
    }

    /// 名字校验：路径分隔符与 `..` 一律拒绝。
    #[test]
    fn names_reject_path_traversal() {
        assert!(valid_name("feat-1"));
        assert!(valid_name("v1.2_x"));
        assert!(!valid_name(""));
        assert!(!valid_name("../escape"));
        assert!(!valid_name(".hidden"));
        assert!(!valid_name("a/b"));
    }

    /// 分支名固定带前缀。
    #[test]
    fn branch_name_is_prefixed() {
        assert_eq!(branch_name("feat"), "prux-wt-feat");
    }

    /// 完整来回：进副本 → 改文件 → 退出；改动以分支形式留在原仓库，副本被删，cwd 切回。
    #[test]
    fn enter_and_exit_commit_the_work_to_a_branch() {
        let Some(dir) = repo() else {
            return;
        };
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        *ACTIVE.lock().unwrap() = None;

        let root = dir.path().to_string_lossy().to_string();
        let mut host = MockHost::new(&root);
        command_worktree(&mut host, "worktree enter feat");

        let copy = host.cwd.clone();
        assert_ne!(copy, root, "进入副本后 cwd 应该变了");
        assert!(Path::new(&copy).join("a.txt").exists(), "副本里有文件");
        assert!(ACTIVE.lock().unwrap().is_some(), "状态已记录");

        std::fs::write(Path::new(&copy).join("a.txt"), "two\n").unwrap();
        command_worktree(&mut host, "worktree exit");

        assert_eq!(host.cwd, root, "退出后回到原目录");
        assert!(!Path::new(&copy).exists(), "副本应被删掉");
        assert!(ACTIVE.lock().unwrap().is_none(), "状态已清空");
        assert_eq!(
            git(&root, &["show", "prux-wt-feat:a.txt"]).expect("branch content"),
            "two",
            "改动应在分支上"
        );
    }

    /// `drop` 丢掉改动、不建分支；忙碌时 `enter` 被拒绝。
    #[test]
    fn drop_discards_changes_and_busy_enter_is_refused() {
        let Some(dir) = repo() else {
            return;
        };
        let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let _agent_dir = crate::test_support::AgentDirGuard::temp();
        *ACTIVE.lock().unwrap() = None;

        let root = dir.path().to_string_lossy().to_string();
        let mut host = MockHost::new(&root);
        host.busy = true;
        command_worktree(&mut host, "worktree enter feat");
        assert_eq!(host.cwd, root, "忙碌时不换目录");
        assert!(ACTIVE.lock().unwrap().is_none());

        host.busy = false;
        command_worktree(&mut host, "worktree enter thrown");
        let copy = host.cwd.clone();
        std::fs::write(Path::new(&copy).join("a.txt"), "gone\n").unwrap();
        command_worktree(&mut host, "worktree drop");

        assert_eq!(host.cwd, root);
        assert!(!Path::new(&copy).exists());
        assert!(
            git(&root, &["branch", "--list", "prux-wt-thrown"])
                .unwrap()
                .trim()
                .is_empty(),
            "drop 不该留下分支"
        );
    }
}

//! `rollback` 扩展：回合级文件检查点，`edit` / `write` 改坏的东西能退回这一回合开始前。
//!
//! 与 `rewind` 的分工：`rewind` 只把**会话分支**退回去（磁盘一个字节都不动），本扩展补的是
//! 另一半——**文件**也能退。两者互不依赖，可以只开一个。
//!
//! **为什么单独一个扩展**：它会在每次 `edit` / `write` 前复制一份文件，而且是**默认关闭**的
//! 能力。混进 `rewind` 里意味着「只想撤回一句误发的话」也要顺带付磁盘与 IO 的钱。
//! 已经用 git 的项目通常也不需要它（`git checkout` / `git restore` 覆盖了已提交的状态），
//! 但它能退的还有 git 退不了的东西：**本回合开始前尚未提交**的内容。本扩展不检测 git 是否存在，
//! 用不用由使用者决定。
//!
//! 快照按**回合**分组（一条用户消息 = 一回合）：`/rollback` 列出改过文件的回合，
//! `/rollback diff <n>` 看某回合动了哪些文件，`/rollback <n>` 把那一回合改过的文件退回回合
//! 开始前的样子——**还原前会先给当前内容再拍一张快照**，所以手滑也能再还原回来。
//! 忙碌（流式输出 / 工具执行）期间拒绝执行：此时 agent 可能正在写同一个文件。
//! 磁盘布局与清理策略见 [`checkpoints`]。
//!
//! **默认不启用**，声明 `Dev` / `Creator` 两个并排模式（`Minimal` 下不可用）。

mod checkpoints;

use crate::{
    core::{
        extensions::{
            BeforeToolCallOutcome, CommandHost, Extension, ExtensionCommand, ExtensionHook,
            ExtensionMode, ExtensionTool, SubcommandDef, ToolExecCtx, UiNotifyLevel,
        },
        provider::AgentMessage,
        tools::ToolError,
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_ROLLBACK, command_arg},
    modes::tui::handlers::register_host_command,
    utils::{display::plural, paths::short_path},
};
use futures_util::future::BoxFuture;
use serde_json::Value;

/// 扩展名（注册名，也是 `/extension` 面板里的名字）。
const EXT: &str = "rollback";

/// 斜杠命令名（`/rollback`）。
const CMD: &str = "rollback";

/// `/rollback` 子命令（顺序 = 输入框候选顺序）。
const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "list",
        description: "List recent turns that changed files (default)",
    },
    SubcommandDef {
        name: "diff",
        description: "List the files a turn changed: /rollback diff <n>",
    },
];

/// `/rollback` 列表最多列出的回合数。
const MAX_LIST_TURNS: usize = 10;

/// `/rollback` 列表里每个回合最多列出的文件名数（其余用 `+N more` 概括）。
const MAX_LIST_FILES: usize = 5;

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static ROLLBACK_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_ROLLBACK,
    make: || -> std::sync::Arc<dyn Extension> { std::sync::Arc::new(Rollback) },
};

/// `rollback` 扩展。
pub struct Rollback;

impl Extension for Rollback {
    /// 返回扩展标识常量 `EXT`（`"rollback"`）。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的说明文案。
    fn description(&self) -> &str {
        "Per-turn file checkpoints: files are snapshotted before edit/write, grouped by turn, so \
         /rollback <n> can put back what one user message changed. /rollback lists the turns, \
         /rollback diff <n> shows a turn's files. The current content is snapshotted before a \
         restore, so a mistaken rollback can be rolled back too."
    }

    /// 声明 `Dev` / `Creator` 两个并排模式：`Minimal` 下不可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认不启用：每次写盘都要复制一份文件，属于用户显式选择的能力。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供工具：检查点只在写盘前被动产生，由命令消费。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/rollback`（`list` / `diff <n>` / `<n>`）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Restore the files a turn changed: /rollback [list] | diff <n> | <n>"
                .to_string(),
            // handler 自己检查忙碌（忙碌时直接拒绝），不锁 agent。
            busy_safe: true,
            subcommands: SUBCOMMANDS.to_vec(),
        }]
    }

    /// 接线 `/rollback` 执行入口（命令的"存在性"由已启用扩展的 [`Self::commands`] 声明）。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_rollback);
    }

    /// 订阅工具前钩子（文件快照）与提交通知（回合计数）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![
            ExtensionHook::BeforeToolCallAsync,
            ExtensionHook::UserPrompt,
        ]
    }

    /// 绑定当前工作目录：检查点按工程分桶，回合号接着该目录已有的最大值往下数。
    ///
    /// 核心在会话建立 / 每回合开始 / 会话切换后各投一次，因此中途启用本扩展也不会
    /// 记到错的地方。
    fn on_exec_ctx(&self, ctx: &ToolExecCtx) {
        checkpoints::set_workspace(&ctx.cwd);
    }

    /// 用户提交（且当时空闲）= 新回合开始：后续文件快照都记在这一回合名下。
    /// 忙碌时的提交是 steer，仍属当前回合。
    fn on_user_submit(&self, _text: &str, busy: bool) {
        if !busy {
            checkpoints::start_turn();
        }
    }

    /// 写盘前快照目标文件；不影响工具执行（恒放行）。
    fn before_tool_call_async(
        &self,
        name: String,
        args: Value,
        _assistant_message: Option<AgentMessage>,
        _context: Vec<AgentMessage>,
        _all_args: Value,
    ) -> BoxFuture<'static, std::result::Result<BeforeToolCallOutcome, ToolError>> {
        Box::pin(async move {
            checkpoints::snapshot(&name, &args);
            Ok(BeforeToolCallOutcome::allow())
        })
    }
}

/// `/rollback` 命令分发：`list`（默认）/ `diff <n>` / `<n>`。
///
/// 忙碌时一律拒绝：此时 agent 可能正在写同一个文件，还原会被下一笔写入盖掉。
fn command_rollback(host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let (sub, rest) = match arg.split_once(char::is_whitespace) {
        Some((head, tail)) => (head, tail.trim()),
        None => (arg, ""),
    };

    if host.is_busy() {
        host.push_message(
            "Cannot roll back files while a run is in progress; press Esc to interrupt first.",
            UiNotifyLevel::Warning,
        );
        return false;
    }

    match sub {
        "" | "list" => list_turns(host),
        "diff" => show_turn(host, rest),
        turn => restore_turn(host, turn),
    }
    false
}

/// `/rollback list`（默认）：列出改过文件的回合，最近的在前。
fn list_turns(host: &mut dyn CommandHost) {
    let turns = checkpoints::turns();
    if turns.is_empty() {
        host.push_message(
            "rollback: no file checkpoints in this working directory yet (files are snapshotted \
             before edit/write).",
            UiNotifyLevel::Info,
        );
        return;
    }

    let mut lines = vec![format!(
        "rollback: {} turn(s) with file changes — /rollback <n> restores one, /rollback diff <n> shows it",
        turns.len()
    )];
    for view in turns.iter().take(MAX_LIST_TURNS) {
        lines.push(format!("  turn {} — {}", view.turn, summarize(&view.paths)));
    }
    if turns.len() > MAX_LIST_TURNS {
        lines.push(format!(
            "  … {} earlier turn(s)",
            turns.len() - MAX_LIST_TURNS
        ));
    }
    host.push_message(&lines.join("\n"), UiNotifyLevel::Info);
}

/// `/rollback diff <n>`：只列文件，不动磁盘。
fn show_turn(host: &mut dyn CommandHost, arg: &str) {
    let Some(turn) = parse_turn(host, arg) else {
        return;
    };
    match turn_paths(turn) {
        Some(paths) => host.push_message(
            &format!(
                "rollback: turn {turn} changed {} file{}\n  {}",
                paths.len(),
                plural(paths.len()),
                paths.join("\n  ")
            ),
            UiNotifyLevel::Info,
        ),
        None => host.push_message(
            &format!("rollback: turn {turn} has no file changes"),
            UiNotifyLevel::Warning,
        ),
    }
}

/// `/rollback <n>`：还原该回合改过的文件（还原前会自动给当前内容再拍一张快照）。
fn restore_turn(host: &mut dyn CommandHost, arg: &str) {
    let Some(turn) = parse_turn(host, arg) else {
        return;
    };
    match checkpoints::restore(turn) {
        Ok(paths) => host.push_message(
            &format!(
                "rollback: restored {} file{} from turn {turn}\n  {}",
                paths.len(),
                plural(paths.len()),
                paths.join("\n  ")
            ),
            UiNotifyLevel::Success,
        ),
        Err(err) => host.push_message(&format!("rollback: {err}"), UiNotifyLevel::Warning),
    }
}

/// 解析回合号；空/非数字时提示用法并返回 `None`。
fn parse_turn(host: &mut dyn CommandHost, arg: &str) -> Option<u32> {
    match arg.trim().parse::<u32>() {
        Ok(turn) if turn > 0 => Some(turn),
        _ => {
            host.push_message(
                "Usage: /rollback [list] | /rollback diff <n> | /rollback <n>",
                UiNotifyLevel::Warning,
            );
            None
        }
    }
}

/// 某一回合的文件清单（不存在则为 `None`）。
fn turn_paths(turn: u32) -> Option<Vec<String>> {
    checkpoints::turns()
        .into_iter()
        .find(|view| view.turn == turn)
        .map(|view| view.paths)
}

/// 列表里的回合摘要：`3 files: a/b.rs, c/d.rs +2 more`。
fn summarize(paths: &[String]) -> String {
    let shown: Vec<String> = paths
        .iter()
        .take(MAX_LIST_FILES)
        .map(|path| short_path(path))
        .collect();
    let mut text = format!(
        "{} file{}: {}",
        paths.len(),
        plural(paths.len()),
        shown.join(", ")
    );
    if paths.len() > MAX_LIST_FILES {
        text.push_str(&format!(" +{} more", paths.len() - MAX_LIST_FILES));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modes::tui::app::App;
    use serde_json::json;

    /// 临时 agent 目录 + 交给测试用的工程目录（快照与工程桶都落在前者里）。
    fn sandbox() -> (crate::test_support::AgentDirGuard, std::path::PathBuf) {
        let guard = crate::test_support::AgentDirGuard::temp();
        let dir = crate::test_support::agent_dir_override().expect("agent dir override");
        checkpoints::reset_for_tests();
        checkpoints::set_workspace(&dir.to_string_lossy());
        (guard, dir)
    }

    #[test]
    fn declares_dev_and_creator_modes_and_default_disabled() {
        let ext = Rollback;
        assert!(!ext.default_enabled(), "要复制文件，不该默认开启");
        assert_eq!(
            ext.modes(),
            vec![ExtensionMode::Dev, ExtensionMode::Creator],
            "应在 Dev / Creator 两个模式下可用"
        );
        assert!(ext.tools().is_empty());
    }

    /// 一个命令带 `list` / `diff` 两个子命令，且忙碌安全（忙碌判定在 handler 里）。
    #[test]
    fn declares_rollback_command() {
        let cmds = Rollback.commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "rollback");
        assert!(cmds[0].busy_safe);
        let subs: Vec<&str> = cmds[0].subcommands.iter().map(|s| s.name).collect();
        assert_eq!(subs, vec!["list", "diff"]);
    }

    /// 声明两个钩子：写盘前快照（异步）、提交通知（回合计数）。
    #[test]
    fn hooks_cover_snapshot_and_turn_counting() {
        let hooks = Rollback.hooks();
        assert!(hooks.contains(&ExtensionHook::BeforeToolCallAsync));
        assert!(hooks.contains(&ExtensionHook::UserPrompt));
    }

    /// 本回合改过的文件能退回回合开始前的样子。
    #[test]
    fn rollback_restores_a_turn() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_dir, dir) = sandbox();

        let file = dir.join("notes.txt");
        std::fs::write(&file, "before").unwrap();
        checkpoints::start_turn();
        checkpoints::snapshot("write", &json!({ "path": file.to_string_lossy() }));
        std::fs::write(&file, "after").unwrap();

        let turns = checkpoints::turns();
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].turn, 1);
        assert_eq!(turns[0].paths, vec![file.to_string_lossy().to_string()]);

        let restored = checkpoints::restore(1).expect("restore turn 1");
        assert_eq!(restored.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "before");
    }

    /// 回合里新建的文件，回滚等于删掉它（不是留一个空文件）。
    #[test]
    fn rollback_deletes_files_created_in_the_turn() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_dir, dir) = sandbox();

        let file = dir.join("new.rs");
        checkpoints::start_turn();
        checkpoints::snapshot("write", &json!({ "path": file.to_string_lossy() }));
        std::fs::write(&file, "fn main() {}").unwrap();

        assert!(checkpoints::restore(1).is_ok());
        assert!(!file.exists(), "新建的文件应被删除");
    }

    /// 同一回合对同一文件只拍最早那张：回滚拿回的是回合开始前的状态。
    #[test]
    fn keeps_the_earliest_snapshot_of_a_turn() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_dir, dir) = sandbox();

        let file = dir.join("a.rs");
        std::fs::write(&file, "v0").unwrap();
        checkpoints::start_turn();
        checkpoints::snapshot("edit", &json!({ "path": file.to_string_lossy() }));
        std::fs::write(&file, "v1").unwrap();
        checkpoints::snapshot("edit", &json!({ "path": file.to_string_lossy() }));
        std::fs::write(&file, "v2").unwrap();

        assert!(checkpoints::restore(1).is_ok());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v0");
    }

    /// 还原本身也能被还原：当前内容在覆盖前被拍成新快照。
    #[test]
    fn restore_snapshots_the_current_content_first() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_dir, dir) = sandbox();

        let file = dir.join("b.rs");
        std::fs::write(&file, "v0").unwrap();
        checkpoints::start_turn();
        checkpoints::snapshot("write", &json!({ "path": file.to_string_lossy() }));
        std::fs::write(&file, "v1").unwrap();
        checkpoints::start_turn();
        assert!(checkpoints::restore(1).is_ok());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v0");

        // 第 2 回合里拍了「v1」，再还原它就能拿回 v1。
        let turns = checkpoints::turns();
        let second = turns.iter().find(|t| t.turn == 2).expect("turn 2 snapshot");
        assert_eq!(second.paths, vec![file.to_string_lossy().to_string()]);
        assert!(checkpoints::restore(2).is_ok());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");
    }

    /// 只认 `edit` / `write`；其它工具不拍快照，未知回合号报错。
    #[test]
    fn ignores_other_tools_and_unknown_turns() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_dir, dir) = sandbox();

        let file = dir.join("read-only.txt");
        std::fs::write(&file, "x").unwrap();
        checkpoints::start_turn();
        checkpoints::snapshot("read", &json!({ "path": file.to_string_lossy() }));
        assert!(checkpoints::turns().is_empty(), "read 不该产生检查点");
        assert!(checkpoints::restore(99).is_err(), "未知回合应报错");
    }

    /// `/rollback` 命令：忙碌拒绝；无检查点时给出提示；参数非法时提示用法。
    #[test]
    fn command_guards_busy_and_arguments() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let (_dir, _workspace) = sandbox();

        let mut busy = App::new();
        busy.busy = true;
        assert!(!command_rollback(&mut busy, "rollback 1"));

        let mut idle = App::new();
        assert!(!command_rollback(&mut idle, "rollback"));
        assert!(!command_rollback(&mut idle, "rollback diff"));
        assert!(!command_rollback(&mut idle, "rollback nope"));
    }

    /// 摘要文案：短路径 + 超出上限时用 `+N more` 概括。
    #[test]
    fn summarizes_paths() {
        let paths: Vec<String> = (0..7).map(|i| format!("/w/f{i}.rs")).collect();
        assert_eq!(
            summarize(&paths),
            "7 files: w/f0.rs, w/f1.rs, w/f2.rs, w/f3.rs, w/f4.rs +2 more"
        );
        assert_eq!(summarize(&paths[..1]), "1 file: w/f0.rs");
    }
}

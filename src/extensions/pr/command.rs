//! `/pr` 命令族：看配置与最近一次交付、手动跑一次。
//!
//! 命令只读写本扩展自身的配置与运行态，因此声明 `busy_safe`。

use super::{Mode, remember_session, run_now, status_text};
use crate::{
    core::extensions::{CommandHost, SubcommandDef, UiNotifyLevel},
    extensions::{command_arg, util::notify_text, util::session_id_of},
};

/// `/pr` 子命令（顺序 = 输入框候选顺序，与 [`handle`] 的 match 分支一一对应）。
pub(super) const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "status",
        description: "Show the config, the session branch / PR and the last result",
    },
    SubcommandDef {
        name: "run",
        description: "Commit, push, open the pull request and report the CI result",
    },
    SubcommandDef {
        name: "new",
        description: "Same, but always on a brand-new branch and a brand-new PR",
    },
];

/// `/pr` 命令入口。返回值恒为 `false`（不退出 TUI）。
pub(super) fn handle(host: &mut dyn CommandHost, raw: &str) -> bool {
    let arg = command_arg(raw).trim();
    let sub = match arg.split_once(char::is_whitespace) {
        Some((head, _)) => head,
        None => arg,
    };

    match sub {
        "" | "status" => host.push_message(&status_text(), UiNotifyLevel::Info),
        "run" => run_async(host, Mode::Run),
        "new" => run_async(host, Mode::New),
        other => host.push_message(
            &format!("pr: unknown subcommand `{other}` (status|run|new)"),
            UiNotifyLevel::Warning,
        ),
    }
    false
}

/// `/pr run` / `/pr new`：交付一次（后台线程，结果回聊天区）。
fn run_async(host: &mut dyn CommandHost, mode: Mode) {
    // 提交信息要在后台线程里生成，模型与会话 id 得先在这里拿（宿主只在同步回边可得）：
    // 本会话中途启用时 `on_session_start` 没跑过，槽里就没有 id，opencode 系会回 400。
    remember_session(session_id_of(host.session_path().as_deref()));

    let model = host.current_model();
    host.push_message(
        "pr: delivering now — results land here when done",
        UiNotifyLevel::Info,
    );
    // 等 CI 可能是分钟级，不能卡住事件循环：扔到独立线程。
    std::thread::spawn(move || {
        let (text, level) = run_now(mode, model);
        notify_text(&format!("pr: {text}"), level);
    });
}

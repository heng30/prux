//! `rewind` 扩展：回退最后一次用户输入及由它产生的所有后续输出。
//!
//! **用途**：误发送内容（错别字、发错对象、发到错会话）时，把「上一次用户输入 + 基于它
//! 产生的全部 assistant / 工具输出」从模型上下文中一起撤回，避免污染后续对话。
//!
//! **语义**：把当前会话活动分支的叶子指针移到最后一条 **user 消息**的父节点——该 user
//! 消息与它之后的所有条目一起离开活动分支，不再进入后续 provider 请求。原始日志不会被
//! 删除，仍保留在会话树里（`/tree` 可查看 / 切回）；只移动指针 + 追加一条 lane mutation。
//!
//! **确认**：`/rewind` 每次都弹出选择面板（避免误操作），三个选项：
//! - `Rewind and edit input`：回退并把被撤回的用户输入回填输入框，供修改后重发；
//! - `Rewind and discard input`：回退并丢弃输入（历史记录仍可用 `/history` 找回）；
//! - `Cancel`：取消。
//!
//! 忙碌（流式输出 / 工具执行）期间拒绝执行：此时分支正被追加，回退会破坏进行中的回合；
//! 需先按 Esc 中断。
//!
//! **默认不启用**，声明 `Dev` / `Creator` 两个并排模式（`Minimal` 下不可用）。

use crate::{
    core::extensions::{
        self, Extension, ExtensionCommand, ExtensionMode, ExtensionTool, ExtensionUiRequest,
        SelectOption, next_ui_id,
    },
    extensions::{
        EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_REWIND, command_arg, util::choice_key,
    },
    modes::interactive::{
        app::{App, MsgLevel},
        handlers::register_slash_command,
    },
};
use std::sync::{Mutex, OnceLock};

/// 扩展名（注册名，也是 `/extension` 面板里的名字）
const EXT: &str = "rewind";

/// 斜杠命令名（`/rewind`）
const CMD: &str = "rewind";

/// 确认面板标题里回显的最后一条用户输入的最大字符数（超出截断并加省略号）。
const PREVIEW_MAX: usize = 60;

/// 确认面板选项的 key（面板回传串的首个 token，见 [`choice_key`]）。
const KEY_EDIT: &str = "rewind-edit";
/// 选择「回退并丢弃输入」时回传的 key（历史记录仍可用 `/history` 找回）。
const KEY_DISCARD: &str = "rewind-discard";
/// 选择「取消」时回传的 key，与其它面板共用的通用取消值。
const KEY_CANCEL: &str = "cancel";

/// 自声明工厂：linkme 分布式切片
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static REWIND_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_REWIND,
    make: || -> std::sync::Arc<dyn Extension> { std::sync::Arc::new(Rewind) },
};

/// 待用户确认的面板 id（`on_ui_choice` 据此认领回传；一次只有一个面板）。
fn pending() -> &'static Mutex<Option<u64>> {
    /// 待确认面板 id 的进程级单例，一次只允许存在一个待确认面板。
    static P: OnceLock<Mutex<Option<u64>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

/// 取走并校验待确认面板 id：id 不匹配（不是本扩展的面板）返回 `false`。
fn take_pending(id: u64) -> bool {
    let mut guard = pending().lock().unwrap_or_else(|e| e.into_inner());
    if *guard == Some(id) {
        *guard = None;
        true
    } else {
        false
    }
}

/// `rewind` 扩展。
pub struct Rewind;

impl Extension for Rewind {
    /// 返回扩展标识常量 `EXT`（`"rewind"`）。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的说明文案。
    fn description(&self) -> &str {
        "Rewind the last user input: remove it and everything generated from it from the context. \
         /rewind opens a confirmation panel each time (rewind & edit / rewind & discard / cancel). \
         History is kept in the session tree; only the active branch is moved back."
    }

    /// 声明 `Dev` / `Creator` 两个并排模式：`Minimal` 下不可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认不启用：回退会改变模型上下文，属用户显式开启的能力。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供任何工具，只提供斜杠命令。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/rewind` 命令；标为 `busy_safe` 以便忙碌时也能弹确认面板。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "Rewind the last user input and all later output (asks for confirmation)"
                .to_string(),
            // handler 不锁 agent：忙碌时只给提示，空闲时只弹确认面板，故忙碌可安全执行
            // （否则忙碌时 `/rewind` 会被当作普通文本排进 steer 队列发给模型）。
            busy_safe: true,
            subcommands: Vec::new(),
        }]
    }

    /// 接线 `/rewind` 执行入口（命令的"存在性"由已启用扩展的 [`Self::commands`] 声明）。
    fn on_registered(&self) {
        register_slash_command(EXT, CMD, command_rewind);
    }

    /// 启用状态变化：禁用时清掉待确认状态，避免残留 id 误认领后续面板回传。
    fn on_enabled_changed(&self, enabled: bool) {
        if !enabled {
            *pending().lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// 认领确认面板回传：按选择发起回退（带/不带还原到编辑器）。
    /// id 不匹配（非本扩展待确认项）或选择为取消时返回 None；始终不返回 follow-up 文本。
    fn on_ui_choice(&self, id: u64, choice: Option<String>) -> Option<String> {
        if !take_pending(id) {
            return None;
        }

        match choice_key(&choice.unwrap_or_default()) {
            KEY_EDIT => extensions::request_ui(ExtensionUiRequest::RewindLastUserInput {
                restore_to_editor: true,
            }),
            KEY_DISCARD => extensions::request_ui(ExtensionUiRequest::RewindLastUserInput {
                restore_to_editor: false,
            }),
            // cancel / Esc：什么都不做
            _ => {}
        }

        None // 必须返回 None：返回文本会被当作 follow-up prompt 起新回合
    }
}

/// `/rewind` 命令分发：校验前置条件 → 弹确认面板 → 结果经 [`Extension::on_ui_choice`] 处理。
fn command_rewind(st: &mut App, raw: &str) -> bool {
    if !command_arg(raw).trim().is_empty() {
        st.push_msg(
            "Usage: /rewind (no arguments)".to_string(),
            MsgLevel::Warning,
        );
        return false;
    }

    // 忙碌时分支正被追加：回退会破坏进行中的回合，且回合结束后叶子又会前进。
    if st.busy {
        st.push_msg(
            "Cannot rewind while a run is in progress; press Esc to interrupt first.".to_string(),
            MsgLevel::Warning,
        );
        return false;
    }

    // 会话里至少要有 user 消息才可能回退（否则直接提示，不弹无意义的面板）。
    let Some(preview) = last_user_preview(st) else {
        st.push_msg(
            "Nothing to rewind: no user input in the current session.".to_string(),
            MsgLevel::Info,
        );
        return false;
    };

    let id = next_ui_id();
    *pending().lock().unwrap_or_else(|e| e.into_inner()) = Some(id);
    extensions::request_ui(confirm_panel_request(id, &preview));
    false
}

/// 最后一条 user 消息的单行预览（折叠空白、超长截断）；无 user 消息返回 `None`。
///
/// 面板标题回显它，让用户确认即将回退的是哪一条输入（避免与最近一次提交错位）。
fn last_user_preview(st: &App) -> Option<String> {
    let text = st.messages.iter().rev().find(|m| m.role == "user")?.text();
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.is_empty() {
        return Some("(empty message)".to_string());
    }

    let mut chars: Vec<char> = one_line.chars().collect();
    if chars.len() > PREVIEW_MAX {
        chars.truncate(PREVIEW_MAX);
        Some(format!("{}…", chars.into_iter().collect::<String>()))
    } else {
        Some(one_line)
    }
}

/// 组装确认面板请求：每次 `/rewind` 都必须经此确认，避免误操作。
///
/// 选项串按扩展约定 `"<key>  <说明>"`（回传后取首个 token 当 key，见 [`choice_key`]）；
/// `preview` 是即将回退的最后一条用户输入（单行截断），显示在标题里。
fn confirm_panel_request(id: u64, preview: &str) -> ExtensionUiRequest {
    ExtensionUiRequest::Select {
        id,
        title: format!(
            "Rewind \"{preview}\" and everything after it? They will leave the context."
        ),
        options: vec![
            SelectOption::new(format!(
                "{KEY_EDIT}  Rewind and restore input to the editor",
            )),
            SelectOption::new(format!("{KEY_DISCARD}  Rewind and discard the input")),
            SelectOption::new(format!("{KEY_CANCEL}  Cancel")),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::take_pending_ui;
    use crate::core::provider::AgentMessage;

    #[test]
    fn declares_dev_and_creator_modes_and_default_disabled() {
        let ext = Rewind;
        assert!(!ext.default_enabled(), "rewind 应默认不启用");
        assert_eq!(
            ext.modes(),
            vec![ExtensionMode::Dev, ExtensionMode::Creator],
            "rewind 应在 Dev / Creator 两个模式下可用"
        );
        assert!(ExtensionMode::Dev.usable_in(ExtensionMode::Dev));
        assert!(ExtensionMode::Creator.usable_in(ExtensionMode::Creator));
        assert!(!ExtensionMode::Dev.usable_in(ExtensionMode::Minimal));
        assert!(!ExtensionMode::Creator.usable_in(ExtensionMode::Minimal));
    }

    #[test]
    fn command_is_busy_safe_and_has_no_subcommands() {
        let cmds = Rewind.commands();
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "rewind");
        assert!(cmds[0].busy_safe, "handler 不锁 agent，忙碌时可安全执行");
        assert!(cmds[0].subcommands.is_empty());
    }

    /// 确认面板：标题回显待回退输入 + 三条选项，key 分别可识别。
    #[test]
    fn confirm_panel_offers_edit_discard_cancel() {
        match confirm_panel_request(42, "hello world") {
            ExtensionUiRequest::Select { id, title, options } => {
                assert_eq!(id, 42);
                assert!(title.contains("hello world"), "got: {title}");
                assert_eq!(options.len(), 3);
                let keys: Vec<&str> = options.iter().map(|o| choice_key(&o.text)).collect();
                assert_eq!(keys, vec![KEY_EDIT, KEY_DISCARD, KEY_CANCEL]);
                // 不得给任何选项设展示色：面板选中色即 accent，固定色会让该行永远
                // 看着像被选中（选中与否都穿 fg，只有 `→` 前缀区分）。
                assert!(
                    options.iter().all(|o| o.fg.is_none()),
                    "选项应走默认配色：{options:?}"
                );
            }
            other => panic!("应为选择面板：{other:?}"),
        }
    }

    /// 标题预览：折叠空白、超长截断；无 user 消息返回 None。
    #[test]
    fn last_user_preview_collapses_and_truncates() {
        let mut st = App::new();
        assert_eq!(last_user_preview(&st), None, "无 user 消息应返回 None");

        st.messages
            .push(AgentMessage::user_text("line1\nline2\t  line3"));
        assert_eq!(last_user_preview(&st).as_deref(), Some("line1 line2 line3"));

        st.messages
            .push(AgentMessage::user_text(&"x".repeat(PREVIEW_MAX + 10)));
        let preview = last_user_preview(&st).unwrap();
        assert!(preview.ends_with('…'), "超长应加省略号：{preview}");
        assert_eq!(preview.chars().count(), PREVIEW_MAX + 1);

        let mut empty = App::new();
        empty.messages.push(AgentMessage::user_text("   "));
        assert_eq!(
            last_user_preview(&empty).as_deref(),
            Some("(empty message)")
        );
    }

    /// 确认「回退并编辑」→ 回传 `restore_to_editor = true` 的 Rewind 请求。
    #[test]
    fn confirm_edit_requests_restore_to_editor() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while take_pending_ui().is_some() {}

        let id = next_ui_id();
        *pending().lock().unwrap() = Some(id);
        let ext = Rewind;
        assert_eq!(
            ext.on_ui_choice(
                id,
                Some(format!(
                    "{KEY_EDIT}  Rewind and restore input to the editor"
                ))
            ),
            None,
            "不得返回 follow-up 文本"
        );
        match take_pending_ui() {
            Some(ExtensionUiRequest::RewindLastUserInput {
                restore_to_editor: true,
            }) => {}
            other => panic!("应为 restore_to_editor=true 的 Rewind 请求：{other:?}"),
        }
        assert!(!take_pending(id), "确认后 pending 应清空");
    }

    /// 确认「回退并丢弃」→ `restore_to_editor = false`；取消 → 不发请求。
    #[test]
    fn confirm_discard_and_cancel() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while take_pending_ui().is_some() {}

        let id = next_ui_id();
        *pending().lock().unwrap() = Some(id);
        let _ = Rewind.on_ui_choice(id, Some(KEY_DISCARD.to_string()));
        match take_pending_ui() {
            Some(ExtensionUiRequest::RewindLastUserInput {
                restore_to_editor: false,
            }) => {}
            other => panic!("应为 restore_to_editor=false 的 Rewind 请求：{other:?}"),
        }

        // 取消：只清 pending，不发请求
        let id = next_ui_id();
        *pending().lock().unwrap() = Some(id);
        assert_eq!(Rewind.on_ui_choice(id, Some(KEY_CANCEL.to_string())), None);
        assert!(take_pending_ui().is_none(), "取消不应发请求");
    }

    /// id 不匹配（别的扩展的面板）不认领、不清 pending。
    #[test]
    fn foreign_panel_id_is_ignored() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while take_pending_ui().is_some() {}

        let id = next_ui_id();
        *pending().lock().unwrap() = Some(id);
        assert_eq!(
            Rewind.on_ui_choice(id + 1, Some(KEY_EDIT.to_string())),
            None
        );
        assert!(take_pending_ui().is_none());
        assert!(*pending().lock().unwrap() == Some(id), "pending 不应被消费");
        *pending().lock().unwrap() = None;
    }

    #[test]
    fn command_rejects_arguments_and_empty_sessions() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while take_pending_ui().is_some() {}

        let mut st = App::new();
        st.messages.push(AgentMessage::user_text("hi"));

        // 带参数：拒绝，不弹面板
        assert!(!command_rewind(&mut st, "rewind now"));
        assert!(take_pending_ui().is_none(), "带参数不应弹面板");

        // 忙碌：拒绝，不弹面板
        st.busy = true;
        assert!(!command_rewind(&mut st, "rewind"));
        assert!(take_pending_ui().is_none(), "忙碌时不应弹面板");
        st.busy = false;

        // 无 user 消息：拒绝，不弹面板
        let mut empty = App::new();
        assert!(!command_rewind(&mut empty, "rewind"));
        assert!(take_pending_ui().is_none(), "无 user 消息不应弹面板");
    }

    /// 空闲 + 有 user 消息：弹确认面板。
    #[test]
    fn command_opens_confirmation_when_idle_with_user_input() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while take_pending_ui().is_some() {}
        *pending().lock().unwrap() = None;

        let mut st = App::new();
        st.messages.push(AgentMessage::user_text("oops"));
        assert!(!command_rewind(&mut st, "rewind"));

        match take_pending_ui() {
            Some(ExtensionUiRequest::Select { options, .. }) => {
                assert_eq!(options.len(), 3);
            }
            other => panic!("应为确认面板：{other:?}"),
        }
        *pending().lock().unwrap() = None;
    }
}

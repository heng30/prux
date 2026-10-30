//! 扩展斜杠命令的宿主能力面与执行入口注册表。
//!
//! 命令正文只依赖 [`CommandHost`]：TUI（`App`）与桌面 GUI（`GuiApp`）各自实现，
//! 因此同一个 handler 在两个宿主上都能跑。碰面板、聊天区内部结构、worker 通道的
//! 命令用 [`CommandHost::as_any_mut`] 取回具体宿主（见 `tui_host`），取不到时给提示。

use super::{command_provider, ui::UiNotifyLevel};
use std::{
    any::Any,
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
};

/// 扩展命令执行入口：`/` 后完整原文（含命令名，如 `plan 分析`），返回是否退出宿主。
///
/// 参数是 [`CommandHost`] 而不是具体宿主，故 TUI 与 GUI 共用一个 handler。
pub type SlashCommandHandler = fn(&mut dyn CommandHost, &str) -> bool;

/// 执行入口的装箱形式：注册表存它，这样「只依赖 [`CommandHost`]」与
/// 「需要具体宿主 App」两类处理器都能放进同一张表（后者由宿主侧适配器包装）。
pub type BoxedHandler = Arc<dyn Fn(&mut dyn CommandHost, &str) -> bool + Send + Sync>;

/// 已注册的执行入口：`(扩展名, 命令名, handler)`，同键覆盖（幂等）。
static EXT_COMMAND_HANDLERS: OnceLock<Mutex<Vec<(String, String, BoxedHandler)>>> = OnceLock::new();

/// 扩展斜杠命令的宿主能力面：命令正文能对宿主做的事。
///
/// 判据：**任何宿主都能满足**的动作才收进来。需要面板/表单/聊天区内部结构的行为
/// 不在这里加方法，改成用 [`Self::as_any_mut`] 下转型取回具体宿主。
pub trait CommandHost {
    /// 往聊天区追加一条消息：`level` 决定呈现级别（TUI 走系统消息流，GUI 走 transcript）。
    fn push_message(&mut self, text: &str, level: UiNotifyLevel);

    /// 往聊天区追加一条 Markdown 消息（宿主不支持富文本时降级为普通消息）。
    fn push_markdown(&mut self, text: &str, level: UiNotifyLevel);

    /// 让宿主的停靠面板可见（扩展要展示自己那一段时用）。
    fn show_dock(&mut self);

    /// 宿主的停靠面板当前是否可见（命令做「显示 / 隐藏」三段式开关时用）。
    fn dock_visible(&self) -> bool;

    /// 打开某个扩展的设置面板（`/codemode`、`/vet settings` 这类「设置即界面」的命令用）。
    fn open_extension_settings(&mut self, ext: &str);

    /// 把文本作为用户消息发给 agent：空闲时开新回合，忙碌时并入当前回合。
    fn send_prompt(&mut self, text: &str);

    /// 当前选中的模型：`(provider, 模型 id)`，未确定时 `None`。
    fn current_model(&self) -> Option<(String, String)>;

    /// 切换当前模型（`persist` 由宿主的常规切换语义决定，命令只请求一次切换）。
    fn switch_model(&mut self, provider: &str, model_id: &str);

    /// 当前工作目录：命令解析相对路径、展示路径时用。
    fn cwd(&self) -> PathBuf;

    /// 是否正在流式输出：命令据此决定直接执行还是交给上层排队。
    fn is_busy(&self) -> bool;

    /// 在状态栏显示一条临时提示（`creating recap…` 这类进度文案）。
    fn set_status(&mut self, text: &str);

    /// 最近一条用户消息的正文（`/rewind` 的确认预览用）；没有则 `None`。
    fn last_user_message(&self) -> Option<String>;

    /// 输出 `/debug info` 的诊断信息（终端走 worker 回执，窗口走会话快照）。
    fn debug_info(&mut self);

    /// 输出当前系统提示词原文（`/debug system-prompt`）。
    fn debug_system_prompt(&mut self);

    /// 当前会话文件路径；内存会话（`--no-session`）为 `None`。
    fn session_path(&self) -> Option<String>;

    /// 最近若干条对话的纯文本摘录（`/btw`、`/recap` 要「带上下文再问一次」时用）。
    ///
    /// 每条一行、`role: 正文`，超过 `max_chars` 截断；没有对话时返回空串。
    fn transcript_excerpt(&self, max_messages: usize, max_chars: usize) -> String;

    /// 上转型逃生口：具体宿主类型（TUI 的 `App` / GUI 的 `GuiApp`）据此恢复完整能力。
    ///
    /// 只给「正文就是 TUI 面板/表单」的命令用；取不到时**必须**给出诚实提示，
    /// 不允许静默 no-op（用户点了没反应比报错更难查）。
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

/// 执行入口表（叶级锁；注册与查表都只持有这一把）。
fn handlers() -> &'static Mutex<Vec<(String, String, BoxedHandler)>> {
    EXT_COMMAND_HANDLERS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 注册「只依赖 [`CommandHost`]」的扩展命令（TUI 与 GUI 都能跑）。
///
/// 命令的"存在性"由已启用扩展的 [`super::Extension::commands`] 声明动态决定，
/// 本函数只绑定"谁来执行"：同一 `(ext, cmd)` 重复注册替换旧接线。
pub fn register_host_command(ext: &str, cmd: &str, handler: SlashCommandHandler) {
    register_command(ext, cmd, Arc::new(handler));
}

/// 注册任意执行入口（宿主侧适配器用，如把 `&mut App` 命令包成 [`BoxedHandler`]）。
pub fn register_command(ext: &str, cmd: &str, handler: BoxedHandler) {
    let mut table = handlers().lock().unwrap();
    table.retain(|(e, c, _)| e != ext || c != cmd);
    table.push((ext.to_string(), cmd.to_string(), handler));
}

/// 查询扩展命令执行入口：仅当命令由当前已启用扩展声明时返回。
///
/// 扩展禁用 / 模式不可用时 [`super::command_provider`] 返回 `None`，命令按未知处理。
pub fn command_handler(cmd: &str) -> Option<BoxedHandler> {
    let provider = command_provider(cmd)?;
    let table = handlers().lock().unwrap();
    table
        .iter()
        .find(|(ext, name, _)| ext == &provider && name == cmd)
        .map(|(_, _, handler)| Arc::clone(handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::extensions::{
        Extension, ExtensionCommand, register_extension, unregister_extension,
    };

    /// 记录宿主收到的消息，验证 handler 只依赖 [`CommandHost`]。
    struct Recorder {
        /// 收到的 `(text, level)`。
        messages: Vec<(String, UiNotifyLevel)>,
        /// 停靠面板是否被要求显示。
        dock: bool,
    }

    impl CommandHost for Recorder {
        fn push_message(&mut self, text: &str, level: UiNotifyLevel) {
            self.messages.push((text.to_string(), level));
        }

        fn push_markdown(&mut self, text: &str, level: UiNotifyLevel) {
            self.push_message(text, level);
        }

        fn show_dock(&mut self) {
            self.dock = true;
        }

        fn dock_visible(&self) -> bool {
            self.dock
        }

        fn open_extension_settings(&mut self, _ext: &str) {}

        fn send_prompt(&mut self, text: &str) {
            self.messages.push((text.to_string(), UiNotifyLevel::Info));
        }

        fn current_model(&self) -> Option<(String, String)> {
            None
        }

        fn last_user_message(&self) -> Option<String> {
            None
        }

        fn debug_info(&mut self) {}

        fn debug_system_prompt(&mut self) {}

        fn set_status(&mut self, text: &str) {
            self.messages.push((text.to_string(), UiNotifyLevel::Info));
        }

        fn session_path(&self) -> Option<String> {
            None
        }

        fn transcript_excerpt(&self, _max_messages: usize, _max_chars: usize) -> String {
            String::new()
        }

        fn switch_model(&mut self, _provider: &str, _model_id: &str) {}

        fn cwd(&self) -> PathBuf {
            PathBuf::from("/tmp")
        }

        fn is_busy(&self) -> bool {
            false
        }

        fn as_any_mut(&mut self) -> &mut dyn Any {
            self
        }
    }

    /// 声明了 `hostcmd` 并把它接到宿主无关 handler 上的测试扩展。
    struct HostCommandExt;

    impl Extension for HostCommandExt {
        fn name(&self) -> &str {
            "hostcmd-ext"
        }

        fn tools(&self) -> Vec<crate::core::extensions::ExtensionTool> {
            Vec::new()
        }

        fn commands(&self) -> Vec<ExtensionCommand> {
            vec![ExtensionCommand {
                name: "hostcmd".to_string(),
                description: "test".to_string(),
                busy_safe: true,
                subcommands: Vec::new(),
            }]
        }

        fn on_registered(&self) {
            register_host_command("hostcmd-ext", "hostcmd", run);
        }
    }

    /// 测试用 handler：把原文发成一条 Info 消息，并要求显示停靠面板。
    fn run(host: &mut dyn CommandHost, raw: &str) -> bool {
        host.push_message(raw, UiNotifyLevel::Info);
        host.show_dock();
        false
    }

    /// 注册后可查到；未注册的扩展（即"禁用"）查不到；卸载后也查不到。
    #[test]
    fn registered_handler_is_reachable_only_while_enabled() {
        let _g = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ad = crate::test_support::AgentDirGuard::temp();
        crate::core::extensions::set_extension_mode(crate::core::extensions::ExtensionMode::All);
        register_extension(HostCommandExt);
        let _ = crate::core::extensions::set_extension_enabled("hostcmd-ext", true);

        assert!(command_handler("hostcmd").is_some());
        assert!(command_handler("hostcmd-not-declared").is_none());

        let handler = command_handler("hostcmd").unwrap();
        let mut host = Recorder {
            messages: Vec::new(),
            dock: false,
        };
        assert!(!handler(&mut host, "/hostcmd hello"));
        assert_eq!(
            host.messages,
            vec![("/hostcmd hello".to_string(), UiNotifyLevel::Info)]
        );
        assert!(host.dock);

        assert!(unregister_extension("hostcmd-ext"));
        assert!(command_handler("hostcmd").is_none());
    }
}

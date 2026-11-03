//! `rtk` 扩展：用外部 [rtk](https://github.com/rtk-ai/rtk) 把 `bash` 命令改写成等价但更省上下文的写法。
//!
//! 每次 `bash` 调用前跑一次 `rtk rewrite <命令>`：rtk 判定有更紧凑的等价写法时把它打印在
//! stdout（退出码 `0` 或 `3`），否则不打印（退出码 `1`）——没有任何输出就照原命令执行。
//!
//! **全程 fail-open**：二进制不在 PATH 上、超时、输出为空、退出码不认，一律按原命令执行，
//! 绝不因为一个可选的输出压缩工具挡住真正的命令。唯一会打扰用户的是「启用了但没装」：
//! 启动/启用时检查一次，缺失就在聊天区报错并给出下载地址。

use crate::{
    core::{
        extensions::{
            BeforeToolCallOutcome, Extension, ExtensionHook, ExtensionMode, ExtensionTool,
            ForkProjectInfo, UiNotifyLevel,
        },
        provider::AgentMessage,
        session_manager::Session,
        tools::ToolError,
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_RTK, util::notify_plain},
    utils::process::{blocking, run_timed},
};
use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

/// 扩展名（`/extension` 面板里的标识）。
const EXT: &str = "rtk";

/// 外部二进制名（从 `PATH` 上找）。
const BIN: &str = "rtk";

/// 缺失时的下载地址（报错文案里给用户）。
const DOWNLOAD_URL: &str = "https://github.com/rtk-ai/rtk";

/// 单次调用的等待上限：rtk 自称 <10ms 开销，这里只为兜住卡死的二进制。
const TIMEOUT: Duration = Duration::from_secs(5);

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static RTK_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_RTK,
    make: || -> Arc<dyn Extension> { Arc::new(Rtk) },
};

/// 「已提示过缺失」标志：一个进程只报一次，装上之后自动复位。
static MISSING_WARNED: AtomicBool = AtomicBool::new(false);

/// 一次 `rtk rewrite` 的结果。
enum Rewrite {
    /// 改写成功，带上新命令。
    Rewritten(String),
    /// rtk 跑通了但认为这条命令没有紧凑等价写法。
    Skipped,
    /// 二进制起不来（没装 / 没有执行权限）。
    Unavailable,
}

/// 探测 `rtk` 在不在：只看**能不能起进程**（`rtk --version`）。
///
/// 起得来就算在——版本子命令非零退出只能说明这个二进制怪，不能说明它没装，
/// 那种情况留给 [`rewrite`] 的 fail-open 处理。起不来才可能是 PATH 里没有它。
fn probe() -> bool {
    let mut cmd = Command::new(BIN);
    cmd.arg("--version");
    run_timed(cmd, None, TIMEOUT).is_ok()
}

/// 检查环境里有没有 `rtk`；缺失时在聊天区报错并给出下载地址（只报一次）。
///
/// 返回 `true` 表示可用。可用时复位「已提示」标志，便于装好后重新提示。
fn ensure_available() -> bool {
    if probe() {
        MISSING_WARNED.store(false, Ordering::Relaxed);
        return true;
    }
    if !MISSING_WARNED.swap(true, Ordering::Relaxed) {
        notify_plain(
            format!(
                "`{EXT}` extension is enabled but `{BIN}` was not found in PATH (or it failed to start). \
                 Install it from {DOWNLOAD_URL} — until then bash commands run unchanged."
            ),
            UiNotifyLevel::Error,
        );
    }
    false
}

/// 跑一次 `rtk rewrite <command>`，按退出码解释结果。
fn rewrite(command: &str) -> Rewrite {
    let mut cmd = Command::new(BIN);
    cmd.arg("rewrite").arg(command);
    let out = match run_timed(cmd, None, TIMEOUT) {
        Ok(out) => out,
        Err(_) => return Rewrite::Unavailable,
    };

    // 0 = 改写了；3 = 改写了（旧版本用它表示成功）；其它一律当「没有更紧凑的写法」。
    let rewritten = out.stdout.trim();
    if matches!(out.code, Some(0) | Some(3)) && !rewritten.is_empty() && rewritten != command.trim()
    {
        return Rewrite::Rewritten(rewritten.to_string());
    }
    Rewrite::Skipped
}

/// `rtk` 扩展主体。
pub struct Rtk;

impl Extension for Rtk {
    /// 扩展名 `rtk`。
    fn name(&self) -> &str {
        EXT
    }

    /// 来源：移植自 zerostack 的 rtk 集成（`src/extras/rtk.rs`）。
    fn fork_project(&self) -> Option<ForkProjectInfo> {
        Some(ForkProjectInfo {
            plugin_name: "zerostack".to_string(),
            plugin_version: "1.8.5".to_string(),
            url: String::from("https://github.com/gi-dellav/zerostack"),
        })
    }

    /// 面板说明：把 bash 命令交给 rtk 改写以压缩工具输出。
    fn description(&self) -> &str {
        "Rewrite bash commands through the external `rtk` CLI so their output is more compact. \
         Fail-open: a missing or failing rtk never blocks the command. Requires rtk in PATH \
         (https://github.com/rtk-ai/rtk)"
    }

    /// 仅 `Dev` / `Creator` 模式可用：它会改变实际执行的命令。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 默认关闭：依赖外部二进制，属于用户显式开启的能力。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 不提供工具：只在 `bash` 调用前介入。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 只关心工具调用前。
    fn hooks(&self) -> Vec<ExtensionHook> {
        vec![ExtensionHook::BeforeToolCallAsync]
    }

    /// 启用时检查环境，缺失就报错并给出下载地址。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            ensure_available();
        }
    }

    /// 会话开始时再检查一次：启动期注册时 UI 可能还没就绪，错过了提示就补上。
    fn on_session_start(&self, _cwd: &str, _messages: &[AgentMessage], _session: Option<&Session>) {
        ensure_available();
    }

    /// `bash` 调用前把命令换成 rtk 的紧凑等价写法；其它工具、任何失败都原样放行。
    fn before_tool_call_async(
        &self,
        name: String,
        args: Value,
        _assistant_message: Option<AgentMessage>,
        _context: Vec<AgentMessage>,
        _all_args: Value,
    ) -> BoxFuture<'static, std::result::Result<BeforeToolCallOutcome, ToolError>> {
        Box::pin(async move {
            if name != "bash" {
                return Ok(BeforeToolCallOutcome::allow());
            }
            let Some(command) = args.get("command").and_then(|v| v.as_str()) else {
                return Ok(BeforeToolCallOutcome::allow());
            };
            if command.trim().is_empty() {
                return Ok(BeforeToolCallOutcome::allow());
            }

            let original = command.to_string();
            let outcome = blocking(move || rewrite(&original));

            let rewritten = match outcome {
                Rewrite::Rewritten(command) => command,
                Rewrite::Skipped => return Ok(BeforeToolCallOutcome::allow()),
                Rewrite::Unavailable => {
                    // 二进制起不来：报一次错（带下载地址），本次照原命令执行。
                    ensure_available();
                    return Ok(BeforeToolCallOutcome::allow());
                }
            };

            let mut args = args;
            if let Some(obj) = args.as_object_mut() {
                obj.insert("command".to_string(), Value::String(rewritten));
            }
            Ok(BeforeToolCallOutcome {
                block: false,
                reason: None,
                terminate: false,
                args: Some(args),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 独立 runtime 跑 async 测试体（不与其它测试共享全局 runtime）。
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(fut)
    }

    /// `/extension` 二级详情要展示「移植自 zerostack v1.8.5」及项目 URL。
    #[test]
    fn fork_project_reports_zerostack_source() {
        let info = Rtk.fork_project().expect("rtk 应声明 fork 来源");
        assert_eq!(info.plugin_name, "zerostack");
        assert_eq!(info.url, "https://github.com/gi-dellav/zerostack");
    }

    /// 非 `bash` 工具、以及缺 `command` 的调用一律原样放行，不碰外部二进制。
    #[test]
    fn leaves_other_tools_alone() {
        let rtk = Rtk;
        let out = block_on(rtk.before_tool_call_async(
            "read".to_string(),
            json!({ "path": "x" }),
            None,
            Vec::new(),
            json!({}),
        ))
        .unwrap();
        assert!(!out.block);
        assert!(out.args.is_none());

        let out = block_on(rtk.before_tool_call_async(
            "bash".to_string(),
            json!({}),
            None,
            Vec::new(),
            json!({}),
        ))
        .unwrap();
        assert!(out.args.is_none());
    }
}

//! `sandbox` 扩展：把模型提的 shell 命令关进 **OS 级**沙箱。
//!
//! 与 [`crate::extensions::permissions`] 的分工：那个是**执行前的静态判定**（按规则与参数
//! 形状拦命令，拿不准就 fail-closed），它挡不住动态构造的命令；本扩展是**内核层的边界**——
//! 命令照跑，但文件系统写权限由内核按红线裁决。
//!
//! 后端（运行期探测，不由用户选）：
//!
//! | 平台 | 程序 | 报告名 |
//! |---|---|---|
//! | Linux | PATH 上的 `bwrap`（bubblewrap） | `linux-bwrap` |
//! | macOS | `/usr/bin/sandbox-exec`（Seatbelt） | `macos-seatbelt` |
//!
//! 边界（两个后端一致）：整棵文件系统**只读**，只有工作目录与临时目录可写，网络放行。
//! 因此 `cargo build` 写不了 `~/.cargo`、`npm` 写不了 `~/.npm` —— 这是刻意的：
//! 想让它写，就把缓存目录挪进工作目录或设成环境变量指到工作目录里。
//!
//! **失败即拒绝**：扩展启用时先探测后端；探测失败**不会**退回无沙箱执行，而是装上一个
//! 「一律拒绝启动」的包装器——命令直接报出原因，用户看得见，也就能决定要不要关掉本扩展。
//! 中途后端被卸载同理（每次启动都重新拼命令行，拿不到就 `Err`）。
//!
//! 生效范围：只覆盖 `bash` 工具（含扩展经它发起的命令）。`powershell`（Windows）没有
//! OS 级后端，Windows 上本扩展恒为「不可用」。`git` / `grep` 这类工具自己起的小命令不经过
//! 沙箱——它们不接受模型给的任意命令行，也不是这条威胁模型的入口。
//!
//! **默认不启用**：沙箱会让一部分正常命令失败，什么时候付这个代价由用户决定。

use crate::{
    core::{
        extensions::{
            CommandHost, Extension, ExtensionCommand, ExtensionMode, ExtensionTool, UiNotifyLevel,
        },
        sandbox::{self, SandboxedCommand},
    },
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_SANDBOX, util::notify_text},
    modes::tui::handlers::register_host_command,
    utils::{
        paths::{cwd, find_in_path},
        process,
    },
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// 扩展名（也是 `/extension` 面板标识与斜杠命令名）。
const EXT: &str = "sandbox";
/// 斜杠命令名。
const CMD: &str = "sandbox";
/// 探测时用来验证沙箱真能起进程的最小命令。
const PROBE_CMD: &str = "true";

/// 自声明工厂：linkme 分布式切片。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static SANDBOX_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_SANDBOX,
    make: || -> Arc<dyn Extension> { Arc::new(Sandbox) },
};

/// 最近一次激活的结论一句话（`/sandbox` 展示；`None` = 尚未激活）。
static STATUS: Mutex<Option<String>> = Mutex::new(None);

/// 可用的沙箱后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backend {
    /// Linux bubblewrap（`bwrap`）。
    Bwrap,
    /// macOS Seatbelt（`sandbox-exec`）。
    Seatbelt,
}

impl Backend {
    /// 状态展示用的后端名。
    fn label(self) -> &'static str {
        match self {
            Backend::Bwrap => "linux-bwrap",
            Backend::Seatbelt => "macos-seatbelt",
        }
    }

    /// 启动包装器用的固定参数（真正的 shell 与 `-c <命令>` 由调用方接在后面）。
    ///
    /// `cwd` 是本次调用的工作目录：它是唯一的可写路径，因此每次调用都重新拼。
    fn args(self, cwd: &Path) -> Vec<String> {
        let cwd = cwd.display().to_string();
        match self {
            // ro-bind 必须排在这些挂载之前：bwrap 按参数顺序叠挂载，晚挂的 `/` 会把先挂的
            // `/dev`、`/proc`、`/tmp` 一起盖掉，命令照样能跑，但 `/tmp` 写不了、`/dev/null` 直接 EACCES。
            Backend::Bwrap => vec![
                "--die-with-parent".to_string(),
                "--ro-bind".to_string(),
                "/".to_string(),
                "/".to_string(),
                "--dev-bind".to_string(),
                "/dev".to_string(),
                "/dev".to_string(),
                "--proc".to_string(),
                "/proc".to_string(),
                "--tmpfs".to_string(),
                "/tmp".to_string(),
                // 后挂的 bind 覆盖前面的 ro-bind，工作目录因此可写。
                "--bind".to_string(),
                cwd.clone(),
                cwd.clone(),
                "--chdir".to_string(),
                cwd,
            ],
            Backend::Seatbelt => vec!["-p".to_string(), seatbelt_profile(cwd.as_str())],
        }
    }
}

/// 探测结果。
struct Probe {
    /// 探测成功的后端。
    backend: Backend,
    /// 后端可执行文件的绝对路径。
    program: PathBuf,
}

/// macOS Seatbelt 策略：只读全局、仅 `cwd` 与系统临时目录可写、网络放行。
fn seatbelt_profile(cwd: &str) -> String {
    format!(
        "(version 1)\n(deny default)\n(allow process*)\n(allow sysctl-read)\n(allow mach-lookup)\n\
         (allow ipc-posix*)\n(allow network*)\n(allow file-read*)\n\
         (allow file-write* (subpath \"{}\") (subpath \"/private/tmp\") \
         (subpath \"/private/var/folders\") (subpath \"/dev\"))\n",
        cwd.replace('\\', "\\\\").replace('"', "\\\"")
    )
}

/// 探测可用后端：找程序 → 用它真跑一次 [`PROBE_CMD`]（用户命名空间被禁时 bwrap 会在这里失败）。
fn probe(cwd: &Path) -> Result<Probe, String> {
    let (backend, name, path) = if cfg!(target_os = "macos") {
        let path = PathBuf::from("/usr/bin/sandbox-exec");
        path.exists()
            .then_some((Backend::Seatbelt, "sandbox-exec", path))
    } else if cfg!(target_os = "linux") {
        find_in_path("bwrap").map(|p| (Backend::Bwrap, "bwrap", p))
    } else {
        None
    }
    .ok_or_else(|| {
        if cfg!(target_os = "linux") {
            "bwrap (bubblewrap) was not found on PATH".to_string()
        } else if cfg!(target_os = "macos") {
            "/usr/bin/sandbox-exec is missing".to_string()
        } else {
            "no OS-level sandbox backend exists on this platform".to_string()
        }
    })?;

    let mut cmd = std::process::Command::new(&path);
    cmd.args(backend.args(cwd));
    cmd.arg("/bin/sh").arg("-c").arg(PROBE_CMD);
    process::run_stdout(cmd, &format!("{name} probe"))
        .map_err(|e| format!("{name} is installed but cannot start a sandboxed process: {e}"))?;

    Ok(Probe {
        backend,
        program: path,
    })
}

/// 用探测成功的后端装一个真正的包装器。
fn install_backend(probe: &Probe) {
    let (backend, program) = (probe.backend, probe.program.clone());
    sandbox::install(Arc::new(move |cwd: &Path| {
        Ok(SandboxedCommand {
            program: program.display().to_string(),
            prefix_args: backend.args(cwd),
        })
    }));
}

/// 装一个「一律拒绝启动」的包装器：后端不可用时宁可命令失败，也不退回无沙箱执行。
fn install_refusal(reason: &str) {
    let reason = format!(
        "Sandbox refused to start the command: {reason}. \
         Fix the backend or turn the `sandbox` extension off to run commands unsandboxed."
    );
    sandbox::install(Arc::new(move |_: &Path| Err(reason.clone())));
}

/// 写状态。锁内只赋字符串，不回调任何东西。
fn set_status(text: String) {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
}

/// 读状态（克隆出来再展示）。
fn status() -> Option<String> {
    STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// 按当前 cwd 激活：探测成功装真包装器，失败装拒绝式包装器。
fn activate() {
    let cwd = cwd();
    match probe(&cwd) {
        Ok(probe) => {
            let line = format!(
                "on · {} ({}) · writable: {} · network: allowed",
                probe.backend.label(),
                probe.program.display(),
                cwd.display()
            );
            install_backend(&probe);
            set_status(line.clone());
            notify_text(&format!("Command sandbox {line}"), UiNotifyLevel::Success);
        }
        Err(reason) => {
            install_refusal(&reason);
            set_status(format!("unavailable · {reason}"));
            notify_text(
                &format!("Command sandbox unavailable: {reason} — commands will be refused"),
                UiNotifyLevel::Warning,
            );
        }
    }
}

/// 关闭沙箱扩展时撤销包装器（命令回到无沙箱执行）。
fn deactivate() {
    sandbox::clear();
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// OS 级命令沙箱扩展（`/sandbox`）。
pub struct Sandbox;

impl Extension for Sandbox {
    /// 返回固定扩展名 `sandbox`。
    fn name(&self) -> &str {
        EXT
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Run model-issued shell commands inside an OS sandbox (bwrap / sandbox-exec)"
    }

    /// 默认关闭：沙箱会让写缓存的命令失败，是否付这个代价由用户决定。
    fn default_enabled(&self) -> bool {
        false
    }

    /// 仅在 Dev / Creator 模式可用（与 permissions / leak-guard 等安全层扩展同口径）。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Dev, ExtensionMode::Creator]
    }

    /// 不提供工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 声明 `/sandbox` 斜杠命令（只读状态展示，故 busy-safe）。
    fn commands(&self) -> Vec<ExtensionCommand> {
        vec![ExtensionCommand {
            name: CMD.to_string(),
            description: "/sandbox - show the command sandbox backend and its boundaries"
                .to_string(),
            busy_safe: true,
            subcommands: Vec::new(),
        }]
    }

    /// 注册 `/sandbox` 的命令处理器。
    fn on_registered(&self) {
        register_host_command(EXT, CMD, command_sandbox);
    }

    /// 启用时探测并装上包装器，禁用时撤销——沙箱只在这段时间内生效。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            activate();
        } else {
            deactivate();
        }
    }
}

/// `/sandbox`：打印当前沙箱状态与边界；未被启用过时明确说「未生效」。
fn command_sandbox(host: &mut dyn CommandHost, _raw: &str) -> bool {
    let text = match status() {
        Some(line) => format!(
            "Command sandbox: {line}\n\
             Every `bash` call runs inside it: the filesystem is read-only except the working \
             directory and the temporary directory; the network stays open.\n\
             If the backend stops working, commands are refused instead of running unsandboxed. \
             Turn the extension off in /extension to disable it."
        ),
        None => "Command sandbox is not active. Enable the `sandbox` extension in /extension."
            .to_string(),
    };
    host.push_message(&text, UiNotifyLevel::Info);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    /// bwrap 参数必须包含「全局只读 + cwd 可写 + chdir」，且顺序上 bind 在 ro-bind 之后。
    #[test]
    fn bwrap_args_bind_workspace_after_readonly_root() {
        let args = Backend::Bwrap.args(Path::new("/work/repo"));
        let ro = args.iter().position(|a| a == "--ro-bind").unwrap();
        let bind = args.iter().position(|a| a == "--bind").unwrap();
        assert!(bind > ro, "{args:?}");

        // 反过来则 `/` 会盖掉先挂的 /dev、/proc、/tmp：/tmp 不可写、/dev/null EACCES。
        for flag in ["--dev-bind", "--proc", "--tmpfs"] {
            let pos = args.iter().position(|a| a == flag).unwrap();
            assert!(pos > ro, "{flag} 必须在 --ro-bind / / 之后: {args:?}");
        }
        assert_eq!(&args[bind + 1..bind + 3], ["/work/repo", "/work/repo"]);
        assert_eq!(args[args.len() - 2], "--chdir");
        assert_eq!(args[args.len() - 1], "/work/repo");
    }

    /// Seatbelt 策略里工作目录是唯一放开的写路径之一，引号必须转义（否则策略解析失败）。
    #[test]
    fn seatbelt_profile_escapes_and_allows_cwd() {
        let profile = seatbelt_profile("/tmp/we\"ird");
        assert!(profile.contains("(deny default)"));
        assert!(profile.contains("(subpath \"/tmp/we\\\"ird\")"));
        assert!(profile.contains("(allow file-read*)"));
    }

    /// 拒绝式包装器必须给出原因，且不得变成「放行」。
    #[test]
    fn refusal_wrapper_always_errors() {
        install_refusal("bwrap was not found on PATH");
        let err = sandbox::current().expect("wrapper")(Path::new("/work"))
            .expect_err("refusal must be an error");
        assert!(err.contains("bwrap was not found on PATH"), "{err}");
        assert!(err.contains("refused to start"), "{err}");
        sandbox::clear();
    }
}

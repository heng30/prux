//! 候选浏览器的探测与启动：配置端口是否在监听、候选浏览器是否已经在跑、
//! 挑一个空闲端口把它带调试端口地拉起来，并等调试端点就绪。
//!
//! # 为什么要有「候选浏览器」这一层
//!
//! CDP 端点只在浏览器**启动时**带 `--remote-debugging-port` 才会有。用户自己开的浏览器窗口
//! （没有这个参数）永远开不出端点，而带着这个参数再启动一次也拿不到端口——浏览器会把自己
//! 当成第二个实例，把命令行转交给已有实例后退出。所以自动启动只能在「该浏览器当前没在跑」
//! 时进行，这正是 [`ProcessSnapshot::state_of`] 要回答的问题。
//!
//! # 进程匹配是启发式的
//!
//! 判定「某个候选浏览器在跑」看进程的两个身份名字（去掉目录、引号、`.exe` 后小写）：内核给的
//! 可执行文件名，与命令行第 0 个参数的文件名——**任一**命中该候选的进程名别名即算。不匹配其余
//! 参数：`grep browser` 这种命令行里也会出现候选的名字，而那不是浏览器。Chromium 系会派生
//! 渲染 / GPU 子进程，它们复用同一个可执行文件却**不带**调试端口，所以判定分两步：命中任何
//! 进程 = 在跑；命中的进程里带 `--remote-debugging-port` = 有可用端口。
//!
//! 别名表里同一族的浏览器会共享名字（`chrome` 既是 Google Chrome 也是部分发行版的 Chromium
//! 真身），所以「Chrome 在跑」也会让 Chromium 候选判为在跑——两者确实没法用同一个配置文件
//! 同时开两个实例，跳过是正确结果。误判的后果只有两种：跳过该候选（少一次自动启动），
//! 或对一个连不上的端口报错（`cdp::discover` 仍会拒绝非 DevTools 端点），不会连到别的浏览器上。
//!
//! 内置列表全是 Chromium 系：Firefox 141 起把 CDP 连 `remote.active-protocols` 一起删了
//! （只剩 WebDriver BiDi），140 ESR 虽然有 CDP 但默认不启用（要 profile 里开 pref），所以它
//! 不进默认候选；要用就自己写进 `browsers`，启动器会按家族给它 `--no-remote --new-instance`，
//! 等不到端点时报错也会点名原因（见 [`cdp_hint`] 与 [`launch_switches`]）。

use super::cdp::{self, Endpoint};
use crate::utils::env;
use std::{
    process::Stdio,
    time::{Duration, Instant},
};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

/// 等调试端点就绪时的轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 等调试端点就绪时单次 `cdp::discover` 的上限（浏览器刚起来时连不上是常态，别一次耗完预算）。
const DISCOVER_ATTEMPT: Duration = Duration::from_secs(2);

/// Chromium 系（Chrome / Chromium / Edge / Brave）的启动开关：跳过首次运行向导与默认浏览器检查。
const CHROMIUM_SWITCHES: &[&str] = &["--no-first-run", "--no-default-browser-check"];

/// Firefox 的启动开关：`--no-remote --new-instance` 保证哪怕已有实例在跑也另起一个进程
/// （否则它只会把命令行转交给已有实例）。它不认 Chromium 的那两个开关。
const FIREFOX_SWITCHES: &[&str] = &["--no-remote", "--new-instance"];

/// Linux 的默认候选浏览器（按顺序尝试；不含 Firefox，理由见模块文档与 [`cdp_hint`]）。
#[cfg(target_os = "linux")]
const BUILTIN: &[Builtin] = &[
    Builtin {
        program: "google-chrome",
        process_names: &["google-chrome", "chrome"],
    },
    Builtin {
        program: "google-chrome-stable",
        process_names: &["google-chrome-stable", "chrome"],
    },
    Builtin {
        program: "chromium",
        process_names: &["chromium", "chrome"],
    },
    Builtin {
        program: "chromium-browser",
        process_names: &["chromium-browser", "chromium", "chrome"],
    },
    Builtin {
        program: "microsoft-edge",
        process_names: &["microsoft-edge", "msedge"],
    },
    Builtin {
        program: "brave-browser",
        process_names: &["brave-browser", "brave"],
    },
    Builtin {
        program: "brave",
        process_names: &["brave", "brave-browser"],
    },
];

/// macOS 的默认候选浏览器（`.app` 内的可执行文件路径，按顺序尝试；不含 Firefox）。
#[cfg(target_os = "macos")]
const BUILTIN: &[Builtin] = &[
    Builtin {
        program: "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        process_names: &["google chrome", "chrome"],
    },
    Builtin {
        program: "/Applications/Chromium.app/Contents/MacOS/Chromium",
        process_names: &["chromium", "chrome"],
    },
    Builtin {
        program: "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
        process_names: &["microsoft edge", "msedge"],
    },
    Builtin {
        program: "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
        process_names: &["brave browser", "brave"],
    },
];

/// Windows 的默认候选浏览器（`${VAR}` 由 [`spawn_detached`] 展开，按顺序尝试；不含 Firefox）。
#[cfg(target_os = "windows")]
const BUILTIN: &[Builtin] = &[
    Builtin {
        program: "${ProgramFiles}\\Google\\Chrome\\Application\\chrome.exe",
        process_names: &["chrome"],
    },
    Builtin {
        program: "${ProgramFiles(x86)}\\Google\\Chrome\\Application\\chrome.exe",
        process_names: &["chrome"],
    },
    Builtin {
        program: "${LOCALAPPDATA}\\Google\\Chrome\\Application\\chrome.exe",
        process_names: &["chrome"],
    },
    Builtin {
        program: "${ProgramFiles(x86)}\\Microsoft\\Edge\\Application\\msedge.exe",
        process_names: &["msedge"],
    },
    Builtin {
        program: "${ProgramFiles}\\BraveSoftware\\Brave-Browser\\Application\\brave.exe",
        process_names: &["brave"],
    },
];

/// 其它平台的默认候选浏览器：没有已知的启动路径，只能手工配置 `browsers`。
#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
const BUILTIN: &[Builtin] = &[
    Builtin {
        program: "google-chrome",
        process_names: &["google-chrome", "chrome"],
    },
    Builtin {
        program: "chromium",
        process_names: &["chromium", "chrome"],
    },
];

/// 一个内置候选浏览器。
struct Builtin {
    /// 启动命令：PATH 里的命令名（macOS / Windows 上是绝对路径，可含 `${VAR}`）。
    program: &'static str,
    /// 进程名别名：进程的可执行文件名命中其中一项，就认为该浏览器在跑。
    process_names: &'static [&'static str],
}

/// 默认候选浏览器启动命令（配置里 `browsers` 省略时用它，配置给了就以配置为准）。
pub fn default_browsers() -> Vec<String> {
    BUILTIN
        .iter()
        .map(|builtin| builtin.program.to_string())
        .collect()
}

/// 候选浏览器的进程名别名：内置项取手工维护的别名表，自定义项取命令名本身。
///
/// 别名已规范化（小写、无目录、无 `.exe`），可直接与 [`ProcessInfo::exe`] 比较；
/// 空白的自定义项会先 `trim`。
pub fn process_names(program: &str) -> Vec<String> {
    let trimmed = program.trim();
    match BUILTIN.iter().find(|builtin| builtin.program == trimmed) {
        Some(builtin) => builtin
            .process_names
            .iter()
            .map(|name| (*name).to_string())
            .collect(),
        None => vec![basename(trimmed)],
    }
}

/// 启动某个候选时要附加的开关（两类浏览器各一套，见 [`CHROMIUM_SWITCHES`] / [`FIREFOX_SWITCHES`]）。
///
/// 按进程名别名里有没有 `firefox*` 判定：自定义的 `browsers` 项（例如 `/opt/ff/firefox`）
/// 也能因此拿到对的开关，而不是 Chromium 的。
fn launch_switches(program: &str) -> &'static [&'static str] {
    let is_firefox = process_names(program)
        .iter()
        .any(|name| name.starts_with("firefox"));

    if is_firefox {
        FIREFOX_SWITCHES
    } else {
        CHROMIUM_SWITCHES
    }
}

/// 候选浏览器当前的运行状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Running {
    /// 没有同名进程在跑：现在启动会得到一个独立实例，能拿到调试端口。
    Absent,
    /// 已经在跑，但拿不到可连的调试端口（没带 `--remote-debugging-port`，或用的是
    /// `--remote-debugging-pipe`）：再启动只会把命令行转交给已有实例，跳过该候选。
    NoDebugPort,
    /// 已经在跑且带着 `--remote-debugging-port`：直接试这些端口（可能不是配置里的那个）。
    DebugPorts(Vec<u16>),
}

/// 系统进程表的一次快照。
///
/// 一次连接尝试里逐个候选判定共用同一个快照：既是省掉重复扫描，也保证「谁在跑」是同一个
/// 时刻的观察结果。
pub struct ProcessSnapshot {
    /// 快照里的全部进程。
    processes: Vec<ProcessInfo>,
}

impl ProcessSnapshot {
    /// 扫一次系统进程表（拿不到的信息退化成空串，不会因此报错）。
    pub fn scan() -> Self {
        let Ok(mut system) = System::new() else {
            return Self {
                processes: Vec::new(),
            };
        };

        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_cmd(UpdateKind::Always)
                .with_exe(UpdateKind::Always),
        );

        Self {
            processes: system.processes().values().map(process_info).collect(),
        }
    }

    /// 判定候选浏览器当前的运行状态（见 [`Running`]）。
    pub fn state_of(&self, program: &str) -> Running {
        classify(&self.processes, &process_names(program))
    }
}

/// 一条进程记录：它的两个身份名字（内核给的可执行文件、命令行第 0 个参数）+ 完整命令行。
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProcessInfo {
    /// 可执行文件名（已规范化：小写、无目录、无 `.exe`），来自内核给的真实二进制路径。
    exe: String,
    /// 命令行第 0 个参数的文件名（同样已规范化），即进程自称的名字。
    argv0: String,
    /// 完整命令行，用于找 `--remote-debugging-port`。
    command_line: String,
}

impl ProcessInfo {
    /// 本进程是否可能叫 `name`：`exe` 或 `argv0` 命中即算。
    ///
    /// 两个都得看，因为哪一个都可能不准：NixOS 的 coreutils、busybox 这类**多合一二进制**
    /// 的 `exe` 是 `coreutils`，真名（`sleep`、`chromium`…）只在 `argv0` 里；反过来没
    /// `exec` 的 shell 包装脚本里 `argv0` 是脚本名，`exe` 才是真身。只看这两个位置，
    /// 不看其余参数——`grep browser` 那种命令行不该命中。
    fn is_named(&self, name: &str) -> bool {
        self.exe == name || self.argv0 == name
    }
}

/// 把一条 `sysinfo` 的进程记录转成 [`ProcessInfo`]；进程表由 [`ProcessSnapshot::scan`] 读
/// （`sysinfo` 的跨平台实现：Linux 读 `/proc`、macOS 走 `libproc` / `sysctl`、Windows 走
/// `NtQuerySystemInformation`，不依赖 `ps` / PowerShell 是否存在）。
///
/// `exe()` 可能拿不到（Linux 上跨 UID 命名空间读不了 `/proc/<pid>/exe`、或进程已无权限），
/// 那时退化成空串；`argv0` 取不到（内核线程没有命令行）时同样退化成空串。
fn process_info(process: &sysinfo::Process) -> ProcessInfo {
    let name_of = |value: Option<&std::ffi::OsStr>| {
        value
            .map(|value| basename(&value.to_string_lossy()))
            .unwrap_or_default()
    };

    ProcessInfo {
        exe: name_of(process.exe().map(|path| path.as_os_str())),
        argv0: name_of(process.cmd().first().map(|arg| arg.as_os_str())),
        command_line: process
            .cmd()
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" "),
    }
}

/// 在已读到的进程列表上做判定（与 IO 分离，便于单测）。
fn classify(processes: &[ProcessInfo], names: &[String]) -> Running {
    let mut found = false;
    let mut ports: Vec<u16> = Vec::new();

    for process in processes {
        if !names.iter().any(|name| process.is_named(name)) {
            continue;
        }
        found = true;
        if let Some(port) = debug_port(&process.command_line)
            && !ports.contains(&port)
        {
            ports.push(port);
        }
    }

    if !ports.is_empty() {
        Running::DebugPorts(ports)
    } else if found {
        Running::NoDebugPort
    } else {
        Running::Absent
    }
}

/// 取命令行里的 `--remote-debugging-port=<port>`；没有或值不是端口时返回 `None`。
fn debug_port(command_line: &str) -> Option<u16> {
    command_line.split_whitespace().find_map(|token| {
        token
            .trim_matches(|c| c == '"' || c == '\'')
            .strip_prefix("--remote-debugging-port=")
            .and_then(|value| value.parse::<u16>().ok())
    })
}

/// 取参数的文件名部分：去引号、去掉目录（`/` 与 `\`）、小写、去掉 `.exe` 后缀。
fn basename(token: &str) -> String {
    let token = token.trim_matches(|c| c == '"' || c == '\'');
    let name = token.rsplit(['/', '\\']).next().unwrap_or(token);
    let name = name.to_ascii_lowercase();
    match name.strip_suffix(".exe") {
        Some(stripped) => stripped.to_string(),
        None => name,
    }
}

/// 启动候选浏览器：带 `--remote-debugging-port=<port>` 与家族对应的开关（见 [`launch_switches`]），
/// 标准流全部丢弃、脱离 prux 的进程组。
///
/// 因此它既不会跟着 prux 退出一起消失（浏览器留在桌面上给用户用），也不会把 prux 的
/// `Ctrl-C` / 窗口关闭传给它自己。子进程由后台任务回收，本扩展不负责结束它。
///
/// 返回子进程 pid，仅用于报错时说明启动了什么。起不来（命令不存在、没有显示环境等）返回 `Err`。
pub async fn spawn_detached(program: &str, port: u16) -> Result<u32, String> {
    let program = env::expand(program.trim());
    let mut cmd = tokio::process::Command::new(&program);
    cmd.arg(format!("--remote-debugging-port={port}"))
        .args(launch_switches(&program))
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    #[cfg(unix)]
    cmd.process_group(0);

    #[cfg(windows)]
    {
        /// Windows：不继承控制台。
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        /// Windows：新建进程组，`Ctrl-C` 不会广播给它。
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        cmd.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("browser: cannot start `{program}`: {e}"))?;
    let pid = child
        .id()
        .ok_or_else(|| format!("browser: `{program}` exited right after being started"))?;

    tokio::spawn(async move {
        _ = child.wait().await;
    });
    Ok(pid)
}

/// 已知**拿不到** CDP 端点的候选的额外说明；其它浏览器返回 `None`。
///
/// Firefox 141 起把 Remote Agent 收成只提供 WebDriver **BiDi**：`--remote-debugging-port`
/// 能起来端点，但 `/json/version`、`/json/list` 全是 404（实测 155 如此，且显式写
/// `remote.active-protocols = 1` 也没用——该 pref 已被删）；140 ESR 还有 CDP，但要 profile 里
/// 把 `remote.active-protocols` 开到含 CDP 才会应答（默认 404）。所以 Firefox 不在默认候选里，
/// 用户手工写进 `browsers` 时，等不到端点就把原因写进报错，免得以为是端口配错了。
pub fn cdp_hint(program: &str) -> Option<&'static str> {
    let is_firefox = process_names(program)
        .iter()
        .any(|name| name.starts_with("firefox"));
    is_firefox.then_some(
        "Firefox's remote agent serves WebDriver BiDi only, not the CDP `/json/version` endpoint, \
         so it cannot be driven by this tool; use a Chromium-based browser or an older Firefox \
         whose remote agent still speaks CDP",
    )
}

/// 轮询调试端点直到就绪，或 `budget` 用完。
///
/// 浏览器起来到调试端口可用之间有几百毫秒的间隙，所以这里必须重试；
/// 预算用完返回 `Err`（文案里带上端口，调用方会再补上是哪个候选没起来）。
pub async fn wait_for_endpoint(
    host: &str,
    port: u16,
    budget: Duration,
) -> Result<Endpoint, String> {
    let deadline = Instant::now() + budget;

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(format!(
                "its debug endpoint at {host}:{port} did not come up within {}s",
                budget.as_secs()
            ));
        }

        match cdp::discover(host, port, remaining.min(DISCOVER_ATTEMPT)).await {
            Ok(endpoint) => return Ok(endpoint),
            Err(_) => tokio::time::sleep(POLL_INTERVAL).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一条「`exe` 与 `argv0` 同名」的进程记录：`name` 是规范化后的名字，`line` 是完整命令行。
    fn process(name: &str, line: &str) -> ProcessInfo {
        ProcessInfo {
            exe: name.to_string(),
            argv0: name.to_string(),
            command_line: line.to_string(),
        }
    }

    /// 当前平台第一个内置候选的进程名别名。
    ///
    /// 不写死 `google-chrome`：那是 Linux 的程序名，Windows 的候选是
    /// `${ProgramFiles}\Google\Chrome\Application\chrome.exe`，别名只有 `chrome`，
    /// 写死了就只能一路判成 `Absent`。
    fn builtin_aliases() -> Vec<String> {
        let program = default_browsers()
            .into_iter()
            .next()
            .expect("内置候选列表非空");
        process_names(&program)
    }

    /// 内置候选列表非空，且每项都能算出进程名别名。
    #[test]
    fn builtin_candidates_are_listed_with_aliases() {
        let browsers = default_browsers();
        assert!(!browsers.is_empty());
        for program in &browsers {
            let names = process_names(program);
            assert!(!names.is_empty(), "{program} 应有进程名别名");
            assert!(
                names.iter().all(|name| name == &basename(name)),
                "{program} 的别名应已规范化（小写、无目录、无 .exe）：{names:?}"
            );
        }
    }

    /// 内置项用别名表（`google-chrome` 的进程名是 `chrome`），自定义项退回命令名本身。
    #[test]
    fn aliases_come_from_the_table_or_the_command_name() {
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert!(process_names("google-chrome").contains(&"chrome".to_string()));
        assert_eq!(
            process_names("/opt/weird/MyBrowser.exe"),
            vec!["mybrowser".to_string()]
        );
        assert_eq!(process_names("firefox"), vec!["firefox".to_string()]);
    }

    /// 别名比较忽略目录、引号、大小写与 `.exe`。
    #[test]
    fn basename_normalises_paths_and_quotes() {
        assert_eq!(
            basename("\"C:\\Program Files\\Google\\Chrome\\chrome.exe\""),
            "chrome"
        );
        assert_eq!(
            basename("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            "google chrome"
        );
        assert_eq!(basename("Chrome"), "chrome");
    }

    /// `--remote-debugging-port=` 能被读出来；没带或值非法时为 `None`。
    #[test]
    fn debug_port_is_read_from_the_command_line() {
        assert_eq!(
            debug_port("/opt/google/chrome/chrome --type=renderer --remote-debugging-port=9333"),
            Some(9333)
        );
        assert_eq!(debug_port("chrome about:blank"), None);
        assert_eq!(
            debug_port("chrome --remote-debugging-port=not-a-port"),
            None
        );
    }

    /// 判定：只有渲染子进程（没端口）算「在跑但没端口」，主进程带端口则给出端口列表。
    #[test]
    fn classify_distinguishes_child_processes_from_the_owner() {
        let names = builtin_aliases();
        let alias = names[0].clone();

        let renderer_only = vec![process(&alias, "chrome --type=renderer")];
        assert_eq!(classify(&renderer_only, &names), Running::NoDebugPort);

        let with_port = vec![
            process(&alias, "chrome --type=renderer"),
            process(&alias, "chrome --remote-debugging-port=9333"),
        ];
        assert_eq!(
            classify(&with_port, &names),
            Running::DebugPorts(vec![9333])
        );

        let pipe = vec![process(&alias, "chrome --remote-debugging-pipe")];
        assert_eq!(classify(&pipe, &names), Running::NoDebugPort);

        let unrelated = vec![process("firefox", "firefox --remote-debugging-port=9333")];
        assert_eq!(classify(&unrelated, &names), Running::Absent);
    }

    /// 多合一二进制（NixOS 的 coreutils、busybox）：`exe` 是 `coreutils`，真名只在 `argv0` 里。
    #[test]
    fn multi_call_binaries_match_through_argv0() {
        let info = ProcessInfo {
            exe: "coreutils".to_string(),
            argv0: "sleep".to_string(),
            command_line: "sleep 30".to_string(),
        };
        assert_eq!(
            classify(&[info], &["sleep".to_string()]),
            Running::NoDebugPort
        );
    }

    /// 没 `exec` 的 shell 包装脚本：`argv0` 是脚本名，`exe` 才是真身。
    #[test]
    fn wrapper_scripts_match_through_the_real_binary() {
        let info = ProcessInfo {
            exe: "chrome".to_string(),
            argv0: "google-chrome".to_string(),
            command_line: "google-chrome".to_string(),
        };
        assert_eq!(
            classify(&[info], &process_names("google-chrome")),
            Running::NoDebugPort
        );
    }

    /// 命令行里出现候选名字（`grep browser` 之类）不算命中——只看 exe 与 argv0。
    #[test]
    fn only_the_executable_name_counts_as_a_match() {
        let names = vec!["browser".to_string()];
        let processes = vec![process("grep", "grep -rn browser /somewhere")];
        assert_eq!(classify(&processes, &names), Running::Absent);
    }

    /// 相同端口只记一次（多个子进程都带同一个端口参数）。
    #[test]
    fn classify_deduplicates_ports() {
        let names = builtin_aliases();
        let alias = names[0].clone();
        let commands = vec![
            process(&alias, "chrome --remote-debugging-port=9222"),
            process(&alias, "chrome --remote-debugging-port=9222"),
        ];
        assert_eq!(classify(&commands, &names), Running::DebugPorts(vec![9222]));
    }

    /// 默认候选**不含** Firefox（141+ 没有 CDP、140 ESR 默认不启用），但手工写进来时
    /// 会拿到 Firefox 自己的开关而不是 Chromium 的。
    #[test]
    fn firefox_is_not_a_default_candidate_but_gets_its_own_switches() {
        let browsers = default_browsers();
        assert!(
            !browsers.iter().any(|program| program.contains("firefox")),
            "默认候选不应含 Firefox：{browsers:?}"
        );

        assert_eq!(launch_switches("firefox"), FIREFOX_SWITCHES);
        assert_eq!(launch_switches("firefox-esr"), FIREFOX_SWITCHES);
        assert_eq!(
            launch_switches("/opt/ff/firefox"),
            FIREFOX_SWITCHES,
            "自定义路径也按家族判定"
        );

        assert_eq!(
            launch_switches("google-chrome"),
            &["--no-first-run", "--no-default-browser-check"]
        );
        assert_eq!(
            launch_switches("/opt/my-chromium"),
            CHROMIUM_SWITCHES,
            "自定义项默认按 Chromium 系处理"
        );
    }

    /// 只有 Firefox 系列带「拿不到 CDP 端点」的说明，其它浏览器没有。
    #[test]
    fn only_firefox_carries_the_bidi_hint() {
        assert!(cdp_hint("firefox").unwrap().contains("BiDi"));
        assert!(cdp_hint("/usr/lib/firefox-esr/firefox-esr").is_some());
        assert!(cdp_hint("google-chrome").is_none());
        assert!(cdp_hint("chromium").is_none());
    }

    /// 端点一直不出现时 `wait_for_endpoint` 在预算内就返回，不拖到超时之外。
    #[tokio::test]
    async fn wait_for_endpoint_gives_up_on_schedule() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);

        let started = Instant::now();
        let error = wait_for_endpoint("127.0.0.1", port, Duration::from_millis(400))
            .await
            .unwrap_err();
        assert!(error.contains(&port.to_string()), "{error}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "不该超出预算太多"
        );
    }

    /// 真起一个进程扫一遍：认得出它在跑（但没有调试端口），认不出的当没在跑。
    #[cfg(unix)]
    #[test]
    fn a_real_process_shows_up_in_the_snapshot() {
        let Ok(mut child) = std::process::Command::new("sleep").arg("30").spawn() else {
            return; // 环境里没有 sleep（非 unix），跳过
        };
        // 等进程表的采集拿到它
        std::thread::sleep(Duration::from_millis(300));
        let snapshot = ProcessSnapshot::scan();
        assert_eq!(snapshot.state_of("sleep"), Running::NoDebugPort);
        assert_eq!(snapshot.state_of("zzz-nothing-here"), Running::Absent);
        _ = child.kill();
        _ = child.wait();
    }

    /// 本机进程表读得到，并且里面一定有我们自己（取 `exe` 与 `argv0` 的规范化名字比对）。
    #[test]
    fn the_snapshot_includes_this_test_process() {
        let processes = ProcessSnapshot::scan().processes;
        assert!(!processes.is_empty());
        let self_name = basename(
            &std::env::current_exe()
                .expect("测试进程应有可执行文件路径")
                .file_name()
                .unwrap_or_default()
                .to_string_lossy(),
        );
        assert!(
            processes
                .iter()
                .any(|process| process.exe == self_name || process.argv0 == self_name),
            "进程列表里应有本测试进程（{self_name}）"
        );
    }
}

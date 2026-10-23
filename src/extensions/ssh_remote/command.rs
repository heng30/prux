//! `/ssh` 与 `/tunnel` 两个斜杠命令。
//!
//! `/ssh` 只管「默认目标是谁、现在连着哪些、凭据清掉」；它不改变任何本地工具的行为。
//! `/tunnel` 管端口转发：把远端服务映射到 `127.0.0.1:<local>`，之后本地一切照旧。

use crate::{
    core::extensions::{ExtensionCommand, SubcommandDef, UiNotifyLevel},
    extensions::{
        ssh_remote::{
            self, auth,
            sshcfg::SshHost,
            state::{self, Endpoint, RemoteConfig, TunnelSpec},
            transport, tunnel,
        },
        util::notify_text,
    },
    modes::interactive::app::App,
    utils::{
        display::split_head, env::default_user, net::split_host_port, process::shell_tokenize,
    },
};
use std::time::Duration;

/// 斜杠命令名
pub const CMD: &str = "ssh";
/// 隧道命令名
pub const TUNNEL_CMD: &str = "tunnel";
/// 命令描述（`/` 候选面板里显示；一行写完，所以只点出可用的 flag 名）
pub const DESCRIPTION: &str = "/ssh [user@host[:port]] [-p port] [-i key] [-J jump] [-o PasswordFile=path] sets the default SSH target; /ssh list|status|cd|note|disconnect|forget-host|forget";

/// 目标写法的完整说明：`/ssh` 空参时随状态一起显示。
///
/// 面板里的一行放不下 flag 的取值与语义，而用户不知道 `-i` / `-J` 存在就用不上它们，
/// 所以把「能写什么」集中在状态输出里。
const USAGE: &str = "Target: /ssh [user@host[:port]] (an ~/.ssh/config alias works too) plus ssh-style flags:
  -p <port>                port on the remote host
  -i <key>                 private key; no password fallback unless a password source is also given
  -J [user@]host[:port]    jump host (same as `-o ProxyJump=...`)
  -o PasswordFile=<path>   read the password from a 0600 file (`PRUX_SSH_PASSWORD` also works)
Host / HostName / User / Port / IdentityFile / ProxyJump come from ~/.ssh/config when not given.
Subcommands: list | status | cd <path> | note <text> | disconnect [target|all] | forget-host <host:port> | forget [all]";

/// 子命令候选（顺序 = 面板候选顺序，与 [`handle`] 的分支一一对应）
pub const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "list",
        description: "List connected endpoints and remembered targets",
    },
    SubcommandDef {
        name: "status",
        description: "Show the default target, output budget and active tunnels",
    },
    SubcommandDef {
        name: "cd",
        description: "Set the default remote working directory: /ssh cd <path>",
    },
    SubcommandDef {
        name: "note",
        description: "Set a note on the default target: /ssh note <text>",
    },
    SubcommandDef {
        name: "disconnect",
        description: "Drop connections: /ssh disconnect [target|all]",
    },
    SubcommandDef {
        name: "forget-host",
        description: "Drop a remembered host key so a changed key can be re-trusted",
    },
    SubcommandDef {
        name: "forget",
        description: "Clear cached passwords / key passphrases (all = every target)",
    },
];

/// 隧道的子命令候选
pub const TUNNEL_SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "add",
        description: "Forward a remote port: /tunnel add [host:]localPort:remoteHost:remotePort",
    },
    SubcommandDef {
        name: "remove",
        description: "Stop one forward by local port: /tunnel remove <localPort>",
    },
    SubcommandDef {
        name: "remove-all",
        description: "Stop and delete every forward (including the configured ones)",
    },
    SubcommandDef {
        name: "list",
        description: "List active and configured forwards",
    },
    SubcommandDef {
        name: "stop-all",
        description: "Stop every active forward",
    },
];

/// `/ssh` 的命令声明
pub fn command() -> ExtensionCommand {
    ExtensionCommand {
        name: CMD.to_string(),
        description: DESCRIPTION.to_string(),
        // handler 只读写本扩展状态 + 发通知，不锁 agent
        busy_safe: true,
        subcommands: SUBCOMMANDS.to_vec(),
    }
}

/// `/tunnel` 的命令声明
pub fn tunnel_command() -> ExtensionCommand {
    ExtensionCommand {
        name: TUNNEL_CMD.to_string(),
        description:
            "/tunnel add [host:]localPort:remoteHost:remotePort | remove <localPort> | remove-all | list | stop-all "
                .to_string(),
        busy_safe: true,
        subcommands: TUNNEL_SUBCOMMANDS.to_vec(),
    }
}

/// `/ssh` 执行入口
pub fn handle(_app: &mut App, raw: &str) -> bool {
    let text = raw.trim();
    if text.is_empty() {
        notify_text(&status_text(), UiNotifyLevel::Info);
        return false;
    }

    let (head, rest) = split_head(text);
    match head.to_ascii_lowercase().as_str() {
        "list" => notify_text(&list_text(), UiNotifyLevel::Info),
        "status" => notify_text(&status_text(), UiNotifyLevel::Info),
        "cd" => set_default(|ep| ep.cwd = expand_remote_path(rest)),
        "note" => set_default(|ep| ep.note = rest.trim().to_string()),
        "disconnect" => disconnect(rest),
        "forget-host" => {
            let id = rest.trim();
            if id.is_empty() {
                notify_text(
                    "Usage: /ssh forget-host <host:port>",
                    UiNotifyLevel::Warning,
                );
            } else if state::forget_known_host(id) {
                notify_text(&format!("Forgot host key for {id}."), UiNotifyLevel::Info);
            } else {
                notify_text(
                    &format!("No remembered host key for {id}."),
                    UiNotifyLevel::Warning,
                );
            }
        }
        "forget" => {
            let cleared = if rest.trim().eq_ignore_ascii_case("all") {
                auth::forget_all()
            } else {
                RemoteConfig::load()
                    .default
                    .map(|ep| auth::forget(&ep.key()))
                    .unwrap_or(0)
            };
            notify_text(
                &format!("Cleared {cleared} cached credential(s)."),
                UiNotifyLevel::Info,
            );
        }
        _ => set_target(text),
    }
    false
}

/// `/tunnel` 执行入口
pub fn handle_tunnel(_app: &mut App, raw: &str) -> bool {
    let text = raw.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("list") {
        notify_text(&tunnel_text(), UiNotifyLevel::Info);
        return false;
    }

    let (head, rest) = split_head(text);
    match head.to_ascii_lowercase().as_str() {
        "add" => add_tunnel(rest),
        "remove" => {
            let port: u16 = match rest.trim().parse() {
                Ok(p) => p,
                Err(_) => {
                    notify_text("Usage: /tunnel remove <localPort>", UiNotifyLevel::Warning);
                    return false;
                }
            };
            let stopped = tunnel::stop(port);
            let forgotten = tunnel::forget(port).unwrap_or(false);
            let message = match (stopped, forgotten) {
                (true, _) => format!("Stopped forward on 127.0.0.1:{port}."),
                (false, true) => format!("Removed configured forward for port {port}."),
                (false, false) => format!("No forward on port {port}."),
            };
            notify_text(&message, UiNotifyLevel::Info);
        }
        "stop-all" => {
            let count = tunnel::stop_all();
            notify_text(&format!("Stopped {count} forward(s)."), UiNotifyLevel::Info);
        }
        "remove-all" => {
            let stopped = tunnel::stop_all();
            match tunnel::forget_all() {
                Ok(removed) => notify_text(
                    &format!(
                        "Stopped {stopped} forward(s) and deleted {removed} configured one(s)."
                    ),
                    UiNotifyLevel::Info,
                ),
                // 转发已经停了，只是配置没落盘：下次启动不会再自动拉起
                Err(e) => notify_text(
                    &format!("Stopped {stopped} forward(s) but cannot update config: {e}"),
                    UiNotifyLevel::Error,
                ),
            }
        }
        "list" => notify_text(&tunnel_text(), UiNotifyLevel::Info),
        _ => notify_text(
            "Usage: /tunnel add [host:]localPort:remoteHost:remotePort | remove <localPort> | remove-all | list | stop-all",
            UiNotifyLevel::Warning,
        ),
    }
    false
}

/// 远端路径：`~` 原样交给远端 shell 展开，其余按输入保存
fn expand_remote_path(path: &str) -> String {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        state::FALLBACK_REMOTE_CWD.to_string()
    } else {
        trimmed.to_string()
    }
}

/// 修改默认端点（没有默认端点时提示）
fn set_default(f: impl FnOnce(&mut Endpoint)) {
    let mut cfg = RemoteConfig::load();
    let Some(mut ep) = cfg.default.clone() else {
        notify_text(
            "No default SSH target yet: run `/ssh user@host` first.",
            UiNotifyLevel::Warning,
        );
        return;
    };

    f(&mut ep);

    // 已连接的端点同步更新，避免下一次工具调用还用旧目录/旧备注
    let updated = ep.clone();
    cfg.set_default(updated.clone());
    if let Err(e) = cfg.save() {
        notify_text(&format!("Cannot save config: {e}"), UiNotifyLevel::Error);
        return;
    }

    notify_text(
        &format!("Default target is now {}.", updated.display()),
        UiNotifyLevel::Info,
    );
}

/// 断连：`all` / 目标 / 默认端点
fn disconnect(rest: &str) {
    let arg = rest.trim();
    let count = if arg.eq_ignore_ascii_case("all") {
        transport::disconnect_all()
    } else if arg.is_empty() {
        match RemoteConfig::load().default_key() {
            Some(key) if transport::disconnect(&key) => 1,
            _ => 0,
        }
    } else {
        // 可能是完整 key，也可能是别名：两类都试一次
        let direct = transport::disconnect(arg);
        if direct {
            1
        } else {
            let mut hit = 0;
            for ep in RemoteConfig::load().known {
                if ep.host == arg || ep.command.contains(arg) {
                    hit += usize::from(transport::disconnect(&ep.key()));
                }
            }
            hit
        }
    };

    if count == 0 {
        notify_text("No matching connection.", UiNotifyLevel::Warning);
    } else {
        notify_text(
            &format!("Closed {count} connection(s)."),
            UiNotifyLevel::Info,
        );
    }
}

/// 设置目标：解析后存为默认端点，并后台连一次验证凭据
fn set_target(text: &str) {
    let query = if text.to_ascii_lowercase().starts_with("ssh ") {
        text.to_string()
    } else {
        format!("ssh {text}")
    };

    let endpoint = match parse_ssh_command(&query) {
        Ok(ep) => ep,
        Err(e) => {
            notify_text(&format!("Cannot parse target: {e}"), UiNotifyLevel::Warning);
            return;
        }
    };

    let mut cfg = RemoteConfig::load();
    cfg.set_default(endpoint.clone());
    if let Err(e) = cfg.save() {
        notify_text(&format!("Cannot save config: {e}"), UiNotifyLevel::Error);
        return;
    }

    notify_text(
        &format!("Default SSH target: {}.", endpoint.display()),
        UiNotifyLevel::Info,
    );

    // 连一次当场暴露认证/主机密钥问题，而不是等到第一次工具调用
    let probe = endpoint.clone();
    tokio::spawn(async move {
        match tokio::time::timeout(Duration::from_secs(20), transport::ensure(probe.clone())).await
        {
            Ok(Ok(_)) => notify_text(
                &format!("Connected to {}.", probe.display()),
                UiNotifyLevel::Info,
            ),
            Ok(Err(e)) => notify_text(
                &format!("Cannot connect to {}: {e}", probe.key()),
                UiNotifyLevel::Error,
            ),
            Err(_) => notify_text(
                &format!("Connecting to {} timed out.", probe.key()),
                UiNotifyLevel::Warning,
            ),
        }
    });
}

/// 加一条转发：先起再落盘，起失败不写配置
fn add_tunnel(rest: &str) {
    let Some(spec) = TunnelSpec::parse(rest.trim()) else {
        notify_text(
            "Usage: /tunnel add [host:]localPort:remoteHost:remotePort (e.g. /tunnel add 15432:localhost:5432)",
            UiNotifyLevel::Warning,
        );
        return;
    };

    let saved = spec.clone();
    tokio::spawn(async move {
        match tunnel::start(saved.clone()).await {
            Ok(port) => {
                let mut stored = saved.clone();
                stored.local_port = port;
                if let Err(e) = tunnel::remember(&stored) {
                    notify_text(
                        &format!("Forward started but not saved: {e}"),
                        UiNotifyLevel::Warning,
                    );
                }
                notify_text(
                    &format!(
                        "Forwarding 127.0.0.1:{port} → {}:{} (via {}).",
                        stored.remote_host,
                        stored.remote_port,
                        if stored.host.is_empty() {
                            "default target".to_string()
                        } else {
                            stored.host.clone()
                        }
                    ),
                    UiNotifyLevel::Info,
                );
            }
            Err(e) => notify_text(&format!("Cannot forward: {e}"), UiNotifyLevel::Error),
        }
    });
}

/// 状态文本
fn status_text() -> String {
    let cfg = RemoteConfig::load();
    let default = cfg
        .default
        .as_ref()
        .map(|ep| ep.display())
        .unwrap_or_else(|| "(none: run `/ssh user@host[:port]`)".to_string());
    let active = tunnel::active();
    format!(
        "Default target: {default}\nKnown targets: {}\nActive forwards: {}\nExec timeout: {}s · per-turn output budget: {} KiB\n\n{USAGE}",
        cfg.known.len(),
        active.len(),
        cfg.limits.timeout_seconds,
        cfg.limits.turn_max_bytes / 1024
    )
}

/// 已连接 + 已记住的端点
fn list_text() -> String {
    let cfg = RemoteConfig::load();
    let mut lines = vec!["Connected:".to_string()];
    let connected = transport::connected();
    if connected.is_empty() {
        lines.push("  (none)".to_string());
    }

    for ep in &connected {
        lines.push(format!("  {} · cwd {}", ep.key(), ep.cwd));
    }

    lines.push("Remembered:".to_string());
    if cfg.known.is_empty() {
        lines.push("  (none)".to_string());
    }

    for ep in &cfg.known {
        lines.push(format!("  {}", ep.display()));
    }

    if !cfg.tunnels.is_empty() {
        lines.push("Configured forwards:".to_string());
        for spec in &cfg.tunnels {
            lines.push(format!("  {}", spec.to_spec_text()));
        }
    }
    lines.join("\n")
}

/// 转发状态文本
fn tunnel_text() -> String {
    let active = tunnel::active();
    let cfg = RemoteConfig::load();
    let mut lines = vec!["Active forwards:".to_string()];
    if active.is_empty() {
        lines.push("  (none)".to_string());
    }

    for spec in &active {
        lines.push(format!(
            "  127.0.0.1:{} → {}:{} (via {})",
            spec.local_port,
            spec.remote_host,
            spec.remote_port,
            if spec.host.is_empty() {
                "default target"
            } else {
                spec.host.as_str()
            }
        ));
    }

    lines.push("Configured:".to_string());
    if cfg.tunnels.is_empty() {
        lines.push("  (none)".to_string());
    }

    for spec in &cfg.tunnels {
        lines.push(format!("  {}", spec.to_spec_text()));
    }

    lines.join("\n")
}

pub fn parse_ssh_command(input: &str) -> Result<Endpoint, String> {
    let mut tokens = shell_tokenize(input);
    if tokens
        .first()
        .map(|t| t.eq_ignore_ascii_case("ssh"))
        .unwrap_or(false)
    {
        tokens.remove(0);
    }

    if tokens.is_empty() {
        return Err("empty ssh command".to_string());
    }

    let mut port: Option<u16> = None;
    let mut identity: Option<String> = None;
    let mut password_file: Option<String> = None;
    let mut jump: Option<String> = None;
    let mut target: Option<String> = None;
    let mut idx = 0;
    while idx < tokens.len() {
        let token = tokens[idx].clone();
        match token.as_str() {
            "-p" => {
                idx += 1;
                port = tokens.get(idx).and_then(|p| p.parse().ok());
                if port.is_none() {
                    return Err("-p requires a valid port".to_string());
                }
            }
            "-i" => {
                idx += 1;
                let value = tokens.get(idx).ok_or("-i requires a key path")?;
                identity = Some(value.clone());
            }
            "-J" => {
                idx += 1;
                let value = tokens.get(idx).ok_or("-J requires a jump host")?;
                jump = Some(value.clone());
            }
            "-o" => {
                idx += 1;
                let value = tokens.get(idx).ok_or("-o requires Key=Value")?;
                if let Some((k, v)) = value.split_once('=') {
                    match k.trim().to_ascii_lowercase().as_str() {
                        "proxyjump" if !v.trim().eq_ignore_ascii_case("none") => {
                            jump = Some(v.trim().to_string());
                        }
                        // 只认 `PasswordFile`：密码证书由文件提供，绝不从参数里读明文密码
                        "passwordfile" if !v.trim().is_empty() => {
                            password_file = Some(v.trim().to_string());
                        }
                        _ => {}
                    }
                }
            }
            // 紧凑写法 `-p2202` / `-i~/.ssh/k` / `-Jbastion`；用 strip_prefix 而不是
            // 按字节切片，避免 `-p€` 这类非 ASCII 值落到字符边界之外
            other if other.len() > 2 && other.starts_with("-p") => {
                port = other.strip_prefix("-p").and_then(|v| v.parse().ok());
            }
            other if other.len() > 2 && other.starts_with("-i") => {
                identity = other.strip_prefix("-i").map(|v| v.to_string());
            }
            other if other.len() > 2 && other.starts_with("-J") => {
                jump = other.strip_prefix("-J").map(|v| v.to_string());
            }
            other if other.starts_with('-') => {}
            other => target = Some(other.to_string()),
        }
        idx += 1;
    }

    let target = target.ok_or("missing host (expected [user@]host)")?;
    let (mut user, host) = match target.rsplit_once('@') {
        Some((u, h)) => (Some(u.to_string()), h.to_string()),
        None => (None, target.clone()),
    };

    let (host, host_port) = split_host_port(&host, 22);
    let mut port = port.unwrap_or(host_port);
    let mut host = host;

    // 应用 ~/.ssh/config：别名 → 实际主机名 / 用户 / 端口 / 私钥 / 跳板机
    if let Some(cfg_host) = lookup_ssh_config(&target) {
        if cfg_host.hostname != cfg_host.alias {
            host = cfg_host.hostname.clone();
        }
        if user.is_none() {
            user = cfg_host.user.clone();
        }
        if port == 22
            && let Some(configured) = cfg_host.port
        {
            port = configured;
        }
        if identity.is_none() {
            identity = cfg_host.identity_file.clone();
        }
        if jump.is_none() {
            jump = cfg_host.proxy_jump.clone();
        }
    }

    Ok(Endpoint {
        command: format!("ssh {target}"),
        host,
        user: user.unwrap_or_else(default_user),
        port,
        identity,
        password_file,
        jump,
        note: String::new(),
        cwd: state::FALLBACK_REMOTE_CWD.to_string(),
    })
}

/// 在当前用户下按别名（或配置里写的主机名）查 ssh config
fn lookup_ssh_config(alias: &str) -> Option<SshHost> {
    ssh_remote::sshcfg::lookup(alias)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_target() {
        let ep =
            parse_ssh_command("ssh root@gpu.example.com -p 2202 -i ~/.ssh/id_ed25519").unwrap();
        assert_eq!(ep.user, "root");
        assert_eq!(ep.host, "gpu.example.com");
        assert_eq!(ep.port, 2202);
        assert_eq!(ep.identity.as_deref(), Some("~/.ssh/id_ed25519"));
    }

    #[test]
    fn parses_jump_and_proxyjump() {
        let ep = parse_ssh_command("ssh -J deploy@bastion:2222 root@target").unwrap();
        assert_eq!(ep.jump.as_deref(), Some("deploy@bastion:2222"));

        let ep = parse_ssh_command("ssh -o ProxyJump=bastion root@target").unwrap();
        assert_eq!(ep.jump.as_deref(), Some("bastion"));
    }

    #[test]
    fn rejects_missing_host() {
        assert!(parse_ssh_command("ssh -p 22").is_err());
        assert!(parse_ssh_command("ssh").is_err());
    }

    #[test]
    fn parses_quoted_target() {
        let ep = parse_ssh_command("ssh \"my host\" -p 22").unwrap();
        assert_eq!(ep.host, "my host");
    }

    /// 命令描述与状态输出必须把可用的 flag 写全，否则用户不知道能设跳板机 / 私钥。
    #[test]
    fn description_lists_the_supported_flags() {
        for flag in ["-p ", "-i ", "-J ", "-o PasswordFile="] {
            assert!(DESCRIPTION.contains(flag), "描述缺少 {flag}");
            assert!(USAGE.contains(flag), "用法说明缺少 {flag}");
        }
        assert!(USAGE.contains("ProxyJump"), "应说明 -o ProxyJump= 等价写法");
    }
}

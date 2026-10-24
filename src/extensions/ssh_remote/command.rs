//! `/ssh` 与 `/tunnel` 两个斜杠命令。
//!
//! `/ssh` 只管「默认目标是谁、现在连着哪些、凭据清掉」；它不改变任何本地工具的行为。
//! `/tunnel` 管端口转发：把远端服务映射到 `127.0.0.1:<local>`，之后本地一切照旧。

use crate::{
    core::extensions::{ExtensionCommand, SubcommandDef, UiNotifyLevel},
    extensions::{
        command_arg,
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
pub const DESCRIPTION: &str = "/ssh [[user@]host[:port]] [-i key] [-J jump] [-o PasswordFile=path] sets the default SSH target; /ssh help|list|status|cd|note|disconnect|remove|forget";

/// 目标写法的完整说明：`/ssh` 空参时随状态一起显示。
///
/// 面板里的一行放不下 flag 的取值与语义，而用户不知道 `-i` / `-J` 存在就用不上它们，
/// 所以把「能写什么」集中在状态输出里。
const USAGE: &str = "Target: /ssh [user@]host[:port] (an ~/.ssh/config alias works too) plus ssh-style flags:
  -i <key>                 private key; no password fallback unless a password source is also given
  -J [user@]host[:port]    jump host (same as `-o ProxyJump=...`)
  -o PasswordFile=<path>   read the password from a 0600 file (`PRUX_SSH_PASSWORD` also works)
Host / HostName / User / Port / IdentityFile / ProxyJump come from ~/.ssh/config when not given.
Subcommands: help | list | status | cd <path> | note <text> | disconnect [target|all] | remove <target|all> | forget <host[:port]|all>";

/// 子命令候选（顺序 = 面板候选顺序，与 [`handle`] 的分支一一对应）
pub const SUBCOMMANDS: &[SubcommandDef] = &[
    SubcommandDef {
        name: "help",
        description: "Print the target syntax and every subcommand: /ssh help",
    },
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
        name: "remove",
        description: "Forget a target: drops it from config.json, its host key and its cached password: /ssh remove <target|all>",
    },
    SubcommandDef {
        name: "forget",
        description: "Drop a recorded host key so a changed key can be re-trusted (the target stays in the list): /ssh forget <host[:port]|all>",
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
    // `raw` 是 `/` 后的原文（含命令名，如 `ssh help`），与其它扩展命令同一约定
    let text = command_arg(raw);
    if text.is_empty() {
        notify_text(&status_text(), UiNotifyLevel::Info);
        return false;
    }

    let (head, rest) = split_head(text);
    match head.to_ascii_lowercase().as_str() {
        "help" => notify_text(USAGE, UiNotifyLevel::Info),
        "list" => notify_text(&list_text(), UiNotifyLevel::Info),
        "status" => notify_text(&status_text(), UiNotifyLevel::Info),
        "cd" => set_default(|ep| ep.cwd = expand_remote_path(rest)),
        "note" => set_default(|ep| ep.note = rest.trim().to_string()),
        "disconnect" => disconnect(rest),
        "remove" => remove_target(rest.trim()),
        "forget" => forget_host_key(rest.trim()),
        _ => set_target(text),
    }
    false
}

/// `/tunnel` 执行入口
pub fn handle_tunnel(_app: &mut App, raw: &str) -> bool {
    let text = command_arg(raw);
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
        // 只断 target 真正命中的那条：以前直接拿 `command.contains(arg)` 扫 known，
        // 半截子串（`gpu` 之于 `gpu.example.com`）会把不相关的连接一并断掉
        let keys = matching_keys(&disconnect_candidates(), arg);
        keys.iter().filter(|key| transport::disconnect(key)).count()
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

/// 断连候选：连接池里的活连接在前，配置里的 known 去重后补齐
///
/// 只有池里的能真正断开；known 里那些没连着的也放进来，方便「配了但已掉线」的情况
/// 走到同一个匹配口径上。
fn disconnect_candidates() -> Vec<Endpoint> {
    let mut out = transport::connected();
    for ep in RemoteConfig::load().known {
        if !out.iter().any(|conn| conn.key() == ep.key()) {
            out.push(ep);
        }
    }
    out
}

/// 候选里命中 `arg` 的端点 key（不匹配时为空，调用方据此报「没连上」）
fn matching_keys(candidates: &[Endpoint], arg: &str) -> Vec<String> {
    candidates
        .iter()
        .filter(|ep| transport::matches_spec(ep, arg))
        .map(Endpoint::key)
        .collect()
}

/// 把 `[user@]host[:port]` 拆成主机与端口；输入没写端口时端口为 `None`
fn host_key_arg(arg: &str) -> (String, Option<u16>) {
    let bare = arg.rsplit_once('@').map(|(_, host)| host).unwrap_or(arg);
    let (host, port) = split_host_port(bare, 0);
    (host, (port != 0).then_some(port))
}

/// 只删 `known-hosts.json` 里的主机公钥：目标本身与缓存凭据保留
fn forget_host_key(arg: &str) {
    if arg.is_empty() {
        notify_text(
            "Usage: /ssh forget <host[:port]|all>",
            UiNotifyLevel::Warning,
        );
        return;
    }

    if arg.eq_ignore_ascii_case("all") {
        let count = state::forget_all_known_hosts();
        notify_text(&format!("Forgot {count} host key(s)."), UiNotifyLevel::Info);
        return;
    }

    // 不带端口时删该主机的所有端口记录（known-hosts 的键恒为 `host:port`）
    let (host, port) = host_key_arg(arg);
    let ids: Vec<String> = state::load_known_hosts()
        .into_keys()
        .filter(|id| match port {
            Some(port) => id == &format!("{host}:{port}"),
            None => split_host_port(id, 0).0 == host,
        })
        .collect();
    let removed = ids.iter().filter(|id| state::forget_known_host(id)).count();

    if removed == 0 {
        notify_text(
            &format!("No remembered host key for {arg}."),
            UiNotifyLevel::Warning,
        );
    } else {
        notify_text(
            &format!("Forgot {removed} host key(s)."),
            UiNotifyLevel::Info,
        );
    }
}

/// 彻底移除目标：`config.json` 的 known/默认项、`known-hosts.json` 的公钥、内存里的凭据
fn remove_target(arg: &str) {
    if arg.is_empty() {
        notify_text("Usage: /ssh remove <target|all>", UiNotifyLevel::Warning);
        return;
    }

    let all = arg.eq_ignore_ascii_case("all");
    let mut cfg = RemoteConfig::load();
    let known = std::mem::take(&mut cfg.known);
    let (removed, rest): (Vec<Endpoint>, Vec<Endpoint>) = known
        .into_iter()
        .partition(|ep| all || transport::matches_spec(ep, arg));

    if removed.is_empty() && !all {
        notify_text(
            &format!("No remembered target matching {arg}."),
            UiNotifyLevel::Warning,
        );
        return;
    }
    cfg.known = rest;

    // 默认目标命中时一并清掉，否则工具还会继续用它
    if cfg
        .default
        .as_ref()
        .is_some_and(|ep| all || transport::matches_spec(ep, arg))
    {
        cfg.default = None;
    }

    if let Err(e) = cfg.save() {
        notify_text(&format!("Cannot save config: {e}"), UiNotifyLevel::Error);
        return;
    }

    // 配置落盘后才动凭据与主机公钥，避免半途失败留下残缺状态
    let cleared = if all {
        state::forget_all_known_hosts();
        auth::forget_all()
    } else {
        let keys: Vec<String> = removed.iter().map(Endpoint::key).collect();
        for ep in &removed {
            state::forget_known_host(&format!("{}:{}", ep.host, ep.port));
        }
        keys.iter().map(|key| auth::forget(key)).sum()
    };

    notify_text(
        &format!(
            "Removed {} target(s) and cleared {cleared} cached credential(s).",
            removed.len()
        ),
        UiNotifyLevel::Info,
    );
}

/// 设置目标：先连一次，登录成功才把它写成默认端点
///
/// 未成功登录的端点既不进 `known` 也不改 `default`——否则 `/ssh list` 会一直挂着
/// 连不上的目标，`/ssh status` 也会显示一个形同虚设的默认值。
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

    // 连一次当场暴露认证/主机密钥问题，而不是等到第一次工具调用
    let probe = endpoint.clone();
    tokio::spawn(async move {
        match tokio::time::timeout(Duration::from_secs(20), transport::ensure(probe.clone())).await
        {
            Ok(Ok(_)) => save_target(probe),
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

/// 把已登录成功的端点存为默认目标（同时进 `known` 列表）
fn save_target(ep: Endpoint) {
    let mut cfg = RemoteConfig::load();
    cfg.set_default(ep.clone());
    match cfg.save() {
        Ok(()) => notify_text(
            &format!("Default SSH target: {} (connected).", ep.display()),
            UiNotifyLevel::Info,
        ),
        // 连接已建立，只是没落盘：本次会话可以用，重启后要重新 `/ssh`
        Err(e) => notify_text(
            &format!("Connected to {} but cannot save config: {e}", ep.key()),
            UiNotifyLevel::Error,
        ),
    }
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
        .unwrap_or_else(|| "(none: run `/ssh [user@]host[:port]`)".to_string());
    let active = tunnel::active();
    format!(
        "Default target: {default}\nKnown targets: {}\nActive forwards: {}\nExec timeout: {}s · per-turn output budget: {} KiB",
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

    let mut identity: Option<String> = None;
    let mut password_file: Option<String> = None;
    let mut jump: Option<String> = None;
    let mut target: Option<String> = None;
    let mut idx = 0;
    while idx < tokens.len() {
        let token = tokens[idx].clone();
        match token.as_str() {
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
            // `-p` 与 `host:port` 重复，而且只忽略它会把紧随的端口号当成主机名：直接拒绝
            other if other == "-p" || (other.len() > 2 && other.starts_with("-p")) => {
                return Err("`-p` is not supported: write the port as `host:port`".to_string());
            }
            // 紧凑写法 `-i~/.ssh/k` / `-Jbastion`；用 strip_prefix 而不是
            // 按字节切片，避免 `-i€` 这类非 ASCII 值落到字符边界之外
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
    let mut port = host_port;
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
        let ep = parse_ssh_command("ssh root@gpu.example.com:2202 -i ~/.ssh/id_ed25519").unwrap();
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
        assert!(parse_ssh_command("ssh -i ~/.ssh/k").is_err());
        assert!(parse_ssh_command("ssh").is_err());
    }

    /// `-p` 与 `host:port` 重复，必须报错而不是把端口号当成主机名去连
    #[test]
    fn rejects_dash_p() {
        for input in ["ssh -p 22 gpu", "ssh gpu -p22", "ssh -p22 gpu"] {
            let err = parse_ssh_command(input).expect_err("-p 应被拒绝");
            assert!(err.contains("host:port"), "{err}");
        }
    }

    #[test]
    fn parses_quoted_target() {
        let ep = parse_ssh_command("ssh \"my host\"").unwrap();
        assert_eq!(ep.host, "my host");
    }

    /// 命令描述与状态输出必须把可用的 flag 写全，否则用户不知道能设跳板机 / 私钥。
    #[test]
    fn description_lists_the_supported_flags() {
        for flag in ["-i ", "-J ", "-o PasswordFile="] {
            assert!(DESCRIPTION.contains(flag), "描述缺少 {flag}");
            assert!(USAGE.contains(flag), "用法说明缺少 {flag}");
        }
        assert!(USAGE.contains("ProxyJump"), "应说明 -o ProxyJump= 等价写法");
        assert!(
            !DESCRIPTION.contains("-p"),
            "端口写在 host:port 里，不该有 -p"
        );
        assert!(!USAGE.contains("-p "), "端口写在 host:port 里，不该有 -p");
        assert!(
            DESCRIPTION.contains("[user@]host[:port]"),
            "user@ 可选，应写成 [user@]"
        );
        assert!(
            USAGE.contains("[user@]host[:port]"),
            "user@ 可选，应写成 [user@]"
        );
    }

    /// `/ssh help` 要能把面板候选里的每个子命令都印出来，候选与用法不许各说各话。
    #[test]
    fn help_covers_every_subcommand() {
        assert!(
            SUBCOMMANDS.iter().any(|s| s.name == "help"),
            "help 必须是子命令候选"
        );
        for sub in SUBCOMMANDS {
            assert!(USAGE.contains(sub.name), "USAGE 缺少子命令 {}", sub.name);
        }
    }

    /// `remove` 同时清配置、主机密钥与凭据；`forget` 只动主机密钥。
    #[test]
    fn remove_drops_target_and_forget_keeps_it() {
        let _lock = state::TEST_ROOT_LOCK.blocking_lock();
        let _root = state::TestRootGuard::temp();
        let mut app = App::new();

        let ep = parse_ssh_command("ssh root@gpu.example.com").unwrap();
        let mut cfg = RemoteConfig::load();
        cfg.set_default(ep.clone());
        cfg.save().unwrap();
        state::save_known_host("gpu.example.com:22", "ssh-ed25519 AAAA").unwrap();

        handle(&mut app, "ssh forget gpu.example.com");
        assert!(state::load_known_hosts().is_empty(), "forget 应删主机密钥");
        assert_eq!(RemoteConfig::load().known.len(), 1, "forget 不应动配置");

        state::save_known_host("gpu.example.com:22", "ssh-ed25519 AAAA").unwrap();
        handle(&mut app, "ssh remove gpu.example.com");
        let cfg = RemoteConfig::load();
        assert!(cfg.known.is_empty(), "remove 应清空 known");
        assert!(cfg.default.is_none(), "remove 应清掉命中的默认目标");
        assert!(state::load_known_hosts().is_empty(), "remove 应删主机密钥");
    }

    /// 登录失败的目标不许留在 `Remembered`：只有探测成功才写 `known` / `default`。
    #[tokio::test]
    async fn failed_target_is_not_remembered() {
        let _lock = state::TEST_ROOT_LOCK.lock().await;
        let _root = state::TestRootGuard::temp();
        let mut app = App::new();

        // 端口没人监听：TCP 连接直接失败，不必等认证
        handle(&mut app, "ssh root@127.0.0.1:1");
        assert!(
            RemoteConfig::load().known.is_empty(),
            "还没验证成功就把目标写进了 known"
        );

        tokio::time::sleep(Duration::from_millis(300)).await;
        let cfg = RemoteConfig::load();
        assert!(cfg.known.is_empty(), "登录失败后 known 仍应是空的");
        assert!(cfg.default.is_none(), "登录失败不该改默认目标");
    }

    /// `/ssh disconnect <target>` 只断命中的那条：拼错/半截的名字不能把别的连接一并断了。
    /// 候选取自池里的活连接与配置的 known，这里直接验证筛选函数。
    #[test]
    fn disconnect_only_matches_the_named_target() {
        let gpu = parse_ssh_command("ssh root@gpu.example.com:2202").unwrap();
        let ubuntu = parse_ssh_command("ssh blue@ubuntu").unwrap();
        let candidates = [gpu.clone(), ubuntu.clone()];

        assert_eq!(
            matching_keys(&candidates, "gpu.example.com"),
            vec![gpu.key()]
        );
        assert_eq!(
            matching_keys(&candidates, "blue@ubuntu:22"),
            vec![ubuntu.key()]
        );
        // 拼错的目标、半截主机名都不该命中——旧实现用 `command.contains(arg)` 会把它们断掉
        assert!(matching_keys(&candidates, "gpu2").is_empty());
        assert!(matching_keys(&candidates, "gpu").is_empty());
        assert!(matching_keys(&candidates, "").is_empty());
    }

    /// TUI 传来的原文含命令名：不剥就会把 `help` / `list` 当成主机名去连接。
    #[test]
    fn subcommands_are_not_parsed_as_targets() {
        let _lock = state::TEST_ROOT_LOCK.blocking_lock();
        let _root = state::TestRootGuard::temp();
        let mut app = App::new();

        for raw in ["ssh help", "ssh list", "ssh status"] {
            handle(&mut app, raw);
            assert!(
                RemoteConfig::load().default.is_none(),
                "`/{raw}` 被当成了目标"
            );
        }
    }
}

//! 建立并认证一条 SSH 连接。
//!
//! 流程：解析目标 →（可选）经跳板机开 `direct-tcpip` 通道 → SSH 握手（含主机密钥校验）
//! → 认证。每个网络阶段都有独立超时，避免一条半开的 TCP 把整次连接拖死。
//!
//! 认证先试公钥认证：用 `-i` 指定的私钥、`~/.ssh/config` 的 `IdentityFile`，或 `~/.ssh/`
//! 下依次尝试的默认私钥；失败后若有密码来源（`-o PasswordFile=` 的 0600 文件、
//! `PRUX_SSH_PASSWORD`、TUI 掩码输入）再试密码，并对只开 keyboard-interactive 的服务器
//! 用同一密码兜底。凭据只留在进程内存，来源顺序与掩码输入的实现见 `auth` 模块。
//! 显式 `-i` 时保持排他语义：只有同时提供了密码来源才会回退到密码。

use crate::{
    extensions::ssh_remote::{
        self, auth,
        state::{RemoteConfig, load_known_hosts, save_known_host},
    },
    utils::{net, paths::expand_tilde_path, process, proxy, truncate},
};
use russh::{
    ChannelMsg,
    client::{self, Handle, Handler},
    keys::{PrivateKey, PrivateKeyWithHashAlg, PublicKeyOrCertificate, load_secret_key},
};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::net::TcpStream;

/// 默认私钥候选（按顺序尝试，缺文件跳过）
const DEFAULT_KEYS: &[&str] = &["id_ed25519", "id_ecdsa", "id_rsa"];

/// 可作为 SSH 承载流的对象
trait TransportStream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl TransportStream for TcpStream {}
impl TransportStream for russh::ChannelStream<russh::client::Msg> {}

/// 一个连接目标
#[derive(Debug, Clone)]
pub struct Target {
    /// 实际主机名
    pub host: String,
    /// 端口
    pub port: u16,
    /// 登录用户
    pub user: String,
    /// 显式私钥路径；Some 时只用它，不回退默认密钥
    pub identity: Option<String>,
    /// 密码来源文件（`-o PasswordFile=<path>`）；Some 时允许在公钥失败后尝试密码
    pub password_file: Option<String>,
    /// 跳板机（`[user@]host[:port]`）
    pub jump: Option<String>,
}

impl Target {
    /// `host:port` 形式的标识（known_hosts 的键）
    pub fn id(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// 凭据缓存的作用域（`user@host:port`）
    pub fn scope(&self) -> String {
        format!("{}@{}", self.user, self.id())
    }
}

/// 一次成功建立的连接
pub struct Connection {
    /// 目标主机句柄
    pub handle: Handle<ClientHandler>,
    /// 跳板机句柄（必须与目标句柄同寿：隧道建在它上面）
    pub jump_handles: Vec<Handle<ClientHandler>>,
}

/// 客户端握手处理器：负责主机密钥校验
pub struct ClientHandler {
    /// `host:port`（known_hosts 键）
    id: String,
    /// 本次握手看到的公钥（OpenSSH 单行形式）
    seen: Arc<Mutex<Option<String>>>,
    /// 拒绝原因（指纹变化时写入）
    rejection: Arc<Mutex<Option<String>>>,
}

impl Handler for ClientHandler {
    type Error = russh::Error;

    /// 校验服务端主机密钥。
    ///
    /// 未知主机按 TOFU 接受（首次连接后记录指纹）；已记录且不匹配时拒绝，
    /// 这是唯一能挡下中间人的地方——首连接本身没有可信渠道可比对。
    async fn check_server_key(
        &mut self,
        server_public_key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let line = server_public_key
            .public_key()
            .to_openssh()
            .unwrap_or_default();

        *self.seen.lock().unwrap() = Some(line.clone());

        let hosts = load_known_hosts();
        match hosts.get(&self.id) {
            Some(known) if known != &line => {
                *self.rejection.lock().unwrap() = Some(format!(
                    "host key for {} changed (known fingerprint differs)\nrun `/ssh forget {}` to trust the new key",
                    self.id, self.id
                ));
                Ok(false)
            }
            _ => Ok(true),
        }
    }

    /// 连接断开：不把它升级成错误，由调用方决定是否重连。
    async fn disconnected(
        &mut self,
        _reason: client::DisconnectReason<Self::Error>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// 建立并认证连接
pub async fn connect(target: &Target) -> Result<Connection, String> {
    let id = target.id();
    let seen = Arc::new(Mutex::new(None));
    let rejection = Arc::new(Mutex::new(None));
    let handler = ClientHandler {
        id: id.clone(),
        seen: seen.clone(),
        rejection: rejection.clone(),
    };
    let config = client_config();

    let (mut handle, jump_handles) =
        if let Some(jump) = target.jump.as_deref().filter(|j| !j.trim().is_empty()) {
            connect_via_jump(target, jump, config, handler, rejection.clone()).await?
        } else {
            let stream = connect_transport(&target.host, target.port).await?;
            let handshake = net::stage_timeout(
                &format!("SSH handshake to {id}"),
                client::connect_stream(config, stream, handler),
            )
            .await;
            match handshake {
                Ok(handle) => (handle, Vec::new()),
                Err(e) => {
                    // 指纹不匹配时 russh 只报 "Unknown server key"：用我们记下的原因替换它
                    return Err(rejection.lock().unwrap().clone().unwrap_or(e));
                }
            }
        };

    authenticate(&mut handle, target).await?;

    // 握手通过且认证成功才记录指纹：失败连接不该污染 known_hosts
    // 记录失败不阻断使用：下次仍按 TOFU 处理，只是这台主机的指纹不会被记住
    if let Some(line) = seen.lock().unwrap().clone()
        && !line.is_empty()
    {
        _ = save_known_host(&id, &line);
    }

    Ok(Connection {
        handle,
        jump_handles,
    })
}

/// 建一条到 `host:port` 的 TCP 流（配置里 `proxy` 非空时经代理，不读环境变量）
async fn connect_transport(host: &str, port: u16) -> Result<Box<dyn TransportStream>, String> {
    let configured = RemoteConfig::load().proxy;
    let stream: Box<dyn TransportStream> = match proxy::from_url(configured.as_deref())? {
        Some(proxy) => {
            let s = net::stage_timeout(
                &format!("proxy connect to {host}:{port} via {}", proxy.describe()),
                proxy.connect(host, port),
            )
            .await?;
            Box::new(s)
        }
        None => {
            let s = net::stage_timeout(
                &format!("TCP connect to {host}:{port}"),
                TcpStream::connect((host, port)),
            )
            .await?;
            Box::new(s)
        }
    };
    Ok(stream)
}

/// 经跳板机连接：先连并认证跳板机，在它上面开 `direct-tcpip` 通道，再在通道上握手
async fn connect_via_jump(
    target: &Target,
    jump: &str,
    config: Arc<client::Config>,
    handler: ClientHandler,
    rejection: Arc<Mutex<Option<String>>>,
) -> Result<(Handle<ClientHandler>, Vec<Handle<ClientHandler>>), String> {
    let (jump_target, _) = parse_jump(jump)?;
    let jump_conn = Box::pin(connect_jump(&jump_target)).await?;
    let jump_handle = jump_conn.handle;

    let channel = net::stage_timeout(
        &format!(
            "open jump tunnel {}:{} → {}:{}",
            jump_target.host, jump_target.port, target.host, target.port
        ),
        jump_handle.channel_open_direct_tcpip(
            target.host.clone(),
            target.port as u32,
            "127.0.0.1".to_string(),
            0,
        ),
    )
    .await?;

    let handshake = net::stage_timeout(
        &format!("SSH handshake to {}:{} via jump", target.host, target.port),
        client::connect_stream(config, channel.into_stream(), handler),
    )
    .await;

    match handshake {
        Ok(handle) => Ok((handle, vec![jump_handle])),
        Err(e) => Err(rejection.lock().unwrap().clone().unwrap_or(e)),
    }
}

/// 连接跳板机本身（不再嵌套跳板，避免无限递归）
async fn connect_jump(target: &Target) -> Result<Connection, String> {
    let id = target.id();
    let seen = Arc::new(Mutex::new(None));
    let rejection = Arc::new(Mutex::new(None));
    let handler = ClientHandler {
        id: id.clone(),
        seen,
        rejection: rejection.clone(),
    };
    let stream = connect_transport(&target.host, target.port).await?;
    let handshake = net::stage_timeout(
        &format!("SSH handshake to jump host {id}"),
        client::connect_stream(client_config(), stream, handler),
    )
    .await;
    let mut handle = match handshake {
        Ok(handle) => handle,
        Err(e) => return Err(rejection.lock().unwrap().clone().unwrap_or(e)),
    };
    authenticate(&mut handle, target).await?;
    Ok(Connection {
        handle,
        jump_handles: Vec::new(),
    })
}

/// 解析 `-J` 的跳板机串（支持 `[user@]host[:port]`）
pub fn parse_jump(value: &str) -> Result<(Target, ()), String> {
    let value = value.trim();
    let (user, hostport) = match value.split_once('@') {
        Some((u, h)) => (u.to_string(), h),
        None => (
            std::env::var("USER").unwrap_or_else(|_| "root".to_string()),
            value,
        ),
    };

    let (host, port) = net::split_host_port(hostport, 22);
    if host.is_empty() {
        return Err("jump host is empty".to_string());
    }

    Ok((
        Target {
            host,
            port,
            user,
            identity: None,
            password_file: None,
            jump: None,
        },
        (),
    ))
}

/// 认证：公钥认证优先，失败后（有密码来源时）尝试密码与 keyboard-interactive。
///
/// 显式 `-i` 保持「排他」语义：只有在同时给出 `PasswordFile` 或设置了
/// `PRUX_SSH_PASSWORD` 时才回退到密码。没有可用密钥时直接走密码。
async fn authenticate(handle: &mut Handle<ClientHandler>, target: &Target) -> Result<(), String> {
    let mut last_error = String::new();
    let mut candidates: Vec<PathBuf> = Vec::new();

    if let Some(identity) = target.identity.as_deref().filter(|s| !s.trim().is_empty()) {
        let path = expand_tilde_path(identity.trim().to_string());
        if path.exists() {
            candidates.push(path);
        } else {
            // 显式路径写错时不要立刻失败：同时给了密码来源就还有别的路可走
            last_error = format!("identity file not found: {}", path.display());
        }
    } else if let Some(ssh_dir) = ssh_remote::sshcfg::ssh_dir() {
        for name in DEFAULT_KEYS {
            let path = ssh_dir.join(name);
            if path.exists() {
                candidates.push(path);
            }
        }
    }

    for path in candidates {
        let key = match load_key(&path, target).await {
            Ok(key) => key,
            Err(e) => {
                last_error = e;
                continue;
            }
        };

        // RSA 必须显式指定签名哈希，其它算法自带
        let hash = key
            .algorithm()
            .is_rsa()
            .then_some(russh::keys::HashAlg::Sha256);

        let with_hash = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        let result = net::stage_timeout(
            &format!("public-key auth at {} as {}", target.id(), target.user),
            handle.authenticate_publickey(&target.user, with_hash),
        )
        .await?;

        if result.success() {
            return Ok(());
        }

        last_error = format!("{}: rejected by server", path.display());
    }

    if password_allowed(target) {
        let scope = target.scope();
        let prompt = format!("password for {scope}");

        match auth::acquire(
            &scope,
            auth::SecretKind::Password,
            &prompt,
            target.password_file.as_deref(),
        )
        .await
        {
            auth::Acquired::Found(password) => {
                let result = net::stage_timeout(
                    &format!("password auth at {} as {}", target.id(), target.user),
                    handle.authenticate_password(&target.user, password.clone()),
                )
                .await?;

                if result.success() {
                    return Ok(());
                }

                // 只开 keyboard-interactive 的服务器（常见于 PAM 配置）用同一密码再试一次
                if keyboard_interactive(handle, target, &password).await? {
                    return Ok(());
                }
                last_error = "password rejected by server".to_string();
            }
            auth::Acquired::Rejected(reason) => last_error = reason,
            auth::Acquired::Missing => {
                if last_error.is_empty() {
                    last_error = "no private key and no password available".to_string();
                }
            }
        }
    } else if last_error.is_empty() {
        last_error = format!(
            "no usable private key for {}; pass one with `-i <path>`, set IdentityFile in ~/.ssh/config, or add `-o PasswordFile=<path>`",
            target.scope()
        );
    }

    Err(format!(
        "authentication failed for {} ({last_error})",
        target.scope()
    ))
}

/// 是否允许在公钥失败后尝试密码
///
/// 没有显式 `-i` 时总是允许（默认密钥失败后弹一次密码框）；
/// 显式 `-i` 时只在同时提供了密码来源时才允许。
fn password_allowed(target: &Target) -> bool {
    if target.password_file.is_some() {
        return true;
    }

    if std::env::var("PRUX_SSH_PASSWORD").is_ok_and(|v| !v.is_empty()) {
        return true;
    }

    target.identity.is_none()
}

/// 加载私钥；口令保护时询问口令并重试一次
async fn load_key(path: &Path, target: &Target) -> Result<PrivateKey, String> {
    match load_secret_key(path, None) {
        Ok(key) => Ok(key),
        Err(first) => {
            let scope = target.scope();
            let prompt = format!("passphrase for {}", path.display());
            match auth::acquire(
                &scope,
                auth::SecretKind::KeyPassphrase,
                &prompt,
                None::<&str>,
            )
            .await
            {
                auth::Acquired::Found(passphrase) => load_secret_key(path, Some(&passphrase))
                    .map_err(|e| format!("{}: {e}", path.display())),
                auth::Acquired::Rejected(reason) => {
                    Err(format!("{}: {first} ({reason})", path.display()))
                }
                auth::Acquired::Missing => Err(format!(
                    "{}: {first} (encrypted key needs a passphrase)",
                    path.display()
                )),
            }
        }
    }
}

/// keyboard-interactive 认证：把所有提问都填成同一个密码（PAM 里通常只有一项）
async fn keyboard_interactive(
    handle: &mut Handle<ClientHandler>,
    target: &Target,
    password: &str,
) -> Result<bool, String> {
    use russh::client::KeyboardInteractiveAuthResponse as Response;
    let start = net::stage_timeout(
        &format!("keyboard-interactive auth at {}", target.id()),
        handle.authenticate_keyboard_interactive_start(&target.user, None::<String>),
    )
    .await?;

    match start {
        Response::Success => Ok(true),
        Response::Failure { .. } => Ok(false),
        Response::InfoRequest { prompts, .. } => {
            let responses: Vec<String> = prompts.iter().map(|_| password.to_string()).collect();
            let next = net::stage_timeout(
                &format!("keyboard-interactive response at {}", target.id()),
                handle.authenticate_keyboard_interactive_respond(responses),
            )
            .await?;
            Ok(matches!(next, Response::Success))
        }
    }
}

/// 客户端配置：30 秒 keepalive（NAT / 防火墙下长连接不至于被静默回收）
fn client_config() -> Arc<client::Config> {
    Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        ..<_>::default()
    })
}

/// 执行一条命令并收集输出（一次性捕获，用于 sf 之外的短命令）
pub async fn exec_capture(
    handle: &Handle<ClientHandler>,
    command: &str,
    cwd: &str,
    max_bytes: usize,
    timeout: Duration,
) -> Result<CommandOutput, String> {
    let full = process::wrap_cd(command, cwd);
    let collected = tokio::time::timeout(timeout, async {
        let mut channel = handle
            .channel_open_session()
            .await
            .map_err(|e| format!("open exec channel: {e}"))?;
        channel
            .exec(true, full.as_bytes())
            .await
            .map_err(|e| format!("send command: {e}"))?;

        let mut stdout: Vec<u8> = Vec::new();
        let mut stderr: Vec<u8> = Vec::new();
        let mut exit_code = None;
        let mut truncated = false;

        while let Some(msg) = channel.wait().await {
            match msg {
                ChannelMsg::Data { data } => {
                    truncate::append_bounded(&mut stdout, &data, max_bytes, &mut truncated);
                }
                ChannelMsg::ExtendedData { data, .. } => {
                    truncate::append_bounded(&mut stderr, &data, max_bytes, &mut truncated);
                }
                ChannelMsg::ExitStatus { exit_status } => exit_code = Some(exit_status as i32),
                ChannelMsg::Close => break,
                _ => {}
            }
        }
        Ok::<CommandOutput, String>(CommandOutput {
            stdout: String::from_utf8_lossy(&stdout).to_string(),
            stderr: String::from_utf8_lossy(&stderr).to_string(),
            exit_code,
            truncated,
        })
    })
    .await;

    match collected {
        Ok(Ok(out)) => Ok(out),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(format!(
            "remote command timed out after {}s",
            timeout.as_secs()
        )),
    }
}

/// 一次性命令的捕获结果
#[derive(Debug, Clone, Default)]
pub struct CommandOutput {
    /// 标准输出
    pub stdout: String,
    /// 标准错误
    pub stderr: String,
    /// 退出码；被信号终止或截断时为 None
    pub exit_code: Option<i32>,
    /// 输出是否因上限被截断（截断时命令会被中止）
    pub truncated: bool,
}

impl CommandOutput {
    /// 合并输出（远端命令的 stderr 往往带关键信息，不能丢）
    pub fn combined(&self) -> String {
        if self.stderr.trim().is_empty() {
            self.stdout.clone()
        } else if self.stdout.trim().is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n{}", self.stdout, self.stderr)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jump_host() {
        let (t, _) = parse_jump("deploy@bastion:2222").unwrap();
        assert_eq!(t.user, "deploy");
        assert_eq!(t.host, "bastion");
        assert_eq!(t.port, 2222);
    }
}

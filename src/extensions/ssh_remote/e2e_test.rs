//! 进程内端到端测试：真的起一个 SSH + SFTP 服务端，验证连接栈与远程工具族。
//!
//! 只监听回环、只跑在测试里。服务端把 `/` 映射到临时目录（SFTP 侧），`exec` 真的用
//! `sh -c` 在临时目录里执行，因此 SFTP 与 exec 走的是完整协议路径，而不是被 mock 掉。
//!
//! 客户端私钥是随测试内嵌的固定 ed25519 测试密钥（服务端 `auth_publickey` 直接接受），
//! 不是任何真实环境的凭据。

use super::{command, connect, state, tool, transport};
use russh::{
    Channel, ChannelId,
    keys::PrivateKey,
    server::{Auth, Msg, Server as _, Session},
};
use russh_sftp::{
    protocol::{
        Attrs, Data, File as SftpFile, FileAttributes, Handle, Name, OpenFlags, Status, StatusCode,
    },
    server::Handler as SftpHandler,
};
use std::{
    collections::HashMap,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::net::TcpListener;

/// 只接受密码的测试用户名
const PASSWORD_USER: &str = "pwuser";
/// 该用户的测试密码
const PASSWORD_VALUE: &str = "s3cret-pass";

/// 测试用 ed25519 私钥（OpenSSH 格式，仅测试）
const TEST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACCgyMZOI+XWUaG2HHfkGxDI0hV3MrJCojMZOR3gTunB1gAAAJBVbgxvVW4M
bwAAAAtzc2gtZWQyNTUxOQAAACCgyMZOI+XWUaG2HHfkGxDI0hV3MrJCojMZOR3gTunB1g
AAAEB0LcN/nC5++cDN//jY7HgVYz30yW9lH9bJtw6d6VfeAKDIxk4j5dZRobYcd+QbEMjS
FXcyskKiMxk5HeBO6cHWAAAACXBydXgtdGVzdAECAwQ=
-----END OPENSSH PRIVATE KEY-----";

/// 服务端 handler
struct TestServer {
    /// `/` 对应的本地临时目录
    root: Arc<PathBuf>,
}

impl russh::server::Server for TestServer {
    type Handler = TestSession;

    fn new_client(&mut self, _peer: Option<SocketAddr>) -> Self::Handler {
        TestSession {
            root: self.root.clone(),
            channels: HashMap::new(),
        }
    }
}

/// 单连接会话
struct TestSession {
    /// `/` 对应的本地临时目录
    root: Arc<PathBuf>,
    /// 打开的 session 通道（SFTP 子系统要用）
    channels: HashMap<ChannelId, Channel<Msg>>,
}

/// 把远端绝对路径映射到临时目录下的本地路径
fn local_path(root: &Path, remote: &str) -> PathBuf {
    // exec 的 `pwd -P` 给的是宿主的绝对路径；真实 SFTP 与 exec 共享同一个文件系统，
    // 所以已经落在 root 里的路径要原样使用，不能再拼一次 root
    let direct = Path::new(remote);
    if direct.starts_with(root) {
        return direct.to_path_buf();
    }
    let trimmed = remote.trim_start_matches('/');
    if trimmed.is_empty() {
        root.to_path_buf()
    } else {
        root.join(trimmed)
    }
}

/// SFTP 状态回复
fn ok_status(id: u32) -> Status {
    Status {
        id,
        status_code: StatusCode::Ok,
        error_message: "Ok".to_string(),
        language_tag: "en-US".to_string(),
    }
}

/// io 错误 → SFTP 状态码
fn io_status(e: &std::io::Error) -> StatusCode {
    match e.kind() {
        std::io::ErrorKind::NotFound => StatusCode::NoSuchFile,
        std::io::ErrorKind::PermissionDenied => StatusCode::PermissionDenied,
        _ => StatusCode::Failure,
    }
}

impl russh::server::Handler for TestSession {
    type Error = russh::Error;

    /// 认证：`tester` 走公钥（一律接受），`pwuser` 只接受密码 `s3cret`
    async fn auth_publickey(
        &mut self,
        user: &str,
        _public_key: russh::server::VerifiedPublicKey<'_>,
    ) -> Result<Auth, Self::Error> {
        if user == PASSWORD_USER {
            return Ok(Auth::Reject {
                proceed_with_methods: None,
                partial_success: false,
            });
        }
        Ok(Auth::Accept)
    }

    /// 密码认证：只有 `pwuser` + 正确密码才通过
    async fn auth_password(&mut self, user: &str, password: &str) -> Result<Auth, Self::Error> {
        if user == PASSWORD_USER && password == PASSWORD_VALUE {
            return Ok(Auth::Accept);
        }
        Ok(Auth::Reject {
            proceed_with_methods: None,
            partial_success: false,
        })
    }

    /// 保存通道，供 SFTP 子系统接管
    async fn channel_open_session(
        &mut self,
        channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.insert(channel.id(), channel);
        reply.accept().await;
        Ok(())
    }

    /// 真的执行命令并把 stdout/stderr 送回客户端
    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).to_string();
        let root = self.root.clone();
        let handle = session.handle();
        tokio::spawn(async move {
            let output = tokio::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .current_dir(root.as_path())
                .output()
                .await;
            match output {
                Ok(out) => {
                    if !out.stdout.is_empty() {
                        _ = handle.data(channel, out.stdout).await;
                    }
                    if !out.stderr.is_empty() {
                        _ = handle.extended_data(channel, 1, out.stderr).await;
                    }
                    let code = out.status.code().unwrap_or(0) as u32;
                    _ = handle.exit_status_request(channel, code).await;
                }
                Err(_) => {
                    _ = handle.exit_status_request(channel, 127).await;
                }
            }
            _ = handle.close(channel).await;
        });
        Ok(())
    }

    /// SFTP 子系统：交给 russh-sftp 的服务端跑
    async fn subsystem_request(
        &mut self,
        channel_id: ChannelId,
        name: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name != "sftp" {
            session.channel_failure(channel_id)?;
            return Ok(());
        }
        let Some(channel) = self.channels.remove(&channel_id) else {
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        session.channel_success(channel_id)?;
        let root = self.root.clone();
        tokio::spawn(async move {
            let handler = LocalSftp {
                root,
                open_dirs: HashMap::new(),
            };
            russh_sftp::server::run(channel.into_stream(), handler).await;
        });
        Ok(())
    }

    /// 未处理的通道消息
    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        self.channels.remove(&channel);
        Ok(())
    }
}

/// 以本地临时目录为根的最小 SFTP 服务端
struct LocalSftp {
    /// `/` 对应的本地临时目录
    root: Arc<PathBuf>,
    /// 已经返回过内容的目录句柄
    open_dirs: HashMap<String, bool>,
}

impl SftpHandler for LocalSftp {
    type Error = StatusCode;

    /// 未实现的操作统一回 OpUnsupported
    fn unimplemented(&self) -> Self::Error {
        StatusCode::OpUnsupported
    }

    /// 打开文件：句柄直接用路径
    async fn open(
        &mut self,
        id: u32,
        filename: String,
        _pflags: OpenFlags,
        _attrs: FileAttributes,
    ) -> Result<Handle, Self::Error> {
        let path = local_path(&self.root, &filename);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if !path.exists() {
            std::fs::File::create(&path).map_err(|e| io_status(&e))?;
        }
        Ok(Handle {
            id,
            handle: filename,
        })
    }

    /// 关闭文件
    async fn close(&mut self, id: u32, _handle: String) -> Result<Status, Self::Error> {
        Ok(ok_status(id))
    }

    /// 按偏移读文件
    async fn read(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        len: u32,
    ) -> Result<Data, Self::Error> {
        use std::io::{Read, Seek, SeekFrom};
        let path = local_path(&self.root, &handle);
        let mut file = std::fs::File::open(&path).map_err(|e| io_status(&e))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| io_status(&e))?;
        let mut buf = vec![0u8; len as usize];
        let n = file.read(&mut buf).map_err(|e| io_status(&e))?;
        if n == 0 && offset > 0 {
            return Err(StatusCode::Eof);
        }
        buf.truncate(n);
        Ok(Data { id, data: buf })
    }

    /// 按偏移写文件
    async fn write(
        &mut self,
        id: u32,
        handle: String,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<Status, Self::Error> {
        use std::io::{Seek, SeekFrom, Write};
        let path = local_path(&self.root, &handle);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io_status(&e))?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|e| io_status(&e))?;
        file.seek(SeekFrom::Start(offset))
            .map_err(|e| io_status(&e))?;
        file.write_all(&data).map_err(|e| io_status(&e))?;
        Ok(ok_status(id))
    }

    /// 元数据（跟随符号链接）
    async fn stat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let local = local_path(&self.root, &path);
        let meta = std::fs::metadata(&local).map_err(|e| io_status(&e))?;
        Ok(Attrs {
            id,
            attrs: attributes_of(&meta),
        })
    }

    /// 元数据（不跟随符号链接）
    async fn lstat(&mut self, id: u32, path: String) -> Result<Attrs, Self::Error> {
        let local = local_path(&self.root, &path);
        let meta = std::fs::symlink_metadata(&local).map_err(|e| io_status(&e))?;
        Ok(Attrs {
            id,
            attrs: attributes_of(&meta),
        })
    }

    /// 句柄元数据（句柄就是路径）
    async fn fstat(&mut self, id: u32, handle: String) -> Result<Attrs, Self::Error> {
        self.stat(id, handle).await
    }

    /// 打开目录：句柄用路径
    async fn opendir(&mut self, id: u32, path: String) -> Result<Handle, Self::Error> {
        self.open_dirs.insert(path.clone(), false);
        Ok(Handle { id, handle: path })
    }

    /// 列目录：第一次返回全部条目，之后 EOF
    async fn readdir(&mut self, id: u32, handle: String) -> Result<Name, Self::Error> {
        if self.open_dirs.get(&handle).copied().unwrap_or(true) {
            return Err(StatusCode::Eof);
        }
        self.open_dirs.insert(handle.clone(), true);
        let local = local_path(&self.root, &handle);
        let entries = std::fs::read_dir(&local).map_err(|e| io_status(&e))?;
        let mut files = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let meta = entry.metadata().ok();
            files.push(SftpFile::new(
                name,
                meta.map(|m| attributes_of(&m)).unwrap_or_default(),
            ));
        }
        Ok(Name { id, files })
    }

    /// 解析为真实路径（这里原样返回）
    async fn realpath(&mut self, id: u32, path: String) -> Result<Name, Self::Error> {
        let path = if path == "." { "/".to_string() } else { path };
        Ok(Name {
            id,
            files: vec![SftpFile::dummy(path)],
        })
    }

    /// 建目录
    async fn mkdir(
        &mut self,
        id: u32,
        path: String,
        _attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        let local = local_path(&self.root, &path);
        std::fs::create_dir_all(&local).map_err(|e| io_status(&e))?;
        Ok(ok_status(id))
    }

    /// 删除文件
    async fn remove(&mut self, id: u32, filename: String) -> Result<Status, Self::Error> {
        let local = local_path(&self.root, &filename);
        std::fs::remove_file(&local).map_err(|e| io_status(&e))?;
        Ok(ok_status(id))
    }

    /// 删除目录
    async fn rmdir(&mut self, id: u32, path: String) -> Result<Status, Self::Error> {
        let local = local_path(&self.root, &path);
        std::fs::remove_dir(&local).map_err(|e| io_status(&e))?;
        Ok(ok_status(id))
    }

    /// 修改属性：只实现截断（客户端写文件时会带 size=0）
    async fn setstat(
        &mut self,
        id: u32,
        path: String,
        attrs: FileAttributes,
    ) -> Result<Status, Self::Error> {
        if let Some(size) = attrs.size {
            let local = local_path(&self.root, &path);
            let file = std::fs::OpenOptions::new()
                .write(true)
                .open(&local)
                .map_err(|e| io_status(&e))?;
            file.set_len(size).map_err(|e| io_status(&e))?;
        }
        Ok(ok_status(id))
    }
}

/// 从本地元数据构造 SFTP 属性
fn attributes_of(meta: &std::fs::Metadata) -> FileAttributes {
    #[cfg(unix)]
    let permissions = {
        use std::os::unix::fs::PermissionsExt;
        Some(meta.permissions().mode())
    };
    #[cfg(not(unix))]
    let permissions = Some(if meta.permissions().readonly() {
        0o444
    } else {
        0o644
    });
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as u32);
    let mut attrs = FileAttributes {
        size: Some(meta.len()),
        permissions,
        mtime,
        ..Default::default()
    };
    attrs.set_dir(meta.is_dir());
    attrs
}

/// 起一个测试服务端，返回 (端口, 根目录, 关闭信号)
async fn start_server() -> (u16, Arc<PathBuf>, tokio::sync::watch::Sender<bool>) {
    let root = Arc::new(std::env::temp_dir().join(format!(
        "prux-ssh-e2e-{}-{}",
        std::process::id(),
        fastrand()
    )));
    std::fs::create_dir_all(root.as_path()).unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("data.txt"), "alpha\nbeta\ngamma\ndelta\n").unwrap();
    for i in 0..600 {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("big.log"))
            .unwrap();
        writeln!(f, "line {i}").unwrap();
    }

    let host_key = PrivateKey::from_openssh(TEST_KEY).unwrap();
    let config = Arc::new(russh::server::Config {
        keys: vec![host_key],
        auth_rejection_time: Duration::from_millis(1),
        auth_rejection_time_initial: Some(Duration::from_millis(0)),
        inactivity_timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    });

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, mut rx) = tokio::sync::watch::channel(false);

    let server_root = root.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = rx.changed() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _peer)) = accepted else { break };
                    let config = config.clone();
                    let mut server = TestServer { root: server_root.clone() };
                    tokio::spawn(async move {
                        let handler = server.new_client(None);
                        // RunningSession 本身就是 Future：必须 await 到连接结束，否则 drop 即断开
                        if let Ok(running) = russh::server::run_stream(config, stream, handler).await {
                            let _ = running.await;
                        }
                    });
                }
            }
        }
    });

    (port, root, tx)
}

/// 极简随机数（只用于临时目录名）
fn fastrand() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0)
}

/// 端到端：连接 → 六个需要连接的远程工具 → 密码认证 → 主机密钥变化被拒绝
///
/// 一个测试跑完全部场景：连接池与凭据缓存都是进程级状态，
/// 拆成多个并发测试会互相顶掉。
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[allow(clippy::await_holding_lock)] // 锁只用来串行化进程级 UI 队列，见下方注释
async fn connects_runs_remote_tools_and_enforces_host_keys() {
    // 本用例经 `command::handle` 调 `/ssh disconnect`、`/ssh cd` 等，都会往进程级
    // UI 队列推通知；subagent / plan_mode 的用例读同一队列并断言「没有多余通知」，
    // 不串行时它们会抓到本用例排的请求。锁序：AUTH → TEST_ROOT_LOCK。
    let _auth = crate::test_support::AUTH_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // 状态隔离用 ssh-remote 自己的根目录覆盖：连接流程的路径读取发生在
    // tokio 的任意 worker 线程上，线程本地 guard 会漏
    let _lock = state::TEST_ROOT_LOCK.lock().await;
    let _state = state::TestRootGuard::temp();
    let (port, root, shutdown) = start_server().await;

    // 客户端私钥：写在状态临时目录里，用显式 -i 传入（不碰真实 HOME）
    let key_dir = state::config_dir();
    std::fs::create_dir_all(&key_dir).unwrap();
    let key_path = key_dir.join("id_ed25519");
    std::fs::write(&key_path, TEST_KEY).unwrap();

    let endpoint = state::Endpoint {
        command: format!("ssh tester@127.0.0.1:{port}"),
        host: "127.0.0.1".to_string(),
        user: "tester".to_string(),
        port,
        identity: Some(key_path.to_string_lossy().to_string()),
        password_file: None,
        jump: None,
        note: "e2e".to_string(),
        // exec 在服务端以临时目录为 cwd 跑，所以默认目录用 `.`
        cwd: ".".to_string(),
    };
    let key = endpoint.key();

    // 1) 连接（公钥），并且真的进了连接池
    let conn = transport::ensure(endpoint.clone())
        .await
        .expect("public key connect");
    assert!(conn.is_alive());
    let connected = transport::connected();
    assert_eq!(connected.len(), 1);
    assert_eq!(connected[0].key(), key);

    // 1b) `/ssh disconnect <target>`：拼错/半截的 target 不得动别的连接，命中的才断
    // （旧实现用 `command.contains(arg)` 扫 known，`127.0.0` 这种半截串会把连接一并断掉）
    let mut app = crate::modes::interactive::app::App::new();
    command::handle(&mut app, "ssh disconnect 127.0.0");
    assert_eq!(transport::connected().len(), 1, "拼错的 target 断开了连接");
    command::handle(&mut app, &format!("ssh disconnect {key}"));
    assert!(transport::connected().is_empty(), "命中的 target 应断开");
    let conn = transport::ensure(endpoint.clone())
        .await
        .expect("reconnect after disconnect");

    // 1c) `/ssh cd` 改的是配置里的端点，已连接的那条也得跟着换目录：否则老连接继续用
    // 建连时的 cwd（以前 `/ssh list` 只印 key，改没改成也看不出来）
    {
        let mut cfg = state::RemoteConfig::load();
        cfg.set_default(endpoint.clone());
        cfg.save().unwrap();
    }
    command::handle(&mut app, "ssh cd /");
    assert_eq!(
        transport::connected()[0].cwd,
        "/",
        "已连接的端点没跟着 `/ssh cd` 换目录"
    );
    let out = transport::exec(&conn, "pwd", None, None, 64 * 1024)
        .await
        .expect("exec after cd");
    assert_eq!(out.stdout.trim(), "/", "exec 仍在旧 cwd 里跑");
    command::handle(&mut app, "ssh cd .");

    // 1d) 重启后重新 `/ssh` 同一目标：记住的 cwd 与备注不能被默认值冲掉。
    // 断开全部连接等价于重启后的空连接池，重新登录要能接回旧设置。
    command::handle(&mut app, "ssh note restart-check");
    transport::disconnect_all();
    command::handle(
        &mut app,
        &format!(
            "ssh -i {} tester@127.0.0.1:{port}",
            key_path.to_string_lossy()
        ),
    );
    let mut relogged = None;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Some(ep) = transport::connected()
            .into_iter()
            .find(|ep| ep.key() == key)
        {
            relogged = Some(ep);
            break;
        }
    }
    let relogged = relogged.expect("重新登录应在探测成功后重新连上");
    assert_eq!(relogged.cwd, ".", "重新登录把 cwd 冲回了默认值");
    assert_eq!(relogged.note, "restart-check", "重新登录把备注清掉了");
    let saved = state::RemoteConfig::load()
        .default
        .expect("重新登录应写回默认目标");
    assert_eq!(saved.cwd, ".", "落盘的 cwd 被重置了");
    assert_eq!(saved.note, "restart-check", "落盘的备注被清掉了");
    let conn = transport::ensure(endpoint.clone())
        .await
        .expect("reuse relogin");

    // 2) remote_exec：服务端真的跑 sh -c
    let out = transport::exec(&conn, "echo hello-from-remote", None, None, 64 * 1024)
        .await
        .expect("exec");
    assert!(out.stdout.contains("hello-from-remote"), "{out:?}");
    assert_eq!(out.exit_code, Some(0));

    let host_arg = serde_json::json!(key);
    let exec = tool::dispatch(
        tool::EXEC,
        &serde_json::json!({ "host": host_arg, "command": "echo via-tool", "cwd": "." }),
        ".",
    )
    .await
    .expect("remote_exec");
    assert!(exec.text.contains("via-tool"), "{}", exec.text);
    assert!(exec.text.contains("exit 0"), "{}", exec.text);

    // 2b) remote_pwd：报出端点记住的 cwd 与它在远端解析出的绝对路径
    let pwd = tool::dispatch(tool::PWD, &serde_json::json!({ "host": host_arg }), ".")
        .await
        .expect("remote_pwd");
    assert!(pwd.text.contains("cwd ."), "{}", pwd.text);
    assert!(
        pwd.text.lines().any(|l| l.starts_with("absolute: /")),
        "没报出远端绝对路径：{}",
        pwd.text
    );
    assert!(pwd.text.contains("\nhome: "), "{}", pwd.text);

    // 3) remote_ls：SFTP 列出临时根（服务端已放好 fixture）
    std::fs::write(root.join("notes.txt"), "alpha\nbeta\ngamma\n").unwrap();
    std::fs::create_dir_all(root.join("sub")).unwrap();
    let ls = tool::dispatch(
        tool::LS,
        &serde_json::json!({ "host": host_arg, "path": "/" }),
        ".",
    )
    .await
    .expect("remote_ls");
    assert!(ls.text.contains("notes.txt"), "{}", ls.text);
    assert!(ls.text.contains("sub/"), "{}", ls.text);

    // 4) remote_read：行范围语义与本地 read 一致
    let read = tool::dispatch(
        tool::READ,
        &serde_json::json!({ "host": host_arg, "path": "/notes.txt", "offset": 2, "limit": 1 }),
        ".",
    )
    .await
    .expect("remote_read");
    assert!(read.text.contains("beta"), "{}", read.text);
    assert!(!read.text.contains("alpha"), "{}", read.text);

    // 5) remote_grep：远端跑 rg/grep，命中带路径前缀
    let grep = tool::dispatch(
        tool::GREP,
        &serde_json::json!({ "host": host_arg, "pattern": "gamma", "path": "." }),
        ".",
    )
    .await
    .expect("remote_grep");
    assert!(grep.text.contains("gamma"), "{}", grep.text);

    // 6) remote_sync：push 后能在远端读到，dry-run 不写文件
    let local_dir = root.join("local-src");
    std::fs::create_dir_all(&local_dir).unwrap();
    std::fs::write(local_dir.join("a.txt"), "payload-a").unwrap();
    std::fs::create_dir_all(local_dir.join("nested")).unwrap();
    std::fs::write(local_dir.join("nested/b.txt"), "payload-b").unwrap();

    let dry = tool::dispatch(
        tool::SYNC,
        &serde_json::json!({
            "host": host_arg, "direction": "push",
            "local": local_dir.to_string_lossy(), "remote": "/uploaded", "dry_run": true
        }),
        ".",
    )
    .await
    .expect("dry run");
    assert!(dry.text.contains("would push 2 files"), "{}", dry.text);
    assert!(!root.join("uploaded").exists(), "dry run 不应写文件");

    let pushed = tool::dispatch(
        tool::SYNC,
        &serde_json::json!({
            "host": host_arg, "direction": "push",
            "local": local_dir.to_string_lossy(), "remote": "/uploaded"
        }),
        ".",
    )
    .await
    .expect("push");
    assert!(pushed.text.contains("pushed 2 files"), "{}", pushed.text);
    assert_eq!(
        std::fs::read_to_string(root.join("uploaded/a.txt")).unwrap(),
        "payload-a"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("uploaded/nested/b.txt")).unwrap(),
        "payload-b"
    );

    // pull 到本地另一个目录
    let pull_dir = root.join("pulled");
    std::fs::create_dir_all(&pull_dir).unwrap();
    let pulled = tool::dispatch(
        tool::SYNC,
        &serde_json::json!({
            "host": host_arg, "direction": "pull",
            "local": pull_dir.to_string_lossy(), "remote": "/uploaded"
        }),
        ".",
    )
    .await
    .expect("pull");
    assert!(pulled.text.contains("pulled 2 files"), "{}", pulled.text);
    assert_eq!(
        std::fs::read_to_string(pull_dir.join("a.txt")).unwrap(),
        "payload-a"
    );

    // 6b) 工具主动解析工作目录：记住的 cwd 换成相对路径后，SFTP 族也必须拿到绝对路径
    // （SFTP 不会像 `exec` 那样先 `cd`，也不认 `~`；以前相对路径直接交给 SFTP，语义就漂了）
    transport::reset_turn_budget();
    command::handle(&mut app, "ssh cd sub");
    let sub_ls = tool::dispatch(tool::LS, &serde_json::json!({ "host": host_arg }), ".")
        .await
        .expect("remote_ls in sub");
    assert!(
        sub_ls
            .text
            .lines()
            .next()
            .unwrap_or_default()
            .contains(":/"),
        "路径没有按工作目录解析成绝对路径：{}",
        sub_ls.text
    );
    command::handle(&mut app, "ssh cd .");
    let rel_read = tool::dispatch(
        tool::READ,
        &serde_json::json!({ "host": host_arg, "path": "data.txt", "limit": 1 }),
        ".",
    )
    .await
    .expect("remote_read with relative path");
    let canonical = std::fs::canonicalize(root.as_path()).unwrap();
    assert!(
        rel_read
            .text
            .contains(&format!("{}/data.txt", canonical.display())),
        "相对路径没拼到工作目录上：{}",
        rel_read.text
    );
    assert!(rel_read.text.contains("alpha"), "{}", rel_read.text);

    // 7) 池按端点复用：同一个 key 再取一次不会新建连接
    let again = transport::ensure(endpoint.clone()).await.expect("reuse");
    assert!(Arc::ptr_eq(&again, &conn), "同一端点应复用同一条连接");

    // 8) 密码认证：显式私钥不存在 + 提供了 0600 密码文件 → 走密码
    // 凭据缓存是进程级单例，与 auth 的单测互斥（先 TEST_ROOT 后 AUTH，锁序固定）
    let _auth_lock = crate::extensions::ssh_remote::auth::AUTH_TEST_LOCK
        .lock()
        .await;
    let pw_file = key_dir.join("pw");
    std::fs::write(&pw_file, PASSWORD_VALUE).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pw_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let pw_endpoint = state::Endpoint {
        command: format!("ssh {PASSWORD_USER}@127.0.0.1 -p {port}"),
        host: "127.0.0.1".to_string(),
        user: PASSWORD_USER.to_string(),
        port,
        identity: Some("/nonexistent/prux-e2e-key".to_string()),
        password_file: Some(pw_file.to_string_lossy().to_string()),
        jump: None,
        note: String::new(),
        cwd: ".".to_string(),
    };
    crate::extensions::ssh_remote::auth::forget_all();
    let pw_conn = transport::ensure(pw_endpoint.clone())
        .await
        .expect("password auth");
    let out = transport::exec(&pw_conn, "echo via-password", None, None, 4096)
        .await
        .expect("exec over password session");
    assert!(out.stdout.contains("via-password"));
    transport::disconnect(&pw_endpoint.key());

    // 密码文件权限过宽时必须拒绝读取（不静默降级到其它来源）
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pw_file, std::fs::Permissions::from_mode(0o644)).unwrap();
        crate::extensions::ssh_remote::auth::forget_all();
        let err = transport::ensure(pw_endpoint.clone())
            .await
            .expect_err("wide-permission credential file must be rejected");
        assert!(err.contains("chmod 600"), "{err}");
        transport::disconnect(&pw_endpoint.key());
        std::fs::set_permissions(&pw_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    // 错误密码必须失败
    std::fs::write(&pw_file, "wrong-pass").unwrap();
    crate::extensions::ssh_remote::auth::forget_all();
    let err = transport::ensure(pw_endpoint)
        .await
        .expect_err("wrong password must fail");
    assert!(err.contains("authentication failed"), "{err}");

    // 9) 主机密钥变化必须拒绝：篡改已知指纹后再连
    transport::disconnect(&key);
    let id = format!("127.0.0.1:{port}");
    state::save_known_host(&id, "ssh-ed25519 AAAA-bogus").unwrap();
    let err = transport::ensure(endpoint)
        .await
        .expect_err("changed host key must fail");
    assert!(err.contains("host key"), "{err}");
    assert!(err.contains("/ssh forget"), "{err}");

    let _ = shutdown.send(true);
    let _ = std::fs::remove_dir_all(root.as_path());
}

/// 连接层：目标标识与跳板机解析
#[test]
fn target_identity() {
    let target = connect::Target {
        host: "h".into(),
        port: 2222,
        user: "root".into(),
        identity: None,
        password_file: None,
        jump: None,
    };
    assert_eq!(target.id(), "h:2222");
    assert_eq!(target.scope(), "root@h:2222");

    let (jump, ()) = connect::parse_jump("deploy@bastion:2200").unwrap();
    assert_eq!(jump.user, "deploy");
    assert_eq!(jump.host, "bastion");
    assert_eq!(jump.port, 2200);

    let (plain, ()) = connect::parse_jump("bastion").unwrap();
    assert_eq!(plain.host, "bastion");
    assert_eq!(plain.port, 22);
}

/// SFTP 侧的远端根映射：绝对路径都落在临时目录里
#[test]
fn remote_paths_map_under_root() {
    let root = Path::new("/tmp/prux-root");
    assert_eq!(local_path(root, "/a/b"), root.join("a/b"));
    assert_eq!(local_path(root, "."), root.to_path_buf());
    assert_eq!(local_path(root, "/"), root.to_path_buf());
}

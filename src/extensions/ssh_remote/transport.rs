//! 多端点 SSH 连接池：按端点缓存连接，供工具与隧道复用。
//!
//! 与「接管工作区」的旧设计不同，这里没有「当前连接」这种全局状态：
//! 每次工具调用自己指定端点（`host` 参数或 `/ssh` 设的默认值），连接按
//! `user@host:port` 缓存在池里，多台机器可以同时在线、互不影响。
//!
//! 一条连接上可以并发开多条通道（exec 与 SFTP 各走各的），句柄失效时
//! [`exec`] 会按端点重连一次再重试。

use crate::extensions::ssh_remote::{
    command::parse_ssh_command,
    connect::{self, ClientHandler, CommandOutput},
    state::{Endpoint, Limits, RemoteConfig},
};
use russh::client::Handle;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex as AsyncMutex;

/// 连接池上限：超过后新建连接会先踢掉最久未用的一半，避免长跑会话把 fd 用光
const MAX_POOLED: usize = 8;

/// 串行化「建连/重连」：并发工具不会对同一端点重复握手
static CONNECT_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

/// 本回合已回给模型的远程输出字节数（每轮 `turn_start` 归零）
static TURN_OUTPUT_BYTES: AtomicUsize = AtomicUsize::new(0);

/// 端点标识 → 连接
static POOL: OnceLock<Mutex<HashMap<String, Arc<RemoteConn>>>> = OnceLock::new();

/// 连接池容器
fn pool() -> &'static Mutex<HashMap<String, Arc<RemoteConn>>> {
    POOL.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 一个已建立的端点连接（跳板机句柄必须与目标同寿，一起放在这里）
pub struct RemoteConn {
    /// 端点配置（用户、主机、端口、密钥、默认远端目录）
    ///
    /// 放在锁里是因为 `/ssh cd` / `/ssh note` 会就地改它：连接已建立时也要跟上，
    /// 否则下一次工具调用还用旧目录。
    endpoint: RwLock<Endpoint>,
    /// 目标主机句柄
    pub handle: Arc<Handle<ClientHandler>>,
    /// 跳板机句柄：跟着连接一起 drop，提前关掉会让目标通道一起断
    _jumps: Vec<Handle<ClientHandler>>,
}

impl std::fmt::Debug for RemoteConn {
    /// 只打印端点信息：句柄本身没有有意义的调试输出
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteConn")
            .field("endpoint", &self.endpoint().key())
            .field("alive", &self.is_alive())
            .finish()
    }
}

impl RemoteConn {
    /// 句柄是否仍然可用（网络断开后 russh 会把连接标为 closed）
    pub fn is_alive(&self) -> bool {
        !self.handle.is_closed()
    }

    /// 端点配置的副本：读一次就用这一份，别在同一段逻辑里读到两个版本
    pub fn endpoint(&self) -> Endpoint {
        self.endpoint
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// 归零本回合的远程输出预算
pub fn reset_turn_budget() {
    TURN_OUTPUT_BYTES.store(0, Ordering::Relaxed);
}

/// 申请本回合剩余预算里的 `wanted` 字节；返回真正可分到的上限（0 = 已用尽）
pub fn take_turn_budget(wanted: usize) -> usize {
    let remaining = limits().turn_max_bytes;
    let used = TURN_OUTPUT_BYTES.load(Ordering::Relaxed);
    let granted = wanted.min(remaining.saturating_sub(used));
    TURN_OUTPUT_BYTES.fetch_add(granted, Ordering::Relaxed);
    granted
}

/// 当前生效的输出上限与超时
pub fn limits() -> Limits {
    RemoteConfig::load().limits
}

/// 已连接端点列表（按 key 排序，供 `/ssh list` 与面板展示）
pub fn connected() -> Vec<Endpoint> {
    let mut map = pool().lock().unwrap();
    map.retain(|_, conn| conn.is_alive());
    let mut out: Vec<Endpoint> = map.values().map(|conn| conn.endpoint()).collect();
    out.sort_by_key(|e| e.key());
    out
}

/// 把更新过的端点写回池里的同名连接；返回是否命中活连接
///
/// `/ssh cd` / `/ssh note` 改的是配置里的端点，连接池里的那份也得跟着变，
/// 否则下一次 `remote_exec` 还会用建连时的旧目录。锁序：连接池 → 端点 `RwLock`。
pub fn sync_endpoint(ep: &Endpoint) -> bool {
    let mut map = pool().lock().unwrap();
    map.retain(|_, conn| conn.is_alive());
    let Some(conn) = map.get(&ep.key()) else {
        return false;
    };
    *conn.endpoint.write().unwrap_or_else(|e| e.into_inner()) = ep.clone();
    true
}

/// 按端点取连接：池里有活的就复用，否则新建
pub async fn ensure(ep: Endpoint) -> Result<Arc<RemoteConn>, String> {
    let key = ep.key();
    if let Some(conn) = lookup(&key) {
        return Ok(conn);
    }

    let _guard = CONNECT_LOCK.lock().await;
    if let Some(conn) = lookup(&key) {
        return Ok(conn);
    }

    let conn = Arc::new(connect_now(ep).await?);
    install(conn.clone());
    Ok(conn)
}

/// 解析默认端点（`/ssh <target>` 设置的那个）
pub async fn resolve_default() -> Result<Arc<RemoteConn>, String> {
    let cfg = RemoteConfig::load();
    match cfg.default {
        Some(ep) => ensure(ep).await,
        None => Err(
            "no SSH target: pass `host` (`[user@]host[:port]` or a ~/.ssh/config alias) or set a \
             default with `/ssh <target>`"
                .to_string(),
        ),
    }
}

/// 解析 `host` 参数：已连接 → 已知端点 → 当作 ssh 目标现场解析
pub async fn resolve_host(host: &str) -> Result<Arc<RemoteConn>, String> {
    let spec = host.trim();
    if spec.is_empty() {
        return resolve_default().await;
    }

    if let Some(conn) = find_connected(spec) {
        return Ok(conn);
    }

    let cfg = RemoteConfig::load();
    if let Some(ep) = cfg.known.iter().find(|ep| matches_spec(ep, spec)) {
        return ensure(ep.clone()).await;
    }

    let endpoint = parse_ssh_command(&format!("ssh {spec}"))?;
    ensure(endpoint).await
}

/// 在池里按「完整 key / 主机名 / 命令里的目标段」匹配一个活连接
fn find_connected(spec: &str) -> Option<Arc<RemoteConn>> {
    let mut map = pool().lock().unwrap();
    map.retain(|_, conn| conn.is_alive());
    map.values()
        .find(|conn| matches_spec(&conn.endpoint(), spec))
        .cloned()
}

/// 端点是否对应用户写的 `spec`
///
/// 用户可能写完整 key（`user@host:port`）、主机名、`host:port`，或 ssh 命令里的目标段
/// （`ssh root@alias` 里的 `root@alias`，也接受不带用户的 `alias`）。**不做子串匹配**：
/// `gpu` 不能命中 `gpu.example.com`，否则断连、删除这类操作会误伤。
pub fn matches_spec(ep: &Endpoint, spec: &str) -> bool {
    let spec = spec.trim();
    if spec.is_empty() {
        return false;
    }

    if ep.key() == spec || ep.host == spec || format!("{}:{}", ep.host, ep.port) == spec {
        return true;
    }

    ep.command
        .split_whitespace()
        // `endswith` 而不是 `contains`：`gpu` 不能命中命令里的 `root@gpu.example.com`
        .any(|word| word == spec || word.ends_with(&format!("@{spec}")))
}

/// 池内查询（顺手剔除已失效的句柄）
fn lookup(key: &str) -> Option<Arc<RemoteConn>> {
    let mut map = pool().lock().unwrap();
    match map.get(key) {
        Some(conn) if conn.is_alive() => Some(conn.clone()),
        Some(_) => {
            map.remove(key);
            None
        }
        None => None,
    }
}

/// 放入池中，必要时先踢掉一批旧连接
fn install(conn: Arc<RemoteConn>) {
    let mut map = pool().lock().unwrap();
    if map.len() >= MAX_POOLED {
        let mut keys: Vec<String> = map.keys().cloned().collect();
        keys.sort();
        for key in keys.into_iter().take(MAX_POOLED / 2) {
            map.remove(&key);
        }
    }
    map.insert(conn.endpoint().key(), conn);
}

/// 建立一条新连接（调用方必须已持有 [`CONNECT_LOCK`]，或明确知道只有自己在建）
async fn connect_now(ep: Endpoint) -> Result<RemoteConn, String> {
    let target = connect::Target {
        host: ep.host.clone(),
        port: ep.port,
        user: ep.user.clone(),
        identity: ep.identity.clone(),
        password_file: ep.password_file.clone(),
        jump: ep.jump.clone(),
    };

    let conn = connect::connect(&target).await?;
    remember(&ep);
    Ok(RemoteConn {
        endpoint: RwLock::new(ep),
        handle: Arc::new(conn.handle),
        _jumps: conn.jump_handles,
    })
}

/// 把端点记进配置的 known 列表（下次直接用解析结果，省掉 ssh config 查询）
fn remember(ep: &Endpoint) {
    let mut cfg = RemoteConfig::load();
    if cfg.remember(ep) {
        _ = cfg.save();
    }
}

/// 强制重连一个端点（旧句柄已失效时用）
async fn reconnect(ep: Endpoint) -> Result<Arc<RemoteConn>, String> {
    let _guard = CONNECT_LOCK.lock().await;
    let key = ep.key();
    pool().lock().unwrap().remove(&key);
    let conn = Arc::new(connect_now(ep).await?);
    install(conn.clone());
    Ok(conn)
}

/// 在一个连接上执行命令；通道打不开时按端点重连一次再重试
///
/// `cwd` 为 `None` 时用端点记住的默认远端目录。
pub async fn exec(
    conn: &Arc<RemoteConn>,
    command: &str,
    cwd: Option<&str>,
    timeout: Option<Duration>,
    max_bytes: usize,
) -> Result<CommandOutput, String> {
    let endpoint = conn.endpoint();
    let cwd = cwd
        .map(|c| c.to_string())
        .unwrap_or_else(|| endpoint.cwd.clone());
    let timeout = timeout.unwrap_or_else(|| Duration::from_secs(limits().timeout_seconds as u64));
    let first = connect::exec_capture(&conn.handle, command, &cwd, max_bytes, timeout).await;
    if first.is_ok() {
        return first;
    }

    let first_err = first.expect_err("checked is_ok above");
    let fresh = reconnect(endpoint)
        .await
        .map_err(|e| format!("{first_err}; reconnect failed: {e}"))?;
    connect::exec_capture(&fresh.handle, command, &cwd, max_bytes, timeout)
        .await
        .map_err(|second| format!("{first_err}; retried after reconnect: {second}"))
}

/// 断开通往某个端点的连接；返回是否真的断开了
pub fn disconnect(key: &str) -> bool {
    pool().lock().unwrap().remove(key).is_some()
}

/// 断开全部连接；返回数量
pub fn disconnect_all() -> usize {
    let mut map = pool().lock().unwrap();
    let count = map.len();
    map.clear();
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端点匹配：用户可能写 key、主机名或完整 ssh 目标
    #[test]
    fn spec_matching_accepts_key_host_and_command() {
        let ep = Endpoint {
            command: "ssh root@gpu1 -p 2202".to_string(),
            host: "10.0.0.5".to_string(),
            user: "root".to_string(),
            port: 2202,
            identity: None,
            password_file: None,
            jump: None,
            note: String::new(),
            cwd: "~".to_string(),
        };
        assert!(matches_spec(&ep, "root@10.0.0.5:2202"));
        assert!(matches_spec(&ep, "10.0.0.5"));
        assert!(matches_spec(&ep, "10.0.0.5:2202"));
        assert!(matches_spec(&ep, "root@gpu1"));
        assert!(matches_spec(&ep, "gpu1"), "别名不带用户也认");
        assert!(!matches_spec(&ep, "gpu2"));
        // 不做子串匹配：半截主机名会把不相干的连接一起断掉 / 删掉
        assert!(!matches_spec(&ep, "10.0.0"));
        assert!(!matches_spec(&ep, "gpu"));
        assert!(!matches_spec(&ep, ""), "空串不该命中任何端点");
    }

    /// 预算是原子累加的：并发申请不会超过上限
    #[test]
    fn turn_budget_is_shared_and_capped() {
        reset_turn_budget();
        let cap = limits().turn_max_bytes;
        let granted = take_turn_budget(cap * 2);
        assert_eq!(granted, cap, "单次申请不应超过剩余预算");
        assert_eq!(take_turn_budget(1), 0, "预算用尽后不再发放");
        reset_turn_budget();
        assert!(take_turn_budget(16) > 0, "归零后重新可用");
    }
}

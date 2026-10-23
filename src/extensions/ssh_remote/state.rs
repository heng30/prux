//! SSH 端点的持久化状态。
//!
//! 两个文件，都在 `agent_dir()` 下：
//! - `extensions/ssh-remote.json`：默认端点、已知端点、隧道列表与输出预算；
//! - `ssh-remote-known-hosts.json`：`host:port` → 已确认的主机公钥（OpenSSH 单行形式）。

use crate::core::settings_manager::agent_dir;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// 配置子目录（与其它扩展一致）
const CONFIG_SUBDIR: &str = "extensions";
/// 主配置文件名
const CONFIG_FILE: &str = "ssh-remote.json";
/// 已知主机文件名
const KNOWN_HOSTS_FILE: &str = "ssh-remote-known-hosts.json";
/// 远端默认目录（未显式设置时）
pub const FALLBACK_REMOTE_CWD: &str = "~";
/// 远程命令默认超时秒数
pub const DEFAULT_TIMEOUT_SECONDS: u32 = 30;
/// 远端命令超时允许的最大秒数
pub const MAX_TIMEOUT_SECONDS: u32 = 2_147_483;

/// 一个 SSH 端点（`user@host:port` 唯一标识）的持久化描述。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Endpoint {
    /// 用户输入的原始 ssh 命令（`ssh root@host -p 22` 的完整形式）
    #[serde(default)]
    pub command: String,
    /// 解析后的主机名（已应用 `~/.ssh/config` 的 HostName）
    #[serde(default)]
    pub host: String,
    /// 登录用户名
    #[serde(default)]
    pub user: String,
    /// 端口，22 为默认
    #[serde(default = "default_port")]
    pub port: u16,
    /// 显式私钥路径（`-i`）；`Some` 时只用它，不再回退默认密钥
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity: Option<String>,
    /// 密码文件路径（`-o PasswordFile=<path>`，需 0600）；`Some` 时公钥失败后允许密码认证
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password_file: Option<String>,
    /// 跳板机（`-J user@jump:22` 的原文）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jump: Option<String>,
    /// 端点备注（显示在状态里，如 `H100 training`）
    #[serde(default)]
    pub note: String,
    /// 该端点的默认远端目录（`remote_exec` 省略 `cwd` 时用它）
    #[serde(default = "fallback_cwd")]
    pub cwd: String,
}

/// 默认端口
fn default_port() -> u16 {
    22
}

/// 默认远端目录
fn fallback_cwd() -> String {
    FALLBACK_REMOTE_CWD.to_string()
}

impl Endpoint {
    /// `user@host:port` 形式的唯一键（连接池与「已知端点」都按它索引）
    pub fn key(&self) -> String {
        format!("{}@{}:{}", self.user, self.host, self.port)
    }

    /// 状态展示串：`note (user@host:port)`，无备注时只有括号部分
    pub fn display(&self) -> String {
        if self.note.trim().is_empty() {
            self.key()
        } else {
            format!("{} ({})", self.note.trim(), self.key())
        }
    }
}

/// 输出预算与超时（`ssh-remote.json` 的顶层字段，可手改）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Limits {
    /// 单次 `remote_read` 默认最大行数
    #[serde(default = "default_read_lines")]
    pub read_max_lines: usize,
    /// 单次 `remote_read` 默认最大字节数
    #[serde(default = "default_read_bytes")]
    pub read_max_bytes: usize,
    /// 单次 `remote_exec` 返回模型的最大行数
    #[serde(default = "default_exec_lines")]
    pub exec_max_lines: usize,
    /// 单次 `remote_exec` 返回模型的最大字节数
    #[serde(default = "default_exec_bytes")]
    pub exec_max_bytes: usize,
    /// 每轮所有远程工具输出之和的上限
    #[serde(default = "default_turn_bytes")]
    pub turn_max_bytes: usize,
    /// 远程命令超时（秒）
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u32,
}

/// read 默认行数
fn default_read_lines() -> usize {
    400
}
/// read 默认字节数
fn default_read_bytes() -> usize {
    16 * 1024
}
/// exec 默认行数
fn default_exec_lines() -> usize {
    200
}
/// exec 默认字节数
fn default_exec_bytes() -> usize {
    8 * 1024
}
/// 每轮输出上限
fn default_turn_bytes() -> usize {
    32 * 1024
}
/// 命令超时
fn default_timeout() -> u32 {
    DEFAULT_TIMEOUT_SECONDS
}

impl Default for Limits {
    /// 全部取默认值
    fn default() -> Self {
        Self {
            read_max_lines: default_read_lines(),
            read_max_bytes: default_read_bytes(),
            exec_max_lines: default_exec_lines(),
            exec_max_bytes: default_exec_bytes(),
            turn_max_bytes: default_turn_bytes(),
            timeout_seconds: default_timeout(),
        }
    }
}

impl Limits {
    /// 从 JSON 值读取，非法值回退默认（配置是用户手写的，单字段写错不该让整份配置失效）
    fn from_value(v: &Value) -> Self {
        let d = Self::default();
        let get = |key: &str, fallback: usize, max: usize| -> usize {
            v.get(key)
                .and_then(|x| x.as_u64())
                .map(|n| (n as usize).clamp(1, max))
                .unwrap_or(fallback)
        };
        Self {
            read_max_lines: get("readMaxLines", d.read_max_lines, 100_000),
            read_max_bytes: get("readMaxBytes", d.read_max_bytes, 64 * 1024 * 1024),
            exec_max_lines: get("execMaxLines", d.exec_max_lines, 100_000),
            exec_max_bytes: get("execMaxBytes", d.exec_max_bytes, 64 * 1024 * 1024),
            turn_max_bytes: get("turnMaxBytes", d.turn_max_bytes, 512 * 1024 * 1024),
            timeout_seconds: v
                .get("timeoutSeconds")
                .and_then(|x| x.as_u64())
                .map(|n| n.clamp(1, MAX_TIMEOUT_SECONDS as u64) as u32)
                .unwrap_or(d.timeout_seconds),
        }
    }
}

/// 一条本地端口转发：把 `local_port` 映射到某端点上可见的 `remote_host:remote_port`
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TunnelSpec {
    /// 走哪个端点（`Endpoint::key()`）；空串表示用默认端点
    #[serde(default)]
    pub host: String,
    /// 本地监听端口
    pub local_port: u16,
    /// 从远端视角看的目标主机（`localhost` = 远端本机）
    pub remote_host: String,
    /// 从远端视角看的目标端口
    pub remote_port: u16,
}

impl TunnelSpec {
    /// 解析 `[host:]本地端口:远端主机:远端端口`
    pub fn parse(text: &str) -> Option<Self> {
        let parts: Vec<&str> = text.split(':').collect();
        let (host, rest) = match parts.len() {
            3 => (String::new(), &parts[..]),
            4 => (parts[0].to_string(), &parts[1..]),
            _ => return None,
        };
        let local_port: u16 = rest[0].parse().ok()?;
        let remote_host = rest[1].to_string();
        let remote_port: u16 = rest[2].parse().ok()?;
        if remote_host.is_empty() {
            return None;
        }
        Some(Self {
            host,
            local_port,
            remote_host,
            remote_port,
        })
    }

    /// 反解成配置里保存的文本
    pub fn to_spec_text(&self) -> String {
        let core = format!(
            "{}:{}:{}",
            self.local_port, self.remote_host, self.remote_port
        );
        if self.host.is_empty() {
            core
        } else {
            format!("{}:{core}", self.host)
        }
    }
}

/// `ssh-remote.json` 的完整内容。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteConfig {
    /// 默认端点（`/ssh <target>` 设置；工具省略 `host` 时用它）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Endpoint>,
    /// 已知端点（连过一次就记住，省掉 `~/.ssh/config` 查询与参数重述）
    #[serde(default)]
    pub known: Vec<Endpoint>,
    /// 端口转发（`/tunnel add` 写入，扩展启用时自动拉起）
    #[serde(default)]
    pub tunnels: Vec<TunnelSpec>,
    /// 输出预算
    #[serde(flatten, default)]
    pub limits: Limits,
}

impl RemoteConfig {
    /// 读配置：缺失或损坏时返回默认值（不覆盖用户文件）
    pub fn load() -> Self {
        let path = config_path();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Self::default();
        };
        let Ok(value) = serde_json::from_str::<Value>(&text) else {
            return Self::default();
        };
        let endpoint = |raw: &Value| -> Option<Endpoint> {
            let mut ep = serde_json::from_value::<Endpoint>(raw.clone()).ok()?;
            if ep.port == 0 {
                ep.port = 22;
            }
            if ep.cwd.trim().is_empty() {
                ep.cwd = FALLBACK_REMOTE_CWD.to_string();
            }
            Some(ep)
        };
        let default = value.get("default").and_then(endpoint);
        let mut known = Vec::new();
        if let Some(items) = value.get("known").and_then(|v| v.as_array()) {
            for raw in items {
                if let Some(ep) = endpoint(raw)
                    && !ep.host.is_empty()
                {
                    known.push(ep);
                }
            }
        }
        let mut tunnels = Vec::new();
        if let Some(items) = value.get("tunnels").and_then(|v| v.as_array()) {
            for raw in items {
                if let Ok(spec) = serde_json::from_value::<TunnelSpec>(raw.clone()) {
                    tunnels.push(spec);
                }
            }
        }
        Self {
            default,
            known,
            tunnels,
            limits: Limits::from_value(&value),
        }
    }

    /// 写配置：目录不存在时创建；写入失败返回错误文本
    pub fn save(&self) -> Result<(), String> {
        let path = config_path();
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
        }
        let text =
            serde_json::to_string_pretty(self).map_err(|e| format!("serialize config: {e}"))?;
        std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))
    }

    /// 记住一个端点（按 key 去重覆盖）；返回是否有变化
    pub fn remember(&mut self, ep: &Endpoint) -> bool {
        let key = ep.key();
        match self.known.iter_mut().find(|e| e.key() == key) {
            Some(existing) => {
                if existing == ep {
                    return false;
                }
                *existing = ep.clone();
                true
            }
            None => {
                self.known.push(ep.clone());
                true
            }
        }
    }

    /// 设为默认端点
    pub fn set_default(&mut self, ep: Endpoint) {
        self.remember(&ep);
        self.default = Some(ep);
    }

    /// 默认端点的 key（无默认时为 None）
    pub fn default_key(&self) -> Option<String> {
        self.default.as_ref().map(|e| e.key())
    }
}

/// 测试用的状态根目录覆盖（`None` = 用 `agent_dir()`）
///
/// 不用 env / 线程本地：连接流程里的路径读取发生在 tokio 的任意 worker 线程上，
/// 线程本地 override 会漏，而进程级 env 会污染同一进程里其它并行测试。
#[cfg(any(test, feature = "test-support"))]
static TEST_ROOT: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// 串行化用到 [`TestRootGuard`] 的测试。
///
/// 根目录是进程级单例：两个测试各自 `temp()` 会互相覆盖，先 drop 的那个还会把
/// 后一个的目录还原掉。同步测试用 `blocking_lock()`，async 测试（e2e）用 `lock().await`。
#[cfg(any(test, feature = "test-support"))]
#[allow(dead_code)]
pub static TEST_ROOT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 状态根目录（配置与已知主机都在它下面）
fn root_dir() -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(dir) = TEST_ROOT.lock().unwrap().clone() {
        return dir;
    }
    agent_dir()
}

/// 测试期间把状态根目录指向一个临时目录，返回的 guard drop 时还原。
///
/// 只在 `cfg(test)` 下被 crate 内测试使用；`--all-features` 构建里编译进来但无调用方。
#[cfg(any(test, feature = "test-support"))]
#[allow(dead_code)]
pub struct TestRootGuard {
    /// 替换前的值，drop 时还原
    prev: Option<PathBuf>,
    /// 持有的临时目录（`None` 表示调用方自己保证目录存活）
    _dir: Option<tempfile::TempDir>,
}

#[cfg(any(test, feature = "test-support"))]
#[allow(dead_code)]
impl TestRootGuard {
    /// 指向一个全新的临时目录
    pub fn temp() -> Self {
        let dir = tempfile::tempdir().expect("tempdir for ssh-remote state");
        let prev = TEST_ROOT.lock().unwrap().replace(dir.path().to_path_buf());
        Self {
            prev,
            _dir: Some(dir),
        }
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestRootGuard {
    /// 还原状态根目录
    fn drop(&mut self) {
        *TEST_ROOT.lock().unwrap() = self.prev.take();
    }
}

/// 主配置文件路径
pub fn config_path() -> PathBuf {
    root_dir().join(CONFIG_SUBDIR).join(CONFIG_FILE)
}

/// 已知主机文件路径
pub fn known_hosts_path() -> PathBuf {
    root_dir().join(KNOWN_HOSTS_FILE)
}

/// 载入已确认的主机公钥：`host:port` → OpenSSH 单行形式
pub fn load_known_hosts() -> BTreeMap<String, String> {
    let path = known_hosts_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return BTreeMap::new();
    };
    serde_json::from_str::<BTreeMap<String, String>>(&text).unwrap_or_default()
}

/// 记录一台主机的公钥（覆盖旧值）
pub fn save_known_host(id: &str, openssh_key: &str) -> Result<(), String> {
    let mut hosts = load_known_hosts();
    hosts.insert(id.to_string(), openssh_key.to_string());
    let path = known_hosts_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    }
    let text =
        serde_json::to_string_pretty(&hosts).map_err(|e| format!("serialize known hosts: {e}"))?;
    std::fs::write(&path, text).map_err(|e| format!("write {}: {e}", path.display()))
}

/// 删除一台主机的公钥记录（主机密钥变化后重新走 TOFU）
pub fn forget_known_host(id: &str) -> bool {
    let mut hosts = load_known_hosts();
    if hosts.remove(id).is_none() {
        return false;
    }
    let path = known_hosts_path();
    let Ok(text) = serde_json::to_string_pretty(&hosts) else {
        return false;
    };
    std::fs::write(&path, text).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_key_and_display() {
        let ep = Endpoint {
            user: "root".into(),
            host: "gpu".into(),
            port: 2202,
            note: "H100".into(),
            cwd: "/srv/x".into(),
            ..Default::default()
        };
        assert_eq!(ep.key(), "root@gpu:2202");
        assert_eq!(ep.display(), "H100 (root@gpu:2202)");
    }

    #[test]
    fn tunnel_spec_roundtrip() {
        let t = TunnelSpec::parse("8080:localhost:80").unwrap();
        assert_eq!(t.local_port, 8080);
        assert_eq!(t.remote_host, "localhost");
        assert_eq!(t.remote_port, 80);
        assert!(t.host.is_empty());
        assert_eq!(t.to_spec_text(), "8080:localhost:80");

        let t = TunnelSpec::parse("root@gpu:5432:db:5432").unwrap();
        assert_eq!(t.host, "root@gpu");
        assert_eq!(t.remote_host, "db");
        assert_eq!(t.to_spec_text(), "root@gpu:5432:db:5432");

        assert!(TunnelSpec::parse("8080:localhost").is_none());
        assert!(TunnelSpec::parse("a:b:c:d:e").is_none());
    }

    #[test]
    fn limits_clamp_from_value() {
        let limits = Limits::from_value(
            &serde_json::json!({ "readMaxLines": 0, "timeoutSeconds": 999999999 }),
        );
        assert_eq!(limits.read_max_lines, 1);
        assert_eq!(limits.timeout_seconds, MAX_TIMEOUT_SECONDS);
    }

    #[test]
    fn remember_dedups_by_key() {
        let _l = TEST_ROOT_LOCK.blocking_lock();
        let _g = TestRootGuard::temp();
        let ep = Endpoint {
            host: "gpu".into(),
            user: "root".into(),
            port: 22,
            ..Default::default()
        };
        let mut cfg = RemoteConfig::default();
        assert!(cfg.remember(&ep));
        assert!(!cfg.remember(&ep), "同样的端点不应重复记录");
        let mut moved = ep.clone();
        moved.cwd = "/srv".into();
        assert!(cfg.remember(&moved));
        assert_eq!(cfg.known.len(), 1);
        assert_eq!(cfg.known[0].cwd, "/srv");
        cfg.set_default(moved);
        assert_eq!(cfg.default_key().as_deref(), Some("root@gpu:22"));
    }

    #[test]
    fn known_hosts_roundtrip_and_forget() {
        let _l = TEST_ROOT_LOCK.blocking_lock();
        let _g = TestRootGuard::temp();
        save_known_host("h:22", "ssh-ed25519 AAAA").unwrap();
        assert_eq!(
            load_known_hosts().get("h:22").map(String::as_str),
            Some("ssh-ed25519 AAAA")
        );
        assert!(forget_known_host("h:22"));
        assert!(load_known_hosts().is_empty());
        assert!(!forget_known_host("h:22"), "重复删除应返回 false");
    }
}

//! 出站代理：环境变量 → reqwest 的逐请求决策（含 `NO_PROXY`），以及不依赖 reqwest
//! 的 SOCKS5 / HTTP `CONNECT` 隧道——只为打通一条 TCP 流，协议自带，不值得引一个 SOCKS 依赖。

use base64::Engine as _;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

/// 本模块参与代理决策的环境变量名（小写形式，[`env_var_ci`] 读取时大写名兜底）。
///
/// `http_proxy` / `https_proxy` / `all_proxy` 给代理地址与兜底，`no_proxy` 给直连排除表。
pub const ENV_VARS: [&str; 4] = ["http_proxy", "https_proxy", "all_proxy", "no_proxy"];

/// 解析后的 NO_PROXY 条目。
#[derive(Debug, Clone, PartialEq, Eq)]
struct NoProxyEntry {
    /// 小写、已去掉 `[]` 的 host（域名或 IP）
    host: String,
    /// 限定端口；None 表示不限制（含 pi 中解析为 0 的情形）
    port: Option<u16>,
}

/// NO_PROXY 配置：`all` 表示列表为 `*`（所有目标直连）。
#[derive(Debug, Clone, Default)]
pub struct NoProxyList {
    /// true 表示列表为 `*`，所有目标一律直连、不走代理。
    all: bool,
    /// 逐个解析出的匹配条目（`all` 为 true 时为空）。
    entries: Vec<NoProxyEntry>,
}

impl NoProxyList {
    /// 解析 NO_PROXY 字符串（逗号/空白分隔）。
    pub fn parse(list: &str) -> Self {
        let trimmed = list.trim();
        if trimmed.is_empty() {
            return Self::default();
        }

        if trimmed == "*" {
            return Self {
                all: true,
                entries: Vec::new(),
            };
        }

        Self {
            all: false,
            entries: trimmed
                .split([',', ' '])
                .filter_map(parse_no_proxy_entry)
                .collect(),
        }
    }

    /// hostname:port 是否命中 NO_PROXY（命中 → 该请求直连，不走代理）。
    ///
    /// 匹配规则：
    /// - host 相等，或目标为条目的子域（`example.com` 命中 `api.example.com`）
    /// - 前导 `.` / `*.` / `*` 等价于通配子域
    /// - 条目带端口时仅匹配同端口
    pub fn hits(&self, host: &str, port: u16) -> bool {
        if self.all {
            return true;
        }

        let target = strip_brackets(host).to_ascii_lowercase();
        self.entries.iter().any(|e| {
            if e.port.is_some_and(|p| p != port) {
                return false;
            }

            let mut domain = e.host.as_str();
            if let Some(rest) = domain.strip_prefix("*.") {
                domain = rest;
            } else if let Some(rest) = domain.strip_prefix('.') {
                domain = rest;
            } else if let Some(rest) = domain.strip_prefix('*') {
                domain = rest;
            }

            if domain.is_empty() {
                return false;
            }
            target == domain || target.ends_with(&format!(".{}", domain))
        })
    }
}

/// 去掉 IPv6 地址的方括号（如 `[::1]` → `::1`），无方括号时原样返回
fn strip_brackets(host: &str) -> &str {
    host.strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(host)
}

/// 解析单条 `no_proxy` 条目，支持 `host`、`host:port`、`[ipv6]`、`[ipv6]:port` 与裸 IPv6。
///
/// 空白条目返回 None；端口非数字时整段按无端口的 host 处理。
fn parse_no_proxy_entry(entry: &str) -> Option<NoProxyEntry> {
    let trimmed = entry.trim().to_ascii_lowercase();
    if trimmed.is_empty() {
        return None;
    }

    // [ipv6] 或 [ipv6]:port
    if trimmed.starts_with('[') {
        let closing = trimmed.find(']')?;
        let host = strip_brackets(&trimmed).to_string();
        let rest = &trimmed[closing + 1..];

        if let Some(port_str) = rest.strip_prefix(':') {
            return Some(NoProxyEntry {
                host,
                port: parse_port(port_str),
            });
        }

        return Some(NoProxyEntry { host, port: None });
    }

    // 裸 IPv6（多于一个冒号、无方括号）：整段作为 host
    if trimmed.split(':').count() > 2 {
        return Some(NoProxyEntry {
            host: trimmed,
            port: None,
        });
    }

    // host:port（恰好一个冒号）；端口非数字时整段作为 host
    if let Some((host, port_str)) = trimmed.split_once(':') {
        return Some(match parse_port(port_str) {
            Some(port) => NoProxyEntry {
                host: host.to_string(),
                port: Some(port),
            },
            None => NoProxyEntry {
                host: trimmed,
                port: None,
            },
        });
    }

    Some(NoProxyEntry {
        host: trimmed,
        port: None,
    })
}

/// 解析端口；0/非法 → None（端口 0 表示不限制）。
fn parse_port(s: &str) -> Option<u16> {
    s.parse::<u16>().ok().filter(|&p| p != 0)
}

/// 逐个报告 [`ENV_VARS`] 是否已配置（走 [`env_var_ci`]，大小写不敏感，空值视为未配置）。
///
/// 只取有无、**不回显值**——值里可能带凭据；供诊断面板展示代理配置用。
pub fn env_flags() -> Vec<(&'static str, bool)> {
    ENV_VARS
        .iter()
        .map(|name| (*name, env_var_ci(name).is_some()))
        .collect()
}

/// 读取环境变量：小写名优先、大写名兜底，trim 后非空才返回。
/// 对应 pi `getProxyEnv` 无 provider 作用域时的读取次序。
pub fn env_var_ci(key: &str) -> Option<String> {
    let lower = key.to_ascii_lowercase();
    let upper = key.to_ascii_uppercase();
    for name in [lower, upper] {
        if let Ok(v) = std::env::var(name) {
            let t = v.trim();
            if !t.is_empty() {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// 规范化代理串：无 scheme 时按目标协议补齐；仅接受 reqwest 支持的代理协议。
fn qualify_proxy_url(proxy: &str, scheme: &str) -> Option<String> {
    let candidate = if proxy.contains("://") {
        proxy.to_string()
    } else {
        format!("{}://{}", scheme, proxy)
    };

    match reqwest::Url::parse(&candidate) {
        Ok(url)
            if matches!(
                url.scheme(),
                "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
            ) =>
        {
            Some(candidate)
        }
        _ => None,
    }
}

/// 代理环境变量 → reqwest Proxy（按目标 URL 逐请求决策）。
///
/// - 按目标协议优先选 `<scheme>_proxy`，回退 `all_proxy`（大小写均可，小写优先）
/// - 命中 `NO_PROXY` / `no_proxy` 的目标（`*`、域、子域、IPv6、host:port）直连
///
/// 未配置任何代理变量时返回 `None`（调用方不设置代理）。
pub fn proxy_from_env() -> Option<reqwest::Proxy> {
    let no_proxy = NoProxyList::parse(&env_var_ci("no_proxy").unwrap_or_default());
    let http = env_var_ci("http_proxy")
        .or_else(|| env_var_ci("all_proxy"))
        .and_then(|v| qualify_proxy_url(&v, "http"));
    let https = env_var_ci("https_proxy")
        .or_else(|| env_var_ci("all_proxy"))
        .and_then(|v| qualify_proxy_url(&v, "https"));
    if http.is_none() && https.is_none() {
        return None;
    }

    Some(reqwest::Proxy::custom(
        move |url: &reqwest::Url| -> Option<String> {
            // 仅 http(s) 目标参与代理决策
            let scheme = match url.scheme() {
                "http" => "http",
                "https" => "https",
                _ => return None,
            };

            // 无 host / 无法推断端口 → 直连
            let host = url.host_str()?;
            let port = url.port_or_known_default()?;
            if no_proxy.hits(host, port) {
                return None;
            }

            match scheme {
                "https" => https.clone(),
                _ => http.clone(),
            }
        },
    ))
}

/// 代理种类
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKind {
    /// SOCKS5（远端解析域名）
    Socks5,
    /// HTTP CONNECT
    Http,
}

/// 一个已解析的代理
#[derive(Debug, Clone)]
pub struct Proxy {
    /// 代理协议
    pub kind: ProxyKind,
    /// 代理主机
    pub host: String,
    /// 代理端口
    pub port: u16,
    /// `(用户名, 密码)`，None = 不认证
    pub auth: Option<(String, String)>,
}

/// 从配置里的代理 URL 解析代理。
///
/// `None` 或空串表示直连（返回 `Ok(None)`）；非空但格式不认识时返回错误文本，
/// 而不静默直连——用户写了代理却被绕过，比连不上更难查。
pub fn from_url(url: Option<&str>) -> Result<Option<Proxy>, String> {
    let Some(url) = url.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    parse(url)
        .map(Some)
        .ok_or_else(|| format!("invalid proxy url: {url}"))
}

/// 解析 `scheme://[user:pass@]host:port`。
///
/// 认 `socks5` / `socks5h` / `socks` 与 `http` / `https` 两类协议，缺 scheme 时按 `socks5` 处理；
/// 协议不认识、主机为空或端口非数字时返回 `None`。
pub fn parse(url: &str) -> Option<Proxy> {
    let (scheme, rest) = url.split_once("://").unwrap_or(("socks5", url));
    let kind = match scheme.to_ascii_lowercase().as_str() {
        "socks5" | "socks5h" | "socks" => ProxyKind::Socks5,
        "http" | "https" => ProxyKind::Http,
        _ => return None,
    };
    let (auth, hostport) = match rest.rsplit_once('@') {
        Some((userinfo, hp)) => {
            let (u, p) = userinfo.split_once(':').unwrap_or((userinfo, ""));
            (Some((u.to_string(), p.to_string())), hp)
        }
        None => (None, rest),
    };
    let hostport = hostport.trim_end_matches('/');
    let (host, port) = hostport.rsplit_once(':')?;
    if host.is_empty() {
        return None;
    }

    Some(Proxy {
        kind,
        host: strip_brackets(host).to_string(),
        port: port.parse().ok()?,
        auth,
    })
}

impl Proxy {
    /// 展示串（状态提示用，不含凭据）
    pub fn describe(&self) -> String {
        let scheme = match self.kind {
            ProxyKind::Socks5 => "socks5",
            ProxyKind::Http => "http",
        };
        format!("{scheme}://{}:{}", self.host, self.port)
    }

    /// 打通到 `target_host:target_port` 的 TCP 隧道
    pub async fn connect(&self, target_host: &str, target_port: u16) -> std::io::Result<TcpStream> {
        let mut stream = TcpStream::connect((self.host.as_str(), self.port)).await?;
        match self.kind {
            ProxyKind::Socks5 => {
                socks5_handshake(&mut stream, self, target_host, target_port).await?
            }
            ProxyKind::Http => http_connect(&mut stream, self, target_host, target_port).await?,
        }
        Ok(stream)
    }
}

/// 无效输入错误
fn invalid(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_string())
}

/// SOCKS5 握手（RFC 1928 + RFC 1929 用户名/密码认证）
async fn socks5_handshake(
    stream: &mut TcpStream,
    proxy: &Proxy,
    target_host: &str,
    target_port: u16,
) -> std::io::Result<()> {
    // 问候：无认证 / 用户名密码
    let methods: &[u8] = if proxy.auth.is_some() {
        &[0x00, 0x02]
    } else {
        &[0x00]
    };
    let mut greeting = vec![0x05u8, methods.len() as u8];
    greeting.extend_from_slice(methods);
    stream.write_all(&greeting).await?;

    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await?;
    if reply[0] != 0x05 {
        return Err(invalid("proxy is not SOCKS5"));
    }

    match reply[1] {
        0x00 => {}
        0x02 => {
            let Some((user, pass)) = &proxy.auth else {
                return Err(invalid("proxy requires authentication"));
            };

            let mut auth = vec![0x01u8, user.len().min(255) as u8];
            auth.extend_from_slice(&user.as_bytes()[..user.len().min(255)]);
            auth.push(pass.len().min(255) as u8);
            auth.extend_from_slice(&pass.as_bytes()[..pass.len().min(255)]);
            stream.write_all(&auth).await?;

            let mut status = [0u8; 2];
            stream.read_exact(&mut status).await?;
            if status[1] != 0x00 {
                return Err(invalid("proxy rejected credentials"));
            }
        }
        0xff => return Err(invalid("proxy accepted no auth method")),
        other => return Err(invalid(&format!("proxy chose unsupported auth {other}"))),
    }

    // 连接请求：域名交给代理解析（socks5h 语义）
    let host_bytes = target_host.as_bytes();
    if host_bytes.len() > 255 {
        return Err(invalid("target hostname too long"));
    }

    let mut request = vec![0x05u8, 0x01, 0x00, 0x03, host_bytes.len() as u8];
    request.extend_from_slice(host_bytes);
    request.extend_from_slice(&target_port.to_be_bytes());
    stream.write_all(&request).await?;

    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != 0x05 {
        return Err(invalid("bad SOCKS5 reply version"));
    }

    if head[1] != 0x00 {
        return Err(invalid(&format!(
            "SOCKS5 connect failed (code {})",
            head[1]
        )));
    }

    // 吃掉绑定地址
    match head[3] {
        0x01 => {
            let mut skip = [0u8; 4 + 2];
            stream.read_exact(&mut skip).await?;
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut skip = vec![0u8; len[0] as usize + 2];
            stream.read_exact(&mut skip).await?;
        }
        0x04 => {
            let mut skip = [0u8; 16 + 2];
            stream.read_exact(&mut skip).await?;
        }
        _ => return Err(invalid("bad SOCKS5 bind address type")),
    }
    Ok(())
}

/// HTTP `CONNECT` 隧道
async fn http_connect(
    stream: &mut TcpStream,
    proxy: &Proxy,
    target_host: &str,
    target_port: u16,
) -> std::io::Result<()> {
    let mut request = format!(
        "CONNECT {target_host}:{target_port} HTTP/1.1\r\nHost: {target_host}:{target_port}\r\n"
    );

    if let Some((user, pass)) = &proxy.auth {
        let token =
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}").as_bytes());
        request.push_str(&format!("Proxy-Authorization: Basic {token}\r\n"));
    }
    request.push_str("Proxy-Connection: keep-alive\r\n\r\n");
    stream.write_all(request.as_bytes()).await?;

    // 读到空行为止，上限 8 KiB：避免恶意代理无限发头
    let mut buf = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte).await? == 0 {
            return Err(invalid("proxy closed during CONNECT"));
        }
        buf.push(byte[0]);
        if buf.ends_with(b"\r\n\r\n") {
            break;
        }
        if buf.len() > 8192 {
            return Err(invalid("proxy CONNECT response too large"));
        }
    }

    let head = String::from_utf8_lossy(&buf);
    let status = head.lines().next().unwrap_or("");
    let ok = status.split_whitespace().nth(1) == Some("200");
    if !ok {
        return Err(invalid(&format!(
            "proxy CONNECT rejected: {}",
            status.trim()
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_proxy_urls() {
        let p = parse("socks5://127.0.0.1:1080").unwrap();
        assert_eq!(p.kind, ProxyKind::Socks5);
        assert_eq!(p.port, 1080);
        assert!(p.auth.is_none());

        let p = parse("http://u:p@proxy:3128").unwrap();
        assert_eq!(p.kind, ProxyKind::Http);
        assert_eq!(p.auth.as_ref().unwrap().0, "u");

        assert!(parse("ftp://x:1").is_none());
        assert!(parse("socks5://hostonly").is_none());
    }

    #[test]
    fn from_url_treats_empty_as_direct() {
        assert!(from_url(None).unwrap().is_none());
        assert!(from_url(Some("  ")).unwrap().is_none());
        assert_eq!(
            from_url(Some("http://u:p@proxy:3128"))
                .unwrap()
                .unwrap()
                .port,
            3128
        );
        assert!(
            from_url(Some("ftp://x:1")).is_err(),
            "写错的代理不应静默直连"
        );
    }

    #[test]
    fn no_proxy_respects_exclusions() {
        let list = NoProxyList::parse("bedrock-runtime.us-east-1.amazonaws.com");
        assert!(list.hits("bedrock-runtime.us-east-1.amazonaws.com", 443));
        assert!(!list.hits("other.example.com", 443));
    }

    #[test]
    fn no_proxy_wildcards_ipv6_ports() {
        // 覆盖 pi 测试用例：域、.域、*.域、IPv6（裸/带括号）、host:port
        let list = NoProxyList::parse(
            "example.com, .wildcard.org, *.star.net, ::1, [2001:db8::1], 127.0.0.1:8080",
        );
        assert!(list.hits("example.com", 443));
        assert!(list.hits("api.example.com", 443));
        assert!(!list.hits("notexample.com", 443));
        assert!(list.hits("wildcard.org", 443));
        assert!(list.hits("api.wildcard.org", 443));
        assert!(list.hits("star.net", 443));
        assert!(list.hits("api.star.net", 443));
        assert!(list.hits("::1", 80));
        assert!(list.hits("2001:db8::1", 443));
        assert!(list.hits("127.0.0.1", 8080));
        assert!(!list.hits("127.0.0.1", 3000));
    }

    #[test]
    fn no_proxy_entry_port_limits() {
        let list = NoProxyList::parse("127.0.0.1:8080");
        assert!(list.hits("127.0.0.1", 8080));
        assert!(!list.hits("127.0.0.1", 80));
        assert!(!list.hits("127.0.0.1", 3000));
        let bare = NoProxyList::parse("example.com");
        assert!(bare.hits("example.com", 80));
        assert!(bare.hits("example.com", 443));
    }

    #[test]
    fn no_proxy_case_insensitive() {
        assert!(NoProxyList::parse("EXAMPLE.COM").hits("api.example.com", 443));
        assert!(NoProxyList::parse("Example.Com").hits("API.EXAMPLE.COM", 443));
    }

    #[test]
    fn no_proxy_star_and_empty() {
        assert!(NoProxyList::parse("*").hits("anything.example.com", 443));
        assert!(!NoProxyList::parse("").hits("x.com", 443));
        assert!(!NoProxyList::default().hits("x.com", 443));
        // 空格分隔
        assert!(NoProxyList::parse("a.com b.com").hits("b.com", 443));
    }

    /// 诊断面板拿到的名单必须就是本模块真正读的那些变量（含 all_proxy / no_proxy），
    /// 少一个就会把「配了却没用上」和「没配」混为一谈。
    #[test]
    fn env_flags_cover_every_decision_var() {
        let flags = env_flags();
        assert_eq!(flags.len(), ENV_VARS.len());
        let names: Vec<&str> = flags.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, ENV_VARS.to_vec());
    }
}

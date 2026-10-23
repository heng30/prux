//! 出站代理：先连代理，再由代理打通到目标主机，最后把这条透明 TCP 流交给 SSH 握手。
//!
//! 支持 SOCKS5（含用户名/密码认证）与 HTTP `CONNECT`，代理地址取 `ALL_PROXY`
//! 环境变量（`socks5://` / `socks5h://` / `http://` / `https://`）。
//! 协议实现是自带的：只为打通一条 TCP 隧道，不值得为此引入一个 SOCKS 客户端依赖。

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

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

/// 从环境变量解析代理；未设置或格式不认识时返回 None（直连）
pub fn from_env() -> Option<Proxy> {
    for var in ["ALL_PROXY", "all_proxy"] {
        if let Ok(value) = std::env::var(var) {
            let value = value.trim();
            if !value.is_empty() {
                return parse(value);
            }
        }
    }
    None
}

/// 解析 `scheme://[user:pass@]host:port`
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
        host: host.to_string(),
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
        use base64::Engine as _;
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
}

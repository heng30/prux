//! 网络地址工具：监听地址的校验与规范化、OpenAI 兼容 base_url 推导。

/// 校验并规范化监听地址 `host:port`。
///
/// 接受 `127.0.0.1:8080` / `0.0.0.0:8080` / `localhost:8080` / `[::1]:8080`；
/// 拒绝空串、裸端口、空 host 与端口 0（端口 0 由内核分配实际端口，与配置不符会让
/// dock 展示的地址说谎）。
pub fn normalize_listen(raw: &str) -> Result<String, String> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("listen address is empty (expected host:port)".to_string());
    }

    if let Ok(addr) = s.parse::<std::net::SocketAddr>() {
        if addr.port() == 0 {
            return Err(port_zero_error());
        }
        return Ok(addr.to_string());
    }

    let Some((host, port)) = s.rsplit_once(':') else {
        return Err(format!(
            "invalid listen address \"{s}\": expected host:port"
        ));
    };
    let host = host.trim();
    if host.is_empty() {
        return Err(format!("invalid listen address \"{s}\": host is empty"));
    }
    let port: u16 = port
        .trim()
        .parse()
        .map_err(|_| format!("invalid listen address \"{s}\": port must be 1-65535"))?;
    if port == 0 {
        return Err(port_zero_error());
    }
    Ok(format!("{host}:{port}"))
}

/// 拒绝端口 0 时的报错文案：内核会自动换端口，与界面展示的端口不一致。
fn port_zero_error() -> String {
    "port 0 is not allowed (the kernel would pick a different port than the one shown)".to_string()
}

/// 把监听地址推导成客户端应配置的 OpenAI 兼容 base_url：`http://{host}:{port}/v1`。
///
/// 通配地址（`0.0.0.0` / `[::]`）无法直接当 URL 主机用（连过去是未定义行为），
/// 展示时换成回环地址；否则用户照着提示配置会拿到连不上的 base_url。
pub fn base_url(listen: &str) -> String {
    let (host, port) = match listen.rsplit_once(':') {
        Some((host, port)) => (host, port),
        None => (listen, ""),
    };
    let host = match host {
        "0.0.0.0" | "*" => "127.0.0.1",
        "[::]" | "::" => "[::1]",
        h => h,
    };
    format!("http://{host}:{port}/v1")
}

/// loopback 主机判定：`localhost` / `127.0.0.1` / `::1`，大小写不敏感。
///
/// 兼容 URL 主机的写法：去掉 IPv6 的方括号（`Url::host_str` 带方括号）、
/// 接受未压缩的 `0:0:0:0:0:0:0:1`。
pub fn is_loopback_host(host: &str) -> bool {
    let trimmed = host
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_ascii_lowercase();
    matches!(
        trimmed.as_str(),
        "localhost" | "127.0.0.1" | "::1" | "0:0:0:0:0:0:0:1"
    )
}

/// URL 是否指向回环主机（用于无鉴权的本机端点，绝不允许连到别的机器）。
///
/// 解析失败、没有主机名的 URL 都返回 `false`。
pub fn url_is_loopback(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .is_some_and(|host| is_loopback_host(&host))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 回环判定：接受三种写法与方括号 / 未压缩 IPv6，大小写不敏感，拒绝其它主机。
    #[test]
    fn loopback_host_detection() {
        for host in [
            "localhost",
            "LOCALHOST",
            "127.0.0.1",
            "::1",
            "[::1]",
            "0:0:0:0:0:0:0:1",
        ] {
            assert!(is_loopback_host(host), "{host} 应判为回环");
        }
        for host in ["", "example.com", "10.0.0.1", "0.0.0.0", "127.0.0.2"] {
            assert!(!is_loopback_host(host), "{host} 不应判为回环");
        }
    }

    /// 只有回环的 URL 才被接受，无法解析的字符串一律拒绝。
    #[test]
    fn url_loopback_detection() {
        assert!(url_is_loopback("ws://127.0.0.1:9222/devtools/browser/x"));
        assert!(url_is_loopback("ws://[::1]:9222/devtools/browser/x"));
        assert!(!url_is_loopback("ws://example.com:9222/devtools/browser/x"));
        assert!(!url_is_loopback("ws://10.0.0.5:9222/x"));
        assert!(!url_is_loopback("not a url"));
    }

    #[test]
    fn normalize_listen_accepts_host_port_forms() {
        assert_eq!(
            normalize_listen("127.0.0.1:8765").unwrap(),
            "127.0.0.1:8765"
        );
        assert_eq!(normalize_listen(" 0.0.0.0:80 ").unwrap(), "0.0.0.0:80");
        assert_eq!(
            normalize_listen("localhost:8765").unwrap(),
            "localhost:8765"
        );
        assert_eq!(normalize_listen("[::1]:8765").unwrap(), "[::1]:8765");
    }

    #[test]
    fn base_url_derives_openai_compatible_endpoint() {
        assert_eq!(base_url("127.0.0.1:8765"), "http://127.0.0.1:8765/v1");
        assert_eq!(base_url("localhost:8765"), "http://localhost:8765/v1");
        assert_eq!(base_url("[::1]:8765"), "http://[::1]:8765/v1");
        // 通配地址不能照抄：客户端要连得上的主机
        assert_eq!(base_url("0.0.0.0:8765"), "http://127.0.0.1:8765/v1");
        assert_eq!(base_url("[::]:8765"), "http://[::1]:8765/v1");
    }

    #[test]
    fn normalize_listen_rejects_bad_forms() {
        for bad in [
            "",
            "8765",
            "127.0.0.1",
            ":8765",
            "127.0.0.1:",
            "127.0.0.1:0",
            "127.0.0.1:99999",
        ] {
            assert!(normalize_listen(bad).is_err(), "{bad} 应被拒绝");
        }
    }
}

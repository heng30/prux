//! 网络地址与端口工具：监听地址的校验与规范化、`host[:port]` 拆分、OpenAI 兼容
//! base_url 推导、回环端口的挑选与探活，以及给单个网络阶段套超时。

use std::time::Duration;

/// 探活时连一下回环地址的超时（端口没人监听会立刻被拒，这个上限只是防某些防火墙丢包）。
const PROBE_TIMEOUT: Duration = Duration::from_millis(300);

/// 单个网络阶段的超时：一条半开的 TCP 不该把整次连接（握手 / 建隧道 / 认证）拖死。
const STAGE_TIMEOUT: Duration = Duration::from_secs(20);

/// 拆分 `host[:port]`，缺省端口用 `default_port`。
///
/// 支持 IPv6 字面量 `[::1]:22`（裸 `::1` 无法与端口区分，按「无端口的 IPv6」处理）；
/// 冒号后不是纯数字时整串当主机名，不报错，因此 `dash-name` 这类写法原样返回。
pub fn split_host_port(value: &str, default_port: u16) -> (String, u16) {
    // IPv6 字面量 `[::1]:22`
    if let Some(rest) = value.strip_prefix('[')
        && let Some((host, tail)) = rest.split_once(']')
    {
        let port = tail
            .strip_prefix(':')
            .and_then(|p| p.parse().ok())
            .unwrap_or(default_port);
        return (host.to_string(), port);
    }

    match value.rsplit_once(':') {
        Some((host, port)) if port.chars().all(|c| c.is_ascii_digit()) && !host.is_empty() => {
            (host.to_string(), port.parse().unwrap_or(default_port))
        }
        _ => (value.to_string(), default_port),
    }
}

/// 给一个网络阶段套独立超时，失败或超时时把阶段名写进错误。
///
/// `label` 是人类可读的阶段描述（如 `TCP connect to gpu:22`），作为错误前缀，
/// 于是调用方拿到的报错总能说清是哪一步卡住了。底层错误按 `Display` 呈现。
pub async fn stage_timeout<T, E>(
    label: &str,
    future: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, String>
where
    E: std::fmt::Display,
{
    match tokio::time::timeout(STAGE_TIMEOUT, future).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{label}: {e}")),
        Err(_) => Err(format!(
            "{label}: timed out after {} seconds",
            STAGE_TIMEOUT.as_secs()
        )),
    }
}

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

/// 挑一个当前空闲的回环端口（绑定 `127.0.0.1:0` 读出内核分配的端口后立刻释放）。
///
/// 释放到真正有人绑上之间有竞态窗口；被抢走时表现为连不上，而不是连到别的服务上。
/// 挑不出端口（回环不可用、fd 耗尽等）时返回 `Err`，文案供调用方加自己的域前缀。
pub fn pick_free_port() -> Result<u16, String> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .map_err(|e| format!("cannot find a free loopback port: {e}"))?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|e| format!("cannot read the port the kernel assigned: {e}"))
}

/// 探一下 `host:port` 上有没有人在监听（只做一次 TCP 连接，[`PROBE_TIMEOUT`] 内没连上就算没有）。
pub async fn is_port_listening(host: &str, port: u16) -> bool {
    matches!(
        tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect((host, port))).await,
        Ok(Ok(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

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
    fn splits_host_port() {
        assert_eq!(split_host_port("gpu:2202", 22), ("gpu".into(), 2202));
        assert_eq!(split_host_port("gpu", 22), ("gpu".into(), 22));
        assert_eq!(split_host_port("[::1]:2222", 22), ("::1".into(), 2222));
        assert_eq!(split_host_port("dash-name", 22), ("dash-name".into(), 22));
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

    /// 空闲端口真的空着（挑出来之后能立刻绑上）。
    #[test]
    fn pick_free_port_returns_a_bindable_port() {
        let port = pick_free_port().unwrap();
        assert!(port > 0);
        std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
    }

    /// 监听探测：绑着的端口为真，松开后为假。
    #[tokio::test]
    async fn is_port_listening_follows_the_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(is_port_listening("127.0.0.1", port).await);

        drop(listener);
        // 同时跑的用例可能正好又绑上刚释放的这个端口（`bind(:0)` 会复用），
        // 所以给一段时间等它空出来，而不是绑完立刻断言
        let deadline = Instant::now() + Duration::from_secs(10);
        while is_port_listening("127.0.0.1", port).await {
            assert!(
                Instant::now() < deadline,
                "端口 {port} 已松开却仍显示在监听"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

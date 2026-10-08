use super::proxy::proxy_from_env;
use reqwest::{Client, Proxy, header::HeaderMap, redirect::Policy};
use std::time::Duration;
use url::Url;

/// 流式响应读取时相邻两个分块之间允许的最大间隔。
///
/// 连接建立后对端长时间不吐任何字节（含迟迟不返回响应头）时，请求会永久挂起：
/// 既不成功也不失败，上层没有超时可用。用 reqwest 的读超时把它转成一次普通失败。
pub const STREAM_READ_TIMEOUT: Duration = Duration::from_secs(300);

/// 连接超时
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// `reqwest::Client` 构建参数（默认值贴近 web 抓取场景：30s 超时、跟随重定向、连接池 4）。
#[derive(Debug, Clone)]
pub struct ClientOptions<'a> {
    /// 请求总超时
    pub timeout: Duration,
    /// 连接超时；`None` 用 reqwest 默认
    pub connect_timeout: Option<Duration>,
    /// User-Agent；`None` 用 reqwest 默认
    pub user_agent: Option<&'a str>,
    /// 代理；`None` / 空串表示回退环境代理（`http_proxy` 等，见 [`proxy_from_env`]），环境未配置时才直连
    pub proxy: Option<&'a str>,
    /// 是否跟随重定向
    pub follow_redirects: bool,
    /// 每主机空闲连接上限
    pub pool_max_idle_per_host: usize,
}

impl Default for ClientOptions<'_> {
    /// 默认参数：30s 总超时、跟随重定向、每主机 4 个空闲连接，其余交给 reqwest 默认。
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(30),
            connect_timeout: None,
            user_agent: None,
            proxy: None,
            follow_redirects: true,
            pool_max_idle_per_host: 4,
        }
    }
}

/// 构建 reqwest 客户端。错误消息面向用户，由调用方转成自身错误类型。
///
/// 代理优先取 [`ClientOptions::proxy`]，未配置时回退环境代理（[`proxy_from_env`]）。
pub fn build_client_with_options(options: &ClientOptions<'_>) -> Result<Client, String> {
    let mut builder = Client::builder()
        .timeout(options.timeout)
        .pool_max_idle_per_host(options.pool_max_idle_per_host);
    if let Some(user_agent) = options.user_agent {
        builder = builder.user_agent(user_agent);
    }
    if let Some(connect_timeout) = options.connect_timeout {
        builder = builder.connect_timeout(connect_timeout);
    }
    if !options.follow_redirects {
        builder = builder.redirect(Policy::none());
    }
    if let Some(proxy) = options.proxy.map(str::trim).filter(|p| !p.is_empty()) {
        let parsed =
            Proxy::all(proxy).map_err(|err| format!("Invalid proxy \"{proxy}\": {err}"))?;
        builder = builder.proxy(parsed);
    } else if let Some(proxy) = proxy_from_env() {
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|err| format!("Failed to build HTTP client: {err}"))
}

/// 按默认读超时（[`STREAM_READ_TIMEOUT`]）构造客户端（provider 各协议复用）。
pub(crate) fn build_client() -> crate::error::Result<Client> {
    build_client_with_read_timeout(STREAM_READ_TIMEOUT)
}

/// 按指定读超时构造客户端（[`build_client`] 与测试共用）。
pub(crate) fn build_client_with_read_timeout(
    read_timeout: Duration,
) -> crate::error::Result<Client> {
    client_builder(Some(CONNECT_TIMEOUT))
        .read_timeout(read_timeout)
        .build()
        .map_err(crate::error::Error::from)
}

/// 构造带环境代理的 reqwest 客户端 builder；调用方自行设置超时后 `.build()`。
///
/// 代理取自 [`proxy_from_env`]（含 `NO_PROXY` 判定）；`connect_timeout` 为 `None`
/// 时跟随 reqwest 默认（不设连接超时）。
pub(crate) fn client_builder(connect_timeout: Option<Duration>) -> reqwest::ClientBuilder {
    let mut b = Client::builder();
    if let Some(timeout) = connect_timeout {
        b = b.connect_timeout(timeout);
    }
    if let Some(p) = proxy_from_env() {
        b = b.proxy(p);
    }
    b
}

/// 百分号编码：保留 RFC 3986 未保留字符 `A-Za-z0-9-_.~`，其余字节按 UTF-8 转成 `%XX`。
pub fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

/// URL 解码（仅处理 %XX 与 '+' → 空格）
pub fn url_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                if i + 2 >= bytes.len() {
                    return None;
                }
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
                let v = u8::from_str_radix(hex, 16).ok()?;
                out.push(v);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// 把查询参数追加到 URL（reqwest 未启用 query 便捷方法时的替代实现）。
///
/// 解析失败时原样返回 `base`。
pub fn with_query(base: &str, params: &[(&str, &str)]) -> String {
    match Url::parse(base) {
        Ok(mut url) => {
            {
                let mut pairs = url.query_pairs_mut();
                for (key, value) in params {
                    pairs.append_pair(key, value);
                }
            }
            url.to_string()
        }
        Err(_) => base.to_string(),
    }
}

/// HTTP 错误摘要：`HTTP {status}: {body 前 300 字符}`。
///
/// 消费 `response`（错误路径上 body 只用于诊断，不再做其它处理）。
pub async fn error_summary(response: reqwest::Response) -> String {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let trimmed: String = body.chars().take(300).collect();
    format!("HTTP {status}: {trimmed}")
}

/// 从 `retry-after-ms`（毫秒）或 `retry-after`（秒或 HTTP-date）头解析服务端要求的等待时长；
/// 两个头都缺失、或取值非法（非有限数、无法解析的日期、已过期）时返回 None，
/// 调用方改用指数退避——**不可**把非法值当作 0 立即重试。
pub fn retry_after_ms(headers: &HeaderMap) -> Option<u64> {
    if let Some(v) = headers.get("retry-after-ms").and_then(|v| v.to_str().ok())
        && let Ok(ms) = v.trim().parse::<f64>()
        && ms.is_finite()
    {
        return Some(ms.max(0.0) as u64);
    }

    if let Some(v) = headers.get("retry-after").and_then(|v| v.to_str().ok()) {
        return parse_retry_after(v);
    }
    None
}

/// 解析 `retry-after` 的值：数值秒或 HTTP-date（IMF-fixdate / RFC 850）。
/// 数值非法或日期不可解析/已过期时返回 None（回落指数退避）。
fn parse_retry_after(value: &str) -> Option<u64> {
    let v = value.trim();
    if let Ok(seconds) = v.parse::<f64>() {
        return seconds
            .is_finite()
            .then(|| (seconds.max(0.0) * 1000.0) as u64);
    }
    http_date_ms(v)
}

/// HTTP-date 距当前时刻的毫秒数；不可解析或在过去时返回 None。
fn http_date_ms(value: &str) -> Option<u64> {
    let at = chrono::DateTime::parse_from_rfc2822(value).ok()?;
    let ms = at.with_timezone(&chrono::Utc) - chrono::Utc::now();
    let ms = ms.num_milliseconds();
    (ms > 0).then_some(ms as u64)
}

/// 取值顺序：`configured` → 环境变量 `env_name` → `default`，最后去掉尾斜杠。
pub fn resolve_base_url(configured: Option<&str>, env_name: &str, default: &str) -> String {
    let value = configured
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            std::env::var(env_name)
                .ok()
                .filter(|s| !s.trim().is_empty())
        })
        .unwrap_or_else(|| default.to_string());
    value.trim_end_matches('/').to_string()
}

/// hostname 是否等于 `domain` 或为其子域。
pub fn host_matches(hostname: &str, domain: &str) -> bool {
    hostname == domain || hostname.ends_with(&format!(".{domain}"))
}

/// URL 是否通过 include / exclude 域名过滤（两者都为空则一律通过）。
pub fn passes_domain_filter(url: &str, include: &[String], exclude: &[String]) -> bool {
    if include.is_empty() && exclude.is_empty() {
        return true;
    }
    let hostname = match Url::parse(url) {
        Ok(parsed) => parsed.host_str().unwrap_or("").to_ascii_lowercase(),
        Err(_) => return false,
    };
    if !include.is_empty() && !include.iter().any(|d| host_matches(&hostname, d)) {
        return false;
    }
    !exclude.iter().any(|d| host_matches(&hostname, d))
}

/// 归一化服务基址（hostname 或完整 URL → http(s) base URL，去掉尾斜杠）。
/// 与 normalize_domain 的区别：保留 scheme；无 scheme 时默认补 https。
pub fn normalize_base_url(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let (scheme, rest) = if let Some(rest) = trimmed.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        ("http", rest)
    } else if trimmed.contains("://") {
        // 显式非 http(s) scheme（或空 scheme）→ 拒绝
        return None;
    } else {
        ("https", trimmed)
    };

    let host = rest.split(['/', '?', '#']).next()?.trim();
    if host.is_empty() {
        None
    } else {
        Some(format!("{}://{}", scheme, host))
    }
}

/// 归一化企业域名（接受 hostname 或 URL，返回 hostname）
pub fn normalize_domain(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    let without_scheme = if let Some(rest) = trimmed.strip_prefix("https://") {
        rest
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        rest
    } else if trimmed.contains("://") {
        // 显式非 http(s) scheme（或空 scheme）→ 拒绝
        return None;
    } else {
        trimmed
    };

    let host = without_scheme.split(['/', '?', '#']).next()?.trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_string())
    }
}

/// 归一化域名（用于域名过滤）：小写、去尾点、校验合法 FQDN。
///
/// 与 [`normalize_domain`] 的区别：这里会小写并拒绝非法输入（单标签 host、含空格等），
/// 保证结果可直接与 [`passes_domain_filter`] 里的 hostname 比较。
pub fn normalize_hostname(input: &str) -> Option<String> {
    let mut normalized = input.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return None;
    }
    let parsed = if normalized.contains("://") {
        url::Url::parse(&normalized).ok()
    } else {
        url::Url::parse(&format!("https://{normalized}")).ok()
    };
    normalized = match parsed {
        Some(url) => url.host_str().unwrap_or("").to_string(),
        None => normalized
            .split(['/', ':'])
            .next()
            .unwrap_or("")
            .to_string(),
    };
    let normalized = normalized.trim_matches('.').to_string();
    let valid = normalized.split('.').count() >= 2
        && normalized
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    valid.then_some(normalized)
}

/// `https` URL 且路径不是根（CIMD 的 client_id 要求，与 rmcp 的校验口径一致）。
pub fn is_https_with_path(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    url.scheme() == "https"
        && url.host_str().is_some_and(|host| !host.is_empty())
        && !url.path().trim_matches('/').is_empty()
}

/// URL 的 host 是否为本机回环（`localhost` / `127.0.0.0/8` / `::1`）。
pub fn is_loopback_url(value: &str) -> bool {
    let Ok(url) = Url::parse(value) else {
        return false;
    };
    match url.host_str() {
        Some("localhost") => true,
        // `Url::host_str` 对 IPv6 带方括号（`[::1]`），解析前先去掉
        Some(host) => host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    /// 用给定的 (名, 值) 对构造 HeaderMap，覆盖头解析的各分支测试。
    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        m
    }

    #[test]
    fn retry_after_headers_parse() {
        let h = headers(&[("retry-after-ms", "2500")]);
        assert_eq!(retry_after_ms(&h), Some(2500));
        let h = headers(&[("retry-after", "2")]);
        assert_eq!(retry_after_ms(&h), Some(2000));
        let h = headers(&[("retry-after", "1.5")]);
        assert_eq!(retry_after_ms(&h), Some(1500));
    }

    #[test]
    fn retry_after_invalid_values_fall_back_to_backoff() {
        // 非法数值不得被当成 0（立即重试）
        let h = headers(&[("retry-after-ms", "NaN")]);
        assert_eq!(retry_after_ms(&h), None);
        let h = headers(&[("retry-after-ms", "inf")]);
        assert_eq!(retry_after_ms(&h), None);
        // 非法数值：非法 retry-after-ms 继续看 retry-after
        let h = headers(&[("retry-after-ms", "oops"), ("retry-after", "3")]);
        assert_eq!(retry_after_ms(&h), Some(3000));
        // 无法解析的日期（pi #9571 的原始场景）
        let h = headers(&[("retry-after", "not-a-date")]);
        assert_eq!(retry_after_ms(&h), None);
    }

    #[test]
    fn retry_after_http_date() {
        let future = chrono::Utc::now() + chrono::Duration::seconds(60);
        let h = headers(&[(
            "retry-after",
            &future.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
        )]);
        let ms = retry_after_ms(&h).expect("未来日期应解析出等待时长");
        assert!((50_000..=70_000).contains(&ms), "unexpected delay {ms}");

        let past = chrono::Utc::now() - chrono::Duration::seconds(60);
        let h = headers(&[(
            "retry-after",
            &past.format("%a, %d %b %Y %H:%M:%S GMT").to_string(),
        )]);
        assert_eq!(retry_after_ms(&h), None, "过期日期应回落指数退避");
    }

    #[test]
    fn normalize_base_url_variants() {
        assert_eq!(
            normalize_base_url("company.ghe.com").as_deref(),
            Some("https://company.ghe.com")
        );
        assert_eq!(
            normalize_base_url("https://company.ghe.com").as_deref(),
            Some("https://company.ghe.com")
        );
        assert_eq!(
            normalize_base_url("http://127.0.0.1:8080").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(
            normalize_base_url("https://auth.kimi.com/").as_deref(),
            Some("https://auth.kimi.com")
        );
        assert_eq!(
            normalize_base_url("  https://x.com/path?q=1  ").as_deref(),
            Some("https://x.com")
        );
        assert_eq!(normalize_base_url(""), None);
        assert_eq!(normalize_base_url("://bad"), None);
    }

    #[test]
    fn normalize_domain_accepts_variants() {
        assert_eq!(
            normalize_domain("company.ghe.com").as_deref(),
            Some("company.ghe.com")
        );
        assert_eq!(
            normalize_domain("https://company.ghe.com").as_deref(),
            Some("company.ghe.com")
        );
        assert_eq!(
            normalize_domain("  company.ghe.com  ").as_deref(),
            Some("company.ghe.com")
        );
        assert_eq!(
            normalize_domain("http://company.ghe.com/path").as_deref(),
            Some("company.ghe.com")
        );
        assert_eq!(normalize_domain(""), None);
        assert_eq!(normalize_domain("   "), None);
        assert_eq!(normalize_domain("://bad"), None);
    }

    #[test]
    fn normalize_hostname_lowercases_and_validates() {
        assert_eq!(
            normalize_hostname("GitHub.COM").as_deref(),
            Some("github.com")
        );
        assert_eq!(
            normalize_hostname("https://Docs.rs/crate").as_deref(),
            Some("docs.rs")
        );
        assert_eq!(
            normalize_hostname("docs.rs:8443").as_deref(),
            Some("docs.rs")
        );
        assert_eq!(
            normalize_hostname(".github.com.").as_deref(),
            Some("github.com")
        );
        // 单标签 host 与非法字符被拒（域名过滤要求可匹配的 FQDN）
        assert_eq!(normalize_hostname("localhost"), None);
        assert_eq!(normalize_hostname("not a domain"), None);
        assert_eq!(normalize_hostname(""), None);
    }

    #[test]
    fn base_url_resolution_order() {
        assert_eq!(
            resolve_base_url(
                Some(" https://x.com/api/ "),
                "PRUX_TEST_UNSET_ENV",
                "https://d"
            ),
            "https://x.com/api"
        );
        assert_eq!(
            resolve_base_url(Some("   "), "PRUX_TEST_UNSET_ENV", "https://d/"),
            "https://d"
        );
        assert_eq!(
            resolve_base_url(None, "PRUX_TEST_UNSET_ENV", "https://d"),
            "https://d"
        );
    }

    #[test]
    fn query_and_domain_helpers() {
        assert_eq!(
            with_query("https://x.com/s", &[("q", "a b"), ("n", "5")]),
            "https://x.com/s?q=a+b&n=5"
        );
        assert_eq!(with_query("not a url", &[("q", "1")]), "not a url");

        assert!(host_matches("github.com", "github.com"));
        assert!(host_matches("a.github.com", "github.com"));
        assert!(!host_matches("notgithub.com", "github.com"));

        let include = vec!["github.com".to_string()];
        let exclude = vec!["old.github.com".to_string()];
        assert!(passes_domain_filter(
            "https://github.com/a",
            &include,
            &exclude
        ));
        assert!(!passes_domain_filter(
            "https://old.github.com/a",
            &include,
            &exclude
        ));
        assert!(!passes_domain_filter("https://x.com/a", &include, &exclude));
        assert!(passes_domain_filter("https://x.com/a", &[], &[]));
        assert!(!passes_domain_filter("not a url", &include, &[]));
    }

    #[test]
    fn build_client_honours_options() {
        let client = build_client_with_options(&ClientOptions {
            timeout: Duration::from_secs(5),
            connect_timeout: Some(Duration::from_secs(2)),
            user_agent: Some("test-agent"),
            follow_redirects: false,
            pool_max_idle_per_host: 2,
            proxy: None,
        })
        .expect("client builds");
        let _ = client;
        assert!(
            build_client_with_options(&ClientOptions {
                proxy: Some("not a proxy"),
                ..Default::default()
            })
            .unwrap_err()
            .starts_with("Invalid proxy")
        );
    }

    /// 建连后一个字节都不回的对端必须在读超时后失败，而不是永久挂起：
    /// 挂起期间请求既不成功也不失败，没有超时兜底，进程只能一直等。
    #[tokio::test]
    async fn silent_peer_hits_read_timeout() {
        use tokio::net::TcpListener;

        // 代理环境变量会把请求绕开这台假服务器，先清掉
        for key in [
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
        ] {
            unsafe { std::env::remove_var(key) };
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            // 收下连接后静默：不发响应头也不发数据（半开连接 / 服务端 hang）
            let _held = listener.accept().await;
            std::future::pending::<()>().await;
        });

        let client = build_client_with_read_timeout(Duration::from_millis(200)).unwrap();
        let err = client
            .get(format!("http://{addr}/v1/chat/completions"))
            .send()
            .await
            .expect_err("静默连接不应成功返回");
        assert!(err.is_timeout(), "应为读超时，got: {err}");
    }
}

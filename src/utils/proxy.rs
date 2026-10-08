//! HTTP 代理 / NO_PROXY 解析。

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

#[cfg(test)]
mod tests {
    use super::*;

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
}

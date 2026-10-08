//! URL 抓取与正文提取
//!
//! 本阶段覆盖：
//! - `readable`：HTTP(S) 抓取后用 html2text 转 markdown；
//! - `raw`：原样返回文本响应体；
//! - SSRF 防护：手动逐跳跟随重定向，每一跳都校验协议与目标地址（回环/私有/链路本地等默认拒绝）。

use super::super::util::default_user_agent;
use crate::{core::tools::ToolError, utils::http};
use std::{net::IpAddr, sync::OnceLock, time::Duration};
use strum_macros::{EnumString, IntoStaticStr};
use url::Url;

/// 手动跟随重定向的最大跳数，超过则报错
const MAX_REDIRECTS: usize = 10;
/// 响应体大小上限（32 MiB），超过则拒绝处理
const MAX_BODY_BYTES: usize = 32 * 1024 * 1024;

/// 抓取模式
#[derive(Debug, Clone, Default, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub enum FetchMode {
    /// 抓取后转 markdown 正文（默认）
    #[default]
    Readable,
    /// 原样返回响应体文本
    Raw,
}

impl FetchMode {
    /// 解析模式名（区分大小写，只认 `readable` / `raw`）；无法识别时返回 `None`。
    pub fn parse(s: &str) -> Option<FetchMode> {
        s.parse().ok()
    }

    /// 返回该模式的规范字符串（`readable` / `raw`），用于写回配置与工具参数。
    pub fn as_str(&self) -> &'static str {
        self.into()
    }
}

/// 抓取选项
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// 抓取模式（`readable` / `raw`）
    pub mode: FetchMode,
    /// 单次请求超时
    pub timeout: Duration,
    /// 代理地址；`None` 表示直连
    pub proxy: Option<String>,
    /// `User-Agent` 请求头
    pub user_agent: String,
    /// 是否放行私有/保留地址（即关闭 SSRF 防护）
    pub allow_private: bool,
    /// 额外放行的私有网段（CIDR 或单地址）；仅在 `allow_private` 为假时生效
    pub allow_ranges: Vec<String>,
}

impl Default for FetchOptions {
    /// 默认选项：readable 模式、30s 超时、直连、默认 UA、开启 SSRF 防护且无额外放行网段。
    fn default() -> Self {
        FetchOptions {
            mode: FetchMode::Readable,
            timeout: Duration::from_secs(30),
            proxy: None,
            user_agent: default_user_agent(),
            allow_private: false,
            allow_ranges: Vec::new(),
        }
    }
}

/// 单条抓取结果（字段对齐原版 `ExtractedContent` 的核心子集）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FetchedContent {
    /// 最终 URL（跟随重定向后）；请求失败时为原始输入 URL
    pub url: String,
    /// 页面标题（仅 `readable` 模式可能非空）
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// 正文：`readable` 为 markdown，`raw` 为原始响应文本；失败时为空
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content: String,
    /// 失败原因；成功时为 `None`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// HTTP 状态码；未取得响应（如网络/协议错误）时为 `None`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// 响应 `Content-Type`（保留原始形式，如 `text/html; charset=utf-8`）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
}

impl FetchedContent {
    /// 构造失败结果：只填 `url` 与 `error`，标题/正文为空、状态码为 `None`。
    pub fn error(url: &str, message: impl Into<String>) -> Self {
        FetchedContent {
            url: url.to_string(),
            title: String::new(),
            content: String::new(),
            error: Some(message.into()),
            status: None,
            mime_type: None,
        }
    }
}

/// 抓取单个 URL（含重定向与 SSRF 校验）。
pub async fn fetch_url(raw_url: &str, options: &FetchOptions) -> FetchedContent {
    match fetch_inner(raw_url, options).await {
        Ok(result) => result,
        Err(err) => FetchedContent::error(raw_url, err.0),
    }
}

/// 真正执行抓取：逐跳校验重定向目标（SSRF），再按 `options.mode` 决定返回原始文本还是抽取后的 markdown。
///
/// URL 非法、请求失败、重定向超限等返回 `Err`；HTTP 非 2xx、响应体过大或
/// PDF/图片等暂不支持的内容则返回带 `error` 的 `Ok`（由调用方展示）。
async fn fetch_inner(raw_url: &str, options: &FetchOptions) -> Result<FetchedContent, ToolError> {
    let initial = Url::parse(raw_url)
        .map_err(|err| ToolError(format!("Invalid URL \"{raw_url}\": {err}")))?;
    guard_url(&initial, options)?;

    let client = build_client(options)?;
    let mut current = initial.clone();
    let mut response = None;
    for _ in 0..=MAX_REDIRECTS {
        let request = client
            .get(current.clone())
            .header("accept-encoding", "identity")
            .header(
                "accept",
                "text/html,application/xhtml+xml,application/xml;q=0.9,text/plain;q=0.8,*/*;q=0.5",
            )
            .header("accept-language", "en-US,en;q=0.9");
        let resp = request
            .send()
            .await
            .map_err(|err| ToolError(format!("Request failed for {current}: {err}")))?;
        let status = resp.status();
        if status.is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| ToolError(format!("Redirect from {current} missing Location")))?;
            let next = current.join(location).map_err(|err| {
                ToolError(format!("Invalid redirect target \"{location}\": {err}"))
            })?;
            guard_url(&next, options)?;
            current = next;
            continue;
        }
        response = Some(resp);
        break;
    }
    let response =
        response.ok_or_else(|| ToolError(format!("Too many redirects fetching {raw_url}")))?;

    let status_code = response.status().as_u16();
    let final_url = response.url().clone();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if !response.status().is_success() {
        return Ok(FetchedContent {
            url: final_url.to_string(),
            title: String::new(),
            content: String::new(),
            error: Some(format!(
                "HTTP {status_code} {}",
                response.status().canonical_reason().unwrap_or("")
            )),
            status: Some(status_code),
            mime_type: content_type,
        });
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|err| ToolError(format!("Failed reading body of {final_url}: {err}")))?;
    if bytes.len() > MAX_BODY_BYTES {
        return Ok(FetchedContent::error(
            final_url.as_str(),
            format!("Response too large ({} bytes)", bytes.len()),
        ));
    }

    let mime = content_type
        .as_deref()
        .map(|c| c.split(';').next().unwrap_or(c).trim().to_ascii_lowercase())
        .unwrap_or_default();

    if options.mode == FetchMode::Raw {
        let text = String::from_utf8_lossy(&bytes).into_owned();
        return Ok(FetchedContent {
            url: final_url.to_string(),
            title: String::new(),
            content: text,
            error: None,
            status: Some(status_code),
            mime_type: content_type,
        });
    }

    if mime == "application/pdf" {
        return Ok(FetchedContent::error(
            final_url.as_str(),
            "PDF extraction is not supported yet in this port",
        ));
    }
    if mime.starts_with("image/") {
        return Ok(FetchedContent::error(
            final_url.as_str(),
            "Direct image fetching is not supported yet in this port",
        ));
    }

    let body = String::from_utf8_lossy(&bytes).into_owned();
    let (title, content) = if mime.contains("html") || mime.is_empty() && looks_like_html(&body) {
        extract_readable(&body, &final_url)
    } else {
        (String::new(), body)
    };

    Ok(FetchedContent {
        url: final_url.to_string(),
        title,
        content,
        error: None,
        status: Some(status_code),
        mime_type: content_type,
    })
}

/// 按抓取选项构建 reqwest 客户端：关闭自动重定向（改由 [`fetch_inner`] 逐跳校验后手动跟随）。
fn build_client(options: &FetchOptions) -> Result<reqwest::Client, ToolError> {
    http::build_client_with_options(&http::ClientOptions {
        timeout: options.timeout,
        user_agent: Some(options.user_agent.as_str()),
        proxy: options.proxy.as_deref(),
        follow_redirects: false, // SSRF 防护要求逐跳校验，重定向由本模块手动跟随
        pool_max_idle_per_host: 2,
        ..Default::default()
    })
    .map_err(ToolError)
}

/// 粗略判断响应体是否像 HTML（开头是 `<!doctype html>`/`<html` 或含 `<body`），
/// 用于缺失 `Content-Type` 时兜底决定是否走正文抽取。
fn looks_like_html(body: &str) -> bool {
    let head = body.trim_start();
    head.starts_with("<!doctype html")
        || head.starts_with("<!DOCTYPE html")
        || head.starts_with("<html")
        || head.contains("<body")
}

/// 从 HTML 提取标题与正文 markdown。
pub fn extract_readable(html: &str, base: &Url) -> (String, String) {
    let title = extract_title(html).unwrap_or_default();
    let cleaned = strip_noise(html);
    let absolutized = absolutize_attributes(&cleaned, base);
    let markdown = html2text::from_read(absolutized.as_bytes(), 1_000).unwrap_or_default();
    let trimmed = collapse_blank_lines(markdown.trim());
    let with_base = rewrite_relative_links(&trimmed, base);
    (title, with_base)
}

/// 把 HTML 中 href/src 的相对地址改写为绝对地址（html2text 会输出引用式链接）
fn absolutize_attributes(html: &str, base: &Url) -> String {
    /// `href` / `src` 属性（含引号）正则缓存
    static ATTR: OnceLock<regex::Regex> = OnceLock::new();

    let attr = ATTR.get_or_init(|| {
        regex::Regex::new(r#"(?is)(href|src)\s*=\s*("([^"]*)"|'([^']*)')"#).expect("attr regex")
    });

    attr.replace_all(html, |caps: &regex::Captures| {
        let name = &caps[1];
        let quote = if caps.get(3).is_some() { '"' } else { '\'' };
        let value = caps
            .get(3)
            .or_else(|| caps.get(4))
            .map(|m| m.as_str())
            .unwrap_or("");
        if value.is_empty()
            || value.starts_with("http://")
            || value.starts_with("https://")
            || value.starts_with("mailto:")
            || value.starts_with("#")
            || value.starts_with("data:")
            || value.starts_with("javascript:")
        {
            return caps[0].to_string();
        }
        // protocol-relative 与相对地址都交给 base.join
        let resolved = if let Some(rest) = value.strip_prefix("//") {
            format!("{}://{rest}", base.scheme())
        } else {
            match base.join(value) {
                Ok(abs) => abs.to_string(),
                Err(_) => return caps[0].to_string(),
            }
        };
        format!("{name}={quote}{resolved}{quote}")
    })
    .into_owned()
}

/// 去掉 `<script>`/`<style>`/HTML 注释与 nav/footer/aside 等结构性噪音标签，为正文抽取做粗筛。
fn strip_noise(html: &str) -> String {
    // 去脚本/样式/注释与结构性噪音元素；正则近似原版 readability 的粗筛。`<script>` 块正则缓存
    /// `<script>` 块正则缓存，惰性初始化以避免每次抓取重复编译。
    static SCRIPT: OnceLock<regex::Regex> = OnceLock::new();
    /// `<style>` 块正则缓存
    static STYLE: OnceLock<regex::Regex> = OnceLock::new();
    /// HTML 注释正则缓存
    static COMMENT: OnceLock<regex::Regex> = OnceLock::new();
    /// 结构性噪音标签（nav/footer/aside/...）正则缓存
    static NOISE_TAGS: OnceLock<regex::Regex> = OnceLock::new();

    let script = SCRIPT.get_or_init(|| {
        regex::Regex::new(r"(?is)<script\b[^>]*>.*?</script\s*>").expect("script regex")
    });
    let style = STYLE.get_or_init(|| {
        regex::Regex::new(r"(?is)<style\b[^>]*>.*?</style\s*>").expect("style regex")
    });
    let comment =
        COMMENT.get_or_init(|| regex::Regex::new(r"(?s)<!--.*?-->").expect("comment regex"));
    let noise = NOISE_TAGS.get_or_init(|| {
        // regex crate 不支持反向引用，逐个标签写完整配对
        regex::Regex::new(concat!(
            r"(?is)<nav\b[^>]*>.*?</nav\s*>",
            r"|<footer\b[^>]*>.*?</footer\s*>",
            r"|<aside\b[^>]*>.*?</aside\s*>",
            r"|<noscript\b[^>]*>.*?</noscript\s*>",
            r"|<template\b[^>]*>.*?</template\s*>",
            r"|<svg\b[^>]*>.*?</svg\s*>",
            r"|<iframe\b[^>]*>.*?</iframe\s*>"
        ))
        .expect("noise regex")
    });
    let out = script.replace_all(html, " ");
    let out = style.replace_all(&out, " ");
    let out = comment.replace_all(&out, " ");
    noise.replace_all(&out, " ").into_owned()
}

/// 抽取页面标题：优先 `og:title`，否则回退 `<title>`；都取不到或结果为空时返回 `None`。
fn extract_title(html: &str) -> Option<String> {
    /// `<title>` 标签正则缓存
    static TITLE: OnceLock<regex::Regex> = OnceLock::new();
    /// `og:title` meta 标签正则缓存
    static OG_TITLE: OnceLock<regex::Regex> = OnceLock::new();
    /// `content="..."` 属性正则缓存
    static CONTENT: OnceLock<regex::Regex> = OnceLock::new();

    let og = OG_TITLE.get_or_init(|| {
        regex::Regex::new(r#"(?is)<meta[^>]+property\s*=\s*["']og:title["'][^>]*>"#)
            .expect("og regex")
    });
    let title_re = TITLE
        .get_or_init(|| regex::Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("title regex"));

    // og:title 内容属性可能前后顺序不同，先直接抓 content=
    if let Some(m) = og.find(html) {
        let tag = m.as_str();

        let content = CONTENT.get_or_init(|| {
            regex::Regex::new(r#"(?is)content\s*=\s*["'](.*?)["']"#).expect("content regex")
        });

        if let Some(c) = content.captures(tag).and_then(|c| c.get(1)) {
            let value = decode_entities(c.as_str()).trim().to_string();
            if !value.is_empty() {
                return Some(value);
            }
        }
    }
    title_re
        .captures(html)
        .and_then(|c| c.get(1))
        .map(|m| decode_entities(m.as_str()).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// 把 markdown 里的相对链接目标改写为绝对 URL（`base.join`）；
/// 已是绝对地址或 `#`/`data:`/`mailto:` 等特殊目标则原样保留。
fn rewrite_relative_links(markdown: &str, base: &Url) -> String {
    // markdown 链接是 [text](url)，把相对 URL 绝对化，方便后续按 URL 检索。
    /// markdown 链接目标正则缓存
    static LINK: OnceLock<regex::Regex> = OnceLock::new();

    let link = LINK
        .get_or_init(|| regex::Regex::new(r#"\]\(([^)\s]+)(\s+"[^"]*")?\)"#).expect("link regex"));

    link.replace_all(markdown, |caps: &regex::Captures| {
        let raw = &caps[1];
        let suffix = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        if raw.starts_with("http://")
            || raw.starts_with("https://")
            || raw.starts_with("mailto:")
            || raw.starts_with('#')
            || raw.starts_with("data:")
        {
            return caps[0].to_string();
        }
        match base.join(raw) {
            Ok(abs) => format!("]({}{})", abs, suffix),
            Err(_) => caps[0].to_string(),
        }
    })
    .into_owned()
}

/// 把连续 3 个以上的换行折叠成一个空行，压缩抽取文本里的多余空行。
fn collapse_blank_lines(text: &str) -> String {
    /// 连续空行正则缓存
    static BLANKS: OnceLock<regex::Regex> = OnceLock::new();

    let re = BLANKS.get_or_init(|| regex::Regex::new(r"\n{3,}").expect("blanks regex"));
    re.replace_all(text, "\n\n").into_owned()
}

/// 简易 HTML 实体解码（&amp; &lt; &#39; 等）
pub fn decode_entities(text: &str) -> String {
    /// HTML 实体（命名/十进制/十六进制）正则缓存
    static ENTITY: OnceLock<regex::Regex> = OnceLock::new();

    let re = ENTITY
        .get_or_init(|| regex::Regex::new(r"&(#x?[0-9a-fA-F]+|[a-zA-Z]+);").expect("entity regex"));

    re.replace_all(text, |caps: &regex::Captures| {
        let body = &caps[1];
        if let Some(hex) = body.strip_prefix("#x").or_else(|| body.strip_prefix("#X")) {
            return u32::from_str_radix(hex, 16)
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_else(|| caps[0].to_string());
        }

        if let Some(dec) = body.strip_prefix('#') {
            return dec
                .parse::<u32>()
                .ok()
                .and_then(char::from_u32)
                .map(|c| c.to_string())
                .unwrap_or_else(|| caps[0].to_string());
        }
        match body {
            "amp" => "&".to_string(),
            "lt" => "<".to_string(),
            "gt" => ">".to_string(),
            "quot" => "\"".to_string(),
            "apos" => "'".to_string(),
            "nbsp" => " ".to_string(),
            "mdash" => "—".to_string(),
            "ndash" => "–".to_string(),
            "hellip" => "…".to_string(),
            "copy" => "©".to_string(),
            _ => caps[0].to_string(),
        }
    })
    .into_owned()
}

/// 校验 URL：协议必须 http/https；非白名单目标拒绝回环/私有/链路本地等地址。
pub fn guard_url(url: &Url, options: &FetchOptions) -> Result<(), ToolError> {
    match url.scheme() {
        "http" | "https" => {}
        other => {
            return Err(ToolError(format!(
                "Unsupported URL scheme \"{other}\" (only http/https)"
            )));
        }
    }
    let host = url
        .host_str()
        .ok_or_else(|| ToolError(format!("URL has no host: {url}")))?;
    if options.allow_private {
        return Ok(());
    }
    let allow: Vec<Cidr> = options
        .allow_ranges
        .iter()
        .filter_map(|s| Cidr::parse(s))
        .collect();

    if let Ok(ip) = host.parse::<IpAddr>() {
        return check_ip(ip, &allow);
    }

    // DNS 解析（阻塞式线程池解析亦可；这里用 std 避免依赖运行时 net feature 细节）
    let host_owned = host.to_string();
    let addrs = std::net::ToSocketAddrs::to_socket_addrs(&(host_owned.as_str(), 0))
        .map_err(|err| ToolError(format!("DNS resolution failed for {host}: {err}")))?;
    let mut any = false;
    for addr in addrs {
        any = true;
        check_ip(addr.ip(), &allow)?;
    }
    if !any {
        return Err(ToolError(format!("No addresses resolved for {host}")));
    }
    Ok(())
}

/// 校验单个 IP：命中 `allow` 白名单直接放行，否则命中私有/保留网段时返回 SSRF 错误。
fn check_ip(ip: IpAddr, allow: &[Cidr]) -> Result<(), ToolError> {
    if allow.iter().any(|c| c.contains(ip)) {
        return Ok(());
    }
    if is_blocked_ip(ip) {
        return Err(ToolError(format!(
            "Blocked request to private/reserved address {ip} (SSRF protection)"
        )));
    }
    Ok(())
}

/// 判断 IP 是否属于回环/私有/链路本地/组播/保留等 SSRF 防护要拒绝的网段
/// （IPv4-mapped IPv6 按其 IPv4 地址判定）。
fn is_blocked_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_documentation()
                || o[0] == 0
                || (o[0] == 100 && (64..=127).contains(&o[1])) // CGNAT 100.64/10
                || o[0] >= 224 // multicast + reserved
        }
        IpAddr::V6(v6) => {
            if let Some(mapped) = v6.to_ipv4_mapped() {
                return is_blocked_ip(IpAddr::V4(mapped));
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xfe00) == 0xfc00 // fc00::/7 ULA
                || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

/// CIDR / 单地址匹配（IPv4 + IPv6）
#[derive(Debug, Clone, Copy)]
pub struct Cidr {
    /// 网段基地址
    addr: IpAddr,
    /// 前缀长度（IPv4 为 0-32，IPv6 为 0-128）
    prefix: u8,
}

impl Cidr {
    /// 解析 `a.b.c.d/len` 或单个地址（无 `/` 时前缀取满长）；
    /// 地址非法或前缀越界时返回 `None`。
    pub fn parse(text: &str) -> Option<Cidr> {
        let text = text.trim();
        let (addr_part, prefix_part) = match text.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (text, None),
        };
        let addr: IpAddr = addr_part.parse().ok()?;
        let max = match addr {
            IpAddr::V4(_) => 32,
            IpAddr::V6(_) => 128,
        };
        let prefix = match prefix_part {
            Some(p) => p.parse::<u8>().ok().filter(|p| *p <= max)?,
            None => max,
        };
        Some(Cidr { addr, prefix })
    }

    /// 判断 `ip` 是否落在本网段内；地址族不同（IPv4 对 IPv6）时返回 `false`。
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let net = u32::from(net);
                let ip = u32::from(ip);
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u32::MAX << (32 - self.prefix)
                };
                net & mask == ip & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let net = u128::from(net);
                let ip = u128::from(ip);
                let mask = if self.prefix == 0 {
                    0
                } else {
                    u128::MAX << (128 - self.prefix)
                };
                net & mask == ip & mask
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_private_and_allows_public() {
        let opts = FetchOptions::default();
        assert!(guard_url(&Url::parse("http://127.0.0.1/").unwrap(), &opts).is_err());
        assert!(guard_url(&Url::parse("http://10.0.0.5/").unwrap(), &opts).is_err());
        assert!(guard_url(&Url::parse("http://192.168.1.1/").unwrap(), &opts).is_err());
        assert!(guard_url(&Url::parse("http://169.254.1.1/").unwrap(), &opts).is_err());
        assert!(guard_url(&Url::parse("ftp://example.com/").unwrap(), &opts).is_err());
        assert!(guard_url(&Url::parse("http://93.184.216.34/").unwrap(), &opts).is_ok());
    }

    #[test]
    fn allow_ranges_override_block() {
        let opts = FetchOptions {
            allow_ranges: vec!["127.0.0.0/8".to_string()],
            ..FetchOptions::default()
        };
        assert!(guard_url(&Url::parse("http://127.0.0.1/").unwrap(), &opts).is_ok());
    }

    #[test]
    fn extracts_title_and_markdown() {
        let html = r#"<html><head><title>Hello &amp; World</title>
            <style>body{color:red}</style></head>
            <body><nav>menu</nav><h1>Title</h1><p>Some <b>bold</b> text.</p>
            <script>alert(1)</script><a href="/rel">rel</a></body></html>"#;
        let base = Url::parse("https://example.com/dir/page").unwrap();
        let (title, md) = extract_readable(html, &base);
        assert_eq!(title, "Hello & World");
        assert!(md.contains("Title"), "{md}");
        assert!(md.contains("Some"), "{md}");
        assert!(!md.contains("alert"), "{md}");
        assert!(!md.contains("color:red"), "{md}");
        assert!(!md.contains("menu"), "{md}");
        assert!(md.contains("https://example.com/rel"), "{md}");
    }

    #[test]
    fn cidr_matching() {
        let c = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(c.contains("10.1.2.3".parse().unwrap()));
        assert!(!c.contains("11.1.2.3".parse().unwrap()));
        let v6 = Cidr::parse("fc00::/7").unwrap();
        assert!(v6.contains("fc00::1".parse().unwrap()));
        assert!(Cidr::parse("bogus").is_none());
    }

    #[test]
    fn decode_entities_works() {
        assert_eq!(decode_entities("a &amp; b &#39;c&#x27;"), "a & b 'c'");
    }
}

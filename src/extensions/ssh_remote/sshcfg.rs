//! `~/.ssh/config` 读取：把主机别名解析成实际连接参数。
//!
//! 只支持建立连接必需的那部分指令（`Host` / `HostName` / `User` / `Port` /
//! `IdentityFile` / `ProxyJump`），并遵循 OpenSSH 的「首个匹配生效」：一个参数一旦由
//! 某条 `Host` 块给出，后续块的同名指令不再覆盖它。
//!
//! `Include` 只展开一层（相对路径按 `~/.ssh` 解析）；嵌套 include 不再递归，
//! 避免用户配置里的循环引用把启动卡住。

use crate::utils::{
    ignore::pattern_matches,
    paths::{expand_tilde, home_dir},
};
use std::path::{Path, PathBuf};

/// 从 ssh config 解析出的一台主机
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SshHost {
    /// `Host` 行里的原始模式（别名，可能含通配符）
    pub alias: String,
    /// 实际主机名（`HostName`；未给出时为别名本身）
    pub hostname: String,
    /// 登录用户名（`User`）
    pub user: Option<String>,
    /// 端口（`Port`）
    pub port: Option<u16>,
    /// 私钥路径（`IdentityFile`，已展开 `~`）
    pub identity_file: Option<String>,
    /// 跳板机（`ProxyJump`）
    pub proxy_jump: Option<String>,
}

/// ssh 配置目录
pub fn ssh_dir() -> Option<PathBuf> {
    let home = home_dir();
    (!home.as_os_str().is_empty()).then(|| home.join(".ssh"))
}

/// 解析默认配置文件 `~/.ssh/config`；文件不存在时返回空列表
pub fn parse_default() -> Vec<SshHost> {
    let Some(dir) = ssh_dir() else {
        return Vec::new();
    };
    parse_file(&dir.join("config"), &dir)
}

/// 按别名查主机参数：返回首个 `Host` 模式命中 `alias`（或 `hostname`）的块
pub fn lookup(alias: &str) -> Option<SshHost> {
    parse_default().into_iter().find(|h| {
        h.alias == alias
            || h.hostname == alias
            || (h.alias.contains(['*', '?']) && pattern_matches(&h.alias, alias))
    })
}

/// 解析指定文件
pub fn parse_file(path: &Path, ssh_dir: &Path) -> Vec<SshHost> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    parse_str(&text, ssh_dir)
}

/// 解析配置文本：每个 `Host` 块产出一项，独占一个匹配模式时 `hostname` 留空
/// （表示「别名通配」，由调用方按需展开）。
pub fn parse_str(text: &str, ssh_dir: &Path) -> Vec<SshHost> {
    let mut hosts: Vec<SshHost> = Vec::new();
    let mut current: Option<usize> = None;
    // 每个块已显式给出的字段（保证「首个匹配生效」）
    let mut seen: Vec<u8> = Vec::new();
    let mut include_targets: Vec<PathBuf> = Vec::new();

    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // 兼容 `Key=value` 与 `Key value`
        let (key, value) = match line.split_once('=') {
            Some((k, v)) => (k.trim(), v.trim()),
            None => match line.split_once(char::is_whitespace) {
                Some((k, v)) => (k.trim(), v.trim()),
                None => (line, ""),
            },
        };
        let key_lower = key.to_ascii_lowercase();
        if key_lower == "host" {
            let pattern = value.split_whitespace().next().unwrap_or("");
            hosts.push(SshHost {
                alias: pattern.to_string(),
                hostname: pattern.to_string(),
                ..Default::default()
            });
            current = Some(hosts.len() - 1);
            seen.push(0);
            continue;
        }

        // `Host` 之前的全局指令（`Host *` 之外）OpenSSH 不加限制施加到所有主机；
        // 这里要求必须在某个 Host 块内，避免把全局 `User` 误当成具体别名。
        let Some(idx) = current else {
            if key_lower == "include" {
                include_targets.push(expand_include(value, ssh_dir));
            }
            continue;
        };
        let Some(host) = hosts.get_mut(idx) else {
            continue;
        };
        let flags = &mut seen[idx];
        match key_lower.as_str() {
            "hostname" => {
                if *flags & 1 == 0 {
                    host.hostname = value.to_string();
                    *flags |= 1;
                }
            }
            "user" => {
                if *flags & 2 == 0 {
                    host.user = Some(value.to_string());
                    *flags |= 2;
                }
            }
            "port" => {
                if *flags & 4 == 0
                    && let Ok(p) = value.parse::<u16>()
                    && p > 0
                {
                    host.port = Some(p);
                    *flags |= 4;
                }
            }
            "identityfile" => {
                if *flags & 8 == 0 {
                    host.identity_file = Some(
                        expand_tilde(PathBuf::from(value))
                            .to_string_lossy()
                            .to_string(),
                    );
                    *flags |= 8;
                }
            }
            "proxyjump" => {
                if *flags & 16 == 0 && !value.eq_ignore_ascii_case("none") {
                    host.proxy_jump = Some(value.to_string());
                    *flags |= 16;
                }
            }
            "include" => include_targets.push(expand_include(value, ssh_dir)),
            _ => {}
        }
    }

    // 一层 Include：把外部文件里的主机块接在后面（不再递归）
    for target in include_targets {
        hosts.extend(parse_file(&target, ssh_dir));
    }
    hosts
}

/// `Include` 的相对路径按 `~/.ssh` 解析
fn expand_include(value: &str, ssh_dir: &Path) -> PathBuf {
    let expanded = expand_tilde(PathBuf::from(value.trim()));
    if expanded.is_absolute() {
        expanded
    } else {
        ssh_dir.join(expanded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
Host gpu-box
    HostName gpu.example.com
    User root
    Port 2202
    IdentityFile ~/.ssh/id_ed25519

Host *.internal
    User deploy

Host *
    User fallback
"#;

    #[test]
    fn parses_host_blocks() {
        let hosts = parse_str(SAMPLE, Path::new("/home/u/.ssh"));
        assert_eq!(hosts.len(), 3);
        assert_eq!(hosts[0].hostname, "gpu.example.com");
        assert_eq!(hosts[0].user.as_deref(), Some("root"));
        assert_eq!(hosts[0].port, Some(2202));
        assert!(
            hosts[0]
                .identity_file
                .as_deref()
                .unwrap()
                .ends_with(".ssh/id_ed25519")
        );
        assert_eq!(hosts[1].hostname, "*.internal");
    }

    #[test]
    fn glob_patterns() {
        assert!(pattern_matches("gpu-*", "gpu-1"));
        assert!(pattern_matches("gpu-?", "gpu-1"));
        assert!(!pattern_matches("gpu-?", "gpu-11"));
        assert!(pattern_matches("!gpu-1", "gpu-2"));
        assert!(!pattern_matches("!gpu-1", "gpu-1"));
    }
}

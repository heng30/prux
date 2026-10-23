//! 本地端口转发：把远端服务映射到 `127.0.0.1:<localPort>`。
//!
//! 这是「远程资源本地化」的通道：映射之后，本地工具、本地 MCP server、本地数据库
//! 客户端全部照旧连 `localhost`，不需要任何工具改造。转发记录写在
//! `config.json` 的 `tunnels` 里，扩展启用时自动拉起。

use crate::extensions::ssh_remote::{
    state::{RemoteConfig, TunnelSpec},
    transport,
};
use std::{
    collections::BTreeMap,
    sync::{Mutex, OnceLock},
};
use tokio::{net::TcpListener, sync::oneshot, task::JoinHandle};

/// 本地端口 → 运行中的转发
static RUNNING: OnceLock<Mutex<BTreeMap<u16, Running>>> = OnceLock::new();

/// 一条正在运行的转发
struct Running {
    /// 转发规格
    spec: TunnelSpec,
    /// 关闭信号（drop 后 accept 循环退出）
    shutdown: oneshot::Sender<()>,
    /// accept 循环的任务句柄
    task: JoinHandle<()>,
}

/// 运行表容器
fn running() -> &'static Mutex<BTreeMap<u16, Running>> {
    RUNNING.get_or_init(|| Mutex::new(BTreeMap::new()))
}

/// 当前活跃转发（按本地端口排序）
pub fn active() -> Vec<TunnelSpec> {
    running()
        .lock()
        .unwrap()
        .values()
        .map(|r| r.spec.clone())
        .collect()
}

/// 启动一条转发；`local_port` 为 0 时由系统分配，返回实际端口
pub async fn start(spec: TunnelSpec) -> Result<u16, String> {
    if spec.local_port != 0 && running().lock().unwrap().contains_key(&spec.local_port) {
        return Err(format!(
            "local port {} is already forwarded",
            spec.local_port
        ));
    }

    let listener = TcpListener::bind(("127.0.0.1", spec.local_port))
        .await
        .map_err(|e| format!("bind 127.0.0.1:{}: {e}", spec.local_port))?;
    let actual = listener
        .local_addr()
        .map_err(|e| format!("local addr: {e}"))?
        .port();

    let conn = if spec.host.trim().is_empty() {
        transport::resolve_default().await?
    } else {
        transport::resolve_host(&spec.host).await?
    };

    let (shutdown, mut stopped) = oneshot::channel::<()>();
    let target = (spec.remote_host.clone(), spec.remote_port);
    let task = tokio::spawn(async move {
        loop {
            let accepted = tokio::select! {
                _ = &mut stopped => break,
                accepted = listener.accept() => accepted,
            };

            let Ok((mut local, _)) = accepted else {
                continue;
            };

            let handle = conn.handle.clone();
            let (remote_host, remote_port) = target.clone();

            // 每条客户端连接一条 direct-tcpip 通道，互相不阻塞
            tokio::spawn(async move {
                let Ok(channel) = handle
                    .channel_open_direct_tcpip(&remote_host, remote_port as u32, "127.0.0.1", 0)
                    .await
                else {
                    return;
                };
                let mut stream = channel.into_stream();
                _ = tokio::io::copy_bidirectional(&mut local, &mut stream).await;
            });
        }
    });

    let mut stored = spec.clone();
    stored.local_port = actual;
    running().lock().unwrap().insert(
        actual,
        Running {
            spec: stored,
            shutdown,
            task,
        },
    );
    Ok(actual)
}

/// 停掉一条转发；返回是否存在
pub fn stop(local_port: u16) -> bool {
    let Some(r) = running().lock().unwrap().remove(&local_port) else {
        return false;
    };
    _ = r.shutdown.send(());
    r.task.abort();
    true
}

/// 停掉全部转发；返回数量
pub fn stop_all() -> usize {
    let items: Vec<u16> = running().lock().unwrap().keys().copied().collect();
    let mut count = 0;
    for port in items {
        if stop(port) {
            count += 1;
        }
    }
    count
}

/// 按配置拉起全部转发；返回 `(规格文本, 结果)`
///
/// 单条失败不影响其它条目（端口被占、主机暂时连不上都只报出来）。
pub async fn restore_all() -> Vec<(String, Result<u16, String>)> {
    let cfg = RemoteConfig::load();
    let mut out = Vec::new();
    for spec in cfg.tunnels {
        let text = spec.to_spec_text();
        let result = start(spec).await;
        out.push((text, result));
    }
    out
}

/// 把一条转发写进配置（去重后保存）
pub fn remember(spec: &TunnelSpec) -> Result<(), String> {
    let mut cfg = RemoteConfig::load();
    if !cfg
        .tunnels
        .iter()
        .any(|t| t.to_spec_text() == spec.to_spec_text())
    {
        cfg.tunnels.push(spec.clone());
    }
    cfg.save()
}

/// 从配置里删掉一条转发（按本地端口匹配）
pub fn forget(local_port: u16) -> Result<bool, String> {
    let mut cfg = RemoteConfig::load();
    let before = cfg.tunnels.len();
    cfg.tunnels.retain(|t| t.local_port != local_port);
    let changed = cfg.tunnels.len() != before;
    if changed {
        cfg.save()?;
    }
    Ok(changed)
}

/// 清空配置里的全部转发，返回删掉的条数。
///
/// 只改配置，不管正在跑的转发：停掉它们由 [`stop_all`] 负责，两者分开才能让
/// `/tunnel stop-all`（暂时断）与 `/tunnel remove-all`（永久删）语义不混。
/// 配置本来就空时不写文件，避免无谓的磁盘写入。
pub fn forget_all() -> Result<usize, String> {
    let mut cfg = RemoteConfig::load();
    let removed = cfg.tunnels.len();
    if removed == 0 {
        return Ok(0);
    }

    cfg.tunnels.clear();
    cfg.save()?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端口占用检查走的是运行表，不碰网络
    #[test]
    fn stop_unknown_port_is_noop() {
        assert!(!stop(1));
        assert!(active().is_empty());
        assert_eq!(stop_all(), 0);
    }

    /// 配置往返：记住、去重、忘记
    #[test]
    fn remember_dedups_and_forget_removes() {
        let _l = crate::extensions::ssh_remote::state::TEST_ROOT_LOCK.blocking_lock();
        let _g = crate::extensions::ssh_remote::state::TestRootGuard::temp();
        let spec = TunnelSpec {
            host: String::new(),
            local_port: 15432,
            remote_host: "db".to_string(),
            remote_port: 5432,
        };
        remember(&spec).unwrap();
        remember(&spec).unwrap();
        let cfg = RemoteConfig::load();
        assert_eq!(cfg.tunnels.len(), 1, "同一条转发不应重复写入");
        assert!(forget(15432).unwrap());
        assert!(!forget(15432).unwrap(), "重复删除返回 false");
        assert!(RemoteConfig::load().tunnels.is_empty());
    }

    /// 批量清空：返回删掉条数，第二次清空返回 0
    #[test]
    fn forget_all_clears_every_configured_forward() {
        let _l = crate::extensions::ssh_remote::state::TEST_ROOT_LOCK.blocking_lock();
        let _g = crate::extensions::ssh_remote::state::TestRootGuard::temp();
        let spec = |local_port: u16| TunnelSpec {
            host: String::new(),
            local_port,
            remote_host: "db".to_string(),
            remote_port: 5432,
        };
        remember(&spec(15432)).unwrap();
        remember(&spec(15433)).unwrap();
        assert_eq!(forget_all().unwrap(), 2);
        assert!(RemoteConfig::load().tunnels.is_empty());
        assert_eq!(forget_all().unwrap(), 0, "已空时返回 0");
    }
}

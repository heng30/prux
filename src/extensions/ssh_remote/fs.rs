//! SFTP 原语：读远端文件、列目录、递归上传/下载。
//!
//! 这些原语不改写任何内置工具的行为，只服务于 `remote_read` / `remote_ls` /
//! `remote_sync` 三个显式工具。小文件走 SFTP 全量读（切行语义与本地 `read` 一致），
//! 大文件走「远端算行号 + 取范围」，避免为读 20 行把 2 GB 日志拉回本地。

use crate::{
    extensions::ssh_remote::{
        state::Limits,
        transport::{self, RemoteConn},
    },
    utils::{
        paths::parent_posix,
        process::shell_quote,
        truncate::{LineRange, line_range},
    },
};
use russh_sftp::client::SftpSession;
use std::{path::Path, sync::Arc};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// 小于该字节数的文件直接全量读（本地切行，语义与本地 read 一致）
const FULL_READ_LIMIT: u64 = 2 * 1024 * 1024;

/// 单次 `remote_sync` 允许的文件数上限（防止误操作搬走整个 HOME）
const MAX_SYNC_FILES: usize = 2000;

/// 文件元数据
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteStat {
    /// 是否目录
    pub is_dir: bool,
    /// 字节数（目录时为 0）
    pub size: u64,
}

/// 一个已打开的 SFTP 通道
pub struct Sftp {
    /// 所属连接（大文件范围读要靠它跑 `wc` / `sed`）
    conn: Arc<RemoteConn>,
    /// SFTP 会话
    session: Arc<SftpSession>,
}

/// 在一个连接上打开 SFTP 通道
pub async fn open(conn: &Arc<RemoteConn>) -> Result<Sftp, String> {
    let channel = conn
        .handle
        .channel_open_session()
        .await
        .map_err(|e| format!("open sftp channel: {e}"))?;
    channel
        .request_subsystem(true, "sftp")
        .await
        .map_err(|e| format!("request sftp subsystem: {e}"))?;
    let session = SftpSession::new(channel.into_stream())
        .await
        .map_err(|e| format!("sftp handshake: {e}"))?;
    Ok(Sftp {
        conn: conn.clone(),
        session: Arc::new(session),
    })
}

impl Sftp {
    /// 元数据
    pub async fn stat(&self, path: &str) -> Result<RemoteStat, String> {
        let meta = self
            .session
            .metadata(path)
            .await
            .map_err(|e| format!("stat {path}: {e}"))?;
        Ok(RemoteStat {
            is_dir: meta.is_dir(),
            size: meta.size.unwrap_or(0),
        })
    }

    /// 目录条目（类型随一次往返带回，不逐条 stat）
    pub async fn list(&self, path: &str) -> Result<Vec<(String, bool)>, String> {
        let mut out: Vec<(String, bool)> = self
            .session
            .read_dir(path)
            .await
            .map_err(|e| format!("list {path}: {e}"))?
            .map(|entry| (entry.file_name(), entry.file_type().is_dir()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(out)
    }

    /// 全量读字节
    pub async fn read_all(&self, path: &str) -> Result<Vec<u8>, String> {
        self.session
            .read(path)
            .await
            .map_err(|e| format!("read {path}: {e}"))
    }

    /// 按行范围读：小文件全量后本地切行，大文件只在远端取需要的范围
    pub async fn read_range(
        &self,
        path: &str,
        offset: Option<i64>,
        limit: Option<i64>,
        limits: &Limits,
    ) -> Result<LineRange, String> {
        let stat = self.stat(path).await?;
        if stat.is_dir {
            return Err(format!("{path} is a directory"));
        }
        if stat.size <= FULL_READ_LIMIT {
            let bytes = self.read_all(path).await?;
            return Ok(line_range(&bytes, offset, limit));
        }
        self.read_range_remote(path, offset, limit, limits).await
    }

    /// 大文件范围读：远端 `wc -l` + `sed -n`，一次往返
    async fn read_range_remote(
        &self,
        path: &str,
        offset: Option<i64>,
        limit: Option<i64>,
        limits: &Limits,
    ) -> Result<LineRange, String> {
        let start_line = offset.unwrap_or(1).max(1) as usize;
        let max_take = limit
            .map(|l| l.max(0) as usize)
            .unwrap_or(limits.read_max_lines);
        let quoted = shell_quote(path);

        // split('\n') 的段数恒等于换行符数 + 1：用它把 wc 的结果对齐到本地语义
        let command = format!(
            "if [ -f {quoted} ]; then wc -l < {quoted}; else echo MISSING; fi; sed -n '{} , {} p' {quoted} 2>/dev/null | head -c {}",
            start_line,
            start_line.saturating_add(max_take.saturating_sub(1)),
            limits.read_max_bytes
        );

        let out = transport::exec(&self.conn, &command, None, None, 4 * 1024 * 1024)
            .await
            .map_err(|e| format!("read {path}: {e}"))?;
        let mut lines = out.stdout.lines();
        let head = lines.next().unwrap_or("").trim();
        if head == "MISSING" {
            return Err(format!("{path}: no such file"));
        }

        let newlines: usize = head.parse().unwrap_or(0);
        let total_lines = newlines + 1;
        if start_line > total_lines {
            return Ok(LineRange {
                offset_beyond_end: true,
                total_lines,
                ..Default::default()
            });
        }

        let mut selected: Vec<&str> = lines.collect();
        // sed 输出以换行结尾，split 后多一个空段
        if selected.last().is_some_and(|l| l.is_empty()) {
            selected.pop();
        }

        Ok(LineRange {
            selected_lines: selected.len(),
            content: selected.join("\n"),
            total_lines,
            offset_beyond_end: false,
        })
    }

    /// 逐级建父目录后写入（不做 rename 替换，避免破坏符号链接与权限）
    pub async fn write(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        if let Some(parent) = parent_posix(path) {
            self.mkdir_p(&parent).await?;
        }

        let mut file = self
            .session
            .create(path)
            .await
            .map_err(|e| format!("create {path}: {e}"))?;
        file.write_all(bytes)
            .await
            .map_err(|e| format!("write {path}: {e}"))?;
        file.shutdown()
            .await
            .map_err(|e| format!("close {path}: {e}"))
    }

    /// 逐级创建目录，已存在的层忽略
    pub async fn mkdir_p(&self, dir: &str) -> Result<(), String> {
        if dir.is_empty() || dir == "/" {
            return Ok(());
        }

        let mut current = String::new();
        for part in dir.split('/').filter(|p| !p.is_empty()) {
            current.push('/');
            current.push_str(part);
            _ = self.session.create_dir(current.clone()).await;
        }
        Ok(())
    }

    /// 递归列出某个远端目录下的所有普通文件（相对路径），供 push/pull 使用
    pub async fn walk_files(&self, root: &str) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        let mut stack = vec![(root.to_string(), String::new())];

        while let Some((dir, prefix)) = stack.pop() {
            for (name, is_dir) in self.list(&dir).await? {
                if name == "." || name == ".." {
                    continue;
                }

                let rel = if prefix.is_empty() {
                    name.clone()
                } else {
                    format!("{prefix}/{name}")
                };

                if is_dir {
                    stack.push((format!("{dir}/{name}"), rel));
                } else {
                    if out.len() >= MAX_SYNC_FILES {
                        return Err(format!(
                            "more than {MAX_SYNC_FILES} files under {root}; narrow the path"
                        ));
                    }
                    out.push(rel);
                }
            }
        }
        out.sort();
        Ok(out)
    }

    /// 下载一个远端文件到本地（父目录自动创建）；返回字节数
    pub async fn download_to(&self, remote: &str, local: &Path) -> Result<u64, String> {
        let bytes = self.read_all(remote).await?;
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create {}: {e}", parent.display()))?;
        }

        std::fs::write(local, &bytes).map_err(|e| format!("write {}: {e}", local.display()))?;
        Ok(bytes.len() as u64)
    }

    /// 上传一个本地文件到远端（父目录自动创建）；返回字节数
    pub async fn upload_from(&self, local: &Path, remote: &str) -> Result<u64, String> {
        let mut file = tokio::fs::File::open(local)
            .await
            .map_err(|e| format!("open {}: {e}", local.display()))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .await
            .map_err(|e| format!("read {}: {e}", local.display()))?;
        self.write(remote, &bytes).await?;
        Ok(bytes.len() as u64)
    }
}

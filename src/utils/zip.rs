//! 最小 ZIP 归档写入（stored / 不压缩），供 `/bug` 报告导出使用。
//!
//! 仅实现用到的子集：本地文件头 + 中央目录 + EOCD，
//! 条目以 method 0（stored）写入，无需 deflate 依赖。文件名以 UTF-8 标志
//! （general purpose bit 11）标记，中文名可被标准解压工具正确还原。

use crate::error::{Error, Result};
use std::path::Path;

/// 一个待写入的归档条目。
#[derive(Debug, Clone)]
pub struct ZipEntry {
    /// 归档内路径（使用 `/` 分隔，目录项以 `/` 结尾）
    pub name: String,
    /// 条目的原始字节内容，以 stored（不压缩）方式原样写入归档
    pub data: Vec<u8>,
}

impl ZipEntry {
    /// 文本条目（UTF-8）
    pub fn text(name: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            data: data.into().into_bytes(),
        }
    }

    /// 二进制条目
    pub fn bytes(name: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            data,
        }
    }
}

/// 把条目写成 ZIP 归档并落盘。
pub fn write_zip(path: &Path, entries: &[ZipEntry]) -> Result<()> {
    let data = build_zip(entries);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(path, data).map_err(|source| Error::Io {
        context: format!("Failed to write bug report archive: {}", path.display()),
        source,
    })
}

/// 组装 ZIP 字节流（本地头 + 数据 + 中央目录 + EOCD）。
fn build_zip(entries: &[ZipEntry]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::new();
    let mut central: Vec<u8> = Vec::new();

    for entry in entries {
        let name = entry.name.as_bytes();
        let crc = crc32(&entry.data);
        let size = entry.data.len() as u32;
        let offset = out.len() as u32;

        // 本地文件头
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes()); // signature
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 名
        out.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0x0021u16.to_le_bytes()); // mod date (1980-01-01)
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes()); // compressed
        out.extend_from_slice(&size.to_le_bytes()); // uncompressed
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name);
        out.extend_from_slice(&entry.data);

        // 中央目录项
        central.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central.extend_from_slice(&0x0800u16.to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        central.extend_from_slice(&0u16.to_le_bytes()); // mod time
        central.extend_from_slice(&0x0021u16.to_le_bytes()); // mod date
        central.extend_from_slice(&crc.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&size.to_le_bytes());
        central.extend_from_slice(&(name.len() as u16).to_le_bytes());
        central.extend_from_slice(&0u16.to_le_bytes()); // extra len
        central.extend_from_slice(&0u16.to_le_bytes()); // comment len
        central.extend_from_slice(&0u16.to_le_bytes()); // disk number
        central.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central.extend_from_slice(&offset.to_le_bytes());
        central.extend_from_slice(name);
    }

    let central_offset = out.len() as u32;
    let central_size = central.len() as u32;
    out.extend_from_slice(&central);

    // EOCD
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk number
    out.extend_from_slice(&0u16.to_le_bytes()); // disk with central dir
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
    out.extend_from_slice(&central_size.to_le_bytes());
    out.extend_from_slice(&central_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    out
}

/// 标准 CRC-32（IEEE 802.3，反射多项式 0xEDB88320）。
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &byte in data {
        crc ^= byte as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_values() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"hello"), 0x3610_A686);
    }

    #[test]
    fn archive_has_signatures_and_names() {
        let entries = vec![
            ZipEntry::text("report.json", "{}"),
            ZipEntry::text("summary.md", "# hi\n"),
        ];
        let bytes = build_zip(&entries);
        // 本地头 / 中央目录 / EOCD 签名均在
        assert_eq!(&bytes[0..4], &0x0403_4b50u32.to_le_bytes());
        let eocd = bytes.len() - 22;
        assert_eq!(&bytes[eocd..eocd + 4], &0x0605_4b50u32.to_le_bytes());
        assert_eq!(
            u16::from_le_bytes([bytes[eocd + 10], bytes[eocd + 11]]),
            2,
            "两条目录项"
        );
        // 名称与内容原样可寻
        let all = String::from_utf8_lossy(&bytes).to_string();
        assert!(all.contains("report.json"));
        assert!(all.contains("summary.md"));
        assert!(all.contains("# hi"));
    }

    #[test]
    fn zip_is_readable_by_system_unzip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.zip");
        write_zip(
            &path,
            &[
                ZipEntry::text("report.json", "{\"a\":1}\n"),
                ZipEntry::bytes("bin.dat", vec![0, 1, 2, 255]),
            ],
        )
        .unwrap();

        // 无外置 unzip 时跳过（CI 环境不保证）
        if std::process::Command::new("unzip")
            .arg("-v")
            .output()
            .is_err()
        {
            return;
        }
        let out = std::process::Command::new("unzip")
            .arg("-p")
            .arg(&path)
            .arg("report.json")
            .output()
            .unwrap();
        assert!(out.status.success(), "unzip -p failed: {:?}", out);
        assert_eq!(String::from_utf8_lossy(&out.stdout), "{\"a\":1}\n");
    }
}

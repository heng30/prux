//! 工具后端抽象。
//! 默认实现走本地 std::fs；外部可注入自定义实现（远程/容器后端），与无远程执行场景的现状保持接口同构。

use std::io;

/// 全局默认后端（工具函数未显式注入时使用）
pub static DEFAULT_TOOL_OPS: DefaultToolOperations = DefaultToolOperations;

/// 目录项元数据
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolStat {
    /// 该路径是否为目录
    pub is_dir: bool,
    /// 该路径是否为普通文件（符号链接等为 false）
    pub is_file: bool,
    /// 文件字节长度；目录下为平台相关的元数据大小，不代表内容
    pub len: u64,
}

/// 工具文件系统操作的抽象接口（全部同步；bash 的 exec 不在此列）
pub trait ToolOperations: Send + Sync {
    /// 读文件全部字节（read 工具）
    fn read_file(&self, path: &str) -> io::Result<Vec<u8>>;

    /// 访问探测（是否可读；read 工具 resolveReadPath 前用）
    fn access(&self, path: &str) -> io::Result<()>;

    /// 路径是否存在（ls/find）
    fn exists(&self, path: &str) -> bool;

    /// 路径元数据（ls）
    fn stat(&self, path: &str) -> Option<ToolStat>;

    /// 读取目录条目（ls）
    fn read_dir(&self, path: &str) -> io::Result<Vec<String>>;

    /// glob 匹配（find 自定义后端；默认实现做 walk + 通配符匹配）
    fn glob(&self, pattern: &str, base: &str) -> io::Result<Vec<String>>;

    /// 读取目录内容为文本（grep 读文件内容）
    fn read_to_string(&self, path: &str) -> io::Result<String> {
        self.read_file(path).and_then(|b| {
            String::from_utf8(b)
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid utf-8"))
        })
    }

    /// 只读文件开头 `n` 字节（read 工具判图片用）；默认实现整文件读取后截断。
    fn read_head(&self, path: &str, n: usize) -> io::Result<Vec<u8>> {
        let mut head = self.read_file(path)?;
        head.truncate(n);
        Ok(head)
    }

    /// 读文本文件 `[start, end)` 区间的行（0 基，`end` 为 None 表示读到末尾），
    /// 返回 `(选中的行, 文件总行数)`；行内不改动 `\r`，调用方自行清理。
    /// 默认实现整文件读取后切片；`DefaultToolOperations` 覆写成流式读，避免大文件 OOM。
    fn read_lines_range(
        &self,
        path: &str,
        start: usize,
        end: Option<usize>,
    ) -> io::Result<(Vec<String>, usize)> {
        let text = String::from_utf8_lossy(&self.read_file(path)?).to_string();
        let all: Vec<String> = text.split('\n').map(|s| s.to_string()).collect();
        let end = end.unwrap_or(all.len()).min(all.len());
        let selected = if start >= all.len() {
            Vec::new()
        } else {
            all[start..end.max(start)].to_vec()
        };
        Ok((selected, all.len()))
    }
}

/// 本地文件系统默认实现（当前唯一后端）
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultToolOperations;

impl ToolOperations for DefaultToolOperations {
    /// 直接调用 `std::fs::read`，IO 失败（不存在/无权限）返回 Err。
    fn read_file(&self, path: &str) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    /// 用 `metadata` 探测可访问性：路径不存在或无权限时返回 Err。
    fn access(&self, path: &str) -> io::Result<()> {
        std::fs::metadata(path).map(|_| ())
    }

    /// 通过 `metadata` 是否成功判断存在性，不区分“不存在”与“无权限”。
    fn exists(&self, path: &str) -> bool {
        std::fs::metadata(path).is_ok()
    }

    /// `metadata` 失败（不存在/无权限）时返回 None。
    fn stat(&self, path: &str) -> Option<ToolStat> {
        std::fs::metadata(path).ok().map(|m| ToolStat {
            is_dir: m.is_dir(),
            is_file: m.is_file(),
            len: m.len(),
        })
    }

    /// 返回目录下条目的文件名字（不含路径），已排序；读取失败返回 Err。
    fn read_dir(&self, path: &str) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(path)? {
            let e = e?;
            out.push(e.file_name().to_string_lossy().to_string());
        }
        out.sort();
        Ok(out)
    }

    /// 默认后端不支持 glob，恒返回空列表（find 的自定义后端会覆写）。
    // 默认实现：不做递归 glob
    fn glob(&self, _pattern: &str, _base: &str) -> io::Result<Vec<String>> {
        Ok(Vec::new())
    }

    /// 只读文件头 `n` 字节，不把整个文件拉进内存。
    fn read_head(&self, path: &str, n: usize) -> io::Result<Vec<u8>> {
        use std::io::Read;
        let mut buf = vec![0u8; n];
        let mut file = std::fs::File::open(path)?;
        let mut filled = 0usize;
        while filled < n {
            match file.read(&mut buf[filled..])? {
                0 => break,
                read => filled += read,
            }
        }
        buf.truncate(filled);
        Ok(buf)
    }

    /// 流式逐行读，只保留 `[start, end)` 区间的行；总行数按 `split('\n')` 的语义计
    /// （以换行结尾时末尾再算一个空行，空文件算一行空行）。
    fn read_lines_range(
        &self,
        path: &str,
        start: usize,
        end: Option<usize>,
    ) -> io::Result<(Vec<String>, usize)> {
        use std::io::BufRead;
        let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
        let mut lines: Vec<String> = Vec::new();
        let mut buf: Vec<u8> = Vec::new();
        let mut counted = 0usize;
        let mut ended_with_newline = false;
        loop {
            buf.clear();
            if reader.read_until(b'\n', &mut buf)? == 0 {
                break;
            }
            ended_with_newline = buf.ends_with(b"\n");
            if ended_with_newline {
                buf.pop();
            }
            if start <= counted && end.is_none_or(|e| counted < e) {
                // 非 UTF-8 用 U+FFFD 替换（与整文件读取一致）
                lines.push(String::from_utf8_lossy(&buf).to_string());
            }
            counted += 1;
        }
        let trailing_empty = counted == 0 || ended_with_newline;
        let total = counted + usize::from(trailing_empty);
        if trailing_empty && start <= counted && end.is_none_or(|e| counted < e) {
            lines.push(String::new());
        }
        Ok((lines, total))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 流式行区间与「整文件 read_to_string 再 split('\n')」的语义必须完全一致。
    #[test]
    fn read_lines_range_matches_split_semantics() {
        let dir = tempfile::tempdir().unwrap();
        let cases: [(&str, &str); 4] = [
            ("empty.txt", ""),
            ("no_trailing.txt", "a\nb"),
            ("trailing.txt", "a\nb\n"),
            ("crlf.txt", "a\r\nb\r\n"),
        ];
        for (name, content) in cases {
            let path = dir.path().join(name);
            std::fs::File::create(&path)
                .unwrap()
                .write_all(content.as_bytes())
                .unwrap();
            let path = path.to_string_lossy().to_string();
            let all: Vec<String> = content.split('\n').map(|s| s.to_string()).collect();
            for start in 0..=all.len() {
                for end in [None, Some(start), Some(start + 1), Some(all.len())] {
                    let (got, total) = DEFAULT_TOOL_OPS
                        .read_lines_range(&path, start, end)
                        .unwrap();
                    let want: Vec<String> = if start >= all.len() {
                        Vec::new()
                    } else {
                        let e = end.unwrap_or(all.len()).min(all.len());
                        all[start..e.max(start)].to_vec()
                    };
                    assert_eq!(total, all.len(), "{name} 总行数");
                    assert_eq!(got, want, "{name} start={start} end={end:?}");
                }
            }
        }
    }

    #[test]
    fn read_head_only_reads_the_requested_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.txt");
        std::fs::write(&path, "0123456789abcdef").unwrap();
        let path = path.to_string_lossy().to_string();
        assert_eq!(DEFAULT_TOOL_OPS.read_head(&path, 4).unwrap(), b"0123");
        assert_eq!(DEFAULT_TOOL_OPS.read_head(&path, 64).unwrap().len(), 16);
    }
}

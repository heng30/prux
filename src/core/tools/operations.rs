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
}

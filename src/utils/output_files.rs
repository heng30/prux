//! 输出文件：为「模型没有看全的那部分输出」写的临时文件。
//!
//! 包括截断的工具输出全文、codemode 脚本展示的图片等。所有这类文件都经本模块创建，
//! 落盘位置与权限因此只有一个真源（当前落在 OS 临时目录）。
//!
//! 文件只有属主可读（`0o600`），且用独占创建（不跟随别人预置在目标路径上的符号链接）。
//!
//! **文件不随句柄 drop 而删除**：路径会交给模型去 `read`，必须一直有效；
//! 清理交给 OS 的临时目录策略。

use crate::APP_NAME;
use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
};

/// 输出文件只有属主可读（Unix 权限位；Windows 上忽略）。
const OUTPUT_FILE_MODE: u32 = 0o600;

/// 在 OS 临时目录里新建一个独占的输出文件并返回它的句柄。
///
/// 文件名形如 `<APP_NAME>-<prefix>-<随机串><extension>`；`extension` 含点（`.log`、`.png`）。
/// 随机串与独占创建（`O_EXCL`）保证不会撞名，也不会跟随已存在的符号链接；
/// Unix 上以 [`OUTPUT_FILE_MODE`] 创建。
fn create_output_file(prefix: &str, extension: &str) -> io::Result<(File, PathBuf)> {
    let file = tempfile::Builder::new()
        .prefix(&format!("{APP_NAME}-{prefix}-"))
        .suffix(extension)
        .tempfile_in(std::env::temp_dir())?;
    let (file, path) = file.keep()?;
    set_owner_only_permissions(&path);
    Ok((file, path))
}

/// 把已创建的文件权限收紧到属主可读可写。
///
/// `tempfile` 在 Unix 上已按 `0o600` 创建，这里只做一次显式兜底
/// （不改变非 Unix 平台行为）；失败时忽略——权限收不紧不应让整条工具调用失败。
fn set_owner_only_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(OUTPUT_FILE_MODE));
    }

    #[cfg(not(unix))]
    {
        _ = path;
    }
}

/// 把 `data` 一次性写入一个新的输出文件，返回它的路径。
///
/// 出错时返回 IO 错误（磁盘满、临时目录不可写），由调用方决定是否降级
/// （如把原因写进面向模型的提示文本，而不是丢弃已经产生的工具结果）。
pub fn write_output_file(prefix: &str, extension: &str, data: &[u8]) -> io::Result<PathBuf> {
    let (mut file, path) = create_output_file(prefix, extension)?;
    file.write_all(data)?;
    Ok(path)
}

/// 打开一个新的输出文件供**流式**写入，返回可写的句柄与它的路径。
///
/// 用于边产生边落盘的长输出（如 bash 命令的完整输出）：
/// 调用方把路径交给模型，句柄写入完毕后即可丢弃，文件本身保留。
pub fn create_output_file_stream(prefix: &str, extension: &str) -> io::Result<(File, PathBuf)> {
    create_output_file(prefix, extension)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 写入的内容原样可读，且落在 OS 临时目录下。
    #[test]
    fn writes_data_to_a_temp_file() {
        let path = write_output_file("prux-test", ".txt", b"hello output").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello output");
        assert!(
            path.starts_with(std::env::temp_dir()),
            "应在 OS 临时目录下: {path:?}"
        );
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(&format!("{}-prux-test-", crate::APP_NAME)),
            "文件名应带前缀: {path:?}"
        );
        let _ = std::fs::remove_file(&path);
    }

    /// 同一前缀连续两次写入得到不同路径（随机串保证不撞名）。
    #[test]
    fn creates_a_new_path_each_time() {
        let a = write_output_file("prux-test", ".txt", b"a").unwrap();
        let b = write_output_file("prux-test", ".txt", b"b").unwrap();
        assert_ne!(a, b);
        let _ = std::fs::remove_file(&a);
        let _ = std::fs::remove_file(&b);
    }

    /// 流式句柄写完后 drop，文件仍然存在（路径要对模型一直有效）。
    #[test]
    fn streamed_file_survives_the_handle() {
        let path = {
            let (mut file, path) = create_output_file_stream("prux-test", ".log").unwrap();
            file.write_all(b"streamed").unwrap();
            path
        };
        assert_eq!(std::fs::read(&path).unwrap(), b"streamed");
        let _ = std::fs::remove_file(&path);
    }

    /// Unix 上输出文件只有属主可读。
    #[cfg(unix)]
    #[test]
    fn output_file_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let path = write_output_file("prux-test", ".txt", b"secret").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, OUTPUT_FILE_MODE, "权限应为 0o600: {mode:o}");
        let _ = std::fs::remove_file(&path);
    }
}

// 路径工具

use serde_json::Value;
use std::{
    ffi::OsStr,
    io::{BufRead, BufReader},
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

/// 目录项类型：区分普通文件与目录，符号链接会先跟随再判定。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum EntryKind {
    /// 普通文件（含指向文件的符号链接）。
    File,
    /// 目录（含指向目录的符号链接）。
    Dir,
}

/// 解析目录项类型并跟随符号链接（断链跳过）
pub fn entry_kind(path: &Path) -> Option<EntryKind> {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_symlink() => match std::fs::metadata(path) {
            Ok(real) => {
                if real.is_dir() {
                    Some(EntryKind::Dir)
                } else if real.is_file() {
                    Some(EntryKind::File)
                } else {
                    None
                }
            }
            Err(_) => None,
        },
        Ok(md) if md.is_dir() => Some(EntryKind::Dir),
        Ok(md) if md.is_file() => Some(EntryKind::File),
        _ => None,
    }
}

/// 路径 → POSIX 风格字符串（反斜杠统一为 `/`，非 UTF-8 用替换字符）。
#[inline(always)]
pub fn to_posix(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// `target` 相对 `root` 的 POSIX 风格路径；不在 `root` 下时原样返回 `target`。
pub fn relative_posix(root: &Path, target: &Path) -> String {
    match target.strip_prefix(root) {
        Ok(rel) => to_posix(rel),
        Err(_) => to_posix(target),
    }
}

/// 取 POSIX 风格路径的父目录：`/a/b` → `/a`，`/a` → `/`。
///
/// 只做字符串处理，不碰文件系统，因此对远端路径也能用。没有父目录时返回 `None`
/// （`a.txt` 这类相对单名），末尾多余的 `/` 先被剥掉。
pub fn parent_posix(path: &str) -> Option<String> {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rsplit_once('/') {
        Some(("", _)) => Some("/".to_string()),
        Some((parent, _)) => Some(parent.to_string()),
        None => None,
    }
}

/// 拼接 POSIX 风格路径：去掉 `base` 尾部斜杠后接 `rel`；`rel` 为空时原样返回 `base`。
pub fn join_posix(base: &str, rel: &str) -> String {
    if rel.is_empty() {
        return base.to_string();
    }
    format!("{}/{}", base.trim_end_matches('/'), rel)
}

/// 列出 `root` 下所有普通文件，结果已排序（不限层数，符号链接目录会被跟随）。
///
/// 超过 `max_files` 时返回 `Err` 而不是截断：静默截断会让调用方以为「搬完了」，
/// 而实际上只搬了一部分，比直接报错危险得多。读取失败时错误带上是哪个目录。
pub fn walk_files(root: &Path, max_files: usize) -> Result<Vec<PathBuf>, String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries =
            std::fs::read_dir(&dir).map_err(|e| format!("read {}: {e}", dir.display()))?;

        for entry in entries {
            let entry = entry.map_err(|e| format!("read {}: {e}", dir.display()))?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                if out.len() >= max_files {
                    return Err(format!(
                        "more than {max_files} files under {}; narrow the path",
                        root.display()
                    ));
                }
                out.push(path);
            }
        }
    }
    out.sort();
    Ok(out)
}

/// 工作目录的末级目录名（根目录等无末级名时退回原路径）。
///
/// 只做字符串处理，不碰文件系统；结尾多余的 `/` 先被剥掉。
pub fn dir_name(cwd: &str) -> String {
    Path::new(cwd.trim_end_matches('/'))
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| cwd.to_string())
}

/// 取路径的最后一段文件名；`/`、`..` 这类没有文件名的路径返回 `Err`（文案带原路径）。
pub fn file_name_of(path: &Path) -> Result<String, String> {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| format!("{}: no file name", path.display()))
}

/// 展开 ~ 与 ~/ 前缀
pub fn expand_tilde(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if s == "~" {
        return home_dir();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    path
}

/// 把字符串**开头**的 `~`、`~/`、`$HOME`、`${HOME}` 展开为家目录。
///
/// 只认前缀：`~user/x`、`/a/$HOME/b` 这类写法原样返回（中段的 `$HOME` 是 shell 语义，
/// 由真正的 shell 解释）。家目录未知（空）时也原样返回。
/// 家目录取自 [`home_dir`]，因此测试接缝（`HomeGuard`）对它生效。
pub fn expand_home(s: &str) -> String {
    let home = home_dir();
    let home = home.to_string_lossy();
    if home.is_empty() {
        return s.to_string();
    }
    for prefix in ["$HOME", "${HOME}"] {
        if let Some(rest) = s.strip_prefix(prefix) {
            return format!("{home}{rest}");
        }
    }
    if s == "~" {
        return home.to_string();
    }
    if let Some(rest) = s.strip_prefix("~/") {
        return Path::new(home.as_ref())
            .join(rest)
            .to_string_lossy()
            .to_string();
    }
    s.to_string()
}

/// Windows 上把 `HOME` 环境变量统一成 [`dirs::home_dir`]（启动时调用一次）。
///
/// **不管 `HOME` 已有何值都重写**：本仓处处以 `HOME` 为 home 目录的唯一来源
/// （[`home_dir`] → 全局 agent 目录、`~` 展开、主题 / 技能发现、项目信任……），
/// 而各家终端给的值并不一致：PowerShell 根本**不导出** `HOME`（只有 `$HOME` 变量和
/// `%USERPROFILE%`），git bash / MSYS 则导出 `/c/Users/xx` 这类 POSIX 形式。
/// 于是同一台机器“换个 shell 就换一份配置”：agent 目录从 `C:\Users\xx\.prux`
/// 变成 cwd 下的 `.\prux`（缺 HOME 时 [`home_dir`] 回退 `.`），主题 / 会话历史全对不上；
/// 而且项目所在卷未必支持硬链接，sidecar 锁会跟着失败（见 [`crate::utils::file_lock`]）。
/// 统一写 `dirs::home_dir()`（Windows = `SHGetKnownFolderPath(FOLDERID_Profile)`，
/// 即 `%USERPROFILE%`）后，所有终端环境指向同一个目录，也与 pi 的
/// `os.homedir()`（Node 在 Windows 上同样只看 profile 目录、不看 `HOME`）对齐。
///
/// 代价：Windows 上用户显式设置的 `HOME`（如把配置放 D 盘的便携安装）不再生效，
/// 要用 `PRUX_AGENT_DIR` 改 agent 目录。
///
/// 非 Windows 平台无操作；`dirs` 也拿不到目录时保持原状。
/// 调用方必须保证在任何线程诞生之前调用（写在 `main` 第一句），
/// 否则 `set_var` 会与其它线程读 env 并发。
pub fn ensure_home_env() {
    // 非 Windows 的 shell（bash / zsh / …）都导出 HOME，且它就是约定的 home 来源。
    if !cfg!(windows) {
        return;
    }

    let Some(home) = dirs::home_dir().filter(|h| !h.as_os_str().is_empty()) else {
        return;
    };

    // SAFETY: 只由 `main` 在任何线程诞生之前调用一次，此刻无并发 env 访问。
    unsafe { std::env::set_var("HOME", home) };
}

/// HOME 目录（测试接缝：`test_support::HomeGuard`，仅测试构建存在）。
///
/// 也是全局 agent 目录 [`crate::core::settings_manager::agent_dir`] 的基准目录。
pub(crate) fn home_dir() -> PathBuf {
    // 测试接缝：线程本地 HOME override（HomeGuard）优先，避免 set_var 竞态。
    #[cfg(any(test, feature = "test-support"))]
    if let Some(home) = crate::test_support::home_override() {
        return home;
    }

    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".to_string()))
}

/// 与 [`expand_home`] 相反：`$HOME` 前缀缩写为 `~`（长路径省宽度，与 shell 习惯一致）。
/// 只在路径分段边界上缩写（`/home/user2/a` 不会被 home=`/home/user` 命中）。
pub fn shorten_home(s: &str) -> String {
    match std::env::var_os("HOME").and_then(|h| h.into_string().ok()) {
        Some(home) => shorten_home_in(s, &home),
        None => s.to_string(),
    }
}

/// [`shorten_home`] 的测试接缝：把 `home` 前缀缩写为 `~`，仅在路径分段边界命中。
fn shorten_home_in(s: &str, home: &str) -> String {
    if !home.is_empty()
        && let Some(rest) = s.strip_prefix(home)
        && (rest.is_empty() || rest.starts_with('/'))
    {
        return format!("~{}", rest);
    }
    s.to_string()
}

/// 以 cwd 为基准解析参数路径（~、绝对路径、相对路径）
pub fn resolve_path(raw: &str, cwd: &str) -> PathBuf {
    let raw = raw.trim();
    if raw == "~" {
        return home_dir();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return home_dir().join(rest);
    }
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        path
    } else {
        PathBuf::from(cwd).join(path)
    }
}

/// 展开 ~ 前缀的裸路径
pub fn expand_tilde_path(raw: String) -> PathBuf {
    resolve_path(&raw, ".")
}

/// 文件元数据（不存在 → `(None, None)`，因此「删除」也会让缓存失效）。
pub fn file_stamp(path: &Path) -> (Option<SystemTime>, Option<u64>) {
    match std::fs::metadata(path) {
        Ok(m) => (m.modified().ok(), Some(m.len())),
        Err(_) => (None, None),
    }
}

/// 读一个 JSON 配置文件（缺失 / 内容损坏 → `Value::Null`，不报错也不改写用户文件）。
///
/// 配合配置读缓存使用：缓存层只存 `Value`，结构体解析与容错口径由调用方决定。
pub fn read_json_file(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null)
}

/// 把一个 JSON 值写进配置文件：pretty 排版 + 结尾换行，父目录不存在时创建。
///
/// 写入不是原子的（需要强一致性的调用方走 core 的原子写）；序列化失败或写盘失败都
/// 返回错误，调用方据此决定要不要回填配置缓存。
pub fn write_json_file(path: &Path, value: &Value) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(path, format!("{text}\n"))
}

/// 从主题参数中提取主题名（取文件名主干）
pub fn theme_name_from_arg(arg: &str) -> String {
    let path = Path::new(arg);
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(arg)
        .to_string()
}

/// 当前工作目录；读取失败时回退 `.`。
///
/// 测试构建下线程本地 override 优先（[`crate::test_support::CwdGuard`]）：
/// 项目层配置（`.prux/settings.json`）按它定位，避免并行测试靠进程级
/// `set_current_dir` 互踩。
pub fn cwd() -> PathBuf {
    #[cfg(any(test, feature = "test-support"))]
    if let Some(dir) = crate::test_support::cwd_override() {
        return dir;
    }

    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// 纯词法归一化（折叠 `.` / `..`，不碰文件系统，不解析符号链接）。
///
/// 已到根时 `..` 停在根（`/..` == `/`）。
pub fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(Component::RootDir);
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// 把相对路径按 `base` 归一化成绝对路径（纯词法，不碰文件系统）。
///
/// `base` 本身是相对路径时按原样拼接，再由 [`lexical_normalize`] 折叠 `..` / `.`。
/// 不解析符号链接——需要真实路径时另外用 [`canonicalize`]。
pub fn absolutize(path: &str, base: &Path) -> PathBuf {
    let raw = Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        base.join(raw)
    };
    lexical_normalize(&joined)
}

/// 路径是否落在 `cwd` 之外。
///
/// 两侧都先做词法归一化；`cwd` 为空时一律视为外部（调用方走保守分支）。
/// 相等（就在工作目录上）不算外部。
pub fn is_outside(path: &Path, cwd: &Path) -> bool {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let abs = lexical_normalize(&joined);
    let base = lexical_normalize(cwd);
    if base.as_os_str().is_empty() {
        return true;
    }
    !(abs == base || abs.starts_with(&base))
}

/// 规范化 cwd（绝对路径、折叠 . 和 ..；不做 fs realpath）
pub fn normalize_cwd(path: &Path) -> String {
    if let Ok(real) = std::fs::canonicalize(path) {
        return real.to_string_lossy().to_string();
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    lexical_normalize(&absolute).to_string_lossy().to_string()
}

/// 剥掉 Windows 规范化路径的 verbatim 前缀（`\\?\` 与 `\\?\UNC\`）。
///
/// `std::fs::canonicalize` 在 Windows 上返回 verbatim 路径（`\\?\C:\...`、
/// `\\?\UNC\server\share\...`），而 `?` 是**非法文件名字符**：把这种路径当作
/// 目录名/文件名的一部分编码出去（prompt 历史按项目根分桶即如此），
/// `CreateFile` 会直接以 `ERROR_INVALID_NAME` 失败，写入被静默丢掉。
///
/// - `\\?\UNC\server\share\x` → `\\server\share\x`（还原成普通 UNC）
/// - `\\?\C:\x` → `C:\x`（还原成普通盘符路径）
/// - 其余（含非 Windows 路径、本就没有前缀的普通路径）原样返回，故 Linux 行为不变。
///
/// 剥离后同一目录的 verbatim / 普通两种写法编码结果一致，避免同一个桶被拆成两份。
pub fn strip_verbatim_prefix(path: &str) -> String {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }

    if let Some(rest) = path.strip_prefix(r"\\?\") {
        return rest.to_string();
    }
    path.to_string()
}

/// 项目根：从 `cwd` 向上找最近的 `.git`（目录或文件都认——文件是 git worktree / 链接工作树的写法），
/// 找到即返回该层规范路径；一路到根都没有则回退 [`normalize_cwd`]。
///
/// 纯文件系统判定，不起 `git` 子进程、不依赖 PATH。用于把「同一个项目的子目录」
/// 归到同一个桶（prompt 历史按项目根分桶）。
pub fn project_root(cwd: &str) -> String {
    let normalized = normalize_cwd(Path::new(cwd));
    let mut current = PathBuf::from(&normalized);
    loop {
        if current.join(".git").exists() {
            return normalize_cwd(&current);
        }
        if !current.pop() {
            return normalized;
        }
    }
}

/// 归一化 home 路径：空路径视为根 `/`，否则原样返回。
pub fn resolve_home(home_dir: &Path) -> PathBuf {
    if home_dir.as_os_str().is_empty() {
        PathBuf::from("/")
    } else {
        home_dir.to_path_buf()
    }
}

/// 路径相对化：绝对路径 → 相对搜索目录
pub fn relativize_path(result_path: &str, search_path: &Path) -> String {
    let had_trailing = result_path.ends_with('/');
    let rel = if Path::new(result_path).is_absolute() {
        match Path::new(result_path).strip_prefix(search_path) {
            Ok(r) => r.to_string_lossy().to_string(),
            Err(_) => result_path.to_string(),
        }
    } else {
        result_path.to_string()
    };
    let posix = rel.replace('\\', "/");
    if had_trailing && !posix.ends_with('/') {
        format!("{}/", posix)
    } else {
        posix
    }
}

/// fs realpath（解析符号链接）；路径不存在时原样返回入参。
pub fn canonicalize(p: &str) -> String {
    std::fs::canonicalize(p)
        .map(|x| x.to_string_lossy().to_string())
        .unwrap_or_else(|_| p.to_string())
}

/// /import 目标路径：同名已存在时递增编号 `name-1.jsonl`、`name-2.jsonl`…
pub fn uniquify_import_destination(dir: &Path, file_name: &OsStr) -> PathBuf {
    let first = dir.join(file_name);
    if !first.exists() {
        return first;
    }
    let p = Path::new(file_name);
    let stem = p
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| file_name.to_string_lossy().to_string());
    let ext = p
        .extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();
    let mut n = 1;
    loop {
        let candidate = dir.join(format!("{stem}-{n}{ext}"));
        if !candidate.exists() {
            return candidate;
        }
        n += 1;
    }
}

/// 不覆盖复制（O_EXCL 语义）：目标已存在时直接报错，绝不静默覆盖。
pub fn copy_no_overwrite(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut input = std::fs::File::open(src)?;
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    std::io::copy(&mut input, &mut output)?;
    Ok(())
}

/// 自身是否为符号链接（不跟随）；元数据不可读时为 false。
pub fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// 文件修改时间（缺失/不可读视为最旧，仍会尝试加载）。
pub fn mtime_of(path: &Path) -> SystemTime {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// 只读文件首行（header）。会话发现路径上高频调用，不解析 JSON。
pub fn read_first_line(path: &Path) -> Option<String> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    reader.read_line(&mut line).ok()?;
    if line.is_empty() { None } else { Some(line) }
}

/// 归一化目录路径（尾部斜杠/相对段差异不影响比较）。
pub fn normalize_dir(dir: &Path) -> PathBuf {
    std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())
}

/// 在 `$PATH` 中查找可执行文件，返回完整路径；`PATH` 未设置或各目录都不命中时返回 `None`。
///
/// Windows 上自动补 `.exe` 后缀。不检查可执行位，只要求是已存在的文件。
pub fn find_in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    std::env::split_paths(&path)
        .map(|dir| dir.join(&exe))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_home_handles_shell_prefixes_only() {
        let home = PathBuf::from("/home/me");
        let _guard = crate::test_support::HomeGuard::set(home.clone());

        assert_eq!(expand_home("~/work"), "/home/me/work");
        assert_eq!(expand_home("~"), "/home/me");
        assert_eq!(expand_home("$HOME/work"), "/home/me/work");
        assert_eq!(expand_home("${HOME}/work"), "/home/me/work");
        // 只认前缀：`~user` 与中段的 `$HOME` 交给 shell，原样返回。
        assert_eq!(expand_home("~user/work"), "~user/work");
        assert_eq!(expand_home("/a/$HOME/b"), "/a/$HOME/b");
        assert_eq!(expand_home("work"), "work");
    }

    /// 目录名取末级：去掉结尾斜杠，根目录退回原路径。
    #[test]
    fn dir_name_takes_last_component() {
        assert_eq!(dir_name("/data/Code/rust/prux"), "prux");
        assert_eq!(dir_name("/data/Code/rust/prux/"), "prux");
        assert_eq!(dir_name("/"), "/");
    }

    #[test]
    fn join_posix_handles_trailing_slash() {
        assert_eq!(join_posix("/srv/app", "a.rs"), "/srv/app/a.rs");
        assert_eq!(join_posix("/srv/app/", "a.rs"), "/srv/app/a.rs");
        assert_eq!(join_posix("/srv/app", ""), "/srv/app");
    }

    #[test]
    fn parents_are_split_correctly() {
        assert_eq!(parent_posix("/srv/x/a.txt").as_deref(), Some("/srv/x"));
        assert_eq!(parent_posix("/a").as_deref(), Some("/"));
        assert_eq!(parent_posix("a.txt"), None);
    }

    /// 递归列出本地文件：不限层数、结果排序、超上限报错而不是截断。
    #[test]
    fn walk_files_lists_local_files() {
        let dir = std::env::temp_dir().join(format!("prux-walk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "a").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "b").unwrap();
        let files = walk_files(&dir, 100).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|p| p.starts_with(&dir)));
        // 上限为 1 时必须报错，不能只返回第一个文件
        assert!(walk_files(&dir, 1).is_err());
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lexical_normalize_folds_dot_and_dotdot() {
        assert_eq!(
            lexical_normalize(Path::new("/work/proj/./a/../b")),
            PathBuf::from("/work/proj/b")
        );
        assert_eq!(lexical_normalize(Path::new("/..")), PathBuf::from("/"));
    }

    #[test]
    fn absolutize_folds_dotdot() {
        let cwd = Path::new("/work/proj");
        assert_eq!(
            absolutize("src/a.rs", cwd),
            PathBuf::from("/work/proj/src/a.rs")
        );
        assert_eq!(absolutize("../x", cwd), PathBuf::from("/work/x"));
        assert_eq!(absolutize("/a/../b", cwd), PathBuf::from("/b"));
        assert_eq!(absolutize("/..", cwd), PathBuf::from("/"));
    }

    #[test]
    fn outside_detection() {
        let cwd = Path::new("/work/proj");
        assert!(!is_outside(Path::new("/work/proj/src/a.rs"), cwd));
        assert!(is_outside(Path::new("/work/other/a.rs"), cwd));
        assert!(is_outside(Path::new("../sibling"), cwd));
        assert!(!is_outside(Path::new("/work/proj"), cwd));
        assert!(is_outside(Path::new("/any"), Path::new("")));
    }

    /// 非 Windows 平台不得动 `HOME`：那里 HOME 就是约定的 home 来源，
    /// 重写成 `dirs::home_dir()` 会把用户显式设的（如 MSYS 的 `/c/Users/x`）改成别的。
    #[cfg(not(windows))]
    #[test]
    fn ensure_home_env_leaves_home_alone_off_windows() {
        let before = std::env::var_os("HOME");

        ensure_home_env();

        assert_eq!(std::env::var_os("HOME"), before);
    }

    #[test]
    fn project_root_walks_up_to_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        let nested = root.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();

        let expected = normalize_cwd(&root);
        assert_eq!(project_root(nested.to_str().unwrap()), expected);
        // 根自身也命中
        assert_eq!(project_root(root.to_str().unwrap()), expected);
    }

    #[test]
    fn project_root_accepts_git_file_for_worktrees() {
        // 链接工作树的 `.git` 是文件（内容 `gitdir: ...`），也要认。
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("wt");
        let nested = root.join("sub");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(root.join(".git"), "gitdir: /elsewhere/.git/worktrees/wt\n").unwrap();

        assert_eq!(project_root(nested.to_str().unwrap()), normalize_cwd(&root));
    }

    #[test]
    fn project_root_falls_back_to_cwd_outside_repo() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();

        let expected = normalize_cwd(&plain);
        assert_eq!(project_root(plain.to_str().unwrap()), expected);
    }

    #[test]
    fn strip_verbatim_prefix_removes_windows_prefixes_only() {
        // Windows canonicalize 的两种 verbatim 形状
        assert_eq!(
            strip_verbatim_prefix(r"\\?\C:\Users\blue"),
            r"C:\Users\blue"
        );
        assert_eq!(
            strip_verbatim_prefix(r"\\?\UNC\srv\share\proj"),
            r"\\srv\share\proj"
        );
        // 普通路径（含 UNC、Unix 绝对路径）原样返回
        assert_eq!(strip_verbatim_prefix(r"C:\Users\blue"), r"C:\Users\blue");
        assert_eq!(strip_verbatim_prefix(r"\\srv\share"), r"\\srv\share");
        assert_eq!(strip_verbatim_prefix("/home/blue/repo"), "/home/blue/repo");
        // `\\?` 后面不跟分隔符的只是普通名字，不能误剥
        assert_eq!(strip_verbatim_prefix(r"\\?weird"), r"\\?weird");
    }

    #[test]
    fn uniquify_import_destination_honors_extension_and_holes() {
        // 无扩展名、连续编号、中间编号被占时跳到下一个空闲编号。
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        assert_eq!(
            uniquify_import_destination(p, std::ffi::OsStr::new("a.jsonl")),
            p.join("a.jsonl")
        );
        std::fs::write(p.join("a.jsonl"), "1").unwrap();
        std::fs::write(p.join("a-1.jsonl"), "2").unwrap();
        assert_eq!(
            uniquify_import_destination(p, std::ffi::OsStr::new("a.jsonl")),
            p.join("a-2.jsonl")
        );
        std::fs::write(p.join("noext"), "3").unwrap();
        assert_eq!(
            uniquify_import_destination(p, std::ffi::OsStr::new("noext")),
            p.join("noext-1")
        );
    }

    #[test]
    fn shorten_home_only_on_segment_boundary() {
        assert_eq!(
            shorten_home_in("/h/u/skills/a/SKILL.md", "/h/u"),
            "~/skills/a/SKILL.md"
        );
        assert_eq!(shorten_home_in("/h/user2/a.md", "/h/u"), "/h/user2/a.md");
        assert_eq!(shorten_home_in("/h/u", "/h/u"), "~");
        assert_eq!(shorten_home_in("/etc/x", ""), "/etc/x");
    }

    #[test]
    fn copy_no_overwrite_refuses_existing_target() {
        // O_EXCL 语义：目标已存在时返回错误，不回写内容。
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        std::fs::write(&src, "new").unwrap();
        std::fs::write(&dst, "old").unwrap();
        assert!(copy_no_overwrite(&src, &dst).is_err());
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "old");
        copy_no_overwrite(&src, &dir.path().join("free")).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("free")).unwrap(),
            "new"
        );
    }
}

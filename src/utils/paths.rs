// 路径工具

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

/// 把字符串开头的 `~/` 展开为 `$HOME`；其它形式（含裸 `~`）原样返回。
pub fn expand_home(s: &str) -> String {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return Path::new(&home).join(rest).to_string_lossy().to_string();
    }
    s.to_string()
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
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::RootDir => out.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(part) => out.push(part),
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
        }
    }
    out.to_string_lossy().to_string()
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

#[cfg(test)]
mod tests {
    use super::*;

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

//! 技能运行前的环境预检。
//!
//! 一份技能失败起来最难查的样子是：`SKILL.md` 让模型跑 `scripts/run.sh`，
//! 而它要么没执行位、要么 shebang 指向的解释器本机没装——于是模型拿一句
//! `Permission denied` / `command not found` 去猜下一步。预检把这类问题在**跑之前**
//! 变成一句人话。
//!
//! 检查范围刻意窄，只查**确定能判定的**三件事：
//! 1. 技能目录里的脚本（`.sh` / `.bash` / `.py` / `.js`，`*.template.*` 除外）有没有执行位；
//! 2. 脚本 shebang 的解释器在不在 `PATH` 上（`#!/usr/bin/env X` 与绝对路径两种写法）；
//! 3. 脚本是否压根没有 shebang（`./run.sh` 直接执行会失败）。
//!
//! 不做的事：不猜 `SKILL.md` 正文里提到的路径是否有效（散文里出现的路径太容易误报），
//! 也不检查非脚本文本文件。所有检查都是只读的，报告出来的问题**不阻止**技能使用。

use crate::core::skills::Skill;
use std::{
    env,
    path::{Path, PathBuf},
};

/// 需要检查预处理条件的脚本扩展名。
const SCRIPT_EXTS: [&str; 4] = ["sh", "bash", "py", "js"];
/// 目录递归深度上限：技能目录约定是 `scripts/`、`references/` 这一层，再深就不扫了。
const MAX_DEPTH: usize = 3;

/// 对一个已加载技能跑预检，返回面向用户的问题列表（空 = 没有问题）。
///
/// 每条都以脚本的相对路径开头，便于用户直接定位。
pub fn check(skill: &Skill) -> Vec<String> {
    let mut issues = Vec::new();
    for path in scripts(&skill.base_dir) {
        let rel = path
            .strip_prefix(&skill.base_dir)
            .unwrap_or(&path)
            .display()
            .to_string();

        let Ok(bytes) = std::fs::read(&path) else {
            issues.push(format!("{rel}: cannot be read"));
            continue;
        };

        let Some(shebang) = first_line(&bytes)
            .and_then(|line| line.strip_prefix("#!"))
            .map(str::trim)
        else {
            issues.push(format!("{rel}: no shebang (running it directly will fail)"));
            continue;
        };

        if let Some(interp) = interpreter(shebang)
            && !in_path(interp)
        {
            issues.push(format!("{rel}: `{interp}` not found in PATH"));
        }

        if !is_executable(&path) {
            issues.push(format!("{rel}: not executable (chmod +x)"));
        }
    }
    issues
}

/// 对一批技能跑预检，每条问题前置技能名。
pub fn check_all(skills: &[Skill]) -> Vec<String> {
    let mut out = Vec::new();
    for skill in skills {
        out.extend(
            check(skill)
                .into_iter()
                .map(|m| format!("{}: {m}", skill.name)),
        );
    }
    out
}

/// 技能目录下的全部脚本（递归、按路径排序，跳过 `*.template.*`）。
fn scripts(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(root, 0, &mut out);
    out.sort();
    out
}

/// 递归收集脚本；符号链接目录不跟随（避免技能目录里指回去的环）。
fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_dir() {
            walk(&path, depth + 1, out);
            continue;
        }
        if !kind.is_file() || is_template(&path) || !is_script(&path) {
            continue;
        }
        out.push(path);
    }
}

/// 该路径是否是要检查的脚本（按扩展名判）。
pub(super) fn is_script(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SCRIPT_EXTS.iter().any(|s| s.eq_ignore_ascii_case(e)))
        .unwrap_or(false)
}

/// 是否是模板文件（`name.template.sh` 一类）：模板本来就该被复制后再改，不查执行位。
pub(super) fn is_template(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.contains(".template."))
        .unwrap_or(false)
}

/// 文件首行（按 `\n` 切，去掉 `\r`）；不是 UTF-8 时为 `None`。
fn first_line(bytes: &[u8]) -> Option<&str> {
    let end = bytes
        .iter()
        .position(|b| *b == b'\n')
        .unwrap_or(bytes.len());
    std::str::from_utf8(&bytes[..end])
        .ok()
        .map(|s| s.trim_end_matches('\r'))
}

/// 从 shebang 里取解释器名：`/usr/bin/env python3` → `python3`，`/bin/sh -e` → `/bin/sh`。
///
/// `env` 后带 `-S` 或参数时取第一个非选项词。取不到（空 shebang）时 `None`。
fn interpreter(shebang: &str) -> Option<&str> {
    let mut parts = shebang.split_whitespace();
    let first = parts.next()?;
    let name = first.rsplit('/').next().unwrap_or(first);
    if name == "env" {
        return parts.find(|p| !p.starts_with('-'));
    }
    Some(first)
}

/// 该解释器能否在本机找到：带 `/` 的按路径判执行位，否则在 `PATH` 里逐个找。
fn in_path(interp: &str) -> bool {
    if interp.contains('/') {
        return is_executable(Path::new(interp));
    }
    env::var_os("PATH")
        .map(|paths| env::split_paths(&paths).any(|dir| is_executable(&dir.join(interp))))
        .unwrap_or(false)
}

/// 文件存在且带执行位（非 Unix 平台退化成「是文件」）。
fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::skills::Skill;

    /// 造一个技能目录，`files` 是 `(相对路径, 内容, 是否加执行位)`。
    fn skill_with(files: &[(&str, &str, bool)]) -> (tempfile::TempDir, Skill) {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path();
        std::fs::write(root.join("SKILL.md"), "---\nname: t\ndescription: d\n---\n").unwrap();
        for (rel, content, exec) in files {
            let path = root.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, content).unwrap();
            if *exec {
                set_exec(&path);
            }
        }
        let skill = Skill {
            name: "t".to_string(),
            description: "d".to_string(),
            path: root.join("SKILL.md"),
            base_dir: root.to_path_buf(),
            disable_model_invocation: false,
            source: "test".to_string(),
        };
        (dir, skill)
    }

    /// 加执行位（非 Unix 上是空操作，对应平台上 `is_executable` 只看「是文件」）。
    fn set_exec(path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).unwrap();
        }
        #[cfg(not(unix))]
        let _ = path;
    }

    /// 可执行、解释器存在的脚本不报问题。
    #[test]
    fn clean_script_passes() {
        let (_dir, skill) = skill_with(&[("scripts/ok.sh", "#!/bin/sh\necho hi\n", true)]);
        assert_eq!(check(&skill), Vec::<String>::new());
    }

    /// 缺执行位要报，且带上技能内的相对路径。
    #[test]
    fn reports_missing_exec_bit() {
        let (_dir, skill) = skill_with(&[("scripts/run.sh", "#!/bin/sh\necho hi\n", false)]);
        let issues = check(&skill);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("scripts/run.sh"));
        assert!(issues[0].contains("not executable"));
    }

    /// 找不到的解释器要报（`env` 形式与绝对路径形式都认）。
    #[test]
    fn reports_missing_interpreter() {
        let (_dir, skill) = skill_with(&[
            ("a.sh", "#!/usr/bin/env prux-no-such-interp\n", true),
            ("b.sh", "#!/usr/bin/prux-no-such-interp\n", true),
        ]);
        let issues = check(&skill);
        assert_eq!(issues.len(), 2, "{issues:?}");
        assert!(
            issues
                .iter()
                .any(|m| m.contains("a.sh") && m.contains("prux-no-such-interp"))
        );
        assert!(issues.iter().any(|m| m.contains("b.sh")));
    }

    /// 没有 shebang、模板文件、非脚本文件都不算问题（模板按约定不是直接执行的）。
    #[test]
    fn ignores_templates_and_non_scripts() {
        let (_dir, skill) = skill_with(&[
            ("scripts/tpl.template.sh", "no shebang here\n", false),
            ("notes.md", "plain text\n", false),
        ]);
        assert_eq!(check(&skill), Vec::<String>::new());
    }

    /// 没有 shebang 的脚本要报（`./run.sh` 会失败）。
    #[test]
    fn reports_missing_shebang() {
        let (_dir, skill) = skill_with(&[("run.sh", "echo hi\n", true)]);
        let issues = check(&skill);
        assert_eq!(issues.len(), 1, "{issues:?}");
        assert!(issues[0].contains("no shebang"));
    }

    /// `env -S` 形式的 shebang 取到真正的解释器。
    #[test]
    fn parses_env_options() {
        assert_eq!(interpreter("/usr/bin/env python3"), Some("python3"));
        assert_eq!(interpreter("/usr/bin/env -S python3 -u"), Some("python3"));
        assert_eq!(interpreter("/bin/sh -e"), Some("/bin/sh"));
        assert_eq!(interpreter(""), None);
    }
}

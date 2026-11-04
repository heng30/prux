//! 从本次改动面挑出相关检查（`diffScoped`）。
//!
//! **问题**：`autoRun` 一开，每次结算都跑全部检查——改一行 README 也要 `cargo test`。
//!
//! 这里用一条确定性规则摘掉不相关的检查（不碰模型，同一份 diff 永远同一结果）：
//!
//! 1. `git status --porcelain` 拿改动路径；不是 git 仓库 / git 不可用 → 不缩减；
//! 2. 路径 → 面（rust / node / python / go）；纯文档、图片这类改动不产生任何面；
//! 3. 检查的命令首词 → 面；认不出的命令（`make check`、`./scripts/check.sh`）**永远保留**；
//! 4. 依赖清单与构建入口（`Cargo.toml`、`package.json`、各种 lock 文件）改动 → 全部保留。
//!
//! 手动路径（`/vet run`）不经过这里：用户明确要跑，就是跑全部。

use super::config::Check;
use crate::utils::process::HideConsole;
use std::{collections::HashSet, path::Path, process::Command};
use strum_macros::{EnumString, IntoStaticStr};

/// 一个项目面：一类源文件对应的一组检查。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase")]
pub(super) enum Face {
    /// Rust 源与构建脚本。
    Rust,
    /// JS / TS 及其生态的样式与模板文件。
    Node,
    /// Python 源。
    Python,
    /// Go 源。
    Go,
}

/// 依赖清单与构建入口：改了它们，任何一个面的检查都可能受影响，故不缩减。
const GLOBAL_FILES: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "package.json",
    "package-lock.json",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "pyproject.toml",
    "requirements.txt",
    "go.mod",
    "go.sum",
    "Makefile",
    "makefile",
    "GNUmakefile",
    "justfile",
    "Justfile",
];

/// 命令首词 → 面。表里没有的首词视为「认不出」。
const COMMAND_FACES: &[(&str, Face)] = &[
    ("cargo", Face::Rust),
    ("rustc", Face::Rust),
    ("clippy-driver", Face::Rust),
    ("npm", Face::Node),
    ("pnpm", Face::Node),
    ("yarn", Face::Node),
    ("bun", Face::Node),
    ("deno", Face::Node),
    ("npx", Face::Node),
    ("node", Face::Node),
    ("tsc", Face::Node),
    ("eslint", Face::Node),
    ("prettier", Face::Node),
    ("vitest", Face::Node),
    ("jest", Face::Node),
    ("pytest", Face::Python),
    ("python", Face::Python),
    ("python3", Face::Python),
    ("ruff", Face::Python),
    ("mypy", Face::Python),
    ("go", Face::Go),
];

/// 路径 → 面；不认识的后缀返回 `None`（文档、图片、配置都不影响任何面）。
fn face_of_path(path: &str) -> Option<Face> {
    let ext = Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "rs" => Some(Face::Rust),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "vue" | "svelte" | "css" | "scss"
        | "html" => Some(Face::Node),
        "py" => Some(Face::Python),
        "go" => Some(Face::Go),
        _ => None,
    }
}

/// 改动路径集合 → 命中的面。
///
/// 返回 `None` 意味着「别缩减」——只要有一个全局影响文件（依赖清单、构建入口）被改，
/// 或者改动里的文件不属于任何面（无法判断影响），就退回全量检查。
fn faces_of(changed: &[String]) -> Option<HashSet<Face>> {
    if changed.is_empty() {
        return None;
    }
    let mut faces = HashSet::new();
    for path in changed {
        let name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(path);
        if GLOBAL_FILES.contains(&name) {
            return None;
        }
        if let Some(face) = face_of_path(path) {
            faces.insert(face);
        }
    }
    // 改动里一个源文件都没有（只改了文档/资源）：空集是有效结论——已知面的检查全跳过。
    Some(faces)
}

/// 检查的命令 → 面。认不出的命令返回 `None`，调用方据此保留该检查。
///
/// 复合命令（含 `&&`、`;`、`|`）一律认不出：其中的面无法用一个标签概括，保守全跑。
fn face_of_command(command: &str) -> Option<Face> {
    let command = command.trim();
    if command.contains("&&")
        || command.contains("||")
        || command.contains(';')
        || command.contains('|')
    {
        return None;
    }
    let head = command.split_whitespace().next()?;
    let head = head.rsplit('/').next().unwrap_or(head);
    COMMAND_FACES
        .iter()
        .find(|(name, _)| *name == head)
        .map(|(_, face)| *face)
}

/// 跑 `git status --porcelain` 拿改动路径（含未跟踪文件）；失败或非仓库返回 `None`。
///
/// 输出落在管道里，绝不继承终端——prux 是 TUI 程序（见仓库「外部命令的输出必须有去处」）。
fn git_changed_paths(cwd: &Path) -> Option<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .hide_console()
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(
        text.lines()
            .filter(|line| line.len() > 3)
            // 格式：`XY <path>`；重命名是 `R  old -> new`，取新路径。
            .map(|line| {
                let path = &line[3..];
                path.rsplit_once(" -> ")
                    .map(|(_, new)| new)
                    .unwrap_or(path)
                    .trim()
                    .trim_matches('"')
                    .to_string()
            })
            .collect(),
    )
}

/// 按本次改动面缩减检查列表。`enabled` 关时、或判定不出改动面时，原样返回全部检查。
pub(super) fn narrow(cwd: &Path, checks: &[Check], enabled: bool) -> Vec<Check> {
    if !enabled {
        return checks.to_vec();
    }
    let Some(faces) = git_changed_paths(cwd).as_deref().and_then(faces_of) else {
        return checks.to_vec();
    };

    checks
        .iter()
        .filter(|check| match face_of_command(&check.command) {
            // 认不出的命令必须跑：跳过它就是假的通过。
            None => true,
            Some(face) => faces.contains(&face),
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一条检查。
    fn check(name: &str, command: &str) -> Check {
        Check {
            name: name.to_string(),
            command: command.to_string(),
            optional: false,
        }
    }

    fn paths(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rust_change_keeps_only_rust_checks() {
        let faces = faces_of(&paths(&["src/main.rs"])).expect("认出面");
        assert!(faces.contains(&Face::Rust));
        assert!(!faces.contains(&Face::Node));
    }

    #[test]
    fn docs_only_change_yields_an_empty_face_set() {
        let faces = faces_of(&paths(&["README.md", "assets/logo.png"])).expect("空集是有效结论");
        assert!(faces.is_empty());
    }

    #[test]
    fn manifest_change_disables_narrowing() {
        assert!(faces_of(&paths(&["Cargo.toml"])).is_none());
        assert!(faces_of(&paths(&["src/lib.rs", "package.json"])).is_none());
    }

    #[test]
    fn no_paths_disables_narrowing() {
        assert!(faces_of(&[]).is_none());
    }

    #[test]
    fn command_head_maps_to_a_face_and_keeps_unknowns() {
        assert_eq!(face_of_command("cargo test"), Some(Face::Rust));
        assert_eq!(face_of_command("  pnpm run lint "), Some(Face::Node));
        assert_eq!(
            face_of_command("./node_modules/.bin/tsc -p ."),
            Some(Face::Node)
        );
        assert_eq!(face_of_command("pytest -q"), Some(Face::Python));
        assert_eq!(face_of_command("just check"), None, "认不出的命令必须全跑");
        assert_eq!(
            face_of_command("cargo test && pnpm test"),
            None,
            "复合命令全跑"
        );
    }

    #[test]
    fn narrow_drops_checks_for_untouched_faces_and_keeps_unknown_commands() {
        let checks = [
            check("test", "cargo test"),
            check("lint", "pnpm run lint"),
            check("script", "just check"),
        ];
        let faces: HashSet<Face> = [Face::Rust].into_iter().collect();
        let kept: Vec<String> = checks
            .iter()
            .filter(|c| match face_of_command(&c.command) {
                None => true,
                Some(face) => faces.contains(&face),
            })
            .map(|c| c.name.clone())
            .collect();
        assert_eq!(kept, vec!["test", "script"]);
    }

    #[test]
    fn disabled_selector_returns_everything() {
        let checks = vec![check("test", "cargo test"), check("lint", "pnpm run lint")];
        assert_eq!(narrow(Path::new("."), &checks, false).len(), 2);
    }

    #[test]
    fn non_git_directory_returns_everything() {
        let dir = std::env::temp_dir().join(format!("prux-vet-selector-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let checks = vec![check("test", "cargo test")];
        assert_eq!(
            narrow(&dir, &checks, true).len(),
            1,
            "不是 git 仓时不能缩减检查"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

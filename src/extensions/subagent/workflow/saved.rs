//! 具名工作流：把 `SubagentWorkflow({ name })` 解析成磁盘上的脚本。
//!
//! 具名工作流就是普通的 `.js` 文件，内容与 `script` 参数一模一样；这里**不解析**它——
//! `meta` 提取仍在调用点跑，所以具名文件与内联源码的失败方式一致。
//!
//! 根沿用**两层**约定：
//!
//! 1. `<cwd>/PROJECT_SCOPE_NAME/workflows/<name>.js` —— 项目级，**受项目信任门控**；
//! 2. `<agent_dir()>/extensions/workflows/<name>.js` —— 全局
//!
//! **first-hit-wins**（项目优先）：一个名字只对应一个文件，没有"覆盖"可言。
//!
//! ## 目录里的 `.js` 不等于工作流
//!
//! 这两个是普通目录，可能放着构建产物或随手写的脚本。只有带 `export const meta =` 声明的文件才算工作流。
//! 检查是**纯正则、从不执行**（[`super::meta::has_meta_declaration`]）。
//! 这样列出来的东西就是真能跑的：把 `utils.js` 当成可运行工作流展示出去，等于请模型去踩。
//!
//! ## 名字先校验再拼路径
//!
//! `name` 来自模型，`../../etc/passwd` 不能变成一个可读的工作流。校验照（`^[a-zA-Z0-9][a-zA-Z0-9._-]*$`、≤128，
//! 与 agent 名共用 [`crate::extensions::subagent::memory::is_unsafe_name`]），并且**显式拒绝符号链接**：
//! agent 发现是枚举目录，而这里是"模型给的名字 → 拼路径"，正是不该跟着链接走的那一步。

use super::meta;
use crate::{
    PROJECT_SCOPE_NAME,
    core::{project_trust::is_project_trusted, settings_manager::agent_dir},
    extensions::subagent::memory::is_unsafe_name,
    utils::paths::is_symlink,
};
use std::path::PathBuf;

/// 工作流目录名（两层根的末段）。
pub(crate) const WORKFLOW_DIR: &str = "workflows";

/// 全局根所在的扩展数据子目录（`agent_dir()/extensions/`）。
const EXTENSIONS_SUBDIR: &str = "extensions";

/// 具名工作流的扩展名。
const SCRIPT_EXT: &str = "js";

/// 名字的查找根，**优先级从高到低**（项目受信任门控）。
pub(crate) fn roots(cwd: &str) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let agent_dir = agent_dir();
    let cwd_path = PathBuf::from(cwd);
    let project = cwd_path.join(PROJECT_SCOPE_NAME).join(WORKFLOW_DIR);

    if project.is_dir() && is_project_trusted(&cwd_path, &agent_dir) {
        roots.push(project);
    }

    roots.push(agent_dir.join(EXTENSIONS_SUBDIR).join(WORKFLOW_DIR));
    roots
}

/// 读到的具名工作流。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Saved {
    /// 脚本文本。
    pub script: String,
    /// 命中的文件路径。
    pub path: PathBuf,
}

/// 读一个具名工作流。
///
/// 失败时带上**搜过的根**与**实际存在的名字**：猜错名字的模型可以据此自己纠正，而不是花一整轮来问。
pub(crate) fn read(name: &str, cwd: &str) -> Result<Saved, String> {
    let name = name.trim();
    if is_unsafe_name(name) {
        return Err(format!(
            "\"{name}\" is not a usable workflow name. Use letters, digits, dots, hyphens and \
             underscores only — a path is what `scriptPath` is for."
        ));
    }

    let mut searched: Vec<String> = Vec::new();
    for root in roots(cwd) {
        if is_symlink(&root) {
            continue;
        }

        let path = root.join(format!("{name}.{SCRIPT_EXT}"));
        if !path.is_file() {
            searched.push(root.display().to_string());
            continue;
        }

        if is_symlink(&path) {
            return Err(format!(
                "saved workflow {} is a symlink; refusing to follow it. Copy the file in, or pass \
                 scriptPath explicitly.",
                path.display()
            ));
        }

        let script = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read saved workflow {}: {e}", path.display()))?;

        if !meta::has_meta_declaration(&script) {
            return Err(format!(
                "{} is not a workflow: it has no `export const meta = {{ name, description }}` \
                 declaration.",
                path.display()
            ));
        }
        return Ok(Saved { script, path });
    }

    let known = list(cwd);
    Err(format!(
        "no saved workflow named \"{name}\". Searched: {}.{}",
        searched.join(", "),
        if known.is_empty() {
            String::new()
        } else {
            format!(" Saved workflows: {}.", known.join(", "))
        }
    ))
}

/// 列出两层根里**看着像工作流**的名字（项目优先去重，按名排序）。
pub(crate) fn list(cwd: &str) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for root in roots(cwd) {
        if is_symlink(&root) {
            continue;
        }

        let Ok(read) = std::fs::read_dir(&root) else {
            continue;
        };

        let mut files: Vec<PathBuf> = read
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.is_file()
                    && !is_symlink(p)
                    && p.extension().and_then(|e| e.to_str()) == Some(SCRIPT_EXT)
            })
            .collect();

        files.sort();

        for path in files {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };

            if is_unsafe_name(stem) || names.iter().any(|n| n == stem) {
                continue;
            }

            if std::fs::read_to_string(&path)
                .map(|s| meta::has_meta_declaration(&s))
                .unwrap_or(false)
            {
                names.push(stem.to_string());
            }
        }
    }

    names.sort();
    names.dedup();
    names
}

/// 一次调用的脚本来源（`script` / `scriptPath` / `name` 三者合流）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Resolved {
    /// 脚本文本（内联或读文件）。
    pub script: String,
    /// 来源文件路径（`scriptPath` 或 `name` 命中时）。它会跟着运行记下来，
    /// 所以"编辑它再用 `scriptPath` 跑一遍"对具名工作流同样成立。
    pub path: Option<PathBuf>,
}

/// 解析脚本来源：**`scriptPath` > `script` > `name`**。
pub(crate) fn resolve(
    script: Option<&str>,
    script_path: Option<&str>,
    name: Option<&str>,
    cwd: &str,
) -> Result<Resolved, String> {
    if let Some(path) = script_path.map(str::trim).filter(|p| !p.is_empty()) {
        let script = std::fs::read_to_string(path)
            .map_err(|e| format!("SubagentWorkflow: cannot read {path}: {e}"))?;

        return Ok(Resolved {
            script,
            path: Some(PathBuf::from(path)),
        });
    }

    if let Some(script) = script.filter(|s| !s.trim().is_empty()) {
        return Ok(Resolved {
            script: script.to_string(),
            path: None,
        });
    }

    if let Some(name) = name.map(str::trim).filter(|n| !n.is_empty()) {
        let saved = read(name, cwd)?;
        return Ok(Resolved {
            script: saved.script,
            path: Some(saved.path),
        });
    }

    let known = list(cwd);
    Err(format!(
        "SubagentWorkflow: provide `script` (inline source), `scriptPath` (a file to read), or \
         `name` (a saved workflow). `scriptPath` takes precedence, then `script`, then `name`.{}",
        if known.is_empty() {
            String::new()
        } else {
            format!(" Saved workflows: {}.", known.join(", "))
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::AgentDirGuard;
    use std::path::Path;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, text).unwrap();
    }

    /// 测试脚本的 `meta` 前缀。
    const META: &str = "export const meta = { name: 'a', description: 'd' }\nreturn 1\n";

    #[test]
    fn name_validation_matches_upstream() {
        assert!(!is_unsafe_name("my-workflow"));
        assert!(!is_unsafe_name("v1.2_x"));
        assert!(is_unsafe_name(""));
        assert!(is_unsafe_name("../etc/passwd"));
        assert!(is_unsafe_name(".hidden"));
        assert!(is_unsafe_name("a/b"));
        assert!(is_unsafe_name(&"x".repeat(129)));
    }

    #[test]
    fn saved_workflow_resolves_project_first_then_global() {
        let _ad = AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_str = cwd.path().to_string_lossy().to_string();
        let project = cwd.path().join(".prux").join("workflows");
        let global = crate::core::settings_manager::agent_dir()
            .join("extensions")
            .join("workflows");
        // 项目层的门控：先记下信任，否则项目根不参与查找
        crate::core::project_trust::set_project_trust(
            cwd.path(),
            &crate::core::settings_manager::agent_dir(),
            true,
        );
        write(&project.join("both.js"), META);
        write(
            &global.join("both.js"),
            "export const meta = { name: 'g', description: 'd' }\n",
        );
        write(&global.join("only-global.js"), META);
        // 不是工作流（无 meta 声明）的文件不算
        write(&global.join("utils.js"), "module.exports = 1\n");

        let saved = read("both", &cwd_str).unwrap();
        assert_eq!(saved.path, project.join("both.js"), "项目层优先");
        assert_eq!(
            read("only-global", &cwd_str).unwrap().path,
            global.join("only-global.js")
        );

        let names = list(&cwd_str);
        assert!(names.contains(&"both".to_string()), "{names:?}");
        assert!(names.contains(&"only-global".to_string()), "{names:?}");
        assert!(
            !names.contains(&"utils".to_string()),
            "无 meta 声明的不算: {names:?}"
        );

        // 未知名字：错误里带上搜过的根与已存在的名字
        let err = read("nope", &cwd_str).unwrap_err();
        assert!(err.contains("no saved workflow named \"nope\""), "{err}");
        assert!(err.contains("both"), "应列出已存在的名字: {err}");

        // 有 meta 声明的目录里放着无声明的文件：报"不是工作流"而不是解析错
        let err = read("utils", &cwd_str).unwrap_err();
        assert!(err.contains("is not a workflow"), "{err}");
    }

    #[test]
    fn project_root_is_trust_gated() {
        let _ad = AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_str = cwd.path().to_string_lossy().to_string();
        let project = cwd.path().join(".prux").join("workflows");
        write(&project.join("proj.js"), META);
        // 缺省信任策略（Ask）下未记录 = 不信任 → 项目根不参与
        assert!(
            read("proj", &cwd_str).is_err(),
            "未受信任的项目根不该被读到"
        );
    }

    #[test]
    fn session_trust_enables_project_workflows_without_writing_trust_json() {
        // 「Trust (this session only)」：不落盘，但本次运行必须能读到项目工作流。
        // 子代理工具跑在 worker 线程，会话信任因此必须是进程级的（见 core::project_trust）。
        let _ad = AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_str = cwd.path().to_string_lossy().to_string();
        let file = cwd.path().join(".prux").join("workflows").join("proj.js");
        write(&file, META);

        assert!(read("proj", &cwd_str).is_err(), "未信任前项目根不参与");

        crate::core::project_trust::set_session_trust(cwd.path(), true);
        assert_eq!(
            read("proj", &cwd_str).unwrap().path,
            file,
            "仅本次信任应让项目工作流可读"
        );
        assert!(
            crate::core::project_trust::trust_decision(
                cwd.path(),
                &crate::core::settings_manager::agent_dir()
            )
            .is_none(),
            "仅本次信任不得写 trust.json"
        );
        crate::core::project_trust::clear_session_trust(cwd.path());
    }

    #[test]
    fn resolve_prefers_script_path_over_script_over_name() {
        let _ad = AgentDirGuard::temp();
        let cwd = tempfile::tempdir().unwrap();
        let cwd_str = cwd.path().to_string_lossy().to_string();
        let file = cwd.path().join("inline.js");
        write(
            &file,
            "export const meta = { name: 'f', description: 'd' }\n",
        );
        write(
            &crate::core::settings_manager::agent_dir()
                .join("extensions")
                .join("workflows")
                .join("saved.js"),
            META,
        );

        // scriptPath 赢过 script（上游描述：Takes precedence over `script`）
        let r = resolve(
            Some("export const meta = { name: 'inline', description: 'd' }\n"),
            Some(file.to_str().unwrap()),
            Some("saved"),
            &cwd_str,
        )
        .unwrap();
        assert_eq!(
            r.script,
            "export const meta = { name: 'f', description: 'd' }\n"
        );
        assert_eq!(r.path.as_deref(), Some(file.as_path()));

        // script 赢过 name
        let r = resolve(Some(META), None, Some("saved"), &cwd_str).unwrap();
        assert_eq!(r.script, META);
        assert!(r.path.is_none());

        // name 命中时把文件路径带上（"编辑它再跑"对具名工作流同样成立）
        let r = resolve(None, None, Some("saved"), &cwd_str).unwrap();
        assert_eq!(r.script, META);
        assert!(r.path.unwrap().ends_with("extensions/workflows/saved.js"));

        // 三者都没有 → 可读错误
        let err = resolve(None, None, None, &cwd_str).unwrap_err();
        assert!(err.contains("scriptPath` takes precedence"), "{err}");
    }
}

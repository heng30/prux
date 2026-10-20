//! 记忆存储的路径与读写：全局一份、每个工作目录一份。
//!
//! 目录布局（`agent_dir()/extensions/memory/`）：
//!
//! ```text
//! memory/
//!   global.json               全局记忆（跨项目）
//!   projects/<project-key>.json   项目记忆（键与会话目录同名：--Users-me-repo--）
//! ```
//!
//! 项目键复用 [`crate::extensions::util::project_key`]，因此同一个仓库的记忆文件与它的
//! 会话目录一眼能对上。没有工作目录时项目存储不可用（而不是悄悄写进全局）。

use super::graph::{MemoryEntry, MemoryGraph, MemoryScope, load_graph, save_graph};
use crate::{core::settings_manager::agent_dir, extensions::util::project_key};
use std::{
    io,
    path::{Path, PathBuf},
};

/// 记忆存储的根目录 `agent_dir()/extensions/memory/`。
pub(super) fn memory_dir() -> PathBuf {
    agent_dir().join("extensions").join("memory")
}

/// 全局记忆文件。
pub(super) fn global_path() -> PathBuf {
    memory_dir().join("global.json")
}

/// 指定工作目录的项目记忆文件。
pub(super) fn project_path(cwd: &str) -> PathBuf {
    memory_dir()
        .join("projects")
        .join(format!("{}.json", project_key(cwd)))
}

/// 一份已定位的存储（路径 + 内容）。
#[derive(Debug, Clone)]
pub(super) struct Located {
    /// 文件路径。
    pub(super) path: PathBuf,
    /// 文件内容。
    pub(super) graph: MemoryGraph,
}

impl Located {
    /// 把内容写回自己的路径。
    pub(super) fn save(&self) -> io::Result<()> {
        save_graph(&self.path, &self.graph)
    }
}

/// 读一个作用域命中的存储；`scope` 里不含的那一份返回 `None`。
#[derive(Debug, Clone)]
pub(super) struct Scoped {
    /// 项目存储（`scope` 不含项目或无工作目录时为 `None`）。
    pub(super) project: Option<Located>,
    /// 全局存储（`scope` 不含全局时为 `None`）。
    pub(super) global: Option<Located>,
}

impl Scoped {
    /// 取某个作用域那份存储的可变图；`All` 或该份缺失时为 `None`。
    pub(super) fn graph_mut(&mut self, scope: MemoryScope) -> Option<&mut MemoryGraph> {
        match scope {
            MemoryScope::Project => self.project.as_mut().map(|located| &mut located.graph),
            MemoryScope::Global => self.global.as_mut().map(|located| &mut located.graph),
            MemoryScope::All => None,
        }
    }

    /// 把两份存储写回磁盘（缺失的那份跳过）；任一失败即返回错误。
    pub(super) fn save(&self) -> Result<(), String> {
        if let Some(project) = &self.project {
            project.save().map_err(|err| format!("{err}"))?;
        }
        if let Some(global) = &self.global {
            global.save().map_err(|err| format!("{err}"))?;
        }
        Ok(())
    }

    /// 两份存储里的有效记忆。
    pub(super) fn active_entries(&self) -> Vec<(MemoryScope, &MemoryEntry)> {
        let mut out = Vec::new();
        if let Some(project) = &self.project {
            out.extend(
                project
                    .graph
                    .active_memories()
                    .map(|entry| (MemoryScope::Project, entry)),
            );
        }
        if let Some(global) = &self.global {
            out.extend(
                global
                    .graph
                    .active_memories()
                    .map(|entry| (MemoryScope::Global, entry)),
            );
        }
        out
    }
}

/// 按作用域读存储。项目作用域需要 `cwd`，缺失时返回错误（不静默丢写）。
pub(super) fn read_scoped(scope: MemoryScope, cwd: Option<&str>) -> Result<Scoped, String> {
    let project = if scope.includes_project() {
        let cwd = cwd.filter(|dir| !dir.trim().is_empty()).ok_or_else(|| {
            "project scope needs a working directory; pass `scope: \"global\"` explicitly"
                .to_string()
        })?;
        let path = project_path(cwd);
        Some(load_at(&path)?)
    } else {
        None
    };

    let global = if scope.includes_global() {
        Some(load_at(&global_path())?)
    } else {
        None
    };

    Ok(Scoped { project, global })
}

/// 读单个存储；文件缺失得到空图。
pub(super) fn load_at(path: &Path) -> Result<Located, String> {
    let graph = load_graph(path).map_err(|err| format!("{err}"))?;
    Ok(Located {
        path: path.to_path_buf(),
        graph,
    })
}

/// 读全局存储。
pub(super) fn read_global() -> Result<Located, String> {
    load_at(&global_path())
}

/// 读（或惰性创建）指定工作目录的项目存储。
pub(super) fn read_project(cwd: &str) -> Result<Located, String> {
    if cwd.trim().is_empty() {
        return Err(
            "project scope needs a working directory; pass `scope: \"global\"` explicitly"
                .to_string(),
        );
    }
    load_at(&project_path(cwd))
}

#[cfg(test)]
mod tests {
    use super::super::graph::MemoryCategory;
    use super::*;
    use crate::test_support::AgentDirGuard;

    #[test]
    fn project_path_follows_the_session_directory_encoding() {
        let path = project_path("/Users/me/work/repo");
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert_eq!(name, "--Users-me-work-repo--.json");
        assert!(path.starts_with(memory_dir().join("projects")));
        assert_eq!(global_path(), memory_dir().join("global.json"));
    }

    #[test]
    fn project_scope_without_cwd_fails_instead_of_writing_global() {
        let _guard = AgentDirGuard::temp();
        let err = read_scoped(MemoryScope::Project, None).unwrap_err();
        assert!(err.contains("working directory"), "{err}");
        let err = read_scoped(MemoryScope::All, Some("  ")).unwrap_err();
        assert!(err.contains("working directory"), "{err}");
        // 只读全局时不要求 cwd
        assert!(read_scoped(MemoryScope::Global, None).is_ok());
    }

    #[test]
    fn scoped_reads_keep_the_two_stores_apart() {
        let _guard = AgentDirGuard::temp();
        let mut project = read_project("/tmp/project-a").unwrap();
        project
            .graph
            .add_memory(MemoryEntry::new(MemoryCategory::Fact, "project note"));
        project.save().unwrap();

        let mut global = read_global().unwrap();
        global
            .graph
            .add_memory(MemoryEntry::new(MemoryCategory::Fact, "global note"));
        global.save().unwrap();

        let scoped = read_scoped(MemoryScope::Project, Some("/tmp/project-a")).unwrap();
        assert!(scoped.global.is_none(), "项目作用域不读全局");
        assert_eq!(scoped.active_entries().len(), 1);

        let scoped = read_scoped(MemoryScope::Global, Some("/tmp/project-a")).unwrap();
        assert!(scoped.project.is_none(), "全局作用域不读项目");
        assert_eq!(scoped.active_entries().len(), 1);

        let scoped = read_scoped(MemoryScope::All, Some("/tmp/project-a")).unwrap();
        assert_eq!(scoped.active_entries().len(), 2);

        // 另一个工作目录看不到第一个项目的记忆
        let other = read_scoped(MemoryScope::Project, Some("/tmp/project-b")).unwrap();
        assert!(other.active_entries().is_empty());
    }
}

//! `doc-sync` 模块：把编译期内嵌的「中文文档」同步进 `agent_dir()/docs`。
//!
//! - 系统提示词（[`crate::core::system_prompt`]）把 `agent_dir()/docs` 作为官方文档目录，
//!   agent 据此提问配置当前项目，因此启动时必须保证该目录是最新的；
//! - 文档强制最新：目标不存在 → 写入；内容与内嵌不一致（含被外部修改）→ 覆写；
//!   内容一致 → 跳过；清单外的用户文件 → 保留不动。被删除的文档 → 重建。
//! - 全程无状态（不依赖 manifest），天然幂等、可自愈；失败仅静默记录，不阻断启动。

use crate::{core::settings_manager::agent_dir, embedded};
use std::path::PathBuf;

/// 文档清单：(文件名, 内嵌内容)。
const DOCS: &[(&str, &str)] = &[
    ("index.md", embedded!("docs/index.md")),
    ("quickstart.md", embedded!("docs/quickstart.md")),
    ("settings.md", embedded!("docs/settings.md")),
    ("keybindings.md", embedded!("docs/keybindings.md")),
    ("models.md", embedded!("docs/models.md")),
    (
        "environment-variables.md",
        embedded!("docs/environment-variables.md"),
    ),
    ("http-proxy.md", embedded!("docs/http-proxy.md")),
    ("themes.md", embedded!("docs/themes.md")),
    ("skills.md", embedded!("docs/skills.md")),
    ("prompt-templates.md", embedded!("docs/prompt-templates.md")),
    ("tui.md", embedded!("docs/tui.md")),
    ("extensions.md", embedded!("docs/extensions.md")),
    ("virtual-models.md", embedded!("docs/virtual-models.md")),
    (
        "extensions/subagent.md",
        embedded!("docs/extensions/subagent.md"),
    ),
    (
        "extensions/codemode.md",
        embedded!("docs/extensions/codemode.md"),
    ),
    ("extensions/mcp.md", embedded!("docs/extensions/mcp.md")),
    ("custom-provider.md", embedded!("docs/custom-provider.md")),
    ("sessions.md", embedded!("docs/sessions.md")),
    ("compaction.md", embedded!("docs/compaction.md")),
    ("sdk.md", embedded!("docs/sdk.md")),
];

/// 文档目标目录：`agent_dir()/docs`（与系统提示词 `docs_path()` 一致）。
pub fn docs_dir() -> PathBuf {
    agent_dir().join("docs")
}

/// 同步内嵌文档到 `docs_dir()`，保证最新。
/// 对每个内嵌文档：目标不存在或内容不一致 → 覆写；内容一致 → 跳过。
/// 清单外的用户文件保留不动。
/// 返回 `(写入数, 跳过数)`。
pub fn sync_docs() -> (usize, usize) {
    let mut written = 0;
    let mut skipped = 0;
    let dir = docs_dir();
    _ = std::fs::create_dir_all(&dir);

    for (file, content) in DOCS {
        let path = dir.join(file);

        // 清单里可以带子目录（如 `extensions/subagent.md`）：先建父目录，否则写入会静默失败。
        if let Some(parent) = path.parent() {
            _ = std::fs::create_dir_all(parent);
        }

        match std::fs::read_to_string(&path) {
            Ok(existing) if existing == *content => skipped += 1,
            _ => {
                if std::fs::write(&path, content).is_ok() {
                    written += 1;
                }
            }
        }
    }
    (written, skipped)
}

/// 启动时保证 `docs_dir()` 就绪。
/// 磁盘写量极小（若干小型 md），同步执行即可；首个 agent turn 前完成，
/// 保证系统提示词指向的文档目录最新。失败仅记录，不抛出、不阻断启动。
pub fn ensure_docs() {
    let (written, _) = sync_docs();
    if written > 0 {
        eprintln!(
            "doc-sync: wrote {written} bundled doc(s) into {}",
            docs_dir().display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每测试独立临时 agent_dir（线程本地 override），锁/共享目录历史做法已废弃。
    fn setup() -> (crate::test_support::AgentDirGuard, std::path::PathBuf) {
        let ad = crate::test_support::AgentDirGuard::temp();
        let docs = crate::core::settings_manager::agent_dir().join("docs");
        let _ = std::fs::remove_dir_all(&docs);
        (ad, docs)
    }

    #[test]
    fn writes_all_and_is_idempotent() {
        let (_g, docs) = setup();

        let (written, skipped) = sync_docs();
        assert_eq!(written, DOCS.len());
        assert_eq!(skipped, 0);

        for (file, content) in DOCS {
            assert_eq!(
                std::fs::read_to_string(docs.join(file)).unwrap(),
                *content,
                "{}",
                file
            );
        }

        // 幂等：内容一致 → 二次同步全部跳过
        let (written2, skipped2) = sync_docs();
        assert_eq!(written2, 0);
        assert_eq!(skipped2, DOCS.len());
    }

    #[test]
    fn overwrites_modified_file_and_recreates_deleted() {
        let (_g, docs) = setup();
        sync_docs();

        // 外部修改其中一个文档 → 应被覆写回内嵌内容
        let keybindings = docs.join("keybindings.md");
        std::fs::write(&keybindings, "外部篡改内容").unwrap();
        let (written, _) = sync_docs();
        assert!(written >= 1);
        assert_eq!(
            std::fs::read_to_string(&keybindings).unwrap(),
            *DOCS.iter().find(|(f, _)| *f == "keybindings.md").unwrap().1,
        );

        // 删除一个文档 → 应被重建
        let models = docs.join("models.md");
        std::fs::remove_file(&models).unwrap();
        sync_docs();
        assert!(models.exists());
    }

    #[test]
    fn preserves_files_not_in_manifest() {
        let (_g, docs) = setup();
        sync_docs();

        // 清单外的用户文件 → 保留不动
        let extra = docs.join("user-notes.md");
        std::fs::write(&extra, "# 用户自己的笔记").unwrap();
        sync_docs();
        assert_eq!(std::fs::read_to_string(&extra).unwrap(), "# 用户自己的笔记");
    }
}

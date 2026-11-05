//! 最近工作目录清单（`agent_dir()/recent-cwds.json`）：TUI 与 GUI 的 `/cwd` 面板共用。
//!
//! 清单独立于 `settings.json`，读写失败都当空列表处理——它只是补全候选，不值得报错。

use crate::core::settings_manager;
use std::{path::Path, path::PathBuf};

/// 清单最多保留几条（最新的在前）。
pub const RECENT_CWD_LIMIT: usize = 30;

/// 最近工作目录清单文件路径。
pub fn recent_cwds_path() -> PathBuf {
    settings_manager::agent_dir().join("recent-cwds.json")
}

/// 读清单（最新的在前；文件缺失或损坏都返回空列表）。
pub fn recent_cwds() -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(recent_cwds_path()) else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .map(|list| list.into_iter().take(RECENT_CWD_LIMIT).collect())
        .unwrap_or_default()
}

/// 记一个工作目录：去重提到最前，超过上限截断。
pub fn remember_cwd(path: &str) {
    let mut list = recent_cwds();
    list.retain(|item| item != path);
    list.insert(0, path.to_string());
    list.truncate(RECENT_CWD_LIMIT);
    write_recent_cwds(&list);
}

/// 写回清单（父目录不存在时先建）。
pub fn write_recent_cwds(list: &[String]) {
    if let Some(dir) = recent_cwds_path().parent() {
        _ = std::fs::create_dir_all(dir);
    }

    if let Ok(text) = serde_json::to_string_pretty(list) {
        _ = std::fs::write(recent_cwds_path(), text);
    }
}

/// 启动时清理：清单里已经不存在的目录移除并写回，免得面板里留下点不开的条目。
pub fn prune_recent_cwds() {
    let list = recent_cwds();
    let live: Vec<String> = list
        .iter()
        .filter(|path| Path::new(path).is_dir())
        .cloned()
        .collect();

    if live.len() != list.len() {
        write_recent_cwds(&live);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 去重提到最前，超过上限截断。
    #[test]
    fn remember_cwd_dedupes_and_caps() {
        let _guard = crate::test_support::AgentDirGuard::temp();
        for index in 0..(RECENT_CWD_LIMIT + 3) {
            remember_cwd(&format!("/work/{index}"));
        }
        remember_cwd("/work/5");
        let list = recent_cwds();
        assert_eq!(list.len(), RECENT_CWD_LIMIT);
        assert_eq!(list[0], "/work/5");
        assert_eq!(list.iter().filter(|item| *item == "/work/5").count(), 1);
    }

    /// 启动清理：清单里已经不存在的目录会被移除并写回，存在的保留。
    #[test]
    fn prune_recent_cwds_drops_missing_dirs() {
        let _guard = crate::test_support::AgentDirGuard::temp();
        let dir = std::env::temp_dir().join(format!("prux-cwd-prune-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let live = dir.to_string_lossy().to_string();
        let gone = dir.join("gone").to_string_lossy().to_string();

        write_recent_cwds(&[live.clone(), gone]);
        prune_recent_cwds();
        assert_eq!(recent_cwds(), vec![live]);
    }
}

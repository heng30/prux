//! `extra-themes` 扩展：把编译期嵌入的「额外主题」同步进 `agent_dir()/themes`。
//!
//! - 内置 `dark` / `light` 由 [`Theme::load`] 经 `embedded!` 直接回退，无需落盘，
//!   本扩展只管理 `assets/themes` 中除 `dark.json` / `light.json` 之外的额外主题；
//! - 启用（或 `/reload`、启动注册时为启用态）时按「内容幂等」写入：目标不存在→写入；
//!   已存在且内容与嵌入一致→跳过；已存在但内容不同（外部定制）→跳过不覆盖；
//! - 禁用时只删除「内容与嵌入完全一致」的文件；被外部修改过的文件保留。
//!
//! 这样既不覆盖、也不误删用户在 `agent_dir()/themes` 里外部创建/定制的同名主题，
//! 且全程无状态（不依赖任何 manifest），天然幂等、可自愈。

use crate::{
    core::{
        extensions::{Extension, ExtensionHook, ExtensionMode, ExtensionTool},
        settings_manager::agent_dir,
    },
    embedded,
    extensions::{EXTENSION_FACTORIES, ExtensionFactory, PRIORITY_EXTRA_THEMES},
};
use std::{path::PathBuf, sync::Arc};

/// 额外主题清单：(文件名, 嵌入内容)。
/// 文件名用 `<name>.json`，与 [`Theme::load`] 的 `themes/<name>.json` 查找约定一致。
const EXTRA_THEMES: &[(&str, &str)] = &[
    (
        "catppuccin-mocha.json",
        embedded!("themes/catppuccin-mocha.json"),
    ),
    ("cyberpunk.json", embedded!("themes/cyberpunk.json")),
    ("dracula.json", embedded!("themes/dracula.json")),
    ("everforest.json", embedded!("themes/everforest.json")),
    ("gruvbox.json", embedded!("themes/gruvbox.json")),
    (
        "midnight-ocean.json",
        embedded!("themes/midnight-ocean.json"),
    ),
    ("nord.json", embedded!("themes/nord.json")),
    ("ocean-breeze.json", embedded!("themes/ocean-breeze.json")),
    ("rose-pine.json", embedded!("themes/rose-pine.json")),
    ("synthwave.json", embedded!("themes/synthwave.json")),
    ("tokyo-night.json", embedded!("themes/tokyo-night.json")),
];

/// 自声明工厂：linkme 分布式切片，priority 值大者先注册（见 [`crate::extensions`]）。
#[linkme::distributed_slice(EXTENSION_FACTORIES)]
static EXTRA_THEMES_FACTORY: ExtensionFactory = ExtensionFactory {
    priority: PRIORITY_EXTRA_THEMES,
    make: || Arc::new(ExtraThemes::new()),
};

/// `extra-themes` 扩展（无工具、无钩子；纯资源同步）。
#[derive(Default)]
pub struct ExtraThemes;

impl ExtraThemes {
    /// 构造扩展实例（无内部状态）。
    pub fn new() -> Self {
        Self
    }
}

impl Extension for ExtraThemes {
    /// 返回固定扩展名 `extra-themes`。
    fn name(&self) -> &str {
        "extra-themes"
    }

    /// 返回 `/extension` 面板展示的一句话描述。
    fn description(&self) -> &str {
        "Sync bundled extra themes (assets/themes excluding dark/light) into agent_dir/themes"
    }

    /// 仅在 Minimal 模式下可用。
    fn modes(&self) -> Vec<ExtensionMode> {
        vec![ExtensionMode::Minimal]
    }

    /// 不提供任何工具。
    fn tools(&self) -> Vec<ExtensionTool> {
        Vec::new()
    }

    /// 不订阅任何事件钩子（纯资源同步）。
    fn hooks(&self) -> Vec<ExtensionHook> {
        Vec::new()
    }

    /// 启用时把额外主题写入 `themes` 目录，禁用时移除未被外部修改的副本。
    fn on_enabled_changed(&self, enabled: bool) {
        if enabled {
            sync_extra_themes();
        } else {
            remove_extra_themes();
        }
    }
}

/// `agent_dir()/themes` 目录路径。
fn themes_dir() -> PathBuf {
    agent_dir().join("themes")
}

/// 启用：把额外主题按内容幂等写入 `themes` 目录。
/// 返回 `(写入数, 跳过数)`。目标不存在才写入；存在（无论是否一致）均跳过。
fn sync_extra_themes() -> (usize, usize) {
    let dir = themes_dir();
    _ = std::fs::create_dir_all(&dir);
    let mut written = 0;
    let mut skipped = 0;

    for (file, content) in EXTRA_THEMES {
        let path = dir.join(file);
        match std::fs::read_to_string(&path) {
            Ok(_) => skipped += 1, // 已存在：外部创建或上次已写入，均不覆盖
            Err(_) => {
                if std::fs::write(&path, content).is_ok() {
                    written += 1;
                }
            }
        }
    }
    (written, skipped)
}

/// 禁用：删除与嵌入内容完全一致的文件。
/// 返回 `(删除数, 保留数)`。不存在或已被外部修改的文件保留（不误删外部定制）。
fn remove_extra_themes() -> (usize, usize) {
    let dir = themes_dir();
    let mut removed = 0;
    let mut kept = 0;

    for (file, content) in EXTRA_THEMES {
        let path = dir.join(file);
        match std::fs::read_to_string(&path) {
            Ok(existing) if existing == *content => {
                if std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
            _ => kept += 1, // 不存在或已被外部修改 → 不动
        }
    }
    (removed, kept)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁 + 共享目录守卫：Drop 时把 PRUX_AGENT_DIR 归还共享测试目录，
    /// 避免后续测试（test_agent 等依赖 prux-auth-tests）读到本测试的残留 env。
    ///
    /// 不用私有目录：不拿锁的 UI 测试（pasted/key 导航等）会调用 test_agent()
    /// 把进程级 PRUX_AGENT_DIR 改回 prux-auth-tests，并行时私有目录会被
    /// env 劫持导致 sync/remove 落到错误目录；共享目录下两者自洽。
    /// 全局 EXTRA_THEMES 是“被测对象本身”（enable/disable 互相影响）→ 必须持锁串行；
    /// agent 目录用线程本地 guard 每测试独立（注册表测试只断言自己写的东西）。
    fn setup() -> (crate::test_support::AgentDirGuard, std::path::PathBuf) {
        let _lock = crate::test_support::AUTH_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let ad = crate::test_support::AgentDirGuard::temp();
        let dir = crate::core::settings_manager::agent_dir();
        let themes = dir.join("themes");
        let _ = std::fs::create_dir_all(&themes);
        for (file, _) in EXTRA_THEMES {
            let _ = std::fs::remove_file(themes.join(file));
        }
        (ad, dir)
    }

    #[test]
    fn enable_writes_and_disable_removes_only_matching() {
        let (_g, dir) = setup();

        // 外部创建的主题：不该被覆盖，也不该被删
        let external = dir.join("themes").join("external.json");
        std::fs::create_dir_all(external.parent().unwrap()).unwrap();
        std::fs::write(&external, r#"{"name":"external"}"#).unwrap();

        let (written, skipped) = sync_extra_themes();
        assert_eq!(written, EXTRA_THEMES.len());
        assert_eq!(skipped, 0);

        // 全部额外主题已写入且内容一致
        for (file, content) in EXTRA_THEMES {
            let path = dir.join("themes").join(file);
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                *content,
                "{}",
                file
            );
        }
        // 外部文件未被碰
        assert_eq!(
            std::fs::read_to_string(&external).unwrap(),
            r#"{"name":"external"}"#
        );

        // 禁用：只删内容一致的文件，外部文件保留（清单外文件本就不计入 kept 计数）
        let (removed, kept) = remove_extra_themes();
        assert_eq!(removed, EXTRA_THEMES.len());
        assert_eq!(kept, 0);
        for (file, _) in EXTRA_THEMES {
            let path = dir.join("themes").join(file);
            assert!(!path.exists(), "{} should be removed", file);
        }
        assert!(external.exists(), "external theme must be kept");
    }

    #[test]
    fn skips_externally_modified_file_on_both_directions() {
        let (_g, dir) = setup();

        // 预先放一个与嵌入内容不同的 dracula.json（外部定制）
        let dracula = dir.join("themes").join("dracula.json");
        std::fs::create_dir_all(dracula.parent().unwrap()).unwrap();
        std::fs::write(
            &dracula,
            r##"{"name":"dracula","vars":{"accent":"#ffffff"}}"##,
        )
        .unwrap();

        // 启用：存在但内容不同 → 跳过不覆盖
        let (written, _) = sync_extra_themes();
        // 除 dracula 外其余 10 个写入
        assert_eq!(written, EXTRA_THEMES.len() - 1);
        assert_eq!(
            std::fs::read_to_string(&dracula).unwrap(),
            r##"{"name":"dracula","vars":{"accent":"#ffffff"}}"##,
            "external modification must be preserved"
        );

        // 禁用：内容不一致 → 保留
        let (_, kept) = remove_extra_themes();
        assert!(dracula.exists(), "modified file must be kept");
        assert!(kept >= 1);
    }

    #[test]
    fn sync_is_idempotent() {
        let (_g, _dir) = setup();

        sync_extra_themes();
        let (written2, skipped2) = sync_extra_themes();
        assert_eq!(written2, 0, "second sync should write nothing");
        assert_eq!(skipped2, EXTRA_THEMES.len());
    }

    #[test]
    fn register_toggle_and_reload_drive_files() {
        let (_g, dir) = setup();
        crate::core::settings_manager::write_disabled_extensions(&[]).ok();

        // 注册（默认启用）→ 初始同步写入全部额外主题
        crate::core::extensions::register_extension(ExtraThemes::new());
        for (file, content) in EXTRA_THEMES {
            assert_eq!(
                std::fs::read_to_string(dir.join("themes").join(file)).unwrap(),
                *content,
                "{}",
                file
            );
        }

        // /extension 面板 toggle 禁用 → on_enabled_changed(false) 移除
        assert!(crate::core::extensions::set_extension_enabled(
            "extra-themes",
            false
        ));
        for (file, _) in EXTRA_THEMES {
            assert!(!dir.join("themes").join(file).exists(), "{}", file);
        }

        // toggle 启用 → 重新写入
        assert!(crate::core::extensions::set_extension_enabled(
            "extra-themes",
            true
        ));
        assert!(dir.join("themes").join("nord.json").exists());

        // /reload：settings.json 标记禁用 → reload_from_settings 恢复后移除
        crate::core::settings_manager::write_disabled_extensions(&["extra-themes".to_string()])
            .ok();
        crate::core::extensions::reload_from_settings();
        for (file, _) in EXTRA_THEMES {
            assert!(!dir.join("themes").join(file).exists(), "reload {}", file);
        }
    }
}

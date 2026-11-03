//! `/debug settings-key`：`settings.json` 未知顶层键诊断。
//!
//! 数据源是 [`settings_manager::settings_key_issues`]——扫描全局（`agent_dir()`）与项目级
//! （`<cwd>/.prux/`）两份 `settings.json` 的顶层键，报出不认识的键、它在原文里的行号，
//! 以及最接近的已知键。JSON 本身语法错误不在这里报（由启动时的 settings 载入警告负责）。
//!
//! 每次执行都重新读盘：`settings.json` 是用户手改的文件，缓存只会给出过期结论。

use crate::{
    core::{
        extensions::{CommandHost, UiNotifyLevel},
        settings_manager,
    },
    utils::glyphs::DEF_FAILED,
};

/// `/debug settings-key`：渲染两份 `settings.json` 的未知键诊断。
///
/// 全部认识时给一行确认；否则每个未知键一行（`<path>:<line> "<key>" is not a known setting`）。
pub(super) fn command(host: &mut dyn CommandHost, _rest: &str) -> bool {
    let issues = settings_manager::settings_key_issues();
    if issues.is_empty() {
        host.push_message(
            &format!(
                "Settings keys: no unknown keys in {}/settings.json or .prux/settings.json.",
                settings_manager::agent_dir().display()
            ),
            UiNotifyLevel::Info,
        );
        return false;
    }

    let mut text = format!("Settings keys: {} unknown", issues.len());
    for issue in &issues {
        text.push('\n');
        text.push_str(DEF_FAILED);
        text.push(' ');
        text.push_str(&issue.describe());
    }
    host.push_message(&text, UiNotifyLevel::Warning);
    false
}

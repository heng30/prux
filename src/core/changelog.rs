//! 变更日志（changelog）：编译期内嵌当前版本的更新日志。
//!
//! 约定：每个版本一个 Markdown 文件，放在 `assets/changelog/v.<version>.md`，其中
//! `<version>` 与 `Cargo.toml` 的 `version`（[`VERSION`]）一致。
//!
//! 只内嵌当前版本：路径用 `CARGO_PKG_VERSION` 在编译期拼接。因此发版时
//! 若忘记补齐对应版本的 changelog，`include_str!` 找不到文件会直接编译失败，
//! 从机制上保证 `/changelog` 永远有内容可展示。内容为 Markdown 正文。

/// 当前版本号（即 `Cargo.toml` 的 `version`）。
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 当前版本的内嵌更新日志（Markdown 正文）。
///
/// 路径由 `CARGO_PKG_VERSION` 决定，只包含当前版本，不随运行时变化。
pub const CHANGELOG: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/assets/changelog/v.",
    env!("CARGO_PKG_VERSION"),
    ".md",
));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_non_empty_changelog_for_current_version() {
        // 版本绑定由 include_str! 的编译期路径（`v.<version>.md`）保证：发版漏补文件即编译失败。
        // 正文不再要求出现版本号：展示时标题已输出「What's New in v<version>?」（cmd_changelog），
        // 正文重复版本号是冗余（见 assets/changelog/v.1.0.0.md）。
        assert!(!CHANGELOG.trim().is_empty(), "当前版本 changelog 不应为空");
    }
}

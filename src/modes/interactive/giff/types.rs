//! `/giff` 审阅视图的基础枚举。

/// 当前获得键盘焦点的窗格（`Tab` / `h` / `l` 切换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pane {
    /// 左侧文件列表：`j`/`k` 移动选中文件。
    FileList,
    /// 右侧 diff 内容：`j`/`k` 滚动当前文件。
    DiffContent,
}

/// diff 内容的排布方式（`u` 切换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// 左右两栏对照：左 `-` 与上下文、右 `+` 与上下文，按改动块对齐。
    SideBySide,
    /// 单栏统一视图：同一改动块里先列删除行、再列新增行。
    Unified,
}

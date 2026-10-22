//! 无障碍通道：读窗口的控件树并在控件上执行动作。
//!
//! 平台各有一套语义（AT-SPI2 / UI Automation / AXUIElement），这里只保留三样共同能力：
//! **读一棵带 ref 的树**、**按 ref 执行一个动作**、**按 ref 读/写值**。工具层（`computer_ui`、
//! `computer_ui_action`）只认这一层，不认任何平台的接口名。

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
use linux as imp;
#[cfg(target_os = "macos")]
use macos as imp;
#[cfg(target_os = "windows")]
use windows as imp;

use crate::extensions::computer::state::Rect;
use std::sync::{Mutex, OnceLock};

/// 无障碍节点的稳定身份：真正能重新找到它的坐标（平台各自解释）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeId {
    /// 平台侧的"地址"第一段（AT-SPI 的总线名；Windows 的 hwnd 字符串；macOS 的 pid）。
    pub bus: String,
    /// 平台侧的"地址"第二段（AT-SPI 的对象路径；Windows 的运行时 id；macOS 的路径串）。
    pub path: String,
    /// 读到时节点的可访问名，用于确认"还是同一个东西"。
    pub name: String,
    /// 读到时节点的角色名。
    pub role: String,
}

/// 读树时的预算与过滤选项。
#[derive(Debug, Clone)]
pub struct A11yOptions {
    /// 最大深度（根为 0）。
    pub max_depth: usize,
    /// 最多返回多少个节点。
    pub max_nodes: usize,
    /// 最多访问多少个节点（含被预算丢弃的）。
    pub max_visited: usize,
    /// 是否探测每个节点支持的动作（关掉更快）。
    pub patterns: bool,
    /// 是否丢掉既无名、无 id、无动作、无矩形的容器。
    pub skip_empty: bool,
}

impl Default for A11yOptions {
    fn default() -> Self {
        Self {
            max_depth: 5,
            max_nodes: 250,
            max_visited: 4000,
            patterns: true,
            skip_empty: true,
        }
    }
}

/// 树里的一行。
#[derive(Debug, Clone)]
pub struct A11yNode {
    /// 在本快照里的序号（ref 的后半段）。
    pub index: usize,
    /// 深度（根为 0）。
    pub depth: usize,
    /// 角色名（`push button` / `entry` / `Window`）。
    pub role: String,
    /// 可访问名（按钮文字、输入框标签）。
    pub name: String,
    /// 描述文本。
    pub description: String,
    /// 平台侧的稳定 id（AT-SPI 的 AccessibleId，UIA 的 AutomationId）。
    pub automation_id: String,
    /// 状态名（`enabled` / `focused` / `checked` …）。
    pub states: Vec<String>,
    /// 屏幕矩形。
    pub rect: Option<Rect>,
    /// 支持的动作名。
    pub actions: Vec<String>,
    /// 实现的接口名。
    pub interfaces: Vec<String>,
    /// 是否还有未展开的子节点。
    pub has_children: bool,
}

/// 一份读树结果在内存里的身份表（ref → [`NodeId`]）。
#[derive(Debug, Clone)]
pub struct A11ySnapshot {
    /// 快照序号（ref 里的 `s<N>.`）。
    pub seq: u64,
    /// ref 前缀，如 `s1.`。
    pub prefix: String,
    /// 每个序号对应的节点身份。
    pub nodes: Vec<NodeId>,
}

/// 快照保留的上限：ref 只在一个观察周期内有意义。
const SNAPSHOT_KEEP: usize = 8;

/// 最近几次读树的快照（ref 前缀 → 节点身份）。
fn snapshots() -> &'static Mutex<Vec<A11ySnapshot>> {
    static SNAPSHOTS: OnceLock<Mutex<Vec<A11ySnapshot>>> = OnceLock::new();
    SNAPSHOTS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 安装一份快照并返回 ref 前缀（`s3.` 这样的形式）。
pub fn install_snapshot(nodes: Vec<NodeId>) -> String {
    let mut list = snapshots().lock().unwrap_or_else(|e| e.into_inner());
    let seq = list.last().map(|s| s.seq + 1).unwrap_or(1);
    let prefix = format!("s{seq}.");
    list.push(A11ySnapshot {
        seq,
        prefix: prefix.clone(),
        nodes,
    });
    while list.len() > SNAPSHOT_KEEP {
        list.remove(0);
    }
    prefix
}

/// 按 ref（`s3.12`）取节点身份。
pub fn resolve_ref(reference: &str) -> Result<NodeId, String> {
    let (prefix, index) = reference.rsplit_once('.').ok_or_else(|| {
        format!("{reference:?} is not a ref; refs look like s1.4 (from computer_ui)")
    })?;
    let index: usize = index.parse().map_err(|_| {
        format!("{reference:?} is not a ref; refs look like s1.4 (from computer_ui)")
    })?;
    let list = snapshots().lock().unwrap_or_else(|e| e.into_inner());
    let snapshot = list
        .iter()
        .find(|s| s.prefix == format!("{prefix}."))
        .ok_or_else(|| {
            format!("{reference} is from an older read; call computer_ui again and use a fresh ref")
        })?;
    snapshot
        .nodes
        .get(index)
        .cloned()
        .ok_or_else(|| format!("{reference} is not in that snapshot"))
}

/// 无障碍通道是否可用（总线在、平台实现有）。
pub fn available() -> bool {
    imp::available()
}

/// 读一棵树。
pub async fn read_tree(
    window: Option<u64>,
    options: A11yOptions,
) -> Result<(Vec<A11yNode>, Vec<NodeId>), String> {
    imp::read_tree(window, options).await
}

/// 执行第 `index` 个动作。
pub async fn do_action(target: &NodeId, index: u32) -> Result<(), String> {
    imp::do_action(target, index).await
}

/// 按名字执行动作（`expand` / `collapse` / `toggle` …）。
pub async fn action_by_name(target: &NodeId, wanted: &str) -> Result<(), String> {
    imp::action_by_name(target, wanted).await
}

/// 读文本内容。
pub async fn read_text(target: &NodeId) -> Result<String, String> {
    imp::read_text(target).await
}

/// 写文本内容。
pub async fn set_text(target: &NodeId, text: &str) -> Result<(), String> {
    imp::set_text(target, text).await
}

/// 读控件值。
pub async fn read_value(target: &NodeId) -> Result<String, String> {
    imp::read_value(target).await
}

/// 给控件焦点。
pub async fn grab_focus(target: &NodeId) -> Result<(), String> {
    imp::grab_focus(target).await
}

/// 选中一段文本。
pub async fn select_text(target: &NodeId, start: i32, end: i32) -> Result<(), String> {
    imp::select_text(target, start, end).await
}

/// 选中一个子项。
pub async fn select_child(target: &NodeId, index: i32) -> Result<(), String> {
    imp::select_child(target, index).await
}

/// 读光标偏移；平台不支持时返回 `None`。
pub async fn caret_offset(target: &NodeId) -> Option<i32> {
    imp::caret_offset(target).await
}

/// 读节点在屏幕上的矩形。
pub async fn node_rect(target: &NodeId) -> Option<Rect> {
    imp::node_rect(target).await
}

/// 把树渲染成紧凑的文本行（`ref role "name" [states] {actions}`）。
pub fn render(nodes: &[A11yNode], prefix: &str, compact: bool) -> String {
    let mut lines = Vec::new();
    for node in nodes {
        let indent = "  ".repeat(node.depth);
        let mut line = format!(
            "{indent}[{prefix}{}] {}",
            node.index,
            if node.role.is_empty() {
                "?"
            } else {
                &node.role
            }
        );
        if !node.name.is_empty() {
            line.push_str(&format!(" {:?}", node.name));
        }
        if !node.automation_id.is_empty() {
            line.push_str(&format!(" #{}", node.automation_id));
        }
        if !node.states.is_empty() {
            line.push_str(&format!(" [{}]", node.states.join(",")));
        }
        if !node.actions.is_empty() {
            line.push_str(&format!(" {{{}}}", node.actions.join(",")));
        }
        if !compact && let Some(rect) = node.rect {
            line.push_str(&format!(
                " @{},{} {}x{}",
                rect.x, rect.y, rect.width, rect.height
            ));
        }
        lines.push(line);
    }
    lines.join("\n")
}

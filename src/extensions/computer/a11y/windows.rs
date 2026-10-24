//! Windows 无障碍通道：UI Automation（COM）。
//!
//! 每个节点用 `(hwnd, 索引链)` 标识：`hwnd` 是窗口句柄，索引链是"从窗口根元素开始，每一步取
//! 第几个子元素"（`0.2.1`）。这样 ref 不需要持有 COM 指针，重新解析时按同样的索引链再下钻一次；
//! 下钻后比对名称与角色，对不上就按过期 ref 拒绝。
//!
//! 所有 COM 调用都在本线程内完成（不跨 `await`），避免 `IUIAutomationElement` 在未初始化 COM 的
//! 线程上被使用。

use super::{A11yNode, A11yOptions, NodeId};
use crate::extensions::computer::{backend, state::Rect};
use windows::{
    Win32::{
        Foundation::HWND,
        System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        },
        UI::Accessibility::{
            CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationExpandCollapsePattern,
            IUIAutomationInvokePattern, IUIAutomationScrollItemPattern,
            IUIAutomationSelectionItemPattern, IUIAutomationTextPattern,
            IUIAutomationTogglePattern, IUIAutomationValuePattern, TreeScope_Children,
            UIA_ExpandCollapsePatternId, UIA_InvokePatternId, UIA_ScrollItemPatternId,
            UIA_SelectionItemPatternId, UIA_TextPatternId, UIA_TogglePatternId, UIA_ValuePatternId,
        },
    },
    core::BSTR,
};

/// 无障碍通道是否可用（Windows 上只要有桌面会话就能建 UIA 客户端）。
pub fn available() -> bool {
    true
}

/// 打开 UIA 客户端（在当前线程初始化 COM）。
fn uia() -> Result<IUIAutomation, String> {
    unsafe {
        // 线程可能已经初始化过 COM（同一线程池线程被复用时）：`RPC_E_CHANGED_MODE` 不是错误。
        _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("could not create the UI Automation client: {e}"))
    }
}

/// 把窗口标识转成 `HWND`。
fn hwnd_of(id: u64) -> HWND {
    HWND(id as usize as *mut core::ffi::c_void)
}

/// 从窗口标识取该窗口的根元素。
fn root_for(id: u64) -> Result<IUIAutomationElement, String> {
    let uia = uia()?;
    unsafe {
        uia.ElementFromHandle(hwnd_of(id))
            .map_err(|e| format!("window {id} exposes no UI Automation element: {e}"))
    }
}

/// 读一个元素的全部子元素。
fn children_of(uia: &IUIAutomation, element: &IUIAutomationElement) -> Vec<IUIAutomationElement> {
    unsafe {
        let Ok(condition) = uia.CreateTrueCondition() else {
            return Vec::new();
        };
        let Ok(array) = element.FindAll(TreeScope_Children, &condition) else {
            return Vec::new();
        };
        let Ok(length) = array.Length() else {
            return Vec::new();
        };
        (0..length)
            .filter_map(|index| array.GetElement(index).ok())
            .collect()
    }
}

/// 读一个字符串属性（失败时为空串）。
fn bstr(value: windows::core::Result<BSTR>) -> String {
    value.map(|v| v.to_string()).unwrap_or_default()
}

/// 读出元素的一句话角色名（本地化控件类型）。
fn role_of(element: &IUIAutomationElement) -> String {
    let localised = bstr(unsafe { element.CurrentLocalizedControlType() });
    if localised.is_empty() {
        "unknown".to_string()
    } else {
        localised
    }
}

/// 读元素的状态名。
fn states_of(element: &IUIAutomationElement) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        if element
            .CurrentIsEnabled()
            .map(|v| v.as_bool())
            .unwrap_or(false)
        {
            out.push("enabled".to_string());
        }
        if element
            .CurrentHasKeyboardFocus()
            .map(|v| v.as_bool())
            .unwrap_or(false)
        {
            out.push("focused".to_string());
        }
        if element
            .CurrentIsOffscreen()
            .map(|v| v.as_bool())
            .unwrap_or(false)
        {
            out.push("offscreen".to_string());
        }
    }
    out
}

/// 探测元素支持哪些动作（会逐个尝试取 pattern）。
fn patterns_of(element: &IUIAutomationElement) -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        if element
            .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
            .is_ok()
        {
            out.push("invoke".to_string());
        }
        if element
            .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .is_ok()
        {
            out.push("set_value".to_string());
        }
        if element
            .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
            .is_ok()
        {
            out.push("get_text".to_string());
        }
        if element
            .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
            .is_ok()
        {
            out.push("toggle".to_string());
        }
        if element
            .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId)
            .is_ok()
        {
            out.push("expand".to_string());
        }
        if element
            .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
            .is_ok()
        {
            out.push("select".to_string());
        }
        if element
            .GetCurrentPatternAs::<IUIAutomationScrollItemPattern>(UIA_ScrollItemPatternId)
            .is_ok()
        {
            out.push("scroll_into_view".to_string());
        }
    }
    out
}

/// 读元素的屏幕矩形（空矩形返回 `None`）。
fn rect_of(element: &IUIAutomationElement) -> Option<Rect> {
    let rect = unsafe { element.CurrentBoundingRectangle() }.ok()?;
    let width = (rect.right - rect.left).max(0) as u32;
    let height = (rect.bottom - rect.top).max(0) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    Some(Rect {
        x: rect.left,
        y: rect.top,
        width,
        height,
    })
}

/// 一次遍历的累积状态。
struct Walk<'a> {
    /// UIA 客户端。
    uia: &'a IUIAutomation,
    /// 预算与选项。
    options: &'a A11yOptions,
    /// 已经访问过的节点数。
    visited: usize,
    /// ref 序号 → 节点身份。
    nodes: Vec<NodeId>,
    /// 输出的行。
    out: Vec<A11yNode>,
}

impl Walk<'_> {
    /// 读一个元素并递归它的子元素。
    fn element(&mut self, element: &IUIAutomationElement, path: String, depth: usize) {
        if self.visited >= self.options.max_visited || self.nodes.len() >= self.options.max_nodes {
            return;
        }
        self.visited += 1;
        let name = bstr(unsafe { element.CurrentName() });
        let automation_id = bstr(unsafe { element.CurrentAutomationId() });
        let role = role_of(element);
        let rect = rect_of(element);
        let states = states_of(element);
        let actions = if self.options.patterns {
            patterns_of(element)
        } else {
            Vec::new()
        };
        if !(self.options.skip_empty
            && name.is_empty()
            && automation_id.is_empty()
            && actions.is_empty()
            && rect.is_none())
        {
            let index = self.nodes.len();
            self.nodes.push(NodeId {
                bus: path.split('.').next().unwrap_or("0").to_string(),
                path: path.clone(),
                name: name.clone(),
                role: role.clone(),
            });
            self.out.push(A11yNode {
                index,
                depth,
                role,
                name,
                description: String::new(),
                automation_id,
                states,
                rect,
                actions,
                interfaces: Vec::new(),
                has_children: false,
            });
        }
        if depth >= self.options.max_depth {
            return;
        }
        let children = children_of(self.uia, element);
        if !children.is_empty()
            && let Some(node) = self.out.last_mut()
        {
            node.has_children = true;
        }
        for (position, child) in children.into_iter().enumerate() {
            if self.nodes.len() >= self.options.max_nodes {
                break;
            }
            let child_path = format!("{path}.{position}");
            self.element(&child, child_path, depth + 1);
        }
    }
}

/// 读一棵无障碍树。
pub async fn read_tree(
    window: Option<u64>,
    options: A11yOptions,
) -> Result<(Vec<A11yNode>, Vec<NodeId>), String> {
    let id = match window {
        Some(id) => id,
        None => backend::windows()?
            .into_iter()
            .find(|w| w.focused)
            .map(|w| w.id)
            .ok_or_else(|| "nothing is focused; pass a window id".to_string())?,
    };
    let uia = uia()?;
    let root = root_for(id)?;
    let mut walk = Walk {
        uia: &uia,
        options: &options,
        visited: 0,
        nodes: Vec::new(),
        out: Vec::new(),
    };
    walk.element(&root, id.to_string(), 0);
    if walk.out.is_empty() {
        return Err(format!(
            "window {id} exposes an empty UI Automation tree (the application may not implement accessibility)"
        ));
    }
    Ok((walk.out, walk.nodes))
}

/// 按 ref 的内容重新找到元素（索引链逐层下钻，并核对名称与角色）。
fn element_for(target: &NodeId) -> Result<IUIAutomationElement, String> {
    let window_id: u64 = target
        .bus
        .parse()
        .map_err(|_| format!("{} is not a window handle", target.bus))?;
    let uia = uia()?;
    let mut element = root_for(window_id)?;
    let mut segments = target.path.split('.').skip(1).peekable();
    while let Some(segment) = segments.next() {
        let index: i32 = segment
            .parse()
            .map_err(|_| format!("{} is not a node path", target.path))?;
        element = children_of(&uia, &element)
            .into_iter()
            .nth(index.max(0) as usize)
            .ok_or_else(|| {
                format!(
                    "[STALE_REF] {} is no longer at that position; read the tree again",
                    target.path
                )
            })?;
    }
    let name = bstr(unsafe { element.CurrentName() });
    let role = role_of(&element);
    if name != target.name || role != target.role {
        return Err(format!(
            "[STALE_REF] the element at {} is now {role} {name:?}, not {} {:?}; read the tree again",
            target.path, target.role, target.name
        ));
    }
    Ok(element)
}

/// 执行第 `index` 个动作（UIA 的 `invoke` 没有序号概念，0 表示"默认动作"）。
pub async fn do_action(target: &NodeId, index: u32) -> Result<(), String> {
    if index != 0 {
        return Err("UI Automation has no numbered actions; use action_by_name".to_string());
    }
    action_by_name(target, "invoke").await
}

/// 按名字执行动作。
pub async fn action_by_name(target: &NodeId, wanted: &str) -> Result<(), String> {
    let element = element_for(target)?;
    unsafe {
        match wanted {
            "invoke" => element
                .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                .and_then(|pattern| pattern.Invoke())
                .map_err(|e| format!("{} does not support invoke: {e}", target.name)),
            "toggle" => element
                .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                .and_then(|pattern| pattern.Toggle())
                .map_err(|e| format!("{} does not support toggle: {e}", target.name)),
            "expand" => element
                .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                    UIA_ExpandCollapsePatternId,
                )
                .and_then(|pattern| pattern.Expand())
                .map_err(|e| format!("{} does not support expand: {e}", target.name)),
            "collapse" => element
                .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                    UIA_ExpandCollapsePatternId,
                )
                .and_then(|pattern| pattern.Collapse())
                .map_err(|e| format!("{} does not support collapse: {e}", target.name)),
            other => Err(format!("unknown action {other:?}")),
        }
    }
}

/// 读文本内容。
pub async fn read_text(target: &NodeId) -> Result<String, String> {
    let element = element_for(target)?;
    unsafe {
        let pattern = element
            .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
            .map_err(|e| format!("{} has no text: {e}", target.name))?;
        let range = pattern
            .DocumentRange()
            .map_err(|e| format!("{} has no document range: {e}", target.name))?;
        let text = range
            .GetText(-1)
            .map_err(|e| format!("{} text could not be read: {e}", target.name))?;
        Ok(text.to_string())
    }
}

/// 写文本内容（ValuePattern）。
pub async fn set_text(target: &NodeId, text: &str) -> Result<(), String> {
    let element = element_for(target)?;
    unsafe {
        element
            .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
            .and_then(|pattern| pattern.SetValue(&BSTR::from(text)))
            .map_err(|e| format!("{} does not accept text: {e}", target.name))
    }
}

/// 读控件值。
pub async fn read_value(target: &NodeId) -> Result<String, String> {
    let value = {
        let element = element_for(target)?;
        unsafe {
            element
                .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                .ok()
                .map(|pattern| bstr(pattern.CurrentValue()))
        }
    };
    match value {
        Some(value) => Ok(value),
        None => read_text(target).await,
    }
}

/// 给控件焦点。
pub async fn grab_focus(target: &NodeId) -> Result<(), String> {
    let element = element_for(target)?;
    unsafe {
        element
            .SetFocus()
            .map_err(|e| format!("{} refused focus: {e}", target.name))
    }
}

/// 选中一段文本：UI Automation 的选择要走 text range，这里不假装支持。
pub async fn select_text(_target: &NodeId, _start: i32, _end: i32) -> Result<(), String> {
    Err(
        "select_text is not wired up on Windows yet; use computer_key with ctrl+a / shift+arrows"
            .to_string(),
    )
}

/// 选中一个子项（SelectionItemPattern）。
pub async fn select_child(target: &NodeId, index: i32) -> Result<(), String> {
    if index != 0 {
        return Err("select on Windows selects the node itself; pass index 0".to_string());
    }
    let element = element_for(target)?;
    unsafe {
        element
            .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
            .and_then(|pattern| pattern.Select())
            .map_err(|e| format!("{} cannot be selected: {e}", target.name))
    }
}

/// 读光标偏移：需要 text range 的 selection，这里返回 `None`。
pub async fn caret_offset(_target: &NodeId) -> Option<i32> {
    None
}

/// 读节点的屏幕矩形。
pub async fn node_rect(target: &NodeId) -> Option<Rect> {
    let element = element_for(target).ok()?;
    rect_of(&element)
}

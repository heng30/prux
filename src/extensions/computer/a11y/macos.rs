//! macOS 无障碍通道：AXUIElement（ApplicationServices）。
//!
//! 直接调用 CoreFoundation / Accessibility 的 C API，不引第三方绑定：
//! 节点用 `(pid, 索引链)` 标识，索引链是"从应用的 AX 根元素开始，每一步取 AXChildren 的第几个"。
//! 重新解析时按同一索引链再下钻一次，并比对标题与角色；对不上就按过期 ref 拒绝。
//!
//! 需要用户在「系统设置 → 隐私与安全性 → 辅助功能」里给 prux 授权；没有授权时
//! `AXUIElementCopyAttributeValue` 统一返回 `-25211`（kAXErrorAPIDisabled）。

use super::{A11yNode, A11yOptions, NodeId};
use crate::extensions::computer::{backend, state::Rect};
use std::{
    ffi::{c_char, c_void},
    ptr,
};

/// CoreFoundation 的 UTF-8 编码常量。
const CF_UTF8: u32 = 0x0800_0100;
/// `kAXValueCGPointType`。
const AX_VALUE_CG_POINT: u32 = 1;
/// `kAXValueCGSizeType`。
const AX_VALUE_CG_SIZE: u32 = 2;

/// `CGPoint` 的内存布局（两个 f64）。
#[repr(C)]
struct CGPoint {
    /// x 坐标（点）。
    x: f64,
    /// y 坐标（点）。
    y: f64,
}

/// `CGSize` 的内存布局（两个 f64）。
#[repr(C)]
struct CGSize {
    /// 宽度（点）。
    width: f64,
    /// 高度（点）。
    height: f64,
}

/// CoreFoundation 的 CFTypeRef。
type CFRef = *const c_void;

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    /// 释放一个 CF 对象。
    fn CFRelease(cf: CFRef);
    /// 增加一个 CF 对象的引用计数。
    fn CFRetain(cf: CFRef) -> CFRef;
    /// 取一个 CF 对象的类型 ID。
    fn CFGetTypeID(cf: CFRef) -> usize;
    /// 字符串类型 ID。
    fn CFStringGetTypeID() -> usize;
    /// 数组类型 ID。
    fn CFArrayGetTypeID() -> usize;
    /// 布尔类型 ID。
    fn CFBooleanGetTypeID() -> usize;
    /// 数组元素个数。
    fn CFArrayGetCount(array: CFRef) -> isize;
    /// 数组第 `index` 个元素（借用，不增加引用）。
    fn CFArrayGetValueAtIndex(array: CFRef, index: isize) -> CFRef;
    /// 字符串长度（UTF-16 单元）。
    fn CFStringGetLength(text: CFRef) -> isize;
    /// 指定编码下字符串的最大字节数。
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    /// 把字符串写进缓冲区（UTF-8，带结尾 0）。
    fn CFStringGetCString(text: CFRef, buffer: *mut c_char, size: isize, encoding: u32) -> bool;
    /// 用 C 字符串造一个 CF 字符串（+1 引用）。
    fn CFStringCreateWithCString(alloc: CFRef, text: *const c_char, encoding: u32) -> CFRef;
    /// 布尔值。
    fn CFBooleanGetValue(value: CFRef) -> bool;
    /// `kCFBooleanTrue`。
    static kCFBooleanTrue: CFRef;
    /// `kCFBooleanFalse`。
    static kCFBooleanFalse: CFRef;
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    /// 为某个进程创建 AX 根元素（+1 引用）。
    fn AXUIElementCreateApplication(pid: i32) -> CFRef;
    /// 读一个 AX 属性（成功后 `value` 带 +1 引用）。
    fn AXUIElementCopyAttributeValue(element: CFRef, attribute: CFRef, value: *mut CFRef) -> i32;
    /// 写一个 AX 属性。
    fn AXUIElementSetAttributeValue(element: CFRef, attribute: CFRef, value: CFRef) -> i32;
    /// 执行一个 AX 动作（`AXPress`、`AXShowMenu` …）。
    fn AXUIElementPerformAction(element: CFRef, action: CFRef) -> i32;
    /// 读元素支持的动作名（成功后 `names` 带 +1 引用）。
    fn AXUIElementCopyActionNames(element: CFRef, names: *mut CFRef) -> i32;
    /// 从一个 AXValue 里取出 `CGPoint` / `CGSize` 等值。
    fn AXValueGetValue(value: CFRef, the_type: u32, out: *mut c_void) -> bool;
}

/// 一个持有 +1 引用的 CF 对象，`Drop` 时释放。
struct Owned(CFRef);

impl Owned {
    /// 把 +1 引用的裸指针包成 `Owned`（空指针返回 `None`）。
    fn new(raw: CFRef) -> Option<Self> {
        (!raw.is_null()).then_some(Owned(raw))
    }

    /// 包一个用 `CFStringCreateWithCString` 造出来的字符串。
    fn string(text: &str) -> Option<Self> {
        let c = std::ffi::CString::new(text).ok()?;
        Self::new(unsafe { CFStringCreateWithCString(ptr::null(), c.as_ptr(), CF_UTF8) })
    }

    /// 借用裸指针。
    fn raw(&self) -> CFRef {
        self.0
    }
}

impl Drop for Owned {
    /// 释放持有的引用。
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { CFRelease(self.0) };
        }
    }
}

/// 读一个 AX 属性（失败返回 `None`）。
fn attribute(element: CFRef, name: &str) -> Option<Owned> {
    let key = Owned::string(name)?;
    let mut value: CFRef = ptr::null();
    let status = unsafe { AXUIElementCopyAttributeValue(element, key.raw(), &mut value) };
    if status != 0 {
        return None;
    }
    Owned::new(value)
}

/// 把 CF 字符串转成 Rust 字符串。
fn as_string(value: CFRef) -> Option<String> {
    unsafe {
        if CFGetTypeID(value) != CFStringGetTypeID() {
            return None;
        }
        let length = CFStringGetLength(value);
        let size = CFStringGetMaximumSizeForEncoding(length, CF_UTF8) + 1;
        if size <= 1 {
            return Some(String::new());
        }
        let mut buffer = vec![0i8; size as usize];
        if !CFStringGetCString(value, buffer.as_mut_ptr(), size, CF_UTF8) {
            return None;
        }
        std::ffi::CStr::from_ptr(buffer.as_ptr())
            .to_str()
            .ok()
            .map(str::to_string)
    }
}

/// 读一个字符串属性。
fn string_attribute(element: CFRef, name: &str) -> String {
    attribute(element, name)
        .and_then(|value| as_string(value.raw()))
        .unwrap_or_default()
}

/// 读子元素（每个都 +1 引用，由 `Owned` 释放）。
fn children(element: CFRef) -> Vec<Owned> {
    let Some(list) = attribute(element, "AXChildren") else {
        return Vec::new();
    };
    unsafe {
        if CFGetTypeID(list.raw()) != CFArrayGetTypeID() {
            return Vec::new();
        }
        let count = CFArrayGetCount(list.raw());
        (0..count)
            .filter_map(|index| {
                let child = CFArrayGetValueAtIndex(list.raw(), index);
                if child.is_null() {
                    None
                } else {
                    Owned::new(CFRetain(child))
                }
            })
            .collect()
    }
}

/// 读角色名。
fn role_of(element: CFRef) -> String {
    string_attribute(element, "AXRole")
}

/// 读可访问名（标题优先，其次描述，最后值）。
fn name_of(element: CFRef) -> String {
    for key in ["AXTitle", "AXDescription"] {
        let text = string_attribute(element, key);
        if !text.is_empty() {
            return text;
        }
    }
    attribute(element, "AXValue")
        .and_then(|value| as_string(value.raw()))
        .unwrap_or_default()
}

/// 读状态名。
fn states_of(element: CFRef) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(value) = attribute(element, "AXEnabled") {
        unsafe {
            if CFGetTypeID(value.raw()) == CFBooleanGetTypeID() && CFBooleanGetValue(value.raw()) {
                out.push("enabled".to_string());
            }
        }
    }
    if let Some(value) = attribute(element, "AXFocused") {
        unsafe {
            if CFGetTypeID(value.raw()) == CFBooleanGetTypeID() && CFBooleanGetValue(value.raw()) {
                out.push("focused".to_string());
            }
        }
    }
    if let Some(value) = attribute(element, "AXSelected") {
        unsafe {
            if CFGetTypeID(value.raw()) == CFBooleanGetTypeID() && CFBooleanGetValue(value.raw()) {
                out.push("selected".to_string());
            }
        }
    }
    out
}

/// 探测元素支持的动作（`AXPress` → invoke，能写 `AXValue` → set_value）。
fn actions_of(element: CFRef) -> Vec<String> {
    let mut out = Vec::new();
    let mut raw: CFRef = ptr::null();
    let status = unsafe { AXUIElementCopyActionNames(element, &mut raw) };
    if status == 0
        && let Some(list) = Owned::new(raw)
    {
        unsafe {
            if CFGetTypeID(list.raw()) == CFArrayGetTypeID() {
                for index in 0..CFArrayGetCount(list.raw()) {
                    let item = CFArrayGetValueAtIndex(list.raw(), index);
                    if let Some(name) = as_string(item)
                        && name.contains("Press")
                    {
                        out.push("invoke".to_string());
                    }
                }
            }
        }
    }
    if attribute(element, "AXValue").is_some() && out.is_empty() {
        out.push("set_value".to_string());
    }
    out
}

/// 读元素的屏幕矩形（位置 + 尺寸，AXValue 解析失败时返回 `None`）。
fn rect_of(element: CFRef) -> Option<Rect> {
    let position = attribute(element, "AXPosition")?;
    let size = attribute(element, "AXSize")?;
    let mut point = CGPoint { x: 0.0, y: 0.0 };
    let mut extent = CGSize {
        width: 0.0,
        height: 0.0,
    };
    unsafe {
        if !AXValueGetValue(
            position.raw(),
            AX_VALUE_CG_POINT,
            &mut point as *mut CGPoint as *mut c_void,
        ) {
            return None;
        }
        if !AXValueGetValue(
            size.raw(),
            AX_VALUE_CG_SIZE,
            &mut extent as *mut CGSize as *mut c_void,
        ) {
            return None;
        }
    }
    if extent.width <= 0.0 || extent.height <= 0.0 {
        return None;
    }
    Some(Rect {
        x: point.x.round() as i32,
        y: point.y.round() as i32,
        width: extent.width.round() as u32,
        height: extent.height.round() as u32,
    })
}

/// 无障碍通道是否可用（macOS 上只要有窗口服务器就能建 AX 客户端）。
pub fn available() -> bool {
    true
}

/// 一次遍历的累积状态。
struct Walk<'a> {
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
    fn element(&mut self, element: CFRef, path: String, depth: usize) {
        if self.visited >= self.options.max_visited || self.nodes.len() >= self.options.max_nodes {
            return;
        }
        self.visited += 1;
        let name = name_of(element);
        let role = role_of(element);
        let rect = rect_of(element);
        let states = states_of(element);
        let actions = if self.options.patterns {
            actions_of(element)
        } else {
            Vec::new()
        };
        if !(self.options.skip_empty && name.is_empty() && actions.is_empty() && rect.is_none()) {
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
                automation_id: String::new(),
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
        let list = children(element);
        if !list.is_empty()
            && let Some(node) = self.out.last_mut()
        {
            node.has_children = true;
        }
        for (position, child) in list.iter().enumerate() {
            if self.nodes.len() >= self.options.max_nodes {
                break;
            }
            let child_path = format!("{path}.{position}");
            self.element(child.raw(), child_path, depth + 1);
        }
    }
}

/// 为窗口的进程创建 AX 根元素。
fn root_for(pid: u32) -> Result<Owned, String> {
    let raw = unsafe { AXUIElementCreateApplication(pid as i32) };
    Owned::new(raw).ok_or_else(|| format!("could not create an AX element for pid {pid}"))
}

/// 读一棵无障碍树。
pub async fn read_tree(
    window: Option<u64>,
    options: A11yOptions,
) -> Result<(Vec<A11yNode>, Vec<NodeId>), String> {
    let target = match window {
        Some(id) => backend::window(id)?,
        None => backend::windows()?
            .into_iter()
            .find(|w| w.focused)
            .ok_or_else(|| "nothing is focused; pass a window id".to_string())?,
    };
    if target.pid == 0 {
        return Err(format!("window {} reports no process id", target.id));
    }
    let root = root_for(target.pid)?;
    let mut walk = Walk {
        options: &options,
        visited: 0,
        nodes: Vec::new(),
        out: Vec::new(),
    };
    walk.element(root.raw(), target.pid.to_string(), 0);
    if walk.out.is_empty() {
        return Err(format!(
            "window {} exposes no accessibility tree; grant Accessibility permission to prux and retry",
            target.id
        ));
    }
    Ok((walk.out, walk.nodes))
}

/// 按 ref 的内容重新找到元素（索引链逐层下钻，并核对名称与角色）。
fn element_for(target: &NodeId) -> Result<Owned, String> {
    let pid: u32 = target
        .bus
        .parse()
        .map_err(|_| format!("{} is not a process id", target.bus))?;
    let root = root_for(pid)?;
    let mut element = root;
    let mut segments = target.path.split('.').skip(1).peekable();
    while let Some(segment) = segments.next() {
        let index: usize = segment
            .parse()
            .map_err(|_| format!("{} is not a node path", target.path))?;
        let list = children(element.raw());
        element = list.into_iter().nth(index).ok_or_else(|| {
            format!(
                "[STALE_REF] {} is no longer at that position; read the tree again",
                target.path
            )
        })?;
    }
    let name = name_of(element.raw());
    let role = role_of(element.raw());
    if name != target.name || role != target.role {
        return Err(format!(
            "[STALE_REF] the element at {} is now {role} {name:?}, not {} {:?}; read the tree again",
            target.path, target.role, target.name
        ));
    }
    Ok(element)
}

/// 执行 AX 动作。
fn perform(element: CFRef, action: &str) -> Result<(), String> {
    let name = Owned::string(action).ok_or_else(|| "invalid action name".to_string())?;
    let status = unsafe { AXUIElementPerformAction(element, name.raw()) };
    if status == 0 {
        Ok(())
    } else {
        Err(format!("AX action {action} failed with {status}"))
    }
}

/// 写一个布尔属性。
fn set_bool(element: CFRef, name: &str, value: bool) -> Result<(), String> {
    let key = Owned::string(name).ok_or_else(|| "invalid attribute name".to_string())?;
    let raw = if value {
        unsafe { kCFBooleanTrue }
    } else {
        unsafe { kCFBooleanFalse }
    };
    let status = unsafe { AXUIElementSetAttributeValue(element, key.raw(), raw) };
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "AX attribute {name} could not be set (status {status})"
        ))
    }
}

/// 执行第 `index` 个动作（AX 只有具名动作，0 表示默认的 `AXPress`）。
pub async fn do_action(target: &NodeId, index: u32) -> Result<(), String> {
    if index != 0 {
        return Err("AX exposes named actions, not numbered ones; use action_by_name".to_string());
    }
    action_by_name(target, "invoke").await
}

/// 按名字执行动作。
pub async fn action_by_name(target: &NodeId, wanted: &str) -> Result<(), String> {
    let element = element_for(target)?;
    match wanted {
        "invoke" | "select" | "toggle" => perform(element.raw(), "AXPress"),
        "expand" => perform(element.raw(), "AXPress"),
        "collapse" => perform(element.raw(), "AXPress"),
        other => Err(format!("unknown action {other:?}")),
    }
}

/// 读文本内容（`AXValue`，没有时退回 `AXDescription`）。
pub async fn read_text(target: &NodeId) -> Result<String, String> {
    let element = element_for(target)?;
    let text = attribute(element.raw(), "AXValue")
        .and_then(|value| as_string(value.raw()))
        .or_else(|| {
            let description = string_attribute(element.raw(), "AXDescription");
            (!description.is_empty()).then_some(description)
        })
        .unwrap_or_default();
    Ok(text)
}

/// 写文本内容（`AXValue`）。
pub async fn set_text(target: &NodeId, text: &str) -> Result<(), String> {
    let element = element_for(target)?;
    let key = Owned::string("AXValue").ok_or_else(|| "invalid attribute name".to_string())?;
    let value = Owned::string(text).ok_or_else(|| "text could not be encoded".to_string())?;
    let status = unsafe { AXUIElementSetAttributeValue(element.raw(), key.raw(), value.raw()) };
    if status == 0 {
        Ok(())
    } else {
        Err(format!(
            "{} does not accept text (AX status {status})",
            target.name
        ))
    }
}

/// 读控件值。
pub async fn read_value(target: &NodeId) -> Result<String, String> {
    read_text(target).await
}

/// 给控件焦点。
pub async fn grab_focus(target: &NodeId) -> Result<(), String> {
    let element = element_for(target)?;
    set_bool(element.raw(), "AXFocused", true)
}

/// 选中一段文本：AX 的选区写 `AXSelectedTextRange`，这里不假装支持。
pub async fn select_text(_target: &NodeId, _start: i32, _end: i32) -> Result<(), String> {
    Err(
        "select_text is not wired up on macOS yet; use computer_key with cmd+a / shift+arrows"
            .to_string(),
    )
}

/// 选中一个子项（按下子元素本身）。
pub async fn select_child(target: &NodeId, index: i32) -> Result<(), String> {
    if index != 0 {
        return Err("select on macOS presses the node itself; pass index 0".to_string());
    }
    action_by_name(target, "select").await
}

/// 读光标偏移：需要解析 `AXSelectedTextRange` 的 range 值，这里返回 `None`。
pub async fn caret_offset(_target: &NodeId) -> Option<i32> {
    None
}

/// 读节点的屏幕矩形。
pub async fn node_rect(target: &NodeId) -> Option<Rect> {
    let element = element_for(target).ok()?;
    rect_of(element.raw())
}

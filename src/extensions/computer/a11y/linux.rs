//! Linux 无障碍通道：AT-SPI2（D-Bus）。
//!
//! Wayland 没有 Windows UIA 那样的进程内 API，无障碍树由每个应用通过 `org.a11y.atspi`
//! 暴露在一条**专用 D-Bus 总线**上（地址由会话总线上的 `org.a11y.Bus.GetAddress` 给出）。
//! 一个节点由 `(bus_name, object_path)` 二元组标识，因此这里所有调用都带目标与路径。
//!
//! 能力取决于应用有没有实现无障碍：GTK/Qt 默认实现，Electron 需要 `--force-renderer-accessibility`，
//! 终端类应用通常只有一层空壳。读不到东西时返回的是"这个应用没有暴露无障碍"，而不是空树。

use super::{A11yNode, A11yOptions, NodeId};
use crate::extensions::computer::{backend, state::Rect};
use zbus::{Connection, OwnedVariant};

/// 无障碍总线的会话总线入口。
const A11Y_BUS: &str = "org.a11y.Bus";
/// 无障碍总线入口的对象路径。
const A11Y_BUS_PATH: &str = "/org/a11y/bus";
/// 无障碍树的根节点路径。
const A11Y_ROOT: &str = "/org/a11y/atspi/accessible/root";
/// `Accessible` 接口名。
const IFACE_ACCESSIBLE: &str = "org.a11y.atspi.Accessible";
/// `Component` 接口名。
const IFACE_COMPONENT: &str = "org.a11y.atspi.Component";
/// `Action` 接口名。
const IFACE_ACTION: &str = "org.a11y.atspi.Action";
/// `Text` 接口名。
const IFACE_TEXT: &str = "org.a11y.atspi.Text";
/// `EditableText` 接口名。
const IFACE_EDITABLE_TEXT: &str = "org.a11y.atspi.EditableText";
/// `Value` 接口名。
const IFACE_VALUE: &str = "org.a11y.atspi.Value";
/// `Application` 接口名。
const IFACE_APPLICATION: &str = "org.a11y.atspi.Application";
/// `Selection` 接口名。
const IFACE_SELECTION: &str = "org.a11y.atspi.Selection";
/// `org.freedesktop.DBus.Properties` 接口名。
const IFACE_PROPS: &str = "org.freedesktop.DBus.Properties";

/// 无障碍通道是否可用：需要一条会话总线（无障碍总线由它派生）。
pub fn available() -> bool {
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
}

/// 连到无障碍总线。
pub async fn connect() -> Result<Connection, String> {
    let session = Connection::session()
        .await
        .map_err(|e| format!("no session bus: {e}"))?;
    let reply = session
        .call_method(
            Some(A11Y_BUS),
            A11Y_BUS_PATH,
            Some(A11Y_BUS),
            "GetAddress",
            &(),
        )
        .await
        .map_err(|e| {
            format!(
                "the accessibility bus is not available: {e}. Start it with `at-spi2-core` \
                 (systemd user unit or `at-spi-bus-launcher`) and retry"
            )
        })?;
    let (address,): (String,) = reply
        .body()
        .deserialize()
        .map_err(|e| format!("a11y bus address is not a string: {e}"))?;
    zbus::connection::Builder::address(address.as_str())
        .build()
        .await
        .map_err(|e| format!("could not connect to the accessibility bus: {e}"))
}

/// 调一个无参方法并把回复解成 `T`。
async fn call<T>(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
) -> Result<T, String>
where
    T: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
{
    let reply = conn
        .call_method(Some(dest), path, Some(iface), method, &())
        .await
        .map_err(|e| format!("{iface}.{method} on {path}: {e}"))?;
    reply
        .body()
        .deserialize()
        .map_err(|e| format!("{iface}.{method} on {path}: unexpected reply: {e}"))
}

/// 调一个带参方法并把回复解成 `T`。
async fn call_with<B, T>(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    method: &str,
    body: &B,
) -> Result<T, String>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
    T: for<'d> serde::Deserialize<'d> + zbus::zvariant::Type,
{
    let reply = conn
        .call_method(Some(dest), path, Some(iface), method, body)
        .await
        .map_err(|e| format!("{iface}.{method} on {path}: {e}"))?;
    reply
        .body()
        .deserialize()
        .map_err(|e| format!("{iface}.{method} on {path}: unexpected reply: {e}"))
}

/// 读一个字符串属性。
async fn string_property(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    name: &str,
) -> Result<String, String> {
    let (value,): (OwnedVariant,) =
        call_with(conn, dest, path, IFACE_PROPS, "Get", &(iface, name)).await?;
    match String::try_from(value) {
        Ok(text) => Ok(text),
        Err(_) => Ok(String::new()),
    }
}

/// 读一个整数属性。
async fn int_property(
    conn: &Connection,
    dest: &str,
    path: &str,
    iface: &str,
    name: &str,
) -> Result<i64, String> {
    let (value,): (OwnedVariant,) =
        call_with(conn, dest, path, IFACE_PROPS, "Get", &(iface, name)).await?;
    if let Ok(v) = i32::try_from(
        value
            .try_clone()
            .unwrap_or_else(|_| OwnedVariant::from(0i32)),
    ) {
        return Ok(i64::from(v));
    }
    if let Ok(v) = i64::try_from(
        value
            .try_clone()
            .unwrap_or_else(|_| OwnedVariant::from(0i32)),
    ) {
        return Ok(v);
    }
    if let Ok(v) = u32::try_from(value) {
        return Ok(i64::from(v));
    }
    Ok(0)
}

/// 子节点列表。
async fn children(
    conn: &Connection,
    dest: &str,
    path: &str,
) -> Result<Vec<(String, String)>, String> {
    let (list,): (Vec<(String, zbus::OwnedObjectPath)>,) =
        call(conn, dest, path, IFACE_ACCESSIBLE, "GetChildren").await?;
    Ok(list
        .into_iter()
        .map(|(bus, object)| (bus, object.to_string()))
        .collect())
}

/// 角色名（如 `push button`）。
async fn role_name(conn: &Connection, dest: &str, path: &str) -> Result<String, String> {
    let (role,): (String,) = call(conn, dest, path, IFACE_ACCESSIBLE, "GetRoleName").await?;
    Ok(role)
}

/// 已实现的接口名列表。
async fn interfaces(conn: &Connection, dest: &str, path: &str) -> Result<Vec<String>, String> {
    let (list,): (Vec<String>,) = call(conn, dest, path, IFACE_ACCESSIBLE, "GetInterfaces").await?;
    Ok(list)
}

/// 屏幕矩形（CoordType::Screen = 0）。
async fn extents(conn: &Connection, dest: &str, path: &str) -> Option<Rect> {
    let reply = conn
        .call_method(
            Some(dest),
            path,
            Some(IFACE_COMPONENT),
            "GetExtents",
            &(0u32,),
        )
        .await
        .ok()?;
    let (x, y, width, height): (i32, i32, i32, i32) = reply.body().deserialize().ok()?;
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(Rect {
        x,
        y,
        width: width as u32,
        height: height as u32,
    })
}

/// 动作名列表（`Action` 接口）。
async fn action_names(conn: &Connection, dest: &str, path: &str) -> Vec<String> {
    let reply = conn
        .call_method(Some(dest), path, Some(IFACE_ACTION), "GetActions", &())
        .await;
    let Ok(reply) = reply else {
        return Vec::new();
    };
    let Ok(actions) = reply
        .body()
        .deserialize::<(Vec<(String, String, String)>,)>()
    else {
        return Vec::new();
    };
    actions.0.into_iter().map(|(name, _, _)| name).collect()
}

/// 状态位名（只映射前 32 位里模型用得上的那些）。
fn state_names(bits: &[u32]) -> Vec<String> {
    const NAMES: [&str; 32] = [
        "invalid",
        "active",
        "armed",
        "busy",
        "checked",
        "collapsed",
        "defunct",
        "editable",
        "enabled",
        "expandable",
        "expanded",
        "focusable",
        "focused",
        "has_tooltip",
        "horizontal",
        "iconified",
        "modal",
        "multi_line",
        "multiselectable",
        "opaque",
        "pressed",
        "resizable",
        "selectable",
        "selected",
        "sensitive",
        "showing",
        "single_line",
        "stale",
        "transient",
        "vertical",
        "visible",
        "manages_descendants",
    ];
    let mut out = Vec::new();
    for (word_index, word) in bits.iter().enumerate() {
        for bit in 0..32u32 {
            if word & (1 << bit) == 0 {
                continue;
            }
            let index = word_index * 32 + bit as usize;
            if let Some(name) = NAMES.get(index) {
                out.push((*name).to_string());
            }
        }
    }
    out
}

/// 读一个节点的状态位。
async fn states(conn: &Connection, dest: &str, path: &str) -> Vec<String> {
    match call::<(Vec<u32>,)>(conn, dest, path, IFACE_ACCESSIBLE, "GetState").await {
        Ok((bits,)) => state_names(&bits),
        Err(_) => Vec::new(),
    }
}

/// 一次树遍历的累积状态（预算、ref 表、输出行）。
struct Walk<'a> {
    /// 无障碍总线连接。
    conn: &'a Connection,
    /// 预算与过滤选项。
    options: &'a A11yOptions,
    /// 已经访问过的节点数（含没进输出的）。
    visited: usize,
    /// ref 序号 → 节点身份。
    nodes: Vec<NodeId>,
    /// 渲染用的行。
    out: Vec<A11yNode>,
}

impl Walk<'_> {
    /// 读取一个节点（并递归它的子节点）。
    async fn node(&mut self, bus: &str, path: &str, depth: usize) {
        if self.visited >= self.options.max_visited || self.nodes.len() >= self.options.max_nodes {
            return;
        }
        self.visited += 1;
        let conn = self.conn;
        let name = string_property(conn, bus, path, IFACE_ACCESSIBLE, "Name")
            .await
            .unwrap_or_default();
        let description = string_property(conn, bus, path, IFACE_ACCESSIBLE, "Description")
            .await
            .unwrap_or_default();
        let accessible_id = string_property(conn, bus, path, IFACE_ACCESSIBLE, "AccessibleId")
            .await
            .unwrap_or_default();
        let role = role_name(conn, bus, path)
            .await
            .unwrap_or_else(|_| "unknown".to_string());
        let interfaces = interfaces(conn, bus, path).await.unwrap_or_default();
        let actions = if self.options.patterns {
            action_names(conn, bus, path).await
        } else {
            Vec::new()
        };
        let rect = extents(conn, bus, path).await;
        let states = states(conn, bus, path).await;

        let id = NodeId {
            bus: bus.to_string(),
            path: path.to_string(),
            name: name.clone(),
            role: role.clone(),
        };
        let index = self.nodes.len();
        self.nodes.push(id);
        self.out.push(A11yNode {
            index,
            depth,
            role,
            name,
            description,
            automation_id: accessible_id,
            states,
            rect,
            actions,
            interfaces,
            has_children: false,
        });

        if depth >= self.options.max_depth {
            return;
        }
        let Ok(list) = children(conn, bus, path).await else {
            return;
        };
        if !list.is_empty()
            && let Some(node) = self.out.last_mut()
        {
            node.has_children = true;
        }
        for (child_bus, child_path) in list {
            if self.visited >= self.options.max_visited
                || self.nodes.len() >= self.options.max_nodes
            {
                break;
            }
            // 子节点通常与应用同一个 bus，但规范允许它们不同，所以按返回值来。
            Box::pin(self.node(&child_bus, &child_path, depth + 1)).await;
        }
    }
}

/// 读一棵无障碍树。
///
/// `window` 为 `Some(id)` 时先按该窗口的 pid 找到对应应用的根节点；`None` 读焦点窗口。
pub async fn read_tree(
    window: Option<u64>,
    options: A11yOptions,
) -> Result<(Vec<A11yNode>, Vec<NodeId>), String> {
    let conn = connect().await?;
    let target = match window {
        Some(id) => Some(backend::window(id)?),
        None => backend::windows()?.into_iter().find(|w| w.focused),
    };
    let (root_bus, root_path) = match &target {
        Some(window) => application_for_pid(&conn, window.pid).await?,
        None => {
            // 没有焦点窗口：读整棵桌面树（根的第一个子节点就是被聚焦的应用）。
            ("".to_string(), A11Y_ROOT.to_string())
        }
    };
    let (bus, path) = if root_bus.is_empty() {
        (String::new(), root_path)
    } else {
        (root_bus, root_path)
    };

    let mut state = Walk {
        conn: &conn,
        options: &options,
        visited: 0,
        nodes: Vec::new(),
        out: Vec::new(),
    };
    if bus.is_empty() {
        // 根节点自身在 `org.a11y.atspi.Registry` 上，子节点才是应用。
        let list = children(&conn, "org.a11y.atspi.Registry", &path).await?;
        for (child_bus, child_path) in list {
            if state.nodes.len() >= options.max_nodes {
                break;
            }
            state.node(&child_bus, &child_path, 0).await;
        }
    } else {
        state.node(&bus, &path, 0).await;
    }
    let Walk { nodes, out, .. } = state;
    if out.is_empty() {
        return Err(match target {
            Some(w) => format!(
                "window {} ({:?}) exposes no accessibility tree; the application may not have accessibility enabled",
                w.id, w.app
            ),
            None => "nothing is focused, and the desktop exposes no accessibility tree".to_string(),
        });
    }
    Ok((out, nodes))
}

/// 按 pid 找到该应用的 AT-SPI 根节点。
async fn application_for_pid(conn: &Connection, pid: u32) -> Result<(String, String), String> {
    let list = children(conn, "org.a11y.atspi.Registry", A11Y_ROOT).await?;
    for (bus, path) in list {
        let id = int_property(conn, &bus, &path, IFACE_APPLICATION, "Id")
            .await
            .unwrap_or(0);
        if id == i64::from(pid) {
            return Ok((bus, path));
        }
    }
    Err(format!(
        "no accessibility application reports pid {pid}; the window may belong to an app without accessibility enabled \
         (Electron needs --force-renderer-accessibility)"
    ))
}

/// 在一个节点上执行动作。
pub async fn do_action(target: &NodeId, index: u32) -> Result<(), String> {
    let conn = connect().await?;
    let (ok,): (bool,) = call_with(
        &conn,
        &target.bus,
        &target.path,
        IFACE_ACTION,
        "DoAction",
        &(index,),
    )
    .await?;
    if ok {
        Ok(())
    } else {
        Err(format!(
            "the application refused action {index} on {}",
            target.name
        ))
    }
}

/// 读一个可编辑控件的文本。
pub async fn read_text(target: &NodeId) -> Result<String, String> {
    let conn = connect().await?;
    let (count,): (i32,) = call(
        &conn,
        &target.bus,
        &target.path,
        IFACE_TEXT,
        "GetCharacterCount",
    )
    .await
    .unwrap_or((0,));
    if count <= 0 {
        return Ok(String::new());
    }
    let (text,): (String,) = call_with(
        &conn,
        &target.bus,
        &target.path,
        IFACE_TEXT,
        "GetText",
        &(0i32, count),
    )
    .await?;
    Ok(text)
}

/// 写入一个可编辑控件（优先 `EditableText.SetTextContents`，退回 `Value.SetCurrentValue`）。
pub async fn set_text(target: &NodeId, text: &str) -> Result<(), String> {
    let conn = connect().await?;
    let result: Result<(bool,), String> = call_with(
        &conn,
        &target.bus,
        &target.path,
        IFACE_EDITABLE_TEXT,
        "SetTextContents",
        &(text,),
    )
    .await;
    match result {
        Ok((true,)) => Ok(()),
        _ => {
            let numeric = text.trim().parse::<f64>().map_err(|_| {
                format!(
                    "{} does not accept text: it implements neither EditableText nor a numeric Value",
                    target.name
                )
            })?;
            let (_,): (bool,) = call_with(
                &conn,
                &target.bus,
                &target.path,
                IFACE_VALUE,
                "SetCurrentValue",
                &(numeric,),
            )
            .await?;
            Ok(())
        }
    }
}

/// 读控件的值（`Value.CurrentValue` 或文本内容）。
pub async fn read_value(target: &NodeId) -> Result<String, String> {
    let conn = connect().await?;
    let (value,): (OwnedVariant,) = call_with(
        &conn,
        &target.bus,
        &target.path,
        IFACE_PROPS,
        "Get",
        &(IFACE_VALUE, "CurrentValue"),
    )
    .await?;
    if let Ok(v) = f64::try_from(value) {
        return Ok(format!("{v}"));
    }
    read_text(target).await
}

/// 给控件焦点。
pub async fn grab_focus(target: &NodeId) -> Result<(), String> {
    let conn = connect().await?;
    let (ok,): (bool,) = call(
        &conn,
        &target.bus,
        &target.path,
        IFACE_COMPONENT,
        "GrabFocus",
    )
    .await?;
    if ok {
        Ok(())
    } else {
        Err(format!("{} refused focus", target.name))
    }
}

/// 选一段文本。
pub async fn select_text(target: &NodeId, start: i32, end: i32) -> Result<(), String> {
    let conn = connect().await?;
    let (ok,): (bool,) = call_with(
        &conn,
        &target.bus,
        &target.path,
        IFACE_TEXT,
        "SetSelection",
        &(0i32, start, end),
    )
    .await?;
    if ok {
        Ok(())
    } else {
        Err(format!("{} refused the selection", target.name))
    }
}

/// 选一个子项（列表/表格）。
pub async fn select_child(target: &NodeId, index: i32) -> Result<(), String> {
    let conn = connect().await?;
    let (ok,): (bool,) = call_with(
        &conn,
        &target.bus,
        &target.path,
        IFACE_SELECTION,
        "SelectChild",
        &(index,),
    )
    .await?;
    if ok {
        Ok(())
    } else {
        Err(format!("{} refused selecting child {index}", target.name))
    }
}

/// 读光标位置（字符偏移）；不支持时返回 `None`。
pub async fn caret_offset(target: &NodeId) -> Option<i32> {
    let conn = connect().await.ok()?;
    let (offset,): (i32,) = call(
        &conn,
        &target.bus,
        &target.path,
        IFACE_TEXT,
        "GetCaretOffset",
    )
    .await
    .ok()?;
    Some(offset)
}

/// 在节点上按下某个具名动作（`expand` / `collapse` / `toggle` …）；找不到就报错。
pub async fn action_by_name(target: &NodeId, wanted: &str) -> Result<(), String> {
    let conn = connect().await?;
    let reply = conn
        .call_method(
            Some(target.bus.as_str()),
            target.path.as_str(),
            Some(IFACE_ACTION),
            "GetActions",
            &(),
        )
        .await
        .map_err(|e| format!("{IFACE_ACTION}.GetActions: {e}"))?;
    let (actions,): (Vec<(String, String, String)>,) = reply
        .body()
        .deserialize()
        .map_err(|e| format!("{IFACE_ACTION}.GetActions: {e}"))?;
    let position = actions.iter().position(|(name, _, _)| {
        let name = name.to_ascii_lowercase();
        name == wanted.to_ascii_lowercase() || name.contains(&wanted.to_ascii_lowercase())
    });
    match position {
        Some(index) => do_action(target, index as u32).await,
        None => Err(format!(
            "{} has no {wanted:?} action (available: {})",
            target.name,
            actions
                .iter()
                .map(|(name, _, _)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// 读节点的矩形（用于 `click_text` 定位）。
pub async fn node_rect(target: &NodeId) -> Option<Rect> {
    let conn = connect().await.ok()?;
    extents(&conn, &target.bus, &target.path).await
}

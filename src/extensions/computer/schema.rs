//! 14 个 `computer_*` 工具的定义（描述、参数 schema、snippet）。
//!
//! 描述里刻意写清能力边界（Wayland 没有全局窗口枚举、窗口截图受遮挡影响、无障碍取决于应用），
//! 因为模型读到的是这些字，而不是平台的错误消息。

use crate::core::extensions::{ExtensionTool, ToolAnnotations, ToolExposure};
use serde_json::{Value, json};

/// `computer_actions` 接受的单步动作名（与执行侧共用同一份列表）。
pub const STEP_ACTIONS: &[&str] = &[
    "click",
    "move",
    "drag",
    "scroll",
    "type",
    "key",
    "wait",
    "screenshot",
    "paste",
    "focus",
    "clipboard",
    "invoke",
    "set_value",
    "get_value",
    "get_text",
    "select_text",
    "scroll_into_view",
    "toggle",
    "select",
    "expand",
    "collapse",
    "close",
    "minimize",
    "maximize",
    "restore",
    "click_text",
    "locate_text",
    "get_caret",
];

/// `computer_ui_action` 接受的动作名（单步工具的动作集）。
pub const UI_ACTIONS: &[&str] = &[
    "invoke",
    "set_value",
    "get_value",
    "get_text",
    "select_text",
    "scroll_into_view",
    "focus",
    "toggle",
    "select",
    "expand",
    "collapse",
    "close",
    "minimize",
    "maximize",
    "restore",
    "click_text",
    "locate_text",
    "get_caret",
];

/// 只读动作：不需要"正在驱动桌面"的记账，也可以安全重发。
pub const READ_ONLY_UI_ACTIONS: &[&str] = &["get_value", "get_text", "locate_text", "get_caret"];

/// 构造全部工具定义。
///
/// 14 个工具**统一**声明为 [`ToolExposure::Deferred`]：描述与参数 schema 平时不进模型上下文，
/// 由 `tool_search` 检索命中后才在下一轮进入工具表。这一族必须**一起出现、一起消失**——
/// 只放出其中一部分会让模型按半套能力规划动作（例如能截图却拿不到点击）。
pub fn tools() -> Vec<ExtensionTool> {
    vec![
        screenshot(),
        actions(),
        click(),
        drag(),
        scroll(),
        type_tool(),
        key(),
        windows(),
        ui(),
        ui_action(),
        clipboard(),
        paste(),
        launch(),
        stop(),
    ]
    .into_iter()
    .map(|tool| tool.with_exposure(ToolExposure::Deferred))
    .collect()
}

/// 共享的 `from_frame` 参数说明。
fn from_frame_param() -> Value {
    json!({
        "type": "string",
        "description": "A frame id from computer_screenshot. x and y are then pixels *in that image* and are projected for you; a frame whose window has moved or resized since the capture is refused rather than clicked."
    })
}

/// `computer_screenshot`。
fn screenshot() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_screenshot",
        "Capture the screen so you can see the desktop before deciding what to click. Returns the image plus capture geometry and the id of the frame (`frame`), so later coordinates can be given as pixels in this image. Use it first, and again after any action whose effect you cannot predict. On Wayland a window capture is a region of the screen: whatever covers the window is in the picture.",
        json!({
            "type": "object",
            "properties": {
                "window": { "type": "integer", "description": "Capture one window instead of the screen (id from computer_windows). Captured as that window's rectangle, so overlapping windows are included." },
                "display": { "type": "integer", "description": "Capture one display by its index in `displays` (0 is the primary display). Omit for the whole desktop." },
                "region": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "minItems": 4,
                    "maxItems": 4,
                    "description": "Capture an absolute rectangle as [x, y, width, height] in screen pixels."
                },
                "scale": { "type": "number", "description": "Downscale before encoding: 1 (default) keeps screen pixels, 0.5 halves both edges and roughly quarters the bytes. The reply reports the mapping so an image coordinate converts back to a click target." },
                "format": { "type": "string", "enum": ["png", "jpeg"], "description": "Image format; jpeg (default) is about a tenth of the bytes at a quality that keeps screen text readable." },
                "save_path": { "type": "string", "description": "Optional absolute path to also write the image to." }
            }
        }),
        "Capture the screen (or one window/region) and get the image plus a frame id for coordinate projection.",
    )
    .with_annotations(ToolAnnotations { read_only: true, destructive: false, idempotent: true })
}

/// `computer_actions`。
fn actions() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_actions",
        "Perform several computer actions in ONE call, optionally ending with a screenshot. Steps run in order; when one fails the earlier ones have already happened and the reply names the failing index. Use it whenever you already know the next few steps: one call instead of a chain, one round trip saved per step.",
        json!({
            "type": "object",
            "required": ["steps"],
            "properties": {
                "steps": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": 50,
                    "description": "The actions to perform, in order.",
                    "items": step_schema()
                },
                "screenshot": { "type": "boolean", "description": "Capture one screenshot after the last step, in the same call." },
                "scale": { "type": "number", "description": "Scale for the trailing screenshot (default 1)." }
            }
        }),
        "Run a list of desktop steps in one call (click/type/key/scroll/drag/UI actions), optionally followed by a screenshot.",
    )
    .with_annotations(ToolAnnotations { read_only: false, destructive: false, idempotent: false })
}

/// `computer_actions` 的单步 schema。
fn step_schema() -> Value {
    json!({
        "type": "object",
        "required": ["action"],
        "properties": {
            "action": { "type": "string", "enum": STEP_ACTIONS, "description": "What this step does." },
            "x": { "type": "integer", "description": "Screen x (or image x with from_frame)." },
            "y": { "type": "integer", "description": "Screen y (or image y with from_frame)." },
            "to_x": { "type": "integer", "description": "drag: end x." },
            "to_y": { "type": "integer", "description": "drag: end y." },
            "text": { "type": "string", "description": "type/paste/set_value: the text." },
            "keys": { "type": "string", "description": "key: chord such as ctrl+s." },
            "direction": { "type": "string", "enum": ["up", "down", "left", "right"], "description": "scroll: direction." },
            "clicks": { "type": "integer", "description": "scroll: wheel notches (1-20) or click: click count." },
            "button": { "type": "string", "description": "click: left (default), right or middle." },
            "seconds": { "type": "number", "description": "wait: how long to sleep (maximum 10)." },
            "ref": { "type": "string", "description": "UI actions: a ref from computer_ui (s1.4)." },
            "window": { "type": "integer", "description": "focus: window id to bring forward." },
            "op": { "type": "string", "enum": ["get", "set", "clear"], "description": "clipboard: operation." },
            "from_frame": from_frame_param(),
            "name": { "type": "string", "description": "click_text/locate_text: text to look for in the accessibility tree." },
            "start": { "type": "integer", "description": "select_text: start offset." },
            "end": { "type": "integer", "description": "select_text: end offset." },
            "index": { "type": "integer", "description": "select: child index." }
        }
    })
}

/// `computer_click`。
fn click() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_click",
        "Click at an absolute screen coordinate. Coordinates are screen pixels — the same units the capture envelope reports, and the units computer_windows prints.",
        json!({
            "type": "object",
            "required": ["x", "y"],
            "properties": {
                "x": { "type": "integer", "description": "Absolute screen x." },
                "y": { "type": "integer", "description": "Absolute screen y." },
                "button": { "type": "string", "description": "Mouse button: left (default), right or middle." },
                "clicks": { "type": "integer", "description": "1 (default), 2 for a double click, 3 for a triple click." },
                "modifiers": { "type": "string", "description": "A '+' separated chord held during the click, e.g. ctrl or shift." },
                "from_frame": from_frame_param()
            }
        }),
        "Click at an absolute screen coordinate (optionally with a modifier chord or as a double/triple click).",
    )
    .with_annotations(ToolAnnotations { read_only: false, destructive: false, idempotent: false })
}

/// `computer_drag`。
fn drag() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_drag",
        "Press the left mouse button at one coordinate, move, and release — for selecting text, moving windows, or dragging sliders. Give `path` instead of the four coordinates to drag through a shape.",
        json!({
            "type": "object",
            "properties": {
                "from_x": { "type": "integer", "description": "Start x (screen pixels, or image pixels with from_frame)." },
                "from_y": { "type": "integer", "description": "Start y." },
                "to_x": { "type": "integer", "description": "End x." },
                "to_y": { "type": "integer", "description": "End y." },
                "path": {
                    "type": "array",
                    "description": "Waypoints to drag through (2-64 points, same space as the coordinates). Excludes the four coordinates.",
                    "items": {
                        "type": "object",
                        "required": ["x", "y"],
                        "properties": {
                            "x": { "type": "integer" },
                            "y": { "type": "integer" }
                        }
                    }
                },
                "button": { "type": "string", "description": "Mouse button: left (default), right or middle." },
                "modifiers": { "type": "string", "description": "Modifiers held for the duration of the drag." },
                "from_frame": from_frame_param()
            }
        }),
        "Drag from one coordinate to another, optionally through waypoints.",
    )
    .with_annotations(ToolAnnotations { read_only: false, destructive: false, idempotent: false })
}

/// `computer_scroll`。
fn scroll() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_scroll",
        "Scroll the mouse wheel, optionally after moving the pointer over the surface to scroll.",
        json!({
            "type": "object",
            "required": ["direction"],
            "properties": {
                "direction": { "type": "string", "enum": ["up", "down", "left", "right"], "description": "Scroll direction." },
                "clicks": { "type": "integer", "description": "Wheel notches, 1 to 20 (default 3)." },
                "x": { "type": "integer", "description": "Move the pointer here first (screen x, or image x with from_frame)." },
                "y": { "type": "integer", "description": "Move the pointer here first." },
                "from_frame": from_frame_param()
            }
        }),
        "Scroll the wheel (optionally aiming it by moving the pointer to a coordinate first).",
    )
    .with_annotations(ToolAnnotations { read_only: false, destructive: false, idempotent: false })
}

/// `computer_type`。
fn type_tool() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_type",
        "Type text into whatever currently has keyboard focus, using Unicode input so any language works. Click the target field first. Newlines press Enter.",
        json!({
            "type": "object",
            "required": ["text"],
            "properties": {
                "text": { "type": "string", "description": "The exact text to type." },
                "into_window": { "type": "integer", "description": "Only send if this window (id from computer_windows) has the focus; a mismatch is refused before anything is typed." }
            }
        }),
        "Type text (Unicode, so Chinese works) into the focused control.",
    )
    .with_annotations(ToolAnnotations { read_only: false, destructive: false, idempotent: false })
}

/// `computer_key`。
fn key() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_key",
        "Press a key or key chord on the keyboard focus, e.g. 'enter', 'esc', 'tab', 'ctrl+c', 'ctrl+shift+t', 'alt+f4'.",
        json!({
            "type": "object",
            "required": ["keys"],
            "properties": {
                "keys": { "type": "string", "description": "Chord with '+' between keys, e.g. ctrl+s. Modifiers come first." },
                "into_window": { "type": "integer", "description": "Only send if this window (id from computer_windows) has the focus." },
                "repeat": { "type": "integer", "description": "How many times to press the chord, 1 to 100 (default 1). Ignored when hold_seconds is set." },
                "hold_seconds": { "type": "number", "description": "Press the chord down, hold it (maximum 10 s), then release." }
            }
        }),
        "Press a key or chord (enter, ctrl+c, alt+f4, …).",
    )
    .with_annotations(ToolAnnotations { read_only: false, destructive: false, idempotent: false })
}

/// `computer_windows`。
fn windows() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_windows",
        "List the visible top-level windows with their titles, application names and screen rectangles, or bring one to the foreground. Prefer this over guessing from a screenshot when you need to switch applications.",
        json!({
            "type": "object",
            "properties": {
                "filter": { "type": "string", "description": "Case-insensitive substring matched against the window title or application name." },
                "focus": { "type": "integer", "description": "Window id from a previous listing to activate and bring to the foreground." }
            }
        }),
        "List visible windows (title, app, rectangle) or focus one by id.",
    )
    .with_annotations(ToolAnnotations { read_only: true, destructive: false, idempotent: true })
}

/// `computer_ui`。
fn ui() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_ui",
        "Read the accessibility tree of a window: roles, accessible names, ids, rectangles, states and the actions each node supports. This is the authoritative view of a native window and much cheaper than screenshot-plus-clicking. It only shows what the application exposes — an app without accessibility enabled returns an error saying so.",
        json!({
            "type": "object",
            "properties": {
                "window": { "type": "integer", "description": "Window id from computer_windows. Omit to read the focused window." },
                "filter": { "type": "string", "description": "Case-insensitive substring matched against name, id and role; matching nodes and their ancestors are returned." },
                "max_depth": { "type": "integer", "description": "Tree depth to walk, 1 to 10 (default 5)." },
                "max_nodes": { "type": "integer", "description": "Node budget, 10 to 1500 (default 250)." },
                "max_visited": { "type": "integer", "description": "Traversal budget: how many nodes the walk may look at (default derived from max_nodes)." },
                "patterns": { "type": "boolean", "description": "Probe which actions each node supports (default true; false is faster)." },
                "skip_empty": { "type": "boolean", "description": "Omit anonymous containers with no id, actions or rectangle (default true)." },
                "compact": { "type": "boolean", "description": "Rows carry ref, role, name, states and actions by default; pass false to also get geometry." },
                "screenshot": { "type": "boolean", "description": "Return a screenshot of the same window in the same call." }
            }
        }),
        "Read a window's accessibility tree with refs, roles, names, states and supported actions.",
    )
    .with_annotations(ToolAnnotations { read_only: true, destructive: false, idempotent: true })
}

/// `computer_ui_action`。
fn ui_action() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_ui_action",
        "Act on a window through its accessibility interface instead of moving the mouse: invoke a button, set an edit field's value, read it back, select text, toggle a checkbox, select a list item, expand or collapse, focus a control, or click/locate a node by its text. Far more reliable than clicking pixels and it works without stealing the user's focus. Use computer_ui first to get a ref.",
        json!({
            "type": "object",
            "required": ["action"],
            "properties": {
                "action": {
                    "type": "string",
                    "enum": UI_ACTIONS,
                    "description": "One of the accessibility actions. get_value/get_text/locate_text/get_caret are reads and safe to repeat."
                },
                "ref": { "type": "string", "description": "The [s1.4] marker computer_ui printed next to a node. The node is re-read and matched by name and role; a ref from an older read is refused with a stale-ref error rather than guessed." },
                "window": { "type": "integer", "description": "Window id; used when no ref is given (or for the window actions close/minimize/maximize/restore)." },
                "name": { "type": "string", "description": "click_text/locate_text: text to search for in the tree." },
                "text": { "type": "string", "description": "set_value: the text to write." },
                "start": { "type": "integer", "description": "select_text: start character offset (default 0)." },
                "end": { "type": "integer", "description": "select_text: end offset (-1 means end of text)." },
                "index": { "type": "integer", "description": "select: child index (default 0)." }
            }
        }),
        "Act through the accessibility tree: invoke, set/get value or text, select, toggle, expand/collapse, focus, close/minimize/maximize.",
    )
    .with_annotations(ToolAnnotations { read_only: false, destructive: false, idempotent: false })
}

/// `computer_clipboard`。
fn clipboard() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_clipboard",
        "Read the system clipboard, write text to it, or clear it. This is the reliable way to get text *out* of an application: select it, copy it with computer_key, then read it here.",
        json!({
            "type": "object",
            "required": ["op"],
            "properties": {
                "op": { "type": "string", "enum": ["get", "set", "clear"], "description": "get returns the current text, set replaces it with `text`, clear empties it." },
                "text": { "type": "string", "description": "For op=set: the text to put on the clipboard." }
            }
        }),
        "Read, write or clear the system clipboard.",
    )
    .with_annotations(ToolAnnotations { read_only: false, destructive: false, idempotent: false })
}

/// `computer_paste`。
fn paste() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_paste",
        "Put text into the focused control by pasting rather than typing: typing sends each character as a synthesized keystroke, which some editors and IME-hostile apps mangle, while a paste is one atomic gesture. The user's clipboard is borrowed and put back, and `restore` says what happened.",
        json!({
            "type": "object",
            "required": ["text"],
            "properties": {
                "text": { "type": "string", "description": "The text to paste." },
                "restore": { "type": "boolean", "description": "Put the previous clipboard content back afterwards (default true)." },
                "require_restore": { "type": "boolean", "description": "Refuse before writing anything when the previous clipboard cannot be restored." }
            }
        }),
        "Paste text into the focused control, borrowing and restoring the clipboard.",
    )
    .with_annotations(ToolAnnotations { read_only: false, destructive: false, idempotent: false })
}

/// `computer_launch`。
fn launch() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_launch",
        "Start an application and return the window it opened, so the next call can act on it. `target` is an executable path or a name on PATH. The app's own stdout/stderr go to a log file named in the result (never to the terminal); when no window appears, the result ends with that log's tail, which usually says why it failed.",
        json!({
            "type": "object",
            "required": ["target"],
            "properties": {
                "target": { "type": "string", "description": "Executable path or command name, e.g. /usr/bin/gedit or firefox." },
                "arguments": { "type": "array", "items": { "type": "string" }, "description": "Optional command-line arguments." },
                "wait_seconds": { "type": "number", "description": "How long to wait for a window (default 8, maximum 30)." }
            }
        }),
        "Start an application and return the window it opened.",
    )
    .with_annotations(ToolAnnotations { read_only: false, destructive: false, idempotent: false })
}

/// `computer_stop`。
fn stop() -> ExtensionTool {
    ExtensionTool::simple(
        "computer_stop",
        "Release any mouse button or key this session is still holding down. Use it when a gesture was interrupted — a drag that never got its release leaves the desktop holding a button the user cannot shake off.",
        json!({
            "type": "object",
            "properties": {
                "release_input": { "type": "boolean", "description": "Release anything still held down (default true)." }
            }
        }),
        "Release held keys/buttons.",
    )
    .with_annotations(ToolAnnotations { read_only: true, destructive: false, idempotent: true })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn all_fourteen_tools_are_declared_once() {
        let tools = tools();
        assert_eq!(tools.len(), 14);
        let names: HashSet<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names.len(), 14);
        assert!(names.iter().all(|name| name.starts_with("computer_")));
    }

    /// 整族工具必须同为延迟工具：一半可见一半不可见会让模型按半套能力规划动作。
    #[test]
    fn every_tool_is_deferred() {
        for tool in tools() {
            assert_eq!(
                tool.exposure,
                ToolExposure::Deferred,
                "{} 必须声明为延迟工具",
                tool.name
            );
        }
    }

    #[test]
    fn step_actions_cover_every_ui_action() {
        for action in UI_ACTIONS {
            assert!(STEP_ACTIONS.contains(action), "{action} missing from steps");
        }
    }

    #[test]
    fn read_only_ui_actions_are_a_subset() {
        for action in READ_ONLY_UI_ACTIONS {
            assert!(UI_ACTIONS.contains(action));
        }
    }
}

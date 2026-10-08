//! 键位绑定
//!
//! - 动作 id 命名空间：`tui.editor.*` / `tui.input.*` / `tui.select.*` / `app.*`
//! - 用户配置：`agent_dir()/keybindings.json`，
//!   格式: `{ "app.clear": "ctrl+c", "tui.editor.cursorUp": ["up", "alt+k"] }`，空数组禁用该动作
//! - 键格式：`modifier+key`（`ctrl`/`shift`/`alt`/`super` 可组合；键 = 字母/数字/符号/特殊键）
//! - [`KeybindingsManager::matches`]：crossterm `KeyEvent` ⇢ 动作 id（支持 kitty 键盘协议下的大小写 / Shift 归一）

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::Value;
use std::{
    collections::HashMap,
    path::Path,
    sync::atomic::{AtomicPtr, Ordering},
};

// 全局键位表：AtomicPtr 保证 &'static 读取接口不变，同时支持 /reload 替换
//（OnceLock 无法二次 set）。替换时旧值刻意泄漏——可能仍有线程持有 &'static
// 引用，直接 drop 是 UB；reload 频次低、对象小，泄漏可接受。
/// 进程级全局键位表指针：以 AtomicPtr 存放，/reload 时整体替换而非原地修改。
static GLOBAL: AtomicPtr<KeybindingsManager> = AtomicPtr::new(std::ptr::null_mut());

/// 基础键：字母/数字/符号/特殊键/F 键
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeyBase {
    /// 普通字符键，保留配置里的原始大小写（如 `a`、`A`、`2`、`@`）。
    Char(char),
    /// Esc 键：取消当前输入或退出当前界面。
    Escape,
    /// 回车键：确认提交或换行。
    Enter,
    /// 制表键：切换焦点或补全输入。
    Tab,
    /// 空格键：插入空格或触发默认动作。
    Space,
    /// 退格键：删除光标前一个字符。
    Backspace,
    /// Delete 键：删除光标处（向后）的字符。
    Delete,
    /// Insert 键：切换插入/覆盖模式。
    Insert,
    /// Home 键：把光标移到行首。
    Home,
    /// End 键：把光标移到行尾。
    End,
    /// PageUp 键：向上翻页。
    PageUp,
    /// PageDown 键：向下翻页。
    PageDown,
    /// 方向键上：光标上移。
    Up,
    /// 方向键下：光标下移。
    Down,
    /// 方向键左：光标左移。
    Left,
    /// 方向键右：光标右移。
    Right,
    /// 功能键 F1~F24，内层数字即键号。
    F(u8),
}

/// 解析后的键 id
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyId {
    /// 主键：不含任何修饰符的基础键。
    base: KeyBase,
    /// 是否要求按住 Ctrl。
    ctrl: bool,
    /// 是否要求按住 Shift。
    shift: bool,
    /// 是否要求按住 Alt。
    alt: bool,
    /// 是否要求按住 Super（macOS 的 Cmd / Windows 的 Win）。
    super_: bool,
}

/// 取已初始化的全局实例；未初始化会触发 debug 断言（只在已知已初始化处调用）。
fn current() -> &'static KeybindingsManager {
    let ptr = GLOBAL.load(Ordering::Acquire);
    debug_assert!(!ptr.is_null(), "keybindings global not initialized");
    unsafe { &*ptr }
}

/// 首次初始化全局实例；若已被其他线程抢先初始化，则释放自建实例并返回现存者。
fn install(mgr: KeybindingsManager) -> &'static KeybindingsManager {
    let raw = Box::into_raw(Box::new(mgr));
    match GLOBAL.compare_exchange(
        std::ptr::null_mut(),
        raw,
        Ordering::AcqRel,
        Ordering::Acquire,
    ) {
        Ok(_) => unsafe { &*raw },
        Err(_) => {
            // 已被其他线程初始化：释放自己构造的实例，返回现存者
            unsafe { drop(Box::from_raw(raw)) };
            current()
        }
    }
}

/// 启动时初始化全局 keybindings（读取 agent_dir/keybindings.json）；
/// 通常由 main 在进入任意模式前调用一次。重复调用只生效第一次。
pub fn init_global(agent_dir: &Path) -> &'static KeybindingsManager {
    install(KeybindingsManager::load(agent_dir))
}

/// 获取全局 keybindings；未初始化时使用默认绑定
pub fn get_global() -> &'static KeybindingsManager {
    let ptr = GLOBAL.load(Ordering::Acquire);
    if ptr.is_null() {
        install(KeybindingsManager::new_default())
    } else {
        unsafe { &*ptr }
    }
}

/// 覆盖全局（/reload 与测试场景重复调用替换，等价重新读取磁盘配置）
pub fn set_global(mgr: KeybindingsManager) -> &'static KeybindingsManager {
    let raw = Box::into_raw(Box::new(mgr));
    let prev = GLOBAL.swap(raw, Ordering::AcqRel);
    if !prev.is_null() {
        // 旧值泄漏（见 GLOBAL 注释）：不能回收
        std::mem::forget(unsafe { Box::from_raw(prev) });
    }
    unsafe { &*raw }
}

/// US 布局 Shift 层映射（与 line_input::shift_layer 同表；keybindings 匹配终端
/// 转换后的大写/符号形态时使用）
fn shift_upper(c: char) -> char {
    match c {
        '1' => '!',
        '2' => '@',
        '3' => '#',
        '4' => '$',
        '5' => '%',
        '6' => '^',
        '7' => '&',
        '8' => '*',
        '9' => '(',
        '0' => ')',
        '`' => '~',
        '-' => '_',
        '=' => '+',
        '[' => '{',
        ']' => '}',
        '\\' => '|',
        ';' => ':',
        '\'' => '"',
        ',' => '<',
        '.' => '>',
        '/' => '?',
        c if c.is_ascii_lowercase() => c.to_ascii_uppercase(),
        c => c,
    }
}

/// 解析主键名（"escape"/"f1"/单字符…）；无法识别时返回 `None`。
fn parse_base(s: &str) -> Option<KeyBase> {
    use KeyBase::*;

    Some(match s.to_ascii_lowercase().as_str() {
        "escape" | "esc" => Escape,
        "enter" | "return" => Enter,
        "tab" => Tab,
        "space" => Space,
        "backspace" => Backspace,
        "delete" => Delete,
        "insert" => Insert,
        "home" => Home,
        "end" => End,
        "pageup" => PageUp,
        "pagedown" => PageDown,
        "up" => Up,
        "down" => Down,
        "left" => Left,
        "right" => Right,
        "f1" => F(1),
        "f2" => F(2),
        "f3" => F(3),
        "f4" => F(4),
        "f5" => F(5),
        "f6" => F(6),
        "f7" => F(7),
        "f8" => F(8),
        "f9" => F(9),
        "f10" => F(10),
        "f11" => F(11),
        "f12" => F(12),
        other if other.chars().count() == 1 => {
            let c = other.chars().next().unwrap();
            // 符号键的 Shift 形态（terminal 转换产物，如 shift+2 → '@'）可直接作为 base
            if c.is_ascii_graphic() || c == ' ' {
                Char(c)
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

/// 解析键 id 字符串（"ctrl+shift+p"、"escape"、"shift+2"…）。
/// 无效的键 id 返回 None（用户配置中的无效项被忽略）。
pub fn parse_key_id(s: &str) -> Option<KeyId> {
    let parts: Vec<&str> = s.split('+').collect();
    let base_str = parts.last().copied()?;
    let base = parse_base(base_str)?;
    Some(KeyId {
        base,
        ctrl: parts.contains(&"ctrl"),
        shift: parts.contains(&"shift"),
        alt: parts.contains(&"alt"),
        super_: parts.contains(&"super"),
    })
}

/// 取出事件修饰位，返回 (ctrl, shift, alt, super)。
fn mods(ev: &KeyEvent) -> (bool, bool, bool, bool) {
    let m = ev.modifiers;
    (
        m.contains(KeyModifiers::CONTROL),
        m.contains(KeyModifiers::SHIFT),
        m.contains(KeyModifiers::ALT),
        m.contains(KeyModifiers::SUPER),
    )
}

/// 事件 ⇢ 键 id 匹配。
///
/// 归一规则（适配 crossterm KeyEvent）：
/// - 字母键：kitty 协议下 shift+a → `Char('a')+SHIFT`；终端也可能转换后上报
///   `Char('A')`（无 SHIFT）。两种形态都匹配 `shift+a`；`a` 只匹配无修饰小写。
/// - ctrl/alt+字母：大小写归一（ctrl+p 可能是 `Char('p')` 或 `Char('P')`），
///   但 shift 位严格区分（避免 ctrl+shift+p 误命中 ctrl+p）。
/// - 数字/符号：`shift+2` 匹配 `Char('2')+SHIFT`（kitty raw）或 `Char('@')`（转换）。
pub fn matches_event(ev: &KeyEvent, key_id: &KeyId) -> bool {
    use KeyBase::*;

    let (has_ctrl, has_shift, has_alt, has_super) = mods(ev);
    if has_super != key_id.super_ {
        return false;
    }

    match key_id.base {
        Char(base) => {
            let KeyCode::Char(actual) = ev.code else {
                return false;
            };
            if has_ctrl != key_id.ctrl || has_alt != key_id.alt {
                return false;
            }
            if base.is_ascii_alphabetic() {
                if !actual.eq_ignore_ascii_case(&base) {
                    return false;
                }
                if key_id.ctrl || key_id.alt {
                    // 组合修饰：shift 位严格区分
                    return has_shift == key_id.shift;
                }
                if key_id.shift {
                    // kitty raw（小写+SHIFT）或终端转换（大写无 SHIFT）
                    return (has_shift && actual.is_ascii_lowercase())
                        || (!has_shift && actual.is_ascii_uppercase());
                }
                return !has_shift && actual.is_ascii_lowercase();
            }
            // 数字/符号
            if key_id.shift {
                let shifted = shift_upper(base);
                return (has_shift && actual == base) || (!has_shift && actual == shifted);
            }
            !has_shift && actual == base
        }
        Escape => ev.code == KeyCode::Esc && !has_ctrl && !has_shift && !has_alt,
        Enter => {
            ev.code == KeyCode::Enter
                && has_shift == key_id.shift
                && has_ctrl == key_id.ctrl
                && has_alt == key_id.alt
        }
        Tab => {
            // shift+tab：crossterm 惯例 KeyCode::BackTab（无修饰），或 kitty 的 Tab+SHIFT
            if key_id.shift {
                matches!(ev.code, KeyCode::BackTab) || (ev.code == KeyCode::Tab && has_shift)
            } else {
                ev.code == KeyCode::Tab && !has_shift && !has_ctrl && !has_alt
            }
        }
        Space => {
            ev.code == KeyCode::Char(' ') && !has_ctrl && !has_alt && !has_shift && !key_id.shift
        }
        Backspace => {
            ev.code == KeyCode::Backspace
                && has_shift == key_id.shift
                && has_ctrl == key_id.ctrl
                && has_alt == key_id.alt
        }
        Delete => {
            ev.code == KeyCode::Delete
                && has_shift == key_id.shift
                && has_ctrl == key_id.ctrl
                && has_alt == key_id.alt
        }
        Insert => ev.code == KeyCode::Insert && !has_shift && !has_ctrl && !has_alt,
        Home => {
            ev.code == KeyCode::Home
                && has_shift == key_id.shift
                && has_ctrl == key_id.ctrl
                && has_alt == key_id.alt
        }
        End => {
            ev.code == KeyCode::End
                && has_shift == key_id.shift
                && has_ctrl == key_id.ctrl
                && has_alt == key_id.alt
        }
        PageUp => {
            ev.code == KeyCode::PageUp
                && has_shift == key_id.shift
                && has_ctrl == key_id.ctrl
                && has_alt == key_id.alt
        }
        PageDown => {
            ev.code == KeyCode::PageDown
                && has_shift == key_id.shift
                && has_ctrl == key_id.ctrl
                && has_alt == key_id.alt
        }
        Up => {
            ev.code == KeyCode::Up
                && has_shift == key_id.shift
                && has_ctrl == key_id.ctrl
                && has_alt == key_id.alt
        }
        Down => {
            ev.code == KeyCode::Down
                && has_shift == key_id.shift
                && has_ctrl == key_id.ctrl
                && has_alt == key_id.alt
        }
        Left => {
            ev.code == KeyCode::Left
                && has_shift == key_id.shift
                && has_ctrl == key_id.ctrl
                && has_alt == key_id.alt
        }
        Right => {
            ev.code == KeyCode::Right
                && has_shift == key_id.shift
                && has_ctrl == key_id.ctrl
                && has_alt == key_id.alt
        }
        F(n) => matches!(ev.code, KeyCode::F(k) if k == n) && !has_shift && !has_ctrl && !has_alt,
    }
}

/// 单条键位绑定的定义：动作 id、默认按键列表与展示用描述。
pub struct KeybindingDef {
    /// 动作 id，命名空间如 `tui.editor.*`、`app.*`。
    pub id: &'static str,
    /// 默认按键字符串列表，空数组表示该动作默认禁用。
    pub default_keys: &'static [&'static str],
    /// 展示给用户看的动作说明（英文）。
    pub description: &'static str,
}

/// 默认键位表：未列入的动作（altScreen.* / tree.* / models.* / warnings 等）无对应实现，跳过。
pub const KEYBINDINGS: &[KeybindingDef] = &[
    // -- tui.editor.*：编辑器移动 / 编辑） --
    KeybindingDef {
        id: "tui.editor.cursorUp",
        default_keys: &["up"],
        description: "Move cursor up",
    },
    KeybindingDef {
        id: "tui.editor.cursorDown",
        default_keys: &["down"],
        description: "Move cursor down",
    },
    KeybindingDef {
        id: "tui.editor.historyPrevious",
        default_keys: &[],
        description: "Select previous prompt history entry",
    },
    KeybindingDef {
        id: "tui.editor.historyNext",
        default_keys: &[],
        description: "Select next prompt history entry",
    },
    KeybindingDef {
        id: "tui.editor.cursorLeft",
        default_keys: &["left", "ctrl+b"],
        description: "Move cursor left",
    },
    KeybindingDef {
        id: "tui.editor.cursorRight",
        default_keys: &["right", "ctrl+f"],
        description: "Move cursor right",
    },
    KeybindingDef {
        id: "tui.editor.cursorWordLeft",
        default_keys: &["alt+left", "ctrl+left", "alt+b"],
        description: "Move cursor word left",
    },
    KeybindingDef {
        id: "tui.editor.cursorWordRight",
        default_keys: &["alt+right", "ctrl+right", "alt+f"],
        description: "Move cursor word right",
    },
    KeybindingDef {
        id: "tui.editor.cursorLineStart",
        default_keys: &["home", "ctrl+home", "ctrl+a"],
        description: "Move to line start",
    },
    KeybindingDef {
        id: "tui.editor.cursorLineEnd",
        default_keys: &["end", "ctrl+end", "ctrl+e"],
        description: "Move to line end",
    },
    KeybindingDef {
        id: "tui.editor.pageUp",
        default_keys: &["pageUp", "ctrl+pageUp"],
        description: "Page up",
    },
    KeybindingDef {
        id: "tui.editor.pageDown",
        default_keys: &["pageDown", "ctrl+pageDown"],
        description: "Page down",
    },
    KeybindingDef {
        id: "tui.editor.deleteCharBackward",
        default_keys: &["backspace", "shift+backspace"],
        description: "Delete character backward",
    },
    KeybindingDef {
        id: "tui.editor.deleteCharForward",
        default_keys: &["delete", "shift+delete", "ctrl+d"],
        description: "Delete character forward",
    },
    KeybindingDef {
        id: "tui.editor.deleteWordBackward",
        default_keys: &["ctrl+w", "alt+backspace", "ctrl+backspace"],
        description: "Delete word backward",
    },
    KeybindingDef {
        id: "tui.editor.deleteWordForward",
        default_keys: &["alt+d", "alt+delete"],
        description: "Delete word forward",
    },
    KeybindingDef {
        id: "tui.editor.deleteToLineStart",
        default_keys: &["ctrl+u"],
        description: "Delete to line start",
    },
    KeybindingDef {
        id: "tui.editor.deleteToLineEnd",
        default_keys: &["ctrl+k"],
        description: "Delete to line end",
    },
    KeybindingDef {
        id: "tui.editor.yank",
        default_keys: &["ctrl+y"],
        description: "Yank",
    },
    KeybindingDef {
        id: "tui.editor.yankPop",
        default_keys: &["alt+y", "alt+shift+y"],
        description: "Yank previous kill-ring entry",
    },
    KeybindingDef {
        id: "tui.editor.undo",
        default_keys: &["ctrl+-", "ctrl+shift+-"],
        description: "Undo",
    },
    // -- tui.input.* --
    KeybindingDef {
        id: "tui.input.newLine",
        default_keys: &["shift+enter", "ctrl+j"],
        description: "Insert newline",
    },
    KeybindingDef {
        id: "tui.input.submit",
        default_keys: &["enter"],
        description: "Submit input",
    },
    KeybindingDef {
        id: "tui.input.tab",
        default_keys: &["tab"],
        description: "Tab / autocomplete",
    },
    KeybindingDef {
        id: "tui.input.copy",
        default_keys: &["ctrl+shift+c"],
        description: "Copy selection",
    },
    // -- tui.select.* -- 选择面板相关操作
    KeybindingDef {
        id: "tui.select.up",
        default_keys: &["up"],
        description: "Move selection up",
    },
    KeybindingDef {
        id: "tui.select.down",
        default_keys: &["down"],
        description: "Move selection down",
    },
    KeybindingDef {
        id: "tui.select.first",
        default_keys: &["home"],
        description: "Select first item",
    },
    KeybindingDef {
        id: "tui.select.last",
        default_keys: &["end"],
        description: "Select last item",
    },
    KeybindingDef {
        id: "tui.select.pageUp",
        default_keys: &["pageUp"],
        description: "Selection page up",
    },
    KeybindingDef {
        id: "tui.select.pageDown",
        default_keys: &["pageDown"],
        description: "Selection page down",
    },
    KeybindingDef {
        id: "tui.select.confirm",
        default_keys: &["enter"],
        description: "Confirm selection",
    },
    KeybindingDef {
        id: "tui.select.cancel",
        default_keys: &["escape", "ctrl+c"],
        description: "Cancel selection",
    },
    // -- app.* --
    KeybindingDef {
        id: "app.interrupt",
        default_keys: &["escape"],
        description: "Cancel or abort",
    },
    KeybindingDef {
        id: "app.clear",
        default_keys: &["ctrl+c"],
        description: "Clear editor",
    },
    KeybindingDef {
        id: "app.exit",
        default_keys: &["ctrl+d"],
        description: "Exit when editor is empty",
    },
    KeybindingDef {
        id: "app.suspend",
        default_keys: &["ctrl+z"],
        description: "Suspend to background",
    },
    KeybindingDef {
        id: "app.thinking.cycle",
        default_keys: &["shift+tab"],
        description: "Cycle thinking level",
    },
    KeybindingDef {
        id: "app.model.cycleForward",
        default_keys: &["ctrl+p"],
        description: "Cycle to next model",
    },
    KeybindingDef {
        id: "app.model.cycleBackward",
        default_keys: &["shift+ctrl+p"],
        description: "Cycle to previous model",
    },
    KeybindingDef {
        id: "app.model.select",
        default_keys: &["ctrl+l"],
        description: "Open model selector",
    },
    KeybindingDef {
        id: "app.tools.expand",
        default_keys: &["ctrl+o"],
        description: "Toggle tool output",
    },
    KeybindingDef {
        id: "app.thinking.toggle",
        default_keys: &["ctrl+t"],
        description: "Toggle thinking blocks",
    },
    KeybindingDef {
        id: "app.theme.cycle",
        default_keys: &["ctrl+shift+t"],
        description: "Cycle to next theme",
    },
    KeybindingDef {
        id: "app.editor.external",
        default_keys: &["ctrl+g"],
        description: "Open external editor",
    },
    KeybindingDef {
        id: "app.message.copy",
        default_keys: &["ctrl+x"],
        description: "Copy message to clipboard",
    },
    KeybindingDef {
        id: "app.editor.copy",
        default_keys: &["alt+c"],
        description: "Copy entire editor content to clipboard",
    },
    KeybindingDef {
        id: "app.message.followUp",
        default_keys: &["alt+enter"],
        description: "Queue follow-up message",
    },
    KeybindingDef {
        id: "app.message.dequeue",
        default_keys: &["alt+up"],
        description: "Restore queued messages",
    },
    KeybindingDef {
        id: "app.message.scrollToTop",
        default_keys: &["shift+home"],
        description: "Scroll transcript to top",
    },
    KeybindingDef {
        id: "app.message.scrollToBottom",
        default_keys: &["shift+end"],
        description: "Scroll transcript to bottom",
    },
    KeybindingDef {
        id: "app.message.pageUp",
        default_keys: &["shift+pageup"],
        description: "Scroll transcript up one page",
    },
    KeybindingDef {
        id: "app.message.pageDown",
        default_keys: &["shift+pagedown"],
        description: "Scroll transcript down one page",
    },
    KeybindingDef {
        id: "app.clipboard.pasteImage",
        default_keys: &["ctrl+v"],
        description: "Paste image from clipboard (text fallback)",
    },
    // -- app.session.* --
    KeybindingDef {
        id: "app.session.toggleNamedFilter",
        default_keys: &["ctrl+n"],
        description: "Toggle named session filter",
    },
    KeybindingDef {
        id: "app.session.togglePath",
        default_keys: &["ctrl+p"],
        description: "Toggle path display",
    },
    KeybindingDef {
        id: "app.session.toggleSort",
        default_keys: &["ctrl+s"],
        description: "Toggle sort mode",
    },
    KeybindingDef {
        id: "app.session.rename",
        default_keys: &["ctrl+r"],
        description: "Rename session",
    },
    KeybindingDef {
        id: "app.session.delete",
        default_keys: &["ctrl+d"],
        description: "Delete session",
    },
    KeybindingDef {
        id: "app.session.deleteNoninvasive",
        default_keys: &["ctrl+backspace"],
        description: "Delete session when query is empty",
    },
    KeybindingDef {
        id: "app.session.next",
        default_keys: &["ctrl+g"],
        description: "Jump to next session after current",
    },
];

/// 单键冲突（同一物理键被多个动作占用）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// 被多个动作同时占用的物理键字符串。
    pub key: String,
    /// 占用该键的全部动作 id。
    pub keybindings: Vec<String>,
}

/// user 覆盖 default + 冲突检测
pub struct KeybindingsManager {
    /// action id → 解析后的键列表（用户配置覆盖默认；空 = 禁用）
    keys: HashMap<&'static str, Vec<KeyId>>,
    /// 用户配置中的键冲突（同一 key 绑定多个动作）
    conflicts: Vec<Conflict>,
}

impl KeybindingsManager {
    /// 仅默认绑定（无用户配置）
    pub fn new_default() -> Self {
        Self::from_user(&HashMap::new())
    }

    /// 加载默认 + 用户配置（agent_dir/keybindings.json）
    pub fn load(agent_dir: &Path) -> Self {
        Self::from_user(&load_user_config(agent_dir))
    }

    /// 由用户配置构建：用户覆盖默认，空数组禁用，非法键 id 忽略，冲突记录。
    fn from_user(user: &HashMap<String, Vec<String>>) -> Self {
        let mut conflicts: Vec<Conflict> = Vec::new();
        // 用户声明：key → 动作列表（仅已知动作 id 参与冲突检测）
        let mut user_claims: HashMap<String, Vec<String>> = HashMap::new();
        for def in KEYBINDINGS {
            let Some(keys) = user.get(def.id) else {
                continue;
            };
            for key in keys {
                user_claims
                    .entry(key.clone())
                    .or_default()
                    .push(def.id.to_string());
            }
        }
        for (key, claimed) in user_claims {
            if claimed.len() > 1 {
                conflicts.push(Conflict {
                    key,
                    keybindings: claimed,
                });
            }
        }

        let mut keys: HashMap<&'static str, Vec<KeyId>> = HashMap::new();
        for def in KEYBINDINGS {
            let raw: Vec<String> = match user.get(def.id) {
                Some(list) => list.clone(),
                None => def.default_keys.iter().map(|s| s.to_string()).collect(),
            };
            let parsed = raw.iter().filter_map(|k| parse_key_id(k)).collect();
            keys.insert(def.id, parsed);
        }
        Self { keys, conflicts }
    }

    /// 事件命中动作（任一绑定键匹配即 true）
    pub fn matches(&self, ev: &KeyEvent, id: &str) -> bool {
        let Some(keys) = self.keys.get(id) else {
            return false;
        };
        keys.iter().any(|k| matches_event(ev, k))
    }

    /// 动作当前绑定的键（解析后的键 id 列表，供提示/测试用）
    pub fn keys_for(&self, id: &str) -> &[KeyId] {
        self.keys.get(id).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// 用户配置冲突
    pub fn conflicts(&self) -> &[Conflict] {
        &self.conflicts
    }

    /// 动作首个绑定键的展示文本（`"ctrl+x"`、`"shift+tab"`）；未绑定时为 `None`。
    pub fn display_for(&self, id: &str) -> Option<String> {
        let key = self.keys_for(id).first()?;
        let mut parts: Vec<&str> = Vec::new();
        if key.ctrl {
            parts.push("ctrl");
        }
        if key.alt {
            parts.push("alt");
        }
        if key.super_ {
            parts.push("super");
        }
        if key.shift {
            parts.push("shift");
        }

        let base = base_display(key.base);
        parts.push(&base);
        Some(parts.join("+"))
    }
}

/// 基础键的展示名（`ctrl+x` 里的 `x`、`escape`、`f5`）。
fn base_display(base: KeyBase) -> String {
    use KeyBase::*;
    match base {
        Char(c) => c.to_string(),
        Escape => "escape".to_string(),
        Enter => "enter".to_string(),
        Tab => "tab".to_string(),
        Space => "space".to_string(),
        Backspace => "backspace".to_string(),
        Delete => "delete".to_string(),
        Insert => "insert".to_string(),
        Home => "home".to_string(),
        End => "end".to_string(),
        PageUp => "pageup".to_string(),
        PageDown => "pagedown".to_string(),
        Up => "up".to_string(),
        Down => "down".to_string(),
        Left => "left".to_string(),
        Right => "right".to_string(),
        F(n) => format!("f{n}"),
    }
}

/// 全局键位表里某动作首个绑定键的展示文本（`"ctrl+x"`），供 TUI 提示行用。
///
/// 未绑定该动作时返回 `None`；用户改过键位后提示显示真实绑定值。
pub fn key_display(id: &str) -> Option<String> {
    get_global().display_for(id)
}

/// 读取 agent_dir/keybindings.json（`{ actionId: key | key[] }`）
pub fn load_user_config(agent_dir: &Path) -> HashMap<String, Vec<String>> {
    let path = agent_dir.join("keybindings.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return HashMap::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return HashMap::new();
    };
    let Some(obj) = v.as_object() else {
        return HashMap::new();
    };
    let mut out: HashMap<String, Vec<String>> = HashMap::new();
    for (id, val) in obj {
        match val {
            Value::String(s) => {
                out.insert(id.clone(), vec![s.clone()]);
            }
            Value::Array(arr) => {
                let keys: Vec<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_string()))
                    .collect();
                out.insert(id.clone(), keys);
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    /// 提示行用的键位展示文本：按绑定顺序取第一个，修饰符按 ctrl/alt/super/shift 组合。
    #[test]
    fn display_for_renders_the_first_binding() {
        let mgr = KeybindingsManager::new_default();
        let shown = mgr.display_for("app.message.copy").unwrap();
        assert_eq!(shown, "ctrl+x");
        assert!(mgr.display_for("no.such.action").is_none());

        // 特殊键与多修饰符
        let mgr = KeybindingsManager {
            keys: HashMap::from([
                ("demo.special", vec![parse_key_id("shift+pageup").unwrap()]),
                ("demo.fkey", vec![parse_key_id("ctrl+alt+f5").unwrap()]),
                (
                    "demo.multi",
                    vec![
                        parse_key_id("ctrl+j").unwrap(),
                        parse_key_id("ctrl+k").unwrap(),
                    ],
                ),
            ]),
            conflicts: Vec::new(),
        };
        assert_eq!(mgr.display_for("demo.special").unwrap(), "shift+pageup");
        assert_eq!(mgr.display_for("demo.fkey").unwrap(), "ctrl+alt+f5");
        assert_eq!(mgr.display_for("demo.multi").unwrap(), "ctrl+j");
    }

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }
    fn ctrl(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::CONTROL)
    }
    fn shift(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::SHIFT)
    }
    fn alt(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::ALT)
    }
    fn plain(c: char) -> KeyEvent {
        key(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn id(s: &str) -> KeyId {
        parse_key_id(s).expect("valid key id")
    }

    #[test]
    fn parses_modifier_combinations() {
        let k = id("ctrl+shift+p");
        assert_eq!(k.base, KeyBase::Char('p'));
        assert!(k.ctrl && k.shift && !k.alt && !k.super_);

        assert_eq!(id("escape").base, KeyBase::Escape);
        assert_eq!(id("shift+tab").base, KeyBase::Tab);
        assert!(id("shift+tab").shift);
        assert_eq!(id("ctrl+-").base, KeyBase::Char('-'));
        assert!(id("ctrl+-").ctrl);

        assert!(parse_key_id("bogus-key").is_none());
        assert!(parse_key_id("super+k").unwrap().super_);
    }

    #[test]
    fn matches_plain_and_modified_chars() {
        let m = KeybindingsManager::new_default();
        assert!(!m.matches(&plain('a'), "tui.editor.cursorWordLeft"));
        assert!(m.matches(&ctrl('b'), "tui.editor.cursorLeft"));
        // ctrl+p / shift+ctrl+p 严格区分（对齐 pi：两个动作互不吞并）
        assert!(m.matches(&ctrl('p'), "app.model.cycleForward"));
        assert!(!m.matches(&ctrl('p'), "app.model.cycleBackward"));
        assert!(m.matches(
            &key(
                KeyCode::Char('p'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            "app.model.cycleBackward"
        ));
        // ctrl 字母大小写归一（终端可能上报 Char('P')+CONTROL）
        assert!(m.matches(
            &key(KeyCode::Char('P'), KeyModifiers::CONTROL),
            "app.model.cycleForward"
        ));
    }

    #[test]
    fn matches_special_keys() {
        let m = KeybindingsManager::new_default();
        assert!(m.matches(&key(KeyCode::Esc, KeyModifiers::NONE), "app.interrupt"));
        assert!(!m.matches(&key(KeyCode::Enter, KeyModifiers::SHIFT), "app.interrupt"));
        assert!(m.matches(
            &key(KeyCode::BackTab, KeyModifiers::NONE),
            "app.thinking.cycle"
        ));
        assert!(m.matches(
            &key(KeyCode::Enter, KeyModifiers::SHIFT),
            "tui.input.newLine"
        ));
        assert!(m.matches(&ctrl('j'), "tui.input.newLine"));
        assert!(m.matches(&key(KeyCode::Enter, KeyModifiers::NONE), "tui.input.submit"));
        assert!(m.matches(&key(KeyCode::Tab, KeyModifiers::NONE), "tui.input.tab"));
    }

    #[test]
    fn shift_letter_accepts_terminal_and_kitty_forms() {
        // shift+a：kitty raw Char('a')+SHIFT 或终端转换 Char('A') 无修饰
        assert!(matches_event(&shift('a'), &id("shift+a")));
        assert!(matches_event(&plain('A'), &id("shift+a")));
        assert!(!matches_event(&plain('a'), &id("shift+a")));
        assert!(!matches_event(&plain('A'), &id("a")));
        assert!(matches_event(&plain('a'), &id("a")));
        // shift+2：Char('2')+SHIFT 或 Char('@') 无修饰
        assert!(matches_event(&shift('2'), &id("shift+2")));
        assert!(matches_event(&plain('@'), &id("shift+2")));
        assert!(!matches_event(&plain('2'), &id("shift+2")));
    }

    #[test]
    fn ctrl_shift_letter_distinct_from_ctrl_letter() {
        let t = |code: KeyCode, mods: KeyModifiers| key(code, mods);
        // kitty raw：Char('t')+CTRL+SHIFT → ctrl+shift+t
        assert!(matches_event(
            &t(
                KeyCode::Char('t'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            &id("ctrl+shift+t")
        ));
        // 大写形式（terminal 上报带 SHIFT）也能匹配
        assert!(matches_event(
            &t(
                KeyCode::Char('T'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            &id("ctrl+shift+t")
        ));
        // 与 ctrl+t（app.thinking.toggle）严格区分
        assert!(!matches_event(
            &t(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &id("ctrl+shift+t")
        ));
        assert!(!matches_event(
            &t(
                KeyCode::Char('t'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT
            ),
            &id("ctrl+t")
        ));
    }

    #[test]
    fn user_config_overrides_and_disables() {
        let mut user = HashMap::new();
        // 覆盖默认：clear 换成 ctrl+q
        user.insert("app.clear".to_string(), vec!["ctrl+q".to_string()]);
        // 空数组禁用：model.select 禁用
        user.insert("app.model.select".to_string(), Vec::new());
        // 多键
        user.insert(
            "tui.editor.cursorLeft".to_string(),
            vec!["left".to_string(), "ctrl+h".to_string()],
        );
        // 非法键 id 忽略
        user.insert(
            "tui.editor.cursorUp".to_string(),
            vec!["nonsense".to_string()],
        );
        let m = KeybindingsManager::from_user(&user);

        assert!(m.matches(&ctrl('q'), "app.clear"));
        assert!(!m.matches(&ctrl('c'), "app.clear"), "默认 ctrl+c 被覆盖");
        assert!(!m.matches(&ctrl('l'), "app.model.select"), "空数组禁用");
        assert!(
            m.matches(&ctrl('h'), "tui.editor.cursorLeft"),
            "多键任意命中"
        );
        assert!(
            !m.matches(&key(KeyCode::Up, KeyModifiers::NONE), "tui.editor.cursorUp"),
            "非法键被忽略"
        );
    }

    #[test]
    fn detects_conflicts_only_between_user_claims() {
        let mut user = HashMap::new();
        user.insert("app.clear".to_string(), vec!["ctrl+x".to_string()]);
        user.insert("app.message.copy".to_string(), vec!["ctrl+x".to_string()]);
        let m = KeybindingsManager::from_user(&user);
        assert_eq!(m.conflicts().len(), 1);
        let c = &m.conflicts()[0];
        assert_eq!(c.key, "ctrl+x");
        assert!(c.keybindings.contains(&"app.clear".to_string()));
        assert!(c.keybindings.contains(&"app.message.copy".to_string()));

        // 用户与默认键冲突不算（对齐 pi：仅用户之间）
        let m2 = KeybindingsManager::new_default();
        assert!(m2.conflicts().is_empty());
    }

    #[test]
    fn user_config_parse_formats() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("keybindings.json"),
            r#"{
                "app.clear": "ctrl+q",
                "tui.editor.cursorLeft": ["left", "ctrl+h"],
                "tui.editor.undo": []
            }"#,
        )
        .unwrap();
        let cfg = load_user_config(dir.path());
        assert_eq!(cfg.get("app.clear").unwrap(), &vec!["ctrl+q".to_string()]);
        assert_eq!(
            cfg.get("tui.editor.cursorLeft").unwrap(),
            &vec!["left".to_string(), "ctrl+h".to_string()]
        );
        assert!(cfg.get("tui.editor.undo").unwrap().is_empty());
        // 缺失文件 → 空配置
        let empty = load_user_config(tempfile::tempdir().unwrap().path());
        assert!(empty.is_empty());
    }

    #[test]
    fn message_scroll_bindings_default_to_shift_home_end() {
        let m = KeybindingsManager::new_default();
        assert!(m.matches(
            &key(KeyCode::Home, KeyModifiers::SHIFT),
            "app.message.scrollToTop"
        ));
        assert!(!m.matches(
            &key(KeyCode::Home, KeyModifiers::NONE),
            "app.message.scrollToTop"
        ));
        assert!(m.matches(
            &key(KeyCode::End, KeyModifiers::SHIFT),
            "app.message.scrollToBottom"
        ));
        assert!(!m.matches(
            &key(KeyCode::End, KeyModifiers::NONE),
            "app.message.scrollToBottom"
        ));
        assert!(m.matches(
            &key(KeyCode::Home, KeyModifiers::NONE),
            "tui.editor.cursorLineStart"
        ));
        assert!(m.matches(
            &key(KeyCode::End, KeyModifiers::NONE),
            "tui.editor.cursorLineEnd"
        ));
    }

    #[test]
    fn message_page_bindings_default_to_shift_pageup_pagedown() {
        let m = KeybindingsManager::new_default();
        assert!(m.matches(
            &key(KeyCode::PageUp, KeyModifiers::SHIFT),
            "app.message.pageUp"
        ));
        assert!(!m.matches(
            &key(KeyCode::PageUp, KeyModifiers::NONE),
            "app.message.pageUp"
        ));
        assert!(m.matches(
            &key(KeyCode::PageDown, KeyModifiers::SHIFT),
            "app.message.pageDown"
        ));
        assert!(!m.matches(
            &key(KeyCode::PageDown, KeyModifiers::NONE),
            "app.message.pageDown"
        ));
        // 裸 PageUp/PageDown 仍是编辑器光标翻页
        assert!(m.matches(
            &key(KeyCode::PageUp, KeyModifiers::NONE),
            "tui.editor.pageUp"
        ));
        assert!(m.matches(
            &key(KeyCode::PageDown, KeyModifiers::NONE),
            "tui.editor.pageDown"
        ));
    }

    #[test]
    fn session_and_editor_bindings_match_pi_defaults() {
        let m = KeybindingsManager::new_default();
        // app.session.* 与 pi 默认一致
        assert!(m.matches(&ctrl('n'), "app.session.toggleNamedFilter"));
        assert!(m.matches(&ctrl('p'), "app.session.togglePath"));
        assert!(m.matches(&ctrl('s'), "app.session.toggleSort"));
        assert!(m.matches(&ctrl('r'), "app.session.rename"));
        assert!(m.matches(&ctrl('d'), "app.session.delete"));
        assert!(m.matches(
            &key(KeyCode::Backspace, KeyModifiers::CONTROL),
            "app.session.deleteNoninvasive"
        ));
        // /resume 内 ctrl+g 跳到当前会话的下一条会话（全局 ctrl+g 仍是 app.editor.external）
        assert!(m.matches(&ctrl('g'), "app.session.next"));
        // tui.select.*
        assert!(m.matches(&key(KeyCode::Up, KeyModifiers::NONE), "tui.select.up"));
        assert!(m.matches(&key(KeyCode::Home, KeyModifiers::NONE), "tui.select.first"));
        assert!(m.matches(&key(KeyCode::End, KeyModifiers::NONE), "tui.select.last"));
        assert!(m.matches(
            &key(KeyCode::PageDown, KeyModifiers::NONE),
            "tui.select.pageDown"
        ));
        assert!(m.matches(
            &key(KeyCode::Enter, KeyModifiers::NONE),
            "tui.select.confirm"
        ));
        assert!(m.matches(&key(KeyCode::Esc, KeyModifiers::NONE), "tui.select.cancel"));
        assert!(m.matches(&ctrl('c'), "tui.select.cancel"));
        // app.*
        assert!(m.matches(&ctrl('t'), "app.thinking.toggle"));
        assert!(m.matches(&ctrl('o'), "app.tools.expand"));
        assert!(m.matches(&alt('b'), "tui.editor.cursorWordLeft"));
        assert!(m.matches(&ctrl('w'), "tui.editor.deleteWordBackward"));
        assert!(m.matches(&ctrl('y'), "tui.editor.yank"));
        assert!(m.matches(&ctrl('-'), "tui.editor.undo"));
    }

    #[test]
    fn editor_copy_all_defaults_to_alt_c() {
        let m = KeybindingsManager::new_default();
        // Alt+C 命中复制整个输入框内容；不带 Alt 的裸键 / Ctrl+C 不误命中
        assert!(m.matches(&alt('c'), "app.editor.copy"));
        assert!(m.matches(&alt('C'), "app.editor.copy"));
        assert!(!m.matches(&plain('c'), "app.editor.copy"));
        assert!(!m.matches(&ctrl('c'), "app.editor.copy"));
    }
}

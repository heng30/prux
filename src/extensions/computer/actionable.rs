//! 动作前判定与投递语义：把「按下去没生效」变成可判定的报告。
//!
//! 一次 `computer_ui_action` 在执行前先过几道闸（可见 / 稳定 / 可用 / 目标声明支持），
//! 执行后回读一次目标，回执尾行给出投递语义，让模型知道「重试安不安全」。
//!
//! 两处刻意与桌面自动化的常见做法不同：
//!
//! - `Unknown` **不阻断**。平台读不到状态（AT-SPI 没给状态位、AX 没有属性）时照常执行，
//!   只把不确定性写进回执——桌面 agent 判错的代价是多点一次，不是写坏数据。
//! - 没有命中测试。Wayland 拿不到窗口 z 序，遮挡只能在 X11 上猜，猜错的代价比不做更大。

use super::state::Rect;

/// 闸名：目标当前可见（没被最小化/离屏）。
pub const VISIBLE: &str = "visible";

/// 闸名：目标矩形在采样窗口内没有变化。
pub const STABLE: &str = "stable";

/// 闸名：目标处于可用状态（不是灰的）。
pub const ENABLED: &str = "enabled";

/// 闸名：目标声明了执行这个动作所需的能力。
pub const SUPPORTED: &str = "supported";

/// 等待一次谓词成立时的轮询间隔。
pub const POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// 稳定性采样两次读之间的间隔（布局与菜单动画的典型收敛时间）。
pub const STABLE_SAMPLE: std::time::Duration = std::time::Duration::from_millis(40);

/// 两个矩形算「同一个」的容差（像素）：亚像素抖动不算移动。
const RECT_TOLERANCE: i32 = 1;

/// 点类动作的共同能力名（各平台 `GetActions` / pattern 探测的并集）。
const CLICK_CAPS: &[&str] = &["click", "press", "activate", "invoke", "do"];

/// 一道闸的结论。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// 满足。
    Pass,
    /// 不满足：动作被拒绝。
    Fail,
    /// 平台没给出证据：不阻断，但写进回执。
    Unknown,
}

/// 一道闸的结论加原因。
#[derive(Debug, Clone)]
pub struct Check {
    /// 闸名，见 [`VISIBLE`] / [`STABLE`] / [`ENABLED`] / [`SUPPORTED`]。
    pub name: &'static str,
    /// 结论。
    pub status: Status,
    /// `Fail` / `Unknown` 时的一句话原因；`Pass` 时为 `None`。
    pub reason: Option<String>,
}

impl Check {
    /// 一条 `Pass` 结论。
    fn pass(name: &'static str) -> Self {
        Self {
            name,
            status: Status::Pass,
            reason: None,
        }
    }

    /// 一条 `Fail` 结论。
    fn fail(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Fail,
            reason: Some(reason.into()),
        }
    }

    /// 一条 `Unknown` 结论。
    fn unknown(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            status: Status::Unknown,
            reason: Some(reason.into()),
        }
    }
}

/// 一次动作前判定的全部结论。
#[derive(Debug, Clone, Default)]
pub struct Report {
    /// 各道闸的结论，顺序固定为 visible → stable → enabled → supported。
    pub checks: Vec<Check>,
}

impl Report {
    /// 是否所有闸都通过（`Unknown` 也算通过）。
    pub fn actionable(&self) -> bool {
        self.failures().next().is_none()
    }

    /// 所有 `Fail` 的闸。
    pub fn failures(&self) -> impl Iterator<Item = &Check> {
        self.checks
            .iter()
            .filter(|check| check.status == Status::Fail)
    }

    /// 失败原因的一句话摘要：`visible (窗口已最小化), enabled (状态是 disabled)`。
    pub fn reasons(&self) -> String {
        self.failures()
            .map(|check| {
                let reason = check.reason.as_deref().unwrap_or("failed");
                format!("{} ({reason})", check.name)
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// 未被证实的闸的摘要，供回执附注；没有时为 `None`。
    pub fn doubts(&self) -> Option<String> {
        let list: Vec<String> = self
            .checks
            .iter()
            .filter(|check| check.status == Status::Unknown)
            .map(|check| {
                let reason = check.reason.as_deref().unwrap_or("no evidence");
                format!("{} ({reason})", check.name)
            })
            .collect();
        (!list.is_empty()).then(|| list.join(", "))
    }

    /// 失败项是否都属于「等一等可能就好」——决定拒绝时该建议等待还是换目标。
    pub fn waitable(&self) -> bool {
        self.failures()
            .all(|check| matches!(check.name, VISIBLE | STABLE | ENABLED))
    }
}

/// 动作执行后能校验到什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readback {
    /// 不校验：回执只能标 `delivered-unverified`。
    None,
    /// 回读控件值并与写入的文本比对。
    Value,
    /// 比对状态的「在/不在」：`checked`（toggle）或 `expanded`（展开折叠）。
    State(&'static str),
}

/// 一个动作对目标的要求。
pub struct Spec {
    /// 动作名（与 `schema::UI_ACTIONS` 同一个词）。
    pub action: &'static str,
    /// 是否要求目标当前可见。
    pub visible: bool,
    /// 是否要求目标矩形在采样窗口内不变（会多读一次目标）。
    pub stable: bool,
    /// 是否要求目标处于可用状态。
    pub enabled: bool,
    /// 目标声明的动作名里命中任一即算支持；空表示不做这项判定。
    pub capabilities: &'static [&'static str],
    /// 执行后能校验到什么。
    pub readback: Readback,
}

impl Spec {
    /// 是否需要先读一次活节点（读类动作不需要）。
    pub fn needs_probe(&self) -> bool {
        self.visible || self.stable || self.enabled || !self.capabilities.is_empty()
    }
}

/// 全部动作的要求表；新增动作时这里必须有一行（`specs_cover_every_action` 测试会盯）。
pub const SPECS: &[Spec] = &[
    Spec {
        action: "invoke",
        visible: true,
        stable: true,
        enabled: true,
        capabilities: CLICK_CAPS,
        readback: Readback::None,
    },
    Spec {
        action: "click_text",
        visible: true,
        stable: true,
        enabled: true,
        capabilities: CLICK_CAPS,
        readback: Readback::None,
    },
    Spec {
        action: "set_value",
        visible: true,
        stable: false,
        enabled: true,
        capabilities: &[],
        readback: Readback::Value,
    },
    Spec {
        action: "select_text",
        visible: true,
        stable: false,
        enabled: true,
        capabilities: &[],
        readback: Readback::None,
    },
    Spec {
        action: "select",
        visible: true,
        stable: false,
        enabled: true,
        capabilities: &["select", "select child"],
        readback: Readback::None,
    },
    Spec {
        action: "scroll_into_view",
        visible: false,
        stable: false,
        enabled: true,
        capabilities: &["scroll", "scroll_into_view", "scroll into view"],
        readback: Readback::None,
    },
    Spec {
        action: "focus",
        visible: true,
        stable: false,
        enabled: true,
        capabilities: &[],
        readback: Readback::None,
    },
    Spec {
        action: "toggle",
        visible: true,
        stable: false,
        enabled: true,
        capabilities: &["toggle", "check", "click"],
        readback: Readback::State("checked"),
    },
    Spec {
        action: "expand",
        visible: true,
        stable: false,
        enabled: true,
        capabilities: &["expand", "invoke", "click"],
        readback: Readback::State("expanded"),
    },
    Spec {
        action: "collapse",
        visible: true,
        stable: false,
        enabled: true,
        capabilities: &["collapse", "invoke", "click"],
        readback: Readback::State("expanded"),
    },
    Spec {
        action: "get_value",
        visible: false,
        stable: false,
        enabled: false,
        capabilities: &[],
        readback: Readback::None,
    },
    Spec {
        action: "get_text",
        visible: false,
        stable: false,
        enabled: false,
        capabilities: &[],
        readback: Readback::None,
    },
    Spec {
        action: "get_caret",
        visible: false,
        stable: false,
        enabled: false,
        capabilities: &[],
        readback: Readback::None,
    },
    Spec {
        action: "locate_text",
        visible: false,
        stable: false,
        enabled: false,
        capabilities: &[],
        readback: Readback::None,
    },
    Spec {
        action: "close",
        visible: false,
        stable: false,
        enabled: false,
        capabilities: &[],
        readback: Readback::None,
    },
    Spec {
        action: "minimize",
        visible: false,
        stable: false,
        enabled: false,
        capabilities: &[],
        readback: Readback::None,
    },
    Spec {
        action: "maximize",
        visible: false,
        stable: false,
        enabled: false,
        capabilities: &[],
        readback: Readback::None,
    },
    Spec {
        action: "restore",
        visible: false,
        stable: false,
        enabled: false,
        capabilities: &[],
        readback: Readback::None,
    },
];

/// 查一个动作的要求；表里没有时返回 `None`。
pub fn spec(action: &str) -> Option<&'static Spec> {
    SPECS.iter().find(|spec| spec.action == action)
}

/// 判定用的一次活节点读。
pub struct Evidence<'a> {
    /// 目标当前的状态名；空表示平台没读到。
    pub states: &'a [String],
    /// 目标当前声明的动作名。
    pub actions: &'a [String],
    /// 这份动作名是否是完整的（只有完整时「没声明」才能当拒绝用）。
    pub actions_complete: bool,
    /// 矩形是否在采样窗口内保持不变；`None` 表示没采样。
    pub stable: Option<bool>,
    /// 平台明确报告的可用状态；`None` 表示读不到。
    pub enabled: Option<bool>,
    /// 平台明确报告的隐藏/离屏状态；`None` 表示读不到。
    pub hidden: Option<bool>,
}

/// 按动作的要求跑一遍闸。
pub fn evaluate(spec: &Spec, evidence: &Evidence<'_>) -> Report {
    let mut checks = Vec::new();
    if spec.visible {
        checks.push(visible(evidence));
    }
    if spec.stable {
        checks.push(stable(evidence));
    }
    if spec.enabled {
        checks.push(enabled(evidence));
    }
    if !spec.capabilities.is_empty() {
        checks.push(supported(evidence, spec.capabilities));
    }
    Report { checks }
}

/// 目标可见：没有明确的「最小化 / 离屏」报告就算通过。
fn visible(evidence: &Evidence<'_>) -> Check {
    match evidence.hidden {
        Some(true) => Check::fail(VISIBLE, "the target is minimized or off screen"),
        Some(false) => Check::pass(VISIBLE),
        None => Check::unknown(VISIBLE, "the platform reports no visibility state"),
    }
}

/// 目标稳定：两次采样的矩形一致。
fn stable(evidence: &Evidence<'_>) -> Check {
    match evidence.stable {
        Some(true) => Check::pass(STABLE),
        Some(false) => Check::fail(
            STABLE,
            "the target moved between two samples; it is still settling",
        ),
        None => Check::unknown(STABLE, "the target has no on-screen rectangle to sample"),
    }
}

/// 目标可用：只认平台明确报告的布尔值。
fn enabled(evidence: &Evidence<'_>) -> Check {
    match evidence.enabled {
        Some(true) => Check::pass(ENABLED),
        Some(false) => Check::fail(ENABLED, "the target is disabled"),
        None => Check::unknown(ENABLED, "the platform reports no enabled state"),
    }
}

/// 目标声明了对应能力：只有动作表完整时「没声明」才算拒绝。
fn supported(evidence: &Evidence<'_>, capabilities: &[&str]) -> Check {
    if !evidence.actions_complete {
        return Check::unknown(SUPPORTED, "the platform does not enumerate every action");
    }
    let hit = evidence.actions.iter().any(|action| {
        let action = action.to_ascii_lowercase();
        capabilities
            .iter()
            .any(|wanted| action == *wanted || action.contains(wanted))
    });
    if hit {
        Check::pass(SUPPORTED)
    } else if evidence.actions.is_empty() {
        Check::unknown(SUPPORTED, "the target advertises no actions at all")
    } else {
        Check::fail(
            SUPPORTED,
            format!(
                "the target advertises {} and none of them does this",
                evidence.actions.join(", ")
            ),
        )
    }
}

/// 从目标声明的动作里挑一个能用的（用于把「挨个试」换成「按声明选」）。
pub fn pick_action<'a>(advertised: &'a [String], capabilities: &[&str]) -> Option<&'a str> {
    advertised
        .iter()
        .find(|action| {
            let action = action.to_ascii_lowercase();
            capabilities
                .iter()
                .any(|wanted| action == *wanted || action.contains(wanted))
        })
        .map(String::as_str)
}

/// 从目标声明的动作里挑一个「点」类动作（`invoke` / `click_text` 共用）。
pub fn click_action(advertised: &[String]) -> Option<&str> {
    pick_action(advertised, CLICK_CAPS)
}

/// 两个矩形是否算同一个（1px 容差）。
pub fn same_rect(left: Rect, right: Rect) -> bool {
    (left.x - right.x).abs() <= RECT_TOLERANCE
        && (left.y - right.y).abs() <= RECT_TOLERANCE
        && left.width.abs_diff(right.width) <= RECT_TOLERANCE as u32
        && left.height.abs_diff(right.height) <= RECT_TOLERANCE as u32
}

/// 一次动作的投递语义：决定模型重试安不安全。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// 没送出去（或送出去之前就被拒）：重试安全。
    NotDelivered,
    /// 送出去了，但没有证据证明目标真的变了：重试可能变成双击。
    Unverified,
    /// 送出去并回读到了变化：重试不安全。
    Verified,
}

impl Delivery {
    /// 回执尾行：`delivery: delivered-unverified (retry unsafe); note`。
    pub fn line(self, note: &str) -> String {
        let (name, retry) = match self {
            Self::NotDelivered => ("not-delivered", "retry safe"),
            Self::Unverified => ("delivered-unverified", "retry unsafe"),
            Self::Verified => ("delivered-verified", "retry unsafe"),
        };
        if note.is_empty() {
            format!("delivery: {name} ({retry})")
        } else {
            format!("delivery: {name} ({retry}); {note}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence<'a>(states: &'a [String], actions: &'a [String]) -> Evidence<'a> {
        Evidence {
            states,
            actions,
            actions_complete: true,
            stable: None,
            enabled: None,
            hidden: None,
        }
    }

    /// 表必须覆盖每一个 `computer_ui_action` 动作，否则判定会被悄悄跳过。
    #[test]
    fn specs_cover_every_action() {
        for action in super::super::schema::UI_ACTIONS {
            assert!(spec(action).is_some(), "{action} 缺少 Spec");
        }
        for spec in SPECS {
            assert!(
                super::super::schema::UI_ACTIONS.contains(&spec.action),
                "{} 不在 UI_ACTIONS 里",
                spec.action
            );
        }
    }

    /// 读类动作不读活节点：它们本身就是要读的东西。
    #[test]
    fn read_only_actions_do_not_probe() {
        for action in super::super::schema::READ_ONLY_UI_ACTIONS {
            let spec = spec(action).expect("spec");
            assert!(!spec.needs_probe(), "{action} 不该触发判定读");
        }
    }

    #[test]
    fn hidden_target_fails_and_is_waitable() {
        let empty: Vec<String> = Vec::new();
        let mut evidence = evidence(&empty, &empty);
        evidence.hidden = Some(true);
        let report = evaluate(spec("invoke").unwrap(), &evidence);
        assert!(!report.actionable());
        assert!(report.reasons().contains("visible"));
        assert!(report.waitable(), "隐藏的目标等一等可能就显示了");
    }

    #[test]
    fn missing_capability_is_terminal() {
        let actions = vec!["expand".to_string()];
        let report = evaluate(spec("invoke").unwrap(), &evidence(&[], &actions));
        assert!(!report.actionable());
        assert!(!report.waitable(), "缺少能力等多久都没用");
        assert!(report.reasons().contains("supported"));
    }

    /// 平台不枚举完整动作表时不许把它当拒绝用。
    #[test]
    fn incomplete_action_list_is_only_a_doubt() {
        let actions = vec!["invoke".to_string()];
        let mut evidence = evidence(&[], &actions);
        evidence.actions_complete = false;
        let report = evaluate(spec("collapse").unwrap(), &evidence);
        assert!(report.actionable());
        assert!(report.doubts().is_some());
    }

    #[test]
    fn disabled_and_unstable_are_waitable() {
        let empty: Vec<String> = Vec::new();
        let mut evidence = evidence(&empty, &empty);
        evidence.enabled = Some(false);
        evidence.stable = Some(false);
        let report = evaluate(spec("invoke").unwrap(), &evidence);
        assert_eq!(report.failures().count(), 2);
        assert!(report.waitable());
    }

    #[test]
    fn pick_action_prefers_an_advertised_match() {
        let actions = vec!["press".to_string(), "click".to_string()];
        assert_eq!(pick_action(&actions, CLICK_CAPS), Some("press"));
        assert_eq!(pick_action(&[], CLICK_CAPS), None);
    }

    #[test]
    fn rect_tolerance_is_one_pixel() {
        let rect = Rect {
            x: 10,
            y: 20,
            width: 100,
            height: 40,
        };
        assert!(same_rect(rect, Rect { x: 11, ..rect }));
        assert!(!same_rect(rect, Rect { x: 12, ..rect }));
    }

    #[test]
    fn delivery_lines_carry_the_retry_verdict() {
        assert!(Delivery::NotDelivered.line("").contains("retry safe"));
        assert!(Delivery::Unverified.line("x").contains("retry unsafe"));
        assert!(Delivery::Verified.line("").contains("delivered-verified"));
    }
}

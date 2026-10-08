//! 滚轮事件 → 行数换算
//!
//! `auto` 模式下：孤立的一格滚 1 行；快速连滚按事件间隔加速（100ms 间隔 1 行、
//! 50ms 2 行、20ms 5 行，上限 6 行）；间隔 <5ms 视为同一物理格的高分辨率分片，恒 1 行。
//! 本地 macOS 终端已由系统加速过滚轮增量，此时 auto 恒 1 行。

use serde_json::Value;

/// 同一物理轮格的分片间隔上限：更近的事件每次只滚 1 行，不参与加速
const BURST_GAP_MS: f64 = 5.0;
/// 超过此间隔视为新手势（重新从 1 行起算）
const GESTURE_GAP_MS: f64 = 200.0;
/// 平均间隔映射到 1 行的参考值；更快的连滚按比例放大
const REFERENCE_GAP_MS: f64 = 100.0;
/// auto 模式每次事件的行数上限
const MAX_AUTO_LINES: f64 = 10.0;

/// 每次滚轮事件滚动的逻辑行数：固定值或 `auto`（按滚动速度加速）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelScrollLines {
    /// 按滚动速度加速
    Auto,
    /// 每次滚动固定行数
    Lines(usize),
}

impl WheelScrollLines {
    /// 固定行数上限
    pub const MAX: usize = 100;

    /// 解析 settings.json 的 `fullscreenWheelScrollLines`：
    /// 数字 → 取整并钳制 1..=100；`"auto"`/缺失/非法 → [`WheelScrollLines::Auto`]
    pub fn from_json(value: Option<&Value>) -> Self {
        match value.and_then(|v| v.as_f64()) {
            Some(n) if n.is_finite() => Self::Lines((n.floor().max(1.0) as usize).min(Self::MAX)),
            _ => Self::Auto,
        }
    }

    /// 用于 /settings 展示与写盘
    pub fn as_str(&self) -> String {
        match self {
            Self::Auto => "auto".to_string(),
            Self::Lines(n) => (*n).clamp(1, Self::MAX).to_string(),
        }
    }
}

/// 本地 macOS 终端（非 SSH）已由系统加速滚轮增量：每次事件本就对应一行
fn terminal_accelerates_wheel() -> bool {
    cfg!(target_os = "macos")
        && std::env::var_os("SSH_CONNECTION").is_none()
        && std::env::var_os("SSH_CLIENT").is_none()
        && std::env::var_os("SSH_TTY").is_none()
}

/// 把连续的滚轮事件换算成行数（有状态：跨事件维护手势与平均间隔）
#[derive(Debug, Clone)]
pub struct WheelScrollAccelerator {
    /// 当前行数模式（固定或 auto）
    lines: WheelScrollLines,
    /// 是否启用速度加速（本地 macOS 终端为 false）
    accelerate: bool,
    /// 上一次事件的毫秒时间戳（`f64::NEG_INFINITY` 表示无历史）
    last_time_ms: f64,
    /// 上一次滚动方向（±1，0 表示无历史）
    last_direction: i32,
    /// 当前手势内事件间隔的滑动平均（缺省表示手势刚开始）
    average_gap_ms: Option<f64>,
    /// 行数取整后余下的小数部分，结转到下一事件
    carry: f64,
}

impl Default for WheelScrollAccelerator {
    /// 默认用 `Auto` 模式，并依终端类型决定是否启用速度加速。
    fn default() -> Self {
        Self::new(WheelScrollLines::Auto, !terminal_accelerates_wheel())
    }
}

impl WheelScrollAccelerator {
    /// 以指定行数模式与加速开关构造；历史时间/方向/平均间隔/余量均清零。
    pub fn new(lines: WheelScrollLines, accelerate: bool) -> Self {
        WheelScrollAccelerator {
            lines,
            accelerate,
            last_time_ms: f64::NEG_INFINITY,
            last_direction: 0,
            average_gap_ms: None,
            carry: 0.0,
        }
    }

    /// 按终端类型推断是否加速（本地 macOS 不加速）
    pub fn for_terminal(lines: WheelScrollLines) -> Self {
        Self::new(lines, !terminal_accelerates_wheel())
    }

    /// 更新行数模式并重置滚动状态（历史手势作废）。
    pub fn set_lines(&mut self, lines: WheelScrollLines) {
        self.lines = lines;
        self.reset();
    }

    /// 返回当前的行数模式。
    pub fn lines(&self) -> WheelScrollLines {
        self.lines
    }

    /// 返回本次滚轮事件应滚动的行数（正数）；`direction` 为 ±1。
    /// `now_ms` 需单调递增（调用方用 [`std::time::Instant`] 的 elapsed 毫秒）。
    pub fn next(&mut self, direction: i32, now_ms: u64) -> usize {
        if let WheelScrollLines::Lines(n) = self.lines {
            return n.clamp(1, WheelScrollLines::MAX);
        }

        if !self.accelerate {
            return 1;
        }

        let now = now_ms as f64;
        let gap = now - self.last_time_ms;
        let same_gesture = direction == self.last_direction && gap <= GESTURE_GAP_MS;
        self.last_time_ms = now;
        self.last_direction = direction;

        if !same_gesture {
            self.average_gap_ms = None;
            self.carry = 0.0;
            return 1;
        }

        if gap < BURST_GAP_MS {
            return 1;
        }

        self.average_gap_ms = Some(match self.average_gap_ms {
            None => gap,
            Some(avg) => (avg + gap) / 2.0,
        });

        let base =
            (REFERENCE_GAP_MS / self.average_gap_ms.unwrap_or(gap)).clamp(1.0, MAX_AUTO_LINES);
        let lines = base + self.carry;
        let whole = lines.floor();
        self.carry = lines - whole;
        whole as usize
    }

    /// 清空手势历史与行数余量（时间、方向、平均间隔、carry）。
    fn reset(&mut self) {
        self.last_time_ms = f64::NEG_INFINITY;
        self.last_direction = 0;
        self.average_gap_ms = None;
        self.carry = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_lines_ignores_velocity() {
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Lines(3), true);
        assert_eq!(acc.next(1, 0), 3);
        assert_eq!(acc.next(1, 10), 3);
        // 越界值被钳制
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Lines(0), true);
        assert_eq!(acc.next(1, 0), 1);
    }

    #[test]
    fn isolated_notch_moves_one_line() {
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        assert_eq!(acc.next(1, 1_000), 1);
        // 200ms 以上算新手势，仍 1 行
        assert_eq!(acc.next(1, 1_500), 1);
        // 反向立即算新手势
        assert_eq!(acc.next(-1, 1_510), 1);
    }

    #[test]
    fn auto_accelerates_fast_spins() {
        // 100ms 间隔：恒 1 行
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        assert_eq!(acc.next(1, 0), 1, "首格恒 1 行");
        assert_eq!(acc.next(1, 100), 1);
        assert_eq!(acc.next(1, 200), 1);
        // 50ms 间隔：2 行
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        assert_eq!(acc.next(1, 0), 1);
        assert_eq!(acc.next(1, 50), 2);
        assert_eq!(acc.next(1, 100), 2);
        // 20ms 间隔：5 行
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        assert_eq!(acc.next(1, 0), 1);
        assert_eq!(acc.next(1, 20), 5);
        // 极小间隔（同一物理格的高分辨率分片）恒 1 行
        assert_eq!(acc.next(1, 22), 1);
        assert_eq!(acc.next(1, 24), 1);
    }

    #[test]
    fn auto_lines_capped_at_six() {
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        acc.next(1, 0);
        for i in 1..20 {
            assert!(acc.next(1, i * 100) <= 6);
        }
        // 极快连滚不超过上限
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Auto, true);
        acc.next(1, 0);
        let mut max = 0;
        for i in 1..50 {
            max = max.max(acc.next(1, i * 100_000 + 6 * i));
        }
        assert!(max <= 6, "max = {max}");
    }

    #[test]
    fn non_accelerating_terminal_stays_one_line() {
        let mut acc = WheelScrollAccelerator::new(WheelScrollLines::Auto, false);
        for i in 0..10 {
            assert_eq!(acc.next(1, i * 10), 1);
        }
    }

    #[test]
    fn parses_setting_values() {
        use serde_json::json;
        assert_eq!(
            WheelScrollLines::from_json(Some(&json!("auto"))),
            WheelScrollLines::Auto
        );
        assert_eq!(WheelScrollLines::from_json(None), WheelScrollLines::Auto);
        assert_eq!(
            WheelScrollLines::from_json(Some(&json!(5))),
            WheelScrollLines::Lines(5)
        );
        assert_eq!(
            WheelScrollLines::from_json(Some(&json!(3.7))),
            WheelScrollLines::Lines(3)
        );
        assert_eq!(
            WheelScrollLines::from_json(Some(&json!(0))),
            WheelScrollLines::Lines(1)
        );
        assert_eq!(
            WheelScrollLines::from_json(Some(&json!(9999))),
            WheelScrollLines::Lines(WheelScrollLines::MAX)
        );
        assert_eq!(WheelScrollLines::Auto.as_str(), "auto");
        assert_eq!(WheelScrollLines::Lines(7).as_str(), "7");
    }
}

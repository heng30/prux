//! 调度表达式解析。
//!
//! 四种写法，**判定顺序**就是上游的顺序（`+10m` 与 `5m` 都是"数字+单位"，靠前导 `+` 区分"一次性"与"周期"）：
//!
//! | 写法 | 类型 | 例 |
//! |---|---|---|
//! | `+10m` / `+1h` / `+2d` / `+30s` | 一次性（相对） | 10 分钟后跑一次 |
//! | `5m` / `1h` / `2d` / `30s` | 周期 | 每 5 分钟 |
//! | ISO 时间戳 | 一次性（绝对） | `2026-02-14T09:00:00` |
//! | 6 段 cron | cron | `0 0 9 * * 1`（本地时间每周一 9:00） |
//!
//! 这里只留"调度串 → 种类"的判别与"下一次触发"的分派：
//! 时间换算（时长/时间戳/epoch）在 [`crate::utils::time`]，cron 本身在 [`crate::utils::cron`]。
//!
//! **下一次触发按本地时间算**：`0 0 9 * * 1` 对用户就是本地周一 9 点，用 UTC 算会在错误的时刻叫醒人。

use crate::utils::{
    cron::{self, Cron},
    time::{
        epoch_ms_to_local_rfc3339, ms_to_system, now_ms, parse_duration_ms, parse_timestamp_ms,
        system_to_ms,
    },
};
use std::time::SystemTime;
use strum_macros::{EnumString, IntoStaticStr};

/// 调度类型。
///
/// 变体名与字符串的互转由 `strum` 派生（全小写、大小写不敏感）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString, IntoStaticStr)]
#[strum(serialize_all = "lowercase", ascii_case_insensitive)]
pub(crate) enum ScheduleKind {
    /// 6 段 cron
    Cron,
    /// 固定间隔（`interval_ms`）
    Interval,
    /// 一次性（`schedule` 就是目标时间）
    Once,
}

impl ScheduleKind {
    /// 变体名的小写静态字符串（`"cron"` / `"interval"` / `"once"`），用于持久化与展示。
    pub(crate) fn as_str(self) -> &'static str {
        self.into()
    }

    /// 解析种类名（大小写不敏感、允许首尾空白）；无法识别时返回 `None`。
    pub(crate) fn parse(s: &str) -> Option<Self> {
        s.trim().parse().ok()
    }
}

/// 解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Detected {
    /// 识别出的调度种类（cron / interval / 一次性）。
    pub kind: ScheduleKind,
    /// 规范化后的调度串（`+10m` → ISO/epoch 目标；其余原样）
    pub normalized: String,
    /// 周期任务的间隔毫秒数
    pub interval_ms: Option<u64>,
    /// 一次性任务的目标时间（epoch ms）
    pub at_ms: Option<u64>,
}

/// 嗅探一个调度串（非法就报错，错误里给例子）。
pub(crate) fn detect(spec: &str) -> Result<Detected, String> {
    let trimmed = spec.trim();

    // "+10m" —— 一次性（相对）
    if let Some(ms) = parse_relative(trimmed) {
        let at = now_ms() + ms;
        return Ok(Detected {
            kind: ScheduleKind::Once,
            normalized: epoch_ms_to_local_rfc3339(at),
            interval_ms: None,
            at_ms: Some(at),
        });
    }

    // "5m" —— 周期
    if let Some(ms) = parse_duration_ms(trimmed) {
        return Ok(Detected {
            kind: ScheduleKind::Interval,
            normalized: trimmed.to_string(),
            interval_ms: Some(ms),
            at_ms: None,
        });
    }

    // ISO 时间戳 —— 一次性（绝对）。过去的时间直接拒，别造一个"永远不会跑"的记录
    if trimmed.len() >= 11
        && trimmed.as_bytes().get(4) == Some(&b'-')
        && trimmed.contains('T')
        && let Some(at) = parse_timestamp_ms(trimmed)
    {
        if at <= now_ms() {
            return Err(format!("Scheduled time {trimmed} is in the past."));
        }
        return Ok(Detected {
            kind: ScheduleKind::Once,
            normalized: epoch_ms_to_local_rfc3339(at),
            interval_ms: None,
            at_ms: Some(at),
        });
    }

    if Cron::parse(trimmed).is_ok() {
        return Ok(Detected {
            kind: ScheduleKind::Cron,
            normalized: trimmed.to_string(),
            interval_ms: None,
            at_ms: None,
        });
    }

    Err(format!(
        "Invalid schedule {spec:?}. Use 6-field cron (e.g. \"0 0 9 * * 1\" — 9am every Monday), \
         interval (\"5m\"/\"1h\"), or one-shot (\"+10m\" / ISO)."
    ))
}

/// 一次性/周期任务的下一次触发。
pub(crate) fn next_run_after(
    kind: ScheduleKind,
    schedule: &str,
    interval_ms: Option<u64>,
    last_run_ms: Option<u64>,
    after: SystemTime,
) -> Option<SystemTime> {
    match kind {
        ScheduleKind::Cron => cron::next_cron_after(schedule, after),
        ScheduleKind::Interval => {
            // 还没跑过：以"现在"为基准（刚武装）；跑过：上次 + 间隔
            let base = last_run_ms.unwrap_or_else(now_ms);
            let next = base.checked_add(interval_ms?)?;
            Some(ms_to_system(next))
        }
        ScheduleKind::Once => {
            let at = at_ms_from_schedule(schedule)?;
            (at > system_to_ms(after)).then(|| ms_to_system(at))
        }
    }
}

/// 一次性任务的调度串 → epoch ms（存的是 ISO，但接受纯数字毫秒）。
pub(crate) fn at_ms_from_schedule(schedule: &str) -> Option<u64> {
    schedule
        .parse::<u64>()
        .ok()
        .or_else(|| parse_timestamp_ms(schedule))
}

/// `+10s`/`+5m`/`+1h`/`+2d` → 毫秒。
fn parse_relative(s: &str) -> Option<u64> {
    parse_duration_ms(s.strip_prefix('+')?)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// strum 派生的双向转换：`as_str` 是规范名、`parse` 去空白且大小写不敏感。
    #[test]
    fn kind_strum_conversions() {
        assert_eq!(ScheduleKind::Cron.as_str(), "cron");
        assert_eq!(ScheduleKind::Interval.as_str(), "interval");
        assert_eq!(ScheduleKind::Once.as_str(), "once");
        assert_eq!(ScheduleKind::parse(" CRON "), Some(ScheduleKind::Cron));
        assert_eq!(ScheduleKind::parse("Once"), Some(ScheduleKind::Once));
        assert_eq!(ScheduleKind::parse("nope"), None);
    }

    #[test]
    fn detects_relative_interval_iso_and_cron() {
        let rel = detect("+10m").unwrap();
        assert_eq!(rel.kind, ScheduleKind::Once);
        assert!(rel.at_ms.unwrap() > now_ms());

        let ivl = detect("5m").unwrap();
        assert_eq!(ivl.kind, ScheduleKind::Interval);
        assert_eq!(ivl.interval_ms, Some(300_000));
        assert_eq!(detect("30s").unwrap().interval_ms, Some(30_000));
        assert_eq!(detect("2d").unwrap().interval_ms, Some(172_800_000));

        let cron = detect("0 0 9 * * 1").unwrap();
        assert_eq!(cron.kind, ScheduleKind::Cron);
        assert_eq!(cron.normalized, "0 0 9 * * 1");

        // 过去的一次性时间 → 直接拒
        let err = detect("2020-01-01T00:00:00Z").unwrap_err();
        assert!(err.contains("in the past"), "{err}");
        // 未来的一次性时间 → 接受
        let future = epoch_ms_to_local_rfc3339(now_ms() + 3_600_000);
        assert_eq!(detect(&future).unwrap().kind, ScheduleKind::Once);

        // 非法：错误里给例子
        let err = detect("every monday").unwrap_err();
        assert!(err.contains("6-field cron"), "{err}");
        // `5m` 是周期，`+5m` 才是一次性（判定顺序不能反）
        assert_eq!(detect("+5m").unwrap().kind, ScheduleKind::Once);
        assert_eq!(detect("5m").unwrap().kind, ScheduleKind::Interval);
    }

    #[test]
    fn next_run_for_kinds() {
        // 周期：没跑过以"现在"为基准，跑过以上次为基准
        let after = SystemTime::now();
        let n = next_run_after(ScheduleKind::Interval, "5m", Some(300_000), None, after).unwrap();
        assert!(system_to_ms(n) >= system_to_ms(after));
        let last = system_to_ms(after);
        let n = next_run_after(
            ScheduleKind::Interval,
            "5m",
            Some(300_000),
            Some(last),
            after,
        )
        .unwrap();
        assert_eq!(system_to_ms(n), last + 300_000);
        // 一次性：目标在未来才有下一次
        let at = now_ms() + 60_000;
        assert!(
            next_run_after(
                ScheduleKind::Once,
                &epoch_ms_to_local_rfc3339(at),
                None,
                None,
                after
            )
            .is_some()
        );
        let past = now_ms() - 60_000;
        assert!(
            next_run_after(
                ScheduleKind::Once,
                &epoch_ms_to_local_rfc3339(past),
                None,
                None,
                after
            )
            .is_none()
        );
    }
}

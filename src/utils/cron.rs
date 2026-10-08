//! 6 段 cron（秒 分 时 日 月 周）的解析与"下一次触发"计算。
//!
//! 形状沿用上游（croner）：每段支持 `*`、`a`、`a-b`、`*/n`、`a-b/n`、`a,b,c`，
//! 周日的 `0` 与 `7` 都当周日。
//!
//! **下一次触发按本地时间算**：`0 0 9 * * 1` 对用户就是本地周一 9 点，用 UTC 算会在错误的时刻叫醒人。
//! 日/周字段都受限时按 cron 老规矩取**或**（不是与）。

use crate::utils::time::{ms_to_system, system_to_ms};
use chrono::{Datelike, Local, NaiveDate, TimeZone};
use std::time::SystemTime;

/// 6 段 cron（秒 分 时 日 月 周）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cron {
    /// 允许的“秒”取值集合。
    secs: Vec<u32>,
    /// 允许的“分”取值集合。
    mins: Vec<u32>,
    /// 允许的“时”取值集合。
    hours: Vec<u32>,
    /// 允许的“日”取值集合。
    doms: Vec<u32>,
    /// 允许的“月”取值集合。
    months: Vec<u32>,
    /// 允许的“周”取值集合。
    dows: Vec<u32>,
    /// 日字段是否受限（决定"日与周都受限时取或"）
    dom_restricted: bool,
    /// 周字段是否受限。
    dow_restricted: bool,
}

impl Cron {
    /// 解析 6 段 cron 表达式（秒 分 时 日 月 周），周日的 7 归一为 0。
    ///
    /// 段数不为 6 或某段取值非法时返回 `Err`（错误信息带示例）。
    pub fn parse(expr: &str) -> Result<Cron, String> {
        let fields: Vec<&str> = expr.split_whitespace().collect();
        if fields.len() != 6 {
            return Err(format!(
                "Cron must have 6 fields (second minute hour dom month dow), got {}. \
                 Example: \"0 0 9 * * 1\" for 9am every Monday.",
                fields.len()
            ));
        }

        let dom_raw = fields[3];
        let dow_raw = fields[5];

        // 周日既写 0 也写 7：把 7 折成 0
        let dows = parse_field(fields[5], 0, 7)?
            .into_iter()
            .map(|d| if d == 7 { 0 } else { d })
            .collect();

        Ok(Cron {
            secs: parse_field(fields[0], 0, 59)?,
            mins: parse_field(fields[1], 0, 59)?,
            hours: parse_field(fields[2], 0, 23)?,
            doms: parse_field(fields[3], 1, 31)?,
            months: parse_field(fields[4], 1, 12)?,
            dows,
            dom_restricted: dom_raw.trim() != "*",
            dow_restricted: dow_raw.trim() != "*",
        })
    }

    /// `after` 之后（严格大于，秒粒度）的下一次触发。
    pub fn next_after(&self, after: SystemTime) -> Option<SystemTime> {
        // 从"下一秒"开始找；给 4 年上限（闰年 + 2 月 29 日这种罕见组合也够）
        let start_ms = system_to_ms(after) + 1000;
        let start = Local
            .timestamp_millis_opt(start_ms as i64)
            .single()?
            .naive_local();
        let start_date = start.date();

        for day_offset in 0..=(366 * 4) {
            let date = start_date.checked_add_signed(chrono::Duration::days(day_offset))?;
            if !self.matches_day(date) {
                continue;
            }

            for h in &self.hours {
                for m in &self.mins {
                    for s in &self.secs {
                        let naive = date.and_hms_opt(*h, *m, *s)?;
                        // 本地时区下可能不存在（夏令时跳过的钟点）或重复；取最早的那个
                        let Some(dt) = Local.from_local_datetime(&naive).earliest() else {
                            continue;
                        };

                        if dt.timestamp_millis() as u64 > system_to_ms(after) {
                            return Some(ms_to_system(dt.timestamp_millis().max(0) as u64));
                        }
                    }
                }
            }
        }
        None
    }

    /// 判断某日期是否命中月份与「日/周」条件。
    ///
    /// 日、周都受限时取或（cron 惯例），否则只看受限的那个；都不受限则恒为真。
    fn matches_day(&self, date: NaiveDate) -> bool {
        if !self.months.contains(&date.month()) {
            return false;
        }

        let dom_ok = self.doms.contains(&date.day());

        // chrono 的 weekday：周一=0 … 周日=6 → cron 的周日=0
        let dow = date.weekday().num_days_from_sunday();
        let dow_ok = self.dows.contains(&dow);

        match (self.dom_restricted, self.dow_restricted) {
            // 两个都受限 → 或（cron 的老规矩）；否则只看受限的那个
            (true, true) => dom_ok || dow_ok,
            (true, false) => dom_ok,
            (false, true) => dow_ok,
            (false, false) => true,
        }
    }
}

/// 一次 cron 的下一次触发（本地时间）。**没有**下一次（如 2 月 31 日）返回 `None`。
pub fn next_cron_after(expr: &str, after: SystemTime) -> Option<SystemTime> {
    Cron::parse(expr).ok()?.next_after(after)
}

/// 解析一段：`*`、`a`、`a-b`、`*/n`、`a-b/n`、逗号列表。
fn parse_field(field: &str, min: u32, max: u32) -> Result<Vec<u32>, String> {
    let mut out: Vec<u32> = Vec::new();
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("empty cron field part in {field:?}"));
        }

        let (range, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| format!("invalid cron step {part:?}"))?;
                if step == 0 {
                    return Err(format!("cron step must be > 0 in {part:?}"));
                }
                (r, step)
            }
            None => (part, 1),
        };

        let (lo, hi) = if range == "*" {
            (min, max)
        } else if let Some((a, b)) = range.split_once('-') {
            let a: u32 = a
                .parse()
                .map_err(|_| format!("invalid cron range {part:?}"))?;
            let b: u32 = b
                .parse()
                .map_err(|_| format!("invalid cron range {part:?}"))?;
            if a > b {
                return Err(format!("cron range start > end in {part:?}"));
            }
            (a, b)
        } else {
            let v: u32 = range
                .parse()
                .map_err(|_| format!("invalid cron value {part:?}"))?;
            (v, v)
        };

        if lo < min || hi > max {
            return Err(format!("cron value out of range ({min}-{max}) in {part:?}"));
        }

        let mut v = lo;
        while v <= hi {
            out.push(v);
            v += step;
        }
    }

    out.sort_unstable();
    out.dedup();

    if out.is_empty() {
        return Err(format!("cron field {field:?} matches nothing"));
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::utils::time::now_ms;
    use chrono::Timelike;

    #[test]
    fn parses_fields_and_rejects_bad_shapes() {
        let c = Cron::parse("*/15 0 9-17 * * 1,3,5").unwrap();
        assert_eq!(c.secs, vec![0, 15, 30, 45]);
        assert_eq!(c.mins, vec![0]);
        assert_eq!(c.hours, (9..=17).collect::<Vec<u32>>());
        assert_eq!(c.dows, vec![1, 3, 5]);
        // 周日写 0 或 7 都行
        assert_eq!(Cron::parse("0 0 9 * * 7").unwrap().dows, vec![0]);
        assert_eq!(Cron::parse("0 0 9 * * 0").unwrap().dows, vec![0]);
        // 段数不对 / 越界 / 步长为 0
        assert!(Cron::parse("0 9 * * 1").is_err());
        assert!(Cron::parse("0 0 25 * * *").is_err());
        assert!(Cron::parse("*/0 * * * * *").is_err());
    }

    #[test]
    fn next_run_uses_local_wall_clock() {
        // 每周一 9:00：从某天的任意时刻起，下一次必定是周一 9:00:00
        let next =
            next_cron_after("0 0 9 * * 1", SystemTime::now()).expect("每周一 9 点一定有下一次");
        let dt = Local
            .timestamp_millis_opt(system_to_ms(next) as i64)
            .unwrap();
        assert_eq!(dt.hour(), 9);
        assert_eq!(dt.minute(), 0);
        assert_eq!(dt.second(), 0);
        assert_eq!(dt.weekday().num_days_from_sunday(), 1, "周一");
        assert!(system_to_ms(next) > now_ms());

        // 每秒：下一次就在 1 秒内（`saturating_sub`：两次取"现在"之间可能跨过整秒）
        let next = next_cron_after("* * * * * *", SystemTime::now()).unwrap();
        let delta = system_to_ms(next).saturating_sub(now_ms());
        assert!(delta <= 2000, "delta={delta}");

        // 2 月 30 日永远不会到 → None（不会死循环）
        assert!(next_cron_after("0 0 9 30 2 *", SystemTime::now()).is_none());
    }
}

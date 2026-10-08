use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone};

/// 当前 Unix 时间戳（毫秒）；系统时钟早于 epoch 时返回 0。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 当前 Unix 时间戳（秒）；系统时钟早于 epoch 时返回 0。
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 当前 UTC 时间的 ISO-8601 字符串（毫秒 3 位，以 Z 结尾）。
pub fn now_iso() -> String {
    let ms = now_ms();
    let secs = ms / 1000;
    let millis = ms % 1000;
    let days = secs / 86400;
    let (mut y, m, d) = civil_from_days(days as i64);
    let rem = secs % 86400;
    let hh = rem / 3600;
    let mm = (rem % 3600) / 60;
    let ss = rem % 60;

    // UTC ISO 格式（毫秒 3 位）
    if y < 1000 {
        y += 2000;
    }
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, m, d, hh, mm, ss, millis
    )
}

/// 把 Unix 毫秒时间戳格式化为 UTC ISO-8601 字符串（毫秒 3 位）。
/// 允许负时间戳（epoch 之前），按欧几里得除法取整。
pub fn chrono_ms_to_iso(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86400);
    let secs_of_day = secs.rem_euclid(86400);

    // 从 1970-01-01 推算（简化：仅需唯一可排序文件名；用合法 ISO）
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (
        secs_of_day / 3600,
        (secs_of_day % 3600) / 60,
        secs_of_day % 60,
    );

    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, mo, d, h, mi, s, millis
    )
}

/// 日志行的时间（**本地**时区 `HH:MM:SS`）。
///
/// 面板上展示的是用户看到的钟点，必须走本地时区；越界时间戳退回 `--:--:--`。
pub fn format_clock(at_ms: u64) -> String {
    let secs = (at_ms / 1000) as i64;
    let nanos = ((at_ms % 1000) * 1_000_000) as u32;
    match chrono::DateTime::from_timestamp(secs, nanos) {
        Some(utc) => utc.with_timezone(&Local).format("%H:%M:%S").to_string(),
        None => "--:--:--".to_string(),
    }
}

/// 将 Unix epoch 起的天数转为 (年, 月, 日)（Howard Hinnant 民用历算法）。
pub fn civil_from_days(z: i64) -> (u64, u64, u64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((if m <= 2 { y + 1 } else { y }) as u64, m, d)
}

/// 从(年,月,日)计算 Unix epoch 起的天数（Howard Hinnant 算法逆运算）。
pub fn civil_to_days(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let mp = (m + 9) % 12; // [0, 11]
    let doy = (153 * mp + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// `SystemTime` → 自 Unix epoch 起的毫秒（早于 epoch 记 0）。
pub fn system_to_ms(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 自 Unix epoch 起的毫秒 → `SystemTime`。
pub fn ms_to_system(ms: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_millis(ms)
}

/// `10s`/`5m`/`1h`/`2d` → 毫秒；零、空串、未知单位都是 `None`。
pub fn parse_duration_ms(s: &str) -> Option<u64> {
    let (digits, unit) = s.split_at(s.len().checked_sub(1)?);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }

    let n: u64 = digits.parse().ok()?;
    if n == 0 {
        return None;
    }

    let scale = match unit {
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        _ => return None,
    };
    n.checked_mul(scale)
}

/// 时间戳字符串 → 自 Unix epoch 起的毫秒。
///
/// 先当 RFC3339（带偏移，如 `2026-02-14T09:00:00+08:00`）解析；不行再当**无时区的本地时间**
/// （`2026-02-14T09:00:00`）解析——用户手写的钟点就是本地的钟点。
///
/// （与 [`parse_iso_ms`] 不同：那条是 UTC 的纯数字扫描，这条走 chrono 的偏移/本地时区。）
pub fn parse_timestamp_ms(s: &str) -> Option<u64> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis().max(0) as u64);
    }
    let naive = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S").ok()?;
    let dt = Local.from_local_datetime(&naive).earliest()?;
    Some(dt.timestamp_millis().max(0) as u64)
}

/// epoch ms → RFC3339（**本地**时区，带偏移）；越界的时间戳退回纯数字毫秒串。
pub fn epoch_ms_to_local_rfc3339(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let nanos = ((ms % 1000) * 1_000_000) as u32;
    match chrono::DateTime::from_timestamp(secs, nanos) {
        Some(utc) => utc.with_timezone(&Local).to_rfc3339(),
        None => ms.to_string(),
    }
}

/// 生成 `days` 天前的 RFC3339 UTC 时间戳（秒级精度，`YYYY-MM-DDTHH:MM:SSZ`）。
///
/// 以当前时间为基准往前推 `days * 86400` 秒；`days` 为负值时得到未来时间。
pub fn rfc3339_days_ago(days: i64) -> String {
    let target = (now_secs() as i64) - days * 86_400;
    let days_since_epoch = target.div_euclid(86_400);
    let seconds_of_day = target.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days_since_epoch);
    let hour = seconds_of_day / 3_600;
    let minute = (seconds_of_day % 3_600) / 60;
    let second = seconds_of_day % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// 解析 UTC ISO 时间戳（YYYY-MM-DDTHH:MM:SS.mmmZ）为自 Unix epoch 起的毫秒。
pub fn parse_iso_ms(ts: &str) -> Option<u64> {
    // 2026-08-14T10:17:52.957Z
    let digits: Vec<u32> = ts
        .chars()
        .filter(|c| c.is_ascii_digit())
        .map(|c| c.to_digit(10).unwrap())
        .collect();
    if digits.len() < 14 {
        return None;
    }
    let y = (digits[0] * 1000 + digits[1] * 100 + digits[2] * 10 + digits[3]) as i64;
    let mo = (digits[4] * 10 + digits[5]) as i64;
    let d = (digits[6] * 10 + digits[7]) as i64;
    let hh = (digits[8] * 10 + digits[9]) as i64;
    let mm = (digits[10] * 10 + digits[11]) as i64;
    let ss = (digits[12] * 10 + digits[13]) as i64;
    let milli = if digits.len() >= 17 {
        digits[14] * 100 + digits[15] * 10 + digits[16]
    } else {
        0
    } as i64;
    let days = civil_to_days(y, mo, d);
    let secs = days * 86400 + hh * 3600 + mm * 60 + ss;
    Some((secs as u64) * 1000 + milli as u64)
}

/// 相对时间（"in 4h" / "2d ago" / "—"）
pub fn rel_time(ms: Option<u64>) -> String {
    let Some(ms) = ms else {
        return "—".to_string();
    };

    let now = now_ms() as i64;
    let diff = ms as i64 - now;
    let abs = diff.unsigned_abs();
    let future = diff > 0;

    let text = if abs < 60_000 {
        "<1m".to_string()
    } else if abs < 3_600_000 {
        format!("{}m", abs / 60_000)
    } else if abs < 86_400_000 {
        format!("{}h", abs / 3_600_000)
    } else {
        format!("{}d", abs / 86_400_000)
    };

    if future {
        format!("in {text}")
    } else {
        format!("{text} ago")
    }
}

/// 紧凑时长标签（`340ms` / `9s` / `1m12s`）。
pub fn format_duration(ms: u64) -> String {
    format_duration_ms_precise::<0>(ms)
}

/// 供人阅读的时长（秒部分默认 1 位小数，如 `42.1s`）。
///
/// 与紧凑的 [`format_duration`] 不同：这里保留小数，且按 `ms` 原值分档
/// （`59_600` → `59.6s`，不会进位成 `1m00s`）。
pub fn format_duration_ms(ms: u64) -> String {
    format_duration_ms_precise::<1>(ms)
}

/// 指定秒部分小数位数的时长格式化（`format_duration_ms` 默认 1 位小数）。
/// 函数 const 泛型不能带默认值，故通过该函数显式指定。
pub fn format_duration_ms_precise<const PRECISION: u8>(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.p$}s", ms as f64 / 1000.0, p = PRECISION as usize)
    } else {
        format!(
            "{}m{:.p$}s",
            ms / 60_000,
            (ms % 60_000) as f64 / 1000.0,
            p = PRECISION as usize
        )
    }
}

/// 把秒数格式化为紧凑的耗时文本：`<60s` 用秒，`<60m` 用分，更久用 `h m`。
pub fn format_elapsed(seconds: u64) -> String {
    if seconds < 60 {
        return format!("{}s", seconds);
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{}m", minutes);
    }
    let hours = minutes / 60;
    let rem = minutes % 60;
    if rem > 0 {
        format!("{}h {}m", hours, rem)
    } else {
        format!("{}h", hours)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 紧凑标签的形状是上游契约（`workflow/progress.ts`），别顺手改成带小数。
    #[test]
    fn format_duration_matches_upstream_shape() {
        assert_eq!(format_duration(340), "340ms");
        assert_eq!(format_duration(9_400), "9s");
        assert_eq!(format_duration(72_000), "1m12s");
        assert_eq!(format_duration(4_000), "4s");
    }

    /// 与紧凑版刻意分档不同：这里按原值保留小数、不进位。
    #[test]
    fn format_duration_ms_keeps_decimals_and_does_not_carry() {
        assert_eq!(format_duration_ms(340), "340ms");
        assert_eq!(format_duration_ms(9_400), "9.4s");
        assert_eq!(format_duration_ms(59_600), "59.6s");
        assert_eq!(format_duration_ms(72_100), "1m12.1s");
    }

    #[test]
    fn formats_clock_in_local_time() {
        // 期望值同样用本地时区推算，保证在任意 TZ 下都成立
        let expected = |secs: i64| {
            chrono::DateTime::from_timestamp(secs, 0)
                .unwrap()
                .with_timezone(&Local)
                .format("%H:%M:%S")
                .to_string()
        };
        assert_eq!(format_clock(0), expected(0));
        assert_eq!(format_clock(3_661_000), expected(3_661));
    }

    #[test]
    fn rfc3339_days_ago_roundtrips_and_formats() {
        let start = rfc3339_days_ago(7);
        // 形如 2026-08-07T10:17:52Z（秒级、无毫秒、UTC）
        assert_eq!(start.len(), 20, "{start}");
        assert!(start.ends_with('Z'), "{start}");
        let parsed = parse_iso_ms(&start).expect("parseable ISO");
        let now = now_secs();
        let expected = now - 7 * 86_400;
        // now_secs 在调用前后可能跳动 1 秒
        assert!(
            parsed / 1000 == expected
                || parsed / 1000 + 1 == expected
                || parsed / 1000 == expected + 1,
            "parsed={} expected={}",
            parsed / 1000,
            expected
        );
    }

    #[test]
    fn civil_days_roundtrip() {
        for days in [-1i64, 0, 1, 10_000, 20_000, 30_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(civil_to_days(y as i64, m as i64, d as i64), days);
        }
    }

    #[test]
    fn system_time_conversions_roundtrip() {
        let now = SystemTime::now();
        let ms = system_to_ms(now);
        // 毫秒精度内往返（`now` 的亚毫秒部分会被截掉）
        assert!(system_to_ms(ms_to_system(ms)) == ms, "{ms}");
        assert!(ms > 1_600_000_000_000, "now 应该是 2020 年之后：{ms}");
        // 早于 epoch → 0（不 panic）
        assert_eq!(system_to_ms(UNIX_EPOCH - Duration::from_secs(1)), 0);
    }

    #[test]
    fn durations_parse_units_and_reject_junk() {
        assert_eq!(parse_duration_ms("30s"), Some(30_000));
        assert_eq!(parse_duration_ms("5m"), Some(300_000));
        assert_eq!(parse_duration_ms("1h"), Some(3_600_000));
        assert_eq!(parse_duration_ms("2d"), Some(172_800_000));
        // 零、空串、只有单位、单位不对、溢出
        assert_eq!(parse_duration_ms("0m"), None);
        assert_eq!(parse_duration_ms(""), None);
        assert_eq!(parse_duration_ms("m"), None);
        assert_eq!(parse_duration_ms("5w"), None);
        assert_eq!(parse_duration_ms("-5m"), None);
        assert_eq!(parse_duration_ms(&format!("{}d", u64::MAX)), None);
    }

    #[test]
    fn timestamps_parse_with_offset_and_as_local() {
        // 带偏移：偏移参与换算
        let utc = parse_timestamp_ms("2026-02-14T09:00:00Z").unwrap();
        let plus8 = parse_timestamp_ms("2026-02-14T09:00:00+08:00").unwrap();
        assert_eq!(utc - plus8, 8 * 3_600_000);
        // 无时区：当本地时间（与本地格式化反过来一致）
        let ms = parse_timestamp_ms("2026-02-14T09:00:00").unwrap();
        let local = Local.timestamp_millis_opt(ms as i64).unwrap();
        assert_eq!(
            local.format("%Y-%m-%dT%H:%M:%S").to_string(),
            "2026-02-14T09:00:00"
        );
        // 都不是 → None
        assert_eq!(parse_timestamp_ms("yesterday"), None);
        assert_eq!(parse_timestamp_ms("2026-02-14"), None);
        // epoch ms → 本地 RFC3339 → 再解回来
        let iso = epoch_ms_to_local_rfc3339(ms);
        assert_eq!(parse_timestamp_ms(&iso), Some(ms), "{iso}");
    }
}

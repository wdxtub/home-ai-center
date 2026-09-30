//! 「每日启用禁用时间窗」。
//!
//! LLM 节点与 ComfyUI 端点**共用同一套字段与判定口径**（与 `erotic_sci`
//! 的 `in_disabled_hours` 一致）：起止小时 + IANA 时区，支持跨午夜。
//!
//! 判定时刻取服务器当前时间，跨过时段边界的下一个请求自动生效——
//! 无需重启，也不改配置。

use chrono::{Local, TimeZone, Timelike};
use serde::{Deserialize, Serialize};
use chrono_tz::Tz;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisabledWindow {
    pub start: Option<u8>,
    pub end: Option<u8>,
    pub timezone: Option<String>,
}

impl DisabledWindow {
    pub fn new(start: Option<u8>, end: Option<u8>, timezone: Option<String>) -> Self {
        Self {
            start,
            end,
            timezone,
        }
    }

    pub fn is_configured(&self) -> bool {
        self.start.is_some() && self.end.is_some()
    }

    /// 当前时刻是否落在禁用时段内。
    ///
    /// - `start < end`：普通区间，如 1..6。
    /// - `start > end`：**跨午夜**，如 23..6 表示 23:00 到次日 06:00。
    /// - `start == end`：写入校验已禁止（等价永久禁用），这里按「全天禁用」处理。
    pub fn in_disabled_hours(&self) -> bool {
        self.in_disabled_hours_at(Local::now())
    }

    pub fn in_disabled_hours_at<Tz: TimeZone>(&self, now: chrono::DateTime<Tz>) -> bool {
        let (Some(start), Some(end)) = (self.start, self.end) else {
            return false;
        };
        let tz = match self.parse_tz() {
            Some(tz) => tz,
            None => return false,
        };
        let current = now.with_timezone(&tz).hour() as u8;
        if start == end {
            return true;
        }
        if start < end {
            current >= start && current < end
        } else {
            // 跨午夜
            current >= start || current < end
        }
    }

    fn parse_tz(&self) -> Option<Tz> {
        self.timezone.as_deref().and_then(|s| s.parse::<Tz>().ok())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn w(start: u8, end: u8) -> DisabledWindow {
        DisabledWindow::new(Some(start), Some(end), Some("Asia/Shanghai".into()))
    }

    #[test]
    fn unconfigured_window_never_disables() {
        let d = DisabledWindow::default();
        assert!(!d.in_disabled_hours());
        assert!(!d.is_configured());
    }

    #[test]
    fn normal_range_inclusive_start_exclusive_end() {
        let d = w(1, 6);
        let at = |h: u32| Local.with_ymd_and_hms(2026, 9, 30, h, 0, 0).unwrap();
        assert!(!d.in_disabled_hours_at(at(0)));
        assert!(d.in_disabled_hours_at(at(1))); // 起点包含
        assert!(d.in_disabled_hours_at(at(5)));
        assert!(!d.in_disabled_hours_at(at(6))); // 终点不包含
    }

    #[test]
    fn range_crossing_midnight() {
        let d = w(23, 6);
        let at = |h: u32| Local.with_ymd_and_hms(2026, 9, 30, h, 0, 0).unwrap();
        assert!(d.in_disabled_hours_at(at(23)));
        assert!(d.in_disabled_hours_at(at(2))); // 次日凌晨仍应命中
        assert!(!d.in_disabled_hours_at(at(6)));
        assert!(!d.in_disabled_hours_at(at(12)));
    }

    #[test]
    fn equal_bounds_treated_as_all_day_disabled() {
        // 写入校验禁止这种配置，读取侧按「全天禁用」处理
        let d = w(3, 3);
        let at = |h: u32| Local.with_ymd_and_hms(2026, 9, 30, h, 0, 0).unwrap();
        assert!(d.in_disabled_hours_at(at(3)));
        assert!(d.in_disabled_hours_at(at(15)));
    }

    #[test]
    fn unknown_timezone_falls_back_to_never_disabled() {
        let d = DisabledWindow::new(Some(1), Some(6), Some("Not/AZone".into()));
        assert!(!d.in_disabled_hours());
    }

    #[test]
    fn different_timezones_shift_the_boundary() {
        let sh = w(1, 6); // Asia/Shanghai
        let utc = DisabledWindow::new(Some(1), Some(6), Some("UTC".into()));
        // 上海 01:00 == UTC 前一天 17:00
        let at = Local.with_ymd_and_hms(2026, 9, 30, 1, 0, 0).unwrap();
        assert!(sh.in_disabled_hours_at(at));
        assert!(!utc.in_disabled_hours_at(at));
    }
}

//! 重置时间解析：5 种格式 + 合理性钳制。
//!
//! 各家给重置时间的方式完全不同，解析器必须同时吃下：
//!
//! | 来源 | 格式 | 例 |
//! |---|---|---|
//! | HTTP `Retry-After` | 整数秒 | `30` |
//! | HTTP `Retry-After` | HTTP-date | `Wed, 21 Oct 2026 07:28:00 GMT` |
//! | OpenAI `x-ratelimit-reset-*` | **Go duration** | `6m0s` |
//! | Anthropic `anthropic-ratelimit-*-reset` | RFC 3339 | `2026-10-21T07:28:00Z` |
//! | 消息体内 | `\|<unix epoch>` | `Claude AI usage limit reached\|1790000000` |
//!
//! 解析结果一律钳制到 `now < reset ≤ now + 8 天`；越界视为损坏，
//! 调用方降级用 `assumed`。`Retry-After` 额外截到 60 秒——
//! 上游可能返回荒谬值，无脑照等能把请求挂死。

use super::{MAX_RESET_HORIZON, MAX_RETRY_AFTER};

/// 重置时间的来源。`Assumed` 是生产环境最容易出事的状态，
/// 管理页面要能一眼把它标出来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetSource {
    /// 来自响应头。
    Header,
    /// 来自错误消息体。
    Message,
    /// 本地按日历规则算出（OpenAI 花费类 → 下月 1 日）。
    Computed,
    /// 上游没给，用默认值兜的。
    Assumed,
}

impl ResetSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ResetSource::Header => "header",
            ResetSource::Message => "message",
            ResetSource::Computed => "computed",
            ResetSource::Assumed => "assumed",
        }
    }
}

pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 合理性钳制：越界即视为损坏，返回 `None` 让调用方降级。
pub fn clamp_reset(at: i64) -> Option<i64> {
    let now = now_unix();
    let horizon = now + MAX_RESET_HORIZON.as_secs() as i64;
    if at > now && at <= horizon {
        Some(at)
    } else {
        None
    }
}

/// 从消息文本里抠 `|<unix epoch>`。
pub fn parse_reset_from_message(msg: &str) -> Option<i64> {
    let idx = msg.find('|')?;
    let tail = &msg[idx + 1..];
    // epoch 是 10 位（秒）或 13 位（毫秒）
    let digits: String = tail.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.len() != 10 && digits.len() != 13 {
        return None;
    }
    let n: i64 = digits.parse().ok()?;
    let secs = if digits.len() == 13 { n / 1000 } else { n };
    clamp_reset(secs)
}

/// 从 `You will regain access on 2026-10-01 at 00:00 UTC.` 这类句子里取日期。
/// 按 UTC 零点解释。
pub fn parse_natural_date(msg: &str) -> Option<i64> {
    let marker = "regain access on";
    let idx = msg.to_ascii_lowercase().find(marker)?;
    let rest = &msg[idx + marker.len()..];
    let date: String = rest
        .chars()
        .skip_while(|c| c.is_whitespace())
        .take(10)
        .collect();
    let d = chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok()?;
    let dt = d.and_hms_opt(0, 0, 0)?.and_utc();
    clamp_reset(dt.timestamp())
}

/// 下一个月的 1 日 00:00 UTC。
pub fn next_month_utc() -> i64 {
    let now = chrono::Utc::now();
    let (y, m) = if now.month() == 12 {
        (now.year() + 1, 1)
    } else {
        (now.year(), now.month() + 1)
    };
    chrono::NaiveDate::from_ymd_opt(y, m, 1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|dt| dt.and_utc().timestamp())
        .unwrap_or_else(|| now_unix() + MAX_RESET_HORIZON.as_secs() as i64)
}

use chrono::Datelike;

/// 解析 `Retry-After`：整数秒或 HTTP-date。返回**已钳制**的秒数。
pub fn parse_retry_after(raw: &str) -> Option<u64> {
    let t = raw.trim();
    if let Ok(secs) = t.parse::<u64>() {
        return Some(secs.min(MAX_RETRY_AFTER.as_secs()));
    }
    // HTTP-date
    chrono::DateTime::parse_from_rfc2822(t)
        .ok()
        .map(|dt| {
            let delta = dt.timestamp() - now_unix();
            delta.clamp(0, MAX_RETRY_AFTER.as_secs() as i64) as u64
        })
        .or(Some(MAX_RETRY_AFTER.as_secs()))
}

/// 解析 Go duration（OpenAI 的 `x-ratelimit-reset-*` 是 `6m0s` 这种
/// **复合**格式，不是单个「数字+单位」）。
///
/// 逐段扫描 `<数字><单位>`，把各段累加成秒。
pub fn parse_go_duration(raw: &str) -> Option<i64> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let (t, negative) = match t.strip_prefix('-') {
        Some(rest) => (rest, true),
        None => (t.strip_prefix('+').unwrap_or(t), false),
    };

    let bytes: Vec<char> = t.chars().collect();
    let mut i = 0usize;
    let mut total = 0.0f64;
    let mut any = false;

    while i < bytes.len() {
        // 数字（含小数点）
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == '.') {
            i += 1;
        }
        if start == i {
            return None; // 期望数字却没数字 → 不是合法 duration
        }
        let num: String = bytes[start..i].iter().collect();
        let n: f64 = num.parse().ok()?;

        // 单位
        let ustart = i;
        while i < bytes.len() && !bytes[i].is_ascii_digit() && bytes[i] != '.' {
            i += 1;
        }
        let unit: String = bytes[ustart..i].iter().collect();
        let mult = match unit.as_str() {
            "ns" => 1e-9,
            "us" | "\u{b5}s" | "µs" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        total += n * mult;
        any = true;
    }

    if !any {
        return None;
    }
    let v = total.round() as i64;
    Some(if negative { -v } else { v })
}

/// 解析 RFC 3339 时间戳（Anthropic 的 `anthropic-ratelimit-*-reset`）。
pub fn parse_rfc3339(raw: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(raw.trim())
        .ok()
        .map(|dt| dt.timestamp())
}

/// 解析任意一种 reset header：先试 Go duration，再试 RFC 3339，最后试秒数。
pub fn parse_reset_header(raw: &str) -> Option<i64> {
    if let Some(s) = parse_go_duration(raw) {
        return clamp_reset(now_unix() + s);
    }
    if let Some(t) = parse_rfc3339(raw) {
        return clamp_reset(t);
    }
    raw.trim()
        .parse::<i64>()
        .ok()
        .and_then(|t| clamp_reset(now_unix() + t))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Timelike;

    #[test]
    fn epoch_in_message_is_parsed_and_clamped() {
        let future = now_unix() + 3600;
        let msg = format!("Claude AI usage limit reached|{future}");
        assert_eq!(parse_reset_from_message(&msg), Some(future));
    }

    #[test]
    fn millisecond_epoch_is_normalised() {
        let future = now_unix() + 3600;
        let msg = format!("Claude AI usage limit reached|{}000", future);
        assert_eq!(parse_reset_from_message(&msg), Some(future));
    }

    #[test]
    fn past_or_absurd_epoch_is_rejected() {
        // 已过去
        assert_eq!(parse_reset_from_message("limit|1000000000"), None);
        // 超过 8 天上限 → 视为损坏
        let far = now_unix() + 30 * 24 * 3600;
        assert_eq!(parse_reset_from_message(&format!("limit|{far}")), None);
    }

    #[test]
    fn natural_date_is_read_as_utc_midnight() {
        let msg = "You will regain access on 2026-10-01 at 00:00 UTC.";
        let at = parse_natural_date(msg).unwrap();
        let dt = chrono::DateTime::from_timestamp(at, 0).unwrap();
        assert_eq!((dt.year(), dt.month(), dt.day()), (2026, 10, 1));
        assert_eq!(dt.hour(), 0);
    }

    #[test]
    fn go_duration_formats() {
        assert_eq!(parse_go_duration("30s"), Some(30));
        assert_eq!(parse_go_duration("6m0s"), Some(360));
        assert_eq!(parse_go_duration("1h30m"), Some(5400), "复合单位要正确累加");
        assert_eq!(parse_go_duration("1500ms"), Some(2));
        assert_eq!(parse_go_duration("1.5s"), Some(2));
        assert_eq!(parse_go_duration(""), None);
        assert_eq!(parse_go_duration("abc"), None);
    }

    #[test]
    fn rfc3339_parsing() {
        let future = now_unix() + 7200;
        let s = chrono::DateTime::from_timestamp(future, 0)
            .unwrap()
            .to_rfc3339();
        assert_eq!(parse_rfc3339(&s), Some(future));
    }

    #[test]
    fn reset_header_accepts_all_three_shapes() {
        let future = now_unix() + 120;
        // Go duration
        assert_eq!(parse_reset_header("2m0s"), Some(now_unix() + 120));
        // 整数秒
        assert_eq!(parse_reset_header("120"), Some(now_unix() + 120));
        // RFC 3339
        let s = chrono::DateTime::from_timestamp(future, 0).unwrap().to_rfc3339();
        assert_eq!(parse_reset_header(&s), Some(future));
    }

    #[test]
    fn absurd_retry_after_is_clamped() {
        assert_eq!(parse_retry_after("999999"), Some(60));
    }

    #[test]
    fn http_date_retry_after_is_accepted() {
        let s = chrono::DateTime::from_timestamp(now_unix() + 10, 0)
            .unwrap()
            .to_rfc2822();
        let secs = parse_retry_after(&s).unwrap();
        assert!(secs <= 60);
    }

    #[test]
    fn next_month_is_the_first_of_next_month_utc() {
        let at = next_month_utc();
        let dt = chrono::DateTime::from_timestamp(at, 0).unwrap();
        assert_eq!(dt.day(), 1);
        assert_eq!(dt.hour(), 0);
        assert!(at > now_unix());
    }
}

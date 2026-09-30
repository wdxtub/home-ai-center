//! 限额错误分类：**有序决策表，先匹配先返回**。
//!
//! 排序原则：把「上游明说」的判据放前面，把「推断出来的」放后面，
//! 并把**瞬时**类放最后——这样长窗口的额度耗尽永远不会被误判成每分钟限流。
//!
//! 两条必须守住的不变量：
//! - **5xx / 529 / `slow_down` 绝不判成 key 的问题。** 它们是上游容量信号，
//!   轮换只会烧完 key 池并掩盖真实的容量故障（litellm 正是在这里踩坑：
//!   它按状态码一律给 5 秒冷却，无法区分配额耗尽与每分钟限流）。
//! - **400 分支是承重墙。** Anthropic 文档明确自设 spend limit 返回
//!   **400** 而非 429，只按 429 判定的检测器会整类漏掉——
//!   而这类错误 100% 可以换一把 key 重试。

use super::reset::ResetSource;

/// 限额类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaClass {
    /// 短暂拥塞：等一会儿就能继续，**不该换 key**。
    Transient,
    /// 长窗口额度耗尽：换 key。
    LongQuota,
    /// key 本身不可用了（401/403/欠费/泄露）：隔离，等人工介入。
    KeyDead,
}

impl QuotaClass {
    pub fn as_str(self) -> &'static str {
        match self {
            QuotaClass::Transient => "transient",
            QuotaClass::LongQuota => "long_quota",
            QuotaClass::KeyDead => "key_dead",
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuotaVerdict {
    pub class: QuotaClass,
    /// 命中的决策表编号，写进 `key_rotation_event` 供事后审计。
    pub rule: &'static str,
    /// 触发判定的具体信号（error code / header 名 / 匹配到的子串）。
    pub signal: String,
    /// 恢复时刻（unix 秒）。`None` 表示上游没给。
    pub reset_at: Option<i64>,
    pub reset_source: ResetSource,
}

impl QuotaVerdict {
    fn new(class: QuotaClass, rule: &'static str, signal: impl Into<String>) -> Self {
        Self {
            class,
            rule,
            signal: signal.into(),
            reset_at: None,
            reset_source: ResetSource::Assumed,
        }
    }
    fn with_reset(mut self, at: Option<i64>, src: ResetSource) -> Self {
        self.reset_at = at;
        self.reset_source = src;
        self
    }
}

/// OpenAI 的额度/花费类 error.code。命中即 `LongQuota`。
const OPENAI_QUOTA_CODES: &[&str] = &[
    "credit_balance_exhausted",
    "organization_spend_limit_exceeded",
    "project_spend_limit_exceeded",
    "organization_usage_limit_exceeded",
];

/// Claude Code 5 小时窗口的重置时间就在消息体里：`...|<unix epoch>`。
const CLAUDE_USAGE_LIMIT_MARKER: &str = "Claude AI usage limit reached";

/// 一次上游失败的全部可判据。
pub struct Failure<'a> {
    pub status: u16,
    pub headers: &'a dyn Fn(&str) -> Option<String>,
    pub body: &'a serde_json::Value,
}

/// 判定一次失败。顺序即优先级，**不要重排**。
pub fn classify(f: &Failure<'_>) -> QuotaVerdict {
    let body = f.body;

    // ── #1 Anthropic tier 花费上限 ───────────────────────────────
    if body
        .pointer("/error/details/error_code")
        .and_then(|v| v.as_str())
        == Some("enforced_spend_limit_reached")
    {
        let msg = error_message(body);
        let at = super::reset::parse_reset_from_message(&msg)
            .or_else(|| super::reset::parse_natural_date(&msg));
        return QuotaVerdict::new(
            QuotaClass::LongQuota,
            "R1_anthropic_spend_limit",
            "error.details.error_code=enforced_spend_limit_reached",
        )
        .with_reset(at, source_of(at));
    }

    // ── #2 OpenAI 额度/花费类 ───────────────────────────────────
    if let Some(code) = body.pointer("/error/code").and_then(|v| v.as_str()) {
        if OPENAI_QUOTA_CODES.contains(&code) {
            // 这些都是按月计的，冷却到下月 1 日 00:00 UTC
            let at = super::reset::next_month_utc();
            return QuotaVerdict::new(
                QuotaClass::LongQuota,
                "R2_openai_quota_code",
                format!("error.code={code}"),
            )
            .with_reset(Some(at), ResetSource::Computed);
        }
    }

    // ── #3 Anthropic 自设 spend limit：状态码是 400 不是 429 ──────
    // 承重墙：只按 429 判定的检测器会整类漏掉。
    let msg = error_message(body);
    let lower = msg.to_ascii_lowercase();
    if lower.contains("you have reached your specified")
        && lower.contains("api usage limits")
        && (f.status == 400 || f.status == 429)
    {
        let at = super::reset::parse_reset_from_message(&msg)
            .or_else(|| super::reset::parse_natural_date(&msg));
        return QuotaVerdict::new(
            QuotaClass::LongQuota,
            "R3_anthropic_specified_spend_limit",
            format!("status={} message~'specified API usage limits'", f.status),
        )
        .with_reset(at, source_of(at));
    }

    // ── #4 Claude Code 5 小时窗口 ────────────────────────────────
    if msg.contains(CLAUDE_USAGE_LIMIT_MARKER) {
        let at = super::reset::parse_reset_from_message(&msg);
        return QuotaVerdict::new(
            QuotaClass::LongQuota,
            "R4_claude_code_usage_limit",
            format!("message~'{CLAUDE_USAGE_LIMIT_MARKER}|'"),
        )
        .with_reset(at, source_of(at));
    }

    // ── #5 欠费 / key 泄露 ───────────────────────────────────────
    if f.status == 402 {
        return QuotaVerdict::new(QuotaClass::KeyDead, "R5_payment_required", "status=402");
    }
    if lower.contains("reported as leaked") || lower.contains("use another api key") {
        return QuotaVerdict::new(
            QuotaClass::KeyDead,
            "R5_key_leaked",
            "message~'reported as leaked'",
        );
    }

    // ── #6 OpenAI slow_down：斜率限制，是**容量**信号不是 key 状态 ──
    if body.pointer("/error/code").and_then(|v| v.as_str()) == Some("slow_down") {
        let retry = (f.headers)("retry-after")
            .and_then(|v| v.parse::<u64>().ok())
            .map(|s| s.min(super::MAX_RETRY_AFTER.as_secs() as u64));
        return QuotaVerdict::new(QuotaClass::Transient, "R6_slow_down", "error.code=slow_down")
            .with_reset(
                retry.map(|s| super::unix_now() + s as i64),
                if retry.is_some() { ResetSource::Header } else { ResetSource::Assumed },
            );
    }

    // ── #7 429 + 短 Retry-After：原地等，不轮换 ──────────────────
    if f.status == 429 {
        if let Some(retry) = (f.headers)("retry-after") {
            if let Some(secs) = parse_retry_after(&retry) {
                let secs = secs.min(super::MAX_RETRY_AFTER.as_secs());
                return QuotaVerdict::new(
                    QuotaClass::Transient,
                    "R7_transient_429",
                    format!("retry-after={retry}"),
                )
                .with_reset(Some(super::unix_now() + secs as i64), ResetSource::Header);
            }
        }
        // ── #8 429 但没有 Retry-After：推断成长窗口额度耗尽 ──────
        return QuotaVerdict::new(
            QuotaClass::LongQuota,
            "R8_429_without_retry_after",
            "status=429, no retry-after",
        );
    }

    // ── #9 仅 insufficient_quota 是**弱信号** ───────────────────
    // 官方明确它可能是更宽泛的 error.type，单独出现不足以轮换。
    if body.pointer("/error/type").and_then(|v| v.as_str()) == Some("insufficient_quota") {
        return QuotaVerdict::new(
            QuotaClass::Transient,
            "R9_insufficient_quota_weak",
            "error.type=insufficient_quota",
        );
    }

    // ── #10 key 无效 / 无权限 ───────────────────────────────────
    if f.status == 401 || f.status == 403 {
        return QuotaVerdict::new(QuotaClass::KeyDead, "R10_auth", format!("status={}", f.status));
    }

    // ── #11 上游容量：绝不轮换 ───────────────────────────────────
    if f.status == 408 || f.status == 409 || f.status == 529 {
        return QuotaVerdict::new(
            QuotaClass::Transient,
            "R11_upstream_capacity",
            format!("status={}", f.status),
        );
    }
    if f.status == 503 {
        let overloaded = body
            .pointer("/error/type")
            .and_then(|v| v.as_str())
            .map(|t| t.contains("overloaded") || t.contains("server_is_overloaded"))
            .unwrap_or(false);
        return QuotaVerdict::new(
            QuotaClass::Transient,
            "R11_upstream_capacity",
            if overloaded { "503 overloaded" } else { "status=503" },
        );
    }

    // ── #12 上游 5xx ────────────────────────────────────────────
    if f.status >= 500 {
        return QuotaVerdict::new(
            QuotaClass::Transient,
            "R12_upstream_5xx",
            format!("status={}", f.status),
        );
    }

    // 其余按内容错误处理，不动 key。
    QuotaVerdict::new(
        QuotaClass::Transient,
        "R99_client_error",
        format!("status={}", f.status),
    )
}

fn source_of(at: Option<i64>) -> ResetSource {
    if at.is_some() {
        ResetSource::Message
    } else {
        ResetSource::Assumed
    }
}

fn parse_retry_after(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok()
}

/// 从常见错误体里挖出消息文本。
pub fn error_message(body: &serde_json::Value) -> String {
    body.pointer("/error/message")
        .and_then(|v| v.as_str())
        .or_else(|| body.get("message").and_then(|v| v.as_str()))
        .or_else(|| body.pointer("/error/type").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hdr(v: Option<&str>) -> impl Fn(&str) -> Option<String> + '_ {
        move |k: &str| {
            if k.eq_ignore_ascii_case("retry-after") {
                v.map(str::to_string)
            } else {
                None
            }
        }
    }

    fn verdict(status: u16, body: serde_json::Value, retry_after: Option<&str>) -> QuotaVerdict {
        let h = hdr(retry_after);
        classify(&Failure {
            status,
            headers: &h,
            body: &body,
        })
    }

    // ── #1 ──
    #[test]
    fn r1_anthropic_spend_limit() {
        let v = verdict(
            429,
            json!({"type":"error","error":{"type":"rate_limit_error",
                "message":"You have reached your API usage limits: your organization has crossed its monthly API usage threshold. You will regain access on 2026-10-01 at 00:00 UTC.",
                "details":{"error_code":"enforced_spend_limit_reached"}}}),
            None,
        );
        assert_eq!(v.class, QuotaClass::LongQuota);
        assert_eq!(v.rule, "R1_anthropic_spend_limit");
        assert!(v.reset_at.is_some(), "应从消息里解析出恢复时间");
    }

    // ── #2 ──
    #[test]
    fn r2_openai_quota_codes_cool_until_next_month() {
        for code in OPENAI_QUOTA_CODES {
            let v = verdict(429, json!({"error":{"code":code,"message":"x"}}), None);
            assert_eq!(v.class, QuotaClass::LongQuota, "{code}");
            assert_eq!(v.reset_source, ResetSource::Computed);
            let at = v.reset_at.unwrap();
            // 下一个月的 1 号 00:00 UTC
            assert!(at > super::super::unix_now());
        }
    }

    /// 承重墙：Anthropic 自设 spend limit 返回 **400**。
    /// 只按 429 判定的检测器会整类漏掉，而它 100% 可换 key 重试。
    #[test]
    fn r3_specified_spend_limit_detected_on_400_not_429() {
        let body = json!({"type":"error","error":{"type":"invalid_request_error",
            "message":"You have reached your specified API usage limits. You will regain access on 2026-10-01 at 00:00 UTC."}});
        let v = verdict(400, body.clone(), None);
        assert_eq!(v.class, QuotaClass::LongQuota, "400 的 spend limit 必须被识别");
        assert_eq!(v.rule, "R3_anthropic_specified_spend_limit");

        // 429 也要认
        let v2 = verdict(429, body, None);
        assert_eq!(v2.class, QuotaClass::LongQuota);
    }

    #[test]
    fn r3_does_not_fire_on_other_400() {
        let v = verdict(400, json!({"error":{"message":"bad model name"}}), None);
        assert_eq!(v.class, QuotaClass::Transient);
        assert_ne!(v.rule, "R3_anthropic_specified_spend_limit");
    }

    // ── #4 ──
    #[test]
    fn r4_claude_code_five_hour_window_parses_epoch() {
        // 重置时刻必须落在未来 8 天内，超出会被钳制掉
        let reset = super::super::unix_now() + 3 * 3600;
        let v = verdict(
            400,
            json!({"error":{"message": format!("Claude AI usage limit reached|{reset}")}}),
            None,
        );
        assert_eq!(v.class, QuotaClass::LongQuota);
        assert_eq!(v.rule, "R4_claude_code_usage_limit");
        assert_eq!(v.reset_at, Some(reset));
    }

    // ── #5 ──
    #[test]
    fn r5_payment_required_and_leaked_key_are_dead() {
        assert_eq!(verdict(402, json!({"error":{}}), None).class, QuotaClass::KeyDead);
        let v = verdict(400, json!({"error":{"message":"Your API key was reported as leaked. Please use another API key."}}), None);
        assert_eq!(v.class, QuotaClass::KeyDead);
    }

    // ── #6 ──
    /// 防 litellm 反模式的回归守卫：slow_down 是容量信号，绝不换 key。
    #[test]
    fn r6_slow_down_never_rotates() {
        let v = verdict(429, json!({"error":{"code":"slow_down","message":"x"}}), Some("30"));
        assert_eq!(v.class, QuotaClass::Transient);
        assert_ne!(v.rule, "R8_429_without_retry_after");
    }

    // ── #7 ──
    #[test]
    fn r7_short_retry_after_waits_instead_of_rotating() {
        let v = verdict(429, json!({"error":{"type":"rate_limit_error","message":"x"}}), Some("20"));
        assert_eq!(v.class, QuotaClass::Transient);
        assert_eq!(v.rule, "R7_transient_429");
    }

    #[test]
    fn retry_after_is_clamped_to_sixty_seconds() {
        // 上游可能返回荒谬值
        let v = verdict(429, json!({"error":{"message":"x"}}), Some("99999"));
        let until = v.reset_at.unwrap() - super::super::unix_now();
        assert!(until <= 61, "荒谬的 retry-after 必须被截到 60s，实际 {until}");
    }

    // ── #8 ──
    #[test]
    fn r8_429_without_retry_after_is_long_quota() {
        let v = verdict(429, json!({"error":{"type":"rate_limit_error","message":"x"}}), None);
        assert_eq!(v.class, QuotaClass::LongQuota);
        assert_eq!(v.reset_source, ResetSource::Assumed);
    }

    // ── #9 ──
    #[test]
    fn r9_insufficient_quota_alone_is_too_weak_to_rotate() {
        let v = verdict(429, json!({"error":{"type":"insufficient_quota","message":"x"}}), Some("10"));
        assert_eq!(v.class, QuotaClass::Transient, "弱信号不能单独触发轮换");
    }

    // ── #10 ──
    #[test]
    fn r10_auth_is_dead_not_rotated() {
        assert_eq!(verdict(401, json!({"error":{}}), None).class, QuotaClass::KeyDead);
        assert_eq!(verdict(403, json!({"error":{}}), None).class, QuotaClass::KeyDead);
    }

    /// 第二条不变量：5xx / 529 绝不换 key。
    #[test]
    fn r11_capacity_signals_never_rotate() {
        for status in [408u16, 409, 500, 502, 503, 529] {
            let v = verdict(status, json!({"error":{"message":"x"}}), None);
            assert_eq!(v.class, QuotaClass::Transient, "status {status} 不得触发轮换");
            assert!(!v.rule.starts_with("R2") && !v.rule.starts_with("R8"), "{status}");
        }
    }

    #[test]
    fn r11_overloaded_503_is_capacity() {
        let v = verdict(503, json!({"error":{"type":"server_is_overloaded"}}), None);
        assert_eq!(v.class, QuotaClass::Transient);
    }

    #[test]
    fn client_error_leaves_key_untouched() {
        let v = verdict(400, json!({"error":{"message":"context length exceeded"}}), None);
        assert_eq!(v.class, QuotaClass::Transient);
        assert_eq!(v.rule, "R99_client_error");
    }
}

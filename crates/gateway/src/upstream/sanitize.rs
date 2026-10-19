//! 上游错误文本在回给调用方之前要过一遍。
//!
//! 网关把上游的传输错误原样当 `String` 往上传（`openai.rs` 的
//! `Transport { message }` / `Node { message }`、`comfyui.rs` 的
//! `Endpoint { message }`），而 `reqwest::Error` 的 `Display` **会把完整请求
//! URL 拼进去**：
//!
//! ```text
//! error sending request for url (http://<内网地址>:8188/prompt)
//! ```
//!
//! 于是「上游挂了」这条错误会把内网地址、端口、路径一起送到**任何持有调用方
//! key 的人**面前。调用方拿到它除了知道「服务不可用」之外没有别的用处，
//! 却正好回答了「这台网关后面接着什么」。
//!
//! 这里只去掉不该外泄的标识符，保留能据以行动的部分：HTTP 状态码、错误类别、
//! 超时与否、以及上游自己说的原因（那是排障的关键）。**原文不丢**——
//! 调用方看到净化版，原文照旧进日志。

/// 回给客户端时最多保留多少字符。上游响应体可能有好几 KB，
/// 全量回传等于把上游的内部错误页发给调用方。
const MAX_LEN: usize = 200;

/// URL 后面遇到这些字符就停：reqwest 的 URL 后面通常跟 `)` 或空白。
const URL_STOP: &[char] = &[
    ' ', '\t', '\n', '\r', ')', '(', '"', '\'', ',', '，', '。', '、', ']', '[', '<',
];

/// 上游错误里出现这些前缀，后面紧跟的一串 token 按密钥处理。
///
/// 只认「有前缀的」，不按长度瞎涂：中文错误消息里随便一个长单词都不该被
/// 打成 `***`，否则净化完的提示反而没法用。
const SECRET_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "bearer ",
    "api_key=",
    "api-key=",
    "api_key:",
    "token=",
    "authorization:",
];

/// 上游错误的对外版本。
///
/// 顺序要紧：先脱敏再截断。反过来的话，截断可能把 `https://` 切成半个前缀，
/// 反而漏掉。
pub fn sanitize(message: &str) -> String {
    let s = redact_secrets(&redact_urls(message));
    if s.chars().count() <= MAX_LEN {
        return s;
    }
    let cut: String = s.chars().take(MAX_LEN).collect();
    format!("{cut}…")
}

/// 把 `http(s)://…` 整段换成 `<upstream>`。
fn redact_urls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let at = match find_scheme(rest) {
            Some(i) => i,
            None => {
                out.push_str(rest);
                return out;
            }
        };
        let (head, tail) = rest.split_at(at);
        out.push_str(head);
        let body_len = tail
            .char_indices()
            .find(|(_, c)| URL_STOP.contains(c))
            .map(|(i, _)| i)
            .unwrap_or(tail.len());
        out.push_str("<upstream>");
        rest = &tail[body_len..];
    }
}

/// 大小写不敏感地找 `http://` 或 `https://` 的起点。
fn find_scheme(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    let a = lower.find("http://");
    let b = lower.find("https://");
    match (a, b) {
        (Some(x), Some(y)) => Some(x.min(y)),
        (Some(x), None) => Some(x),
        (None, Some(y)) => Some(y),
        (None, None) => None,
    }
}

/// 把 `sk-…` / `Bearer …` / `api_key=…` 后面的凭证打成 `***`。
///
/// 上游 401 报错时经常把「你发来的 key」原样回显，不脱敏就是直接泄露。
fn redact_secrets(s: &str) -> String {
    // `to_ascii_lowercase` 只映射 ASCII，字节偏移与原串一一对应，
    // 所以可以拿它定位、再回到原串上改。
    let lower = s.to_ascii_lowercase();
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for prefix in SECRET_PREFIXES {
        let mut from = 0usize;
        while let Some(rel) = lower[from..].find(prefix) {
            let start = from + rel;
            let body = start + prefix.len();
            let end = body
                + s[body..]
                    .find(|c: char| {
                        !(c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
                    })
                    .unwrap_or(s.len() - body);
            if end > body {
                ranges.push((body, end));
            }
            from = end.max(start + prefix.len());
            if from >= s.len() {
                break;
            }
        }
    }
    if ranges.is_empty() {
        return s.to_string();
    }
    ranges.sort_unstable();
    ranges.dedup();
    let mut out = String::with_capacity(s.len());
    let mut at = 0usize;
    for (start, end) in ranges {
        if start < at {
            continue;
        }
        out.push_str(&s[at..start]);
        out.push_str("***");
        at = end;
    }
    out.push_str(&s[at..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 传输错误里最常见的形态：reqwest 会把完整 URL 拼进 Display。
    #[test]
    fn url_is_replaced() {
        let msg = "error sending request for url (http://192.168.1.20:8188/prompt)";
        let s = sanitize(msg);
        assert!(!s.contains("192.168.1.20"), "内网地址不该出现在响应里：{s}");
        assert!(!s.contains("8188"), "端口也不该出现：{s}");
        assert!(s.contains("<upstream>"), "{s}");
    }

    #[test]
    fn https_url_is_replaced_too() {
        let s = sanitize("failed to connect: https://node-a.example.org/v1/chat/completions");
        assert!(!s.contains("node-a.example.org"), "{s}");
    }

    /// 上游 401 常把「你发来的 key」原样回显，不脱敏就是直接泄露。
    #[test]
    fn echoed_key_is_redacted() {
        let s = sanitize("invalid api key: sk-abc123def456ghi789");
        assert!(!s.contains("abc123def456"), "{s}");
        assert!(s.contains("invalid api key"), "原因要留着：{s}");
    }

    #[test]
    fn bearer_header_value_is_redacted() {
        let s = sanitize("Authorization: Bearer sk-hac-zzz rejected");
        assert!(!s.contains("sk-hac-zzz"), "{s}");
    }

    /// 脱敏不能把正常中文提示打成马赛克——那是净化过头。
    #[test]
    fn plain_message_survives_intact() {
        let msg = "模型正在加载，请重试";
        assert_eq!(sanitize(msg), msg);
    }

    #[test]
    fn long_body_is_truncated() {
        let s = sanitize(&"x".repeat(5000));
        assert!(
            s.chars().count() <= MAX_LEN + 1,
            "应被截断：{}",
            s.chars().count()
        );
        assert!(s.ends_with('…'));
    }

    /// 先脱敏再截断：反过来会把 `https://` 切成半个前缀，反而漏掉。
    #[test]
    fn truncation_never_splits_a_secret() {
        let s = sanitize(&format!(
            "{}{}",
            "y".repeat(MAX_LEN - 3),
            "sk-leakedkeyvalue"
        ));
        assert!(!s.contains("leakedkeyvalue"), "{s}");
    }

    /// 多个 URL 都要换掉，不能只换第一个。
    #[test]
    fn every_url_is_replaced() {
        let s = sanitize("tried http://192.168.1.20:1234/v1, then https://b.example.org/v1");
        assert!(!s.contains("192.168.1.20"), "{s}");
        assert!(!s.contains("b.example.org"), "{s}");
    }
}

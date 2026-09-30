//! Token 估算。
//!
//! 两个地方共用它：
//! 1. **计费预扣**——准入时还没有真实 usage，必须估一个最大费用做预扣；
//! 2. **Anthropic `message_start`**——该事件要求在模型还没生成任何内容时
//!    就给出输入 token 数，而上游 Chat 只在流结束时才报 `prompt_tokens`。
//!
//! 估算与真实值有偏差，因此**估算结果一律记 `tokens_source=estimated`**，
//! 结算仍以上游真实 usage 为准。

use crate::protocol::ir::UnifiedRequest;

/// CJK 字符约等于 1 个 token；ASCII 约 4 个字符 1 个 token。
/// 这是业界常用的粗略系数，误差在 10% 量级，对预扣而言足够。
pub fn estimate_text_tokens(text: &str) -> u32 {
    let mut cjk = 0u32;
    let mut other = 0u32;
    for ch in text.chars() {
        let cp = ch as u32;
        let is_cjk = (0x4E00..=0x9FFF).contains(&cp)   // CJK 统一表意
            || (0x3040..=0x30FF).contains(&cp)         // 日文假名
            || (0xAC00..=0xD7AF).contains(&cp)         // 韩文
            || (0x3000..=0x303F).contains(&cp)         // CJK 标点
            || (0xFF00..=0xFF65).contains(&cp); // 全角
        if is_cjk {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    cjk + other.div_ceil(4)
}

/// 估算整个请求的输入 token 数。
pub fn estimate_request(req: &UnifiedRequest) -> u32 {
    // 每条消息都有角色与分隔开销。
    let overhead = (req.messages.len() as u32) * 4;
    estimate_text_tokens(&req.model)
        + overhead
        + estimate_chars_as_tokens(req.inbound_char_count())
}

/// 按「每 4 字符 1 token」估算一个已量出的字符数。
///
/// 与 [`estimate_text_tokens`] 的 ASCII 分支同系数；入站字符总数在
/// `UnifiedRequest::inbound_char_count()` 里已经算好，这里不再逐字扫描
/// （预扣路径在热路径上，逐字遍历会白烧 CPU）。
pub fn estimate_chars_as_tokens(chars: usize) -> u32 {
    chars.div_ceil(4) as u32
}

/// 估算图片的 token 开销。base64 长度反推字节数后按 1 token / 750 字节近似
/// （OpenAI 的常见经验值），只用于预扣，不会写进账目。
pub fn estimate_image_tokens(base64_len: usize) -> u32 {
    let bytes = base64_len / 4;
    (bytes / 750).max(1) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cjk_costs_about_one_token_per_char() {
        assert_eq!(estimate_text_tokens("你好世界"), 4);
    }

    #[test]
    fn ascii_costs_about_one_token_per_four_chars() {
        assert_eq!(estimate_text_tokens("abcd"), 1);
        assert_eq!(estimate_text_tokens("abcde"), 2);
    }

    #[test]
    fn empty_text_is_zero() {
        assert_eq!(estimate_text_tokens(""), 0);
    }

    #[test]
    fn mixed_text_counts_both_scripts() {
        // 2 个 CJK + 4 个 ASCII = 2 + 1
        assert_eq!(estimate_text_tokens("你好abcd"), 3);
    }
}

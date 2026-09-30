//! 定价：按模型 / 按工作流分别配置，带全局默认值兜底。

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// 单价单位统一为**每 1000 token 的微元**。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelPrice {
    pub input_micro_per_1k: i64,
    pub output_micro_per_1k: i64,
    /// `None` 表示按输入价计（缓存命中通常更便宜，但不强制）。
    pub cached_input_micro_per_1k: Option<i64>,
}

impl ModelPrice {
    pub fn cached(&self) -> i64 {
        self.cached_input_micro_per_1k
            .unwrap_or(self.input_micro_per_1k)
    }
}

/// 全局默认价 + 模型级覆盖。没有任何价格就拒绝请求——
/// 不允许「没配价就免费白嫖」。
#[derive(Debug, Clone, Default)]
pub struct PriceTable {
    pub default_model: Option<ModelPrice>,
    pub by_model: HashMap<String, ModelPrice>,
    /// 工作流级图片单价（微元 / 张）。
    pub default_image_micro: i64,
    pub by_workflow: HashMap<String, i64>,
}

impl PriceTable {
    pub fn model_price(&self, model: &str) -> Option<ModelPrice> {
        self.by_model.get(model).copied().or(self.default_model)
    }

    pub fn image_price(&self, workflow: &str) -> i64 {
        self.by_workflow
            .get(workflow)
            .copied()
            .unwrap_or(self.default_image_micro)
    }

    /// LLM 一次调用的费用（微元）。
    ///
    /// 全程整数运算，`i128` 中间量防溢出；最后**一次**四舍五入到微元，
    /// 避免分项各自取整产生累计误差。
    pub fn llm_cost(
        price: &ModelPrice,
        input_tokens: u32,
        cached_tokens: u32,
        output_tokens: u32,
    ) -> i64 {
        let cached = cached_tokens.min(input_tokens) as i128;
        // 缓存部分是输入的一部分：先扣掉，再按普通输入价计剩余部分。
        let uncached = input_tokens as i128 - cached;
        let micros = uncached * price.input_micro_per_1k as i128
            + cached * price.cached() as i128
            + output_tokens as i128 * price.output_micro_per_1k as i128;
        div_round_1000(micros)
    }

    /// 出图费用（微元）。
    pub fn image_cost(&self, workflow: &str, n: u32) -> i64 {
        self.image_price(workflow) * n as i64
    }
}

/// `(x / 1000)` 四舍五入，负数也按「远离零」处理。
///
/// 结果对 `i64` **饱和**：配置里写了一个荒谬的单价时，宁可算出一个
/// 极大但可表示的数，也不要静默回绕成负数或零——负费用会反过来
/// 变成给账号加钱。
pub fn div_round_1000(x: i128) -> i64 {
    let v = if x >= 0 {
        (x + 500) / 1000
    } else {
        -((-x + 500) / 1000)
    };
    v.clamp(i64::MIN as i128, i64::MAX as i128) as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(inp: i64, out: i64, cached: Option<i64>) -> ModelPrice {
        ModelPrice {
            input_micro_per_1k: inp,
            output_micro_per_1k: out,
            cached_input_micro_per_1k: cached,
        }
    }

    #[test]
    fn missing_model_falls_back_to_global_default() {
        let mut t = PriceTable::default();
        t.default_model = Some(p(1000, 2000, None));
        assert_eq!(t.model_price("qwen").unwrap().input_micro_per_1k, 1000);
    }

    #[test]
    fn model_price_overrides_global_default() {
        let mut t = PriceTable::default();
        t.default_model = Some(p(1000, 2000, None));
        t.by_model.insert("qwen".into(), p(50, 100, None));
        assert_eq!(t.model_price("qwen").unwrap().input_micro_per_1k, 50);
    }

    #[test]
    fn absent_price_table_yields_none_so_request_is_rejected() {
        assert!(PriceTable::default().model_price("qwen").is_none());
    }

    #[test]
    fn cached_price_falls_back_to_input_price() {
        assert_eq!(p(100, 200, None).cached(), 100);
        assert_eq!(p(100, 200, Some(10)).cached(), 10);
    }

    #[test]
    fn cost_is_whole_micro_after_single_rounding() {
        // 1000 输入 + 1000 输出，各 1000 微元/1k → 1000 + 1000 = 2000
        assert_eq!(PriceTable::llm_cost(&p(1000, 1000, None), 1000, 0, 1000), 2000);
        // 1 个 token @1000/1k = 1 微元
        assert_eq!(PriceTable::llm_cost(&p(1000, 1000, None), 1, 0, 0), 1);
    }

    /// 缓存口径：`cached` 是 `input` 的子集，不能重复计费。
    #[test]
    fn cached_tokens_are_billed_at_cached_rate_and_not_double_counted() {
        let price = p(1000, 0, Some(100));
        // 100 输入中 60 缓存：40*1000/1000 + 60*100/1000 = 40 + 6 = 46
        assert_eq!(PriceTable::llm_cost(&price, 100, 60, 0), 46);
    }

    #[test]
    fn cached_greater_than_input_is_clamped() {
        let price = p(1000, 0, Some(100));
        // 脏数据不应产生负费用
        assert!(PriceTable::llm_cost(&price, 10, 50, 0) >= 0);
    }

    #[test]
    fn absurd_price_saturates_instead_of_wrapping_negative() {
        let price = p(i64::MAX / 4, i64::MAX / 4, None);
        let cost = PriceTable::llm_cost(&price, 1_000_000, 0, 1_000_000);
        // 荒谬单价必须饱和到 i64::MAX，绝不能回绕成负数
        // （负费用会反过来变成给账号加钱）。
        assert_eq!(cost, i64::MAX);
    }

    #[test]
    fn image_cost_multiplies_by_count() {
        let mut t = PriceTable::default();
        t.default_image_micro = 500;
        t.by_workflow.insert("cover".into(), 1200);
        assert_eq!(t.image_cost("cover", 3), 3600);
        assert_eq!(t.image_cost("other", 2), 1000);
    }

    #[test]
    fn rounding_rounds_half_away_from_zero() {
        assert_eq!(div_round_1000(1500), 2);
        assert_eq!(div_round_1000(1499), 1);
        assert_eq!(div_round_1000(-1500), -2);
    }
}

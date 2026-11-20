//! 模型调用的 token 用量与花费估算（#8「显示花了多少」）。
//!
//! - [`Usage`]：一次（或累计若干次）调用的 token 数。各家返回的用量格式不同，统一由
//!   [`parse_usage`] 收敛：
//!   - OpenAI 兼容：`usage.prompt_tokens` / `completion_tokens`（`prompt_tokens` 已含缓存命中，
//!     命中数在 `prompt_tokens_details.cached_tokens`，DeepSeek 用 `prompt_cache_hit_tokens`）；
//!     流式时开了 `stream_options.include_usage` 的服务方会在最后一个 chunk 带上它。
//!   - Anthropic：`input_tokens` / `output_tokens`，缓存另记在 `cache_read_input_tokens` /
//!     `cache_creation_input_tokens`（**不含**在 `input_tokens` 里，这里加回去，让「输入」口径和 OpenAI 一致）。
//!     流式时 `message_start.message.usage` 给输入、`message_delta.usage` 给（累计的）输出。
//! - 服务方一个字都不报时，调用方用 [`estimate`] 粗估并标 `estimated = true`，界面上写「估算」。
//! - [`ModelPrice`]：按模型名匹配的单价表（美元 / 百万 token），存进 `AgentConfig.prices`，
//!   设置里可改。**没有单价就只显示 token、不显示钱**，绝不瞎报一个价。
//!
//! 金额内部一律用「微美元」（百万分之一美元）的整数记：`token 数 × 每百万 token 单价` 恰好就是微美元，
//! 也让 `Spent` / `BudgetSpent` 保持 `Eq`。

use serde::{Deserialize, Serialize};

/// token 用量。`input_tokens` 含缓存命中 / 写入（两家口径统一成「整段提示词」）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// 其中缓存命中的输入（只展示，不单独计价）
    pub cache_read_tokens: u64,
    /// 其中写入缓存的输入（Anthropic）
    pub cache_write_tokens: u64,
    /// 服务方没报、按字数估的
    pub estimated: bool,
}

impl Usage {
    pub fn is_empty(&self) -> bool {
        self.input_tokens == 0 && self.output_tokens == 0
    }

    /// 累加（多次调用 → 一轮 / 一份契约）。任何一次是估的，合计就算估的。
    pub fn add(&mut self, o: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(o.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(o.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(o.cache_read_tokens);
        self.cache_write_tokens = self.cache_write_tokens.saturating_add(o.cache_write_tokens);
        self.estimated |= o.estimated;
    }

    /// 合并**同一次**调用里分几次报的用量（流式）：同一字段取较大值。
    /// Anthropic 的 `message_delta.usage.output_tokens` 是累计值，取大即最终值；
    /// 输入只在 `message_start` 报一次，后面的 0 不会把它盖掉。
    pub fn merge_max(&mut self, o: &Usage) {
        self.input_tokens = self.input_tokens.max(o.input_tokens);
        self.output_tokens = self.output_tokens.max(o.output_tokens);
        self.cache_read_tokens = self.cache_read_tokens.max(o.cache_read_tokens);
        self.cache_write_tokens = self.cache_write_tokens.max(o.cache_write_tokens);
    }
}

fn num(v: &serde_json::Value, key: &str) -> Option<u64> {
    v.get(key).and_then(|x| x.as_u64())
}

/// 解析一个 `usage` 对象（两家格式都认）。一个认识的字段都没有返回 `None`。
pub fn parse_usage(u: &serde_json::Value) -> Option<Usage> {
    if !u.is_object() {
        return None;
    }
    // OpenAI 兼容
    if let (Some(p), c) = (num(u, "prompt_tokens"), num(u, "completion_tokens")) {
        let cached = u
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(|x| x.as_u64())
            .or_else(|| num(u, "prompt_cache_hit_tokens"))
            .unwrap_or(0);
        return Some(Usage {
            input_tokens: p,
            output_tokens: c.unwrap_or(0),
            cache_read_tokens: cached,
            ..Default::default()
        });
    }
    // Anthropic（以及 OpenAI Responses 风格的 input_tokens / output_tokens）
    let input = num(u, "input_tokens");
    let output = num(u, "output_tokens");
    if input.is_none() && output.is_none() {
        return None;
    }
    let read = num(u, "cache_read_input_tokens").unwrap_or(0);
    let write = num(u, "cache_creation_input_tokens").unwrap_or(0);
    Some(Usage {
        input_tokens: input.unwrap_or(0).saturating_add(read).saturating_add(write),
        output_tokens: output.unwrap_or(0),
        cache_read_tokens: read,
        cache_write_tokens: write,
        estimated: false,
    })
}

/// 从一个完整响应体 / 一个流式事件里找用量：顶层 `usage`（OpenAI、Anthropic 非流式、`message_delta`），
/// 或 `message.usage`（Anthropic `message_start`）。
pub fn usage_in(v: &serde_json::Value) -> Option<Usage> {
    v.get("usage")
        .and_then(parse_usage)
        .or_else(|| v.pointer("/message/usage").and_then(parse_usage))
}

/// 流式里的一条 data：便宜的子串预检，不含 `usage` 字样就不再解析一遍 JSON。
pub fn usage_in_sse_data(data: &str) -> Option<Usage> {
    if !data.contains("\"usage\"") {
        return None;
    }
    serde_json::from_str::<serde_json::Value>(data).ok().as_ref().and_then(usage_in)
}

/// 服务方没报用量时的粗估（复用上下文压缩的 token 估算：CJK 一字一 token，其余约 3 字符一 token）。
pub fn estimate(input_text_tokens: usize, output: &str) -> Usage {
    Usage {
        input_tokens: input_text_tokens as u64,
        output_tokens: super::compact::estimate_tokens(output) as u64,
        estimated: true,
        ..Default::default()
    }
}

// ——————————————————————————————— 单价 ———————————————————————————————

/// 一条单价：模型名里含 `pattern`（不分大小写）就用它；多条命中取 `pattern` 最长的那条。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ModelPrice {
    pub pattern: String,
    /// 美元 / 百万输入 token
    pub input_per_m: f64,
    /// 美元 / 百万输出 token
    pub output_per_m: f64,
}

impl Default for ModelPrice {
    fn default() -> Self {
        Self { pattern: String::new(), input_per_m: 0.0, output_per_m: 0.0 }
    }
}

fn price(pattern: &str, input_per_m: f64, output_per_m: f64) -> ModelPrice {
    ModelPrice { pattern: pattern.into(), input_per_m, output_per_m }
}

/// 内置默认单价（官方标价、标准档、不含缓存折扣）。只收确定的几家；其余模型留空 =
/// 只显示 token，用户可以在设置里按模型名补上。旧配置没有 `prices` 字段时用它。
pub fn default_prices() -> Vec<ModelPrice> {
    vec![
        price("gpt-4o-mini", 0.15, 0.60),
        price("gpt-4o", 2.50, 10.00),
        price("gpt-4.1-nano", 0.10, 0.40),
        price("gpt-4.1-mini", 0.40, 1.60),
        price("gpt-4.1", 2.00, 8.00),
        price("claude-3-5-sonnet", 3.00, 15.00),
        price("claude-3-7-sonnet", 3.00, 15.00),
        price("claude-sonnet-4", 3.00, 15.00),
    ]
}

/// 给模型名找单价。没命中 / 单价全为 0（空行）返回 `None`。
pub fn price_for<'a>(prices: &'a [ModelPrice], model: &str) -> Option<&'a ModelPrice> {
    let m = model.trim().to_ascii_lowercase();
    if m.is_empty() {
        return None;
    }
    prices
        .iter()
        .filter(|p| {
            let pat = p.pattern.trim().to_ascii_lowercase();
            !pat.is_empty()
                && m.contains(&pat)
                && p.input_per_m.is_finite()
                && p.output_per_m.is_finite()
                && (p.input_per_m > 0.0 || p.output_per_m > 0.0)
        })
        .max_by_key(|p| p.pattern.trim().len())
}

/// 这份用量值多少钱（微美元）。负单价按 0 算。
pub fn cost_micros(p: &ModelPrice, u: &Usage) -> u64 {
    let c = u.input_tokens as f64 * p.input_per_m.max(0.0) + u.output_tokens as f64 * p.output_per_m.max(0.0);
    if c.is_finite() && c > 0.0 {
        c.round().min(u64::MAX as f64) as u64
    } else {
        0
    }
}

/// 微美元 → 美元（给前端）。
pub fn micros_to_usd(m: u64) -> f64 {
    m as f64 / 1_000_000.0
}

/// 一轮对话结束时推给前端的用量（`nx://agent/usage/{session}`，以及流式问答的 `done`）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunUsage {
    #[serde(flatten)]
    pub usage: Usage,
    /// 有单价才有
    pub cost_usd: Option<f64>,
    pub elapsed_secs: u64,
}

impl RunUsage {
    pub fn priced(usage: Usage, prices: &[ModelPrice], model: &str, elapsed_secs: u64) -> Self {
        let cost_usd = price_for(prices, model).map(|p| micros_to_usd(cost_micros(p, &usage)));
        Self { usage, cost_usd, elapsed_secs }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openai_usage_non_streaming() {
        let body = json!({
            "choices": [{"message": {"content": "hi"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 1200, "completion_tokens": 34, "total_tokens": 1234,
                      "prompt_tokens_details": {"cached_tokens": 1024}}
        });
        let u = usage_in(&body).unwrap();
        assert_eq!((u.input_tokens, u.output_tokens, u.cache_read_tokens), (1200, 34, 1024));
        assert!(!u.estimated);
    }

    #[test]
    fn deepseek_cache_hit_field() {
        let u = parse_usage(&json!({"prompt_tokens": 50, "completion_tokens": 5, "prompt_cache_hit_tokens": 40})).unwrap();
        assert_eq!(u.cache_read_tokens, 40);
        assert_eq!(u.input_tokens, 50);
    }

    #[test]
    fn openai_include_usage_final_chunk() {
        // stream_options.include_usage：最后一个 chunk choices 为空、带 usage
        let data = r#"{"id":"c","object":"chat.completion.chunk","choices":[],"usage":{"prompt_tokens":9,"completion_tokens":12,"total_tokens":21}}"#;
        let u = usage_in_sse_data(data).unwrap();
        assert_eq!((u.input_tokens, u.output_tokens), (9, 12));
        // 普通增量 chunk 没有 usage；有些网关给 "usage": null
        assert_eq!(usage_in_sse_data(r#"{"choices":[{"delta":{"content":"a"}}]}"#), None);
        assert_eq!(usage_in_sse_data(r#"{"choices":[{"delta":{"content":"a"}}],"usage":null}"#), None);
    }

    #[test]
    fn anthropic_non_streaming_adds_cache_to_input() {
        let body = json!({
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 100, "output_tokens": 20,
                      "cache_read_input_tokens": 3000, "cache_creation_input_tokens": 500}
        });
        let u = usage_in(&body).unwrap();
        assert_eq!(u.input_tokens, 3600);
        assert_eq!((u.output_tokens, u.cache_read_tokens, u.cache_write_tokens), (20, 3000, 500));
    }

    #[test]
    fn anthropic_stream_start_and_delta_merge() {
        let start = r#"{"type":"message_start","message":{"id":"m","usage":{"input_tokens":472,"output_tokens":1}}}"#;
        let delta = r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":89}}"#;
        let mut u = Usage::default();
        u.merge_max(&usage_in_sse_data(start).unwrap());
        u.merge_max(&usage_in_sse_data(delta).unwrap());
        assert_eq!((u.input_tokens, u.output_tokens), (472, 89));
    }

    #[test]
    fn unknown_or_empty_usage_is_none() {
        assert_eq!(parse_usage(&json!({})), None);
        assert_eq!(parse_usage(&json!(null)), None);
        assert_eq!(parse_usage(&json!({"total_tokens": 5})), None);
    }

    #[test]
    fn add_accumulates_and_keeps_estimated() {
        let mut a = Usage { input_tokens: 10, output_tokens: 2, ..Default::default() };
        a.add(&Usage { input_tokens: 5, output_tokens: 1, estimated: true, ..Default::default() });
        assert_eq!((a.input_tokens, a.output_tokens), (15, 3));
        assert!(a.estimated);
    }

    #[test]
    fn estimate_is_flagged() {
        let u = estimate(100, "hello world!");
        assert!(u.estimated);
        assert_eq!(u.input_tokens, 100);
        assert!(u.output_tokens > 0);
    }

    #[test]
    fn price_lookup_prefers_longest_pattern_and_ignores_empty_rows() {
        let p = default_prices();
        assert_eq!(price_for(&p, "gpt-4o-mini-2024-07-18").unwrap().pattern, "gpt-4o-mini");
        assert_eq!(price_for(&p, "GPT-4o").unwrap().pattern, "gpt-4o");
        assert_eq!(price_for(&p, "gpt-4.1-mini").unwrap().pattern, "gpt-4.1-mini");
        assert_eq!(price_for(&p, "claude-sonnet-4-5").unwrap().pattern, "claude-sonnet-4");
        assert!(price_for(&p, "deepseek-chat").is_none(), "不确定的模型不给默认价");
        let blank = vec![ModelPrice { pattern: "deepseek".into(), ..Default::default() }];
        assert!(price_for(&blank, "deepseek-chat").is_none(), "单价全 0 的行等于没填");
        assert!(price_for(&p, "").is_none());
    }

    #[test]
    fn cost_in_micros() {
        let p = price("x", 2.5, 10.0);
        let u = Usage { input_tokens: 10_000, output_tokens: 2_000, ..Default::default() };
        // 10k × $2.5/M + 2k × $10/M = $0.025 + $0.02 = $0.045
        assert_eq!(cost_micros(&p, &u), 45_000);
        assert!((micros_to_usd(45_000) - 0.045).abs() < 1e-12);
        assert_eq!(cost_micros(&price("x", -1.0, f64::NAN), &u), 0);
    }

    #[test]
    fn prices_deserialize_partial() {
        let p: ModelPrice = serde_json::from_str(r#"{"pattern":"qwen","inputPerM":0.5}"#).unwrap();
        assert_eq!((p.pattern.as_str(), p.input_per_m, p.output_per_m), ("qwen", 0.5, 0.0));
    }

    #[test]
    fn run_usage_serializes_flat() {
        let u = Usage { input_tokens: 1, output_tokens: 2, ..Default::default() };
        let v = serde_json::to_value(RunUsage::priced(u, &default_prices(), "gpt-4o", 3)).unwrap();
        assert_eq!(v["inputTokens"], 1);
        assert_eq!(v["elapsedSecs"], 3);
        assert!(v["costUsd"].as_f64().unwrap() > 0.0);
        let v = serde_json::to_value(RunUsage::priced(u, &[], "gpt-4o", 3)).unwrap();
        assert!(v["costUsd"].is_null());
    }
}

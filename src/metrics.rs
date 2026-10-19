use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
pub struct Metrics {
    observed: AtomicU64,
    input: AtomicU64,
    cached: AtomicU64,
    output: AtomicU64,
}

impl Metrics {
    pub fn record(&self, usage: &Value) -> Option<Value> {
        let input = usage["input_tokens"]
            .as_u64()
            .or_else(|| usage["prompt_tokens"].as_u64())?;
        let output = usage["output_tokens"]
            .as_u64()
            .or_else(|| usage["completion_tokens"].as_u64())
            .unwrap_or(0);
        let cached = usage["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .or_else(|| usage["prompt_tokens_details"]["cached_tokens"].as_u64());
        // Missing cached-token details are unknown, not evidence of zero cache hits.
        let cached = cached.filter(|cached| *cached <= input);
        if let Some(cached) = cached {
            self.observed.fetch_add(1, Ordering::Relaxed);
            self.input.fetch_add(input, Ordering::Relaxed);
            self.cached.fetch_add(cached, Ordering::Relaxed);
            self.output.fetch_add(output, Ordering::Relaxed);
        }
        Some(
            json!({"inputTokens":input,"cachedInputTokens":cached,"outputTokens":output,"inputTokenCacheHitRate":cached.and_then(|cached| (input>0).then_some(cached as f64/input as f64))}),
        )
    }

    pub fn snapshot(&self) -> Value {
        let input = self.input.load(Ordering::Relaxed);
        let cached = self.cached.load(Ordering::Relaxed);
        json!({"responsesWithCacheDetails":self.observed.load(Ordering::Relaxed),"inputTokens":input,"cachedInputTokens":cached,"outputTokens":self.output.load(Ordering::Relaxed),"inputTokenCacheHitRate":(input>0).then_some(cached as f64/input as f64)})
    }
}

//! OTel meter instruments for the OpenAI provider.

use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use simulacra_types::TokenUsage;

/// Lazily-initialized OTel meter instruments for the OpenAI provider.
/// Created on first use so they pick up the global MeterProvider
/// (which may not be set at construction time).
pub(super) struct OpenAiMeters {
    pub(super) duration_histogram: Histogram<f64>,
    pub(super) token_usage_histogram: Histogram<u64>,
    cache_read_histogram: Histogram<u64>,
    cache_write_histogram: Histogram<u64>,
    cache_hit_ratio_histogram: Histogram<f64>,
}

impl OpenAiMeters {
    pub(super) fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<OpenAiMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-provider");
            OpenAiMeters {
                duration_histogram: meter
                    .f64_histogram("gen_ai.client.operation.duration")
                    .with_unit("ms")
                    .with_description("LLM provider call duration")
                    .build(),
                token_usage_histogram: meter
                    .u64_histogram("gen_ai.client.token.usage")
                    .with_unit("{token}")
                    .with_description("Token usage per LLM call")
                    .build(),
                cache_read_histogram: meter
                    .u64_histogram("simulacra.context.cache.read_tokens")
                    .with_unit("{token}")
                    .with_description("Input tokens served from the provider cache")
                    .build(),
                cache_write_histogram: meter
                    .u64_histogram("simulacra.context.cache.write_tokens")
                    .with_unit("{token}")
                    .with_description("Input tokens written to the provider cache")
                    .build(),
                cache_hit_ratio_histogram: meter
                    .f64_histogram("simulacra.context.cache.hit_ratio")
                    .with_unit("1")
                    .with_description("Provider cache-read tokens divided by logical input tokens")
                    .build(),
            }
        })
    }

    pub(super) fn record_cache_usage(&self, usage: &TokenUsage, model: &str) {
        let attrs = &[
            KeyValue::new("gen_ai.provider.name", "openai"),
            KeyValue::new("gen_ai.request.model", model.to_owned()),
        ];
        self.cache_read_histogram
            .record(usage.cache_read_input_tokens, attrs);
        self.cache_write_histogram
            .record(usage.cache_write_input_tokens, attrs);
        let hit_ratio = if usage.input_tokens == 0 {
            0.0
        } else {
            usage.cache_read_input_tokens as f64 / usage.input_tokens as f64
        };
        self.cache_hit_ratio_histogram.record(hit_ratio, attrs);
    }
}

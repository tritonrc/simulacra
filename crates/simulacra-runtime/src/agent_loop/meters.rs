use super::*;

// ── OTel meters ──────────────────────────────────────────────────

/// Lazily-initialized OTel meter instruments for the agent runtime.
/// Created on first use so they pick up the global MeterProvider
/// (which may not be set at construction time).
pub(super) struct RuntimeMeters {
    pub(super) turns_counter: Counter<u64>,
    pub(super) budget_tokens_used: Counter<u64>,
    pub(super) budget_turns_used: Counter<u64>,
    pub(super) budget_exhaustions: Counter<u64>,
}

impl RuntimeMeters {
    pub(super) fn get() -> &'static Self {
        use std::sync::OnceLock;
        static METERS: OnceLock<RuntimeMeters> = OnceLock::new();
        METERS.get_or_init(|| {
            let meter = opentelemetry::global::meter("simulacra-runtime");
            RuntimeMeters {
                turns_counter: meter
                    .u64_counter("simulacra.agent.turns")
                    .with_description("Agent turns consumed")
                    .build(),
                budget_tokens_used: meter
                    .u64_counter("simulacra.agent.budget.tokens_used")
                    .with_description("Agent budget tokens used")
                    .build(),
                budget_turns_used: meter
                    .u64_counter("simulacra.agent.budget.turns_used")
                    .with_description("Agent budget turns used")
                    .build(),
                budget_exhaustions: meter
                    .u64_counter("simulacra.budget.exhaustions")
                    .with_description("Total budget exhaustions")
                    .build(),
            }
        })
    }
}

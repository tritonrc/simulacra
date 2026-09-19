//! Simulacra context crate.
//!
//! Strategies for compacting conversation history to fit within
//! a provider's token limit.

pub use simulacra_types::{ContextStrategy, Message, Role};
use tiktoken_rs::CoreBPE;

mod budget;
mod observation_masking;
mod sliding_window;
#[cfg(test)]
mod tests;

pub use observation_masking::ObservationMaskingStrategy;
pub use sliding_window::SlidingWindowStrategy;

/// Shared `cl100k_base` BPE encoder. `cl100k_base_singleton` initializes it once
/// and hands out a `&'static` reference, so estimation has no per-call setup.
///
/// Why cl100k: Anthropic does not publish Claude's tokenizer, and DevForge's
/// agent loop targets Claude models. cl100k_base is the standard deterministic
/// offline approximation — it runs materially denser than the old 4-chars-per-
/// token stub on prose and sparser on code/JSON, which is the mis-sizing this
/// replaces. It is a heuristic, not Claude's exact count.
pub(crate) fn bpe() -> &'static CoreBPE {
    tiktoken_rs::cl100k_base_singleton()
}

/// Real token count of a text blob under the shared encoder.
pub(crate) fn count_tokens(text: &str) -> u64 {
    bpe().encode_with_special_tokens(text).len() as u64
}

/// Estimated cost of a single message in real BPE tokens, covering `content`,
/// tool-call arguments, and provider-native content blocks. `provider_content`
/// must round-trip unchanged, so `enforce_token_budget` counts it but can only
/// reclaim space from `content`; the `MIN_KEPT_CONTENT_TOKENS` floor keeps that
/// pressure from gutting short turns.
pub(crate) fn message_tokens(message: &Message) -> u64 {
    let mut tokens = count_tokens(&message.content);
    // Tool-call arguments and provider-native blocks (`thinking` etc.) are
    // sent to the provider too; leaving them uncounted is how an "in budget"
    // window overshoots the real limit. Ids/names are short, but counted.
    for call in &message.tool_calls {
        tokens += count_tokens(&call.id);
        tokens += count_tokens(&call.name);
        tokens += count_tokens(&call.arguments.to_string());
    }
    if let Some(id) = &message.tool_call_id {
        tokens += count_tokens(id);
    }
    for block in &message.provider_content {
        tokens += count_tokens(&block.provider);
        tokens += count_tokens(&block.value.to_string());
    }
    tokens
}

/// The share of a message's cost that compaction cannot reclaim: tool-call
/// ids/names/arguments, `tool_call_id`, and provider-native blocks all must
/// reach the provider verbatim. Only `content` is shrinkable.
pub(crate) fn immutable_tokens(message: &Message) -> u64 {
    message_tokens(message).saturating_sub(count_tokens(&message.content))
}

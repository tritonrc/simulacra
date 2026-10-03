//! Shared helpers for turning a provider-native "why did it stop" value into
//! `simulacra_types::FinishReason`. Used by both the Anthropic and OpenAI
//! clients so an unrecognized value is handled identically everywhere.

/// A provider-supplied stop value lands in journals, spans, and logs, so it
/// is bounded to a short, safe charset at the source.
pub(crate) fn sanitize_other_reason(raw: &str) -> String {
    let mut sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    sanitized.truncate(64);
    sanitized
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_other_reason_replaces_unsafe_characters_and_caps_length() {
        assert_eq!(
            sanitize_other_reason("weird\"va\\lue\nhere"),
            "weird_va_lue_here"
        );
        assert_eq!(sanitize_other_reason("a".repeat(100).as_str()).len(), 64);
        assert_eq!(sanitize_other_reason("safe-value_1"), "safe-value_1");
    }
}

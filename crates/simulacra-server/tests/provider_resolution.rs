//! Tests for provider kind inference (S034 assertions: provider resolution).

use simulacra_server::{ProviderKind, infer_provider_kind};

#[test]
fn infer_provider_kind_maps_claude_models_to_anthropic() {
    let kind =
        infer_provider_kind("claude-sonnet-4-6").expect("claude-prefixed models should resolve");
    assert_eq!(kind, ProviderKind::Anthropic);
}

#[test]
fn infer_provider_kind_maps_gpt_models_to_openai() {
    let kind = infer_provider_kind("gpt-4o").expect("gpt-prefixed models should resolve");
    assert_eq!(kind, ProviderKind::OpenAI);
}

#[test]
fn infer_provider_kind_maps_ollama_models_to_ollama() {
    let kind = infer_provider_kind("ollama:llama3").expect("ollama-prefixed models should resolve");
    assert_eq!(kind, ProviderKind::Ollama);
}

#[test]
fn infer_provider_kind_defaults_to_openai_for_unknown_prefixes() {
    let kind = infer_provider_kind("groq-llama-3.3-70b")
        .expect("unknown prefixes should default to OpenAI");
    assert_eq!(kind, ProviderKind::OpenAI);
}

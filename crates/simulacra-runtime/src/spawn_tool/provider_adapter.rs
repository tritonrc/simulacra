use super::*;
use simulacra_provider::{AnthropicProvider, OpenAiProvider};

pub(super) fn build_provider(
    provider_kind: &ProviderKind,
    model: &str,
) -> Result<Box<dyn Provider>, RuntimeError> {
    match provider_kind {
        ProviderKind::Anthropic => {
            let api_key = std::env::var("ANTHROPIC_API_KEY")
                .map_err(|_| RuntimeError::Session("ANTHROPIC_API_KEY not set".into()))?;
            Ok(Box::new(AnthropicProvider::new(api_key, model)))
        }
        ProviderKind::OpenAI => {
            let api_key = std::env::var("OPENAI_API_KEY")
                .map_err(|_| RuntimeError::Session("OPENAI_API_KEY not set".into()))?;
            Ok(Box::new(OpenAiProvider::new(api_key, model)))
        }
        ProviderKind::Ollama => Ok(Box::new(OpenAiProvider::new("ollama", model))),
    }
}

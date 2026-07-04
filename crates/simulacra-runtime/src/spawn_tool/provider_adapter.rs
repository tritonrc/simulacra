use super::*;
use simulacra_provider::{AnthropicProvider, BedrockProvider, OpenAiProvider};

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
        ProviderKind::Bedrock => {
            let region = std::env::var("AWS_REGION")
                .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
                .map_err(|_| {
                    RuntimeError::Session(
                        "AWS_REGION or AWS_DEFAULT_REGION not set for bedrock model".into(),
                    )
                })?;
            Ok(Box::new(BedrockProvider::new(region, model)))
        }
    }
}

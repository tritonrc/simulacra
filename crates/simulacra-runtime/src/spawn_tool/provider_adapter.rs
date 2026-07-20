use super::*;
use simulacra_provider::{AnthropicProvider, BedrockProvider, OpenAiProvider};

pub(super) fn build_provider(
    provider_kind: &ProviderKind,
    model: &str,
) -> Result<Box<dyn Provider>, RuntimeError> {
    build_provider_with_bedrock_factory(provider_kind, model, |model| {
        let region = std::env::var("AWS_REGION")
            .or_else(|_| std::env::var("AWS_DEFAULT_REGION"))
            .map_err(|_| {
                RuntimeError::Session(
                    "AWS_REGION or AWS_DEFAULT_REGION not set for bedrock model".into(),
                )
            })?;
        Ok(Box::new(BedrockProvider::new(region, model)))
    })
}

fn build_provider_with_bedrock_factory<F>(
    provider_kind: &ProviderKind,
    model: &str,
    bedrock_factory: F,
) -> Result<Box<dyn Provider>, RuntimeError>
where
    F: FnOnce(&str) -> Result<Box<dyn Provider>, RuntimeError>,
{
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
        ProviderKind::Bedrock => bedrock_factory(model.strip_prefix("bedrock:").unwrap_or(model)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn model_passed_to_spawned_bedrock_factory(model: &str) -> String {
        let captured = Arc::new(Mutex::new(None));
        let factory_capture = Arc::clone(&captured);
        let result = build_provider_with_bedrock_factory(
            &ProviderKind::Bedrock,
            model,
            move |resolved_model| {
                *factory_capture.lock().expect("capture mutex") = Some(resolved_model.to_owned());
                Err(RuntimeError::Session("captured Bedrock model".into()))
            },
        );
        assert!(
            matches!(result, Err(RuntimeError::Session(message)) if message == "captured Bedrock model"),
            "injected Bedrock factory should control construction"
        );
        captured
            .lock()
            .expect("capture mutex")
            .take()
            .expect("Bedrock arm must invoke the injected factory")
    }

    #[test]
    fn spawned_bedrock_construction_strips_routing_prefix() {
        assert_eq!(
            model_passed_to_spawned_bedrock_factory("bedrock:anthropic.claude-3-5-sonnet-v1:0"),
            "anthropic.claude-3-5-sonnet-v1:0"
        );
    }

    #[test]
    fn spawned_bedrock_construction_strips_only_one_routing_prefix() {
        assert_eq!(
            model_passed_to_spawned_bedrock_factory("bedrock:bedrock:custom.model-v1"),
            "bedrock:custom.model-v1",
            "only the leading routing prefix is host metadata"
        );
    }

    #[test]
    fn spawned_bedrock_construction_preserves_native_model_id() {
        assert_eq!(
            model_passed_to_spawned_bedrock_factory("anthropic.claude-3-5-sonnet-v1:0"),
            "anthropic.claude-3-5-sonnet-v1:0",
            "native Bedrock model ids must pass through unchanged"
        );
    }
}

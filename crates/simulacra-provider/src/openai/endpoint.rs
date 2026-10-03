//! Joining a configured base URL to the chat-completions path.
//!
//! OpenAI-compatible gateways disagree on how much path the base URL already
//! carries: `https://api.openai.com` has none, `https://openrouter.ai/api/v1`
//! and `http://litellm:4000/v1` already end in `/v1`, and Azure-style
//! deployments carry a path plus a `?api-version=...` query string that must
//! survive the join. One rule covers all of them: a base URL with no path
//! (or only `/`) gets the full `/v1/chat/completions` suffix for back-compat
//! with plain `OPENAI_BASE_URL=https://api.openai.com`; any other base URL
//! already supplies its own `/v1`-equivalent prefix, so it only needs
//! `/chat/completions` appended before the query string.

use simulacra_types::ProviderError;
use url::Url;

pub(super) fn build_chat_completions_url(base_url: &str) -> Result<String, ProviderError> {
    let mut url = Url::parse(base_url)
        .map_err(|e| ProviderError::Other(format!("invalid OpenAI base URL '{base_url}': {e}")))?;

    let existing_path = url.path();
    let suffix = if existing_path.is_empty() || existing_path == "/" {
        "/v1/chat/completions"
    } else {
        "/chat/completions"
    };
    let new_path = format!("{}{suffix}", existing_path.trim_end_matches('/'));
    url.set_path(&new_path);

    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_base_url_gets_the_back_compat_v1_prefix() {
        assert_eq!(
            build_chat_completions_url("https://api.openai.com").unwrap(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn bare_base_url_with_trailing_slash_is_not_doubled() {
        assert_eq!(
            build_chat_completions_url("https://api.openai.com/").unwrap(),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn gateway_with_its_own_v1_prefix_only_gets_chat_completions_appended() {
        assert_eq!(
            build_chat_completions_url("https://openrouter.ai/api/v1").unwrap(),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            build_chat_completions_url("http://litellm:4000/v1").unwrap(),
            "http://litellm:4000/v1/chat/completions"
        );
    }

    #[test]
    fn trailing_slash_on_a_non_bare_path_is_not_doubled() {
        assert_eq!(
            build_chat_completions_url("http://litellm:4000/v1/").unwrap(),
            "http://litellm:4000/v1/chat/completions"
        );
    }

    #[test]
    fn query_string_survives_the_join_after_the_new_path() {
        assert_eq!(
            build_chat_completions_url(
                "https://my-resource.openai.azure.com/openai/deployments/my-model?api-version=2024-02-01"
            )
            .unwrap(),
            "https://my-resource.openai.azure.com/openai/deployments/my-model/chat/completions?api-version=2024-02-01"
        );
    }

    #[test]
    fn invalid_base_url_is_a_provider_error_not_a_panic() {
        let err = build_chat_completions_url("not a url").unwrap_err();
        assert!(matches!(err, ProviderError::Other(_)));
    }
}

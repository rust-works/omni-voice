//! Backend-dispatch factory for the [`AiClient`] used by `voice reflect`.
//!
//! `voice reflect` is the only live consumer of this module: it calls
//! [`create_default_claude_client`] to obtain a backend-appropriate
//! [`AiClient`] and drives it directly. The commit-message-improvement
//! wrapper that once lived here was removed with the strip-to-voice-cli
//! refactor.

use anyhow::Result;
use tracing::{debug, info, warn};

use crate::claude::{
    ai::{bedrock::BedrockAiClient, claude::ClaudeAiClient, AiClient},
    error::ClaudeError,
};

fn validate_beta_header(model: &str, beta_header: &Option<(String, String)>) -> Result<()> {
    if let Some((ref key, ref value)) = beta_header {
        let registry = crate::claude::model_config::get_model_registry();
        let supported = registry.get_beta_headers(model);
        if !supported
            .iter()
            .any(|bh| bh.key == *key && bh.value == *value)
        {
            let available: Vec<String> = supported
                .iter()
                .map(|bh| format!("{}:{}", bh.key, bh.value))
                .collect();
            if available.is_empty() {
                anyhow::bail!("Model '{model}' does not support any beta headers");
            }
            anyhow::bail!(
                "Beta header '{key}:{value}' is not supported for model '{model}'. Supported: {}",
                available.join(", ")
            );
        }
    }
    Ok(())
}

/// Creates the default [`AiClient`] using environment variables and settings.
///
/// Async because the Ollama branch probes the local server for its
/// loaded context length so token-budget checks reflect what the server
/// actually loaded the model with (registry values are an estimate that
/// can exceed the live limit). All other branches finish synchronously.
pub async fn create_default_claude_client(
    model: Option<String>,
    beta_header: Option<(String, String)>,
) -> Result<Box<dyn AiClient>> {
    use crate::claude::ai::claude_cli::ClaudeCliAiClient;
    use crate::claude::ai::openai::OpenAiAiClient;
    use crate::utils::settings::{get_env_var, get_env_vars};

    // `claude -p` subprocess backend takes precedence when requested — it
    // reuses an existing Claude Code auth session and is the only backend
    // that accepts short model aliases (sonnet/opus/haiku), so it must
    // short-circuit before `validate_beta_header` runs below.
    let ai_backend = get_env_var("OMNI_VOICE_AI_BACKEND").ok();
    let use_claude_cli = ai_backend
        .as_deref()
        .is_some_and(|v| matches!(v, "claude-cli" | "claude_cli"));

    if use_claude_cli {
        if beta_header.is_some() {
            warn!(
                "--beta-header is ignored when OMNI_VOICE_AI_BACKEND=claude-cli \
                 (the CLI's --betas flag has different semantics and is not forwarded)"
            );
        }
        let registry = crate::claude::model_config::get_model_registry();
        let cli_model = model
            .or_else(|| get_env_var("CLAUDE_MODEL").ok())
            .or_else(|| get_env_var("CLAUDE_CODE_MODEL").ok())
            .or_else(|| get_env_var("ANTHROPIC_MODEL").ok())
            .unwrap_or_else(|| {
                registry
                    .get_default_model("claude")
                    .unwrap_or("claude-sonnet-4-6")
                    .to_string()
            });
        debug!(model = %cli_model, "Creating claude -p subprocess client");
        let ai_client = ClaudeCliAiClient::new(cli_model);
        return Ok(Box::new(ai_client));
    }

    // Check if we should use OpenAI-compatible API (OpenAI or Ollama)
    let use_openai = get_env_var("USE_OPENAI").is_ok_and(|val| val == "true");

    let use_ollama = get_env_var("USE_OLLAMA").is_ok_and(|val| val == "true");

    // Check if we should use Bedrock
    let use_bedrock = get_env_var("CLAUDE_CODE_USE_BEDROCK").is_ok_and(|val| val == "true");

    debug!(
        use_openai = use_openai,
        use_ollama = use_ollama,
        use_bedrock = use_bedrock,
        "Client selection flags"
    );

    let registry = crate::claude::model_config::get_model_registry();

    // Handle Ollama configuration
    if use_ollama {
        let ollama_model = model
            .or_else(|| get_env_var("OLLAMA_MODEL").ok())
            .unwrap_or_else(|| "llama2".to_string());
        validate_beta_header(&ollama_model, &beta_header)?;
        let base_url = get_env_var("OLLAMA_BASE_URL").ok();
        let mut ai_client = OpenAiAiClient::new_ollama(ollama_model, base_url, beta_header)?;
        match ai_client.probe_loaded_context_length().await {
            Some(source) => {
                info!(
                    loaded_context_length = ai_client.loaded_context_length(),
                    source = source.as_str(),
                    model = %ai_client.get_metadata().model,
                    "Probed loaded context length from local server"
                );
            }
            None => {
                debug!(
                    "Loaded context length probe did not return a value; \
                     falling back to registry/default for token budget"
                );
            }
        }
        return Ok(Box::new(ai_client));
    }

    // Handle OpenAI configuration
    if use_openai {
        debug!("Creating OpenAI client");
        let openai_model = model
            .or_else(|| get_env_var("OPENAI_MODEL").ok())
            .unwrap_or_else(|| {
                registry
                    .get_default_model("openai")
                    .unwrap_or("gpt-5")
                    .to_string()
            });
        debug!(openai_model = %openai_model, "Selected OpenAI model");
        validate_beta_header(&openai_model, &beta_header)?;

        let api_key = get_env_vars(&["OPENAI_API_KEY", "OPENAI_AUTH_TOKEN"]).map_err(|e| {
            debug!(error = ?e, "Failed to get OpenAI API key");
            ClaudeError::ApiKeyNotFound
        })?;
        debug!("OpenAI API key found");

        let ai_client = OpenAiAiClient::new_openai(openai_model, api_key, beta_header)?;
        debug!("OpenAI client created successfully");
        return Ok(Box::new(ai_client));
    }

    // For Claude clients, try to get model from env vars or use default
    let claude_model = model
        .or_else(|| get_env_var("ANTHROPIC_MODEL").ok())
        .unwrap_or_else(|| {
            registry
                .get_default_model("claude")
                .unwrap_or("claude-sonnet-4-6")
                .to_string()
        });
    validate_beta_header(&claude_model, &beta_header)?;

    if use_bedrock {
        // Use Bedrock AI client
        let auth_token =
            get_env_var("ANTHROPIC_AUTH_TOKEN").map_err(|_| ClaudeError::ApiKeyNotFound)?;

        let base_url =
            get_env_var("ANTHROPIC_BEDROCK_BASE_URL").map_err(|_| ClaudeError::ApiKeyNotFound)?;

        let ai_client = BedrockAiClient::new(claude_model, auth_token, base_url, beta_header)?;
        return Ok(Box::new(ai_client));
    }

    // Default: use standard Claude AI client
    debug!("Falling back to Claude client");
    let api_key = get_env_vars(&[
        "CLAUDE_API_KEY",
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
    ])
    .map_err(|_| ClaudeError::ApiKeyNotFound)?;

    let ai_client = ClaudeAiClient::new(claude_model, api_key, beta_header)?;
    debug!("Claude client created successfully");
    Ok(Box::new(ai_client))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    // ── validate_beta_header ───────────────────────────────────────

    #[test]
    fn validate_beta_header_none_passes() {
        let result = validate_beta_header("claude-opus-4-1-20250805", &None);
        assert!(result.is_ok());
    }

    #[test]
    fn validate_beta_header_unsupported_fails() {
        let header = Some(("fake-key".to_string(), "fake-value".to_string()));
        let result = validate_beta_header("claude-opus-4-1-20250805", &header);
        assert!(result.is_err());
    }

    // ── create_default_claude_client factory ───────────────────────

    /// Serialises env-mutating factory tests in this module.
    static FACTORY_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct FactoryEnvGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl FactoryEnvGuard {
        fn new(keys: &[&'static str]) -> Self {
            let lock = FACTORY_ENV_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let saved = keys.iter().map(|k| (*k, std::env::var(k).ok())).collect();
            for k in keys {
                std::env::remove_var(k);
            }
            Self { _lock: lock, saved }
        }

        fn set(&self, key: &str, value: &str) {
            std::env::set_var(key, value);
        }
    }

    impl Drop for FactoryEnvGuard {
        fn drop(&mut self) {
            for (k, v) in self.saved.drain(..) {
                match v {
                    Some(val) => std::env::set_var(k, val),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    #[tokio::test]
    async fn factory_claude_cli_backend_dispatches_to_claude_cli_client() {
        let guard = FactoryEnvGuard::new(&[
            "OMNI_VOICE_AI_BACKEND",
            "USE_OPENAI",
            "USE_OLLAMA",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_MODEL",
            "CLAUDE_CODE_MODEL",
            "ANTHROPIC_MODEL",
        ]);
        guard.set("OMNI_VOICE_AI_BACKEND", "claude-cli");

        let client = create_default_claude_client(None, None)
            .await
            .expect("factory should succeed");
        let metadata = client.get_metadata();
        assert_eq!(metadata.provider, "Claude CLI");
        // Default model falls through to the registry's claude default.
        assert_eq!(metadata.model, "claude-sonnet-4-6");
    }

    #[tokio::test]
    async fn factory_claude_cli_backend_honours_model_precedence() {
        let guard = FactoryEnvGuard::new(&[
            "OMNI_VOICE_AI_BACKEND",
            "USE_OPENAI",
            "USE_OLLAMA",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_MODEL",
            "CLAUDE_CODE_MODEL",
            "ANTHROPIC_MODEL",
        ]);
        guard.set("OMNI_VOICE_AI_BACKEND", "claude-cli");
        guard.set("CLAUDE_CODE_MODEL", "opus");
        // CLAUDE_MODEL has higher precedence than CLAUDE_CODE_MODEL.
        guard.set("CLAUDE_MODEL", "haiku");

        let client = create_default_claude_client(None, None)
            .await
            .expect("factory should succeed");
        let metadata = client.get_metadata();
        assert_eq!(metadata.provider, "Claude CLI");
        assert_eq!(metadata.model, "haiku");
    }

    #[tokio::test]
    async fn factory_claude_cli_backend_explicit_model_wins_over_env() {
        let guard = FactoryEnvGuard::new(&[
            "OMNI_VOICE_AI_BACKEND",
            "USE_OPENAI",
            "USE_OLLAMA",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_MODEL",
            "CLAUDE_CODE_MODEL",
            "ANTHROPIC_MODEL",
        ]);
        guard.set("OMNI_VOICE_AI_BACKEND", "claude-cli");
        guard.set("CLAUDE_MODEL", "haiku");

        let client = create_default_claude_client(Some("opus".to_string()), None)
            .await
            .expect("factory should succeed");
        let metadata = client.get_metadata();
        assert_eq!(metadata.model, "opus");
    }

    #[tokio::test]
    async fn factory_claude_cli_backend_accepts_underscore_alias() {
        let guard = FactoryEnvGuard::new(&[
            "OMNI_VOICE_AI_BACKEND",
            "USE_OPENAI",
            "USE_OLLAMA",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_MODEL",
            "CLAUDE_CODE_MODEL",
            "ANTHROPIC_MODEL",
        ]);
        guard.set("OMNI_VOICE_AI_BACKEND", "claude_cli");

        let client = create_default_claude_client(None, None)
            .await
            .expect("factory should succeed");
        let metadata = client.get_metadata();
        assert_eq!(metadata.provider, "Claude CLI");
    }

    #[tokio::test]
    async fn factory_ollama_branch_probes_loaded_context_length() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [
                    { "id": "lm-loaded", "state": "loaded", "loaded_context_length": 6144_u64 }
                ]
            })))
            .mount(&server)
            .await;

        let guard = FactoryEnvGuard::new(&[
            "OMNI_VOICE_AI_BACKEND",
            "USE_OPENAI",
            "USE_OLLAMA",
            "CLAUDE_CODE_USE_BEDROCK",
            "OLLAMA_BASE_URL",
            "OLLAMA_MODEL",
        ]);
        guard.set("USE_OLLAMA", "true");
        guard.set("OLLAMA_BASE_URL", &server.uri());
        guard.set("OLLAMA_MODEL", "lm-loaded");

        let client = create_default_claude_client(None, None)
            .await
            .expect("factory should succeed");
        let metadata = client.get_metadata();
        assert_eq!(metadata.provider, "Ollama");
        assert_eq!(metadata.model, "lm-loaded");
        // The probed value (6144) overrides the registry/default.
        assert_eq!(metadata.max_context_length, 6144);
    }

    #[tokio::test]
    async fn factory_ollama_branch_falls_back_when_probe_fails() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let guard = FactoryEnvGuard::new(&[
            "OMNI_VOICE_AI_BACKEND",
            "USE_OPENAI",
            "USE_OLLAMA",
            "CLAUDE_CODE_USE_BEDROCK",
            "OLLAMA_BASE_URL",
            "OLLAMA_MODEL",
        ]);
        guard.set("USE_OLLAMA", "true");
        guard.set("OLLAMA_BASE_URL", &server.uri());
        guard.set("OLLAMA_MODEL", "no-such-model");

        let client = create_default_claude_client(None, None)
            .await
            .expect("factory should succeed");
        let metadata = client.get_metadata();
        // Probe failure → fall back to the registry estimate (which
        // resolves to FALLBACK_INPUT_CONTEXT for unknown models).
        let registry_value =
            crate::claude::model_config::get_model_registry().get_input_context("no-such-model");
        assert_eq!(metadata.max_context_length, registry_value);
    }

    /// LM Studio path is tested above. This complements it by exercising
    /// the Ollama-native fallthrough through the factory, so the
    /// info-log arm fires for both `ProbeSource` variants.
    #[tokio::test]
    async fn factory_ollama_branch_probes_via_ollama_native() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v0/models"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/show"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model_info": { "llama.context_length": 12288_u64 }
            })))
            .mount(&server)
            .await;

        let guard = FactoryEnvGuard::new(&[
            "OMNI_VOICE_AI_BACKEND",
            "USE_OPENAI",
            "USE_OLLAMA",
            "CLAUDE_CODE_USE_BEDROCK",
            "OLLAMA_BASE_URL",
            "OLLAMA_MODEL",
        ]);
        guard.set("USE_OLLAMA", "true");
        guard.set("OLLAMA_BASE_URL", &server.uri());
        guard.set("OLLAMA_MODEL", "ollama-native-model");

        let client = create_default_claude_client(None, None)
            .await
            .expect("factory should succeed");
        let metadata = client.get_metadata();
        assert_eq!(metadata.max_context_length, 12288);
    }
}

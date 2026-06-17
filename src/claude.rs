//! AI client infrastructure reused by `reflect`.
//!
//! Only the backend-dispatch factory and the [`AiClient`] trait + its
//! provider implementations remain here; the commit-analysis machinery
//! that once drove this module was removed with the strip-to-voice-cli
//! refactor.

pub mod ai;
pub mod client;
pub mod error;
pub mod model_config;
#[cfg(test)]
pub(crate) mod test_utils;

pub use ai::bedrock::BedrockAiClient;
pub use ai::claude::ClaudeAiClient;
pub use ai::{
    AiClient, AiClientCapabilities, AiClientMetadata, PromptStyle, RequestOptions, ResponseFormat,
};
pub use client::create_default_claude_client;
pub use error::ClaudeError;

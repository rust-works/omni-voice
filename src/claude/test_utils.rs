//! Shared test utilities for the `claude` module.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use anyhow::Result;

use crate::claude::ai::{AiClient, AiClientMetadata};

/// Mock AI client with a pre-programmed queue of responses.
///
/// Responses are returned in FIFO order. When the queue is exhausted,
/// subsequent calls return `Err("no more mock responses")`.
///
/// Every call to [`send_request`](AiClient::send_request) records the
/// `(system_prompt, user_prompt)` pair so tests can inspect which prompts
/// were dispatched. Use [`prompt_handle`](Self::prompt_handle) to obtain
/// a shared handle for reading the recorded prompts after the client has
/// been boxed as a `Box<dyn AiClient>` and handed to the code under test.
///
/// # Example
///
/// ```rust
/// let ai: Box<dyn AiClient> = Box::new(ConfigurableMockAiClient::new(vec![
///     Err(anyhow::anyhow!("rate limit")),  // first attempt fails
///     Ok("title: ...".to_string()),        // retry succeeds
/// ]));
/// ```
pub(crate) struct ConfigurableMockAiClient {
    responses: Arc<Mutex<VecDeque<Result<String>>>>,
    metadata: AiClientMetadata,
    recorded_prompts: Arc<Mutex<Vec<(String, String)>>>,
}

impl ConfigurableMockAiClient {
    /// Creates a new mock client that will return the given responses in order.
    pub(crate) fn new(responses: Vec<Result<String>>) -> Self {
        Self {
            responses: Arc::new(Mutex::new(VecDeque::from(responses))),
            metadata: AiClientMetadata {
                provider: "Mock".to_string(),
                model: "mock-model".to_string(),
                max_context_length: 200_000,
                max_response_length: 8_192,
                active_beta: None,
            },
            recorded_prompts: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Returns a handle for inspecting which prompts were sent to the
    /// mock client after it has been boxed as a `Box<dyn AiClient>`.
    pub(crate) fn prompt_handle(&self) -> PromptRecordHandle {
        PromptRecordHandle {
            recorded_prompts: self.recorded_prompts.clone(),
        }
    }
}

/// Shared handle to a mock client's recorded prompts.
///
/// Holds an `Arc` reference to the same prompt log used by the mock
/// client, allowing tests to inspect which prompts were sent after the
/// client has been boxed as a `Box<dyn AiClient>`.
pub(crate) struct PromptRecordHandle {
    recorded_prompts: Arc<Mutex<Vec<(String, String)>>>,
}

impl PromptRecordHandle {
    /// Returns all recorded `(system_prompt, user_prompt)` pairs.
    pub(crate) fn prompts(&self) -> Vec<(String, String)> {
        self.recorded_prompts.lock().unwrap().clone()
    }
}

impl AiClient for ConfigurableMockAiClient {
    fn send_request<'a>(
        &'a self,
        system_prompt: &'a str,
        user_prompt: &'a str,
    ) -> Pin<Box<dyn Future<Output = Result<String>> + Send + 'a>> {
        let responses = self.responses.clone();
        let recorded = self.recorded_prompts.clone();
        let sys = system_prompt.to_string();
        let usr = user_prompt.to_string();
        Box::pin(async move {
            recorded.lock().unwrap().push((sys, usr));
            responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Err(anyhow::anyhow!("no more mock responses")))
        })
    }

    fn get_metadata(&self) -> AiClientMetadata {
        self.metadata.clone()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::claude::ai::RequestOptions;

    /// MockAiClient must default to ''no schema support'' so existing
    /// tests don't have to care about the new schema plumbing.
    #[test]
    fn mock_client_defaults_to_no_schema_support() {
        let client = ConfigurableMockAiClient::new(vec![]);
        let caps = client.capabilities();
        assert!(
            !caps.supports_response_schema,
            "mock client should default to no schema support so tests don't have to care"
        );
    }

    /// `send_request_with_options` falls through to `send_request` by
    /// default — verify the mock observes the call.
    #[tokio::test]
    async fn mock_client_send_with_options_falls_through_to_send_request() {
        let client = ConfigurableMockAiClient::new(vec![Ok("hello".to_string())]);
        let prompt_handle = client.prompt_handle();

        let result = client
            .send_request_with_options("sys", "user", RequestOptions::default())
            .await
            .expect("default send_request_with_options should succeed");
        assert_eq!(result, "hello");

        // The default impl forwards to send_request, so the prompt is recorded
        // exactly once.
        let prompts = prompt_handle.prompts();
        assert_eq!(prompts.len(), 1);
        assert_eq!(prompts[0], ("sys".to_string(), "user".to_string()));
    }
}

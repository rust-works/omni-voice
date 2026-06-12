//! Pull-request content types produced by the AI client.

/// AI-generated PR content with structured fields.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct PrContent {
    /// Concise PR title (ideally 50-80 characters).
    pub title: String,
    /// Full PR description in markdown format.
    pub description: String,
}

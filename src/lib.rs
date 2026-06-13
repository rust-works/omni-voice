//! # omni-voice
//!
//! A comprehensive development toolkit written in Rust.
//!
//! ## Features
//!
//! - Fast and efficient development tools
//! - Extensible architecture
//! - Memory safe and reliable
//!
//! ## Quick Start
//!
//! ```rust
//! use omni_voice::*;
//!
//! println!("Hello from omni-voice!");
//! ```

#![warn(missing_docs)]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod claude;
pub mod cli;
pub mod data;
pub mod git;
pub mod utils;
pub mod voice;

#[cfg(test)]
mod test_support;

pub use crate::cli::Cli;

/// The current version of omni-voice.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

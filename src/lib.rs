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

// omni-voice supports only Apple Silicon macOS (see ADR-0041). Fail fast at
// compile time on any other target rather than producing an unsupported binary.
// The binary crate (`src/main.rs`) depends on this library, so guarding here
// covers both.
#[cfg(not(all(target_os = "macos", target_arch = "aarch64")))]
compile_error!("omni-voice supports only Apple Silicon macOS (aarch64-apple-darwin).");

pub mod claude;
pub mod cli;
pub mod utils;
pub mod voice;

#[cfg(test)]
mod test_support;

pub use crate::cli::Cli;

/// The current version of omni-voice.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

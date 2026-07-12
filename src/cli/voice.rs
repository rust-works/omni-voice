//! Voice command implementations.
//!
//! Each voice-related command (`capture`, `transcribe`, `reflect`, `review`,
//! `install-model`, `enroll`, `listen`) is a top-level subcommand defined in
//! its own submodule to keep help text and parse logic local to each
//! command. The command structs are wired directly into the top-level
//! [`crate::cli::Commands`] enum.

pub mod capture;
pub mod enroll;
pub mod install_model;
pub mod listen;
pub mod reflect;
pub mod review;
pub mod sessions;
pub mod transcribe;

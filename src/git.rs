//! Git operations and repository management.

// The claude commit-analysis pipeline that consumed these types was removed
// when `src/claude` was gutted to the AiClient/factory subset, leaving a few
// crate-internal helpers without callers. This whole module is deleted in the
// follow-up commit (#36 step 2); allow dead_code for the one-commit interim so
// `clippy -D warnings` stays green.
#![allow(dead_code)]

pub mod commit;
pub mod diff_split;
pub mod pr;
pub mod remote;

pub use commit::{
    refine_message_scope, resolve_scope, CommitAnalysis, CommitAnalysisForAI, CommitInfo,
    CommitInfoForAI, FileDiffRef,
};
pub use diff_split::{split_by_file, split_file_by_hunk, FileDiff, HunkDiff};
pub use pr::PrContent;
pub use remote::RemoteInfo;

/// Number of hex characters to show in abbreviated commit hashes.
pub const SHORT_HASH_LEN: usize = 8;

/// Length of a full SHA-1 commit hash in hex characters.
pub const FULL_HASH_LEN: usize = 40;

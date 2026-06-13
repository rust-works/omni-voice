//! Git commit operations and analysis.

use std::fs;
use std::sync::LazyLock;

use anyhow::{Context, Result};
use chrono::{DateTime, FixedOffset};
use globset::Glob;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::data::context::ScopeDefinition;

/// Matches conventional commit scope patterns including breaking-change syntax.
#[allow(clippy::unwrap_used)] // Compile-time constant regex pattern
static SCOPE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[a-z]+!\(([^)]+)\):|^[a-z]+\(([^)]+)\):").unwrap());

/// Commit information structure, generic over analysis type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfo<A = CommitAnalysis> {
    /// Full SHA-1 hash of the commit.
    pub hash: String,
    /// Commit author name and email address.
    pub author: String,
    /// Commit date in ISO format with timezone.
    pub date: DateTime<FixedOffset>,
    /// The original commit message as written by the author.
    pub original_message: String,
    /// Array of remote main branches that contain this commit.
    pub in_main_branches: Vec<String>,
    /// Automated analysis of the commit including type detection and proposed message.
    pub analysis: A,
}

/// Commit analysis information.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitAnalysis {
    /// Automatically detected conventional commit type (feat, fix, docs, test, chore, etc.).
    pub detected_type: String,
    /// Automatically detected scope based on file paths (cli, git, data, etc.).
    pub detected_scope: String,
    /// AI-generated conventional commit message based on file changes.
    pub proposed_message: String,
    /// Detailed statistics about file changes in this commit.
    pub file_changes: FileChanges,
    /// Git diff --stat output showing lines changed per file.
    pub diff_summary: String,
    /// Path to diff file showing line-by-line changes.
    pub diff_file: String,
    /// Per-file diff references for individual file changes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub file_diffs: Vec<FileDiffRef>,
}

/// Reference to a per-file diff stored on disk.
///
/// Tracks the repository-relative file path, the absolute path to the
/// diff file on disk, and the byte length of that diff. Gives consumers
/// per-file size information without loading diff content into memory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDiffRef {
    /// Repository-relative path of the changed file.
    pub path: String,
    /// Absolute path to the per-file diff file on disk.
    pub diff_file: String,
    /// Byte length of the per-file diff content.
    pub byte_len: usize,
}

/// Enhanced commit analysis for AI processing with full diff content.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitAnalysisForAI {
    /// Base commit analysis fields.
    #[serde(flatten)]
    pub base: CommitAnalysis,
    /// Full diff content for AI analysis.
    pub diff_content: String,
}

/// Commit information with enhanced analysis for AI processing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommitInfoForAI {
    /// Base commit information with AI-enhanced analysis.
    #[serde(flatten)]
    pub base: CommitInfo<CommitAnalysisForAI>,
    /// Deterministic checks already performed; the LLM should treat these as authoritative.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pre_validated_checks: Vec<String>,
}

/// File changes statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChanges {
    /// Total number of files modified in this commit.
    pub total_files: usize,
    /// Number of new files added in this commit.
    pub files_added: usize,
    /// Number of files deleted in this commit.
    pub files_deleted: usize,
    /// Array of files changed with their git status (M=modified, A=added, D=deleted).
    pub file_list: Vec<FileChange>,
}

/// Individual file change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileChange {
    /// Git status code (A=added, M=modified, D=deleted, R=renamed).
    pub status: String,
    /// Path to the file relative to repository root.
    pub file: String,
}

impl CommitAnalysis {
    /// Re-detects scope using file_patterns from scope definitions.
    ///
    /// More specific patterns (more literal path components) win regardless of
    /// definition order in scopes.yaml. Equally specific matches are joined
    /// with ", ". If no scope definitions match, the existing detected_scope
    /// is kept as a fallback.
    pub fn refine_scope(&mut self, scope_defs: &[ScopeDefinition]) {
        let files: Vec<&str> = self
            .file_changes
            .file_list
            .iter()
            .map(|f| f.file.as_str())
            .collect();

        if let Some(resolved) = resolve_scope(&files, scope_defs) {
            self.detected_scope = resolved;
        }
    }
}

impl CommitInfoForAI {
    /// Converts from a basic `CommitInfo` by loading diff content.
    pub fn from_commit_info(commit_info: CommitInfo) -> Result<Self> {
        let analysis = CommitAnalysisForAI::from_commit_analysis(commit_info.analysis)?;

        Ok(Self {
            base: CommitInfo {
                hash: commit_info.hash,
                author: commit_info.author,
                date: commit_info.date,
                original_message: commit_info.original_message,
                in_main_branches: commit_info.in_main_branches,
                analysis,
            },
            pre_validated_checks: Vec::new(),
        })
    }

    /// Creates a partial view of a commit containing only the specified file diffs.
    ///
    /// Convenience wrapper around [`Self::from_commit_info_partial_with_overrides`]
    /// with all-`None` overrides (every file loaded from disk).
    #[cfg(test)]
    pub(crate) fn from_commit_info_partial(
        commit_info: CommitInfo,
        file_paths: &[String],
    ) -> Result<Self> {
        let overrides: Vec<Option<String>> = vec![None; file_paths.len()];
        Self::from_commit_info_partial_with_overrides(commit_info, file_paths, &overrides)
    }

    /// Creates a partial view using pre-sliced diff content where available.
    ///
    /// `file_paths` and `diff_overrides` must be parallel slices. When
    /// `diff_overrides[i]` is `Some(content)`, that content is used directly
    /// instead of reading the full per-file diff from disk. This enables
    /// per-hunk partial views where each chunk receives only its assigned
    /// hunk slices rather than the entire file.
    ///
    /// Entries with `None` overrides fall back to loading from disk via
    /// [`FileDiffRef::diff_file`], deduplicated by path.
    pub(crate) fn from_commit_info_partial_with_overrides(
        commit_info: CommitInfo,
        file_paths: &[String],
        diff_overrides: &[Option<String>],
    ) -> Result<Self> {
        let mut diff_parts = Vec::new();
        let mut included_refs = Vec::new();
        let mut loaded_disk_paths: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        for (path, override_content) in file_paths.iter().zip(diff_overrides.iter()) {
            if let Some(content) = override_content {
                // Pre-sliced hunk content — use directly.
                diff_parts.push(content.clone());
                // Include the FileDiffRef for metadata (deduplicated).
                if let Some(file_ref) = commit_info
                    .analysis
                    .file_diffs
                    .iter()
                    .find(|r| r.path == *path)
                {
                    if !included_refs.iter().any(|r: &FileDiffRef| r.path == *path) {
                        included_refs.push(file_ref.clone());
                    }
                }
            } else {
                // Whole-file item — load from disk (deduplicated).
                if loaded_disk_paths.insert(path.clone()) {
                    if let Some(file_ref) = commit_info
                        .analysis
                        .file_diffs
                        .iter()
                        .find(|r| r.path == *path)
                    {
                        let content =
                            fs::read_to_string(&file_ref.diff_file).with_context(|| {
                                format!("Failed to read per-file diff: {}", file_ref.diff_file)
                            })?;
                        diff_parts.push(content);
                        included_refs.push(file_ref.clone());
                    }
                }
            }
        }

        let diff_content = diff_parts.join("\n");

        let partial_analysis = CommitAnalysisForAI {
            base: CommitAnalysis {
                file_diffs: included_refs,
                ..commit_info.analysis
            },
            diff_content,
        };

        Ok(Self {
            base: CommitInfo {
                hash: commit_info.hash,
                author: commit_info.author,
                date: commit_info.date,
                original_message: commit_info.original_message,
                in_main_branches: commit_info.in_main_branches,
                analysis: partial_analysis,
            },
            pre_validated_checks: Vec::new(),
        })
    }

    /// Runs deterministic pre-validation checks on the commit message.
    /// Passing checks are recorded in pre_validated_checks so the LLM
    /// can skip re-checking them. Failing checks are not recorded.
    pub fn run_pre_validation_checks(&mut self, valid_scopes: &[ScopeDefinition]) {
        if let Some(caps) = SCOPE_RE.captures(&self.base.original_message) {
            let scope = caps.get(1).or_else(|| caps.get(2)).map(|m| m.as_str());
            if let Some(scope) = scope {
                if scope.contains(',') && !scope.contains(",  ") && !scope.contains(" ,") {
                    self.pre_validated_checks.push(format!(
                        "Scope format verified: multi-scope '{scope}' uses commas with at most one trailing space"
                    ));
                }

                // Deterministic scope validity check
                if !valid_scopes.is_empty() {
                    let scope_parts: Vec<&str> = scope.split(',').map(str::trim).collect();
                    let all_valid = scope_parts
                        .iter()
                        .all(|part| valid_scopes.iter().any(|s| s.name == *part));
                    if all_valid {
                        self.pre_validated_checks.push(format!(
                            "Scope validity verified: '{scope}' is in the valid scopes list"
                        ));
                    }
                }
            }
        }
    }
}

/// Resolves the best scope for a set of files using scope definition file patterns.
///
/// More specific patterns (more literal path components) win regardless of
/// definition order in `scopes.yaml`. Equally specific matches are joined
/// with ", ". Returns `None` when `scope_defs` or `files` is empty, or no
/// scope definition matches.
pub fn resolve_scope(files: &[&str], scope_defs: &[ScopeDefinition]) -> Option<String> {
    if scope_defs.is_empty() || files.is_empty() {
        return None;
    }

    let mut matches: Vec<(&str, usize)> = Vec::new();
    for scope_def in scope_defs {
        if let Some(specificity) = scope_matches_files(files, &scope_def.file_patterns) {
            matches.push((&scope_def.name, specificity));
        }
    }

    if matches.is_empty() {
        return None;
    }

    // SAFETY: matches is non-empty (guarded by early return above)
    #[allow(clippy::expect_used)] // Guarded by is_empty() check above
    let max_specificity = matches.iter().map(|(_, s)| *s).max().expect("non-empty");
    let best: Vec<&str> = matches
        .into_iter()
        .filter(|(_, s)| *s == max_specificity)
        .map(|(name, _)| name)
        .collect();

    Some(best.join(", "))
}

/// Replaces the scope in a conventional commit message with the deterministically
/// resolved scope based on the given files and scope definitions.
///
/// If the message does not contain a conventional commit scope, or if no scope
/// can be resolved from the files, the message is returned unchanged.
pub fn refine_message_scope(
    message: &str,
    files: &[&str],
    scope_defs: &[ScopeDefinition],
) -> String {
    let Some(resolved) = resolve_scope(files, scope_defs) else {
        return message.to_string();
    };

    // Split into first line and rest
    let (first_line, rest) = message
        .split_once('\n')
        .map_or((message, ""), |(f, r)| (f, r));

    let Some(caps) = SCOPE_RE.captures(first_line) else {
        return message.to_string();
    };

    // Determine which capture group matched (group 1 = breaking, group 2 = normal)
    let existing_scope = caps
        .get(1)
        .or_else(|| caps.get(2))
        .map_or("", |m| m.as_str());

    if existing_scope == resolved {
        return message.to_string();
    }

    let new_first_line =
        first_line.replacen(&format!("({existing_scope})"), &format!("({resolved})"), 1);

    if rest.is_empty() {
        new_first_line
    } else {
        format!("{new_first_line}\n{rest}")
    }
}

/// Checks if a scope's file patterns match any of the given files.
///
/// Returns `Some(max_specificity)` if at least one file matches the scope
/// (after applying negation patterns), or `None` if no file matches.
fn scope_matches_files(files: &[&str], patterns: &[String]) -> Option<usize> {
    let mut positive = Vec::new();
    let mut negative = Vec::new();
    for pat in patterns {
        if let Some(stripped) = pat.strip_prefix('!') {
            negative.push(stripped);
        } else {
            positive.push(pat.as_str());
        }
    }

    // Build negative matchers
    let neg_matchers: Vec<_> = negative
        .iter()
        .filter_map(|p| Glob::new(p).ok().map(|g| g.compile_matcher()))
        .collect();

    let mut max_specificity: Option<usize> = None;
    for pat in &positive {
        let Ok(glob) = Glob::new(pat) else {
            continue;
        };
        let matcher = glob.compile_matcher();
        for file in files {
            if matcher.is_match(file) && !neg_matchers.iter().any(|neg| neg.is_match(file)) {
                let specificity = count_specificity(pat);
                max_specificity =
                    Some(max_specificity.map_or(specificity, |cur| cur.max(specificity)));
            }
        }
    }
    max_specificity
}

/// Counts the number of literal (non-wildcard) path segments in a glob pattern.
///
/// - `docs/adrs/**` → 2 (`docs`, `adrs`)
/// - `docs/**` → 1 (`docs`)
/// - `*.md` → 0
/// - `src/main/scala/**` → 3
fn count_specificity(pattern: &str) -> usize {
    pattern
        .split('/')
        .filter(|segment| !segment.contains('*') && !segment.contains('?'))
        .count()
}

impl CommitAnalysisForAI {
    /// Converts from a basic `CommitAnalysis` by loading diff content from file.
    pub fn from_commit_analysis(analysis: CommitAnalysis) -> Result<Self> {
        // Read the actual diff content from the file
        let diff_content = fs::read_to_string(&analysis.diff_file)
            .with_context(|| format!("Failed to read diff file: {}", analysis.diff_file))?;

        Ok(Self {
            base: analysis,
            diff_content,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::data::context::ScopeDefinition;

    // ── test helpers ─────────────────────────────────────────────────

    fn make_file_changes(files: &[(&str, &str)]) -> FileChanges {
        FileChanges {
            total_files: files.len(),
            files_added: files.iter().filter(|(s, _)| *s == "A").count(),
            files_deleted: files.iter().filter(|(s, _)| *s == "D").count(),
            file_list: files
                .iter()
                .map(|(status, file)| FileChange {
                    status: (*status).to_string(),
                    file: (*file).to_string(),
                })
                .collect(),
        }
    }

    // ── count_specificity ────────────────────────────────────────────

    #[test]
    fn count_specificity_deep_path() {
        assert_eq!(super::count_specificity("src/main/scala/**"), 3);
    }

    #[test]
    fn count_specificity_shallow() {
        assert_eq!(super::count_specificity("docs/**"), 1);
    }

    #[test]
    fn count_specificity_wildcard_only() {
        assert_eq!(super::count_specificity("*.md"), 0);
    }

    #[test]
    fn count_specificity_no_wildcards() {
        assert_eq!(super::count_specificity("src/lib.rs"), 2);
    }

    // ── scope_matches_files ──────────────────────────────────────────

    #[test]
    fn scope_matches_positive_patterns() {
        let patterns = vec!["src/cli/**".to_string()];
        let files = &["src/cli/commands.rs"];
        assert!(super::scope_matches_files(files, &patterns).is_some());
    }

    #[test]
    fn scope_matches_no_match() {
        let patterns = vec!["src/cli/**".to_string()];
        let files = &["src/git/remote.rs"];
        assert!(super::scope_matches_files(files, &patterns).is_none());
    }

    #[test]
    fn scope_matches_with_negation() {
        let patterns = vec!["src/**".to_string(), "!src/test/**".to_string()];
        // File in src/ but not in src/test/ should match
        let files = &["src/lib.rs"];
        assert!(super::scope_matches_files(files, &patterns).is_some());

        // File in src/test/ should be excluded
        let test_files = &["src/test/helper.rs"];
        assert!(super::scope_matches_files(test_files, &patterns).is_none());
    }

    // ── refine_scope ─────────────────────────────────────────────────

    fn make_scope_def(name: &str, patterns: &[&str]) -> ScopeDefinition {
        ScopeDefinition {
            name: name.to_string(),
            description: String::new(),
            examples: vec![],
            file_patterns: patterns.iter().map(|p| (*p).to_string()).collect(),
        }
    }

    #[test]
    fn refine_scope_empty_defs() {
        let mut analysis = CommitAnalysis {
            detected_type: "feat".to_string(),
            detected_scope: "original".to_string(),
            proposed_message: String::new(),
            file_changes: make_file_changes(&[("M", "src/cli/commands.rs")]),
            diff_summary: String::new(),
            diff_file: String::new(),
            file_diffs: Vec::new(),
        };
        analysis.refine_scope(&[]);
        assert_eq!(analysis.detected_scope, "original");
    }

    #[test]
    fn refine_scope_most_specific_wins() {
        let scope_defs = vec![
            make_scope_def("lib", &["src/**"]),
            make_scope_def("cli", &["src/cli/**"]),
        ];
        let mut analysis = CommitAnalysis {
            detected_type: "feat".to_string(),
            detected_scope: String::new(),
            proposed_message: String::new(),
            file_changes: make_file_changes(&[("M", "src/cli/commands.rs")]),
            diff_summary: String::new(),
            diff_file: String::new(),
            file_diffs: Vec::new(),
        };
        analysis.refine_scope(&scope_defs);
        assert_eq!(analysis.detected_scope, "cli");
    }

    #[test]
    fn refine_scope_no_matching_files() {
        let scope_defs = vec![make_scope_def("cli", &["src/cli/**"])];
        let mut analysis = CommitAnalysis {
            detected_type: "feat".to_string(),
            detected_scope: "original".to_string(),
            proposed_message: String::new(),
            file_changes: make_file_changes(&[("M", "README.md")]),
            diff_summary: String::new(),
            diff_file: String::new(),
            file_diffs: Vec::new(),
        };
        analysis.refine_scope(&scope_defs);
        // No match → keeps original
        assert_eq!(analysis.detected_scope, "original");
    }

    #[test]
    fn refine_scope_equal_specificity_joins() {
        let scope_defs = vec![
            make_scope_def("cli", &["src/cli/**"]),
            make_scope_def("git", &["src/git/**"]),
        ];
        let mut analysis = CommitAnalysis {
            detected_type: "feat".to_string(),
            detected_scope: String::new(),
            proposed_message: String::new(),
            file_changes: make_file_changes(&[
                ("M", "src/cli/commands.rs"),
                ("M", "src/git/remote.rs"),
            ]),
            diff_summary: String::new(),
            diff_file: String::new(),
            file_diffs: Vec::new(),
        };
        analysis.refine_scope(&scope_defs);
        // Both have specificity 2 and both match → joined
        assert!(
            analysis.detected_scope == "cli, git" || analysis.detected_scope == "git, cli",
            "expected joined scopes, got: {}",
            analysis.detected_scope
        );
    }

    // ── refine_message_scope ───────────────────────────────────────────

    #[test]
    fn refine_message_scope_replaces_less_specific() {
        let scope_defs = vec![
            make_scope_def("ci", &[".github/**"]),
            make_scope_def("workflows", &[".github/workflows/**"]),
        ];
        let files = &[".github/workflows/ci.yml"];
        let result = super::refine_message_scope(
            "chore(ci): bump EmbarkStudios/cargo-deny-action from 2.0.15 to 2.0.17",
            files,
            &scope_defs,
        );
        assert_eq!(
            result,
            "chore(workflows): bump EmbarkStudios/cargo-deny-action from 2.0.15 to 2.0.17"
        );
    }

    #[test]
    fn refine_message_scope_keeps_already_correct() {
        let scope_defs = vec![
            make_scope_def("ci", &[".github/**"]),
            make_scope_def("workflows", &[".github/workflows/**"]),
        ];
        let files = &[".github/workflows/ci.yml"];
        let msg = "chore(workflows): bump something";
        assert_eq!(super::refine_message_scope(msg, files, &scope_defs), msg);
    }

    #[test]
    fn refine_message_scope_no_scope_in_message() {
        let scope_defs = vec![make_scope_def("cli", &["src/cli/**"])];
        let files = &["src/cli/commands.rs"];
        let msg = "chore: do something";
        assert_eq!(super::refine_message_scope(msg, files, &scope_defs), msg);
    }

    #[test]
    fn refine_message_scope_preserves_body() {
        let scope_defs = vec![
            make_scope_def("ci", &[".github/**"]),
            make_scope_def("workflows", &[".github/workflows/**"]),
        ];
        let files = &[".github/workflows/ci.yml"];
        let msg = "chore(ci): bump dep\n\nSome body text\nMore details";
        let result = super::refine_message_scope(msg, files, &scope_defs);
        assert_eq!(
            result,
            "chore(workflows): bump dep\n\nSome body text\nMore details"
        );
    }

    #[test]
    fn refine_message_scope_breaking_change() {
        let scope_defs = vec![
            make_scope_def("ci", &[".github/**"]),
            make_scope_def("workflows", &[".github/workflows/**"]),
        ];
        let files = &[".github/workflows/ci.yml"];
        let result = super::refine_message_scope("feat!(ci): breaking change", files, &scope_defs);
        assert_eq!(result, "feat!(workflows): breaking change");
    }

    #[test]
    fn refine_message_scope_no_matching_scope_defs() {
        let scope_defs = vec![make_scope_def("cli", &["src/cli/**"])];
        let files = &["README.md"];
        let msg = "docs(docs): update readme";
        assert_eq!(super::refine_message_scope(msg, files, &scope_defs), msg);
    }

    // ── run_pre_validation_checks ────────────────────────────────────

    fn make_commit_info_for_ai(message: &str) -> CommitInfoForAI {
        CommitInfoForAI {
            base: CommitInfo {
                hash: "a".repeat(40),
                author: "Test <test@example.com>".to_string(),
                date: chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00+00:00").unwrap(),
                original_message: message.to_string(),
                in_main_branches: vec![],
                analysis: CommitAnalysisForAI {
                    base: CommitAnalysis {
                        detected_type: "feat".to_string(),
                        detected_scope: String::new(),
                        proposed_message: String::new(),
                        file_changes: make_file_changes(&[]),
                        diff_summary: String::new(),
                        diff_file: String::new(),
                        file_diffs: Vec::new(),
                    },
                    diff_content: String::new(),
                },
            },
            pre_validated_checks: vec![],
        }
    }

    #[test]
    fn pre_validation_valid_single_scope() {
        let scopes = vec![make_scope_def("cli", &["src/cli/**"])];
        let mut info = make_commit_info_for_ai("feat(cli): add command");
        info.run_pre_validation_checks(&scopes);
        assert!(
            info.pre_validated_checks
                .iter()
                .any(|c| c.contains("Scope validity verified")),
            "expected scope validity check, got: {:?}",
            info.pre_validated_checks
        );
    }

    #[test]
    fn pre_validation_multi_scope() {
        let scopes = vec![
            make_scope_def("cli", &["src/cli/**"]),
            make_scope_def("git", &["src/git/**"]),
        ];
        let mut info = make_commit_info_for_ai("feat(cli,git): cross-cutting change");
        info.run_pre_validation_checks(&scopes);
        assert!(info
            .pre_validated_checks
            .iter()
            .any(|c| c.contains("Scope validity verified")),);
        assert!(info
            .pre_validated_checks
            .iter()
            .any(|c| c.contains("multi-scope")),);
    }

    #[test]
    fn pre_validation_multi_scope_with_spaces() {
        let scopes = vec![
            make_scope_def("cli", &["src/cli/**"]),
            make_scope_def("lib", &["src/lib/**"]),
        ];
        let mut info = make_commit_info_for_ai("feat(cli, lib): add something");
        info.run_pre_validation_checks(&scopes);
        assert!(
            info.pre_validated_checks
                .iter()
                .any(|c| c.contains("Scope validity verified")),
            "expected scope validity check for spaced multi-scope, got: {:?}",
            info.pre_validated_checks
        );
        assert!(
            info.pre_validated_checks
                .iter()
                .any(|c| c.contains("Scope format verified")),
            "single-space-after-comma multi-scope should pass the format check, got: {:?}",
            info.pre_validated_checks
        );
    }

    #[test]
    fn pre_validation_multi_scope_double_space_not_format_verified() {
        let scopes = vec![
            make_scope_def("cli", &["src/cli/**"]),
            make_scope_def("lib", &["src/lib/**"]),
        ];
        let mut info = make_commit_info_for_ai("feat(cli,  lib): add something");
        info.run_pre_validation_checks(&scopes);
        assert!(
            !info
                .pre_validated_checks
                .iter()
                .any(|c| c.contains("Scope format verified")),
            "double-space-after-comma must NOT be recorded as format-verified, got: {:?}",
            info.pre_validated_checks
        );
    }

    #[test]
    fn pre_validation_multi_scope_space_before_comma_not_format_verified() {
        let scopes = vec![
            make_scope_def("cli", &["src/cli/**"]),
            make_scope_def("lib", &["src/lib/**"]),
        ];
        let mut info = make_commit_info_for_ai("feat(cli ,lib): add something");
        info.run_pre_validation_checks(&scopes);
        assert!(
            !info
                .pre_validated_checks
                .iter()
                .any(|c| c.contains("Scope format verified")),
            "space-before-comma must NOT be recorded as format-verified, got: {:?}",
            info.pre_validated_checks
        );
    }

    #[test]
    fn pre_validation_invalid_scope_not_added() {
        let scopes = vec![make_scope_def("cli", &["src/cli/**"])];
        let mut info = make_commit_info_for_ai("feat(unknown): something");
        info.run_pre_validation_checks(&scopes);
        assert!(
            !info
                .pre_validated_checks
                .iter()
                .any(|c| c.contains("Scope validity verified")),
            "should not validate unknown scope"
        );
    }

    #[test]
    fn pre_validation_no_scope_message() {
        let scopes = vec![make_scope_def("cli", &["src/cli/**"])];
        let mut info = make_commit_info_for_ai("feat: no scope here");
        info.run_pre_validation_checks(&scopes);
        assert!(info.pre_validated_checks.is_empty());
    }

    // ── property tests ────────────────────────────────────────────

    mod prop {
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn count_specificity_nonnegative(pattern in ".*") {
                // usize is always >= 0; this test catches panics on arbitrary input
                let _ = super::count_specificity(&pattern);
            }

            #[test]
            fn count_specificity_bounded_by_segments(
                segments in proptest::collection::vec("[a-z*?]{1,10}", 1..6),
            ) {
                let pattern = segments.join("/");
                let result = super::count_specificity(&pattern);
                prop_assert!(result <= segments.len());
            }
        }
    }

    // ── conversion tests ────────────────────────────────────────────

    #[test]
    fn from_commit_analysis_loads_diff_content() {
        let dir = tempfile::tempdir().unwrap();
        let diff_path = dir.path().join("test.diff");
        std::fs::write(&diff_path, "+added line\n-removed line\n").unwrap();

        let analysis = CommitAnalysis {
            detected_type: "feat".to_string(),
            detected_scope: "cli".to_string(),
            proposed_message: "feat(cli): test".to_string(),
            file_changes: make_file_changes(&[]),
            diff_summary: "file.rs | 2 +-".to_string(),
            diff_file: diff_path.to_string_lossy().to_string(),
            file_diffs: Vec::new(),
        };

        let ai = CommitAnalysisForAI::from_commit_analysis(analysis.clone()).unwrap();
        assert_eq!(ai.diff_content, "+added line\n-removed line\n");
        assert_eq!(ai.base.detected_type, analysis.detected_type);
        assert_eq!(ai.base.diff_file, analysis.diff_file);
    }

    #[test]
    fn from_commit_info_wraps_and_loads_diff() {
        let dir = tempfile::tempdir().unwrap();
        let diff_path = dir.path().join("test.diff");
        std::fs::write(&diff_path, "diff content").unwrap();

        let info = CommitInfo {
            hash: "a".repeat(40),
            author: "Test <test@example.com>".to_string(),
            date: chrono::DateTime::parse_from_rfc3339("2024-01-01T00:00:00+00:00").unwrap(),
            original_message: "feat(cli): add flag".to_string(),
            in_main_branches: vec!["origin/main".to_string()],
            analysis: CommitAnalysis {
                detected_type: "feat".to_string(),
                detected_scope: "cli".to_string(),
                proposed_message: "feat(cli): add flag".to_string(),
                file_changes: make_file_changes(&[("M", "src/cli.rs")]),
                diff_summary: "cli.rs | 1 +".to_string(),
                diff_file: diff_path.to_string_lossy().to_string(),
                file_diffs: Vec::new(),
            },
        };

        let ai = CommitInfoForAI::from_commit_info(info).unwrap();
        assert_eq!(ai.base.analysis.diff_content, "diff content");
        assert_eq!(ai.base.hash, "a".repeat(40));
        assert_eq!(ai.base.original_message, "feat(cli): add flag");
        assert!(ai.pre_validated_checks.is_empty());
    }

    #[test]
    fn file_diffs_default_empty_on_deserialize() {
        let yaml = r#"
detected_type: feat
detected_scope: cli
proposed_message: "feat(cli): test"
file_changes:
  total_files: 0
  files_added: 0
  files_deleted: 0
  file_list: []
diff_summary: ""
diff_file: "/tmp/test.diff"
"#;
        let analysis: CommitAnalysis = serde_yaml::from_str(yaml).unwrap();
        assert!(analysis.file_diffs.is_empty());
    }

    #[test]
    fn file_diffs_omitted_when_empty_on_serialize() {
        let analysis = CommitAnalysis {
            detected_type: "feat".to_string(),
            detected_scope: "cli".to_string(),
            proposed_message: "feat(cli): test".to_string(),
            file_changes: make_file_changes(&[]),
            diff_summary: String::new(),
            diff_file: String::new(),
            file_diffs: Vec::new(),
        };
        let yaml = serde_yaml::to_string(&analysis).unwrap();
        assert!(!yaml.contains("file_diffs"));
    }

    #[test]
    fn file_diffs_included_when_populated() {
        let analysis = CommitAnalysis {
            detected_type: "feat".to_string(),
            detected_scope: "cli".to_string(),
            proposed_message: "feat(cli): test".to_string(),
            file_changes: make_file_changes(&[]),
            diff_summary: String::new(),
            diff_file: String::new(),
            file_diffs: vec![FileDiffRef {
                path: "src/main.rs".to_string(),
                diff_file: "/tmp/diffs/abc/0000.diff".to_string(),
                byte_len: 42,
            }],
        };
        let yaml = serde_yaml::to_string(&analysis).unwrap();
        assert!(yaml.contains("file_diffs"));
        assert!(yaml.contains("src/main.rs"));
        assert!(yaml.contains("byte_len: 42"));
    }

    // ── from_commit_info_partial ────────────────────────────────────

    /// Helper: creates a `CommitInfo` with N file diffs backed by temp files.
    fn make_commit_with_file_diffs(
        dir: &tempfile::TempDir,
        files: &[(&str, &str)], // (path, diff_content)
    ) -> CommitInfo {
        let file_diffs: Vec<FileDiffRef> = files
            .iter()
            .enumerate()
            .map(|(i, (path, content))| {
                let diff_path = dir.path().join(format!("{i:04}.diff"));
                fs::write(&diff_path, content).unwrap();
                FileDiffRef {
                    path: (*path).to_string(),
                    diff_file: diff_path.to_string_lossy().to_string(),
                    byte_len: content.len(),
                }
            })
            .collect();

        CommitInfo {
            hash: "abc123def456abc123def456abc123def456abc1".to_string(),
            author: "Test Author".to_string(),
            date: DateTime::parse_from_rfc3339("2025-01-01T00:00:00+00:00").unwrap(),
            original_message: "feat(cli): original message".to_string(),
            in_main_branches: vec!["main".to_string()],
            analysis: CommitAnalysis {
                detected_type: "feat".to_string(),
                detected_scope: "cli".to_string(),
                proposed_message: "feat(cli): proposed".to_string(),
                file_changes: make_file_changes(
                    &files.iter().map(|(p, _)| ("M", *p)).collect::<Vec<_>>(),
                ),
                diff_summary: " src/main.rs | 10 ++++\n src/lib.rs | 5 ++\n".to_string(),
                diff_file: dir.path().join("full.diff").to_string_lossy().to_string(),
                file_diffs,
            },
        }
    }

    #[test]
    fn from_commit_info_partial_loads_subset() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let commit = make_commit_with_file_diffs(
            &dir,
            &[
                ("src/main.rs", "diff --git a/src/main.rs\n+main\n"),
                ("src/lib.rs", "diff --git a/src/lib.rs\n+lib\n"),
                ("src/utils.rs", "diff --git a/src/utils.rs\n+utils\n"),
            ],
        );

        let paths = vec!["src/main.rs".to_string(), "src/utils.rs".to_string()];
        let partial = CommitInfoForAI::from_commit_info_partial(commit, &paths)?;

        // Only requested files in diff_content
        assert!(partial.base.analysis.diff_content.contains("+main"));
        assert!(partial.base.analysis.diff_content.contains("+utils"));
        assert!(!partial.base.analysis.diff_content.contains("+lib"));

        // file_diffs filtered to requested paths
        let ref_paths: Vec<&str> = partial
            .base
            .analysis
            .base
            .file_diffs
            .iter()
            .map(|r| r.path.as_str())
            .collect();
        assert_eq!(ref_paths, &["src/main.rs", "src/utils.rs"]);

        Ok(())
    }

    #[test]
    fn from_commit_info_partial_deduplicates_paths() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let commit = make_commit_with_file_diffs(
            &dir,
            &[("src/main.rs", "diff --git a/src/main.rs\n+main\n")],
        );

        // Duplicate path (simulates hunk-split scenario)
        let paths = vec!["src/main.rs".to_string(), "src/main.rs".to_string()];
        let partial = CommitInfoForAI::from_commit_info_partial(commit, &paths)?;

        // Content loaded only once (no duplicate)
        assert_eq!(
            partial.base.analysis.diff_content.matches("+main").count(),
            1
        );

        Ok(())
    }

    #[test]
    fn from_commit_info_partial_preserves_metadata() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let commit = make_commit_with_file_diffs(
            &dir,
            &[("src/main.rs", "diff --git a/src/main.rs\n+main\n")],
        );

        let original_hash = commit.hash.clone();
        let original_author = commit.author.clone();
        let original_date = commit.date;
        let original_message = commit.original_message.clone();
        let original_summary = commit.analysis.diff_summary.clone();

        let paths = vec!["src/main.rs".to_string()];
        let partial = CommitInfoForAI::from_commit_info_partial(commit, &paths)?;

        assert_eq!(partial.base.hash, original_hash);
        assert_eq!(partial.base.author, original_author);
        assert_eq!(partial.base.date, original_date);
        assert_eq!(partial.base.original_message, original_message);
        assert_eq!(partial.base.analysis.base.diff_summary, original_summary);

        Ok(())
    }

    // ── from_commit_info_partial_with_overrides ─────────────────────

    #[test]
    fn with_overrides_uses_override_content() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let commit = make_commit_with_file_diffs(
            &dir,
            &[(
                "src/big.rs",
                "diff --git a/src/big.rs\n+full-file-content\n",
            )],
        );

        let paths = vec!["src/big.rs".to_string(), "src/big.rs".to_string()];
        let overrides = vec![
            Some("diff --git a/src/big.rs\n@@ -1,3 +1,4 @@\n+hunk1\n".to_string()),
            Some("diff --git a/src/big.rs\n@@ -10,3 +10,4 @@\n+hunk2\n".to_string()),
        ];
        let partial =
            CommitInfoForAI::from_commit_info_partial_with_overrides(commit, &paths, &overrides)?;

        // Should contain hunk content, NOT full file content.
        assert!(partial.base.analysis.diff_content.contains("+hunk1"));
        assert!(partial.base.analysis.diff_content.contains("+hunk2"));
        assert!(
            !partial
                .base
                .analysis
                .diff_content
                .contains("+full-file-content"),
            "should not contain full file content"
        );

        Ok(())
    }

    #[test]
    fn with_overrides_mixed_override_and_disk() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let commit = make_commit_with_file_diffs(
            &dir,
            &[
                ("src/big.rs", "diff --git a/src/big.rs\n+big-full\n"),
                ("src/small.rs", "diff --git a/src/small.rs\n+small-disk\n"),
            ],
        );

        let paths = vec!["src/big.rs".to_string(), "src/small.rs".to_string()];
        let overrides = vec![
            Some("diff --git a/src/big.rs\n@@ -1,3 +1,4 @@\n+big-hunk\n".to_string()),
            None, // load from disk
        ];
        let partial =
            CommitInfoForAI::from_commit_info_partial_with_overrides(commit, &paths, &overrides)?;

        // big.rs: override content
        assert!(partial.base.analysis.diff_content.contains("+big-hunk"));
        assert!(!partial.base.analysis.diff_content.contains("+big-full"));
        // small.rs: loaded from disk
        assert!(partial.base.analysis.diff_content.contains("+small-disk"));

        // Both files should appear in file_diffs metadata.
        let ref_paths: Vec<&str> = partial
            .base
            .analysis
            .base
            .file_diffs
            .iter()
            .map(|r| r.path.as_str())
            .collect();
        assert!(ref_paths.contains(&"src/big.rs"));
        assert!(ref_paths.contains(&"src/small.rs"));

        Ok(())
    }

    #[test]
    fn with_overrides_deduplicates_disk_reads() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let commit = make_commit_with_file_diffs(
            &dir,
            &[("src/main.rs", "diff --git a/src/main.rs\n+main\n")],
        );

        // Two None entries for same path (simulates duplicate whole-file items).
        let paths = vec!["src/main.rs".to_string(), "src/main.rs".to_string()];
        let overrides = vec![None, None];
        let partial =
            CommitInfoForAI::from_commit_info_partial_with_overrides(commit, &paths, &overrides)?;

        // Content loaded only once despite two None entries.
        assert_eq!(
            partial.base.analysis.diff_content.matches("+main").count(),
            1
        );

        Ok(())
    }

    #[test]
    fn with_overrides_preserves_metadata() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let commit = make_commit_with_file_diffs(
            &dir,
            &[("src/main.rs", "diff --git a/src/main.rs\n+main\n")],
        );

        let original_hash = commit.hash.clone();
        let original_author = commit.author.clone();
        let original_message = commit.original_message.clone();

        let paths = vec!["src/main.rs".to_string()];
        let overrides = vec![Some("+override-content\n".to_string())];
        let partial =
            CommitInfoForAI::from_commit_info_partial_with_overrides(commit, &paths, &overrides)?;

        assert_eq!(partial.base.hash, original_hash);
        assert_eq!(partial.base.author, original_author);
        assert_eq!(partial.base.original_message, original_message);
        assert!(partial.pre_validated_checks.is_empty());

        Ok(())
    }
}

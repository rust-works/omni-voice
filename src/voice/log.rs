//! Reader for `reflections.log`.
//!
//! The lines themselves are written elsewhere: `reflect` writes a
//! per-reflection summary line ([`crate::voice::reflect`]) and `listen`
//! brackets a run with `listen start` / `listen stop` bookends
//! ([`crate::voice::listen::log`]). This module parses those lines back for
//! `voice sessions show`, which reports how many reflections a session has
//! accumulated.
//!
//! A reflection line has the shape:
//!
//! ```text
//! <ts> <reflection_id> model=<m> cost_usd=<c> latency_ms=<n> events=<n> status=<s>
//! ```
//!
//! where `<reflection_id>` is a ULID or `review`. The `listen` bookends are
//! distinguished by their second token being the literal `listen`, and are
//! skipped by [`read_reflections`]. Malformed and blank lines are skipped so
//! a partially-written log never fails a read-only stats command.

use std::path::Path;

use anyhow::{Context, Result};

/// One parsed reflection summary line from `reflections.log`.
#[derive(Debug, Clone, PartialEq)]
pub struct ReflectionEntry {
    /// RFC3339 timestamp token (kept verbatim; not re-parsed).
    pub ts: String,
    /// Reflection identifier — a ULID, or `review` for a `review` pass.
    pub reflection_id: String,
    /// LLM model, if the line recorded one.
    pub model: Option<String>,
    /// Reported cost in USD. `None` when the backend reported no cost
    /// (the line carries the literal `cost_usd=unknown`, per #64/#72).
    pub cost_usd: Option<f64>,
    /// Wall-clock latency of the reflection, if recorded.
    pub latency_ms: Option<u64>,
    /// Number of events the reflection emitted, if recorded.
    pub events: Option<u64>,
    /// Terminal status token (e.g. `ok`, `error`), if recorded.
    pub status: Option<String>,
}

/// Reads and parses reflection entries from `reflections.log`.
///
/// Returns an empty vec when the file does not exist. `listen` bookends,
/// blank lines, and lines that don't parse as a reflection summary are
/// skipped rather than erroring — this feeds a read-only stats view.
pub fn read_reflections(path: &Path) -> Result<Vec<ReflectionEntry>> {
    let body = match std::fs::read_to_string(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(e).with_context(|| format!("reading reflections log at {}", path.display()))
        }
    };
    Ok(body.lines().filter_map(parse_line).collect())
}

/// Parses a single log line into a [`ReflectionEntry`], or `None` for
/// blanks, `listen` bookends, and unrecognised lines.
fn parse_line(line: &str) -> Option<ReflectionEntry> {
    let mut tokens = line.split_whitespace();
    let ts = tokens.next()?;
    let reflection_id = tokens.next()?;
    // `listen start` / `listen stop` bookends are not reflections.
    if reflection_id == "listen" {
        return None;
    }

    let mut entry = ReflectionEntry {
        ts: ts.to_string(),
        reflection_id: reflection_id.to_string(),
        model: None,
        cost_usd: None,
        latency_ms: None,
        events: None,
        status: None,
    };
    let mut saw_field = false;
    for token in tokens {
        let Some((key, val)) = token.split_once('=') else {
            continue;
        };
        saw_field = true;
        match key {
            "model" => entry.model = Some(val.to_string()),
            // `unknown` (or any unparseable value) records as absent cost.
            "cost_usd" => entry.cost_usd = val.parse::<f64>().ok(),
            "latency_ms" => entry.latency_ms = val.parse::<u64>().ok(),
            "events" => entry.events = val.parse::<u64>().ok(),
            "status" => entry.status = Some(val.to_string()),
            _ => {}
        }
    }
    // A bare "<ts> <id>" with no key=value fields is not a reflection line.
    saw_field.then_some(entry)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn reads_empty_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let entries = read_reflections(&tmp.path().join("nope.log")).unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn parses_reflection_line_fields() {
        let line = "2026-07-09T10:00:00Z 01HZX model=claude-opus-4-7 \
                    cost_usd=0.0123 latency_ms=812 events=3 status=ok";
        let e = parse_line(line).unwrap();
        assert_eq!(e.reflection_id, "01HZX");
        assert_eq!(e.model.as_deref(), Some("claude-opus-4-7"));
        assert_eq!(e.cost_usd, Some(0.0123));
        assert_eq!(e.latency_ms, Some(812));
        assert_eq!(e.events, Some(3));
        assert_eq!(e.status.as_deref(), Some("ok"));
    }

    #[test]
    fn unknown_cost_parses_as_none() {
        let line = "2026-07-09T10:00:00Z 01HZX model=m cost_usd=unknown \
                    latency_ms=5 events=1 status=ok";
        let e = parse_line(line).unwrap();
        assert_eq!(e.cost_usd, None);
    }

    #[test]
    fn skips_listen_bookends_and_blanks() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("reflections.log");
        std::fs::write(
            &path,
            "2026-07-09T10:00:00Z listen start session=morning backend=mock\n\
             \n\
             2026-07-09T10:01:00Z 01HZX model=m cost_usd=0.01 latency_ms=5 events=1 status=ok\n\
             2026-07-09T10:02:00Z review model=m cost_usd=unknown latency_ms=7 events=2 status=ok\n\
             2026-07-09T10:05:00Z listen stop reason=signal reflections=2 dropped_chunks=0\n",
        )
        .unwrap();
        let entries = read_reflections(&path).unwrap();
        assert_eq!(entries.len(), 2, "only the two reflection lines count");
        assert_eq!(entries[0].reflection_id, "01HZX");
        assert_eq!(entries[1].reflection_id, "review");
    }

    #[test]
    fn bare_line_without_fields_is_skipped() {
        assert!(parse_line("2026-07-09T10:00:00Z 01HZX").is_none());
        assert!(parse_line("").is_none());
    }
}

//! `voice listen` status lines for `reflections.log`.
//!
//! Reflection cost/latency lines are written by `reflect` itself
//! ([`crate::voice::reflect`]); these are the *session-level* bookends the
//! scheduler adds around them — a `listen start` line and a `listen stop`
//! summary carrying the reflection count and dropped-chunk total. Kept as
//! pure formatters (timestamp passed in) so they are unit-testable.

/// Formats the `listen start` line written when a session begins.
#[must_use]
pub fn session_start_line(now_rfc3339: &str, session_id: &str, backend: &str) -> String {
    format!("{now_rfc3339} listen start session={session_id} backend={backend}")
}

/// Formats the `listen stop` summary written when a session ends.
#[must_use]
pub fn session_stop_line(
    now_rfc3339: &str,
    reason: &str,
    reflections: u64,
    dropped_chunks: u64,
) -> String {
    format!(
        "{now_rfc3339} listen stop reason={reason} reflections={reflections} \
         dropped_chunks={dropped_chunks}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_line_has_expected_fields() {
        let line = session_start_line("2026-07-09T10:00:00Z", "morning", "mock");
        assert_eq!(
            line,
            "2026-07-09T10:00:00Z listen start session=morning backend=mock"
        );
    }

    #[test]
    fn stop_line_has_expected_fields() {
        let line = session_stop_line("2026-07-09T10:05:00Z", "signal", 3, 12);
        assert_eq!(
            line,
            "2026-07-09T10:05:00Z listen stop reason=signal reflections=3 dropped_chunks=12"
        );
    }
}

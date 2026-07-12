//! `omni-voice sessions` — list / show / gc session storage.
//!
//! Manages the session directories under `~/.omni-voice/voice/<id>/`
//! (see [`crate::voice::session`]). `list` enumerates sessions newest-first
//! with summary stats, `show` prints one session's `meta.yaml` plus computed
//! stats, and `gc` deletes sessions past a retention window (lock-aware, with
//! a confirmation prompt by default).
//!
//! The clap structs stay thin; the enumeration, rendering, and gc logic are
//! free functions that take an explicit `voice_root`, clock, and confirm
//! callback so they can be unit-tested against `tempfile` directories.

use std::io::Write;
use std::path::Path;
use std::time::SystemTime;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};

use crate::voice::events::{Event, EventKind};
use crate::voice::log::read_reflections;
use crate::voice::session::{self, lock_is_active, read_events, read_meta, SessionPaths};

/// Manages session storage under `~/.omni-voice/voice/`.
#[derive(Parser)]
pub struct SessionsCommand {
    /// Which `sessions` subcommand to run.
    #[command(subcommand)]
    pub command: SessionsSubcommand,
}

/// `sessions` subcommands.
#[derive(Subcommand)]
pub enum SessionsSubcommand {
    /// Lists sessions newest-first with summary stats.
    List,
    /// Prints a session's `meta.yaml` and computed stats.
    Show(ShowArgs),
    /// Deletes sessions whose last activity is older than a retention window.
    Gc(GcArgs),
}

/// Arguments for `sessions show`.
#[derive(Parser)]
pub struct ShowArgs {
    /// Session id under `~/.omni-voice/voice/<id>/`.
    #[arg(value_name = "SESSION_ID")]
    pub id: String,
}

/// Arguments for `sessions gc`.
#[derive(Parser)]
pub struct GcArgs {
    /// Delete sessions whose `last_modified` is older than this age.
    /// Accepts a `<number><unit>` shorthand where unit is `d` (days),
    /// `h` (hours), or `m` (minutes) — e.g. `30d`, `24h`, `90m`.
    #[arg(long, default_value = "30d")]
    pub older_than: String,

    /// Skip the confirmation prompt and delete immediately.
    #[arg(long)]
    pub yes: bool,
}

impl SessionsCommand {
    /// Executes the `sessions` command. Sync — pure filesystem work, no AI.
    pub fn execute(self) -> Result<()> {
        let root = session::voice_root()?;
        let mut stdout = std::io::stdout().lock();
        self.command.run(&root, &mut stdout)?;
        stdout.flush()?;
        Ok(())
    }
}

impl SessionsSubcommand {
    /// Dispatches the subcommand against `root`, writing all output to `w`.
    /// Split out from [`SessionsCommand::execute`] so it can be driven over a
    /// `tempfile` root and a captured buffer, independent of the real voice
    /// root and stdout.
    fn run(self, root: &Path, w: &mut impl Write) -> Result<()> {
        match self {
            Self::List => {
                let (summaries, skipped) = gather_sessions(root)?;
                report_skipped(&skipped, w)?;
                render_list(&summaries, w)?;
            }
            Self::Show(args) => show_session(root, &args.id, w)?,
            Self::Gc(args) => {
                let age = parse_age(&args.older_than)?;
                let assume_yes = args.yes;
                run_gc(
                    root,
                    age,
                    Utc::now(),
                    SystemTime::now(),
                    w,
                    |count, bytes| {
                        if assume_yes {
                            Ok(true)
                        } else {
                            prompt_confirm(count, bytes)
                        }
                    },
                )?;
            }
        }
        Ok(())
    }
}

/// Healthy session summaries plus `(id, error)` pairs for any directory
/// whose `meta.yaml` failed the strict read.
type GatherResult = (Vec<SessionSummary>, Vec<(String, String)>);

/// One-line summary of a session, as shown by `list`.
struct SessionSummary {
    id: String,
    created: DateTime<Utc>,
    last_modified: DateTime<Utc>,
    /// `item.create` minus `item.expire` events (net live items).
    items: i64,
    /// `spent_usd` from `meta.yaml` (`0.0` until #72 populates it).
    cost_usd: f64,
}

/// Enumerates the sessions under `root`, newest-first by `created`.
///
/// Directories without a `meta.yaml` (e.g. `models/`, `speakers/`) are not
/// sessions and are ignored. A session whose `meta.yaml` fails the strict
/// read is not dropped silently — it is returned in the second tuple element
/// as `(id, error)` so the caller can warn about it while still listing the
/// healthy sessions.
fn gather_sessions(root: &Path) -> Result<GatherResult> {
    let mut summaries = Vec::new();
    let mut skipped = Vec::new();

    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((summaries, skipped)),
        Err(e) => {
            return Err(e).with_context(|| format!("reading voice root at {}", root.display()))
        }
    };

    for entry in entries {
        let entry = entry.with_context(|| format!("reading an entry under {}", root.display()))?;
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        let paths = SessionPaths::under(root, &id);
        if !paths.meta.exists() {
            continue; // not a session directory
        }
        match summarize(&id, &paths) {
            Ok(s) => summaries.push(s),
            Err(e) => skipped.push((id, format!("{e:#}"))),
        }
    }

    summaries.sort_by_key(|s| std::cmp::Reverse(s.created));
    Ok((summaries, skipped))
}

/// Reads one session's `meta.yaml` (strict) and events to build a summary.
fn summarize(id: &str, paths: &SessionPaths) -> Result<SessionSummary> {
    let meta = read_meta(&paths.meta)?;
    let events = read_events(&paths.events)?;
    Ok(SessionSummary {
        id: id.to_string(),
        created: meta.created,
        last_modified: meta.last_modified,
        items: net_item_count(&events),
        cost_usd: meta.spent_usd,
    })
}

/// Net live-item count: `item.create` events minus `item.expire` events.
fn net_item_count(events: &[Event]) -> i64 {
    events
        .iter()
        .map(|e| match e.kind {
            EventKind::ItemCreate(_) => 1,
            EventKind::ItemExpire(_) => -1,
            _ => 0,
        })
        .sum()
}

/// Writes one `warning:` line per unreadable session.
fn report_skipped(skipped: &[(String, String)], w: &mut impl Write) -> Result<()> {
    for (id, err) in skipped {
        writeln!(w, "warning: skipping unreadable session {id}: {err}")?;
    }
    Ok(())
}

/// Renders the session table, newest-first.
fn render_list(summaries: &[SessionSummary], w: &mut impl Write) -> Result<()> {
    if summaries.is_empty() {
        writeln!(w, "No sessions found.")?;
        return Ok(());
    }
    writeln!(
        w,
        "{:<26}  {:<16}  {:>9}  {:>5}  {:>8}",
        "ID", "CREATED", "DURATION", "ITEMS", "COST"
    )?;
    for s in summaries {
        writeln!(
            w,
            "{:<26}  {:<16}  {:>9}  {:>5}  {:>8}",
            s.id,
            s.created.format("%Y-%m-%d %H:%M").to_string(),
            fmt_duration(s.last_modified - s.created),
            s.items,
            format!("${:.2}", s.cost_usd),
        )?;
    }
    Ok(())
}

/// Formats a session's active span as e.g. `1h 23m`, `2d 3h`, or `5m`.
fn fmt_duration(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {mins:02}m")
    } else {
        format!("{mins}m")
    }
}

/// Prints a session's `meta.yaml` verbatim, then computed stats: event
/// count by type, reflection count, and the first five lines of `todos.md`.
fn show_session(root: &Path, id: &str, w: &mut impl Write) -> Result<()> {
    let paths = SessionPaths::under(root, id);
    if !paths.meta.exists() {
        bail!("no such session: {id}");
    }
    // Strict read validates the meta before we present it.
    read_meta(&paths.meta).with_context(|| format!("session {id} has an invalid meta.yaml"))?;
    let raw = std::fs::read_to_string(&paths.meta)
        .with_context(|| format!("reading meta.yaml for session {id}"))?;
    writeln!(w, "{}", raw.trim_end())?;

    let events = read_events(&paths.events)?;
    writeln!(w, "\nevents ({} total):", events.len())?;
    for (name, count) in event_type_counts(&events) {
        writeln!(w, "  {name}: {count}")?;
    }

    let reflections = read_reflections(&paths.log)?;
    writeln!(w, "reflections: {}", reflections.len())?;

    let todos = paths.root.join("todos.md");
    if let Ok(body) = std::fs::read_to_string(&todos) {
        writeln!(w, "\ntodos.md (first 5 lines):")?;
        for line in body.lines().take(5) {
            writeln!(w, "  {line}")?;
        }
    }
    Ok(())
}

/// Counts events grouped by their `event_type`, in a stable display order.
fn event_type_counts(events: &[Event]) -> Vec<(&'static str, usize)> {
    const ORDER: [&str; 7] = [
        "item.create",
        "item.update",
        "item.expire",
        "item.complete",
        "decision.record",
        "research.note",
        "reflection.error",
    ];
    ORDER
        .iter()
        .filter_map(|name| {
            let count = events
                .iter()
                .filter(|e| event_type_name(&e.kind) == *name)
                .count();
            (count > 0).then_some((*name, count))
        })
        .collect()
}

/// The on-wire `event_type` discriminator for a [`EventKind`].
fn event_type_name(kind: &EventKind) -> &'static str {
    match kind {
        EventKind::ItemCreate(_) => "item.create",
        EventKind::ItemUpdate(_) => "item.update",
        EventKind::ItemExpire(_) => "item.expire",
        EventKind::ItemComplete(_) => "item.complete",
        EventKind::DecisionRecord(_) => "decision.record",
        EventKind::ResearchNote(_) => "research.note",
        EventKind::ReflectionError(_) => "reflection.error",
    }
}

/// Parses a `<number><unit>` retention shorthand into a [`chrono::Duration`].
fn parse_age(s: &str) -> Result<chrono::Duration> {
    let s = s.trim();
    if s.len() < 2 {
        bail!("invalid duration {s:?}; expected e.g. 30d, 24h, or 90m");
    }
    let (digits, unit) = s.split_at(s.len() - 1);
    let n: i64 = digits
        .parse()
        .with_context(|| format!("invalid duration number in {s:?}"))?;
    if n < 0 {
        bail!("duration must not be negative: {s:?}");
    }
    let dur = match unit {
        "d" => chrono::Duration::days(n),
        "h" => chrono::Duration::hours(n),
        "m" => chrono::Duration::minutes(n),
        other => bail!("unsupported duration unit {other:?}; use d, h, or m"),
    };
    Ok(dur)
}

/// Outcome of a `gc` run, returned for reporting and testing.
#[derive(Debug, Default, PartialEq, Eq)]
struct GcOutcome {
    /// Ids of sessions actually deleted.
    deleted: Vec<String>,
    /// Ids skipped because they looked active (held lock).
    skipped_active: Vec<String>,
    /// True when the user declined the confirmation prompt.
    cancelled: bool,
}

/// Deletes sessions whose `last_modified` is older than `age`, skipping any
/// that look actively held. `now`/`now_sys` are injected for testability;
/// `confirm(count, total_bytes)` decides whether to proceed (the CLI wires
/// it to `--yes` or an interactive prompt).
fn run_gc(
    root: &Path,
    age: chrono::Duration,
    now: DateTime<Utc>,
    now_sys: SystemTime,
    w: &mut impl Write,
    confirm: impl FnOnce(usize, u64) -> Result<bool>,
) -> Result<GcOutcome> {
    let (summaries, skipped) = gather_sessions(root)?;
    report_skipped(&skipped, w)?;

    let cutoff = now - age;
    let mut deletable = Vec::new();
    let mut skipped_active = Vec::new();
    let mut total_bytes = 0u64;

    for s in &summaries {
        if s.last_modified >= cutoff {
            continue;
        }
        let paths = SessionPaths::under(root, &s.id);
        if lock_is_active(&paths.lock, now_sys) {
            writeln!(w, "warning: session {} looks active, skipping", s.id)?;
            skipped_active.push(s.id.clone());
            continue;
        }
        total_bytes += dir_size(&paths.root).unwrap_or(0);
        deletable.push(s.id.clone());
    }

    if deletable.is_empty() {
        writeln!(w, "No sessions older than the retention window.")?;
        return Ok(GcOutcome {
            skipped_active,
            ..Default::default()
        });
    }

    if !confirm(deletable.len(), total_bytes)? {
        writeln!(w, "Aborted; nothing deleted.")?;
        return Ok(GcOutcome {
            skipped_active,
            cancelled: true,
            ..Default::default()
        });
    }

    for id in &deletable {
        let paths = SessionPaths::under(root, id);
        std::fs::remove_dir_all(&paths.root)
            .with_context(|| format!("deleting session {id} at {}", paths.root.display()))?;
        writeln!(w, "Deleted {id}")?;
    }

    Ok(GcOutcome {
        deleted: deletable,
        skipped_active,
        cancelled: false,
    })
}

/// Interactive `[y/N]` confirmation for `gc`. Reads a line from stdin and
/// treats only `y`/`yes` (case-insensitive) as consent.
fn prompt_confirm(count: usize, bytes: u64) -> Result<bool> {
    let mb = bytes as f64 / 1_048_576.0;
    print!("Delete {count} sessions totaling {mb:.1} MB? [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let answer = line.trim().to_ascii_lowercase();
    Ok(answer == "y" || answer == "yes")
}

/// Total byte size of the files directly (and recursively) under `path`.
fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(path)? {
        let entry = entry?;
        let ft = entry.file_type()?;
        if ft.is_dir() {
            total += dir_size(&entry.path())?;
        } else {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::events::{
        ExpireReason, ItemClass, ItemCreate, ItemExpire, Provenance, ReflectionId,
    };
    use crate::voice::session::{append_events, write_meta, SessionMeta};
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn ts(day: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 5, day, 10, 0, 0).unwrap()
    }

    /// Writes a minimal session directory with the given timestamps.
    fn make_session(root: &Path, id: &str, created: DateTime<Utc>, last_modified: DateTime<Utc>) {
        let paths = SessionPaths::under(root, id);
        std::fs::create_dir_all(&paths.root).unwrap();
        let mut meta = SessionMeta::new(id, created, "p");
        meta.last_modified = last_modified;
        write_meta(&paths.meta, &meta).unwrap();
    }

    fn event(seq: u128, kind: EventKind) -> Event {
        Event {
            event_id: ulid::Ulid::from_parts(0, seq),
            ts: ts(13),
            reflection_id: ReflectionId::Ulid(ulid::Ulid::from_parts(0, 900)),
            provenance: Provenance {
                transcript_span: None,
                model: None,
                prompt_version: None,
            },
            kind,
        }
    }

    fn create(seq: u128, item: u128) -> Event {
        event(
            seq,
            EventKind::ItemCreate(ItemCreate {
                item_id: ulid::Ulid::from_parts(0, item),
                class: ItemClass::Todo,
                text: format!("item {item}"),
                priority: None,
                valid_until: None,
                tags: None,
            }),
        )
    }

    fn expire(seq: u128, item: u128) -> Event {
        event(
            seq,
            EventKind::ItemExpire(ItemExpire {
                item_id: ulid::Ulid::from_parts(0, item),
                reason: ExpireReason::Ttl,
                superseded_by: None,
            }),
        )
    }

    #[test]
    fn parse_age_accepts_d_h_m() {
        assert_eq!(parse_age("30d").unwrap(), chrono::Duration::days(30));
        assert_eq!(parse_age("24h").unwrap(), chrono::Duration::hours(24));
        assert_eq!(parse_age("90m").unwrap(), chrono::Duration::minutes(90));
    }

    #[test]
    fn parse_age_rejects_bad_input() {
        assert!(parse_age("30").is_err(), "missing unit");
        assert!(parse_age("30w").is_err(), "unsupported unit");
        assert!(parse_age("xd").is_err(), "non-numeric");
        assert!(parse_age("-5d").is_err(), "negative");
    }

    #[test]
    fn fmt_duration_shapes() {
        assert_eq!(fmt_duration(chrono::Duration::minutes(5)), "5m");
        assert_eq!(
            fmt_duration(chrono::Duration::hours(1) + chrono::Duration::minutes(23)),
            "1h 23m"
        );
        assert_eq!(
            fmt_duration(chrono::Duration::days(2) + chrono::Duration::hours(3)),
            "2d 3h"
        );
    }

    #[test]
    fn gather_sorts_newest_first_and_ignores_non_sessions() {
        let tmp = TempDir::new().unwrap();
        make_session(tmp.path(), "older", ts(10), ts(10));
        make_session(tmp.path(), "newer", ts(12), ts(12));
        // A non-session directory (no meta.yaml) must be ignored.
        std::fs::create_dir_all(tmp.path().join("models")).unwrap();

        let (summaries, skipped) = gather_sessions(tmp.path()).unwrap();
        assert!(skipped.is_empty());
        let ids: Vec<_> = summaries.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["newer", "older"]);
    }

    #[test]
    fn gather_reports_unreadable_session() {
        let tmp = TempDir::new().unwrap();
        let bad = SessionPaths::under(tmp.path(), "corrupt");
        std::fs::create_dir_all(&bad.root).unwrap();
        std::fs::write(&bad.meta, "prompt_version: abc\n").unwrap(); // missing required fields

        let (summaries, skipped) = gather_sessions(tmp.path()).unwrap();
        assert!(summaries.is_empty());
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].0, "corrupt");
    }

    #[test]
    fn gather_on_missing_root_is_empty() {
        let tmp = TempDir::new().unwrap();
        let (summaries, skipped) = gather_sessions(&tmp.path().join("nope")).unwrap();
        assert!(summaries.is_empty() && skipped.is_empty());
    }

    #[test]
    fn gc_deletes_old_and_keeps_recent() {
        let tmp = TempDir::new().unwrap();
        make_session(tmp.path(), "old", ts(1), ts(1));
        make_session(tmp.path(), "recent", ts(1), ts(20));
        let now = ts(25);

        let mut out = Vec::new();
        let outcome = run_gc(
            tmp.path(),
            chrono::Duration::days(10),
            now,
            SystemTime::now(),
            &mut out,
            |_, _| Ok(true),
        )
        .unwrap();

        assert_eq!(outcome.deleted, ["old"]);
        assert!(!tmp.path().join("old").exists());
        assert!(tmp.path().join("recent").exists());
    }

    #[test]
    fn gc_declined_deletes_nothing() {
        let tmp = TempDir::new().unwrap();
        make_session(tmp.path(), "old", ts(1), ts(1));
        let now = ts(25);

        let mut out = Vec::new();
        let outcome = run_gc(
            tmp.path(),
            chrono::Duration::days(10),
            now,
            SystemTime::now(),
            &mut out,
            |_, _| Ok(false),
        )
        .unwrap();

        assert!(outcome.cancelled);
        assert!(outcome.deleted.is_empty());
        assert!(tmp.path().join("old").exists());
    }

    #[test]
    fn gc_skips_active_locked_session() {
        let tmp = TempDir::new().unwrap();
        make_session(tmp.path(), "old", ts(1), ts(1));
        // A fresh lock held by our own (live) PID marks it active.
        let paths = SessionPaths::under(tmp.path(), "old");
        session::write_lock(&paths.lock, std::process::id()).unwrap();
        let now = ts(25);

        let mut out = Vec::new();
        let outcome = run_gc(
            tmp.path(),
            chrono::Duration::days(10),
            now,
            SystemTime::now(),
            &mut out,
            |_, _| Ok(true),
        )
        .unwrap();

        assert_eq!(outcome.skipped_active, ["old"]);
        assert!(outcome.deleted.is_empty());
        assert!(tmp.path().join("old").exists());
    }

    #[test]
    fn show_errors_on_missing_session() {
        let tmp = TempDir::new().unwrap();
        let mut out = Vec::new();
        assert!(show_session(tmp.path(), "ghost", &mut out).is_err());
    }

    #[test]
    fn net_item_count_creates_minus_expires() {
        let events = [create(1, 10), create(2, 11), expire(3, 10)];
        assert_eq!(net_item_count(&events), 1);
    }

    #[test]
    fn event_type_counts_groups_in_wire_order() {
        let events = [create(1, 10), create(2, 11), expire(3, 10)];
        assert_eq!(
            event_type_counts(&events),
            vec![("item.create", 2), ("item.expire", 1)]
        );
    }

    #[test]
    fn dir_size_sums_files_recursively() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), b"hello").unwrap(); // 5
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("sub/b.txt"), b"xyz").unwrap(); // 3
        assert_eq!(dir_size(tmp.path()).unwrap(), 8);
    }

    #[test]
    fn render_list_empty_says_none() {
        let mut out = Vec::new();
        render_list(&[], &mut out).unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "No sessions found.\n");
    }

    #[test]
    fn render_list_formats_header_and_row() {
        let summaries = [SessionSummary {
            id: "01ABCXYZ".into(),
            created: ts(13),
            last_modified: ts(13) + chrono::Duration::hours(1) + chrono::Duration::minutes(23),
            items: 3,
            cost_usd: 0.12,
        }];
        let mut out = Vec::new();
        render_list(&summaries, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("ID") && text.contains("CREATED") && text.contains("COST"));
        assert!(text.contains("01ABCXYZ"));
        assert!(text.contains("2026-05-13 10:00"));
        assert!(text.contains("1h 23m"));
        assert!(text.contains('3'));
        assert!(text.contains("$0.12"));
    }

    #[test]
    fn show_prints_meta_events_reflections_and_todos() {
        let tmp = TempDir::new().unwrap();
        make_session(tmp.path(), "s1", ts(13), ts(13));
        let paths = SessionPaths::under(tmp.path(), "s1");
        append_events(
            &paths.events,
            &[create(1, 10), create(2, 11), expire(3, 10)],
        )
        .unwrap();
        std::fs::write(
            &paths.log,
            "2026-05-13T10:31:00Z 01AA model=m cost_usd=unknown latency_ms=5 events=1 status=ok\n",
        )
        .unwrap();
        std::fs::write(paths.root.join("todos.md"), "# Todos\n- [ ] a\n- [ ] b\n").unwrap();

        let mut out = Vec::new();
        show_session(tmp.path(), "s1", &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("session_id: s1"));
        assert!(text.contains("item.create: 2"));
        assert!(text.contains("item.expire: 1"));
        assert!(text.contains("reflections: 1"));
        assert!(text.contains("todos.md (first 5 lines):"));
        assert!(text.contains("# Todos"));
    }

    #[test]
    fn dispatch_list_renders_sessions() {
        let tmp = TempDir::new().unwrap();
        make_session(tmp.path(), "s1", ts(13), ts(13));
        let mut out = Vec::new();
        SessionsSubcommand::List.run(tmp.path(), &mut out).unwrap();
        assert!(String::from_utf8(out).unwrap().contains("s1"));
    }

    #[test]
    fn dispatch_show_prints_meta() {
        let tmp = TempDir::new().unwrap();
        make_session(tmp.path(), "s1", ts(13), ts(13));
        let mut out = Vec::new();
        SessionsSubcommand::Show(ShowArgs { id: "s1".into() })
            .run(tmp.path(), &mut out)
            .unwrap();
        assert!(String::from_utf8(out).unwrap().contains("session_id: s1"));
    }

    #[test]
    fn dispatch_gc_with_yes_deletes_old() {
        let tmp = TempDir::new().unwrap();
        make_session(tmp.path(), "old", ts(1), ts(1));
        let mut out = Vec::new();
        SessionsSubcommand::Gc(GcArgs {
            older_than: "1d".into(),
            yes: true,
        })
        .run(tmp.path(), &mut out)
        .unwrap();
        assert!(!tmp.path().join("old").exists());
    }
}

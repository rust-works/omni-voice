//! `~/.omni-voice/voice/<id>/` session directory I/O.
//!
//! Lays out and reads the session directory format from #799:
//!
//! ```text
//! ~/.omni-voice/voice/<session-id>/
//!   meta.yaml          # session config (this issue: last_reflected_event_id + ttl defaults)
//!   transcript.jsonl   # append-only TranscriptEvent stream from `transcribe`
//!   events.jsonl       # append-only Event stream from `reflect` (and later `review`)
//!   reflections.log    # per-reflection summary line (cost, latency, status)
//! ```
//!
//! Shared with #804 (`review`), which reads the same `events.jsonl`
//! to produce materialised markdown projections. The session root path is
//! derived from `dirs::home_dir()` by default; the `OMNI_VOICE_VOICE_ROOT`
//! environment variable overrides it (intended for tests, not a stable
//! user-facing knob).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::voice::events::Event;
use crate::voice::{EventId, TranscriptEvent};

/// Filesystem paths under a single session directory.
#[derive(Debug, Clone)]
pub struct SessionPaths {
    /// Session root (`<voice-root>/<id>`).
    pub root: PathBuf,
    /// `meta.yaml` — session config (parsed into [`SessionMeta`]).
    pub meta: PathBuf,
    /// `transcript.jsonl` — `TranscriptEvent` log.
    pub transcript: PathBuf,
    /// `events.jsonl` — reflection [`Event`] log.
    pub events: PathBuf,
    /// `reflections.log` — per-reflection summary lines.
    pub log: PathBuf,
    /// `session.lock` — PID lockfile held by a live `listen`.
    pub lock: PathBuf,
}

impl SessionPaths {
    /// Builds [`SessionPaths`] under `voice_root/<id>` without touching disk.
    #[must_use]
    pub fn under(voice_root: &Path, id: &str) -> Self {
        let root = voice_root.join(id);
        Self {
            meta: root.join("meta.yaml"),
            transcript: root.join("transcript.jsonl"),
            events: root.join("events.jsonl"),
            log: root.join("reflections.log"),
            lock: root.join("session.lock"),
            root,
        }
    }
}

/// Default TTLs per item class (per #799), stored in `meta.yaml` so a
/// session can override them. Serialised as ISO-8601 durations (`P7D`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TtlDefaults {
    /// TTL for `class: todo` items.
    #[serde(with = "ttl_iso8601")]
    pub todo: Duration,
    /// TTL for `class: research` items.
    #[serde(with = "ttl_iso8601")]
    pub research: Duration,
    /// TTL for `class: question` items.
    #[serde(with = "ttl_iso8601")]
    pub question: Duration,
    /// TTL for `decision.record` items; `null` means "never expires".
    #[serde(with = "ttl_iso8601_opt", default)]
    pub decision: Option<Duration>,
}

impl Default for TtlDefaults {
    fn default() -> Self {
        Self {
            todo: Duration::from_secs(7 * 86_400),
            research: Duration::from_secs(30 * 86_400),
            question: Duration::from_secs(14 * 86_400),
            decision: None,
        }
    }
}

/// Renders a [`Duration`] as an ISO-8601 duration string, and parses one
/// back. Only the day/hour/minute/second components are used — ample for
/// per-class TTLs, and free of the calendar ambiguity of `Y`/`M`.
mod iso8601 {
    use std::time::Duration;

    /// Formats whole-second `d` as `P[nD][T[nH][nM][nS]]` (e.g. `P7D`).
    #[must_use]
    pub fn to_string(d: Duration) -> String {
        let mut secs = d.as_secs();
        let days = secs / 86_400;
        secs %= 86_400;
        let hours = secs / 3_600;
        secs %= 3_600;
        let mins = secs / 60;
        let s = secs % 60;

        let mut out = String::from("P");
        if days > 0 {
            out.push_str(&format!("{days}D"));
        }
        if hours > 0 || mins > 0 || s > 0 {
            out.push('T');
            if hours > 0 {
                out.push_str(&format!("{hours}H"));
            }
            if mins > 0 {
                out.push_str(&format!("{mins}M"));
            }
            if s > 0 {
                out.push_str(&format!("{s}S"));
            }
        }
        if out == "P" {
            out.push_str("T0S");
        }
        out
    }

    /// Parses `P[nW][nD][T[nH][nM][nS]]` into a [`Duration`].
    pub fn parse(s: &str) -> Result<Duration, String> {
        let rest = s
            .trim()
            .strip_prefix('P')
            .ok_or_else(|| format!("ISO-8601 duration must start with 'P': {s:?}"))?;
        let (date_part, time_part) = rest.split_once('T').unwrap_or((rest, ""));
        let mut total = parse_units(date_part, &[('W', 604_800), ('D', 86_400)])?;
        total += parse_units(time_part, &[('H', 3_600), ('M', 60), ('S', 1)])?;
        Ok(Duration::from_secs(total))
    }

    fn parse_units(part: &str, units: &[(char, u64)]) -> Result<u64, String> {
        let mut total = 0u64;
        let mut num = String::new();
        for ch in part.chars() {
            if ch.is_ascii_digit() {
                num.push(ch);
            } else {
                let (_, mult) = units
                    .iter()
                    .find(|(u, _)| *u == ch)
                    .ok_or_else(|| format!("unsupported ISO-8601 duration unit {ch:?}"))?;
                let n: u64 = num
                    .parse()
                    .map_err(|_| format!("invalid number before {ch:?}"))?;
                total += n * mult;
                num.clear();
            }
        }
        if !num.is_empty() {
            return Err(format!(
                "ISO-8601 duration has a trailing number without unit: {num:?}"
            ));
        }
        Ok(total)
    }
}

mod ttl_iso8601 {
    use super::iso8601;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&iso8601::to_string(*d))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        let s = String::deserialize(d)?;
        iso8601::parse(&s).map_err(serde::de::Error::custom)
    }
}

mod ttl_iso8601_opt {
    use super::iso8601;
    use serde::{Deserialize, Deserializer, Serializer};
    use std::time::Duration;

    pub fn serialize<S: Serializer>(d: &Option<Duration>, s: S) -> Result<S::Ok, S::Error> {
        match d {
            Some(d) => s.serialize_str(&iso8601::to_string(*d)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Duration>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(s) => iso8601::parse(&s)
                .map(Some)
                .map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}

/// Parsed contents of `meta.yaml` — the canonical, frozen session schema
/// (#9).
///
/// Reads are strict: the required identifying fields (`session_id`,
/// `created`, `last_modified`, `prompt_version`) carry no serde default,
/// so a `meta.yaml` missing one fails to parse with serde's own
/// "missing field" error rather than being silently auto-filled — silent
/// defaults hide config drift. The context fields are nullable and default
/// to `None`/`0.0`, which are real values here (an unreflected session has
/// no `model`; a session that hasn't been listened to has no `backend`).
///
/// `budget_cap_usd` and `spent_usd` are part of the frozen schema but are
/// inert in #9: `budget_cap_usd` is readable, `spent_usd` stays `0.0`.
/// Budget-cap enforcement and cost accumulation land in #72.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionMeta {
    /// ULID identifying this session (also its directory name).
    pub session_id: String,
    /// When the session directory was minted (RFC3339 UTC).
    pub created: DateTime<Utc>,
    /// Last time `reflect` / `review` / `listen` mutated the session.
    pub last_modified: DateTime<Utc>,
    /// Enrolled speaker locked onto for this session; `None` = all speakers.
    #[serde(default)]
    pub speaker: Option<String>,
    /// Absolute project path this session is associated with, if any.
    #[serde(default)]
    pub project: Option<String>,
    /// ASR backend used in this session; set by `listen`/`transcribe`,
    /// `None` for a reflect-only session.
    #[serde(default)]
    pub backend: Option<String>,
    /// LLM model used for reflection; populated on the first reflect.
    #[serde(default)]
    pub model: Option<String>,
    /// sha256 prefix of the reflection prompt template in effect when the
    /// session was minted.
    pub prompt_version: String,
    /// Per-session budget cap; overrides `OMNI_VOICE_CLAUDE_CLI_MAX_BUDGET_USD`.
    /// Readable in #9; enforced in #72.
    #[serde(default)]
    pub budget_cap_usd: Option<f64>,
    /// Running reflection spend. Inert (`0.0`) in #9; accumulated in #72.
    #[serde(default)]
    pub spent_usd: f64,
    /// TTL defaults applied at projection time (consumed by #804).
    #[serde(default)]
    pub ttl_defaults: TtlDefaults,
    /// `event_id` of the last `TranscriptEvent::Final` consumed by a
    /// previous `reflect` invocation. `None` until the first
    /// reflection completes.
    #[serde(default)]
    pub last_reflected_event_id: Option<EventId>,
}

impl SessionMeta {
    /// Builds a fresh meta for `session_id` minted at `now`. Context fields
    /// (speaker, project, backend, model, budget cap) start empty and are
    /// filled in by the minting command; `spent_usd` starts at `0.0`.
    #[must_use]
    pub fn new(
        session_id: impl Into<String>,
        now: DateTime<Utc>,
        prompt_version: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            created: now,
            last_modified: now,
            speaker: None,
            project: None,
            backend: None,
            model: None,
            prompt_version: prompt_version.into(),
            budget_cap_usd: None,
            spent_usd: 0.0,
            ttl_defaults: TtlDefaults::default(),
            last_reflected_event_id: None,
        }
    }

    /// Bumps `last_modified` to `now`.
    pub fn touch(&mut self, now: DateTime<Utc>) {
        self.last_modified = now;
    }
}

/// Combination of paths and the parsed meta document.
#[derive(Debug, Clone)]
pub struct Session {
    /// On-disk paths under the session root.
    pub paths: SessionPaths,
    /// Parsed `meta.yaml` contents.
    pub meta: SessionMeta,
}

impl Session {
    /// Reads all `Final` transcript events from `transcript.jsonl` after
    /// `meta.last_reflected_event_id`. Non-`Final` events are skipped —
    /// reflection is driven by committed text only.
    pub fn read_transcript_finals_after(&self) -> Result<Vec<TranscriptEvent>> {
        read_transcript_finals_after(&self.paths.transcript, self.meta.last_reflected_event_id)
    }

    /// Reads `events.jsonl` into a [`Vec<Event>`]. Empty when the file
    /// doesn't exist or contains no events.
    pub fn read_events(&self) -> Result<Vec<Event>> {
        read_events(&self.paths.events)
    }

    /// Appends events to `events.jsonl`.
    pub fn append_events(&self, events: &[Event]) -> Result<()> {
        append_events(&self.paths.events, events)
    }

    /// Appends transcript events to `transcript.jsonl` (used by
    /// `voice listen` to persist streamed `Final`s as they arrive, before
    /// firing a reflection over them).
    pub fn append_transcript(&self, events: &[TranscriptEvent]) -> Result<()> {
        append_transcript(&self.paths.transcript, events)
    }

    /// Updates `meta.last_reflected_event_id` in memory and on disk, and
    /// bumps `last_modified` — `reflect` is a session-mutating write.
    pub fn set_last_reflected(&mut self, id: EventId) -> Result<()> {
        self.meta.last_reflected_event_id = Some(id);
        self.meta.touch(Utc::now());
        write_meta(&self.paths.meta, &self.meta)
    }

    /// Appends a single line to `reflections.log` (no implicit newline).
    pub fn append_log(&self, line: &str) -> Result<()> {
        append_log_line(&self.paths.log, line)
    }

    /// Writes `pid` to this session's `session.lock`.
    pub fn write_lock(&self, pid: u32) -> Result<()> {
        write_lock(&self.paths.lock, pid)
    }

    /// Removes this session's `session.lock` (a missing file is fine).
    pub fn remove_lock(&self) -> Result<()> {
        remove_lock(&self.paths.lock)
    }
}

/// Resolves the session root: `$OMNI_VOICE_VOICE_ROOT` if set, else
/// `~/.omni-voice/voice`.
pub fn voice_root() -> Result<PathBuf> {
    if let Ok(override_root) = std::env::var("OMNI_VOICE_VOICE_ROOT") {
        return Ok(PathBuf::from(override_root));
    }
    let home = dirs::home_dir().context(
        "could not determine HOME directory for ~/.omni-voice/voice; \
         set OMNI_VOICE_VOICE_ROOT to override",
    )?;
    Ok(home.join(".omni-voice").join("voice"))
}

/// Opens an existing session, or creates an empty one if the directory
/// doesn't exist. Bootstrap is idempotent: re-running against an
/// already-populated session reads the existing `meta.yaml`.
pub fn open_or_create(id: &str) -> Result<Session> {
    let root = voice_root()?;
    open_or_create_under(&root, id)
}

/// Variant of [`open_or_create`] that takes an explicit voice root —
/// useful for tests that drive several sessions under a `tempfile`
/// directory.
pub fn open_or_create_under(voice_root: &Path, id: &str) -> Result<Session> {
    if id.is_empty() {
        bail!("session id cannot be empty");
    }
    if id.contains('/') || id.contains('\\') || id == "." || id == ".." {
        bail!("session id must not contain path separators: {id:?}");
    }
    let paths = SessionPaths::under(voice_root, id);
    std::fs::create_dir_all(&paths.root)
        .with_context(|| format!("creating session directory at {}", paths.root.display()))?;

    // Bootstrap empty files (touch only — don't truncate existing ones).
    for p in [&paths.transcript, &paths.events, &paths.log] {
        if !p.exists() {
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .with_context(|| format!("creating {}", p.display()))?;
        }
    }

    let meta = if paths.meta.exists() {
        read_meta(&paths.meta)?
    } else {
        // Every required field is knowable from the id alone: the id, the
        // mint timestamp, and the compile-time prompt-version constant. The
        // minting command (`listen`) overwrites the nullable context fields
        // (backend, speaker, project, …) afterward.
        let m = SessionMeta::new(
            id,
            Utc::now(),
            crate::voice::reflect::prompt::prompt_version(),
        );
        write_meta(&paths.meta, &m)?;
        m
    };

    Ok(Session { paths, meta })
}

/// Reads and parses `meta.yaml`.
pub fn read_meta(path: &Path) -> Result<SessionMeta> {
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading session meta at {}", path.display()))?;
    serde_yaml::from_str(&body)
        .with_context(|| format!("parsing session meta at {}", path.display()))
}

/// Writes `meta.yaml` atomically (write-temp-then-rename).
pub fn write_meta(path: &Path, meta: &SessionMeta) -> Result<()> {
    let body = serde_yaml::to_string(meta).context("serialising session meta to YAML")?;
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, body.as_bytes())
        .with_context(|| format!("writing temp meta at {}", tmp.display()))?;
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming temp meta to {}", path.display()))?;
    Ok(())
}

/// Reads all `TranscriptEvent`s from a JSONL file. Blank lines are
/// skipped; parse errors include the line number.
pub fn read_transcript(path: &Path) -> Result<Vec<TranscriptEvent>> {
    let file =
        File::open(path).with_context(|| format!("opening transcript at {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading {}:{}", path.display(), idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: TranscriptEvent = serde_json::from_str(&line)
            .with_context(|| format!("parsing {}:{}", path.display(), idx + 1))?;
        events.push(event);
    }
    Ok(events)
}

/// Reads only `Final` transcript events after the optional marker.
///
/// Uses stream position (not ULID comparison) — finds the marker line
/// and returns Finals strictly after it. Errors if the marker is set
/// but not present in the file.
pub fn read_transcript_finals_after(
    path: &Path,
    after: Option<EventId>,
) -> Result<Vec<TranscriptEvent>> {
    let all = read_transcript(path)?;
    let finals: Vec<TranscriptEvent> = all
        .into_iter()
        .filter(|e| matches!(e, TranscriptEvent::Final { .. }))
        .collect();
    match after {
        None => Ok(finals),
        Some(target) => {
            let pos = finals.iter().position(|e| match e {
                TranscriptEvent::Final { event_id, .. } => *event_id == target,
                _ => false,
            });
            match pos {
                Some(idx) => Ok(finals.into_iter().skip(idx + 1).collect()),
                None => bail!(
                    "last_reflected_event_id {target} not found in transcript at {}; \
                     meta.yaml may be inconsistent with transcript.jsonl",
                    path.display()
                ),
            }
        }
    }
}

/// Reads all reflection [`Event`]s from `events.jsonl`. Returns an empty
/// vec if the file doesn't exist (greenfield session).
pub fn read_events(path: &Path) -> Result<Vec<Event>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file =
        File::open(path).with_context(|| format!("opening events log at {}", path.display()))?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line.with_context(|| format!("reading {}:{}", path.display(), idx + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let event: Event = serde_json::from_str(&line)
            .with_context(|| format!("parsing {}:{}", path.display(), idx + 1))?;
        events.push(event);
    }
    Ok(events)
}

/// Appends events as JSONL to `path`. Each event is one line, flushed
/// after the batch. Skips silently when `events` is empty.
pub fn append_events(path: &Path, events: &[Event]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening events log for append at {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    for e in events {
        serde_json::to_writer(&mut writer, e)
            .with_context(|| format!("serialising event to {}", path.display()))?;
        writer
            .write_all(b"\n")
            .with_context(|| format!("appending newline to {}", path.display()))?;
    }
    writer
        .flush()
        .with_context(|| format!("flushing events log at {}", path.display()))?;
    Ok(())
}

/// Appends transcript events as JSONL to `path`. Each event is one line,
/// flushed after the batch. Skips silently when `events` is empty. Mirrors
/// [`append_events`] for the `TranscriptEvent` stream.
pub fn append_transcript(path: &Path, events: &[TranscriptEvent]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening transcript log for append at {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    for e in events {
        serde_json::to_writer(&mut writer, e)
            .with_context(|| format!("serialising transcript event to {}", path.display()))?;
        writer
            .write_all(b"\n")
            .with_context(|| format!("appending newline to {}", path.display()))?;
    }
    writer
        .flush()
        .with_context(|| format!("flushing transcript log at {}", path.display()))?;
    Ok(())
}

/// Appends a single line (with newline) to `reflections.log`. Creates
/// the file if it does not exist.
pub fn append_log_line(path: &Path, line: &str) -> Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening reflections log at {}", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer
        .write_all(line.as_bytes())
        .with_context(|| format!("writing log line to {}", path.display()))?;
    if !line.ends_with('\n') {
        writer
            .write_all(b"\n")
            .with_context(|| format!("appending newline to {}", path.display()))?;
    }
    writer
        .flush()
        .with_context(|| format!("flushing reflections log at {}", path.display()))?;
    Ok(())
}

/// A lock is considered "fresh" (and thus active regardless of its PID) if
/// its file was touched within this window. Guards against `gc` racing a
/// `listen` that just started.
const LOCK_FRESH_WINDOW: Duration = Duration::from_secs(3600);

/// Writes `pid` to the session lockfile. `listen` calls this on start; the
/// lock is a plain text file containing the decimal PID.
pub fn write_lock(path: &Path, pid: u32) -> Result<()> {
    std::fs::write(path, pid.to_string())
        .with_context(|| format!("writing session lock at {}", path.display()))
}

/// Reads the PID from a session lockfile. Returns `Ok(None)` when the file
/// does not exist; errors only on unreadable-but-present or malformed files.
pub fn read_lock(path: &Path) -> Result<Option<u32>> {
    match std::fs::read_to_string(path) {
        Ok(body) => {
            let pid = body
                .trim()
                .parse::<u32>()
                .with_context(|| format!("parsing PID from session lock at {}", path.display()))?;
            Ok(Some(pid))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading session lock at {}", path.display())),
    }
}

/// Removes a session lockfile. A missing file is not an error — `listen`
/// calls this on clean shutdown, and a crash may have already lost it.
pub fn remove_lock(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("removing session lock at {}", path.display())),
    }
}

/// Whether `pid` is still a live process, probed with `kill(pid, 0)`.
///
/// `ESRCH` means the process is gone; `EPERM` means it exists but we may not
/// signal it (still alive). On non-Unix targets we cannot probe, so we
/// conservatively report "alive" to avoid deleting a possibly-live session.
#[cfg(unix)]
#[must_use]
pub fn pid_is_alive(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;

    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    !matches!(kill(Pid::from_raw(raw), None), Err(Errno::ESRCH))
}

#[cfg(not(unix))]
#[must_use]
pub fn pid_is_alive(_pid: u32) -> bool {
    true
}

/// Whether a session looks actively held, given the current time `now`.
///
/// True when `session.lock` exists and *either* it was touched within
/// [`LOCK_FRESH_WINDOW`] *or* its recorded PID is still alive. The union is
/// deliberately conservative: `gc` uses it to skip a session a live
/// `listen` is writing to. A stale lock — dead PID and older than the fresh
/// window — reports inactive, so a crashed session's leftover lock does not
/// block cleanup forever.
#[must_use]
pub fn lock_is_active(path: &Path, now: SystemTime) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false; // no lockfile → not held
    };
    if let Ok(modified) = meta.modified() {
        if now
            .duration_since(modified)
            .is_ok_and(|age| age < LOCK_FRESH_WINDOW)
        {
            return true;
        }
    }
    matches!(read_lock(path), Ok(Some(pid)) if pid_is_alive(pid))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::events::{
        EventKind, ItemClass, ItemCreate, Provenance, ReflectionId, TranscriptSpan,
    };
    use crate::voice::reflect::prompt;
    use crate::voice::transcriber::EndpointKind;
    use chrono::TimeZone;
    use tempfile::TempDir;

    fn fixed_ts() -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()
    }

    fn provenance() -> Provenance {
        Provenance {
            transcript_span: Some(TranscriptSpan {
                start_event_id: ulid::Ulid::from_parts(0, 1),
                end_event_id: ulid::Ulid::from_parts(0, 2),
            }),
            model: Some("m".into()),
            prompt_version: Some("p".into()),
        }
    }

    fn make_event(event_id: u128) -> Event {
        Event {
            event_id: ulid::Ulid::from_parts(0, event_id),
            ts: fixed_ts(),
            reflection_id: ReflectionId::Ulid(ulid::Ulid::from_parts(0, 100)),
            provenance: provenance(),
            kind: EventKind::ItemCreate(ItemCreate {
                item_id: ulid::Ulid::from_parts(0, 500),
                class: ItemClass::Todo,
                text: format!("event {event_id}"),
                priority: None,
                valid_until: None,
                tags: None,
            }),
        }
    }

    fn make_final(event_id: u128, text: &str) -> TranscriptEvent {
        TranscriptEvent::Final {
            event_id: ulid::Ulid::from_parts(0, event_id),
            text: text.to_string(),
            start: Duration::from_millis(0),
            end: Duration::from_millis(100),
            confidence: 0.9,
            words: None,
            speaker: None,
            revisable: false,
        }
    }

    #[test]
    fn open_or_create_bootstraps_an_empty_session() {
        let tmp = TempDir::new().unwrap();
        let session = open_or_create_under(tmp.path(), "s1").unwrap();
        assert!(session.paths.meta.exists());
        assert!(session.paths.transcript.exists());
        assert!(session.paths.events.exists());
        assert!(session.paths.log.exists());
        // Required identifying fields are populated from the id + mint time;
        // context fields start empty. Timestamps are live, so assert on the
        // stable fields rather than the whole struct.
        assert_eq!(session.meta.session_id, "s1");
        assert_eq!(session.meta.prompt_version, prompt::prompt_version());
        assert_eq!(session.meta.created, session.meta.last_modified);
        assert!(session.meta.backend.is_none());
        assert!(session.meta.model.is_none());
        assert!(session.meta.spent_usd.abs() < f64::EPSILON);
        assert!(session.meta.budget_cap_usd.is_none());
        assert!(session.meta.last_reflected_event_id.is_none());
        assert_eq!(session.meta.ttl_defaults, TtlDefaults::default());
    }

    #[test]
    fn open_or_create_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let s1 = open_or_create_under(tmp.path(), "s1").unwrap();
        let s2 = open_or_create_under(tmp.path(), "s1").unwrap();
        assert_eq!(s1.meta, s2.meta);
    }

    #[test]
    fn open_or_create_preserves_existing_meta() {
        let tmp = TempDir::new().unwrap();
        let mut s = open_or_create_under(tmp.path(), "s1").unwrap();
        s.set_last_reflected(ulid::Ulid::from_parts(0, 42)).unwrap();
        let reopened = open_or_create_under(tmp.path(), "s1").unwrap();
        assert_eq!(
            reopened.meta.last_reflected_event_id,
            Some(ulid::Ulid::from_parts(0, 42))
        );
    }

    #[test]
    fn rejects_session_id_with_path_separator() {
        let tmp = TempDir::new().unwrap();
        assert!(open_or_create_under(tmp.path(), "a/b").is_err());
        assert!(open_or_create_under(tmp.path(), "a\\b").is_err());
        assert!(open_or_create_under(tmp.path(), "..").is_err());
        assert!(open_or_create_under(tmp.path(), ".").is_err());
        assert!(open_or_create_under(tmp.path(), "").is_err());
    }

    #[test]
    fn ttl_defaults_match_799_defaults() {
        let t = TtlDefaults::default();
        assert_eq!(t.todo, Duration::from_secs(7 * 86_400));
        assert_eq!(t.research, Duration::from_secs(30 * 86_400));
        assert_eq!(t.question, Duration::from_secs(14 * 86_400));
        assert_eq!(t.decision, None);
    }

    #[test]
    fn meta_yaml_round_trips_full_schema() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("meta.yaml");
        let mut meta = SessionMeta::new("01HZX", fixed_ts(), "a3f2c891");
        meta.speaker = Some("jky".into());
        meta.project = Some("/Users/jky/wrk/foo".into());
        meta.backend = Some("whisper-rs".into());
        meta.model = Some("claude-opus-4-7".into());
        meta.budget_cap_usd = Some(5.0);
        meta.spent_usd = 0.0234;
        meta.ttl_defaults.decision = Some(Duration::from_secs(3 * 86_400));
        meta.last_reflected_event_id = Some(ulid::Ulid::from_parts(0, 7));
        write_meta(&path, &meta).unwrap();
        let back = read_meta(&path).unwrap();
        assert_eq!(meta, back);
    }

    #[test]
    fn meta_yaml_serializes_ttls_as_iso8601() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("meta.yaml");
        write_meta(&path, &SessionMeta::new("s", fixed_ts(), "p")).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.contains("todo: P7D"), "got:\n{raw}");
        assert!(raw.contains("research: P30D"), "got:\n{raw}");
        assert!(raw.contains("question: P14D"), "got:\n{raw}");
        assert!(raw.contains("decision: null"), "got:\n{raw}");
    }

    #[test]
    fn read_meta_bails_on_missing_required_field() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("meta.yaml");
        // Missing `session_id` — a strict read must refuse to auto-fill.
        std::fs::write(
            &path,
            "created: 2026-01-01T00:00:00Z\n\
             last_modified: 2026-01-01T00:00:00Z\n\
             prompt_version: abcd1234\n",
        )
        .unwrap();
        let err = read_meta(&path).unwrap_err();
        assert!(
            err.to_string().contains("session_id") || format!("{err:#}").contains("session_id"),
            "error should name the missing field: {err:#}"
        );
    }

    #[test]
    fn iso8601_round_trips_days_and_time() {
        assert_eq!(iso8601::to_string(Duration::from_secs(7 * 86_400)), "P7D");
        assert_eq!(iso8601::to_string(Duration::from_secs(0)), "PT0S");
        assert_eq!(
            iso8601::to_string(Duration::from_secs(86_400 + 3_661)),
            "P1DT1H1M1S"
        );
        assert_eq!(iso8601::parse("P7D").unwrap(), Duration::from_secs(604_800));
        assert_eq!(iso8601::parse("P1W").unwrap(), Duration::from_secs(604_800));
        assert_eq!(iso8601::parse("PT90M").unwrap(), Duration::from_secs(5_400));
        assert!(iso8601::parse("7D").is_err(), "must require leading P");
        assert!(iso8601::parse("P7X").is_err(), "unknown unit rejected");
    }

    #[test]
    fn append_then_read_events_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("events.jsonl");
        append_events(&path, &[make_event(1), make_event(2)]).unwrap();
        let back = read_events(&path).unwrap();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0], make_event(1));
        assert_eq!(back[1], make_event(2));
    }

    #[test]
    fn append_events_with_empty_slice_is_noop() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("events.jsonl");
        append_events(&path, &[]).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn read_events_on_missing_file_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let result = read_events(&tmp.path().join("nothing.jsonl")).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn read_transcript_finals_after_filters_partials_and_endpoints() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n",
                serde_json::to_string(&TranscriptEvent::Partial {
                    text: "ignored".into(),
                    start: Duration::ZERO,
                    end: Duration::from_millis(50),
                    words: None,
                    speaker: None,
                })
                .unwrap(),
                serde_json::to_string(&make_final(1, "first")).unwrap(),
                serde_json::to_string(&TranscriptEvent::Endpoint {
                    at: Duration::from_secs(1),
                    kind: EndpointKind::StreamEnd,
                })
                .unwrap(),
            ),
        )
        .unwrap();
        let finals = read_transcript_finals_after(&path, None).unwrap();
        assert_eq!(finals.len(), 1);
    }

    #[test]
    fn read_transcript_finals_after_skips_through_marker() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        let lines = [
            serde_json::to_string(&make_final(1, "a")).unwrap(),
            serde_json::to_string(&make_final(2, "b")).unwrap(),
            serde_json::to_string(&make_final(3, "c")).unwrap(),
        ];
        std::fs::write(&path, lines.join("\n")).unwrap();
        let after_id = ulid::Ulid::from_parts(0, 2);
        let finals = read_transcript_finals_after(&path, Some(after_id)).unwrap();
        assert_eq!(finals.len(), 1);
        match &finals[0] {
            TranscriptEvent::Final { text, .. } => assert_eq!(text, "c"),
            other => panic!("expected Final, got {other:?}"),
        }
    }

    #[test]
    fn read_transcript_finals_after_errors_when_marker_missing() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        std::fs::write(
            &path,
            serde_json::to_string(&make_final(1, "a")).unwrap() + "\n",
        )
        .unwrap();
        let err =
            read_transcript_finals_after(&path, Some(ulid::Ulid::from_parts(0, 99))).unwrap_err();
        assert!(
            err.to_string().contains("not found in transcript"),
            "got: {err}"
        );
    }

    #[test]
    fn append_log_line_creates_file_and_adds_newline() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("reflections.log");
        append_log_line(&path, "first entry").unwrap();
        append_log_line(&path, "second entry\n").unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, "first entry\nsecond entry\n");
    }

    #[test]
    fn append_transcript_empty_slice_is_noop() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        append_transcript(&path, &[]).unwrap();
        assert!(!path.exists(), "empty append should not create the file");
    }

    #[test]
    fn append_then_read_transcript_round_trips() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        append_transcript(&path, &[make_final(1, "a")]).unwrap();
        append_transcript(&path, &[make_final(2, "b")]).unwrap();
        let back = read_transcript(&path).unwrap();
        assert_eq!(back.len(), 2);
        match (&back[0], &back[1]) {
            (TranscriptEvent::Final { text: t0, .. }, TranscriptEvent::Final { text: t1, .. }) => {
                assert_eq!(t0, "a");
                assert_eq!(t1, "b");
            }
            other => panic!("expected two Finals, got {other:?}"),
        }
    }

    #[test]
    fn read_transcript_skips_blank_lines() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        std::fs::write(
            &path,
            format!(
                "\n{}\n\n   \n{}\n",
                serde_json::to_string(&make_final(1, "a")).unwrap(),
                serde_json::to_string(&make_final(2, "b")).unwrap(),
            ),
        )
        .unwrap();
        let events = read_transcript(&path).unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn read_transcript_reports_parse_failure_with_line_number() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("transcript.jsonl");
        let good = serde_json::to_string(&make_final(1, "ok")).unwrap();
        std::fs::write(&path, format!("{good}\nnot valid json\n")).unwrap();
        let err = read_transcript(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("parsing") && msg.contains(":2"),
            "error should point at line 2: {msg}"
        );
    }

    #[test]
    fn read_events_skips_blank_lines() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("events.jsonl");
        append_events(&path, &[make_event(1)]).unwrap();
        // Add a blank line after the existing content.
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        writeln!(f, "\n  ").unwrap();
        drop(f);
        let events = read_events(&path).unwrap();
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn read_events_reports_parse_failure_with_line_number() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("events.jsonl");
        std::fs::write(&path, "not valid json at all\n").unwrap();
        let err = read_events(&path).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("parsing") && msg.contains(":1"),
            "error should point at line 1: {msg}"
        );
    }

    #[test]
    fn voice_root_respects_override_env_var() {
        // Env mutation is process-wide, and `OMNI_VOICE_VOICE_ROOT` is read
        // by the session-root resolver exercised in `review`/`reflect` tests
        // too — so serialise on the crate-wide env lock and restore on exit
        // (issue #12).
        let _env = crate::test_support::env::env_lock();
        let original = std::env::var("OMNI_VOICE_VOICE_ROOT").ok();
        std::env::set_var("OMNI_VOICE_VOICE_ROOT", "/tmp/overridden");
        let root = voice_root().unwrap();
        assert_eq!(root, PathBuf::from("/tmp/overridden"));
        match original {
            Some(v) => std::env::set_var("OMNI_VOICE_VOICE_ROOT", v),
            None => std::env::remove_var("OMNI_VOICE_VOICE_ROOT"),
        }
    }

    #[test]
    fn lock_round_trips_pid_and_removes() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.lock");
        assert_eq!(read_lock(&path).unwrap(), None);
        write_lock(&path, 4242).unwrap();
        assert_eq!(read_lock(&path).unwrap(), Some(4242));
        remove_lock(&path).unwrap();
        assert!(!path.exists());
        // Removing an already-absent lock is a no-op, not an error.
        remove_lock(&path).unwrap();
    }

    #[test]
    fn own_pid_is_alive() {
        assert!(pid_is_alive(std::process::id()));
    }

    #[test]
    fn pid_is_alive_rejects_out_of_range() {
        // A value above i32::MAX cannot be a real process id.
        assert!(!pid_is_alive(u32::MAX));
    }

    #[test]
    fn read_lock_errors_on_unreadable_lock() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.lock");
        std::fs::create_dir(&path).unwrap(); // a directory where a file is expected
        assert!(read_lock(&path).is_err());
    }

    #[test]
    fn lock_absent_is_inactive() {
        let tmp = TempDir::new().unwrap();
        assert!(!lock_is_active(
            &tmp.path().join("session.lock"),
            SystemTime::now()
        ));
    }

    #[test]
    fn fresh_lock_is_active_regardless_of_pid() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.lock");
        // A very high, almost-certainly-dead PID — freshness alone keeps it active.
        write_lock(&path, 4_000_000).unwrap();
        assert!(lock_is_active(&path, SystemTime::now()));
    }

    #[test]
    fn stale_lock_with_dead_pid_is_inactive() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.lock");
        write_lock(&path, 4_000_000).unwrap();
        // Evaluate as if it were well past the freshness window.
        let future = SystemTime::now() + LOCK_FRESH_WINDOW + Duration::from_secs(60);
        assert!(!lock_is_active(&path, future));
    }

    #[test]
    fn stale_lock_with_live_pid_is_active() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("session.lock");
        write_lock(&path, std::process::id()).unwrap();
        let future = SystemTime::now() + LOCK_FRESH_WINDOW + Duration::from_secs(60);
        assert!(lock_is_active(&path, future));
    }
}

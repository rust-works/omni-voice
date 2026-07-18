//! Live speaker gating **and labelling** for `voice listen`.
//!
//! `voice listen` streams audio into the ASR backend, which emits text-only
//! `Final` events — the PCM is gone from the scheduler's view by the time a
//! `Final` arrives. To decide speaker identity we reconstruct each segment's
//! audio the same way batch `transcribe --speaker` does
//! ([`SpeakerFilter`](crate::cli::voice::transcribe)): retain a rolling window
//! of recent PCM and slice it by the `Final`'s `start`/`end` timestamps (a fixed
//! 16 kHz mono i16 contract), embed the slice **once**, and cosine it against
//! the enrolled speaker(s).
//!
//! [`SpeakerGate`] runs in one of two modes ([`GateMode`]):
//!
//! - **Gate** (`--speaker <name>`, one enrolment) — keep the `Final` only when
//!   its cosine to the enrolled speaker clears the threshold; drop everyone
//!   else. Answers *"transcribe only me."*
//! - **Label** (`--speaker a --speaker b`, or `--label` over all enrolments) —
//!   tag the `Final` with the *nearest* enrolled speaker above the threshold; a
//!   below-threshold or too-short segment follows the [`UnknownPolicy`]
//!   (keep-as-`unknown` or drop). Answers *"who said what."*
//!
//! The rolling window is a [`PcmRing`] filled by a [`TeeAudioInput`] that wraps
//! the audio source on the **consumer** side of the capture channel — *after*
//! its drop-on-overflow point — so the ring stays sample-aligned with what the
//! backend actually consumed. The gate shares the ring with the tee and runs
//! the per-`Final` decision inside
//! [`ListenScheduler`](super::scheduler::ListenScheduler).
//!
//! Deciding is **per-segment (per `Final`)**, not per-frame. Any
//! *infrastructure* failure — segment audio already scrolled out of the ring,
//! an embedding error, or no dimension-compatible enrolment — fails **open**
//! (keeps the segment and warns) so a gate hiccup never silently swallows the
//! user's own speech.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use tracing::warn;

use crate::voice::models::SPEAKER_WESPEAKER_EN;
use crate::voice::transcriber::{AsyncAudioInput, AudioChunk, SpeakerId};
use crate::voice::{
    cosine, load_all_enrolled, speaker_file, EnrolledSpeaker, WespeakerEmbedder, MIN_EMBED_SAMPLES,
};

/// The fixed 16 kHz sample rate of the streaming seam, as an `f64` for
/// timestamp→sample-index arithmetic.
const SAMPLE_RATE: f64 = 16_000.0;

/// How much recent audio the ring retains (30 s ≈ 960 KB of i16). A `Final`'s
/// segment is sliced out by timestamp; at live cadence it is always well within
/// this window, and the cap bounds memory even in a multi-hour session.
const RING_CAPACITY_SAMPLES: usize = 30 * 16_000;

/// A bounded rolling buffer of the most-recent 16 kHz mono i16 samples, indexed
/// by absolute sample position since the stream began.
///
/// The absolute indexing is what lets [`SpeakerGate::decide`] map a `Final`'s
/// `start`/`end` seconds onto retained samples without threading any offset
/// state through the pipeline.
pub struct PcmRing {
    buf: VecDeque<i16>,
    capacity: usize,
    /// Total samples ever pushed (monotonic). `base = pushed - buf.len()` is the
    /// absolute index of `buf.front()`.
    pushed: u64,
}

impl PcmRing {
    /// Builds an empty ring retaining at most `capacity` (clamped to ≥ 1)
    /// samples.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
            pushed: 0,
        }
    }

    /// Appends `samples`, evicting the oldest once capacity is exceeded.
    pub fn push(&mut self, samples: &[i16]) {
        self.buf.extend(samples.iter().copied());
        self.pushed = self.pushed.saturating_add(samples.len() as u64);
        while self.buf.len() > self.capacity {
            self.buf.pop_front();
        }
    }

    /// Absolute index of the oldest sample still retained.
    fn base(&self) -> u64 {
        self.pushed - self.buf.len() as u64
    }

    /// Copies out samples in the absolute range `[start_idx, end_idx)`.
    ///
    /// Returns `None` when the window has already scrolled out of the ring (its
    /// start is older than the oldest retained sample) — the caller treats that
    /// as "can't verify, fail open". `end_idx` is clamped to what has been
    /// pushed so far; an empty or inverted range yields an empty `Vec`.
    #[must_use]
    pub fn slice(&self, start_idx: u64, end_idx: u64) -> Option<Vec<i16>> {
        let base = self.base();
        if start_idx < base {
            return None;
        }
        let end_idx = end_idx.min(self.pushed);
        if end_idx <= start_idx {
            return Some(Vec::new());
        }
        let lo = (start_idx - base) as usize;
        let hi = (end_idx - base) as usize;
        Some(self.buf.iter().copied().skip(lo).take(hi - lo).collect())
    }
}

/// Embeds a 16 kHz mono i16 window into a speaker vector.
///
/// A trait so [`SpeakerGate`] owns the embedder behind a boxed `dyn`, letting
/// production inject the real [`WespeakerEmbedder`] and tests inject a
/// deterministic stub — the gate's ring/threshold/fail-open logic is then
/// exercisable without the ONNX model on disk. `Send + Sync` because the gate
/// lives in the `listen` scheduler's `Send` future.
pub trait SpeakerEmbedder: Send + Sync {
    /// Embeds `pcm` (16 kHz mono i16) into a speaker vector, or errors.
    fn embed(&self, pcm: &[i16]) -> Result<Vec<f32>>;
}

impl SpeakerEmbedder for WespeakerEmbedder {
    fn embed(&self, pcm: &[i16]) -> Result<Vec<f32>> {
        // `Self::embed` resolves to the inherent method (inherent items take
        // path-precedence over the same-named trait method), so this forwards
        // rather than recursing.
        Self::embed(self, pcm)
    }
}

/// How a labelling session treats a segment that matches no enrolled speaker
/// above the threshold (or is too short to embed).
///
/// Only meaningful in [`GateMode::Label`]; the single-speaker gate always drops
/// non-matches. Surfaced as the `--unknown-policy` CLI flag.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum UnknownPolicy {
    /// Keep the segment, tagging it `speaker: "unknown"` (still transcribed).
    #[default]
    Keep,
    /// Drop the segment, as the single-speaker gate does.
    Drop,
}

/// The literal speaker tag stamped on kept-but-unattributable segments under
/// [`UnknownPolicy::Keep`].
const UNKNOWN_SPEAKER: &str = "unknown";

/// Which decision policy [`SpeakerGate::decide`] applies.
#[derive(Clone, Copy, Debug)]
pub enum GateMode {
    /// Single-speaker gate (`--speaker <name>`, Approach 1): keep only the one
    /// enrolled speaker, drop everyone else. `enrolled` holds exactly one.
    Gate,
    /// N-way labeller (`--speaker a --speaker b` / `--label`, Approach 2): tag
    /// each kept segment with the nearest enrolled speaker.
    Label {
        /// How to treat a segment matching no enrolled speaker above threshold.
        unknown_policy: UnknownPolicy,
    },
}

/// The outcome of [`SpeakerGate::decide`] for one `Final` segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateDecision {
    /// Keep the segment, stamping this speaker tag: `Some(name)` on a confident
    /// match, `Some("unknown")` under [`UnknownPolicy::Keep`], or `None` when
    /// the gate can't say (fail-open in label mode) — the caller then falls
    /// back to any backend-provided tag.
    Keep(Option<SpeakerId>),
    /// Drop the segment entirely (it never reaches `transcript.jsonl`).
    Drop,
}

/// Per-`Final` speaker gate / labeller.
///
/// Reconstructs a segment's audio from the shared [`PcmRing`], embeds it once,
/// and cosines it against the enrolled speaker(s) to keep/drop (gate) or tag
/// (label) the segment.
///
/// Load it once at startup with [`SpeakerGate::gate`] or
/// [`SpeakerGate::labeller`] (fail-fast on missing enrolment or model), hand its
/// [`ring`](SpeakerGate::ring) to a [`TeeAudioInput`], and call
/// [`decide`](SpeakerGate::decide) on each `Final`.
pub struct SpeakerGate {
    /// Enrolled speakers to score against — exactly one in [`GateMode::Gate`],
    /// one or more in [`GateMode::Label`]. Never empty.
    enrolled: Vec<EnrolledSpeaker>,
    embedder: Box<dyn SpeakerEmbedder>,
    threshold: f32,
    mode: GateMode,
    ring: Arc<Mutex<PcmRing>>,
}

impl SpeakerGate {
    /// Single-speaker gate for `--speaker <name>` (Approach 1): loads the one
    /// enrolment and the wespeaker embedder, failing fast (before capture) if
    /// either is missing. Non-matching segments are dropped.
    pub fn gate(name: &str, speaker_model: Option<&Path>, threshold: f32) -> Result<Self> {
        let enrolled = load_enrolled(name)?;
        let embedder = load_embedder(speaker_model)?;
        Ok(Self::from_parts(
            vec![enrolled],
            embedder,
            threshold,
            GateMode::Gate,
        ))
    }

    /// N-way labeller (Approach 2): loads the enrolled set and the wespeaker
    /// embedder, then tags each `Final` with the nearest speaker.
    ///
    /// `names` non-empty selects that subset (`--speaker a --speaker b`); an
    /// empty `names` loads *all* enrolments (`--label`). Fails fast if a named
    /// speaker is missing, the model is missing, or no enrolments exist at all.
    pub fn labeller(
        names: &[String],
        speaker_model: Option<&Path>,
        threshold: f32,
        unknown_policy: UnknownPolicy,
    ) -> Result<Self> {
        let enrolled = if names.is_empty() {
            let all = load_all_enrolled()?;
            if all.is_empty() {
                bail!(
                    "no enrolled speakers found; run `omni-voice enroll --name <name>` \
                     before `--label`"
                );
            }
            all
        } else {
            names
                .iter()
                .map(|name| load_enrolled(name))
                .collect::<Result<Vec<_>>>()?
        };
        let embedder = load_embedder(speaker_model)?;
        Ok(Self::from_parts(
            enrolled,
            embedder,
            threshold,
            GateMode::Label { unknown_policy },
        ))
    }

    /// Assembles a gate from already-loaded parts, minting a fresh ring. The
    /// seam the [`gate`](SpeakerGate::gate) / [`labeller`](SpeakerGate::labeller)
    /// constructors build on; tests inject a real [`WespeakerEmbedder`]
    /// (end-to-end) or a stub (decision logic) without going through disk
    /// resolution. `enrolled` must be non-empty.
    #[must_use]
    pub fn from_parts<E: SpeakerEmbedder + 'static>(
        enrolled: Vec<EnrolledSpeaker>,
        embedder: E,
        threshold: f32,
        mode: GateMode,
    ) -> Self {
        Self {
            enrolled,
            embedder: Box::new(embedder),
            threshold,
            mode,
            ring: Arc::new(Mutex::new(PcmRing::new(RING_CAPACITY_SAMPLES))),
        }
    }

    /// A handle to the shared ring, for the [`TeeAudioInput`] that fills it.
    #[must_use]
    pub fn ring(&self) -> Arc<Mutex<PcmRing>> {
        Arc::clone(&self.ring)
    }

    /// Decides how to treat the `Final` spanning `[start, end)`: keep it with a
    /// speaker tag, or drop it.
    ///
    /// Embeds the segment once and cosines it against every enrolled speaker.
    /// In [`GateMode::Gate`] the sole enrolled speaker is kept above threshold
    /// and dropped below (matching batch `transcribe --speaker`). In
    /// [`GateMode::Label`] the nearest speaker above threshold is stamped, and a
    /// below-threshold or too-short segment follows the [`UnknownPolicy`].
    ///
    /// Fails **open** on any infrastructure failure — audio scrolled out of the
    /// ring, an embedding error, or no dimension-compatible enrolment — so a
    /// gate hiccup never silently swallows the user's own speech (keeping with
    /// the enrolled name in gate mode, or unattributed in label mode).
    #[must_use]
    pub fn decide(&self, start: Duration, end: Duration) -> GateDecision {
        let start_idx = (start.as_secs_f64() * SAMPLE_RATE) as u64;
        let end_idx = (end.as_secs_f64() * SAMPLE_RATE) as u64;

        let window = self
            .ring
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .slice(start_idx, end_idx);
        let Some(window) = window else {
            warn!(
                ?start,
                ?end,
                "speaker gate: segment audio scrolled out of the ring; keeping (fail-open)"
            );
            return GateDecision::Keep(self.fail_open_label());
        };
        if window.len() < MIN_EMBED_SAMPLES {
            // Too short for a stable embedding; can't attribute it.
            return self.no_match_decision();
        }
        let emb = match self.embedder.embed(&window) {
            Ok(v) => v,
            Err(e) => {
                warn!("speaker gate: embedding failed ({e:#}); keeping (fail-open)");
                return GateDecision::Keep(self.fail_open_label());
            }
        };
        // Arg-max cosine over the enrolled speakers whose vector dimension
        // matches the embedding — a mismatch would panic `cosine`, so skip
        // those. Ties resolve to the last speaker in enrolment order.
        let best = self
            .enrolled
            .iter()
            .filter(|s| s.vector.len() == emb.len())
            .map(|s| (s, cosine(&emb, &s.vector)))
            .max_by(|(_, a), (_, b)| a.total_cmp(b));
        let Some((speaker, score)) = best else {
            warn!(
                emb = emb.len(),
                "speaker gate: no dimension-compatible enrolment; keeping (fail-open)"
            );
            return GateDecision::Keep(self.fail_open_label());
        };
        if score >= self.threshold {
            GateDecision::Keep(Some(speaker.name.clone()))
        } else {
            self.no_match_decision()
        }
    }

    /// The keep-tag used when the gate can't verify the speaker (fail-open): the
    /// sole enrolled name in gate mode (unchanged #5 behaviour — a fail-open
    /// keep is still attributed to the enrolled speaker), or `None` in label
    /// mode (we genuinely can't say who, so defer to any backend tag).
    fn fail_open_label(&self) -> Option<SpeakerId> {
        match self.mode {
            GateMode::Gate => self.enrolled.first().map(|s| s.name.clone()),
            GateMode::Label { .. } => None,
        }
    }

    /// The decision for a segment matching no enrolled speaker above the
    /// threshold (or too short to embed): drop in gate mode; in label mode,
    /// keep-as-`unknown` or drop per the [`UnknownPolicy`].
    fn no_match_decision(&self) -> GateDecision {
        match self.mode {
            GateMode::Gate
            | GateMode::Label {
                unknown_policy: UnknownPolicy::Drop,
            } => GateDecision::Drop,
            GateMode::Label {
                unknown_policy: UnknownPolicy::Keep,
            } => GateDecision::Keep(Some(UNKNOWN_SPEAKER.to_string())),
        }
    }
}

/// Loads one enrolment by `name`, with an actionable error when it's missing.
fn load_enrolled(name: &str) -> Result<EnrolledSpeaker> {
    let path = speaker_file(name)?;
    EnrolledSpeaker::load(&path).with_context(|| {
        format!(
            "load enrolled speaker {name} from {}; run `omni-voice enroll --name {name}` first",
            path.display()
        )
    })
}

/// Resolves and loads the wespeaker embedder, honouring a `--speaker-model`
/// override. Shared by both [`SpeakerGate`] constructors.
fn load_embedder(speaker_model: Option<&Path>) -> Result<WespeakerEmbedder> {
    let dir = SPEAKER_WESPEAKER_EN.resolve_dir(speaker_model)?;
    SPEAKER_WESPEAKER_EN.ensure_present(&dir)?;
    let model_path = dir.join(SPEAKER_WESPEAKER_EN.required_files[0]);
    WespeakerEmbedder::new(&model_path)
}

/// Wraps an [`AsyncAudioInput`] and mirrors every chunk it yields into a shared
/// [`PcmRing`], so a [`SpeakerGate`] can reconstruct segment audio later.
///
/// Sits on the **consumer** side of the capture channel (after its
/// drop-on-overflow point) so the ring stays sample-aligned with exactly what
/// the backend consumed.
pub struct TeeAudioInput {
    inner: Box<dyn AsyncAudioInput>,
    ring: Arc<Mutex<PcmRing>>,
}

impl TeeAudioInput {
    /// Wraps `inner`, teeing each yielded chunk into `ring`.
    #[must_use]
    pub fn new(inner: Box<dyn AsyncAudioInput>, ring: Arc<Mutex<PcmRing>>) -> Self {
        Self { inner, ring }
    }
}

#[async_trait]
impl AsyncAudioInput for TeeAudioInput {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        let chunk = self.inner.next_chunk().await;
        if let Some(samples) = chunk.as_deref() {
            // Lock is taken after the await and dropped before returning — it is
            // never held across a suspension point.
            self.ring
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(samples);
        }
        chunk
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn ring_slice_round_trips_absolute_indices() {
        let mut ring = PcmRing::new(100);
        let samples: Vec<i16> = (0..40).collect();
        ring.push(&samples);
        // Whole span.
        assert_eq!(ring.slice(0, 40), Some(samples.clone()));
        // Sub-span [10, 20).
        assert_eq!(ring.slice(10, 20), Some((10..20).collect::<Vec<i16>>()));
    }

    #[test]
    fn ring_push_is_cumulative_across_calls() {
        let mut ring = PcmRing::new(100);
        ring.push(&(0..10).collect::<Vec<i16>>());
        ring.push(&(10..20).collect::<Vec<i16>>());
        // Second chunk lives at absolute indices [10, 20).
        assert_eq!(ring.slice(10, 20), Some((10..20).collect::<Vec<i16>>()));
    }

    #[test]
    fn ring_evicts_oldest_beyond_capacity() {
        let mut ring = PcmRing::new(8);
        ring.push(&(0..12).collect::<Vec<i16>>());
        // Only the last 8 samples (indices 4..12) are retained.
        assert_eq!(ring.slice(0, 4), None, "evicted span reports scrolled-out");
        assert_eq!(ring.slice(4, 12), Some((4..12).collect::<Vec<i16>>()));
    }

    #[test]
    fn ring_reports_scrolled_out_window_as_none() {
        let mut ring = PcmRing::new(4);
        ring.push(&(0..10).collect::<Vec<i16>>()); // retains 6..10
        assert_eq!(ring.slice(0, 3), None);
        assert_eq!(ring.slice(5, 9), None, "start still below base");
    }

    #[test]
    fn ring_clamps_end_beyond_pushed() {
        let mut ring = PcmRing::new(100);
        ring.push(&(0..10).collect::<Vec<i16>>());
        // end past what's been pushed clamps to the available tail.
        assert_eq!(ring.slice(5, 999), Some((5..10).collect::<Vec<i16>>()));
    }

    #[test]
    fn ring_inverted_or_empty_range_is_empty() {
        let mut ring = PcmRing::new(100);
        ring.push(&(0..10).collect::<Vec<i16>>());
        assert_eq!(ring.slice(5, 5), Some(Vec::new()));
        assert_eq!(ring.slice(8, 4), Some(Vec::new()));
    }

    #[tokio::test]
    async fn tee_mirrors_chunks_into_ring() {
        use crate::voice::transcriber::FileAsyncAudioInput;

        let samples: Vec<i16> = (0..3200).map(|i| (i % 7) as i16).collect();
        let inner = FileAsyncAudioInput::from_samples(samples.clone(), 800, false);
        let ring = Arc::new(Mutex::new(PcmRing::new(RING_CAPACITY_SAMPLES)));
        let mut tee = TeeAudioInput::new(Box::new(inner), Arc::clone(&ring));

        // Drain the tee; every chunk should also land in the ring.
        while tee.next_chunk().await.is_some() {}

        let mirrored = ring.lock().unwrap().slice(0, 3200).unwrap();
        assert_eq!(mirrored, samples);
    }

    /// Deterministic embedder for unit-testing the gate decision without the
    /// ONNX model: returns `vector` (cloned), or an error when `None`.
    struct StubEmbedder {
        vector: Option<Vec<f32>>,
    }

    impl SpeakerEmbedder for StubEmbedder {
        fn embed(&self, _pcm: &[i16]) -> Result<Vec<f32>> {
            self.vector
                .clone()
                .ok_or_else(|| anyhow::anyhow!("stub embed failure"))
        }
    }

    /// Builds an `EnrolledSpeaker` from a name and vector (dim inferred).
    fn enrolled_speaker(name: &str, vector: Vec<f32>) -> EnrolledSpeaker {
        EnrolledSpeaker {
            name: name.to_string(),
            model: "stub".to_string(),
            dim: vector.len(),
            vector,
            samples_used: 1,
            enrolled_at: chrono::Utc::now(),
        }
    }

    /// A single-speaker **gate** enrolled on `enrolled`, whose embedder returns
    /// `stub_out` for every window, at cosine `threshold`.
    fn stub_gate(enrolled: Vec<f32>, stub_out: Option<Vec<f32>>, threshold: f32) -> SpeakerGate {
        SpeakerGate::from_parts(
            vec![enrolled_speaker("me", enrolled)],
            StubEmbedder { vector: stub_out },
            threshold,
            GateMode::Gate,
        )
    }

    /// An N-way **labeller** over `enrolled` (name → vector), whose embedder
    /// returns `stub_out` for every window, at cosine `threshold`.
    fn stub_labeller(
        enrolled: Vec<(&str, Vec<f32>)>,
        stub_out: Option<Vec<f32>>,
        threshold: f32,
        unknown_policy: UnknownPolicy,
    ) -> SpeakerGate {
        let enrolled = enrolled
            .into_iter()
            .map(|(name, v)| enrolled_speaker(name, v))
            .collect();
        SpeakerGate::from_parts(
            enrolled,
            StubEmbedder { vector: stub_out },
            threshold,
            GateMode::Label { unknown_policy },
        )
    }

    /// Pushes `n` silent samples into the gate's ring.
    fn fill(gate: &SpeakerGate, n: usize) {
        gate.ring().lock().unwrap().push(&vec![0_i16; n]);
    }

    fn secs(t: f64) -> Duration {
        Duration::from_secs_f64(t)
    }

    fn keep(name: &str) -> GateDecision {
        GateDecision::Keep(Some(name.to_string()))
    }

    // ── Gate mode: preserves the #5 single-speaker keep/drop behaviour ──────

    #[test]
    fn gate_keeps_on_cosine_match() {
        let gate = stub_gate(vec![1.0, 0.0], Some(vec![1.0, 0.0]), 0.5);
        fill(&gate, MIN_EMBED_SAMPLES);
        assert_eq!(gate.decide(secs(0.0), secs(0.5)), keep("me"));
    }

    #[test]
    fn gate_drops_on_cosine_mismatch() {
        let gate = stub_gate(vec![1.0, 0.0], Some(vec![0.0, 1.0]), 0.5);
        fill(&gate, MIN_EMBED_SAMPLES);
        assert_eq!(gate.decide(secs(0.0), secs(0.5)), GateDecision::Drop);
    }

    #[test]
    fn gate_drops_segment_too_short_to_embed() {
        // stub_out would match, so a drop here is purely the length guard.
        let gate = stub_gate(vec![1.0, 0.0], Some(vec![1.0, 0.0]), 0.5);
        fill(&gate, 100); // < MIN_EMBED_SAMPLES
        assert_eq!(gate.decide(secs(0.0), secs(0.5)), GateDecision::Drop);
    }

    #[test]
    fn gate_fails_open_when_window_scrolled_out() {
        // Mismatching stub, so a keep proves fail-open (not a real match); the
        // gate still stamps the enrolled name, as #5 did.
        let gate = stub_gate(vec![1.0, 0.0], Some(vec![0.0, 1.0]), 0.5);
        fill(&gate, RING_CAPACITY_SAMPLES + MIN_EMBED_SAMPLES); // evicts index 0
        assert_eq!(gate.decide(secs(0.0), secs(0.5)), keep("me"));
    }

    #[test]
    fn gate_fails_open_on_embed_error() {
        let gate = stub_gate(vec![1.0, 0.0], None, 0.5); // embedder errors
        fill(&gate, MIN_EMBED_SAMPLES);
        assert_eq!(gate.decide(secs(0.0), secs(0.5)), keep("me"));
    }

    #[test]
    fn gate_fails_open_on_dim_mismatch() {
        // 3-dim embedding vs 2-dim enrolled: no dimension-compatible enrolment,
        // so it fails open rather than panicking in cosine.
        let gate = stub_gate(vec![1.0, 0.0], Some(vec![1.0, 0.0, 0.0]), 0.5);
        fill(&gate, MIN_EMBED_SAMPLES);
        assert_eq!(gate.decide(secs(0.0), secs(0.5)), keep("me"));
    }

    // ── Label mode: nearest-of-N tagging ────────────────────────────────────

    #[test]
    fn label_picks_nearest_enrolled_speaker() {
        let enrolled = vec![("a", vec![1.0, 0.0]), ("b", vec![0.0, 1.0])];
        // Closest to A.
        let gate = stub_labeller(
            enrolled.clone(),
            Some(vec![0.9, 0.1]),
            0.5,
            UnknownPolicy::Keep,
        );
        fill(&gate, MIN_EMBED_SAMPLES);
        assert_eq!(gate.decide(secs(0.0), secs(0.5)), keep("a"));
        // Closest to B.
        let gate = stub_labeller(enrolled, Some(vec![0.1, 0.9]), 0.5, UnknownPolicy::Keep);
        fill(&gate, MIN_EMBED_SAMPLES);
        assert_eq!(gate.decide(secs(0.0), secs(0.5)), keep("b"));
    }

    #[test]
    fn label_below_threshold_keeps_unknown() {
        let enrolled = vec![("a", vec![1.0, 0.0]), ("b", vec![0.0, 1.0])];
        // Both cosines (0.3) fall below the 0.5 threshold.
        let gate = stub_labeller(enrolled, Some(vec![0.3, 0.3]), 0.5, UnknownPolicy::Keep);
        fill(&gate, MIN_EMBED_SAMPLES);
        assert_eq!(gate.decide(secs(0.0), secs(0.5)), keep(UNKNOWN_SPEAKER));
    }

    #[test]
    fn label_below_threshold_drops_under_drop_policy() {
        let enrolled = vec![("a", vec![1.0, 0.0]), ("b", vec![0.0, 1.0])];
        let gate = stub_labeller(enrolled, Some(vec![0.3, 0.3]), 0.5, UnknownPolicy::Drop);
        fill(&gate, MIN_EMBED_SAMPLES);
        assert_eq!(gate.decide(secs(0.0), secs(0.5)), GateDecision::Drop);
    }

    #[test]
    fn label_too_short_follows_unknown_policy() {
        // stub_out would match A, so the outcome is purely the length guard.
        let keep_gate = stub_labeller(
            vec![("a", vec![1.0, 0.0])],
            Some(vec![1.0, 0.0]),
            0.5,
            UnknownPolicy::Keep,
        );
        fill(&keep_gate, 100); // < MIN_EMBED_SAMPLES
        assert_eq!(
            keep_gate.decide(secs(0.0), secs(0.5)),
            keep(UNKNOWN_SPEAKER)
        );

        let drop_gate = stub_labeller(
            vec![("a", vec![1.0, 0.0])],
            Some(vec![1.0, 0.0]),
            0.5,
            UnknownPolicy::Drop,
        );
        fill(&drop_gate, 100);
        assert_eq!(drop_gate.decide(secs(0.0), secs(0.5)), GateDecision::Drop);
    }

    #[test]
    fn label_fails_open_with_none_when_scrolled_out() {
        // Label-mode fail-open leaves the segment unattributed (None), not a
        // misattributed name.
        let gate = stub_labeller(
            vec![("a", vec![1.0, 0.0])],
            Some(vec![1.0, 0.0]),
            0.5,
            UnknownPolicy::Keep,
        );
        fill(&gate, RING_CAPACITY_SAMPLES + MIN_EMBED_SAMPLES); // evicts index 0
        assert_eq!(gate.decide(secs(0.0), secs(0.5)), GateDecision::Keep(None));
    }

    #[test]
    fn label_skips_dim_mismatched_enrolment() {
        // One enrolment has an incompatible 3-dim vector; the labeller must skip
        // it (not panic in cosine) and pick the dimension-compatible match.
        let gate = stub_labeller(
            vec![("bad", vec![1.0, 0.0, 0.0]), ("good", vec![1.0, 0.0])],
            Some(vec![1.0, 0.0]),
            0.5,
            UnknownPolicy::Keep,
        );
        fill(&gate, MIN_EMBED_SAMPLES);
        assert_eq!(gate.decide(secs(0.0), secs(0.5)), keep("good"));
    }
}

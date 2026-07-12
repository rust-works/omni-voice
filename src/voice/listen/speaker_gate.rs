//! Live speaker gating for `voice listen`.
//!
//! `voice listen` streams audio into the ASR backend, which emits text-only
//! `Final` events — the PCM is gone from the scheduler's view by the time a
//! `Final` arrives. To gate on speaker identity we reconstruct each segment's
//! audio the same way batch `transcribe --speaker` does
//! ([`SpeakerFilter`](crate::cli::voice::transcribe)): retain a rolling window
//! of recent PCM and slice it by the `Final`'s `start`/`end` timestamps (a fixed
//! 16 kHz mono i16 contract), embed the slice, and keep the `Final` only when
//! its cosine similarity to the enrolled speaker clears the threshold.
//!
//! The rolling window is a [`PcmRing`] filled by a [`TeeAudioInput`] that wraps
//! the audio source on the **consumer** side of the capture channel — *after*
//! its drop-on-overflow point — so the ring stays sample-aligned with what the
//! backend actually consumed. The [`SpeakerGate`] shares the ring with the tee
//! and runs the per-`Final` decision inside
//! [`ListenScheduler`](super::scheduler::ListenScheduler).
//!
//! Gating is **per-segment (per `Final`)**, not per-frame. Any *infrastructure*
//! failure — segment audio already scrolled out of the ring, an embedding
//! error, or a dimensionality mismatch — fails **open** (keeps the segment and
//! warns) so a gate hiccup never silently swallows the user's own speech. A
//! genuine mismatch, or a segment too short to embed, drops the `Final`.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use tracing::warn;

use crate::voice::models::SPEAKER_WESPEAKER_EN;
use crate::voice::transcriber::{AsyncAudioInput, AudioChunk, SpeakerId};
use crate::voice::{cosine, speaker_file, EnrolledSpeaker, WespeakerEmbedder, MIN_EMBED_SAMPLES};

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
/// The absolute indexing is what lets [`SpeakerGate::accept`] map a `Final`'s
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

/// Per-`Final` speaker gate: reconstructs a segment's audio from the shared
/// [`PcmRing`] and keeps the segment only when it matches the enrolled speaker.
///
/// Load it once at startup with [`SpeakerGate::load`] (fail-fast on missing
/// enrolment or model), hand its [`ring`](SpeakerGate::ring) to a
/// [`TeeAudioInput`], and call [`accept`](SpeakerGate::accept) on each `Final`.
pub struct SpeakerGate {
    name: String,
    enrolled: EnrolledSpeaker,
    embedder: Box<dyn SpeakerEmbedder>,
    threshold: f32,
    ring: Arc<Mutex<PcmRing>>,
}

impl SpeakerGate {
    /// Loads the enrolled speaker `name` and the wespeaker embedder, failing
    /// fast (before capture) if either is missing. Mirrors the batch
    /// `transcribe --speaker` loader but feeds a rolling ring instead of a
    /// whole-file PCM buffer.
    pub fn load(name: &str, speaker_model: Option<&Path>, threshold: f32) -> Result<Self> {
        let enrolled_path = speaker_file(name)?;
        let enrolled = EnrolledSpeaker::load(&enrolled_path).with_context(|| {
            format!(
                "load enrolled speaker {name} from {}; run `omni-voice enroll --name {name}` first",
                enrolled_path.display()
            )
        })?;
        let dir = SPEAKER_WESPEAKER_EN.resolve_dir(speaker_model)?;
        SPEAKER_WESPEAKER_EN.ensure_present(&dir)?;
        let model_path = dir.join(SPEAKER_WESPEAKER_EN.required_files[0]);
        let embedder = WespeakerEmbedder::new(&model_path)?;
        Ok(Self::from_parts(name, enrolled, embedder, threshold))
    }

    /// Assembles a gate from already-loaded parts, minting a fresh ring. The
    /// seam [`load`](SpeakerGate::load) builds on; tests inject a real
    /// [`WespeakerEmbedder`] (end-to-end) or a stub (decision logic) without
    /// going through disk resolution.
    #[must_use]
    pub fn from_parts<E: SpeakerEmbedder + 'static>(
        name: &str,
        enrolled: EnrolledSpeaker,
        embedder: E,
        threshold: f32,
    ) -> Self {
        Self {
            name: name.to_string(),
            enrolled,
            embedder: Box::new(embedder),
            threshold,
            ring: Arc::new(Mutex::new(PcmRing::new(RING_CAPACITY_SAMPLES))),
        }
    }

    /// A handle to the shared ring, for the [`TeeAudioInput`] that fills it.
    #[must_use]
    pub fn ring(&self) -> Arc<Mutex<PcmRing>> {
        Arc::clone(&self.ring)
    }

    /// The enrolled speaker name stamped onto kept `Final`s.
    #[must_use]
    pub fn speaker_id(&self) -> SpeakerId {
        self.name.clone()
    }

    /// Decides whether the segment spanning `[start, end)` is the enrolled
    /// speaker.
    ///
    /// Keeps (`true`) on a cosine match; drops (`false`) on a genuine mismatch
    /// or a segment too short to embed (matching batch's conservative drop).
    /// Fails **open** (keeps + warns) on any infrastructure failure so a gate
    /// hiccup never eats the user's own speech.
    #[must_use]
    pub fn accept(&self, start: Duration, end: Duration) -> bool {
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
            return true;
        };
        if window.len() < MIN_EMBED_SAMPLES {
            // Too short for a stable embedding; drop, as `transcribe --speaker`.
            return false;
        }
        let emb = match self.embedder.embed(&window) {
            Ok(v) => v,
            Err(e) => {
                warn!("speaker gate: embedding failed ({e:#}); keeping (fail-open)");
                return true;
            }
        };
        if emb.len() != self.enrolled.vector.len() {
            warn!(
                got = emb.len(),
                want = self.enrolled.vector.len(),
                "speaker gate: embedding dim mismatch; keeping (fail-open)"
            );
            return true;
        }
        cosine(&emb, &self.enrolled.vector) >= self.threshold
    }
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

    /// A gate enrolled on `enrolled`, whose embedder returns `stub_out` for
    /// every window, at cosine `threshold`.
    fn stub_gate(enrolled: Vec<f32>, stub_out: Option<Vec<f32>>, threshold: f32) -> SpeakerGate {
        let enrolled = EnrolledSpeaker {
            name: "me".to_string(),
            model: "stub".to_string(),
            dim: enrolled.len(),
            vector: enrolled,
            samples_used: 1,
            enrolled_at: chrono::Utc::now(),
        };
        SpeakerGate::from_parts("me", enrolled, StubEmbedder { vector: stub_out }, threshold)
    }

    /// Pushes `n` silent samples into the gate's ring.
    fn fill(gate: &SpeakerGate, n: usize) {
        gate.ring().lock().unwrap().push(&vec![0_i16; n]);
    }

    fn secs(t: f64) -> Duration {
        Duration::from_secs_f64(t)
    }

    #[test]
    fn accept_keeps_on_cosine_match() {
        let gate = stub_gate(vec![1.0, 0.0], Some(vec![1.0, 0.0]), 0.5);
        fill(&gate, MIN_EMBED_SAMPLES);
        assert!(gate.accept(secs(0.0), secs(0.5)));
        assert_eq!(gate.speaker_id(), "me");
    }

    #[test]
    fn accept_drops_on_cosine_mismatch() {
        let gate = stub_gate(vec![1.0, 0.0], Some(vec![0.0, 1.0]), 0.5);
        fill(&gate, MIN_EMBED_SAMPLES);
        assert!(!gate.accept(secs(0.0), secs(0.5)));
    }

    #[test]
    fn accept_drops_segment_too_short_to_embed() {
        // stub_out would match, so a drop here is purely the length guard.
        let gate = stub_gate(vec![1.0, 0.0], Some(vec![1.0, 0.0]), 0.5);
        fill(&gate, 100); // < MIN_EMBED_SAMPLES
        assert!(!gate.accept(secs(0.0), secs(0.5)));
    }

    #[test]
    fn accept_fails_open_when_window_scrolled_out() {
        // Mismatching stub, so keeping proves the fail-open (not a match).
        let gate = stub_gate(vec![1.0, 0.0], Some(vec![0.0, 1.0]), 0.5);
        fill(&gate, RING_CAPACITY_SAMPLES + MIN_EMBED_SAMPLES); // evicts index 0
        assert!(gate.accept(secs(0.0), secs(0.5)));
    }

    #[test]
    fn accept_fails_open_on_embed_error() {
        let gate = stub_gate(vec![1.0, 0.0], None, 0.5); // embedder errors
        fill(&gate, MIN_EMBED_SAMPLES);
        assert!(gate.accept(secs(0.0), secs(0.5)));
    }

    #[test]
    fn accept_fails_open_on_dim_mismatch() {
        // 3-dim embedding vs 2-dim enrolled: guarded before cosine would panic.
        let gate = stub_gate(vec![1.0, 0.0], Some(vec![1.0, 0.0, 0.0]), 0.5);
        fill(&gate, MIN_EMBED_SAMPLES);
        assert!(gate.accept(secs(0.0), secs(0.5)));
    }
}

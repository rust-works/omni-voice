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

/// Per-`Final` speaker gate: reconstructs a segment's audio from the shared
/// [`PcmRing`] and keeps the segment only when it matches the enrolled speaker.
///
/// Load it once at startup with [`SpeakerGate::load`] (fail-fast on missing
/// enrolment or model), hand its [`ring`](SpeakerGate::ring) to a
/// [`TeeAudioInput`], and call [`accept`](SpeakerGate::accept) on each `Final`.
pub struct SpeakerGate {
    name: String,
    enrolled: EnrolledSpeaker,
    embedder: WespeakerEmbedder,
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
    /// seam [`load`](SpeakerGate::load) builds on and the one tests use to
    /// inject a real embedder without going through disk resolution.
    #[must_use]
    pub fn from_parts(
        name: &str,
        enrolled: EnrolledSpeaker,
        embedder: WespeakerEmbedder,
        threshold: f32,
    ) -> Self {
        Self {
            name: name.to_string(),
            enrolled,
            embedder,
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
}

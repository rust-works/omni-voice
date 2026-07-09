//! cpal capture supervisor for `voice listen`.
//!
//! Runs on a dedicated OS thread (the cpal `Stream` is `!Send`). It builds a
//! [`CpalAudioSource`], pumps captured f32 frames through mixdown → resample
//! → i16-quantise into the [`AudioChunkSender`], and — when the stream dies
//! (device disconnect, xrun) — re-opens it behind an exponential
//! [`Backoff`], giving up after a bounded number of retries.
//!
//! The audio-conversion core ([`PcmConverter`], [`pump_source`]) is generic
//! over [`AudioSource`], so it is unit-tested with the fixture
//! [`FileAudioSource`](crate::voice::FileAudioSource) — no microphone. The
//! cpal-specific build-and-retry loop ([`run_cpal_supervisor`]) is the thin
//! hardware wrapper on top.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tracing::{error, warn};

use super::input::{AudioChunkSender, SendOutcome};
use crate::voice::audio::{AudioSource, CpalAudioSource};
use crate::voice::wav::{mono_mixdown, Resampler};

/// Default capture buffer size in frames: 100 ms at 16 kHz. Set explicitly
/// (rather than `BufferSize::Default`) so the OS honours a predictable
/// callback cadence.
pub const DEFAULT_BUFFER_FRAMES: u32 = 1600;

/// First backoff delay after a stream failure.
const BACKOFF_BASE: Duration = Duration::from_millis(100);
/// Backoff ceiling — retries never wait longer than this.
const BACKOFF_CAP: Duration = Duration::from_secs(5);
/// Give up re-opening the stream after this many consecutive failures.
const BACKOFF_MAX_RETRIES: u32 = 20;

/// Converts device-native interleaved f32 frames into 16 kHz mono i16 PCM.
///
/// That is the format the ASR
/// [`AsyncAudioInput`](crate::voice::transcriber::AsyncAudioInput) contract
/// requires. Wraps the capture pipeline's [`mono_mixdown`] + [`Resampler`],
/// adding the final f32→i16 quantise step.
pub struct PcmConverter {
    resampler: Resampler,
    channels: u16,
}

impl PcmConverter {
    /// Builds a converter for a source at `input_rate` Hz with `channels`
    /// interleaved channels.
    pub fn new(input_rate: u32, channels: u16) -> Result<Self> {
        Ok(Self {
            resampler: Resampler::new(input_rate)?,
            channels,
        })
    }

    /// Mixes down, resamples, and quantises one interleaved f32 chunk.
    pub fn push(&mut self, interleaved: &[f32]) -> Result<Vec<i16>> {
        let mono = mono_mixdown(interleaved, self.channels);
        let resampled = self.resampler.push(&mono)?;
        Ok(f32_to_i16(&resampled))
    }

    /// Drains the resampler tail at end-of-stream, quantised to i16.
    pub fn flush(&mut self) -> Result<Vec<i16>> {
        Ok(f32_to_i16(&self.resampler.flush()?))
    }
}

/// Quantises 16 kHz mono f32 (in `[-1.0, 1.0]`) to signed 16-bit PCM,
/// clamping overshoot — matches [`crate::voice::wav::WavWriter`]'s cast.
fn f32_to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|s| (s.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16)
        .collect()
}

/// Why [`pump_source`] returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PumpReason {
    /// The [`AudioSource`] returned `None` — stream ended (or died).
    SourceEnded,
    /// The consumer dropped the channel receiver — nothing left to feed.
    ChannelClosed,
}

/// Result of one pump run.
#[derive(Debug, Clone, Copy)]
pub struct PumpResult {
    /// Why the pump stopped.
    pub reason: PumpReason,
    /// How many chunks were successfully enqueued (excludes dropped).
    pub delivered: u64,
}

/// Drains `source` into `sender`, converting each chunk to 16 kHz mono i16,
/// until the source ends, the channel closes, or `shutdown` is set.
///
/// Builds its own [`PcmConverter`] from the source's reported rate/channels,
/// so a re-opened stream at a different native rate is handled transparently.
/// Never blocks on a full queue — over-capacity chunks are dropped by
/// [`AudioChunkSender::try_send`].
pub fn pump_source<S: AudioSource>(
    mut source: S,
    sender: &AudioChunkSender,
    shutdown: &Arc<AtomicBool>,
) -> Result<PumpResult> {
    let mut converter = PcmConverter::new(source.sample_rate(), source.channels())?;
    let mut delivered: u64 = 0;
    loop {
        if shutdown.load(Ordering::Relaxed) {
            // Best-effort tail flush; ignore the outcome (we're stopping).
            if let Ok(tail) = converter.flush() {
                if !tail.is_empty() {
                    let _ = sender.try_send(tail);
                }
            }
            return Ok(PumpResult {
                reason: PumpReason::SourceEnded,
                delivered,
            });
        }
        let Some(chunk) = source.next_chunk() else {
            let tail = converter.flush()?;
            if !tail.is_empty() && sender.try_send(tail) == SendOutcome::Closed {
                return Ok(PumpResult {
                    reason: PumpReason::ChannelClosed,
                    delivered,
                });
            }
            return Ok(PumpResult {
                reason: PumpReason::SourceEnded,
                delivered,
            });
        };
        let converted = converter.push(&chunk)?;
        if converted.is_empty() {
            continue;
        }
        match sender.try_send(converted) {
            SendOutcome::Sent => delivered += 1,
            SendOutcome::Dropped => {}
            SendOutcome::Closed => {
                return Ok(PumpResult {
                    reason: PumpReason::ChannelClosed,
                    delivered,
                })
            }
        }
    }
}

/// Exponential backoff with a ceiling and a retry budget.
///
/// Delays double each attempt from [`BACKOFF_BASE`] up to [`BACKOFF_CAP`];
/// after [`Backoff::max_retries`] consecutive attempts [`Backoff::next_delay`]
/// returns `None` (give up). A successful stream resets the sequence.
pub struct Backoff {
    base: Duration,
    cap: Duration,
    max_retries: u32,
    attempt: u32,
}

impl Backoff {
    /// Builds a backoff with the given base delay, ceiling, and retry budget.
    #[must_use]
    pub fn new(base: Duration, cap: Duration, max_retries: u32) -> Self {
        Self {
            base,
            cap,
            max_retries,
            attempt: 0,
        }
    }

    /// Resets the sequence to the first delay (call after a good stream).
    pub fn reset(&mut self) {
        self.attempt = 0;
    }

    /// Returns the next delay and advances the sequence, or `None` once the
    /// retry budget is exhausted.
    pub fn next_delay(&mut self) -> Option<Duration> {
        if self.attempt >= self.max_retries {
            return None;
        }
        let mult = 1u32.checked_shl(self.attempt.min(20)).unwrap_or(u32::MAX);
        let delay = self
            .base
            .checked_mul(mult)
            .unwrap_or(self.cap)
            .min(self.cap);
        self.attempt += 1;
        Some(delay)
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new(BACKOFF_BASE, BACKOFF_CAP, BACKOFF_MAX_RETRIES)
    }
}

/// The dedicated-thread cpal supervisor loop.
///
/// Repeatedly opens the input device and pumps it into `sender`. On stream
/// death it re-opens behind [`Backoff`]; when the budget is exhausted it
/// sets `shutdown` and returns so the rest of the pipeline can wind down.
/// Returns when the consumer goes away, `shutdown` is set, or retries are
/// exhausted.
pub fn run_cpal_supervisor(
    device: Option<String>,
    buffer_frames: Option<u32>,
    sender: AudioChunkSender,
    shutdown: Arc<AtomicBool>,
) {
    let mut backoff = Backoff::default();
    loop {
        if shutdown.load(Ordering::Relaxed) {
            return;
        }
        match CpalAudioSource::with_buffer_size(device.as_deref(), buffer_frames) {
            Ok(source) => match pump_source(source, &sender, &shutdown) {
                Ok(result) => {
                    if result.reason == PumpReason::ChannelClosed
                        || shutdown.load(Ordering::Relaxed)
                    {
                        return;
                    }
                    if result.delivered > 0 {
                        backoff.reset();
                    }
                    warn!("audio stream ended; attempting to re-open");
                }
                Err(e) => warn!("audio pump error: {e:#}"),
            },
            Err(e) => warn!("failed to open audio input stream: {e:#}"),
        }
        if let Some(delay) = backoff.next_delay() {
            std::thread::sleep(delay);
        } else {
            error!("giving up on the audio input stream after repeated failures");
            shutdown.store(true, Ordering::Relaxed);
            return;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::audio::FileAudioSource;
    use crate::voice::listen::input::audio_channel;

    fn flag(value: bool) -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(value))
    }

    #[test]
    fn f32_to_i16_clamps_and_scales() {
        let out = f32_to_i16(&[0.0, 1.0, -1.0, 2.0, -2.0]);
        assert_eq!(out[0], 0);
        assert_eq!(out[1], i16::MAX);
        assert_eq!(out[2], -i16::MAX); // -32767 (symmetric scale)
        assert_eq!(out[3], i16::MAX); // clamped
        assert_eq!(out[4], -i16::MAX); // clamped
    }

    #[test]
    fn pump_identity_rate_forwards_all_samples_as_i16() {
        // 16 kHz mono source ⇒ resampler is identity; samples pass through.
        let samples = vec![0.5_f32; 4_000];
        let source = FileAudioSource::from_samples(samples, 16_000, 1, 1_000);
        let (tx, mut rx) = audio_channel(64);
        let result = pump_source(source, &tx, &flag(false)).unwrap();
        assert_eq!(result.reason, PumpReason::SourceEnded);
        assert!(result.delivered > 0);

        let mut total = 0usize;
        while let Ok(chunk) = rx.try_recv() {
            for s in chunk {
                assert_eq!(s, (0.5 * f32::from(i16::MAX)).round() as i16);
                total += 1;
            }
        }
        assert_eq!(total, 4_000, "all samples should survive the identity path");
    }

    #[test]
    fn pump_downmixes_stereo_to_mono() {
        // Stereo L/R that averages to a constant; 4 frames per chunk.
        let interleaved = vec![0.2_f32, 0.4, 0.2, 0.4, 0.2, 0.4, 0.2, 0.4];
        let source = FileAudioSource::from_samples(interleaved, 16_000, 2, 2);
        let (tx, mut rx) = audio_channel(64);
        let result = pump_source(source, &tx, &flag(false)).unwrap();
        assert_eq!(result.reason, PumpReason::SourceEnded);
        let expected = (0.3 * f32::from(i16::MAX)).round() as i16;
        let mut mono = Vec::new();
        while let Ok(chunk) = rx.try_recv() {
            mono.extend(chunk);
        }
        assert_eq!(mono.len(), 4, "8 interleaved stereo samples → 4 mono");
        assert!(mono.iter().all(|&s| s == expected));
    }

    #[test]
    fn pump_stops_when_shutdown_is_preset() {
        let source = FileAudioSource::from_samples(vec![0.1_f32; 16_000], 16_000, 1, 1_000);
        let (tx, _rx) = audio_channel(64);
        let result = pump_source(source, &tx, &flag(true)).unwrap();
        assert_eq!(result.reason, PumpReason::SourceEnded);
        assert_eq!(result.delivered, 0, "preset shutdown pumps nothing");
    }

    #[test]
    fn pump_reports_channel_closed_when_receiver_dropped() {
        let source = FileAudioSource::from_samples(vec![0.1_f32; 16_000], 16_000, 1, 100);
        let (tx, rx) = audio_channel(2);
        drop(rx);
        let result = pump_source(source, &tx, &flag(false)).unwrap();
        assert_eq!(result.reason, PumpReason::ChannelClosed);
    }

    #[test]
    fn pump_flushes_resampler_tail_at_source_end() {
        // 48 kHz needs the (non-identity) resampler. 5000 input frames = one
        // 4096-frame chunk processed during push + a buffered remainder that
        // is only emitted by the end-of-source flush — the path identity-rate
        // tests never exercise.
        let source = FileAudioSource::from_samples(vec![0.25_f32; 5_000], 48_000, 1, 5_000);
        let (tx, mut rx) = audio_channel(64);
        let result = pump_source(source, &tx, &flag(false)).unwrap();
        assert_eq!(result.reason, PumpReason::SourceEnded);
        assert!(
            result.delivered > 0,
            "push should deliver at least one chunk"
        );
        let mut got = 0usize;
        while let Ok(chunk) = rx.try_recv() {
            got += chunk.len();
        }
        // ~5000 * 16000/48000 ≈ 1666 samples once the flush tail is included.
        assert!(got > 1_000, "flush tail should be included, got {got}");
    }

    /// [`AudioSource`] wrapper that flips a shutdown flag after returning
    /// `after_chunks` chunks, so the pump's shutdown branch fires while the
    /// resampler still holds buffered (48 kHz) input to flush.
    struct FlipShutdownAfter {
        inner: FileAudioSource,
        flag: Arc<AtomicBool>,
        after_chunks: u32,
        seen: u32,
    }

    impl AudioSource for FlipShutdownAfter {
        fn next_chunk(&mut self) -> Option<Vec<f32>> {
            let chunk = self.inner.next_chunk();
            if chunk.is_some() {
                self.seen += 1;
                if self.seen >= self.after_chunks {
                    self.flag.store(true, Ordering::Relaxed);
                }
            }
            chunk
        }
        fn sample_rate(&self) -> u32 {
            self.inner.sample_rate()
        }
        fn channels(&self) -> u16 {
            self.inner.channels()
        }
    }

    #[test]
    fn pump_flushes_tail_on_shutdown() {
        let flag = flag(false);
        // One 2000-frame chunk (< the 4096-frame resampler chunk) buffers with
        // no push output; the flag then flips, so the next loop iteration hits
        // the shutdown branch and flushes the buffered tail.
        let inner = FileAudioSource::from_samples(vec![0.3_f32; 48_000], 48_000, 1, 2_000);
        let source = FlipShutdownAfter {
            inner,
            flag: Arc::clone(&flag),
            after_chunks: 1,
            seen: 0,
        };
        let (tx, mut rx) = audio_channel(64);
        let result = pump_source(source, &tx, &flag).unwrap();
        assert_eq!(result.reason, PumpReason::SourceEnded);
        let mut got = 0usize;
        while let Ok(chunk) = rx.try_recv() {
            got += chunk.len();
        }
        assert!(got > 0, "shutdown flush should emit the buffered tail");
    }

    #[test]
    fn backoff_doubles_then_caps_then_gives_up() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_secs(5), 20);
        assert_eq!(b.next_delay(), Some(Duration::from_millis(100)));
        assert_eq!(b.next_delay(), Some(Duration::from_millis(200)));
        assert_eq!(b.next_delay(), Some(Duration::from_millis(400)));
        assert_eq!(b.next_delay(), Some(Duration::from_millis(800)));
        assert_eq!(b.next_delay(), Some(Duration::from_millis(1600)));
        assert_eq!(b.next_delay(), Some(Duration::from_millis(3200)));
        // 6400 ms would exceed the 5 s cap.
        assert_eq!(b.next_delay(), Some(Duration::from_secs(5)));
        assert_eq!(b.next_delay(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn backoff_reset_restarts_sequence() {
        let mut b = Backoff::new(Duration::from_millis(100), Duration::from_secs(5), 20);
        let _ = b.next_delay();
        let _ = b.next_delay();
        b.reset();
        assert_eq!(b.next_delay(), Some(Duration::from_millis(100)));
    }

    #[test]
    fn backoff_gives_up_after_budget() {
        let mut b = Backoff::new(Duration::from_millis(1), Duration::from_millis(10), 3);
        assert!(b.next_delay().is_some());
        assert!(b.next_delay().is_some());
        assert!(b.next_delay().is_some());
        assert_eq!(b.next_delay(), None, "budget of 3 exhausted");
    }
}

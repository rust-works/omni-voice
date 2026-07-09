//! `MockStreamingTranscriber` — canned streaming events for `voice listen`.
//!
//! The streaming counterpart of [`MockTranscriber`](super::mock::MockTranscriber):
//! a dependency-free [`StreamingTranscriber`] that drains its
//! [`AsyncAudioInput`] and emits scripted `Partial` / `Final` /
//! `Endpoint` events. It exists so the `voice listen` scheduler can be
//! driven end-to-end under `--no-default-features` and in CI — no MLX
//! toolchain, no ~3 GB model.
//!
//! Timing is driven by the input: each [`MockSegment`] is emitted once the
//! amount of audio consumed reaches the segment's `end` time. Paired with
//! [`FileAsyncAudioInput`](crate::voice::transcriber::FileAsyncAudioInput)
//! (`realtime = true`) it replays on the same timeline a live mic would;
//! with `realtime = false` the whole script drains instantly, which is
//! what the deterministic smoke test relies on.
//!
//! Per segment (while audio is still flowing) the mock emits, in order:
//! `Partial`, a revisable `Final`, and an `Endpoint { kind: SilenceGap }`
//! — the silence endpoint is the signal the scheduler treats as a
//! reflection trigger. Any segments still pending when the input drains are
//! flushed as non-revisable `Final`s, and the stream always closes with a
//! terminal `Endpoint { kind: StreamEnd }`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use crate::voice::backends::mock::MockSegment;
use crate::voice::det::{SystemUlidRng, UlidRng};
use crate::voice::transcriber::{
    AsyncAudioInput, EndpointKind, StreamingTranscriber, TranscriptEvent, TranscriptEventStream,
};

/// Builds a fresh boxed [`UlidRng`] for one `transcribe_stream` call.
type RngFactory = Arc<dyn Fn() -> Box<dyn UlidRng> + Send + Sync>;

/// A canned-script [`StreamingTranscriber`] used as a live-ASR stand-in.
pub struct MockStreamingTranscriber {
    script: Vec<MockSegment>,
    rng_factory: RngFactory,
}

impl MockStreamingTranscriber {
    /// Builds a mock with a real-entropy ULID source.
    #[must_use]
    pub fn new(script: Vec<MockSegment>) -> Self {
        Self {
            script,
            rng_factory: Arc::new(|| Box::new(SystemUlidRng)),
        }
    }

    /// Test-friendly constructor: the caller supplies a factory that mints a
    /// fresh [`UlidRng`] per stream (use
    /// [`CountingUlidRng`](crate::voice::det::CountingUlidRng) for
    /// determinism).
    #[must_use]
    pub fn with_rng_factory(script: Vec<MockSegment>, rng_factory: RngFactory) -> Self {
        Self {
            script,
            rng_factory,
        }
    }

    /// The script the factory uses when no caller-side script is supplied
    /// (i.e. `voice listen --backend mock`). Two short utterances with a
    /// silence gap between them, so consumers see a realistic
    /// `Partial → Final → SilenceGap` cadence.
    #[must_use]
    pub fn default_script() -> Vec<MockSegment> {
        vec![
            MockSegment {
                text: "[mock listen] segment 1".to_string(),
                start: Duration::from_millis(0),
                end: Duration::from_secs(2),
                confidence: 1.0,
            },
            MockSegment {
                text: "[mock listen] segment 2".to_string(),
                start: Duration::from_secs(3),
                end: Duration::from_secs(5),
                confidence: 1.0,
            },
        ]
    }
}

impl StreamingTranscriber for MockStreamingTranscriber {
    fn transcribe_stream(&self, audio: Box<dyn AsyncAudioInput>) -> TranscriptEventStream {
        let state = StreamState {
            audio,
            segments: self.script.clone(),
            next_seg: 0,
            consumed_samples: 0,
            pending: VecDeque::new(),
            rng: (self.rng_factory)(),
            ended: false,
        };
        Box::pin(futures::stream::unfold(state, |mut state| async move {
            loop {
                if let Some(event) = state.pending.pop_front() {
                    return Some((Ok(event), state));
                }
                if state.ended {
                    return None;
                }
                if let Some(chunk) = state.audio.next_chunk().await {
                    state.consumed_samples = state.consumed_samples.saturating_add(chunk.len());
                    let now = samples_to_duration(state.consumed_samples);
                    while state.next_seg < state.segments.len()
                        && state.segments[state.next_seg].end <= now
                    {
                        let seg = state.segments[state.next_seg].clone();
                        queue_segment(
                            &mut state, &seg, /* revisable */ true, /* gap */ true,
                        );
                        state.next_seg += 1;
                    }
                } else {
                    // Input drained: flush any un-emitted segments as
                    // non-revisable finals, then the terminal endpoint.
                    let now = samples_to_duration(state.consumed_samples);
                    while state.next_seg < state.segments.len() {
                        let seg = state.segments[state.next_seg].clone();
                        queue_segment(
                            &mut state, &seg, /* revisable */ false, /* gap */ false,
                        );
                        state.next_seg += 1;
                    }
                    state.pending.push_back(TranscriptEvent::Endpoint {
                        at: now,
                        kind: EndpointKind::StreamEnd,
                    });
                    state.ended = true;
                }
            }
        }))
    }
}

/// Unfold state for one streaming run.
struct StreamState {
    audio: Box<dyn AsyncAudioInput>,
    segments: Vec<MockSegment>,
    next_seg: usize,
    consumed_samples: usize,
    pending: VecDeque<TranscriptEvent>,
    rng: Box<dyn UlidRng>,
    ended: bool,
}

fn samples_to_duration(samples: usize) -> Duration {
    #[allow(clippy::cast_precision_loss)]
    Duration::from_secs_f64(samples as f64 / 16_000.0)
}

fn queue_segment(state: &mut StreamState, seg: &MockSegment, revisable: bool, with_gap: bool) {
    state.pending.push_back(TranscriptEvent::Partial {
        text: seg.text.clone(),
        start: seg.start,
        end: seg.end,
        words: None,
        speaker: None,
    });
    state.pending.push_back(TranscriptEvent::Final {
        event_id: state.rng.next_ulid(),
        text: seg.text.clone(),
        start: seg.start,
        end: seg.end,
        confidence: seg.confidence,
        words: None,
        speaker: None,
        revisable,
    });
    if with_gap {
        state.pending.push_back(TranscriptEvent::Endpoint {
            at: seg.end,
            kind: EndpointKind::SilenceGap,
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::det::CountingUlidRng;
    use crate::voice::transcriber::FileAsyncAudioInput;
    use futures::StreamExt;

    fn counting_factory() -> RngFactory {
        Arc::new(|| Box::new(CountingUlidRng::new()))
    }

    async fn collect(script: Vec<MockSegment>, samples: usize) -> Vec<TranscriptEvent> {
        let backend = MockStreamingTranscriber::with_rng_factory(script, counting_factory());
        // 100 ms chunks, drained instantly (realtime = false).
        let input = FileAsyncAudioInput::from_samples(vec![0_i16; samples], 1_600, false);
        backend
            .transcribe_stream(Box::new(input))
            .map(Result::unwrap)
            .collect()
            .await
    }

    fn seg(text: &str, start_s: u64, end_s: u64) -> MockSegment {
        MockSegment {
            text: text.to_string(),
            start: Duration::from_secs(start_s),
            end: Duration::from_secs(end_s),
            confidence: 1.0,
        }
    }

    #[tokio::test]
    async fn emits_partial_final_gap_per_segment_then_stream_end() {
        // 6 s of audio at 16 kHz; two segments ending at 2 s and 4 s.
        let events = collect(vec![seg("one", 0, 2), seg("two", 2, 4)], 16_000 * 6).await;
        // 2 segments × (Partial, Final, SilenceGap) + terminal StreamEnd.
        assert_eq!(events.len(), 7, "got: {events:#?}");
        assert!(matches!(events[0], TranscriptEvent::Partial { .. }));
        assert!(matches!(
            events[1],
            TranscriptEvent::Final {
                revisable: true,
                ..
            }
        ));
        assert!(matches!(
            events[2],
            TranscriptEvent::Endpoint {
                kind: EndpointKind::SilenceGap,
                ..
            }
        ));
        assert!(matches!(
            events[6],
            TranscriptEvent::Endpoint {
                kind: EndpointKind::StreamEnd,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn segment_past_end_of_audio_is_flushed_nonrevisable() {
        // Only 1 s of audio, but a segment claims to end at 3 s — it must
        // still be flushed at stream end, as a non-revisable final with no
        // silence gap.
        let events = collect(vec![seg("late", 0, 3)], 16_000).await;
        assert_eq!(events.len(), 3, "got: {events:#?}");
        assert!(matches!(events[0], TranscriptEvent::Partial { .. }));
        assert!(matches!(
            events[1],
            TranscriptEvent::Final {
                revisable: false,
                ..
            }
        ));
        assert!(matches!(
            events[2],
            TranscriptEvent::Endpoint {
                kind: EndpointKind::StreamEnd,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn empty_script_still_emits_stream_end() {
        let events = collect(vec![], 16_000).await;
        assert_eq!(events.len(), 1);
        assert!(matches!(
            events[0],
            TranscriptEvent::Endpoint {
                kind: EndpointKind::StreamEnd,
                ..
            }
        ));
    }
}

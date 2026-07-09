//! The capture→ASR bridge: a bounded channel whose receiver is an
//! [`AsyncAudioInput`].
//!
//! The cpal capture source ([`AudioSource`](crate::voice::AudioSource)) is
//! `!Send` (it holds a CoreAudio handle) while the streaming transcriber
//! consumes a `Send` [`AsyncAudioInput`], so the two cannot live on the
//! same object — a thread boundary is mandatory. That boundary is this
//! channel: the [`supervisor`](super::supervisor) thread mixes down,
//! resamples, and quantises each captured chunk to 16 kHz mono i16, then
//! `try_send`s it here; the async side awaits [`ChannelAsyncAudioInput`].
//!
//! `try_send` never blocks the capture thread. When the consumer falls
//! behind and the queue fills, the newest chunk is dropped and a counter is
//! bumped — bounded latency at the cost of a little audio, which is the
//! right trade for realtime.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::voice::transcriber::{AsyncAudioInput, AudioChunk};

/// Default bounded-channel capacity: 160 chunks of 100 ms each ≈ 16 s of
/// audio. Sized to absorb a slow reflection without dropping audio.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 160;

/// Outcome of a single [`AudioChunkSender::try_send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The chunk was enqueued.
    Sent,
    /// The queue was full; the chunk was dropped and the counter bumped.
    Dropped,
    /// The receiver was dropped — the consumer is gone.
    Closed,
}

/// Producer half of the bridge. Cloneable is intentionally *not* derived —
/// there is a single capture producer per session.
pub struct AudioChunkSender {
    tx: mpsc::Sender<AudioChunk>,
    dropped: Arc<AtomicU64>,
}

impl AudioChunkSender {
    /// Enqueues `chunk` without blocking. On a full queue the chunk is
    /// dropped and the dropped-chunk counter is incremented; on a closed
    /// receiver [`SendOutcome::Closed`] is returned so the caller can stop.
    pub fn try_send(&self, chunk: AudioChunk) -> SendOutcome {
        match self.tx.try_send(chunk) {
            Ok(()) => SendOutcome::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
                SendOutcome::Dropped
            }
            Err(mpsc::error::TrySendError::Closed(_)) => SendOutcome::Closed,
        }
    }

    /// Total chunks dropped due to a full queue since construction.
    #[must_use]
    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// A shared handle to the dropped-chunk counter, so a caller can read
    /// the total after the sender has been moved onto the capture thread.
    #[must_use]
    pub fn dropped_handle(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
    }
}

/// Consumer half of the bridge: an [`AsyncAudioInput`] backed by the
/// channel. `next_chunk` awaits the next captured chunk and returns `None`
/// once every sender has been dropped (capture stopped).
pub struct ChannelAsyncAudioInput {
    rx: mpsc::Receiver<AudioChunk>,
}

impl ChannelAsyncAudioInput {
    /// Non-blocking receive: returns the next chunk if one is queued, or an
    /// error when the queue is momentarily empty or the producer is gone.
    /// The async path uses [`AsyncAudioInput::next_chunk`]; this exists for
    /// synchronous callers that drive a producer directly (e.g. tests).
    pub fn try_recv(&mut self) -> Result<AudioChunk, mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }
}

#[async_trait]
impl AsyncAudioInput for ChannelAsyncAudioInput {
    async fn next_chunk(&mut self) -> Option<AudioChunk> {
        self.rx.recv().await
    }
}

/// Builds a bounded bridge with the given queue `capacity` (clamped to at
/// least 1), returning the producer and consumer halves.
#[must_use]
pub fn audio_channel(capacity: usize) -> (AudioChunkSender, ChannelAsyncAudioInput) {
    let (tx, rx) = mpsc::channel(capacity.max(1));
    (
        AudioChunkSender {
            tx,
            dropped: Arc::new(AtomicU64::new(0)),
        },
        ChannelAsyncAudioInput { rx },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sent_chunks_arrive_in_order() {
        let (tx, mut rx) = audio_channel(8);
        assert_eq!(tx.try_send(vec![1, 2, 3]), SendOutcome::Sent);
        assert_eq!(tx.try_send(vec![4, 5]), SendOutcome::Sent);
        assert_eq!(rx.next_chunk().await, Some(vec![1, 2, 3]));
        assert_eq!(rx.next_chunk().await, Some(vec![4, 5]));
    }

    #[tokio::test]
    async fn dropping_receiver_closes_the_channel() {
        let (tx, rx) = audio_channel(4);
        drop(rx);
        assert_eq!(tx.try_send(vec![1]), SendOutcome::Closed);
    }

    #[tokio::test]
    async fn full_queue_drops_newest_and_counts() {
        // Capacity 1: first send fills the queue, second is dropped.
        let (tx, mut rx) = audio_channel(1);
        assert_eq!(tx.try_send(vec![1]), SendOutcome::Sent);
        assert_eq!(tx.try_send(vec![2]), SendOutcome::Dropped);
        assert_eq!(tx.try_send(vec![3]), SendOutcome::Dropped);
        assert_eq!(tx.dropped_count(), 2);
        // The one that made it in is the first (oldest) — newest are dropped.
        assert_eq!(rx.next_chunk().await, Some(vec![1]));
    }

    #[tokio::test]
    async fn exhausted_sender_yields_none() {
        let (tx, mut rx) = audio_channel(4);
        tx.try_send(vec![9]);
        drop(tx);
        assert_eq!(rx.next_chunk().await, Some(vec![9]));
        assert_eq!(rx.next_chunk().await, None);
    }
}

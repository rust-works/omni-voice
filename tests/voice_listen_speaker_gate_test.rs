//! Live-listen speaker gate end-to-end against
//! `tests/fixtures/voice/two_speakers.wav`.
//!
//! Proves the differentiator: with a speaker enrolled, a `SpeakerGate` fed the
//! enrolled speaker's audio *accepts* the segment, and fed a different
//! speaker's audio *rejects* it — the same wespeaker cosine gate batch
//! `transcribe --speaker` ships, now driven off a rolling PCM ring by segment
//! timestamps instead of a whole-file buffer.
//!
//! `#[ignore]`-by-default because it needs the wespeaker ONNX staged on disk.
//! Run locally with:
//!
//! ```text
//! omni-voice install-model --variant speaker-wespeaker-en
//! cargo test --test voice_listen_speaker_gate_test -- --ignored
//! ```
//!
//! Or point at a pre-staged install via `OMNI_VOICE_VOICE_SPEAKER_MODEL` (the
//! CI cache hook, tracked by #13).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use omni_voice::voice::listen::speaker_gate::SpeakerGate;
use omni_voice::voice::models::SPEAKER_WESPEAKER_EN;
use omni_voice::voice::{EnrolledSpeaker, WespeakerEmbedder, DEFAULT_SPEAKER_THRESHOLD};

fn fixture_wav() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/voice/two_speakers.wav")
}

fn resolve_speaker_model_path() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("OMNI_VOICE_VOICE_SPEAKER_MODEL") {
        if !env.is_empty() {
            let p = PathBuf::from(env);
            if p.is_dir() {
                let onnx = p.join(SPEAKER_WESPEAKER_EN.required_files[0]);
                if onnx.is_file() {
                    return Some(onnx);
                }
            } else if p.is_file() {
                return Some(p);
            }
        }
    }
    let dir = SPEAKER_WESPEAKER_EN.default_dir()?;
    let onnx = dir.join(SPEAKER_WESPEAKER_EN.required_files[0]);
    onnx.is_file().then_some(onnx)
}

fn read_pcm(path: &Path) -> Vec<i16> {
    let mut reader = hound::WavReader::open(path).expect("open two_speakers.wav");
    let spec = reader.spec();
    assert_eq!(spec.sample_rate, 16_000);
    assert_eq!(spec.channels, 1);
    assert_eq!(spec.bits_per_sample, 16);
    reader
        .samples::<i16>()
        .collect::<Result<Vec<_>, _>>()
        .expect("decode PCM")
}

fn slice(pcm: &[i16], start_s: f64, end_s: f64) -> Vec<i16> {
    let s = (start_s * 16_000.0) as usize;
    let e = (end_s * 16_000.0) as usize;
    pcm[s..e.min(pcm.len())].to_vec()
}

/// Seconds → segment-relative [`Duration`] for a window occupying the ring's
/// absolute `[start_s, end_s)` sample range.
fn secs(t: f64) -> Duration {
    Duration::from_secs_f64(t)
}

#[test]
#[ignore = "requires wespeaker ONNX on disk; run `omni-voice install-model --variant speaker-wespeaker-en` first"]
fn gate_accepts_enrolled_speaker_and_rejects_other() {
    let Some(model_path) = resolve_speaker_model_path() else {
        panic!(
            "wespeaker model not found. Run \
             `omni-voice install-model --variant speaker-wespeaker-en` or set \
             OMNI_VOICE_VOICE_SPEAKER_MODEL=<dir> to point at a pre-staged install."
        );
    };

    let embedder = WespeakerEmbedder::new(&model_path).expect("WespeakerEmbedder::new");
    let pcm = read_pcm(&fixture_wav());

    // Enroll on the first speaker-A window (same plan as the enroll test).
    let enrolled_vec = embedder
        .embed(&slice(&pcm, 1.0, 7.0))
        .expect("embed enroll");
    let enrolled = EnrolledSpeaker {
        name: "me".to_string(),
        model: "speaker-wespeaker-en".to_string(),
        dim: enrolled_vec.len(),
        vector: enrolled_vec,
        samples_used: 1,
        enrolled_at: Utc::now(),
    };

    // The gate owns a second embedder + a fresh ring; feed it two 6 s windows
    // back to back so their ring positions match the timestamps we query.
    let gate = SpeakerGate::from_parts("me", enrolled, embedder, DEFAULT_SPEAKER_THRESHOLD);
    let a_query = slice(&pcm, 6.0, 12.0); // speaker A → absolute [0 s, 6 s)
    let b_query = slice(&pcm, 13.5, 19.5); // speaker B → absolute [6 s, 12 s)
    {
        let ring = gate.ring();
        let mut ring = ring.lock().unwrap();
        ring.push(&a_query);
        ring.push(&b_query);
    }

    assert!(
        gate.accept(secs(0.0), secs(6.0)),
        "enrolled speaker A's own audio must be accepted"
    );
    assert!(
        !gate.accept(secs(6.0), secs(12.0)),
        "a different speaker (B) must be rejected"
    );
}

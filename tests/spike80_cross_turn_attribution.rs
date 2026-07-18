//! Spike #80 measurement harness: cross-turn attribution errors in the
//! nearest-speaker labeller (`listen --speaker a --speaker b`).
//!
//! Replays WAV variants built from `tests/fixtures/voice/two_speakers.wav`
//! through the real CLI binary (`listen --audio-file`), then scores the
//! `speaker` tags in each session's `transcript.jsonl` against ground-truth
//! windows known from the variants' construction:
//!
//! - `original` — the fixture verbatim: one A→B turn padded by 0.5 s of
//!   silence (the issue-mandated run; enrolment windows overlap the replayed
//!   audio, so its numbers are optimistically biased).
//! - `nogap` — held-out A audio butted directly against held-out B audio:
//!   one turn boundary with zero gap.
//! - `alternate` — held-out A/B audio interleaved in 2.5 s chunks with zero
//!   gap: three turn boundaries, the #70 cross-turn stressor.
//!
//! Prints per-variant segment and metric tables (run with `--nocapture`) and
//! asserts sanity only — this harness measures, it does not gate. Results
//! feed the GO/NO-GO on #70/#71; see `SPIKE.md`.
//!
//! `#[ignore]` by default: needs the wespeaker ONNX and Voxtral MLX INT4
//! models staged on disk, and ~45 s of real-time replay:
//!
//! ```text
//! omni-voice install-model --variant speaker-wespeaker-en
//! omni-voice install-model --variant voxtral-mlx-int4
//! cargo test --test spike80_cross_turn_attribution -- --ignored --nocapture
//! ```

#![cfg(feature = "voxtral-mlx")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use chrono::Utc;
use omni_voice::voice::models::{SPEAKER_WESPEAKER_EN, VOXTRAL_MLX_INT4};
use omni_voice::voice::transcriber::TranscriptEvent;
use omni_voice::voice::{speaker_file, EnrolledSpeaker, WespeakerEmbedder};

const SAMPLE_RATE: f64 = 16_000.0;

/// Spike-namespaced enrolment names: `listen` only loads from the real
/// `~/.omni-voice/voice/speakers/`, so these must not collide with the
/// user's own enrolments. The guard removes them on every exit path.
const SPEAKER_A: &str = "spike80-a";
const SPEAKER_B: &str = "spike80-b";

/// The labeller's below-threshold tag (`UNKNOWN_SPEAKER`, private to
/// `speaker_gate`).
const UNKNOWN_TAG: &str = "unknown";

/// Enrolment windows in the fixture (same plan as the shipped e2e tests);
/// everything outside them is held-out material for the replay variants.
const ENROLL_A: (f64, f64) = (1.0, 7.0);
const ENROLL_B: (f64, f64) = (13.5, 19.5);

/// Overlap below this is boundary jitter (voxtral frame quantisation and
/// endpoint drift), not real cross-turn content.
const BOUNDARY_TOLERANCE_S: f64 = 0.25;

/// Hard deadline per child `listen` run: replay is real-time paced (longest
/// variant 24.5 s) plus voxtral model load.
const CHILD_DEADLINE: Duration = Duration::from_secs(180);

/// AI-backend env vars scrubbed from the child so the end-of-stream drain
/// reflection fails fast instead of calling a live model.
const AI_ENV_VARS: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "CLAUDE_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OMNI_VOICE_AI_BACKEND",
    "USE_OPENAI",
    "USE_OLLAMA",
    "CLAUDE_CODE_USE_BEDROCK",
];

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

fn require_models() -> PathBuf {
    let speaker_model = resolve_speaker_model_path().unwrap_or_else(|| {
        panic!(
            "wespeaker model not found. Run \
             `omni-voice install-model --variant speaker-wespeaker-en` or set \
             OMNI_VOICE_VOICE_SPEAKER_MODEL=<dir> to point at a pre-staged install."
        )
    });
    let voxtral_dir = VOXTRAL_MLX_INT4
        .resolve_dir(None)
        .expect("resolve voxtral-mlx-int4 dir");
    VOXTRAL_MLX_INT4
        .ensure_present(&voxtral_dir)
        .expect("voxtral-mlx-int4 model present");
    speaker_model
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
    let s = (start_s * SAMPLE_RATE) as usize;
    let e = (end_s * SAMPLE_RATE) as usize;
    pcm[s..e.min(pcm.len())].to_vec()
}

fn write_wav(path: &Path, pcm: &[i16]) {
    let spec = hound::WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut writer = hound::WavWriter::create(path, spec).expect("create variant wav");
    for s in pcm {
        writer.write_sample(*s).expect("write sample");
    }
    writer.finalize().expect("finalize variant wav");
}

/// One replay WAV plus the ground-truth speech windows its construction
/// implies (replay-relative seconds).
struct Variant {
    name: &'static str,
    pcm: Vec<i16>,
    truth: Vec<(&'static str, f64, f64)>,
}

/// Concatenates fixture slices and derives the truth table from the actual
/// sample counts, so audio and ground truth cannot drift apart.
fn assemble(name: &'static str, pcm: &[i16], chunks: &[(&'static str, f64, f64)]) -> Variant {
    let mut out = Vec::new();
    let mut truth = Vec::new();
    for (speaker, start_s, end_s) in chunks {
        let seg = slice(pcm, *start_s, *end_s);
        let t0 = out.len() as f64 / SAMPLE_RATE;
        out.extend_from_slice(&seg);
        let t1 = out.len() as f64 / SAMPLE_RATE;
        truth.push((*speaker, t0, t1));
    }
    Variant {
        name,
        pcm: out,
        truth,
    }
}

fn build_variants(pcm: &[i16]) -> Vec<Variant> {
    vec![
        // The fixture verbatim: A [0,12), silence [12,12.5), B [12.5,24.5).
        Variant {
            name: "original",
            pcm: pcm.to_vec(),
            truth: vec![(SPEAKER_A, 0.0, 12.0), (SPEAKER_B, 12.5, 24.5)],
        },
        // Held-out halves butted together: one boundary, zero gap.
        assemble(
            "nogap",
            pcm,
            &[(SPEAKER_A, 7.0, 12.0), (SPEAKER_B, 19.5, 24.5)],
        ),
        // Held-out halves interleaved in 2.5 s chunks: three boundaries,
        // zero gap — chunks are ≥5× MIN_EMBED_SAMPLES yet shorter than
        // voxtral's endpoint cadence, so Finals span turns.
        assemble(
            "alternate",
            pcm,
            &[
                (SPEAKER_A, 7.0, 9.5),
                (SPEAKER_B, 19.5, 22.0),
                (SPEAKER_A, 9.5, 12.0),
                (SPEAKER_B, 22.0, 24.5),
            ],
        ),
    ]
}

/// Writes the two spike enrolments into the real speakers dir; `Drop`
/// removes them again (runs on panic unwind too).
struct EnrolmentGuard(Vec<PathBuf>);

impl EnrolmentGuard {
    fn enroll(speakers: Vec<(&str, Vec<f32>)>) -> Self {
        let mut paths = Vec::new();
        for (name, vector) in speakers {
            let record = EnrolledSpeaker {
                name: name.to_string(),
                model: "speaker-wespeaker-en".to_string(),
                dim: vector.len(),
                vector,
                samples_used: 1,
                enrolled_at: Utc::now(),
            };
            let path = speaker_file(name).expect("speaker file path");
            record.save(&path).expect("save spike enrolment");
            paths.push(path);
        }
        Self(paths)
    }
}

impl Drop for EnrolmentGuard {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Runs `listen --audio-file <wav>` labelling by the two spike enrolments,
/// with reflections neutralised (maxed triggers, scrubbed AI env) and the
/// gate's per-decision score log enabled. Panics with captured stderr on
/// non-zero exit or deadline expiry.
fn run_listen(voice_root: &Path, wav: &Path, session: &str) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_omni-voice"));
    cmd.args([
        "listen",
        "--audio-file",
        wav.to_str().expect("utf-8 wav path"),
        "--speaker",
        SPEAKER_A,
        "--speaker",
        SPEAKER_B,
        "--session",
        session,
        "--unknown-policy",
        "keep",
        "--trigger-word-delta",
        "4294967295",
        "--trigger-silence-gap-ms",
        "3600000",
        "--trigger-max-interval-ms",
        "3600000",
    ]);
    for var in AI_ENV_VARS {
        cmd.env_remove(var);
    }
    cmd.env("OMNI_VOICE_VOICE_ROOT", voice_root)
        .env("RUST_LOG", "omni_voice::voice::listen::speaker_gate=debug")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd.spawn().expect("spawn omni-voice listen");
    // Drain both pipes on threads so a full pipe buffer can't deadlock the
    // child while we poll for exit.
    let mut stdout_pipe = child.stdout.take().expect("child stdout");
    let mut stderr_pipe = child.stderr.take().expect("child stderr");
    let stdout_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout_pipe.read_to_string(&mut s);
        s
    });
    let stderr_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });

    let deadline = Instant::now() + CHILD_DEADLINE;
    let status = loop {
        if let Some(status) = child.try_wait().expect("try_wait listen") {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let stderr = stderr_thread.join().expect("join stderr thread");
            panic!("listen for session {session} exceeded {CHILD_DEADLINE:?}; stderr:\n{stderr}");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let _stdout = stdout_thread.join().expect("join stdout thread");
    let stderr = stderr_thread.join().expect("join stderr thread");
    assert!(
        status.success(),
        "listen for session {session} exited with {status}; stderr:\n{stderr}"
    );
    stderr
}

/// A `Final` line from `transcript.jsonl`, reduced to what scoring needs.
struct FinalSeg {
    start: f64,
    end: f64,
    speaker: Option<String>,
    text: String,
}

fn read_finals(transcript: &Path) -> Vec<FinalSeg> {
    let raw = std::fs::read_to_string(transcript)
        .unwrap_or_else(|e| panic!("read {}: {e}", transcript.display()));
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let event: TranscriptEvent = serde_json::from_str(line)
                .unwrap_or_else(|e| panic!("unparseable transcript line: {e}\n{line}"));
            event
        })
        .filter_map(|event| match event {
            TranscriptEvent::Final {
                start,
                end,
                speaker,
                text,
                ..
            } => Some(FinalSeg {
                start: start.as_secs_f64(),
                end: end.as_secs_f64(),
                speaker,
                text,
            }),
            _ => None,
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq)]
enum Class {
    Correct,
    WrongName,
    Unknown,
    Untagged,
    Unscorable,
}

impl Class {
    fn label(self) -> &'static str {
        match self {
            Self::Correct => "correct",
            Self::WrongName => "wrong-name",
            Self::Unknown => "unknown",
            Self::Untagged => "untagged",
            Self::Unscorable => "unscorable",
        }
    }
}

/// One scored segment: overlap per speaker, dominant-speaker truth, and the
/// tag classification.
struct ScoredSeg {
    start: f64,
    end: f64,
    ov_a: f64,
    ov_b: f64,
    tag: String,
    truth: &'static str,
    cross_turn: bool,
    class: Class,
    text: String,
}

fn overlap(seg_start: f64, seg_end: f64, win_start: f64, win_end: f64) -> f64 {
    (seg_end.min(win_end) - seg_start.max(win_start)).max(0.0)
}

fn score(finals: &[FinalSeg], truth: &[(&'static str, f64, f64)]) -> Vec<ScoredSeg> {
    finals
        .iter()
        .map(|seg| {
            let ov_for = |name: &str| {
                truth
                    .iter()
                    .filter(|(s, _, _)| *s == name)
                    .map(|(_, ws, we)| overlap(seg.start, seg.end, *ws, *we))
                    .sum::<f64>()
            };
            let ov_a = ov_for(SPEAKER_A);
            let ov_b = ov_for(SPEAKER_B);
            let (truth_speaker, best_ov) = if ov_a >= ov_b {
                (SPEAKER_A, ov_a)
            } else {
                (SPEAKER_B, ov_b)
            };
            let cross_turn = ov_a > BOUNDARY_TOLERANCE_S && ov_b > BOUNDARY_TOLERANCE_S;
            let class = if best_ov == 0.0 {
                Class::Unscorable
            } else {
                match seg.speaker.as_deref() {
                    None => Class::Untagged,
                    Some(UNKNOWN_TAG) => Class::Unknown,
                    Some(t) if t == truth_speaker => Class::Correct,
                    Some(_) => Class::WrongName,
                }
            };
            ScoredSeg {
                start: seg.start,
                end: seg.end,
                ov_a,
                ov_b,
                tag: seg.speaker.clone().unwrap_or_else(|| "-".to_string()),
                truth: truth_speaker,
                cross_turn,
                class,
                text: seg.text.clone(),
            }
        })
        .collect()
}

/// Per-variant tallies feeding the metric rates.
#[derive(Default)]
struct Counts {
    finals: usize,
    scored: usize,
    cross_turn: usize,
    correct: usize,
    wrong: usize,
    unknown: usize,
    untagged: usize,
    unscorable: usize,
    ct_wrong: usize,
    ct_unknown: usize,
    ct_untagged: usize,
}

impl Counts {
    fn add(&mut self, other: &Self) {
        self.finals += other.finals;
        self.scored += other.scored;
        self.cross_turn += other.cross_turn;
        self.correct += other.correct;
        self.wrong += other.wrong;
        self.unknown += other.unknown;
        self.untagged += other.untagged;
        self.unscorable += other.unscorable;
        self.ct_wrong += other.ct_wrong;
        self.ct_unknown += other.ct_unknown;
        self.ct_untagged += other.ct_untagged;
    }
}

fn tally(rows: &[ScoredSeg]) -> Counts {
    let mut c = Counts {
        finals: rows.len(),
        ..Counts::default()
    };
    for row in rows {
        if row.class == Class::Unscorable {
            c.unscorable += 1;
            continue;
        }
        c.scored += 1;
        if row.cross_turn {
            c.cross_turn += 1;
        }
        match row.class {
            Class::Correct => c.correct += 1,
            Class::WrongName => {
                c.wrong += 1;
                if row.cross_turn {
                    c.ct_wrong += 1;
                }
            }
            Class::Unknown => {
                c.unknown += 1;
                if row.cross_turn {
                    c.ct_unknown += 1;
                }
            }
            Class::Untagged => {
                c.untagged += 1;
                if row.cross_turn {
                    c.ct_untagged += 1;
                }
            }
            Class::Unscorable => unreachable!("skipped above"),
        }
    }
    c
}

fn rate(num: usize, den: usize) -> String {
    if den == 0 {
        "-".to_string()
    } else {
        format!("{:.0}% ({num}/{den})", 100.0 * num as f64 / den as f64)
    }
}

fn metrics_row(name: &str, c: &Counts) -> String {
    let misattr = c.wrong + c.unknown + c.untagged;
    let ct_misattr = c.ct_wrong + c.ct_unknown + c.ct_untagged;
    format!(
        "| {name} | {} | {} | {} | {} | {} | {} | {} |",
        c.finals,
        c.scored,
        rate(c.cross_turn, c.scored),
        rate(c.wrong, c.scored),
        rate(misattr, c.scored),
        rate(c.ct_wrong, c.cross_turn),
        rate(ct_misattr, c.cross_turn),
    )
}

fn print_segment_table(variant: &str, rows: &[ScoredSeg]) {
    println!("\n### Segments — `{variant}`\n");
    println!("| # | span (s) | ov_a | ov_b | tag | truth | cross-turn | class | text |");
    println!("|---|----------|------|------|-----|-------|------------|-------|------|");
    for (i, r) in rows.iter().enumerate() {
        let text: String = r.text.chars().take(60).collect();
        println!(
            "| {i} | {:.2}-{:.2} | {:.2} | {:.2} | {} | {} | {} | {} | {} |",
            r.start,
            r.end,
            r.ov_a,
            r.ov_b,
            r.tag,
            r.truth,
            if r.cross_turn { "yes" } else { "no" },
            r.class.label(),
            text.replace('|', "\\|"),
        );
    }
}

fn print_score_log(variant: &str, stderr: &str) {
    println!("\n### Gate score log — `{variant}`\n");
    for line in stderr
        .lines()
        .filter(|l| l.contains("per-speaker cosine scores"))
    {
        println!("    {line}");
    }
}

#[test]
#[ignore = "requires voxtral-mlx-int4 + speaker-wespeaker-en models on disk; ~45 s real-time replay"]
fn measure_cross_turn_attribution() {
    let speaker_model = require_models();
    let pcm = read_pcm(&fixture_wav());

    let embedder = WespeakerEmbedder::new(&speaker_model).expect("WespeakerEmbedder::new");
    let vec_a = embedder
        .embed(&slice(&pcm, ENROLL_A.0, ENROLL_A.1))
        .expect("embed enrolment A");
    let vec_b = embedder
        .embed(&slice(&pcm, ENROLL_B.0, ENROLL_B.1))
        .expect("embed enrolment B");
    let _guard = EnrolmentGuard::enroll(vec![(SPEAKER_A, vec_a), (SPEAKER_B, vec_b)]);

    let root = tempfile::tempdir().expect("temp voice root");
    let mut per_variant = Vec::new();
    for variant in build_variants(&pcm) {
        let wav = root.path().join(format!("{}.wav", variant.name));
        write_wav(&wav, &variant.pcm);
        let session = format!("spike80-{}", variant.name);
        let stderr = run_listen(root.path(), &wav, &session);

        let transcript = root.path().join(&session).join("transcript.jsonl");
        let finals = read_finals(&transcript);
        assert!(
            !finals.is_empty(),
            "variant {}: no Final segments in {}; stderr:\n{stderr}",
            variant.name,
            transcript.display()
        );

        let rows = score(&finals, &variant.truth);
        print_segment_table(variant.name, &rows);
        print_score_log(variant.name, &stderr);
        per_variant.push((variant.name, tally(&rows)));
    }

    println!("\n## Metrics\n");
    println!(
        "| variant | finals | scored | cross-turn rate | wrong-name rate | \
         misattr (strict) | ct wrong-name | ct misattr (strict) |"
    );
    println!("|---------|--------|--------|-----------------|-----------------|------------------|---------------|---------------------|");
    let mut held_out = Counts::default();
    let mut all = Counts::default();
    for (name, counts) in &per_variant {
        println!("{}", metrics_row(name, counts));
        all.add(counts);
        if *name != "original" {
            held_out.add(counts);
        }
    }
    println!("{}", metrics_row("pooled(held-out)", &held_out));
    println!("{}", metrics_row("pooled(all)", &all));
    println!(
        "\nDefinitions: scored = finals minus unscorable (all-silence); \
         cross-turn = >{BOUNDARY_TOLERANCE_S} s of ≥2 speakers; \
         misattr (strict) = wrong-name + unknown + untagged over scored; \
         ct columns restrict to cross-turn segments."
    );
}

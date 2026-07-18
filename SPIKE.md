# Spike #80 — cross-turn attribution errors in `listen` speaker labelling

**Status:** controlled measurement done (2026-07-18); dogfooding in progress.
**Decides:** #70 (diarization) and #71 (target-speaker extraction).
**Report due:** 2026-08-01, as comments on #70 and #71.

## Context

The nearest-speaker labeller (#69, PR #79) attributes speech per ASR segment:
each streaming `Final` is embedded once and tagged with the nearest enrolled
speaker. #70 assumes a `Final` spanning a speaker turn gets misattributed;
#71 assumes overlapping speech blends the embedding and defeats the decision.
This spike quantifies both before either follow-up is built.

## Method

### Controlled runs

The harness (`tests/spike80_cross_turn_attribution.rs`) replays three WAV
variants built from `tests/fixtures/voice/two_speakers.wav` (speaker A =
Joplin, speaker B = Kristin Luoma; see the fixture's PROVENANCE) through the
real CLI — `listen --audio-file <wav> --speaker spike80-a --speaker spike80-b`
with the voxtral-mlx streaming backend — and scores each session's
`transcript.jsonl` speaker tags against ground-truth windows known from the
variants' construction:

| Variant     | Construction                                        | Boundaries      |
|-------------|-----------------------------------------------------|-----------------|
| `original`  | fixture verbatim: A [0,12), silence, B [12.5,24.5)  | 1, 0.5 s gap    |
| `nogap`     | held-out A [7,12) butted against held-out B [19.5,24.5) | 1, zero gap |
| `alternate` | same held-out audio interleaved in 2.5 s chunks     | 3, zero gap     |

Enrolment uses A [1,7) and B [13.5,19.5) — disjoint from the `nogap` and
`alternate` replay audio, so those two variants are fully held-out.
`original` replays its own enrolment windows and is optimistically biased;
it is reported but the held-out pool is the trustworthy number.

**Scoring.** Per `Final` `[s,e)`: `ov(X)` = overlap with speaker X's
ground-truth windows. *Unscorable* = zero overlap with anyone (excluded from
denominators). *Cross-turn* = >0.25 s of ≥2 speakers (tolerance absorbs
voxtral frame/endpoint jitter). *Truth* = the dominant (max-overlap) speaker.
Tag classes: `correct`, `wrong-name` (the other enrolled name), `unknown`
(below threshold, kept by `--unknown-policy keep`), `untagged` (fail-open).
`misattr (strict)` = wrong-name + unknown + untagged over scored;
`wrong-name rate` counts only actual name swaps — the headline number for
the #70/#71 decision. `ct` columns restrict to cross-turn segments.

### Reproduce

```text
omni-voice install-model --variant speaker-wespeaker-en
omni-voice install-model --variant voxtral-mlx-int4
cargo test --test spike80_cross_turn_attribution -- --ignored --nocapture
```

~45 s of real-time replay; spike-namespaced enrolments (`spike80-a/b`) are
written to the real speakers dir and removed by an RAII guard.

## Results: controlled (2026-07-18, voxtral-mlx-int4, threshold 0.5)

| variant | finals | scored | cross-turn rate | wrong-name rate | misattr (strict) | ct wrong-name | ct misattr (strict) |
|---------|--------|--------|-----------------|-----------------|------------------|---------------|---------------------|
| original | 5 | 5 | 20% (1/5) | 0% (0/5) | 40% (2/5) | 0% (0/1) | 0% (0/1) |
| nogap | 4 | 4 | 25% (1/4) | 0% (0/4) | 50% (2/4) | 0% (0/1) | 0% (0/1) |
| alternate | 4 | 3 | 67% (2/3) | 0% (0/3) | 33% (1/3) | 0% (0/2) | 0% (0/2) |
| pooled(held-out) | 8 | 7 | 43% (3/7) | 0% (0/7) | 43% (3/7) | 0% (0/3) | 0% (0/3) |
| pooled(all) | 13 | 12 | 33% (4/12) | 0% (0/12) | 42% (5/12) | 0% (0/4) | 0% (0/4) |

Key per-segment observations (full tables in the harness output):

1. **Cross-turn `Final`s are real.** Voxtral does not endpoint on speaker
   change: it produced a 0.5 s A + 2.4 s B segment across `original`'s
   silence-padded boundary, and merged A+B+A audio into one 6.3 s `Final` in
   `alternate` (cross-turn rate 67% there). #70's premise — segments span
   turns — is **confirmed**.
2. **…but the dominant speaker still wins.** Wrong-name rate was 0/12
   overall and 0/4 on cross-turn segments. The nearest-speaker decision
   attributed every cross-turn blend to the speaker with the most audio in
   the window. #70's conclusion — that spanning causes misattribution — is
   **not observed** at this fixture's scale.
3. **Margins shrink on blends** (the #71 mechanism, visible without overlap):
   pure segments scored the winner at cosine 0.87–0.98 with margin ≈0.7–0.9;
   the A+B+A blend scored 0.727 vs 0.399 (margin 0.33). Blending pulls the
   scores together — with more even mixing (or true overlap) the decision
   would flip or drop below threshold. The debug score log
   (`RUST_LOG=omni_voice::voice::listen::speaker_gate=debug`) captures this
   per decision.
4. **The `misattr (strict)` numbers are dominated by short `unknown`
   trailers**, not name swaps: every variant ends its halves with a ~0.7 s
   "LibriVox.org." `Final` whose embedding matches neither speaker
   (cosines ≈0.05/0.10). Sub-second segments embed poorly — a threshold/
   min-length tuning question, not a diarization gap.
5. Caveats: both fixture halves are LibriVox intro announcements, so both
   speakers read nearly the same words — attribution here is voice-only,
   which is the hard case lexically but an easy case acoustically (clean
   audio, no overlap, distinct voices). One zero-length end-of-stream
   `Final` was excluded as unscorable.

## Dogfooding protocol (≥5 multi-speaker sessions by 2026-08-01)

Run real meetings/conversations with labelling and the score log enabled:

```bash
RUST_LOG=omni_voice::voice::listen::speaker_gate=debug \
  omni-voice listen --speaker me --speaker sabine \
  2> ~/spike80-session-N.log
```

After each session (transcript at
`~/.omni-voice/voice/<session-id>/transcript.jsonl`):

1. Read the transcript alongside memory of the conversation; hand-mark each
   `Final` whose audio spanned a speaker change (cross-turn) and each whose
   tag is wrong (misattributed) or `unknown`.
2. Check misattributed/`unknown` segments against the score log: was it a
   near-tie (small margin — blending/overlap) or a confident wrong answer?
3. Note every stretch where two people spoke simultaneously and whether
   those segments were the mislabelled ones (#71's overlap question).

| Date | Session | Dur | Finals | Cross-turn | Wrong-name | Unknown | Overlap notes |
|------|---------|-----|--------|------------|------------|---------|---------------|
|      |         |     |        |            |            |         |               |

## GO/NO-GO scaffold (for the #70/#71 comments)

- **#70 (diarization)** — the pre-committed rule keys on **real sessions**:
  GO if cross-turn `Final`s ≥10% of segments, or wrong-speaker tags >5% (or
  concentrated on cross-turn segments in a way that breaks "who said what");
  NO-GO otherwise. The controlled runs inform but do not trip it: cross-turn
  segments are common even on clean speech (premise holds) but
  dominant-speaker attribution absorbed all of them — 0% wrong-name. The
  dogfooding cross-turn and wrong-name columns decide. If errors stay
  confined to short/`unknown` segments → NO-GO, consider threshold/
  min-length tuning instead.
- **#71 (target-speaker extraction)** — decide on overlap incidence in real
  sessions plus the margin-collapse observation (#3 above). If simultaneous
  speech is frequent and correlates with mislabels/small margins → the
  blending premise holds in practice; otherwise NO-GO.

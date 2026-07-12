//! `omni-voice install-model` — one-time fetch of model artefacts.
//!
//! Supports three variants: `whisper-tiny.en` for the `whisper-candle` ASR
//! backend, `parakeet-tdt-0.6b-v2` for the `parakeet-tdt` backend (downloads
//! MLX weights, converts to candle via `scripts/convert_parakeet_weights.py`,
//! and synthesises a tokenizer), and `speaker-wespeaker-en` for the
//! speaker-embedding runtime added in #805 / ADR-0034. Files land in the
//! conventional install locations beneath `~/.omni-voice/voice/models/`.
//!
//! Bumps the model-download cost to install time rather than transcribe/
//! enrol time, so network failures surface explicitly when the user opts
//! in to installing rather than silently on first use.

use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use hf_hub::{api::sync::ApiBuilder, Repo, RepoType};
use sha1::Sha1;
use sha2::{Digest, Sha256};

use crate::voice::models::{
    ModelSource, ModelSpec, PARAKEET_TDT_0_6B_V2, SPEAKER_WESPEAKER_EN, VOXTRAL_MLX_INT4,
    WHISPER_TINY_EN,
};

/// Which model variant to install.
///
/// `--variant` defaults to `whisper-tiny.en` so bare
/// `install-model` continues to install the ASR model — the
/// pre-#805 behaviour.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Variant {
    /// OpenAI Whisper `tiny.en` (ADR-0033).
    #[default]
    #[value(name = "whisper-tiny.en")]
    WhisperTinyEn,
    /// NVIDIA Parakeet-TDT-0.6B-v2 — pure-Rust ASR (migrated from
    /// omni-dev #898). Downloads the MLX-format safetensors and runs the
    /// `scripts/convert_parakeet_weights.py` permute pass to produce the
    /// candle-friendly weights the backend loads.
    #[value(name = "parakeet-tdt-0.6b-v2")]
    ParakeetTdt06bV2,
    /// Voxtral-Mini-4B-Realtime INT4 (MLX) for the in-process `voxtral-mlx`
    /// backend (ADR-0039 / #27). A plain HuggingFace download (no converter).
    #[value(name = "voxtral-mlx-int4")]
    VoxtralMlxInt4,
    /// Wespeaker `resnet34_LM` English-only speaker embedding (ADR-0034).
    #[value(name = "speaker-wespeaker-en")]
    SpeakerWespeakerEn,
}

impl Variant {
    /// Returns the [`ModelSpec`] for this variant.
    pub fn spec(self) -> &'static ModelSpec {
        match self {
            Self::WhisperTinyEn => &WHISPER_TINY_EN,
            Self::ParakeetTdt06bV2 => &PARAKEET_TDT_0_6B_V2,
            Self::VoxtralMlxInt4 => &VOXTRAL_MLX_INT4,
            Self::SpeakerWespeakerEn => &SPEAKER_WESPEAKER_EN,
        }
    }
}

/// Downloads the model files for a chosen variant into the conventional
/// install location at `~/.omni-voice/voice/models/<variant-subdir>/` (or
/// `--dest` to override).
///
/// Idempotent: if every required file is already present and non-empty,
/// the command prints a "model already installed" line and exits 0 *without*
/// prompting. Pass `--force` to re-download anyway.
///
/// When bytes will actually be transferred, the command first prints the
/// source URL and expected size and prompts for confirmation on stdin (#14).
/// Bypass the prompt with `--accept-downloads` or `OMNI_VOICE_AUTO_DOWNLOAD=true`.
#[derive(Parser)]
pub struct InstallModelCommand {
    /// Override the install directory. Defaults to the variant's
    /// canonical location under `~/.omni-voice/voice/models/`.
    #[arg(long)]
    pub dest: Option<PathBuf>,

    /// Re-download even if all required files are already present.
    #[arg(long)]
    pub force: bool,

    /// Proceed with downloads without the interactive confirmation prompt.
    /// Also bypassable via `OMNI_VOICE_AUTO_DOWNLOAD=true` for
    /// non-interactive automation (CI).
    #[arg(long)]
    pub accept_downloads: bool,

    /// Which model variant to install. Defaults to `whisper-tiny.en`.
    #[arg(long, value_enum, default_value_t = Variant::WhisperTinyEn)]
    pub variant: Variant,
}

impl InstallModelCommand {
    /// Entry point. Writes user-facing progress to stderr so stdout stays
    /// reserved for machine-readable output (parity with `voice
    /// transcribe`'s JSONL pipe-detection convention). Resolves tty-ness and
    /// locks stdin/stderr here, keeping the prompt plumbing out of the
    /// testable core.
    pub fn execute(self) -> Result<()> {
        let stdin = std::io::stdin();
        let is_tty = stdin.is_terminal();
        let mut reader = stdin.lock();
        let mut err = std::io::stderr().lock();
        self.run(&mut reader, is_tty, &mut err)
    }

    /// Reader/writer-generic core, parameterised over stdin + stderr and an
    /// explicit `is_tty` flag so tests can drive the prompt, idempotency, and
    /// success paths without touching the global streams.
    fn run<R: BufRead, W: Write>(self, stdin: &mut R, is_tty: bool, w: &mut W) -> Result<()> {
        let spec = self.variant.spec();
        let dest = match self.dest {
            Some(p) => p,
            None => spec
                .default_dir()
                .ok_or_else(|| anyhow!("could not determine home directory; pass --dest <path>"))?,
        };

        // Idempotent skip never prompts — no bytes will be transferred.
        if !self.force && all_present(spec, &dest) {
            writeln!(w, "model already installed at {}", dest.display())?;
            return Ok(());
        }

        // Bytes will be transferred: gate on explicit consent (#14).
        match confirm_downloads(spec, &dest, self.accept_downloads, stdin, is_tty, w)? {
            Consent::Proceed => {}
            Consent::Declined => {
                writeln!(w, "Aborted.")?;
                return Ok(());
            }
        }

        // Parakeet has a different upstream file set than the install
        // dir's required_files (raw MLX safetensors vs. converted candle
        // safetensors), so it gets its own install path that shells out
        // to the Python converter after download.
        if matches!(self.variant, Variant::ParakeetTdt06bV2) {
            return install_parakeet(spec, &dest, w);
        }

        match spec.source {
            ModelSource::HfHub { repo_id, revision } => {
                download_hf_hub(spec, repo_id, revision, &dest, w)
            }
            ModelSource::HttpReleaseAsset { url, sha256, bytes } => {
                download_release_asset(spec, url, sha256, bytes, &dest, w)
            }
        }
    }
}

/// Outcome of the pre-download confirmation gate.
#[derive(Debug, PartialEq, Eq)]
enum Consent {
    Proceed,
    Declined,
}

/// Environment override that bypasses the download confirmation prompt for
/// non-interactive automation. Truthy values: `true` / `1`.
const AUTO_DOWNLOAD_ENV: &str = "OMNI_VOICE_AUTO_DOWNLOAD";

fn auto_download_env() -> bool {
    std::env::var(AUTO_DOWNLOAD_ENV)
        .is_ok_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "true" | "1"))
}

/// Confirms a model download before any bytes transfer.
///
/// Returns [`Consent::Proceed`] when `--accept-downloads` or
/// `OMNI_VOICE_AUTO_DOWNLOAD` bypasses the prompt, or when the user answers
/// yes at an interactive prompt; [`Consent::Declined`] on any other answer.
/// Prints the source URL and expected size first. When stdin is **not** a
/// terminal and no bypass was given, it bails loudly (pointing at the flag
/// and env var) rather than blocking forever on `read_line`.
fn confirm_downloads<R: BufRead, W: Write>(
    spec: &ModelSpec,
    dest: &Path,
    accept_downloads: bool,
    stdin: &mut R,
    is_tty: bool,
    w: &mut W,
) -> Result<Consent> {
    if accept_downloads || auto_download_env() {
        return Ok(Consent::Proceed);
    }

    let (url, size) = spec.download_summary();
    writeln!(
        w,
        "About to download the {} model ({size}) from:",
        spec.kind_label
    )?;
    writeln!(w, "  {url}")?;
    writeln!(w, "  into {}", dest.display())?;

    if !is_tty {
        bail!(
            "refusing to download without confirmation on a non-interactive stdin; \
             pass --accept-downloads or set {AUTO_DOWNLOAD_ENV}=true"
        );
    }

    write!(w, "Proceed? [y/N] ")?;
    w.flush()?;
    let mut line = String::new();
    stdin
        .read_line(&mut line)
        .context("read confirmation from stdin")?;
    if matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
        Ok(Consent::Proceed)
    } else {
        Ok(Consent::Declined)
    }
}

/// Files the Parakeet install pipeline pulls from the upstream HF repo
/// (distinct from the spec's `required_files`, which lists what the
/// backend expects in the dir *after* conversion). `tokenizer.json` is
/// NOT in this list because the upstream `mlx-community/parakeet-tdt-0.6b-v2`
/// repo doesn't ship one — the 1024-token BPE vocab is embedded in
/// `config.json` at `joint.vocabulary`. The install pipeline synthesises a
/// decode-only `tokenizer.json` from that vocab after download.
const PARAKEET_UPSTREAM_FILES: &[&str] = &["config.json", "model.safetensors"];

/// CC-BY-4.0 attribution written to the Parakeet model dir per the
/// issue #898 acceptance criterion.
const PARAKEET_ATTRIBUTION: &str = "\
NVIDIA Parakeet-TDT-0.6B-v2 (mlx-community/parakeet-tdt-0.6b-v2)
Licensed under CC-BY-4.0
https://creativecommons.org/licenses/by/4.0/

Source: https://huggingface.co/mlx-community/parakeet-tdt-0.6b-v2
Original model: NVIDIA Corporation
MLX port: senstella + mlx-community contributors
Candle port: omni-voice (migrated from omni-dev #898)
";

/// Parakeet install: download raw MLX safetensors from HF, run the Python
/// converter to produce `candle_weights.safetensors`, synthesise
/// `tokenizer.json` from the embedded vocab, and write the CC-BY-4.0
/// attribution. The converter call is a `std::process::Command` because it
/// lives in `scripts/` for iteration independent of the Rust release cycle.
fn install_parakeet<W: Write>(spec: &ModelSpec, dest: &Path, w: &mut W) -> Result<()> {
    let ModelSource::HfHub { repo_id, revision } = spec.source else {
        bail!(
            "internal error: Parakeet variant has non-HfHub source ({:?})",
            spec.source
        );
    };

    writeln!(
        w,
        "Installing {repo_id} (revision {revision}) -> {}",
        dest.display()
    )?;
    std::fs::create_dir_all(dest)
        .with_context(|| format!("create install directory at {}", dest.display()))?;

    let api = ApiBuilder::from_env()
        .build()
        .context("initialise HuggingFace Hub client")?;
    let repo = api.repo(Repo::with_revision(
        repo_id.to_string(),
        RepoType::Model,
        revision.to_string(),
    ));

    // Download upstream files (config.json, model.safetensors) into dest.
    for file in PARAKEET_UPSTREAM_FILES {
        let start = Instant::now();
        write!(w, "  fetching {file}... ")?;
        w.flush()?;
        let downloaded = repo.get(file).with_context(|| {
            format!(
                "download {file} from {repo_id} (revision {revision}). \
                 Check your network or set HTTPS_PROXY"
            )
        })?;
        let note = verify_hub_download(&downloaded)
            .with_context(|| format!("verify integrity of downloaded {file}"))?;
        let target = dest.join(file);
        atomic_install_copy(&downloaded, &target).with_context(|| {
            format!(
                "install {file} into {} (atomic rename failed)",
                target.display()
            )
        })?;
        let bytes = std::fs::metadata(&target).map_or(0, |m| m.len());
        writeln!(
            w,
            "done ({bytes} bytes in {:.1}s; {note})",
            start.elapsed().as_secs_f64()
        )?;
    }

    // Run the Python converter to produce candle_weights.safetensors.
    let src_safetensors = dest.join("model.safetensors");
    let out_safetensors = dest.join("candle_weights.safetensors");
    let converter =
        locate_parakeet_converter().context("locate scripts/convert_parakeet_weights.py")?;

    write!(w, "  converting weights via {}... ", converter.display())?;
    w.flush()?;
    let start = Instant::now();
    let status = std::process::Command::new(python_binary())
        .arg(&converter)
        .arg("--src")
        .arg(&src_safetensors)
        .arg("--out")
        .arg(&out_safetensors)
        .status()
        .context(
            "spawn python3 for converter. \
             Ensure python3 + numpy + safetensors are installed: \
             `pip install numpy safetensors`",
        )?;
    if !status.success() {
        bail!(
            "converter failed with exit code {:?}; see PARAKEET-CONVERT: log lines above",
            status.code()
        );
    }
    writeln!(w, "done ({:.1}s)", start.elapsed().as_secs_f64())?;

    // Delete the raw MLX safetensors to save ~2.47 GB — the converted
    // file is what the backend loads.
    if let Err(e) = std::fs::remove_file(&src_safetensors) {
        writeln!(
            w,
            "  warning: failed to delete raw {} ({e}); leaving in place",
            src_safetensors.display()
        )?;
    }

    // Synthesise tokenizer.json from config.json's embedded vocab.
    let config_path = dest.join("config.json");
    let tokenizer_path = dest.join("tokenizer.json");
    write!(w, "  writing tokenizer.json from config.json vocab... ")?;
    w.flush()?;
    write_tokenizer_json(&config_path, &tokenizer_path)
        .context("synthesise tokenizer.json from config.json vocab")?;
    let tok_bytes = std::fs::metadata(&tokenizer_path).map_or(0, |m| m.len());
    writeln!(w, "done ({tok_bytes} bytes)")?;

    // Write CC-BY-4.0 attribution.
    let attribution_path = dest.join("ATTRIBUTION.txt");
    atomic_install_bytes(PARAKEET_ATTRIBUTION.as_bytes(), &attribution_path)
        .context("write Parakeet ATTRIBUTION.txt")?;

    writeln!(
        w,
        "{} model installed at {}",
        spec.kind_label,
        dest.display()
    )?;
    Ok(())
}

/// Synthesises a decode-only HF `tokenizer.json` from the BPE vocab
/// embedded in Parakeet's `config.json` (`joint.vocabulary` — 1024 entries
/// for v2). Decode-only is sufficient: Parakeet's TDT joiner emits token
/// IDs and never tokenises input text. The shape follows the HuggingFace
/// `tokenizers` BPE schema with a `Metaspace` decoder (replacement `▁`,
/// the SentencePiece word-start marker) and empty `merges` (only needed
/// for *encoding*).
fn write_tokenizer_json(config_path: &Path, out_path: &Path) -> Result<()> {
    let cfg_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("read {}", config_path.display()))?;
    let cfg: serde_json::Value = serde_json::from_str(&cfg_text).context("parse config.json")?;
    let vocab_list = cfg["joint"]["vocabulary"]
        .as_array()
        .context("config.json: missing joint.vocabulary array")?;
    let vocab: serde_json::Map<String, serde_json::Value> = vocab_list
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let tok = t
                .as_str()
                .ok_or_else(|| anyhow!("vocab entry {i} is not a string"))?;
            Ok::<_, anyhow::Error>((tok.to_string(), serde_json::Value::from(i)))
        })
        .collect::<Result<_>>()?;
    let tok = serde_json::json!({
        "version": "1.0",
        "truncation": null,
        "padding": null,
        "added_tokens": [],
        "normalizer": null,
        "pre_tokenizer": null,
        "post_processor": null,
        "decoder": {
            "type": "Metaspace",
            "replacement": "\u{2581}",
            "prepend_scheme": "first",
            "split": true,
        },
        "model": {
            "type": "BPE",
            "dropout": null,
            "unk_token": "<unk>",
            "continuing_subword_prefix": null,
            "end_of_word_suffix": null,
            "fuse_unk": false,
            "byte_fallback": false,
            "ignore_merges": true,
            "vocab": vocab,
            "merges": [],
        },
    });
    let json_bytes = serde_json::to_vec(&tok).context("serialise tokenizer.json")?;
    atomic_install_bytes(&json_bytes, out_path)
        .with_context(|| format!("write tokenizer.json to {}", out_path.display()))
}

/// Returns the python3 binary to invoke. Honours the `PYTHON` env var so a
/// caller can pin a specific interpreter or point at a venv.
fn python_binary() -> std::ffi::OsString {
    std::env::var_os("PYTHON").unwrap_or_else(|| std::ffi::OsString::from("python3"))
}

/// Locates `scripts/convert_parakeet_weights.py` relative to the running
/// binary or the CWD. Honours `OMNI_VOICE_PARAKEET_CONVERTER` for explicit
/// overrides (test harness / non-standard installs).
fn locate_parakeet_converter() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("OMNI_VOICE_PARAKEET_CONVERTER") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Ok(path);
        }
        bail!(
            "OMNI_VOICE_PARAKEET_CONVERTER points at {} which is not a file",
            path.display()
        );
    }
    // Search candidates: CWD/scripts/, exe-dir/../scripts/, exe-dir/scripts/.
    let candidates = std::iter::once(PathBuf::from("scripts/convert_parakeet_weights.py"))
        .chain(std::env::current_exe().ok().and_then(|exe| {
            let dir = exe.parent()?;
            Some(dir.join("../scripts/convert_parakeet_weights.py"))
        }))
        .chain(std::env::current_exe().ok().and_then(|exe| {
            let dir = exe.parent()?;
            Some(dir.join("scripts/convert_parakeet_weights.py"))
        }));
    for c in candidates {
        if c.is_file() {
            return Ok(c);
        }
    }
    bail!(
        "could not find scripts/convert_parakeet_weights.py. \
         Set OMNI_VOICE_PARAKEET_CONVERTER=/path/to/convert_parakeet_weights.py \
         or run install-model from the omni-voice repo root"
    )
}

fn all_present(spec: &ModelSpec, dir: &Path) -> bool {
    spec.required_files_in(dir)
        .iter()
        .all(|p| p.is_file() && p.metadata().is_ok_and(|m| m.len() > 0))
}

fn download_hf_hub<W: Write>(
    spec: &ModelSpec,
    repo_id: &str,
    revision: &str,
    dest: &Path,
    w: &mut W,
) -> Result<()> {
    writeln!(
        w,
        "Installing {repo_id} (revision {revision}) -> {}",
        dest.display()
    )?;
    std::fs::create_dir_all(dest)
        .with_context(|| format!("create install directory at {}", dest.display()))?;

    // `from_env` honours the standard HF env vars: `HF_ENDPOINT` (alternate
    // hub host — also how the hermetic tests below point this at a local
    // mock) and `HF_HOME` (cache location). Both default to the usual
    // huggingface.co endpoint and `~/.cache/huggingface`.
    let api = ApiBuilder::from_env()
        .build()
        .context("initialise HuggingFace Hub client")?;
    let repo = api.repo(Repo::with_revision(
        repo_id.to_string(),
        RepoType::Model,
        revision.to_string(),
    ));

    for file in spec.required_files {
        let start = Instant::now();
        write!(w, "  fetching {file}... ")?;
        w.flush()?;
        let downloaded = repo.get(file).with_context(|| {
            format!(
                "download {file} from {repo_id} (revision {revision}). \
                 Check your network or set HTTPS_PROXY"
            )
        })?;
        // Verify integrity against hf-hub's own content identity *before*
        // installing, so a corrupt/truncated download never lands at `dest`.
        let note = verify_hub_download(&downloaded)
            .with_context(|| format!("verify integrity of downloaded {file}"))?;
        let target = dest.join(file);
        atomic_install_copy(&downloaded, &target).with_context(|| {
            format!(
                "install {file} into {} (atomic rename failed)",
                target.display()
            )
        })?;
        let bytes = std::fs::metadata(&target).map_or(0, |m| m.len());
        writeln!(
            w,
            "done ({bytes} bytes in {:.1}s; {note})",
            start.elapsed().as_secs_f64()
        )?;
    }

    writeln!(
        w,
        "{} model installed at {}",
        spec.kind_label,
        dest.display()
    )?;
    Ok(())
}

/// Verifies an hf-hub-cached file against the integrity hash hf-hub stored
/// it under. hf-hub writes each file to `blobs/<etag>` and symlinks the
/// snapshot path to it, where the etag is the file's HuggingFace content
/// identity: the **SHA-256** LFS OID for large (LFS) files — the only place a
/// silent bad install could ship subtly-wrong weights — and the **git blob
/// SHA-1** for small non-LFS files (`config.json`, `tekken.json`, …).
///
/// Returns a short status note for the caller's progress line. Hashes are
/// computed streaming (64 KiB chunks) so a multi-GB weight file is never read
/// into memory. Mirrors the bail-on-mismatch contract of
/// [`download_release_asset`].
///
/// If the recovered blob name is neither a SHA-256 nor a git-SHA-1 hex string
/// (e.g. hf-hub symlinks disabled, or a hub mock with a synthetic etag),
/// verification degrades to a skip rather than failing — the strong guarantee
/// covers every real hub blob, which is content-addressed by one of the two.
fn verify_hub_download(cached: &Path) -> Result<&'static str> {
    let blob = std::fs::canonicalize(cached)
        .with_context(|| format!("resolve hf-hub cache blob for {}", cached.display()))?;
    let etag = blob
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    if is_sha256_hex(etag) {
        let got = sha256_file(&blob)
            .with_context(|| format!("hash {} for SHA-256 verification", blob.display()))?;
        if !got.eq_ignore_ascii_case(etag) {
            bail!(
                "SHA-256 mismatch for {}: expected {etag}, got {got}",
                cached.display()
            );
        }
        Ok("sha256 verified")
    } else if is_git_sha1_hex(etag) {
        let got = git_blob_sha1_file(&blob)
            .with_context(|| format!("hash {} for git SHA-1 verification", blob.display()))?;
        if !got.eq_ignore_ascii_case(etag) {
            bail!(
                "git SHA-1 mismatch for {}: expected {etag}, got {got}",
                cached.display()
            );
        }
        Ok("git-sha1 verified")
    } else {
        Ok("integrity check skipped")
    }
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_git_sha1_hex(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Lowercase hex of a finalised RustCrypto digest.
fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    let digest = digest.as_ref();
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        // `write!` into a `String` is infallible.
        let _ = write!(&mut hex, "{byte:02x}");
    }
    hex
}

/// SHA-256 of an in-memory buffer (hex).
fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex_digest(hasher.finalize())
}

/// Streaming SHA-256 of a file's contents (hex).
fn sha256_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_digest(hasher.finalize()))
}

/// Streaming git blob SHA-1 (`sha1("blob {len}\0" + content)`) of a file —
/// the OID HuggingFace reports as the etag for small, non-LFS files.
fn git_blob_sha1_file(path: &Path) -> Result<String> {
    let len = std::fs::metadata(path)
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    let mut file = std::fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut hasher = Sha1::new();
    hasher.update(format!("blob {len}\0").as_bytes());
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .with_context(|| format!("read {}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_digest(hasher.finalize()))
}

fn download_release_asset<W: Write>(
    spec: &ModelSpec,
    url: &str,
    expected_sha256: &str,
    expected_bytes: u64,
    dest: &Path,
    w: &mut W,
) -> Result<()> {
    // Wespeaker (and any future single-asset release-driven model) ships
    // exactly one file. The check is defensive: if a future spec mis-
    // declares N!=1 with HttpReleaseAsset, fail loudly rather than
    // silently install only the first.
    if spec.required_files.len() != 1 {
        bail!(
            "HttpReleaseAsset source expects exactly one required_file, \
             got {} for variant {}",
            spec.required_files.len(),
            spec.variant
        );
    }
    let file_name = spec.required_files[0];
    let target = dest.join(file_name);

    writeln!(
        w,
        "Installing {file_name} ({expected_bytes} B) -> {}",
        dest.display()
    )?;
    std::fs::create_dir_all(dest)
        .with_context(|| format!("create install directory at {}", dest.display()))?;

    let start = Instant::now();
    write!(w, "  fetching {url}... ")?;
    w.flush()?;

    // ureq turns non-2xx statuses into `Err` by default, which would make
    // the explicit status check below unreachable; disable that so the
    // bail with the URL and canonical reason is the single status-error
    // path, leaving `call()` errors to mean transport failures only.
    let resp = ureq::get(url)
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .with_context(|| format!("HTTP GET {url}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!(
            "HTTP {} fetching {url}: {}",
            status.as_u16(),
            status.canonical_reason().unwrap_or("Unknown"),
        );
    }
    let bytes = resp
        .into_body()
        .read_to_vec()
        .with_context(|| format!("read response body for {url}"))?;

    let actual_sha = sha256_hex(&bytes);
    if !actual_sha.eq_ignore_ascii_case(expected_sha256) {
        bail!("SHA-256 mismatch for {file_name}: expected {expected_sha256}, got {actual_sha}");
    }

    atomic_install_bytes(&bytes, &target).with_context(|| {
        format!(
            "install {file_name} into {} (atomic rename failed)",
            target.display()
        )
    })?;
    writeln!(
        w,
        "done ({} bytes in {:.1}s; sha256 verified)",
        bytes.len(),
        start.elapsed().as_secs_f64()
    )?;
    writeln!(
        w,
        "{} model installed at {}",
        spec.kind_label,
        dest.display()
    )?;
    Ok(())
}

/// Writes `bytes` to a `.part` sibling of `to`, then atomically renames
/// so a partial download never leaves a half-written file at `to`.
fn atomic_install_bytes(bytes: &[u8], to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir {}", parent.display()))?;
    }
    let tmp = part_sibling(to)?;
    std::fs::write(&tmp, bytes)
        .with_context(|| format!("write {} bytes -> {}", bytes.len(), tmp.display()))?;
    std::fs::rename(&tmp, to)
        .with_context(|| format!("rename {} -> {}", tmp.display(), to.display()))?;
    Ok(())
}

/// Copies `from` into `to` via a temp file sibling + rename so a partial
/// download never leaves a half-written file at the destination.
fn atomic_install_copy(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir {}", parent.display()))?;
    }
    let tmp = part_sibling(to)?;
    std::fs::copy(from, &tmp)
        .with_context(|| format!("copy {} -> {}", from.display(), tmp.display()))?;
    std::fs::rename(&tmp, to)
        .with_context(|| format!("rename {} -> {}", tmp.display(), to.display()))?;
    Ok(())
}

fn part_sibling(to: &Path) -> Result<PathBuf> {
    let file_name = to
        .file_name()
        .ok_or_else(|| anyhow!("destination path has no file name: {}", to.display()))?;
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(file_name);
    tmp_name.push(".part");
    Ok(to.with_file_name(tmp_name))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::voice::models::REQUIRED_FILES;
    // These tests mutate HOME (and other env vars), read by `home_dir()`
    // across many modules — so they serialise on the crate-wide env lock,
    // not a module-local one, to avoid cross-module races (issue #12).
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        crate::test_support::env::env_lock()
    }

    fn stage_complete_whisper_model(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for f in REQUIRED_FILES {
            std::fs::write(dir.join(f), b"placeholder").unwrap();
        }
    }

    fn stage_complete_speaker_model(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        for f in SPEAKER_WESPEAKER_EN.required_files {
            std::fs::write(dir.join(f), b"placeholder").unwrap();
        }
    }

    #[test]
    fn idempotent_when_all_files_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        stage_complete_whisper_model(tmp.path());

        let cmd = InstallModelCommand {
            dest: Some(tmp.path().to_path_buf()),
            force: false,
            accept_downloads: false,
            variant: Variant::WhisperTinyEn,
        };
        let mut out: Vec<u8> = Vec::new();
        cmd.run(&mut std::io::empty(), false, &mut out).unwrap();
        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("already installed"), "got: {msg}");
    }

    #[test]
    fn idempotent_when_speaker_model_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        stage_complete_speaker_model(tmp.path());

        let cmd = InstallModelCommand {
            dest: Some(tmp.path().to_path_buf()),
            force: false,
            accept_downloads: false,
            variant: Variant::SpeakerWespeakerEn,
        };
        let mut out: Vec<u8> = Vec::new();
        cmd.run(&mut std::io::empty(), false, &mut out).unwrap();
        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("already installed"), "got: {msg}");
    }

    #[test]
    fn idempotent_skip_treats_zero_byte_file_as_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path()).unwrap();
        for f in REQUIRED_FILES {
            // Zero-byte file would normally pass `is_file()` but is_not
            // a valid model artifact; the idempotency check must reject it.
            std::fs::write(tmp.path().join(f), b"").unwrap();
        }
        assert!(!all_present(&WHISPER_TINY_EN, tmp.path()));
    }

    #[test]
    fn atomic_install_copy_replaces_target() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::write(&src, b"hello").unwrap();
        std::fs::write(&dst, b"old").unwrap();
        atomic_install_copy(&src, &dst).unwrap();
        let got = std::fs::read(&dst).unwrap();
        assert_eq!(got, b"hello");
        // No leftover temp file.
        let leftover = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().ends_with(".part"));
        assert!(!leftover, "atomic_install_copy must not leave .part files");
    }

    #[test]
    fn atomic_install_bytes_writes_and_renames() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dst = tmp.path().join("out");
        atomic_install_bytes(b"hello", &dst).unwrap();
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello");
        let leftover = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(Result::ok)
            .any(|e| e.file_name().to_string_lossy().ends_with(".part"));
        assert!(!leftover, "atomic_install_bytes must not leave .part files");
    }

    #[test]
    fn sha256_file_matches_in_memory_hash() {
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("f");
        std::fs::write(&p, b"weights-bytes").unwrap();
        assert_eq!(sha256_file(&p).unwrap(), sha256_hex(b"weights-bytes"));
    }

    #[test]
    fn git_blob_sha1_matches_git_hash_object() {
        // Reference value from `printf 'test content\n' | git hash-object --stdin`
        // (the Pro Git book example): blob OID for the 13-byte content.
        let tmp = tempfile::TempDir::new().unwrap();
        let p = tmp.path().join("f");
        std::fs::write(&p, b"test content\n").unwrap();
        assert_eq!(
            git_blob_sha1_file(&p).unwrap(),
            "d670460b4b4aece5915caf5c68d12f560a9fe3e4"
        );
    }

    #[test]
    fn is_hex_helpers_classify_by_length() {
        assert!(is_sha256_hex(&"a".repeat(64)));
        assert!(!is_sha256_hex(&"a".repeat(40)));
        assert!(is_git_sha1_hex(&"a".repeat(40)));
        assert!(!is_git_sha1_hex(&"a".repeat(64)));
        // hf-hub mock etags like "etag-config.json" classify as neither,
        // so verification degrades to a skip.
        assert!(!is_sha256_hex("etag-config.json"));
        assert!(!is_git_sha1_hex("etag-config.json"));
    }

    #[test]
    fn parses_no_args() {
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            c: InstallModelCommand,
        }
        let t = T::try_parse_from(["test"]).unwrap();
        assert!(t.c.dest.is_none());
        assert!(!t.c.force);
        assert_eq!(t.c.variant, Variant::WhisperTinyEn);
    }

    #[test]
    fn parses_dest_and_force() {
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            c: InstallModelCommand,
        }
        let t = T::try_parse_from(["test", "--dest", "/opt/x", "--force"]).unwrap();
        assert_eq!(t.c.dest.as_deref(), Some(Path::new("/opt/x")));
        assert!(t.c.force);
    }

    #[test]
    fn parses_accept_downloads() {
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            c: InstallModelCommand,
        }
        let default = T::try_parse_from(["test"]).unwrap();
        assert!(!default.c.accept_downloads);
        let t = T::try_parse_from(["test", "--accept-downloads"]).unwrap();
        assert!(t.c.accept_downloads);
    }

    #[test]
    fn parses_speaker_variant() {
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            c: InstallModelCommand,
        }
        let t = T::try_parse_from(["test", "--variant", "speaker-wespeaker-en"]).unwrap();
        assert_eq!(t.c.variant, Variant::SpeakerWespeakerEn);
    }

    #[test]
    fn parses_whisper_variant_explicit() {
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            c: InstallModelCommand,
        }
        let t = T::try_parse_from(["test", "--variant", "whisper-tiny.en"]).unwrap();
        assert_eq!(t.c.variant, Variant::WhisperTinyEn);
    }

    #[test]
    fn rejects_unknown_variant() {
        #[derive(Parser)]
        struct T {
            #[command(flatten)]
            c: InstallModelCommand,
        }
        let err = T::try_parse_from(["test", "--variant", "klingon"]);
        assert!(err.is_err(), "unknown variant should fail to parse");
    }

    #[test]
    fn run_with_dest_none_resolves_default_install_dir_from_home() {
        // Covers the `match self.dest { None => spec.default_dir()… }`
        // arm — the priority-3 path that the explicit-dest tests skip.
        // We stage the model files at the default location *under a
        // tempdir HOME* so the idempotent branch returns Ok and we
        // never touch the network or the real user's home.
        let _g = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        let default_dir = WHISPER_TINY_EN.default_dir().unwrap();
        stage_complete_whisper_model(&default_dir);

        let cmd = InstallModelCommand {
            dest: None,
            force: false,
            accept_downloads: false,
            variant: Variant::WhisperTinyEn,
        };
        let mut out: Vec<u8> = Vec::new();
        let result = cmd.run(&mut std::io::empty(), false, &mut out);

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        result.unwrap();
        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("already installed"), "got: {msg}");
        assert!(
            msg.contains("whisper-tiny.en"),
            "expected resolved default dir in message, got: {msg}"
        );
    }

    #[test]
    fn run_speaker_variant_with_dest_none_resolves_default() {
        let _g = env_guard();
        let tmp = tempfile::TempDir::new().unwrap();
        let prev_home = std::env::var_os("HOME");
        std::env::set_var("HOME", tmp.path());

        let default_dir = SPEAKER_WESPEAKER_EN.default_dir().unwrap();
        stage_complete_speaker_model(&default_dir);

        let cmd = InstallModelCommand {
            dest: None,
            force: false,
            accept_downloads: false,
            variant: Variant::SpeakerWespeakerEn,
        };
        let mut out: Vec<u8> = Vec::new();
        let result = cmd.run(&mut std::io::empty(), false, &mut out);

        match prev_home {
            Some(v) => std::env::set_var("HOME", v),
            None => std::env::remove_var("HOME"),
        }

        result.unwrap();
        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("already installed"), "got: {msg}");
        assert!(
            msg.contains("wespeaker-en-voxceleb-resnet34-LM"),
            "expected resolved default dir in message, got: {msg}"
        );
    }

    #[test]
    fn variant_spec_returns_correct_spec() {
        assert_eq!(
            Variant::WhisperTinyEn.spec().variant,
            WHISPER_TINY_EN.variant
        );
        assert_eq!(
            Variant::SpeakerWespeakerEn.spec().variant,
            SPEAKER_WESPEAKER_EN.variant
        );
        assert_eq!(
            Variant::ParakeetTdt06bV2.spec().variant,
            PARAKEET_TDT_0_6B_V2.variant
        );
        assert_eq!(
            Variant::VoxtralMlxInt4.spec().variant,
            VOXTRAL_MLX_INT4.variant
        );
    }

    // ── Download confirmation gate (#14) ─────────────────────────────────

    #[test]
    fn confirm_proceeds_with_accept_flag() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut out: Vec<u8> = Vec::new();
        // `is_tty=false` must not matter once the flag bypasses the prompt.
        let outcome = confirm_downloads(
            &WHISPER_TINY_EN,
            tmp.path(),
            true,
            &mut std::io::empty(),
            false,
            &mut out,
        )
        .unwrap();
        assert_eq!(outcome, Consent::Proceed);
        assert!(out.is_empty(), "accepted download should not prompt");
    }

    #[test]
    fn confirm_proceeds_with_env_var() {
        let _g = env_guard();
        let prev = std::env::var_os(AUTO_DOWNLOAD_ENV);
        std::env::set_var(AUTO_DOWNLOAD_ENV, "true");
        let tmp = tempfile::TempDir::new().unwrap();
        let mut out: Vec<u8> = Vec::new();
        let outcome = confirm_downloads(
            &WHISPER_TINY_EN,
            tmp.path(),
            false,
            &mut std::io::empty(),
            false,
            &mut out,
        );
        match prev {
            Some(v) => std::env::set_var(AUTO_DOWNLOAD_ENV, v),
            None => std::env::remove_var(AUTO_DOWNLOAD_ENV),
        }
        assert_eq!(outcome.unwrap(), Consent::Proceed);
    }

    #[test]
    fn confirm_prompts_and_proceeds_on_yes() {
        let _g = env_guard();
        std::env::remove_var(AUTO_DOWNLOAD_ENV);
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reader = std::io::Cursor::new(b"y\n".to_vec());
        let mut out: Vec<u8> = Vec::new();
        let outcome = confirm_downloads(
            &WHISPER_TINY_EN,
            tmp.path(),
            false,
            &mut reader,
            true,
            &mut out,
        )
        .unwrap();
        assert_eq!(outcome, Consent::Proceed);
        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("About to download"), "got: {msg}");
        assert!(
            msg.contains("huggingface.co/openai/whisper-tiny.en"),
            "summary must show the source URL, got: {msg}"
        );
        assert!(msg.contains("Proceed? [y/N]"), "got: {msg}");
    }

    #[test]
    fn confirm_declines_on_no() {
        let _g = env_guard();
        std::env::remove_var(AUTO_DOWNLOAD_ENV);
        let tmp = tempfile::TempDir::new().unwrap();
        let mut reader = std::io::Cursor::new(b"n\n".to_vec());
        let mut out: Vec<u8> = Vec::new();
        let outcome = confirm_downloads(
            &WHISPER_TINY_EN,
            tmp.path(),
            false,
            &mut reader,
            true,
            &mut out,
        )
        .unwrap();
        assert_eq!(outcome, Consent::Declined);
    }

    #[test]
    fn confirm_bails_on_non_tty_without_bypass() {
        let _g = env_guard();
        std::env::remove_var(AUTO_DOWNLOAD_ENV);
        let tmp = tempfile::TempDir::new().unwrap();
        let mut out: Vec<u8> = Vec::new();
        let err = confirm_downloads(
            &WHISPER_TINY_EN,
            tmp.path(),
            false,
            &mut std::io::empty(),
            false,
            &mut out,
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--accept-downloads"), "got: {msg}");
        assert!(msg.contains(AUTO_DOWNLOAD_ENV), "got: {msg}");
    }

    #[test]
    fn run_aborts_on_declined_prompt_without_downloading() {
        // Not idempotent (dest is empty) so the gate fires; declining must
        // print "Aborted.", touch the network/filesystem not at all, and
        // still exit Ok.
        let _g = env_guard();
        std::env::remove_var(AUTO_DOWNLOAD_ENV);
        let tmp = tempfile::TempDir::new().unwrap();
        let dest = tmp.path().join("models");
        let cmd = InstallModelCommand {
            dest: Some(dest.clone()),
            force: false,
            accept_downloads: false,
            variant: Variant::WhisperTinyEn,
        };
        let mut reader = std::io::Cursor::new(b"n\n".to_vec());
        let mut out: Vec<u8> = Vec::new();
        cmd.run(&mut reader, true, &mut out).unwrap();
        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("Aborted."), "got: {msg}");
        assert!(
            !dest.join("config.json").exists(),
            "declined download must not install anything"
        );
    }

    // ── Download paths (hermetic, via wiremock) ──────────────────────────
    //
    // These cover the network-touching functions that used to be exercised
    // only by CI's real Whisper install step (made coverage-neutral in
    // a608ab3). `download_release_asset` takes its URL as an argument, so
    // tests point it straight at a local mock. `download_hf_hub` builds
    // its client from the environment, so tests set `HF_ENDPOINT` /
    // `HF_HOME` (serialised through `ENV_GUARD`).

    use wiremock::matchers::{method, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Mounts a mock that satisfies both hf-hub requests for `file`: the
    /// metadata probe (`Range: bytes=0-0`, reads `etag` / `x-repo-commit` /
    /// `Content-Range` headers) and the actual download (reads the body).
    ///
    /// The etag is set to the body's real SHA-256 so hf-hub names the cache
    /// blob by that hash — exercising `verify_hub_download`'s LFS path on the
    /// happy path. (Real hub LFS files are content-addressed identically.)
    async fn mount_hub_file(server: &MockServer, file: &str, body: &[u8]) {
        mount_hub_file_with_etag(server, file, body, &sha256_hex(body)).await;
    }

    /// As [`mount_hub_file`] but with an explicit `etag`, so a test can serve
    /// a blob whose stored hash disagrees with its bytes (corruption).
    async fn mount_hub_file_with_etag(server: &MockServer, file: &str, body: &[u8], etag: &str) {
        Mock::given(method("GET"))
            .and(path_regex(format!(r"/{}$", regex_escape(file))))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", etag)
                    .insert_header("x-repo-commit", "0123456789abcdef")
                    .insert_header(
                        "content-range",
                        format!("bytes 0-0/{}", body.len()).as_str(),
                    )
                    .set_body_bytes(body),
            )
            .mount(server)
            .await;
    }

    fn regex_escape(s: &str) -> String {
        s.chars()
            .flat_map(|c| {
                if c.is_ascii_alphanumeric() {
                    vec![c]
                } else {
                    vec!['\\', c]
                }
            })
            .collect()
    }

    /// Runs `f` with `HF_ENDPOINT` and `HF_HOME` pointing at the mock hub
    /// and a temp cache, restoring the previous values afterwards.
    fn with_hf_env<T>(endpoint: &str, hf_home: &Path, f: impl FnOnce() -> T) -> T {
        let _g = env_guard();
        let prev_endpoint = std::env::var_os("HF_ENDPOINT");
        let prev_home = std::env::var_os("HF_HOME");
        std::env::set_var("HF_ENDPOINT", endpoint);
        std::env::set_var("HF_HOME", hf_home);

        let result = f();

        match prev_endpoint {
            Some(v) => std::env::set_var("HF_ENDPOINT", v),
            None => std::env::remove_var("HF_ENDPOINT"),
        }
        match prev_home {
            Some(v) => std::env::set_var("HF_HOME", v),
            None => std::env::remove_var("HF_HOME"),
        }
        result
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_installs_whisper_model_from_hf_endpoint() {
        let server = MockServer::start().await;
        let files: &[(&str, &[u8])] = &[
            ("config.json", b"{\"cfg\":1}"),
            ("tokenizer.json", b"{\"tok\":2}"),
            ("model.safetensors", b"weights-bytes"),
        ];
        for (file, body) in files {
            mount_hub_file(&server, file, body).await;
        }

        let endpoint = server.uri();
        let (out, dest) = tokio::task::spawn_blocking(move || {
            let hf_home = tempfile::TempDir::new().unwrap();
            let dest = tempfile::TempDir::new().unwrap();
            let mut out: Vec<u8> = Vec::new();
            with_hf_env(&endpoint, hf_home.path(), || {
                let cmd = InstallModelCommand {
                    dest: Some(dest.path().to_path_buf()),
                    force: false,
                    accept_downloads: true,
                    variant: Variant::WhisperTinyEn,
                };
                cmd.run(&mut std::io::empty(), false, &mut out).unwrap();
            });
            (out, dest)
        })
        .await
        .unwrap();

        for (file, body) in files {
            let got = std::fs::read(dest.path().join(file)).unwrap();
            assert_eq!(&got, body, "installed content mismatch for {file}");
        }
        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("Whisper model installed at"), "got: {msg}");
        // Each file's etag is its real SHA-256, so the LFS integrity path
        // runs and reports success for every fetched file.
        assert_eq!(
            msg.matches("sha256 verified").count(),
            files.len(),
            "every fetched file should be sha256-verified; got: {msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_hf_hub_rejects_corrupt_file() {
        // The hub reports a (valid-shaped) SHA-256 etag that disagrees with
        // the body it serves — the corruption hf-hub itself would not catch.
        // `verify_hub_download` must bail and leave nothing installed.
        let server = MockServer::start().await;
        let body: &[u8] = b"weights-bytes";
        let wrong_sha = "0".repeat(64);
        mount_hub_file_with_etag(&server, "config.json", body, &wrong_sha).await;

        let endpoint = server.uri();
        let (err, dest) = tokio::task::spawn_blocking(move || {
            let hf_home = tempfile::TempDir::new().unwrap();
            let dest = tempfile::TempDir::new().unwrap();
            let mut out: Vec<u8> = Vec::new();
            let err = with_hf_env(&endpoint, hf_home.path(), || {
                download_hf_hub(&WHISPER_TINY_EN, "test/repo", "main", dest.path(), &mut out)
                    .unwrap_err()
            });
            (err, dest)
        })
        .await
        .unwrap();

        assert!(
            format!("{err:#}").contains("SHA-256 mismatch"),
            "got: {err:#}"
        );
        assert!(
            !dest.path().join("config.json").exists(),
            "corrupt download must not be installed"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_hf_hub_error_names_failing_file() {
        // No mocks mounted: every request 404s, so the first required
        // file's metadata probe fails and the context must say which
        // file could not be downloaded.
        let server = MockServer::start().await;

        let endpoint = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let hf_home = tempfile::TempDir::new().unwrap();
            let dest = tempfile::TempDir::new().unwrap();
            let mut out: Vec<u8> = Vec::new();
            with_hf_env(&endpoint, hf_home.path(), || {
                download_hf_hub(&WHISPER_TINY_EN, "test/repo", "main", dest.path(), &mut out)
            })
        })
        .await
        .unwrap()
        .unwrap_err();

        let msg = format!("{err:#}");
        assert!(
            msg.contains("download config.json from test/repo"),
            "got: {msg}"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_release_asset_installs_and_verifies_sha() {
        let body: &[u8] = b"onnx-model-bytes";
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;

        let url = format!("{}/asset.onnx", server.uri());
        let sha = sha256_hex(body);
        let (result, out, dest) = tokio::task::spawn_blocking(move || {
            let dest = tempfile::TempDir::new().unwrap();
            let mut out: Vec<u8> = Vec::new();
            let result = download_release_asset(
                &SPEAKER_WESPEAKER_EN,
                &url,
                &sha,
                body.len() as u64,
                dest.path(),
                &mut out,
            );
            (result, out, dest)
        })
        .await
        .unwrap();

        result.unwrap();
        let target = dest.path().join(SPEAKER_WESPEAKER_EN.required_files[0]);
        assert_eq!(std::fs::read(&target).unwrap(), body);
        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("sha256 verified"), "got: {msg}");
        assert!(msg.contains("Speaker model installed at"), "got: {msg}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_release_asset_rejects_sha_mismatch() {
        let body: &[u8] = b"onnx-model-bytes";
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;

        let url = format!("{}/asset.onnx", server.uri());
        let (err, dest) = tokio::task::spawn_blocking(move || {
            let dest = tempfile::TempDir::new().unwrap();
            let mut out: Vec<u8> = Vec::new();
            let err = download_release_asset(
                &SPEAKER_WESPEAKER_EN,
                &url,
                &"0".repeat(64),
                body.len() as u64,
                dest.path(),
                &mut out,
            )
            .unwrap_err();
            (err, dest)
        })
        .await
        .unwrap();

        assert!(err.to_string().contains("SHA-256 mismatch"), "got: {err:#}");
        let target = dest.path().join(SPEAKER_WESPEAKER_EN.required_files[0]);
        assert!(
            !target.exists(),
            "mismatched download must not be installed"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn download_release_asset_reports_http_status() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let url = format!("{}/asset.onnx", server.uri());
        let err = tokio::task::spawn_blocking(move || {
            let dest = tempfile::TempDir::new().unwrap();
            let mut out: Vec<u8> = Vec::new();
            download_release_asset(
                &SPEAKER_WESPEAKER_EN,
                &url,
                &"0".repeat(64),
                0,
                dest.path(),
                &mut out,
            )
            .unwrap_err()
        })
        .await
        .unwrap();

        assert!(
            err.to_string().contains("HTTP 404"),
            "expected status in error, got: {err:#}"
        );
    }

    #[test]
    fn download_release_asset_rejects_multi_file_spec() {
        // WHISPER_TINY_EN declares three required files; the defensive
        // single-asset check must fail before any network or filesystem IO.
        let tmp = tempfile::TempDir::new().unwrap();
        let mut out: Vec<u8> = Vec::new();
        let err = download_release_asset(
            &WHISPER_TINY_EN,
            "http://unused.invalid/asset",
            "00",
            0,
            tmp.path(),
            &mut out,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("expects exactly one required_file"),
            "got: {err:#}"
        );
    }

    #[test]
    fn atomic_install_bytes_fails_when_parent_is_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let err = atomic_install_bytes(b"data", &blocker.join("out")).unwrap_err();
        assert!(
            format!("{err:#}").contains("create parent dir"),
            "got: {err:#}"
        );
    }

    #[test]
    fn atomic_install_copy_fails_when_parent_is_file() {
        let tmp = tempfile::TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::write(&src, b"data").unwrap();
        let blocker = tmp.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let err = atomic_install_copy(&src, &blocker.join("out")).unwrap_err();
        assert!(
            format!("{err:#}").contains("create parent dir"),
            "got: {err:#}"
        );
    }

    #[test]
    fn write_tokenizer_json_emits_loadable_decode_only_tokenizer() {
        // Stage a minimal config.json with a 4-token vocab including the
        // SentencePiece word-start marker ▁ (U+2581).
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        let tok = tmp.path().join("tokenizer.json");
        std::fs::write(
            &cfg,
            r#"{"joint": {"vocabulary": ["<unk>", "▁the", "▁cat", "s"]}}"#,
        )
        .unwrap();

        write_tokenizer_json(&cfg, &tok).unwrap();

        // The synthesised file must load through the same `tokenizers`
        // crate path the backend uses.
        let loaded = tokenizers::Tokenizer::from_file(&tok).expect("tokenizer.json must load");
        assert_eq!(loaded.get_vocab_size(false), 4);
        // Metaspace decoder strips ▁ and joins with spaces, so
        // [1, 2, 3] → "the cats".
        let text = loaded.decode(&[1_u32, 2, 3], false).unwrap();
        assert_eq!(text, "the cats");
    }

    #[test]
    fn write_tokenizer_json_errors_when_vocab_missing() {
        let tmp = tempfile::TempDir::new().unwrap();
        let cfg = tmp.path().join("config.json");
        let tok = tmp.path().join("tokenizer.json");
        std::fs::write(&cfg, r#"{"joint": {}}"#).unwrap();
        let Err(err) = write_tokenizer_json(&cfg, &tok) else {
            panic!("expected missing-vocab error");
        };
        let msg = format!("{err:#}");
        assert!(msg.contains("missing joint.vocabulary"), "got: {msg}");
    }

    // ── Parakeet install pipeline ────────────────────────────────────────
    //
    // `install_parakeet` downloads from HF, shells out to the Python
    // converter, then synthesises the tokenizer + attribution. These tests
    // mock the hub (via `with_hf_env` + wiremock) and stub the converter
    // through `PYTHON` + `OMNI_VOICE_PARAKEET_CONVERTER` — a tiny `/bin/sh`
    // script stands in for `python3 convert_parakeet_weights.py`, so no
    // network, python, numpy, or the 2.47 GB model is required.

    #[test]
    fn python_binary_honours_env_and_defaults() {
        let _g = env_guard();
        let prev = std::env::var_os("PYTHON");
        std::env::remove_var("PYTHON");
        assert_eq!(python_binary(), std::ffi::OsString::from("python3"));
        std::env::set_var("PYTHON", "/usr/bin/python3.12");
        assert_eq!(
            python_binary(),
            std::ffi::OsString::from("/usr/bin/python3.12")
        );
        match prev {
            Some(v) => std::env::set_var("PYTHON", v),
            None => std::env::remove_var("PYTHON"),
        }
    }

    #[test]
    fn locate_parakeet_converter_env_override_must_be_a_file() {
        let _g = env_guard();
        let prev = std::env::var_os("OMNI_VOICE_PARAKEET_CONVERTER");
        std::env::set_var("OMNI_VOICE_PARAKEET_CONVERTER", "/nope/not/a/file.py");
        let Err(err) = locate_parakeet_converter() else {
            panic!("non-file override should error");
        };
        assert!(msg_is_not_a_file(&err), "got: {err:#}");
        match prev {
            Some(v) => std::env::set_var("OMNI_VOICE_PARAKEET_CONVERTER", v),
            None => std::env::remove_var("OMNI_VOICE_PARAKEET_CONVERTER"),
        }
    }

    fn msg_is_not_a_file(err: &anyhow::Error) -> bool {
        format!("{err:#}").contains("which is not a file")
    }

    #[test]
    fn locate_parakeet_converter_falls_back_to_repo_script() {
        // With no override, the default search finds the script shipped at
        // the repo root (`cargo test` runs with CWD = crate root).
        let _g = env_guard();
        let prev = std::env::var_os("OMNI_VOICE_PARAKEET_CONVERTER");
        std::env::remove_var("OMNI_VOICE_PARAKEET_CONVERTER");
        let found = locate_parakeet_converter().unwrap();
        assert!(
            found.ends_with("convert_parakeet_weights.py"),
            "got: {}",
            found.display()
        );
        if let Some(v) = prev {
            std::env::set_var("OMNI_VOICE_PARAKEET_CONVERTER", v);
        }
    }

    /// Writes a `/bin/sh` stand-in for the Python converter. `body` is the
    /// script's logic (it receives `--src <p> --out <p>` as `$@`).
    fn write_fake_converter(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("fake_convert.sh");
        std::fs::write(&p, body).unwrap();
        p
    }

    /// Runs `f` with `PYTHON=/bin/sh` and `OMNI_VOICE_PARAKEET_CONVERTER`
    /// pointed at `converter`, restoring both afterwards. Must be called
    /// from inside `with_hf_env`'s closure (which already holds `ENV_GUARD`)
    /// — it does not take the guard itself, to avoid a re-entrant deadlock.
    fn with_fake_converter<T>(converter: &Path, f: impl FnOnce() -> T) -> T {
        let prev_python = std::env::var_os("PYTHON");
        let prev_conv = std::env::var_os("OMNI_VOICE_PARAKEET_CONVERTER");
        std::env::set_var("PYTHON", "/bin/sh");
        std::env::set_var("OMNI_VOICE_PARAKEET_CONVERTER", converter);
        let result = f();
        match prev_python {
            Some(v) => std::env::set_var("PYTHON", v),
            None => std::env::remove_var("PYTHON"),
        }
        match prev_conv {
            Some(v) => std::env::set_var("OMNI_VOICE_PARAKEET_CONVERTER", v),
            None => std::env::remove_var("OMNI_VOICE_PARAKEET_CONVERTER"),
        }
        result
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_installs_parakeet_via_mock_hub_and_fake_converter() {
        let server = MockServer::start().await;
        // Not a byte-string literal: the ▁ (U+2581) marker is non-ASCII.
        let config = r#"{"joint": {"vocabulary": ["<unk>", "▁the", "▁cat", "s"]}}"#.as_bytes();
        mount_hub_file(&server, "config.json", config).await;
        mount_hub_file(&server, "model.safetensors", b"raw-mlx-weights").await;

        let endpoint = server.uri();
        let (out, dest) = tokio::task::spawn_blocking(move || {
            let hf_home = tempfile::TempDir::new().unwrap();
            let dest = tempfile::TempDir::new().unwrap();
            let conv_dir = tempfile::TempDir::new().unwrap();
            // Stub converter: parse `--out <path>` and write a dummy file there.
            let converter = write_fake_converter(
                conv_dir.path(),
                "while [ $# -gt 0 ]; do case \"$1\" in --out) shift; printf weights > \"$1\";; esac; shift; done\n",
            );
            let mut out: Vec<u8> = Vec::new();
            with_hf_env(&endpoint, hf_home.path(), || {
                with_fake_converter(&converter, || {
                    let cmd = InstallModelCommand {
                        dest: Some(dest.path().to_path_buf()),
                        force: false,
                        accept_downloads: true,
                        variant: Variant::ParakeetTdt06bV2,
                    };
                    cmd.run(&mut std::io::empty(), false, &mut out).unwrap();
                });
            });
            (out, dest)
        })
        .await
        .unwrap();

        let d = dest.path();
        assert!(d.join("config.json").is_file(), "config.json downloaded");
        assert!(
            d.join("candle_weights.safetensors").is_file(),
            "converter output present"
        );
        assert!(
            !d.join("model.safetensors").exists(),
            "raw MLX safetensors should be deleted after conversion"
        );
        assert!(
            d.join("ATTRIBUTION.txt").is_file(),
            "CC-BY-4.0 attribution written"
        );
        let attribution = std::fs::read_to_string(d.join("ATTRIBUTION.txt")).unwrap();
        assert!(attribution.contains("CC-BY-4.0"), "got: {attribution}");
        // The synthesised tokenizer must load through the backend's loader.
        let tok = tokenizers::Tokenizer::from_file(d.join("tokenizer.json")).unwrap();
        assert_eq!(tok.get_vocab_size(false), 4);
        let msg = String::from_utf8(out).unwrap();
        assert!(msg.contains("Parakeet model installed at"), "got: {msg}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn install_parakeet_reports_converter_failure() {
        let server = MockServer::start().await;
        mount_hub_file(
            &server,
            "config.json",
            br#"{"joint": {"vocabulary": ["a"]}}"#,
        )
        .await;
        mount_hub_file(&server, "model.safetensors", b"raw").await;

        let endpoint = server.uri();
        let err = tokio::task::spawn_blocking(move || {
            let hf_home = tempfile::TempDir::new().unwrap();
            let dest = tempfile::TempDir::new().unwrap();
            let conv_dir = tempfile::TempDir::new().unwrap();
            // Converter that fails — exercises the non-zero-exit bail.
            let converter = write_fake_converter(conv_dir.path(), "exit 1\n");
            let mut out: Vec<u8> = Vec::new();
            with_hf_env(&endpoint, hf_home.path(), || {
                with_fake_converter(&converter, || {
                    let cmd = InstallModelCommand {
                        dest: Some(dest.path().to_path_buf()),
                        force: false,
                        accept_downloads: true,
                        variant: Variant::ParakeetTdt06bV2,
                    };
                    cmd.run(&mut std::io::empty(), false, &mut out).unwrap_err()
                })
            })
        })
        .await
        .unwrap();

        assert!(
            format!("{err:#}").contains("converter failed"),
            "got: {err:#}"
        );
    }
}

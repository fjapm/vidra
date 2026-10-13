//! Captions: validate and copy whatever the uploader supplied, or transcribe
//! with whisper.cpp when there is nothing to copy.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use tokio::process::Command;
use tracing::{debug, info, warn};

use crate::ladder::audio_extract_args;
use crate::process::run_checked;

/// Language tag pattern, matching what the upload CLI enforces.
fn is_valid_language(tag: &str) -> bool {
    let (base, region) = match tag.split_once('-') {
        Some((base, region)) => (base, Some(region)),
        None => (tag, None),
    };

    let base_ok = (2..=3).contains(&base.len()) && base.chars().all(|c| c.is_ascii_lowercase());

    let region_ok = match region {
        None => true,
        Some(region) => !region.is_empty() && region.chars().all(|c| c.is_ascii_alphanumeric()),
    };

    base_ok && region_ok
}

/// Extract the language tag from `source/{id}/captions/{lang}.vtt`.
pub fn language_from_caption_key(key: &str) -> Option<String> {
    let filename = key.rsplit('/').next()?;
    let tag = filename.strip_suffix(".vtt")?;

    if is_valid_language(tag) {
        Some(tag.to_string())
    } else {
        None
    }
}

/// Check that bytes look like WebVTT.
///
/// A file must begin with the "WEBVTT" magic, optionally preceded by a UTF-8
/// BOM. Serving a non-VTT file as text/vtt makes the player silently show no
/// captions, so this is worth rejecting up front.
pub fn validate_vtt(data: &[u8]) -> Result<()> {
    // Strip a UTF-8 BOM, which plenty of Windows subtitle tools emit.
    let body = data.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(data);

    if !body.starts_with(b"WEBVTT") {
        let preview: String = String::from_utf8_lossy(&body[..body.len().min(32)])
            .chars()
            .filter(|c| !c.is_control())
            .collect();
        bail!("not a WebVTT file; it starts with {preview:?} instead of \"WEBVTT\"");
    }

    // The magic must be followed by end-of-file, a newline, a space or a tab -
    // "WEBVTTX" is not valid.
    match body.get(6) {
        None | Some(b'\n') | Some(b'\r') | Some(b' ') | Some(b'\t') => Ok(()),
        Some(byte) => {
            bail!("malformed WebVTT signature: unexpected byte {byte:#x} after \"WEBVTT\"")
        }
    }
}

/// A caption track ready to be written to `media/{id}/captions/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptionTrack {
    pub language: String,
    pub data: Vec<u8>,
    pub auto_generated: bool,
}

/// Extract the language whisper.cpp auto-detected from its diagnostic output.
///
/// The line whisper-cli actually emits is, verbatim:
///
///   whisper_full_with_state: auto-detected language: en (p = 0.444352)
///
/// It is matched on the "detected language:" substring rather than the whole
/// prefix, so a change to the function name in the log line does not break
/// this. Returns `None` when no such line is present, which is what happens
/// if `--no-prints` is ever added back to the argument list.
fn detected_language(output: &str) -> Option<String> {
    output
        .lines()
        .find_map(|line| {
            line.split("detected language:")
                .nth(1)
                .and_then(|rest| rest.split_whitespace().next())
        })
        .map(normalise_whisper_language)
}

/// Normalise whisper's language output into a tag we will accept.
fn normalise_whisper_language(raw: &str) -> String {
    let tag = raw.trim().to_ascii_lowercase();
    if is_valid_language(&tag) {
        tag
    } else {
        // whisper can report "auto" or an empty string when detection fails.
        // "und" (undetermined) is the ISO 639-2 code for exactly this and
        // keeps the manifest and playlist valid.
        "und".to_string()
    }
}

/// Arguments for a whisper-cli transcription run.
///
/// A separate function so the exact flag list is unit-testable, in the same
/// spirit as `ladder::ffmpeg_args`.
///
/// Note what is *absent*: `--no-prints`. It silences whisper's diagnostics,
/// including the "auto-detected language:" line that [`detected_language`]
/// parses, so passing it would quietly label every auto-generated track
/// "und". The extra output costs nothing because stderr is captured rather
/// than forwarded to the structured log.
fn whisper_args(model_path: &Path, wav_path: &Path, output_prefix: &Path) -> Vec<String> {
    vec![
        "-m".into(),
        model_path.to_string_lossy().into_owned(),
        "-f".into(),
        wav_path.to_string_lossy().into_owned(),
        // Auto-detect the spoken language.
        "-l".into(),
        "auto".into(),
        "--output-vtt".into(),
        "--output-file".into(),
        output_prefix.to_string_lossy().into_owned(),
    ]
}

/// Transcribe a video's audio with whisper.cpp.
///
/// Returns `Ok(None)` when the source has no audio track, which is not an
/// error: a silent screen recording simply has no captions.
pub async fn transcribe(
    source_path: &Path,
    work_dir: &Path,
    model_path: &Path,
    has_audio: bool,
) -> Result<Option<CaptionTrack>> {
    if !has_audio {
        info!("source has no audio track; skipping transcription");
        return Ok(None);
    }

    if !model_path.exists() {
        bail!(
            "whisper model not found at {}; it should have been fetched at startup",
            model_path.display()
        );
    }

    // whisper.cpp requires 16 kHz mono PCM; anything else is rejected or
    // resampled badly.
    let wav_path = work_dir.join("audio.wav");
    run_checked(
        "ffmpeg",
        &audio_extract_args(&source_path.to_string_lossy(), &wav_path.to_string_lossy()),
        "extracting audio for transcription",
    )
    .await?;

    // whisper-cli writes <output_prefix>.vtt.
    let output_prefix = work_dir.join("transcript");

    let output = run_checked(
        "whisper-cli",
        &whisper_args(model_path, &wav_path, &output_prefix),
        "transcribing audio",
    )
    .await?;

    let vtt_path = output_prefix.with_extension("vtt");
    let data = tokio::fs::read(&vtt_path).await.with_context(|| {
        format!(
            "whisper-cli reported success but wrote no VTT at {}",
            vtt_path.display()
        )
    })?;

    validate_vtt(&data).context("whisper-cli produced an invalid VTT file")?;

    let language = detected_language(&output.stderr)
        .or_else(|| detected_language(&output.stdout))
        .unwrap_or_else(|| {
            warn!("could not determine the detected language; labelling it 'und'");
            "und".to_string()
        });

    info!(language, bytes = data.len(), "transcribed audio");

    Ok(Some(CaptionTrack {
        language,
        data,
        auto_generated: true,
    }))
}

/// SHA-256 checksums for the Whisper models we know about.
///
/// Verified so a truncated or corrupted download cannot be mistaken for a
/// working model, which otherwise surfaces as a confusing failure deep inside
/// whisper.cpp rather than as "the download was bad".
///
/// These are the git-LFS object hashes published by the
/// ggerganov/whisper.cpp model repository on Hugging Face. To check or add
/// one:
///
/// ```text
/// curl -s https://huggingface.co/ggerganov/whisper.cpp/raw/main/<file> \
///   | grep oid
/// ```
///
/// A model not listed here is downloaded *without* verification and warned
/// about, rather than refused - so a new model can be used immediately, at the
/// cost of the integrity check.
pub fn known_model_sha256(filename: &str) -> Option<&'static str> {
    match filename {
        "ggml-large-v3-turbo-q5_0.bin" => {
            Some("394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2")
        }
        "ggml-large-v3-turbo.bin" => {
            Some("1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69")
        }
        "ggml-base.en.bin" => {
            Some("a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002")
        }
        _ => None,
    }
}

/// Make a usable model available, reporting whether transcription can proceed.
///
/// Called lazily, immediately before transcribing, rather than at startup: the
/// model is a several-hundred-megabyte download, and fetching it when captions
/// were supplied or transcription is disabled is pure waste. A single run has
/// no later jobs to amortise it over.
///
/// Every failure here is a warning, not an error: transcription is a
/// nice-to-have, and a video without captions is far better than no video.
pub async fn prepare_model(model_path: &Path) -> bool {
    if !whisper_available().await {
        warn!("whisper-cli is not on PATH; this video will have no captions");
        return false;
    }

    let expected = model_path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(known_model_sha256);

    if expected.is_none() {
        warn!(
            path = %model_path.display(),
            "no known checksum for this model; downloading without verification"
        );
    }

    match ensure_model(model_path, expected).await {
        Ok(()) => true,
        Err(error) => {
            warn!(%error, "could not prepare the whisper model; captions are disabled");
            false
        }
    }
}

/// Download the whisper model if it is not already present, verifying its
/// SHA-256.
///
/// Kept in a directory the caller is expected to mount, so it survives between
/// runs: the large-v3-turbo model is around 550 MB and re-downloading it on
/// every run would be slow and rude to the mirror.
pub async fn ensure_model(model_path: &Path, expected_sha256: Option<&str>) -> Result<()> {
    if model_path.exists() {
        debug!(path = %model_path.display(), "whisper model already present");

        if let Some(expected) = expected_sha256 {
            let actual = sha256_file(model_path).await?;
            if !actual.eq_ignore_ascii_case(expected) {
                // A truncated download from a previous run is worse than no
                // model: it fails deep inside whisper with a confusing error.
                bail!(
                    "whisper model at {} has checksum {} but {} was expected; \
                     delete it to force a re-download",
                    model_path.display(),
                    actual,
                    expected
                );
            }
        }
        return Ok(());
    }

    let filename = model_path
        .file_name()
        .and_then(|n| n.to_str())
        .context("the whisper model path has no filename")?;

    if let Some(parent) = model_path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("could not create {}", parent.display()))?;
    }

    let url = format!(
        "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/{filename}?download=true"
    );
    info!(%url, "downloading the whisper model");

    // Download beside the target then rename, so an interrupted download can
    // never be mistaken for a complete model on the next start.
    let temp_path = model_path.with_extension("partial");

    let response = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(1800))
        .build()
        .context("could not build the model download client")?
        .get(&url)
        .send()
        .await
        .with_context(|| format!("could not fetch the whisper model from {url}"))?;

    if !response.status().is_success() {
        bail!(
            "downloading the whisper model from {url} returned {}",
            response.status()
        );
    }

    let bytes = response
        .bytes()
        .await
        .context("error while downloading the whisper model")?;

    tokio::fs::write(&temp_path, &bytes)
        .await
        .with_context(|| format!("could not write {}", temp_path.display()))?;

    if let Some(expected) = expected_sha256 {
        let actual = sha256_file(&temp_path).await?;
        if !actual.eq_ignore_ascii_case(expected) {
            let _ = tokio::fs::remove_file(&temp_path).await;
            bail!("the downloaded whisper model has checksum {actual} but {expected} was expected");
        }
    }

    tokio::fs::rename(&temp_path, model_path)
        .await
        .with_context(|| format!("could not move the model into {}", model_path.display()))?;

    info!(path = %model_path.display(), bytes = bytes.len(), "whisper model ready");
    Ok(())
}

async fn sha256_file(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};

    let data = tokio::fs::read(path)
        .await
        .with_context(|| format!("could not read {} for hashing", path.display()))?;

    let mut hasher = Sha256::new();
    hasher.update(&data);
    Ok(hex::encode(hasher.finalize()))
}

/// Run `whisper-cli --help` to confirm the binary is usable.
pub async fn whisper_available() -> bool {
    Command::new("whisper-cli")
        .arg("--help")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Default path for a transcript inside `media/{id}/`.
pub fn caption_output_path(language: &str) -> PathBuf {
    PathBuf::from(format!("captions/{language}.vtt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- VTT validation ---------------------------------------------------

    #[test]
    fn accepts_a_minimal_webvtt_file() {
        assert!(validate_vtt(b"WEBVTT").is_ok());
        assert!(validate_vtt(b"WEBVTT\n").is_ok());
        assert!(validate_vtt(b"WEBVTT\r\n").is_ok());
        assert!(validate_vtt(b"WEBVTT\n\n00:00:00.000 --> 00:00:01.000\nhi\n").is_ok());
    }

    #[test]
    fn accepts_a_webvtt_header_with_a_description() {
        // "WEBVTT - Some description" is valid per the spec.
        assert!(validate_vtt(b"WEBVTT - subtitles\n").is_ok());
        assert!(validate_vtt(b"WEBVTT\tkind: captions\n").is_ok());
    }

    #[test]
    fn accepts_a_file_with_a_utf8_bom() {
        let mut data = vec![0xEF, 0xBB, 0xBF];
        data.extend_from_slice(b"WEBVTT\n\n");
        assert!(validate_vtt(&data).is_ok());
    }

    #[test]
    fn rejects_a_file_that_is_not_webvtt() {
        assert!(validate_vtt(b"").is_err());
        assert!(validate_vtt(b"1\n00:00:00,000 --> 00:00:01,000\nSRT\n").is_err());
        assert!(validate_vtt(b"<html></html>").is_err());
        assert!(
            validate_vtt(b"webvtt").is_err(),
            "the magic is case-sensitive"
        );
    }

    #[test]
    fn rejects_a_corrupted_signature() {
        // "WEBVTTX" is not the magic; accepting it would mean serving a file
        // the player silently ignores.
        assert!(validate_vtt(b"WEBVTTX\n").is_err());
        assert!(validate_vtt(b"WEBVTT12345").is_err());
    }

    #[test]
    fn the_rejection_message_shows_what_was_found() {
        let error = validate_vtt(b"NOT A VTT AT ALL").expect_err("should reject");
        assert!(error.to_string().contains("NOT A VTT"), "got: {error}");
    }

    // --- language tags ----------------------------------------------------

    #[test]
    fn accepts_valid_language_tags() {
        for tag in [
            "en", "fr", "pt", "fra", "deu", "pt-BR", "zh-Hans", "en-GB", "es-419",
        ] {
            assert!(is_valid_language(tag), "{tag} should be valid");
        }
    }

    #[test]
    fn rejects_invalid_language_tags() {
        for tag in [
            "", "e", "english", "EN", "en_US", "123", "en-", "-en", "en--GB",
        ] {
            assert!(!is_valid_language(tag), "{tag} should be invalid");
        }
    }

    #[test]
    fn extracts_the_language_from_a_caption_key() {
        assert_eq!(
            language_from_caption_key("source/abc/captions/en.vtt").as_deref(),
            Some("en")
        );
        assert_eq!(
            language_from_caption_key("source/abc/captions/pt-BR.vtt").as_deref(),
            Some("pt-BR")
        );
    }

    #[test]
    fn ignores_keys_that_are_not_caption_files() {
        assert_eq!(language_from_caption_key("source/abc/meta.json"), None);
        assert_eq!(language_from_caption_key("source/abc/source.mp4"), None);
        assert_eq!(
            language_from_caption_key("source/abc/captions/not-a-lang-tag.vtt"),
            None
        );
        assert_eq!(
            language_from_caption_key("source/abc/captions/EN.vtt"),
            None
        );
    }

    // --- whisper language normalisation -----------------------------------

    // --- language detection from whisper output ---------------------------
    //
    // The sample lines below are the real output of
    // whisper-cli v1.7.4, captured by running it in the worker image.

    #[test]
    fn parses_the_language_from_real_whisper_output() {
        let stderr = "\
whisper_init_from_file_with_params_no_state: loading model from '/w/ggml-tiny.bin'
whisper_model_load: n_langs       = 99
main: processing '/w/speech.wav' (48000 samples, 3.0 sec), 4 threads, 1 processors, 5 beams + best of 5, lang = auto, task = transcribe, timestamps = 1 ...
whisper_full_with_state: auto-detected language: en (p = 0.444352)
output_vtt: saving output to '/w/out.vtt'
";

        assert_eq!(detected_language(stderr).as_deref(), Some("en"));
    }

    #[test]
    fn parses_other_detected_languages() {
        for (tag, expected) in [("de", "de"), ("pt", "pt"), ("zh", "zh")] {
            let line = format!("whisper_full_with_state: auto-detected language: {tag} (p = 0.91)");
            assert_eq!(detected_language(&line).as_deref(), Some(expected));
        }
    }

    #[test]
    fn returns_none_when_whisper_printed_no_detection_line() {
        // This is exactly what --no-prints produces, and why that flag must
        // not be passed: the fallback would label every track "und".
        let quiet = "output_vtt: saving output to '/w/out.vtt'\n";
        assert_eq!(detected_language(quiet), None);
        assert_eq!(detected_language(""), None);
    }

    #[test]
    fn whisper_arguments_request_auto_detection_and_vtt_output() {
        let args = whisper_args(
            Path::new("/scratch/models/ggml-large-v3-turbo-q5_0.bin"),
            Path::new("/scratch/job/audio.wav"),
            Path::new("/scratch/job/transcript"),
        );

        assert!(args
            .windows(2)
            .any(|w| w == ["-m", "/scratch/models/ggml-large-v3-turbo-q5_0.bin"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["-f", "/scratch/job/audio.wav"]));
        assert!(args.windows(2).any(|w| w == ["-l", "auto"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["--output-file", "/scratch/job/transcript"]));
        assert!(args.iter().any(|a| a == "--output-vtt"));
    }

    #[test]
    fn whisper_arguments_do_not_suppress_its_diagnostics() {
        // A regression guard with teeth. --no-prints hides the
        // "auto-detected language:" line, which would silently degrade every
        // auto-generated caption track to "und" with no other symptom.
        let args = whisper_args(
            Path::new("/model.bin"),
            Path::new("/audio.wav"),
            Path::new("/out"),
        );

        assert!(
            !args.iter().any(|a| a == "--no-prints" || a == "-np"),
            "whisper diagnostics must stay enabled, got {args:?}"
        );
    }

    #[test]
    fn normalises_a_detected_language() {
        assert_eq!(normalise_whisper_language("en"), "en");
        assert_eq!(normalise_whisper_language("EN"), "en");
        assert_eq!(normalise_whisper_language("  fr  "), "fr");
    }

    #[test]
    fn falls_back_to_und_for_an_undetectable_language() {
        // A playlist with LANGUAGE="auto" or LANGUAGE="" is invalid, so a
        // failed detection must become a real tag.
        assert_eq!(normalise_whisper_language("auto"), "und");
        assert_eq!(normalise_whisper_language(""), "und");
        assert_eq!(normalise_whisper_language("unknown"), "und");
        assert_eq!(normalise_whisper_language("?"), "und");
    }

    #[test]
    fn the_fallback_tag_is_itself_valid() {
        assert!(is_valid_language("und"));
    }

    #[test]
    fn caption_output_paths_are_relative_to_the_video_directory() {
        assert_eq!(caption_output_path("en"), PathBuf::from("captions/en.vtt"));
        assert_eq!(
            caption_output_path("pt-BR"),
            PathBuf::from("captions/pt-BR.vtt")
        );
    }

    #[tokio::test]
    async fn a_missing_model_is_reported_clearly() {
        let error = transcribe(
            Path::new("/nonexistent/in.mp4"),
            Path::new("/tmp"),
            Path::new("/nonexistent/model.bin"),
            true,
        )
        .await
        .expect_err("should fail");

        assert!(
            error.to_string().contains("whisper model not found"),
            "got: {error}"
        );
    }

    #[tokio::test]
    async fn a_silent_source_yields_no_captions_rather_than_an_error() {
        let result = transcribe(
            Path::new("/nonexistent/in.mp4"),
            Path::new("/tmp"),
            Path::new("/nonexistent/model.bin"),
            false,
        )
        .await
        .expect("a silent source is not an error");

        assert!(result.is_none());
    }
}

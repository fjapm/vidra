//! Command-line interface.
//!
//! Every option also reads a `VIDRA_*` environment variable, so the same
//! invocation can be expressed as arguments or as `docker run -e`. Flags win
//! over the environment, which is what makes the Dockerfile's `ENV` defaults
//! overridable without rebuilding the image.

use anyhow::{bail, Result};
use clap::Parser;
use std::path::PathBuf;

/// Transcode a video into an HLS ladder with thumbnails and captions.
#[derive(Debug, Clone, Parser)]
#[command(name = "vidra", version, about, long_about = None)]
pub struct Args {
    /// Source video file.
    #[arg(value_name = "INPUT", env = "VIDRA_INPUT")]
    pub input: PathBuf,

    /// Directory to write the HLS output into. Created if absent.
    #[arg(short, long, value_name = "DIR", env = "VIDRA_OUT")]
    pub out: PathBuf,

    /// Identifier recorded as `video_id` in manifest.json.
    ///
    /// Defaults to the input filename without its extension. Consumers
    /// typically serve the output under a path built from this, so it may not
    /// contain a path separator.
    #[arg(long, value_name = "ID", env = "VIDRA_ID")]
    pub id: Option<String>,

    /// Directory of pre-existing WebVTT caption files named `{language}.vtt`,
    /// for example `en.vtt` or `pt-BR.vtt`.
    ///
    /// When this yields at least one valid track, transcription is skipped
    /// entirely.
    #[arg(long, value_name = "DIR", env = "VIDRA_CAPTIONS_DIR")]
    pub captions_dir: Option<PathBuf>,

    /// Do not transcribe when no caption files are supplied.
    #[arg(long, env = "VIDRA_NO_TRANSCRIBE")]
    pub no_transcribe: bool,

    /// Seconds between thumbnail sprite tiles.
    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = 10,
        env = "VIDRA_THUMBNAIL_INTERVAL"
    )]
    pub thumbnail_interval: u32,

    /// Directory for intermediate files. A per-run subdirectory is created
    /// inside it and removed afterwards.
    #[arg(long, value_name = "DIR", env = "VIDRA_SCRATCH_DIR")]
    pub scratch_dir: Option<PathBuf>,

    /// Whisper model filename.
    #[arg(
        long,
        value_name = "FILE",
        default_value = "ggml-large-v3-turbo-q5_0.bin",
        env = "VIDRA_WHISPER_MODEL"
    )]
    pub whisper_model: String,

    /// Directory the Whisper model is cached in. Mount this to avoid
    /// re-downloading roughly a gigabyte on every run.
    #[arg(
        long,
        value_name = "DIR",
        default_value = "/models",
        env = "VIDRA_WHISPER_MODEL_DIR"
    )]
    pub whisper_model_dir: PathBuf,

    /// Clear the output directory first if it is not empty.
    ///
    /// Without this, a non-empty output directory is an error: re-encoding
    /// into one would leave orphaned segments that the new playlists do not
    /// reference.
    #[arg(short, long, env = "VIDRA_FORCE")]
    pub force: bool,

    /// Emit human-readable logs instead of JSON.
    #[arg(long, env = "VIDRA_LOG_PRETTY")]
    pub log_pretty: bool,
}

impl Args {
    /// Validate the arguments and resolve the defaults that depend on others.
    pub fn resolve(mut self) -> Result<Resolved> {
        if self.thumbnail_interval == 0 {
            bail!("--thumbnail-interval must be at least 1");
        }

        let id = match self.id.take() {
            Some(id) => id,
            None => derive_id(&self.input)?,
        };
        validate_id(&id)?;

        let scratch_dir = self.scratch_dir.take().unwrap_or_else(std::env::temp_dir);

        Ok(Resolved {
            id,
            scratch_dir,
            args: self,
        })
    }
}

/// Arguments after validation, with the derived values filled in.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub args: Args,
    pub id: String,
    pub scratch_dir: PathBuf,
}

impl Resolved {
    pub fn input(&self) -> &std::path::Path {
        &self.args.input
    }

    pub fn out(&self) -> &std::path::Path {
        &self.args.out
    }

    pub fn whisper_model_path(&self) -> PathBuf {
        self.args.whisper_model_dir.join(&self.args.whisper_model)
    }
}

/// Use the input filename, without its extension, as the identifier.
fn derive_id(input: &std::path::Path) -> Result<String> {
    input
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            anyhow::anyhow!("could not derive an id from {}; pass --id", input.display())
        })
}

/// Reject identifiers that would misbehave when used to build a path.
///
/// Consumers serve the output under a prefix derived from this value, so a
/// separator or a `..` here turns into a traversal there. Checked once, here,
/// rather than trusted at every use site.
fn validate_id(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("--id must not be empty");
    }
    if id == "." || id == ".." {
        bail!("--id must not be {id:?}");
    }
    if id.contains('/') || id.contains('\\') {
        bail!("--id must not contain a path separator, got {id:?}");
    }
    if id.contains('\0') {
        bail!("--id must not contain a null byte");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(input: &str) -> Args {
        Args {
            input: PathBuf::from(input),
            out: PathBuf::from("/out"),
            id: None,
            captions_dir: None,
            no_transcribe: false,
            thumbnail_interval: 10,
            scratch_dir: Some(PathBuf::from("/scratch")),
            whisper_model: "ggml-base.en.bin".into(),
            whisper_model_dir: PathBuf::from("/models"),
            force: false,
            log_pretty: false,
        }
    }

    #[test]
    fn the_id_defaults_to_the_input_filename_without_its_extension() {
        let resolved = args("/in/my-clip.mp4").resolve().expect("should resolve");
        assert_eq!(resolved.id, "my-clip");
    }

    #[test]
    fn an_explicit_id_wins_over_the_filename() {
        let mut args = args("/in/my-clip.mp4");
        args.id = Some("20260101-deadbeef".into());

        assert_eq!(args.resolve().unwrap().id, "20260101-deadbeef");
    }

    #[test]
    fn a_filename_with_several_dots_keeps_everything_but_the_last() {
        let resolved = args("/in/clip.final.v2.mov").resolve().unwrap();
        assert_eq!(resolved.id, "clip.final.v2");
    }

    #[test]
    fn a_filename_without_an_extension_is_usable() {
        assert_eq!(args("/in/clip").resolve().unwrap().id, "clip");
    }

    #[test]
    fn an_input_with_no_filename_is_rejected() {
        let error = args("/").resolve().expect_err("should be rejected");
        assert!(error.to_string().contains("--id"), "got: {error}");
    }

    #[test]
    fn an_id_containing_a_path_separator_is_rejected() {
        // Consumers build a serving path from this, so a separator here would
        // become a traversal there.
        for id in ["a/b", "../etc", "a\\b", "/abs"] {
            let mut args = args("/in/clip.mp4");
            args.id = Some(id.into());

            assert!(args.resolve().is_err(), "id {id:?} should be rejected");
        }
    }

    #[test]
    fn the_dot_identifiers_are_rejected() {
        for id in [".", ".."] {
            let mut args = args("/in/clip.mp4");
            args.id = Some(id.into());
            assert!(args.resolve().is_err(), "id {id:?} should be rejected");
        }
    }

    #[test]
    fn an_empty_id_is_rejected() {
        let mut args = args("/in/clip.mp4");
        args.id = Some(String::new());
        assert!(args.resolve().is_err());
    }

    #[test]
    fn a_zero_thumbnail_interval_is_rejected() {
        let mut args = args("/in/clip.mp4");
        args.thumbnail_interval = 0;

        let error = args.resolve().expect_err("should be rejected");
        assert!(
            error.to_string().contains("--thumbnail-interval"),
            "got: {error}"
        );
    }

    #[test]
    fn the_scratch_directory_falls_back_to_the_system_temp_directory() {
        let mut args = args("/in/clip.mp4");
        args.scratch_dir = None;

        assert_eq!(args.resolve().unwrap().scratch_dir, std::env::temp_dir());
    }

    #[test]
    fn the_model_path_joins_the_directory_and_filename() {
        let resolved = args("/in/clip.mp4").resolve().unwrap();
        assert_eq!(
            resolved.whisper_model_path(),
            PathBuf::from("/models/ggml-base.en.bin")
        );
    }

    #[test]
    fn the_argument_parser_is_internally_consistent() {
        // clap panics on a malformed command definition (a duplicate long
        // name, a bad default). Asserting here means that is a test failure
        // rather than a crash on first run.
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    #[test]
    fn the_input_and_output_can_be_parsed_from_arguments() {
        let args = Args::try_parse_from(["vidra", "--out", "/out/clip", "/in/clip.mp4"])
            .expect("should parse");

        assert_eq!(args.input, PathBuf::from("/in/clip.mp4"));
        assert_eq!(args.out, PathBuf::from("/out/clip"));
        assert!(!args.force);
    }

    #[test]
    fn the_output_directory_is_required() {
        assert!(Args::try_parse_from(["vidra", "/in/clip.mp4"]).is_err());
    }
}

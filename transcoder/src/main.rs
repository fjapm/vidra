//! Vidra entry point.
//!
//! One video in, one HLS ladder out, then exit. There is no queue, no poll
//! loop and no storage client: the input is a path, the output is a directory,
//! and whatever scheduled this run decides what those are.
//!
//! The exit status is the result. `0` means `manifest.json` says `ready` and
//! everything it references is on disk; non-zero means it does not, and
//! `manifest.json` carries the reason.

use anyhow::{Context, Result};
use clap::Parser;
use std::process::ExitCode;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use vidra::cli::{Args, Resolved};
use vidra::error::{exit_code_for, permanent, Permanent};
use vidra::pipeline;

fn init_tracing(pretty: bool) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,hyper=warn,reqwest=warn"));

    let builder = FmtSubscriber::builder()
        .with_env_filter(filter)
        .with_target(true);

    // JSON by default: whatever collects container logs can then filter by
    // video_id. `--log-pretty` is for running it by hand.
    let installed = if pretty {
        tracing::subscriber::set_global_default(builder.finish())
    } else {
        tracing::subscriber::set_global_default(
            builder
                .json()
                .with_current_span(true)
                .with_span_list(false)
                .finish(),
        )
    };

    if let Err(error) = installed {
        eprintln!("could not install the tracing subscriber: {error}");
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    // Parsed before tracing is installed, because it decides the log format.
    // clap handles --help, --version and a bad argument list itself.
    let args = Args::parse();
    init_tracing(args.log_pretty);

    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // `{error:#}` renders the whole anyhow context chain, which is what
            // makes a failure diagnosable from the log alone.
            let code = exit_code_for(&error);
            error!(
                error = %format!("{error:#}"),
                exit_code = code,
                retryable = code != vidra::error::EXIT_PERMANENT,
                "vidra failed"
            );
            ExitCode::from(code)
        }
    }
}

async fn run(args: Args) -> Result<()> {
    let options = args.resolve()?;

    info!(
        input = %options.input().display(),
        out = %options.out().display(),
        video_id = %options.id,
        version = env!("CARGO_PKG_VERSION"),
        "starting"
    );

    // Fail before encoding anything rather than an hour in.
    preflight(&options).await?;

    pipeline::prepare_output_dir(options.out(), options.args.force).await?;

    // A per-run scratch directory, removed afterwards whatever happens, so a
    // failed run cannot leak the volume.
    let scratch_root = options.scratch_dir.join(format!("vidra-{}", options.id));
    let result = pipeline::process_video(&options, &scratch_root).await;

    // Clean up before reporting, so the disk is reclaimed either way.
    if let Err(error) = tokio::fs::remove_dir_all(&scratch_root).await {
        if error.kind() != std::io::ErrorKind::NotFound {
            warn!(
                path = %scratch_root.display(),
                %error,
                "could not remove the scratch directory"
            );
        }
    }

    match result {
        Ok(_) => {
            info!(video_id = %options.id, "done");
            Ok(())
        }
        Err(error) => {
            let rendered = format!("{error:#}");

            // Leave a durable record next to the partial output. The exit
            // status already says the run failed; this says why, to whoever
            // finds the directory later.
            if let Err(write_error) =
                pipeline::record_failure(options.out(), &options.id, &rendered).await
            {
                warn!(
                    error = %format!("{write_error:#}"),
                    "could not write the failure manifest"
                );
            }

            Err(error)
        }
    }
}

/// Verify the environment before encoding anything.
async fn preflight(options: &Resolved) -> Result<()> {
    for tool in ["ffmpeg", "ffprobe"] {
        let status = tokio::process::Command::new(tool)
            .arg("-version")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;

        match status {
            Ok(status) if status.success() => {}
            _ => anyhow::bail!("{tool} is not usable; it must be on PATH"),
        }
    }

    // Everything about the input is a permanent failure: the same file will be
    // just as missing, just as empty and just as much a directory next time.
    let input = options.input();
    let metadata = tokio::fs::metadata(input)
        .await
        .context(Permanent)
        .with_context(|| format!("could not read the input {}", input.display()))?;

    if !metadata.is_file() {
        return Err(permanent(format!(
            "the input {} is not a regular file",
            input.display()
        )));
    }
    if metadata.len() == 0 {
        return Err(permanent(format!("the input {} is empty", input.display())));
    }

    tokio::fs::create_dir_all(&options.scratch_dir)
        .await
        .with_context(|| {
            format!(
                "could not create the scratch directory {} (is the volume mounted?)",
                options.scratch_dir.display()
            )
        })?;

    // The Whisper model is *not* fetched here. It is a gigabyte-scale download
    // that is pure waste when captions are supplied or transcription is off,
    // and a one-shot run cannot amortise it over later jobs the way a
    // long-lived worker could. captions::prepare_model handles it lazily, at
    // the point transcription is actually about to happen.

    Ok(())
}

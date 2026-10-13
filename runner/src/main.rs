//! vidra-runner entry point.
//!
//! A poll loop. Claim jobs, run each one through vidra, store the output,
//! then acknowledge — and only then, because acknowledging is what makes a job
//! unrecoverable.
//!
//! Retry is not implemented here. The job source owns it: a job that is not
//! acknowledged comes back on its own. Releasing it explicitly only shortens
//! the wait.

use anyhow::{Context, Result};
use clap::Parser;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{error, info, warn};
use tracing_subscriber::{EnvFilter, FmtSubscriber};

use vidra_runner::cloudflare::CloudflareQueues;
use vidra_runner::config::Args;
use vidra_runner::job::{Outcome, Runner, RETRY_DELAY};
use vidra_runner::source::{Job, JobSource};
use vidra_runner::storage::Storage;
use vidra_runner::webhook;

/// Webhook delivery attempts.
const WEBHOOK_ATTEMPTS: u32 = 4;

/// How long to wait after a failed claim before trying again.
const CLAIM_BACKOFF: std::time::Duration = std::time::Duration::from_secs(10);

fn init_tracing(pretty: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::new("info,aws_sdk_s3=warn,aws_smithy_runtime=warn,hyper=warn,reqwest=warn")
    });

    let builder = FmtSubscriber::builder()
        .with_env_filter(filter)
        .with_target(true);

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
    let args = Args::parse();
    init_tracing(args.log_pretty);

    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            error!(error = %format!("{error:#}"), "vidra-runner failed to start");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<()> {
    args.validate()?;

    let layout = args.layout()?;
    let storage = Storage::new(&args.storage())?;
    let source: Arc<dyn JobSource> = Arc::new(CloudflareQueues::new(args.queue(), layout.clone())?);

    // Fail at startup rather than on the first job: a missing vidra would
    // otherwise burn a job's entire retry budget before anyone noticed.
    preflight(&args).await?;

    let runner = Arc::new(Runner {
        storage,
        layout,
        vidra_bin: args.vidra_bin.clone(),
        scratch_dir: args.scratch_dir.clone(),
        thumbnail_interval: args.thumbnail_interval,
        whisper_model_dir: args.whisper_model_dir.clone(),
        no_transcribe: args.no_transcribe,
    });

    info!(
        source = %source.describe(),
        bucket = %args.bucket,
        concurrency = args.concurrency,
        webhook = args.webhook_enabled(),
        version = env!("CARGO_PKG_VERSION"),
        "starting"
    );

    poll_loop(Arc::new(args), runner, source).await
}

/// Verify the environment before claiming any work.
async fn preflight(args: &Args) -> Result<()> {
    let output = tokio::process::Command::new(&args.vidra_bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| {
            format!(
                "could not run {} --version (is it on PATH?)",
                args.vidra_bin.display()
            )
        })?;

    if !output.status.success() {
        anyhow::bail!(
            "{} --version exited with {}",
            args.vidra_bin.display(),
            output.status
        );
    }

    info!(
        vidra = %String::from_utf8_lossy(&output.stdout).trim(),
        "found the transcoder"
    );

    tokio::fs::create_dir_all(&args.scratch_dir)
        .await
        .with_context(|| {
            format!(
                "could not create the scratch directory {} (is the volume mounted?)",
                args.scratch_dir.display()
            )
        })?;

    Ok(())
}

async fn poll_loop(args: Arc<Args>, runner: Arc<Runner>, source: Arc<dyn JobSource>) -> Result<()> {
    let permits = Arc::new(Semaphore::new(args.concurrency));
    let mut shutdown = Box::pin(shutdown_signal());

    loop {
        let claimed = tokio::select! {
            biased;
            _ = &mut shutdown => break,
            result = source.claim() => match result {
                Ok(claimed) => claimed,
                Err(error) => {
                    // Transient: a network blip or a 5xx. Back off rather
                    // than spinning.
                    error!(error = %format!("{error:#}"), "claim failed; backing off");
                    tokio::select! {
                        _ = &mut shutdown => break,
                        _ = tokio::time::sleep(CLAIM_BACKOFF) => {}
                    }
                    continue;
                }
            },
        };

        if claimed.jobs.is_empty() {
            tokio::select! {
                _ = &mut shutdown => break,
                _ = tokio::time::sleep(std::time::Duration::from_secs(args.poll_idle_seconds)) => {}
            }
            continue;
        }

        info!(
            jobs = claimed.jobs.len(),
            backlog = claimed.backlog.unwrap_or(0),
            "claimed a batch"
        );

        let mut handles = Vec::new();

        for job in claimed.jobs {
            // Acquire before spawning, so concurrency bounds the work in
            // flight rather than just the number of tasks.
            let permit = permits
                .clone()
                .acquire_owned()
                .await
                .context("the concurrency semaphore was closed")?;

            let args = args.clone();
            let runner = runner.clone();
            let source = source.clone();

            handles.push(tokio::spawn(async move {
                let _permit = permit;
                handle_job(&args, &runner, source.as_ref(), job).await;
            }));
        }

        // Wait for the whole batch before claiming again. Simple, and it keeps
        // the lease accounting easy to reason about.
        for handle in handles {
            if let Err(error) = handle.await {
                error!(%error, "a job task panicked");
            }
        }
    }

    info!("shutting down");
    Ok(())
}

/// Process one job, then settle it with the source.
async fn handle_job(args: &Args, runner: &Runner, source: &dyn JobSource, job: Job) {
    let max_attempts = source.max_attempts();
    let is_final = job.is_final_attempt(max_attempts);

    info!(
        video_id = %job.video_id,
        key = %job.source_key,
        attempt = job.attempts,
        max_attempts,
        "processing"
    );

    let outcome = match runner.run(&job).await {
        Ok(outcome) => outcome,
        Err(error) => Outcome::Failed {
            error: format!("{error:#}"),
            permanent: false,
        },
    };

    match outcome {
        Outcome::Ready(manifest) => {
            // Acknowledge first: the output and the manifest are stored, so
            // the job is genuinely done. A webhook failure must not undo that.
            complete(source, &job).await;

            if args.webhook_enabled() {
                notify(args, &manifest).await;
            }
            info!(video_id = %job.video_id, "done");
        }

        Outcome::Failed { error, permanent } => {
            error!(
                video_id = %job.video_id,
                attempt = job.attempts,
                permanent,
                %error,
                "processing failed"
            );

            // A permanent failure will fail identically on every remaining
            // attempt, so there is nothing to gain by using them up.
            if permanent || is_final {
                match runner.record_failure(&job, &error).await {
                    Ok(manifest) => {
                        // The failure is durable, so the job can go.
                        complete(source, &job).await;
                        if args.webhook_enabled() {
                            notify(args, &manifest).await;
                        }
                    }
                    Err(write_error) => {
                        error!(
                            video_id = %job.video_id,
                            error = %format!("{write_error:#}"),
                            "could not record the failure; leaving the job unacknowledged"
                        );
                        // Deliberately not acknowledged: better it reappears
                        // or lands in the dead-letter queue than vanish with
                        // no record anywhere.
                    }
                }
            } else {
                // Not acknowledging is the retry. Release the lease so the
                // next attempt starts now rather than after the full
                // visibility timeout.
                if let Err(error) = source.release(&job, Some(RETRY_DELAY)).await {
                    warn!(
                        error = %format!("{error:#}"),
                        "could not release the job; its lease will expire on its own"
                    );
                }
            }
        }
    }
}

async fn complete(source: &dyn JobSource, job: &Job) {
    if let Err(error) = source.complete(job).await {
        // The work is done but the acknowledgement failed, so the job will be
        // redelivered. Processing is idempotent — the output prefix is cleared
        // and rewritten — so a duplicate is wasteful but not harmful.
        warn!(
            error = %format!("{error:#}"),
            video_id = %job.video_id,
            "could not acknowledge the job; it may be redelivered"
        );
    }
}

/// Fire the webhook. Never fails the job.
async fn notify(args: &Args, manifest: &vidra::manifest::Manifest) {
    let client =
        match webhook::WebhookClient::new(args.webhook_url.clone(), args.webhook_secret.clone()) {
            Ok(client) => client,
            Err(error) => {
                warn!(%error, "could not build the webhook client");
                return;
            }
        };

    if let Err(error) = client.deliver(manifest, WEBHOOK_ATTEMPTS).await {
        // The output is stored and the manifest is written, so the video is
        // playable regardless. Losing the notification is not worth
        // reprocessing for.
        warn!(%error, "webhook delivery failed; the video is still processed");
    }
}

/// Resolves on SIGTERM or SIGINT.
///
/// SIGTERM is what `docker stop` and `systemctl stop` send. The loop finishes
/// the batch in flight before exiting, so a deploy does not abandon a
/// half-finished transcode.
async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};

        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(error) => {
                error!(%error, "could not install the SIGTERM handler");
                // Fall back to ctrl-c alone rather than never shutting down.
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };

        tokio::select! {
            _ = sigterm.recv() => info!("received SIGTERM"),
            result = tokio::signal::ctrl_c() => match result {
                Ok(()) => info!("received SIGINT"),
                Err(error) => error!(%error, "error waiting for SIGINT"),
            },
        }
    }

    #[cfg(not(unix))]
    {
        if tokio::signal::ctrl_c().await.is_ok() {
            info!("received ctrl-c");
        }
    }
}

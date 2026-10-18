//! Configuration.
//!
//! Every setting has a `RUNNER_*` environment variable, because this runs as a
//! daemon configured by an env file rather than an argument list. The flags
//! exist anyway so `--help` can document the whole surface in one place.
//!
//! One deliberate omission: there are no boolean *environment* toggles beyond
//! `RUNNER_NO_TRANSCRIBE`. clap parses a bool env var strictly, so a key
//! written as `KEY=` by a config-management template is a hard startup error
//! rather than "false" — a sharp edge worth avoiding in a file that is
//! generated rather than hand-written.

use anyhow::{bail, Result};
use clap::Parser;
use std::path::PathBuf;

use crate::cloudflare::QueueConfig;
use crate::layout::KeyLayout;
use crate::storage::StorageConfig;

#[derive(Debug, Clone, Parser)]
#[command(name = "vidra-runner", version, about, long_about = None)]
pub struct Args {
    // --- queue ------------------------------------------------------------
    /// Cloudflare account that owns the queue.
    #[arg(long, env = "RUNNER_CF_ACCOUNT_ID")]
    pub cf_account_id: String,

    /// Queue to pull jobs from.
    #[arg(long, env = "RUNNER_QUEUE_ID")]
    pub queue_id: String,

    /// API token with Queues Read *and* Write. Ack needs write.
    #[arg(long, env = "RUNNER_QUEUE_API_TOKEN", hide_env_values = true)]
    pub queue_api_token: String,

    /// Messages to lease per pull.
    #[arg(long, env = "RUNNER_QUEUE_BATCH_SIZE", default_value_t = 5)]
    pub queue_batch_size: u32,

    /// Lease duration. Must comfortably exceed the slowest transcode.
    #[arg(
        long,
        env = "RUNNER_QUEUE_VISIBILITY_TIMEOUT_MS",
        default_value_t = 7_200_000
    )]
    pub queue_visibility_timeout_ms: u64,

    /// Delivery attempts before a job is abandoned. Used to recognise the
    /// final attempt, when a `failed` manifest is written.
    #[arg(long, env = "RUNNER_QUEUE_MAX_ATTEMPTS", default_value_t = 3)]
    pub queue_max_attempts: u32,

    // --- storage ----------------------------------------------------------
    /// Bucket holding both the sources and the output.
    #[arg(long, env = "RUNNER_BUCKET")]
    pub bucket: String,

    #[arg(long, env = "AWS_ACCESS_KEY_ID", hide_env_values = true)]
    pub access_key_id: String,

    #[arg(long, env = "AWS_SECRET_ACCESS_KEY", hide_env_values = true)]
    pub secret_access_key: String,

    /// Region. R2 requires `auto`; AWS wants a real region.
    #[arg(long, env = "AWS_DEFAULT_REGION", default_value = "auto")]
    pub region: String,

    /// S3 endpoint. Required for R2 and MinIO; omit for AWS S3.
    #[arg(long, env = "RUNNER_S3_ENDPOINT")]
    pub s3_endpoint: Option<String>,

    /// Address buckets as a path component rather than a subdomain.
    ///
    /// Defaults on, which is what R2 and MinIO need. Set to `false` for AWS.
    #[arg(long, env = "RUNNER_S3_PATH_STYLE", default_value_t = true)]
    pub s3_path_style: bool,

    // --- layout -----------------------------------------------------------
    /// Prefix sources are read from.
    #[arg(long, env = "RUNNER_SOURCE_PREFIX", default_value = "source/")]
    pub source_prefix: String,

    /// Prefix output is written to.
    #[arg(long, env = "RUNNER_OUTPUT_PREFIX", default_value = "media/")]
    pub output_prefix: String,

    // --- processing -------------------------------------------------------
    /// Jobs to process at once. Each one runs its own vidra process.
    #[arg(long, env = "RUNNER_CONCURRENCY", default_value_t = 1)]
    pub concurrency: usize,

    /// Objects uploaded at once when storing a job's output.
    ///
    /// An HLS ladder is thousands of small objects, and each PUT is a round
    /// trip, so this is latency-bound: the useful value tracks the distance to
    /// the bucket rather than the size of the box.
    #[arg(long, env = "RUNNER_UPLOAD_CONCURRENCY", default_value_t = 16)]
    pub upload_concurrency: usize,

    /// The vidra executable.
    #[arg(long, env = "RUNNER_VIDRA_BIN", default_value = "vidra")]
    pub vidra_bin: PathBuf,

    /// Working directory for downloads and output staging.
    #[arg(long, env = "RUNNER_SCRATCH_DIR", default_value = "/scratch")]
    pub scratch_dir: PathBuf,

    /// Passed to vidra when set.
    #[arg(long, env = "RUNNER_THUMBNAIL_INTERVAL")]
    pub thumbnail_interval: Option<u32>,

    /// Passed to vidra when set. Should be persistent, or every job
    /// re-downloads the model.
    #[arg(long, env = "RUNNER_WHISPER_MODEL_DIR", default_value = "/models")]
    pub whisper_model_dir: Option<PathBuf>,

    /// Never transcribe; videos without supplied captions get none.
    #[arg(long, env = "RUNNER_NO_TRANSCRIBE")]
    pub no_transcribe: bool,

    /// Seconds to wait after an empty pull before polling again.
    #[arg(long, env = "RUNNER_POLL_IDLE_SECONDS", default_value_t = 5)]
    pub poll_idle_seconds: u64,

    // --- webhook ----------------------------------------------------------
    /// Notified when a video becomes ready or fails. Empty disables it.
    #[arg(long, env = "RUNNER_WEBHOOK_URL", default_value = "")]
    pub webhook_url: String,

    #[arg(
        long,
        env = "RUNNER_WEBHOOK_SECRET",
        default_value = "",
        hide_env_values = true
    )]
    pub webhook_secret: String,

    // --- logging ----------------------------------------------------------
    /// Human-readable logs instead of JSON.
    #[arg(long, env = "RUNNER_LOG_PRETTY")]
    pub log_pretty: bool,
}

impl Args {
    pub fn validate(&self) -> Result<()> {
        if self.concurrency == 0 {
            bail!("--concurrency must be at least 1");
        }
        if self.upload_concurrency == 0 {
            bail!("--upload-concurrency must be at least 1");
        }
        if self.poll_idle_seconds == 0 {
            bail!("--poll-idle-seconds must be at least 1");
        }
        if let Some(endpoint) = &self.s3_endpoint {
            if !endpoint.starts_with("https://") && !endpoint.starts_with("http://") {
                bail!("--s3-endpoint must be an http(s) URL, got {endpoint:?}");
            }
        }

        // A webhook URL with no secret would send unsigned callbacks, which
        // the receiver cannot authenticate. Fail loudly rather than silently
        // downgrading.
        if !self.webhook_url.is_empty() {
            if self.webhook_secret.is_empty() {
                bail!("--webhook-secret is required when --webhook-url is set");
            }
            if !self.webhook_url.starts_with("https://") {
                bail!(
                    "--webhook-url must be an https URL, got {:?}",
                    self.webhook_url
                );
            }
        }

        Ok(())
    }

    pub fn webhook_enabled(&self) -> bool {
        !self.webhook_url.is_empty()
    }

    pub fn layout(&self) -> Result<KeyLayout> {
        KeyLayout::new(&self.source_prefix, &self.output_prefix)
    }

    pub fn storage(&self) -> StorageConfig {
        StorageConfig {
            bucket: self.bucket.clone(),
            access_key_id: self.access_key_id.clone(),
            secret_access_key: self.secret_access_key.clone(),
            region: self.region.clone(),
            endpoint: self.s3_endpoint.clone(),
            force_path_style: self.s3_path_style,
        }
    }

    pub fn queue(&self) -> QueueConfig {
        QueueConfig {
            account_id: self.cf_account_id.clone(),
            queue_id: self.queue_id.clone(),
            api_token: self.queue_api_token.clone(),
            batch_size: self.queue_batch_size,
            visibility_timeout_ms: self.queue_visibility_timeout_ms,
            max_attempts: self.queue_max_attempts,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Args {
        Args {
            cf_account_id: "acc".into(),
            queue_id: "q".into(),
            queue_api_token: "token".into(),
            queue_batch_size: 5,
            queue_visibility_timeout_ms: 7_200_000,
            queue_max_attempts: 3,
            bucket: "video".into(),
            access_key_id: "key".into(),
            secret_access_key: "secret".into(),
            region: "auto".into(),
            s3_endpoint: Some("https://acc.r2.cloudflarestorage.com".into()),
            s3_path_style: true,
            source_prefix: "source/".into(),
            output_prefix: "media/".into(),
            concurrency: 1,
            upload_concurrency: 16,
            vidra_bin: PathBuf::from("vidra"),
            scratch_dir: PathBuf::from("/scratch"),
            thumbnail_interval: None,
            whisper_model_dir: Some(PathBuf::from("/models")),
            no_transcribe: false,
            poll_idle_seconds: 5,
            webhook_url: String::new(),
            webhook_secret: String::new(),
            log_pretty: false,
        }
    }

    #[test]
    fn a_valid_configuration_passes() {
        assert!(args().validate().is_ok());
    }

    #[test]
    fn concurrency_must_be_at_least_one() {
        let mut args = args();
        args.concurrency = 0;
        assert!(args.validate().is_err());
    }

    #[test]
    fn upload_concurrency_must_be_at_least_one() {
        let mut args = args();
        args.upload_concurrency = 0;
        assert!(args.validate().is_err());
    }

    #[test]
    fn the_idle_interval_must_be_at_least_one_second() {
        let mut args = args();
        args.poll_idle_seconds = 0;
        assert!(args.validate().is_err());
    }

    #[test]
    fn an_endpoint_must_be_a_url() {
        let mut args = args();
        args.s3_endpoint = Some("acc.r2.cloudflarestorage.com".into());
        assert!(args.validate().is_err());
    }

    #[test]
    fn an_absent_endpoint_is_valid() {
        // This is the AWS S3 configuration.
        let mut args = args();
        args.s3_endpoint = None;
        args.region = "eu-central-1".into();
        args.s3_path_style = false;

        assert!(args.validate().is_ok());
    }

    #[test]
    fn a_webhook_url_without_a_secret_is_rejected() {
        let mut args = args();
        args.webhook_url = "https://app.example.com/hooks/video".into();

        let error = args.validate().expect_err("should be rejected");
        assert!(error.to_string().contains("secret"), "got: {error}");
    }

    #[test]
    fn a_webhook_url_must_be_https() {
        let mut args = args();
        args.webhook_url = "http://app.example.com/hooks".into();
        args.webhook_secret = "secret".into();

        assert!(args.validate().is_err());
    }

    #[test]
    fn webhooks_are_disabled_by_an_empty_url() {
        assert!(!args().webhook_enabled());
        assert!(args().validate().is_ok());
    }

    #[test]
    fn the_layout_is_built_from_the_prefixes() {
        let layout = args().layout().expect("should build");
        assert_eq!(layout.output_prefix_for("clip"), "media/clip/");
    }

    #[test]
    fn nested_prefixes_are_rejected_at_startup() {
        let mut args = args();
        args.output_prefix = "source/out/".into();
        assert!(args.layout().is_err());
    }

    #[test]
    fn the_argument_parser_is_internally_consistent() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }
}

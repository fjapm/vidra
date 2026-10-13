//! Cloudflare Queues as a [`JobSource`].
//!
//! Everything Cloudflare-shaped lives here: the HTTP pull protocol, the lease
//! model, and the R2 event-notification payload. Nothing above this module
//! knows any of it exists.
//!
//! Two endpoints:
//!   POST {base}/messages/pull  — lease a batch
//!   POST {base}/messages/ack   — acknowledge and/or explicitly retry
//!
//! A leased message becomes visible again after `visibility_timeout_ms`, so
//! the lease must comfortably outlast the slowest transcode. Not acking is how
//! a failure is retried: the message simply reappears. Acking is the only
//! thing that removes it permanently.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tracing::{info, warn};

use crate::layout::KeyLayout;
use crate::source::{Claimed, Job, JobSource};

// ---------------------------------------------------------------------------
// R2 event notifications
// ---------------------------------------------------------------------------

/// The JSON Cloudflare delivers for an R2 object event.
#[derive(Debug, Clone, Deserialize)]
pub struct R2Event {
    /// e.g. "PutObject", "CompleteMultipartUpload", "CopyObject".
    pub action: String,
    pub bucket: String,
    pub object: R2EventObject,
    #[serde(default)]
    pub event_time: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct R2EventObject {
    pub key: String,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default, rename = "eTag")]
    pub etag: Option<String>,
}

/// Object-create actions. A delete event must never start a transcode.
///
/// All three matter: `aws s3 cp` switches to multipart above its 8 MiB
/// threshold, so real videos arrive as `CompleteMultipartUpload` while small
/// files and captions arrive as `PutObject`. Recognising only `PutObject` is
/// the classic "works for my 2 MB test clip, silently drops every real upload"
/// bug.
const CREATE_ACTIONS: &[&str] = &["PutObject", "CompleteMultipartUpload", "CopyObject"];

pub fn is_create_action(action: &str) -> bool {
    CREATE_ACTIONS.contains(&action)
}

// ---------------------------------------------------------------------------
// Queues wire format
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
struct QueueMessage {
    id: String,
    /// Opaque lease token; required to ack or retry this delivery.
    lease_id: String,
    body: serde_json::Value,
    #[serde(default)]
    attempts: u32,
}

impl QueueMessage {
    /// Deserialise the body into an R2 event.
    ///
    /// R2 sets a JSON content type so the body normally arrives as an object,
    /// but a message published by hand (or re-driven from the dead-letter
    /// queue) can arrive as a JSON *string* containing JSON. Both are
    /// accepted, because getting this wrong turns every DLQ replay into a
    /// parse failure.
    fn parse_body(&self) -> Result<R2Event> {
        match &self.body {
            serde_json::Value::String(raw) => serde_json::from_str(raw)
                .context("message body is a string but not valid JSON inside"),
            other => serde_json::from_value(other.clone())
                .context("message body is not the expected shape"),
        }
    }
}

#[derive(Debug, Deserialize)]
struct PullResponse {
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
    #[serde(default)]
    result: Option<PullResult>,
}

#[derive(Debug, Default, Deserialize)]
struct PullResult {
    #[serde(default)]
    messages: Vec<QueueMessage>,
    #[serde(default)]
    message_backlog_count: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct AckResponse {
    success: bool,
    #[serde(default)]
    errors: Vec<ApiError>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
}

fn describe_errors(errors: &[ApiError]) -> String {
    if errors.is_empty() {
        return "no error detail returned".to_string();
    }
    errors
        .iter()
        .map(|e| {
            format!(
                "{}{}",
                e.code.map(|c| format!("[{c}] ")).unwrap_or_default(),
                e.message.as_deref().unwrap_or("unknown")
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

#[derive(Debug, Serialize)]
struct PullRequest {
    visibility_timeout_ms: u64,
    batch_size: u32,
}

#[derive(Debug, Serialize)]
struct AckRequest {
    acks: Vec<AckEntry>,
    retries: Vec<RetryEntry>,
}

#[derive(Debug, Serialize)]
struct AckEntry {
    lease_id: String,
}

#[derive(Debug, Serialize)]
struct RetryEntry {
    lease_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    delay_seconds: Option<u32>,
}

// ---------------------------------------------------------------------------
// The job source
// ---------------------------------------------------------------------------

/// How to reach a Cloudflare queue.
#[derive(Debug, Clone)]
pub struct QueueConfig {
    pub account_id: String,
    pub queue_id: String,
    pub api_token: String,
    pub batch_size: u32,
    pub visibility_timeout_ms: u64,
    pub max_attempts: u32,
}

pub struct CloudflareQueues {
    http: reqwest::Client,
    base_url: String,
    config: QueueConfig,
    layout: KeyLayout,
}

impl CloudflareQueues {
    pub fn new(config: QueueConfig, layout: KeyLayout) -> Result<Self> {
        if config.account_id.is_empty() {
            bail!("the Cloudflare account id must not be empty");
        }
        if config.queue_id.is_empty() {
            bail!("the queue id must not be empty");
        }
        if config.api_token.is_empty() {
            bail!("the queue API token must not be empty");
        }
        if !(1..=100).contains(&config.batch_size) {
            bail!(
                "the queue batch size must be between 1 and 100, got {}",
                config.batch_size
            );
        }
        if config.max_attempts == 0 {
            bail!("the queue max attempts must be at least 1");
        }

        let http = reqwest::Client::builder()
            // A pull can legitimately block server-side; keep the timeout well
            // above that but not unbounded, so a hung connection is noticed.
            .timeout(Duration::from_secs(60))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("vidra-runner/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("could not build the HTTP client for the Queues API")?;

        let base_url = format!(
            "https://api.cloudflare.com/client/v4/accounts/{}/queues/{}",
            config.account_id, config.queue_id
        );

        Ok(Self {
            http,
            base_url,
            config,
            layout,
        })
    }

    /// Acknowledge messages, and optionally ask for others to be retried.
    async fn ack(&self, acks: &[String], retries: &[(String, Option<u32>)]) -> Result<()> {
        if acks.is_empty() && retries.is_empty() {
            return Ok(());
        }

        let url = format!("{}/messages/ack", self.base_url);
        let body = AckRequest {
            acks: acks
                .iter()
                .map(|lease_id| AckEntry {
                    lease_id: lease_id.clone(),
                })
                .collect(),
            retries: retries
                .iter()
                .map(|(lease_id, delay_seconds)| RetryEntry {
                    lease_id: lease_id.clone(),
                    delay_seconds: *delay_seconds,
                })
                .collect(),
        };

        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.config.api_token)
            .json(&body)
            .send()
            .await
            .context("failed to reach the Queues ack endpoint")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("could not read the ack response body")?;

        if !status.is_success() {
            bail!("Queues ack returned {}: {}", status, text.trim());
        }

        // The API answers 200 with {"success": false} on a logical failure, so
        // the status code alone is not enough.
        let parsed: AckResponse = serde_json::from_str(&text)
            .with_context(|| format!("could not parse the ack response: {}", text.trim()))?;

        if !parsed.success {
            bail!(
                "Queues ack was unsuccessful: {}",
                describe_errors(&parsed.errors)
            );
        }

        Ok(())
    }

    /// Turn a leased message into a job, or `None` if it is not one.
    ///
    /// Anything that is not a job is acked by the caller and forgotten: a
    /// malformed body will never become valid, and a delete event or a sidecar
    /// file is simply not work. Leaving either unacked would cycle it until it
    /// reached the dead-letter queue.
    fn to_job(&self, message: &QueueMessage) -> Option<Job> {
        let event = match message.parse_body() {
            Ok(event) => event,
            Err(error) => {
                warn!(
                    message_id = %message.id,
                    %error,
                    "dropping a message whose body could not be parsed"
                );
                return None;
            }
        };

        if !is_create_action(&event.action) {
            info!(
                action = %event.action,
                key = %event.object.key,
                "ignoring a non-create event"
            );
            return None;
        }

        let source = self.layout.parse_source_key(&event.object.key)?;

        Some(Job {
            lease: message.lease_id.clone(),
            id: message.id.clone(),
            attempts: message.attempts.max(1),
            video_id: source.video_id,
            source_key: source.key,
        })
    }
}

#[async_trait]
impl JobSource for CloudflareQueues {
    async fn claim(&self) -> Result<Claimed> {
        let url = format!("{}/messages/pull", self.base_url);

        let response = self
            .http
            .post(&url)
            .bearer_auth(&self.config.api_token)
            .json(&PullRequest {
                visibility_timeout_ms: self.config.visibility_timeout_ms,
                batch_size: self.config.batch_size,
            })
            .send()
            .await
            .context("failed to reach the Queues pull endpoint")?;

        let status = response.status();
        let text = response
            .text()
            .await
            .context("could not read the pull response body")?;

        if !status.is_success() {
            // 403 here almost always means the API token lacks Queues Write
            // (ack needs write, not just read). Say so, because the raw body
            // is not obviously about permissions.
            if status == reqwest::StatusCode::FORBIDDEN {
                bail!(
                    "Queues pull returned 403; the API token needs both Queues Read and \
                     Queues Write. Body: {}",
                    text.trim()
                );
            }
            bail!("Queues pull returned {}: {}", status, text.trim());
        }

        let parsed: PullResponse = serde_json::from_str(&text)
            .with_context(|| format!("could not parse the pull response: {}", text.trim()))?;

        if !parsed.success {
            bail!(
                "Queues pull was unsuccessful: {}",
                describe_errors(&parsed.errors)
            );
        }

        let result = parsed.result.unwrap_or_default();

        // Split the batch: real jobs go up, everything else is acked here and
        // never seen again.
        let mut jobs = Vec::new();
        let mut discard = Vec::new();

        for message in &result.messages {
            match self.to_job(message) {
                Some(job) => jobs.push(job),
                None => discard.push(message.lease_id.clone()),
            }
        }

        if !discard.is_empty() {
            let count = discard.len();
            if let Err(error) = self.ack(&discard, &[]).await {
                // Not fatal: the lease expires and they are re-examined, with
                // the same outcome. Worth a warning because a persistent
                // failure here means the same messages churn forever.
                warn!(%error, count, "could not ack non-job messages");
            } else {
                info!(count, "acked messages that were not jobs");
            }
        }

        Ok(Claimed {
            jobs,
            backlog: result.message_backlog_count,
        })
    }

    async fn complete(&self, job: &Job) -> Result<()> {
        self.ack(std::slice::from_ref(&job.lease), &[]).await
    }

    async fn release(&self, job: &Job, delay: Option<Duration>) -> Result<()> {
        // Releasing is only an optimisation: not acking already means the
        // message returns when the lease expires. This makes the next attempt
        // start in seconds instead of after the (deliberately long)
        // visibility timeout.
        let delay_seconds = delay.map(|d| d.as_secs().min(u64::from(u32::MAX)) as u32);
        self.ack(&[], &[(job.lease.clone(), delay_seconds)]).await
    }

    fn max_attempts(&self) -> u32 {
        self.config.max_attempts
    }

    fn describe(&self) -> String {
        format!("cloudflare-queues({})", self.config.queue_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queues() -> CloudflareQueues {
        CloudflareQueues::new(
            QueueConfig {
                account_id: "acc".into(),
                queue_id: "q".into(),
                api_token: "token".into(),
                batch_size: 5,
                visibility_timeout_ms: 7_200_000,
                max_attempts: 3,
            },
            KeyLayout::default(),
        )
        .expect("should build")
    }

    fn message(body: serde_json::Value) -> QueueMessage {
        QueueMessage {
            id: "msg-1".into(),
            lease_id: "lease-1".into(),
            body,
            attempts: 1,
        }
    }

    fn put_event(key: &str) -> serde_json::Value {
        serde_json::json!({
            "action": "PutObject",
            "bucket": "video",
            "object": { "key": key, "size": 123, "eTag": "abc" }
        })
    }

    // --- construction -----------------------------------------------------

    #[test]
    fn the_base_url_is_the_cloudflare_api_endpoint() {
        assert_eq!(
            queues().base_url,
            "https://api.cloudflare.com/client/v4/accounts/acc/queues/q"
        );
    }

    #[test]
    fn required_configuration_is_validated() {
        let base = QueueConfig {
            account_id: "acc".into(),
            queue_id: "q".into(),
            api_token: "token".into(),
            batch_size: 5,
            visibility_timeout_ms: 7_200_000,
            max_attempts: 3,
        };

        let mutations: Vec<fn(&mut QueueConfig)> = vec![
            |c| c.account_id.clear(),
            |c| c.queue_id.clear(),
            |c| c.api_token.clear(),
            |c| c.batch_size = 0,
            |c| c.batch_size = 101,
            |c| c.max_attempts = 0,
        ];

        for mutate in mutations {
            let mut config = base.clone();
            mutate(&mut config);
            assert!(CloudflareQueues::new(config, KeyLayout::default()).is_err());
        }
    }

    #[test]
    fn the_backend_describes_itself_with_its_queue() {
        assert_eq!(queues().describe(), "cloudflare-queues(q)");
    }

    // --- event classification --------------------------------------------

    #[test]
    fn a_source_upload_becomes_a_job() {
        let job = queues()
            .to_job(&message(put_event("source/clip-one/source.mp4")))
            .expect("should be a job");

        assert_eq!(job.video_id, "clip-one");
        assert_eq!(job.source_key, "source/clip-one/source.mp4");
        assert_eq!(job.lease, "lease-1");
        assert_eq!(job.attempts, 1);
    }

    #[test]
    fn a_multipart_completion_becomes_a_job() {
        // This is what a real (large) video upload produces; missing it is the
        // classic "works for my tiny test clip" bug.
        let body = serde_json::json!({
            "action": "CompleteMultipartUpload",
            "bucket": "video",
            "object": { "key": "source/clip/source.mov" }
        });

        let job = queues().to_job(&message(body)).expect("should be a job");
        assert_eq!(job.video_id, "clip");
    }

    #[test]
    fn a_delete_event_is_not_a_job() {
        let body = serde_json::json!({
            "action": "DeleteObject",
            "bucket": "video",
            "object": { "key": "source/clip/source.mp4" }
        });

        assert!(queues().to_job(&message(body)).is_none());
    }

    #[test]
    fn the_sidecar_files_are_not_jobs() {
        for key in [
            "source/clip/meta.json",
            "source/clip/captions/en.vtt",
            "media/clip/master.m3u8",
        ] {
            assert!(
                queues().to_job(&message(put_event(key))).is_none(),
                "{key} should not be a job"
            );
        }
    }

    #[test]
    fn a_malformed_body_is_not_a_job() {
        for body in [
            serde_json::json!({ "action": "PutObject" }),
            serde_json::json!({ "nonsense": true }),
            serde_json::json!("not json at all"),
            serde_json::json!(42),
        ] {
            assert!(queues().to_job(&message(body)).is_none());
        }
    }

    #[test]
    fn a_body_delivered_as_a_json_string_is_accepted() {
        // What a dead-letter-queue re-drive looks like. Rejecting it would
        // make every replay fail.
        let inner = put_event("source/clip/source.mp4").to_string();
        let job = queues()
            .to_job(&message(serde_json::Value::String(inner)))
            .expect("should be a job");

        assert_eq!(job.video_id, "clip");
    }

    #[test]
    fn an_attempt_count_of_zero_is_normalised_to_one() {
        // The field defaults to 0 when absent, but attempts start at 1; a 0
        // would make is_final_attempt arithmetic off by one.
        let mut msg = message(put_event("source/clip/source.mp4"));
        msg.attempts = 0;

        assert_eq!(queues().to_job(&msg).unwrap().attempts, 1);
    }

    // --- actions ----------------------------------------------------------

    #[test]
    fn create_actions_are_recognised() {
        assert!(is_create_action("PutObject"));
        assert!(is_create_action("CompleteMultipartUpload"));
        assert!(is_create_action("CopyObject"));
    }

    #[test]
    fn non_create_actions_are_not() {
        assert!(!is_create_action("DeleteObject"));
        assert!(!is_create_action("LifecycleDeletion"));
        assert!(!is_create_action(""));
        assert!(!is_create_action("putobject"), "matching is case-sensitive");
    }

    // --- event deserialisation -------------------------------------------

    #[test]
    fn parses_a_real_put_event() {
        let event: R2Event = serde_json::from_str(
            r#"{
              "account": "abc123",
              "action": "PutObject",
              "bucket": "video",
              "object": {
                "key": "source/20260101120000-deadbeef/source.mp4",
                "size": 123456789,
                "eTag": "d41d8cd98f00b204e9800998ecf8427e"
              },
              "eventTime": "2026-10-09T12:00:00.000Z"
            }"#,
        )
        .expect("should deserialise");

        assert_eq!(event.action, "PutObject");
        assert_eq!(event.bucket, "video");
        assert_eq!(event.object.size, Some(123_456_789));
    }

    #[test]
    fn a_delete_event_omits_size_and_etag() {
        let event: R2Event = serde_json::from_str(
            r#"{"action":"DeleteObject","bucket":"video",
                "object":{"key":"source/abc/source.mp4"}}"#,
        )
        .expect("should deserialise");

        assert_eq!(event.object.size, None);
        assert_eq!(event.object.etag, None);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // Cloudflare may add fields; that must not break the runner.
        let event: R2Event = serde_json::from_str(
            r#"{"action":"PutObject","bucket":"video",
                "object":{"key":"source/abc/source.mp4","newField":1},
                "somethingNew":{"nested":true}}"#,
        )
        .expect("unknown fields should be ignored");

        assert_eq!(event.action, "PutObject");
    }

    #[test]
    fn an_event_missing_required_fields_is_rejected() {
        assert!(serde_json::from_str::<R2Event>(r#"{"action":"PutObject"}"#).is_err());
        assert!(serde_json::from_str::<R2Event>(r#"{"bucket":"video"}"#).is_err());
        assert!(serde_json::from_str::<R2Event>(
            r#"{"action":"PutObject","bucket":"v","object":{}}"#
        )
        .is_err());
    }
}

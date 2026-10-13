//! Optional webhook notifying the application that a video is ready or failed.
//!
//! The body is signed with HMAC-SHA256 over the exact bytes sent, in an
//! `X-Signature` header. The receiver must recompute the MAC over the raw
//! request body *before* JSON parsing: re-serialising the parsed body would
//! change the bytes and the signature would never match.

use anyhow::{Context, Result};
use hmac::{Hmac, Mac};
use serde::Serialize;
use sha2::Sha256;
use std::time::Duration;
use tracing::{debug, warn};

use vidra::manifest::Manifest;

type HmacSha256 = Hmac<Sha256>;

/// Header carrying the hex-encoded HMAC-SHA256 of the request body.
pub const SIGNATURE_HEADER: &str = "X-Signature";

#[derive(Debug, Serialize)]
struct WebhookPayload<'a> {
    video_id: &'a str,
    status: &'a str,
    manifest: &'a Manifest,
}

/// Hex-encoded HMAC-SHA256 of `body` under `secret`.
pub fn sign_body(body: &[u8], secret: &str) -> String {
    // new_from_slice accepts any key length for HMAC, so this cannot fail.
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC accepts keys of any length");
    mac.update(body);
    hex::encode(mac.finalize().into_bytes())
}

pub struct WebhookClient {
    http: reqwest::Client,
    url: String,
    secret: String,
}

impl WebhookClient {
    pub fn new(url: String, secret: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("video-transcode/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("could not build the webhook HTTP client")?;

        Ok(Self { http, url, secret })
    }

    /// Deliver the webhook, retrying with exponential backoff.
    ///
    /// Returns `Ok(())` when delivered. A failure here must NOT fail the job:
    /// the outputs are already in R2 and the manifest is written, so the video
    /// is playable whether or not the application heard about it. The caller
    /// logs and moves on.
    pub async fn deliver(&self, manifest: &Manifest, max_attempts: u32) -> Result<()> {
        let status = match manifest.status {
            vidra::manifest::Status::Ready => "ready",
            vidra::manifest::Status::Failed => "failed",
        };

        // Serialise once: the signature covers these exact bytes, so the body
        // must not be re-serialised per attempt.
        let body = serde_json::to_vec(&WebhookPayload {
            video_id: &manifest.video_id,
            status,
            manifest,
        })
        .context("could not serialise the webhook payload")?;

        let signature = sign_body(&body, &self.secret);

        let mut last_error: Option<anyhow::Error> = None;

        for attempt in 1..=max_attempts {
            if attempt > 1 {
                // 2s, 4s, 8s, ... capped so a long outage does not hold the
                // worker off the queue indefinitely.
                let backoff = Duration::from_secs(2u64.saturating_pow(attempt - 1).min(32));
                debug!(
                    attempt,
                    backoff_secs = backoff.as_secs(),
                    "retrying webhook"
                );
                tokio::time::sleep(backoff).await;
            }

            let result = self
                .http
                .post(&self.url)
                .header("Content-Type", "application/json")
                .header(SIGNATURE_HEADER, &signature)
                .body(body.clone())
                .send()
                .await;

            match result {
                Ok(response) if response.status().is_success() => {
                    debug!(attempt, status = %response.status(), "webhook delivered");
                    return Ok(());
                }
                Ok(response) => {
                    let code = response.status();
                    let text = response.text().await.unwrap_or_default();

                    // 4xx other than 408/429 will not improve on retry.
                    if code.is_client_error()
                        && code != reqwest::StatusCode::REQUEST_TIMEOUT
                        && code != reqwest::StatusCode::TOO_MANY_REQUESTS
                    {
                        anyhow::bail!(
                            "webhook rejected with {} (not retrying): {}",
                            code,
                            text.trim()
                        );
                    }

                    last_error = Some(anyhow::anyhow!(
                        "webhook returned {}: {}",
                        code,
                        text.trim()
                    ));
                }
                Err(error) => {
                    last_error = Some(anyhow::Error::new(error).context("webhook request failed"));
                }
            }
        }

        let error = last_error
            .unwrap_or_else(|| anyhow::anyhow!("webhook delivery failed with no recorded error"));
        warn!(attempts = max_attempts, %error, "giving up on the webhook");
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vidra::manifest::{Manifest, Status};

    #[test]
    fn signature_matches_a_known_hmac_sha256_vector() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?"
        assert_eq!(
            sign_body(b"what do ya want for nothing?", "Jefe"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    #[test]
    fn signature_is_hex_encoded_sha256_length() {
        let signature = sign_body(b"{}", "secret");
        assert_eq!(signature.len(), 64, "32 bytes as hex");
        assert!(signature.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn signature_changes_with_the_body() {
        let a = sign_body(b"{\"a\":1}", "secret");
        let b = sign_body(b"{\"a\":2}", "secret");
        assert_ne!(a, b);
    }

    #[test]
    fn signature_changes_with_the_secret() {
        let a = sign_body(b"{}", "secret-a");
        let b = sign_body(b"{}", "secret-b");
        assert_ne!(a, b);
    }

    #[test]
    fn signature_handles_an_empty_body_and_empty_secret() {
        // Must not panic; HMAC accepts any key length including zero.
        assert_eq!(sign_body(b"", "").len(), 64);
        assert_eq!(sign_body(b"body", "").len(), 64);
    }

    #[test]
    fn the_payload_carries_the_contract_fields() {
        let manifest = Manifest::failed("abc", "boom", "2026-10-09T12:00:00Z".into());
        let body = serde_json::to_vec(&WebhookPayload {
            video_id: &manifest.video_id,
            status: "failed",
            manifest: &manifest,
        })
        .unwrap();

        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            json["videoId"],
            serde_json::Value::Null,
            "field is video_id"
        );
        assert_eq!(json["video_id"], "abc");
        assert_eq!(json["status"], "failed");
        assert_eq!(json["manifest"]["status"], "failed");
        assert_eq!(json["manifest"]["error"], "boom");
    }

    #[test]
    fn the_signature_verifies_against_the_exact_bytes_sent() {
        // Mirrors what a receiver does: MAC the raw body, then compare.
        let manifest = Manifest::failed("abc", "boom", "2026-10-09T12:00:00Z".into());
        let body = serde_json::to_vec(&WebhookPayload {
            video_id: &manifest.video_id,
            status: "failed",
            manifest: &manifest,
        })
        .unwrap();

        let signature = sign_body(&body, "shared-secret");

        let mut mac = HmacSha256::new_from_slice(b"shared-secret").unwrap();
        mac.update(&body);
        assert!(mac.verify_slice(&hex::decode(&signature).unwrap()).is_ok());
    }

    #[test]
    fn re_serialising_the_parsed_body_would_break_verification() {
        // Documents why the receiver must MAC the raw bytes: pretty-printed
        // and compact encodings of the same manifest differ.
        let manifest = Manifest::failed("abc", "boom", "2026-10-09T12:00:00Z".into());

        let compact = serde_json::to_vec(&manifest).unwrap();
        let pretty = serde_json::to_vec_pretty(&manifest).unwrap();

        assert_ne!(
            sign_body(&compact, "s"),
            sign_body(&pretty, "s"),
            "formatting changes the signature"
        );
    }

    #[test]
    fn status_strings_are_the_documented_values() {
        assert_eq!(
            serde_json::to_string(&Status::Ready)
                .unwrap()
                .trim_matches('"'),
            "ready"
        );
        assert_eq!(
            serde_json::to_string(&Status::Failed)
                .unwrap()
                .trim_matches('"'),
            "failed"
        );
    }
}

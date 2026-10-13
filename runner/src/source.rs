//! Where jobs come from.
//!
//! This is the one provider-specific seam in the runner. Object storage is not
//! a seam — every provider worth supporting speaks the S3 API, so `storage`
//! handles them all with configuration rather than polymorphism. Queues are
//! different: Cloudflare Queues, SQS and a Postgres job table share no wire
//! format, no lease model and no event shape.
//!
//! The important rule is that a [`JobSource`] yields an already-normalised
//! [`Job`]. Parsing the provider's event payload, and deciding which events are
//! jobs at all, happens *inside* the implementation. Nothing above this module
//! ever sees a provider-shaped event, which is what keeps adding a backend from
//! touching the pipeline.

use anyhow::Result;
use async_trait::async_trait;
use std::time::Duration;

/// One unit of work, with every provider detail already resolved away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Opaque handle identifying *this delivery*, used to complete or release
    /// it. Not the job identity — the same job redelivered has a new lease.
    pub lease: String,

    /// Provider-side message identifier. Logged, never interpreted.
    pub id: String,

    /// Delivery attempt, starting at 1.
    pub attempts: u32,

    /// The video identifier, validated by the implementation.
    pub video_id: String,

    /// Storage key of the source video.
    pub source_key: String,
}

impl Job {
    /// Whether this is the last attempt the source will make.
    ///
    /// On the final attempt the runner records a `failed` manifest, because
    /// after this the job is gone — into a dead-letter queue or simply
    /// discarded — and the application would otherwise wait forever.
    pub fn is_final_attempt(&self, max_attempts: u32) -> bool {
        self.attempts >= max_attempts
    }
}

/// A source of transcode jobs.
///
/// Implementations are expected to be cheap to clone or share behind an `Arc`,
/// and every method must be cancel-safe: the runner races them against
/// shutdown.
#[async_trait]
pub trait JobSource: Send + Sync {
    /// Claim a batch of jobs, leasing them so no other runner takes them.
    ///
    /// Returns an empty vector when there is nothing to do — not an error. The
    /// caller is responsible for backing off.
    ///
    /// Events that are not jobs (a delete notification, a sidecar file, a
    /// malformed payload) must be dealt with internally — acknowledged and
    /// dropped — rather than surfaced, so they cannot cycle forever.
    async fn claim(&self) -> Result<Claimed>;

    /// Permanently remove a job. Called only once its output is durably
    /// stored, because this is the point of no return.
    async fn complete(&self, job: &Job) -> Result<()>;

    /// Release a job for another attempt, optionally after a delay.
    ///
    /// Implementations whose lease simply expires may treat this as a hint:
    /// the job comes back either way, and releasing it early only shortens the
    /// wait.
    async fn release(&self, job: &Job, delay: Option<Duration>) -> Result<()>;

    /// Total delivery attempts before the job is abandoned.
    fn max_attempts(&self) -> u32;

    /// Human-readable backend name, for logs.
    fn describe(&self) -> String;
}

/// The result of a claim.
#[derive(Debug, Default)]
pub struct Claimed {
    pub jobs: Vec<Job>,
    /// Jobs still waiting, when the backend can report it. Logged so the
    /// operator can tell whether one runner is keeping up.
    pub backlog: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(attempts: u32) -> Job {
        Job {
            lease: "lease-1".into(),
            id: "msg-1".into(),
            attempts,
            video_id: "clip".into(),
            source_key: "source/clip/source.mp4".into(),
        }
    }

    #[test]
    fn the_final_attempt_is_recognised() {
        assert!(!job(1).is_final_attempt(3));
        assert!(!job(2).is_final_attempt(3));
        assert!(job(3).is_final_attempt(3));
    }

    #[test]
    fn an_attempt_beyond_the_maximum_still_counts_as_final() {
        // A redelivery after a lease expiry can push the counter past the
        // configured maximum. Treating that as "not final" would skip the
        // failure manifest in exactly the case that needs it most.
        assert!(job(4).is_final_attempt(3));
        assert!(job(99).is_final_attempt(3));
    }

    #[test]
    fn a_single_attempt_configuration_is_final_immediately() {
        assert!(job(1).is_final_attempt(1));
    }

    #[test]
    fn an_empty_claim_is_not_an_error() {
        let claimed = Claimed::default();
        assert!(claimed.jobs.is_empty());
        assert_eq!(claimed.backlog, None);
    }
}

//! Failure classification, so the caller knows whether retrying is pointless.
//!
//! A one-shot process communicates through its exit status, and "it failed" is
//! not enough for whatever scheduled it. A corrupt upload fails identically
//! every time, so a queue consumer that retries it three times burns the whole
//! retry budget and delays the failure report by hours. A full disk, by
//! contrast, is worth another attempt.
//!
//! The split is deliberately conservative: a failure is transient unless
//! something specifically says otherwise, because retrying a permanent failure
//! wastes time, while *not* retrying a transient one loses the video.

use std::fmt;

/// Exit status for a failure that re-running cannot fix.
///
/// 2 rather than some private number because clap already exits 2 for a bad
/// argument list, and that is the same category of problem: the request is
/// wrong, so sending it again will not help.
pub const EXIT_PERMANENT: u8 = 2;

/// Exit status for a failure that might succeed on another attempt.
pub const EXIT_TRANSIENT: u8 = 1;

/// Marker for a failure that re-running cannot fix.
///
/// Two ways to apply it, and the difference is only cosmetic:
///
/// * [`permanent`] builds a fresh error with the marker underneath, so the
///   real message stays at the head of `{error}`. Prefer it.
/// * `.context(Permanent)` marks an error that already exists, which pushes
///   the marker to the head. Still detected, just reads slightly worse.
#[derive(Debug, Clone, Copy)]
pub struct Permanent;

impl fmt::Display for Permanent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Appears in the rendered `{error:#}` chain, so it has to read as
        // prose rather than as a type name.
        write!(f, "not retryable")
    }
}

impl std::error::Error for Permanent {}

/// Build a permanent failure whose head is `message`.
pub fn permanent<C>(message: C) -> anyhow::Error
where
    C: fmt::Display + Send + Sync + 'static,
{
    anyhow::Error::new(Permanent).context(message)
}

/// Whether `error` carries a [`Permanent`] marker.
///
/// Uses `downcast_ref` rather than walking `chain()`: anyhow's chain yields
/// its internal context wrapper rather than the context value, so a marker
/// attached with `.context(Permanent)` is invisible to `chain()`. `downcast_ref`
/// searches the context values too, and so finds the marker wherever it sits.
pub fn is_permanent(error: &anyhow::Error) -> bool {
    error.downcast_ref::<Permanent>().is_some()
}

/// The exit status a failure should produce.
pub fn exit_code_for(error: &anyhow::Error) -> u8 {
    if is_permanent(error) {
        EXIT_PERMANENT
    } else {
        EXIT_TRANSIENT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn a_plain_error_is_transient() {
        let error = anyhow!("the disk filled up");

        assert!(!is_permanent(&error));
        assert_eq!(exit_code_for(&error), EXIT_TRANSIENT);
        assert_eq!(exit_code_for(&error), 1);
    }

    #[test]
    fn an_error_marked_by_context_is_permanent() {
        let error = anyhow!("moov atom not found").context(Permanent);

        assert!(is_permanent(&error));
        assert_eq!(exit_code_for(&error), EXIT_PERMANENT);
        assert_eq!(exit_code_for(&error), 2);
    }

    #[test]
    fn an_error_built_by_the_helper_is_permanent() {
        let error = permanent("the output directory is not empty");

        assert!(is_permanent(&error));
        assert_eq!(exit_code_for(&error), EXIT_PERMANENT);
    }

    #[test]
    fn the_marker_survives_further_context() {
        // The marker is attached where the problem is recognised, then more
        // context is layered on as the error propagates. It must still be
        // found at the top — this is the case anyhow's `chain()` misses, and
        // the reason detection uses downcast_ref.
        let by_context = anyhow!("moov atom not found")
            .context(Permanent)
            .context("probing /in/clip.mp4")
            .context("processing video clip");
        assert!(is_permanent(&by_context));

        let by_helper = permanent("bad input")
            .context("probing /in/clip.mp4")
            .context("processing video clip");
        assert!(is_permanent(&by_helper));
    }

    #[test]
    fn the_helper_keeps_the_real_message_at_the_head() {
        // What an operator reads first has to be the actual problem, not the
        // classification.
        let error = permanent("the output directory is not empty; pass --force");

        assert!(error.to_string().contains("--force"), "got: {error}");
        // The marker is still in the rendered chain.
        assert!(
            format!("{error:#}").contains("not retryable"),
            "got: {error:#}"
        );
    }

    #[test]
    fn marking_an_existing_error_preserves_its_cause() {
        let error = anyhow!("moov atom not found").context(Permanent);
        let rendered = format!("{error:#}");

        // The underlying cause survives, which is what makes the failure
        // manifest useful.
        assert!(rendered.contains("moov atom"), "got: {rendered}");
        assert!(rendered.contains("not retryable"), "got: {rendered}");
    }

    #[test]
    fn the_two_codes_are_distinct_and_nonzero() {
        // 0 would mean success; equal codes would make the distinction useless.
        assert_ne!(EXIT_PERMANENT, EXIT_TRANSIENT);
        assert_ne!(EXIT_PERMANENT, 0);
        assert_ne!(EXIT_TRANSIENT, 0);
    }
}

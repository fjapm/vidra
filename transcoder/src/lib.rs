//! Vidra: a video in, an HLS ladder out.
//!
//! Split from the binary so the integration tests in `tests/` can exercise the
//! real modules rather than reimplementing them.
//!
//! The modules divide along one line: [`pipeline`] and [`cli`] know about the
//! filesystem layout and the job as a whole, while everything else is a pure
//! function of its inputs — a path in, bytes or FFmpeg arguments out. Keeping
//! that split is what makes most of this crate testable without a video file.

pub mod captions;
pub mod cli;
pub mod error;
pub mod hls;
pub mod ladder;
pub mod manifest;
pub mod pipeline;
pub mod probe;
pub mod process;
pub mod thumbnails;

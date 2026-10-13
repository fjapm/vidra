//! vidra-runner: claims transcode jobs, runs vidra, stores the output.
//!
//! The division of labour across the crate:
//!
//! * [`source`] — the one provider-specific seam. A [`source::JobSource`]
//!   yields normalised [`source::Job`]s, so no provider's event shape reaches
//!   anything else.
//! * [`cloudflare`] — the only implementation today. Everything
//!   Cloudflare-shaped is sealed inside it.
//! * [`storage`] — object access over the S3 API. Not a seam: every provider
//!   worth supporting speaks S3, so configuration covers them all.
//! * [`layout`] — where sources and output live in the bucket.
//! * [`job`] — one job, end to end. The only module that knows vidra is a
//!   subprocess.
//! * [`webhook`] — optional signed notification when a video settles.

pub mod cloudflare;
pub mod config;
pub mod job;
pub mod layout;
pub mod source;
pub mod storage;
pub mod webhook;

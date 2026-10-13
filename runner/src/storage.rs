//! Object storage over the S3 API.
//!
//! There is no trait here on purpose. Every provider worth supporting speaks
//! S3 — R2, AWS, MinIO, Backblaze B2, DigitalOcean Spaces, Wasabi, Ceph — and
//! they differ only in three settings, so configuration does the job that
//! polymorphism would otherwise be asked to do. A trait with one permanent
//! implementation is just ceremony.
//!
//! The three settings, and why they differ:
//!
//! | Setting      | R2                        | AWS S3                 |
//! |--------------|---------------------------|------------------------|
//! | `endpoint`   | account-specific URL      | unset (SDK default)    |
//! | `region`     | must be `auto`            | a real region          |
//! | path style   | required                  | virtual-hosted preferred |
//!
//! One more R2 quirk worth knowing: it rejects `aws s3`-style checksum
//! trailers, so uploads here use plain `PutObject` rather than the SDK's
//! checksum-enabled flows. That is also perfectly valid against AWS, so the
//! same code path serves both.

use anyhow::{bail, Context, Result};
use aws_credential_types::Credentials;
use aws_sdk_s3::config::{BehaviorVersion, Region};
use aws_sdk_s3::primitives::ByteStream;
use aws_sdk_s3::Client;
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tracing::{debug, instrument};

/// Cache-Control for immutable output: segments and init files never change
/// for a given video, so they can be cached forever.
pub const IMMUTABLE_CACHE_CONTROL: &str = "public, max-age=31536000, immutable";

/// How to reach an S3-compatible bucket.
#[derive(Debug, Clone)]
pub struct StorageConfig {
    pub bucket: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub region: String,
    /// Custom endpoint. `None` uses the SDK's default, which is what AWS S3
    /// wants; R2 and MinIO require one.
    pub endpoint: Option<String>,
    /// Address buckets as a path component rather than a DNS subdomain.
    /// Required by R2 and MinIO, unnecessary on AWS.
    pub force_path_style: bool,
}

#[derive(Clone, Debug)]
pub struct Storage {
    client: Client,
    bucket: String,
}

impl Storage {
    pub fn new(config: &StorageConfig) -> Result<Self> {
        if config.bucket.is_empty() {
            bail!("the storage bucket must not be empty");
        }

        let credentials = Credentials::new(
            config.access_key_id.clone(),
            config.secret_access_key.clone(),
            None,
            None,
            "vidra-runner-config",
        );

        let mut builder = aws_sdk_s3::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new(config.region.clone()))
            .credentials_provider(credentials)
            .force_path_style(config.force_path_style);

        if let Some(endpoint) = &config.endpoint {
            builder = builder.endpoint_url(endpoint);
        }

        Ok(Self {
            client: Client::from_conf(builder.build()),
            bucket: config.bucket.clone(),
        })
    }

    /// Download an object to a local file.
    ///
    /// Streamed to disk rather than buffered: a 1080p source can be several
    /// gigabytes, and the scratch volume exists precisely so that it, not RAM,
    /// absorbs that.
    #[instrument(skip(self), fields(bucket = %self.bucket))]
    pub async fn download_to_file(&self, key: &str, destination: &Path) -> Result<u64> {
        let mut response = self
            .client
            .get_object()
            .bucket(&self.bucket)
            .key(key)
            .send()
            .await
            .with_context(|| format!("could not GET {key}"))?;

        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .with_context(|| format!("could not create {}", parent.display()))?;
        }

        let mut file = tokio::fs::File::create(destination)
            .await
            .with_context(|| format!("could not create {}", destination.display()))?;

        let mut written: u64 = 0;
        while let Some(chunk) = response
            .body
            .try_next()
            .await
            .with_context(|| format!("error streaming {key}"))?
        {
            file.write_all(&chunk)
                .await
                .with_context(|| format!("error writing {}", destination.display()))?;
            written += chunk.len() as u64;
        }

        // Without an explicit flush and fsync the tail of the file can still
        // be buffered when FFmpeg opens it.
        file.flush().await.context("could not flush the download")?;
        file.sync_all()
            .await
            .context("could not fsync the download")?;

        debug!(key, bytes = written, "downloaded");
        Ok(written)
    }

    /// Upload a local file.
    #[instrument(skip(self, content_type, cache_control), fields(bucket = %self.bucket))]
    pub async fn upload_file(
        &self,
        path: &Path,
        key: &str,
        content_type: &str,
        cache_control: Option<&str>,
    ) -> Result<()> {
        let body = ByteStream::from_path(path)
            .await
            .with_context(|| format!("could not read {} for upload", path.display()))?;

        let mut request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(body)
            .content_type(content_type);

        if let Some(cache_control) = cache_control {
            request = request.cache_control(cache_control);
        }

        request
            .send()
            .await
            .with_context(|| format!("could not PUT {key}"))?;

        Ok(())
    }

    /// Upload in-memory bytes. For the manifest.
    pub async fn upload_bytes(
        &self,
        data: Vec<u8>,
        key: &str,
        content_type: &str,
        cache_control: Option<&str>,
    ) -> Result<()> {
        let mut request = self
            .client
            .put_object()
            .bucket(&self.bucket)
            .key(key)
            .body(ByteStream::from(data))
            .content_type(content_type);

        if let Some(cache_control) = cache_control {
            request = request.cache_control(cache_control);
        }

        request
            .send()
            .await
            .with_context(|| format!("could not PUT {key}"))?;

        Ok(())
    }

    /// List every key under a prefix, following continuation tokens.
    pub async fn list_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut continuation: Option<String> = None;

        loop {
            let mut request = self
                .client
                .list_objects_v2()
                .bucket(&self.bucket)
                .prefix(prefix);

            if let Some(token) = &continuation {
                request = request.continuation_token(token);
            }

            let response = request
                .send()
                .await
                .with_context(|| format!("could not list {prefix}"))?;

            for object in response.contents() {
                if let Some(key) = object.key() {
                    keys.push(key.to_string());
                }
            }

            // Ignoring is_truncated silently caps listings at 1000 keys, which
            // a long video's segment list will exceed.
            if response.is_truncated().unwrap_or(false) {
                continuation = response.next_continuation_token().map(str::to_string);
                if continuation.is_none() {
                    break;
                }
            } else {
                break;
            }
        }

        Ok(keys)
    }

    /// Delete every key under a prefix.
    ///
    /// Used before uploading a rendition set so that reprocessing is
    /// idempotent: without it, a video re-encoded with a shorter ladder would
    /// leave stale objects that the new playlists do not reference but which
    /// still cost storage and could be served.
    ///
    /// `guard` must be a non-empty prefix that the target is required to start
    /// with — a last line of defence against a bug elsewhere turning this into
    /// "delete the bucket".
    pub async fn delete_prefix(&self, prefix: &str, guard: &str) -> Result<usize> {
        if guard.is_empty() {
            bail!("refusing to delete without a guard prefix");
        }
        if prefix.is_empty() || !prefix.starts_with(guard) || prefix == guard {
            bail!("refusing to delete the unsafe prefix {prefix:?} (guard {guard:?})");
        }

        let keys = self.list_prefix(prefix).await?;
        if keys.is_empty() {
            return Ok(0);
        }

        // delete_objects caps at 1000 keys per request.
        let mut deleted = 0;
        for chunk in keys.chunks(1000) {
            let mut identifiers = Vec::with_capacity(chunk.len());
            for key in chunk {
                identifiers.push(
                    aws_sdk_s3::types::ObjectIdentifier::builder()
                        .key(key)
                        .build()
                        .context("could not build a delete identifier")?,
                );
            }

            let delete = aws_sdk_s3::types::Delete::builder()
                .set_objects(Some(identifiers))
                .quiet(true)
                .build()
                .context("could not build the delete request")?;

            self.client
                .delete_objects()
                .bucket(&self.bucket)
                .delete(delete)
                .send()
                .await
                .with_context(|| format!("could not delete objects under {prefix}"))?;

            deleted += chunk.len();
        }

        debug!(prefix, deleted, "cleared prefix");
        Ok(deleted)
    }
}

/// Content-Type for an output file, chosen by extension.
///
/// Players are sensitive to these: an `.m3u8` served as `text/plain` is ignored
/// by Safari, and an `.m4s` served as `application/octet-stream` breaks some
/// decoders. A delivery layer in front of the bucket may well set its own
/// headers, but the object metadata has to be right for anything that serves
/// the bucket directly.
pub fn content_type_for(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if name.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if name.ends_with(".m4s") {
        "video/iso.segment"
    } else if name.ends_with(".mp4") {
        "video/mp4"
    } else if name.ends_with(".vtt") {
        "text/vtt"
    } else if name.ends_with(".jpg") || name.ends_with(".jpeg") {
        "image/jpeg"
    } else if name.ends_with(".json") {
        "application/json"
    } else {
        "application/octet-stream"
    }
}

/// Whether a file may be cached forever.
///
/// Segments and init files never change for a given video. Playlists and the
/// manifest do change when a video is reprocessed, so they must not be
/// immutable — a stale playlist would point at segments that no longer exist.
pub fn cache_control_for(path: &Path) -> Option<&'static str> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();

    if name.ends_with(".m4s") || name == "init.mp4" {
        Some(IMMUTABLE_CACHE_CONTROL)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn config() -> StorageConfig {
        StorageConfig {
            bucket: "video".into(),
            access_key_id: "key".into(),
            secret_access_key: "secret".into(),
            region: "auto".into(),
            endpoint: Some("https://acc.r2.cloudflarestorage.com".into()),
            force_path_style: true,
        }
    }

    #[test]
    fn a_client_is_built_for_an_r2_style_configuration() {
        assert!(Storage::new(&config()).is_ok());
    }

    #[test]
    fn a_client_is_built_for_an_aws_style_configuration() {
        // No endpoint, a real region, virtual-hosted addressing: the same code
        // path has to serve this.
        let mut config = config();
        config.endpoint = None;
        config.region = "eu-central-1".into();
        config.force_path_style = false;

        assert!(Storage::new(&config).is_ok());
    }

    #[test]
    fn an_empty_bucket_is_rejected() {
        let mut config = config();
        config.bucket.clear();

        let error = Storage::new(&config).expect_err("should be rejected");
        assert!(error.to_string().contains("bucket"), "got: {error}");
    }

    // --- delete guard -----------------------------------------------------
    // The guard is checked before any network call, so these run offline.

    #[tokio::test]
    async fn deleting_without_a_guard_is_refused() {
        let storage = Storage::new(&config()).unwrap();
        assert!(storage.delete_prefix("media/clip/", "").await.is_err());
    }

    #[tokio::test]
    async fn deleting_outside_the_guard_is_refused() {
        let storage = Storage::new(&config()).unwrap();

        for prefix in ["", "source/clip/", "other/clip/", "/", "clip/"] {
            assert!(
                storage.delete_prefix(prefix, "media/").await.is_err(),
                "prefix {prefix:?} should be refused"
            );
        }
    }

    #[tokio::test]
    async fn deleting_the_guard_itself_is_refused() {
        // "media/" would be every video in the bucket.
        let storage = Storage::new(&config()).unwrap();
        assert!(storage.delete_prefix("media/", "media/").await.is_err());
    }

    // --- content types ----------------------------------------------------

    #[test]
    fn content_types_match_the_output_tree() {
        let cases = [
            ("master.m3u8", "application/vnd.apple.mpegurl"),
            ("index.m3u8", "application/vnd.apple.mpegurl"),
            ("seg_00001.m4s", "video/iso.segment"),
            ("init.mp4", "video/mp4"),
            ("en.vtt", "text/vtt"),
            ("thumbs.vtt", "text/vtt"),
            ("poster.jpg", "image/jpeg"),
            ("thumbs.jpg", "image/jpeg"),
            ("manifest.json", "application/json"),
        ];

        for (name, expected) in cases {
            assert_eq!(
                content_type_for(&PathBuf::from(name)),
                expected,
                "for {name}"
            );
        }
    }

    #[test]
    fn content_type_is_case_insensitive_and_uses_the_full_path() {
        assert_eq!(
            content_type_for(&PathBuf::from("MASTER.M3U8")),
            "application/vnd.apple.mpegurl"
        );
        assert_eq!(
            content_type_for(&PathBuf::from("/out/1080p/seg_00042.m4s")),
            "video/iso.segment"
        );
    }

    #[test]
    fn content_type_falls_back_for_unknown_extensions() {
        assert_eq!(
            content_type_for(&PathBuf::from("mystery.xyz")),
            "application/octet-stream"
        );
        assert_eq!(
            content_type_for(&PathBuf::from("noextension")),
            "application/octet-stream"
        );
    }

    #[test]
    fn segments_and_init_files_are_immutable() {
        assert_eq!(
            cache_control_for(&PathBuf::from("seg_00001.m4s")),
            Some(IMMUTABLE_CACHE_CONTROL)
        );
        assert_eq!(
            cache_control_for(&PathBuf::from("/out/720p/init.mp4")),
            Some(IMMUTABLE_CACHE_CONTROL)
        );
    }

    #[test]
    fn playlists_and_manifests_are_not_immutable() {
        // Reprocessing rewrites these, so an immutable cache entry would pin a
        // playlist that references deleted segments.
        for name in [
            "master.m3u8",
            "index.m3u8",
            "manifest.json",
            "en.vtt",
            "poster.jpg",
        ] {
            assert_eq!(cache_control_for(&PathBuf::from(name)), None, "for {name}");
        }
    }
}

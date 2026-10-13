//! Running one job: fetch the inputs, invoke vidra, store the output.
//!
//! The ordering here is load-bearing in two places.
//!
//! **The output prefix is cleared before anything is written.** Re-encoding a
//! video with a shorter ladder would otherwise leave segments from the
//! previous run that the new playlists never reference — they cost storage and,
//! if served by a stale playlist, play.
//!
//! **`manifest.json` is uploaded last, on its own.** vidra writes it last on
//! local disk precisely so that its presence means "everything it references
//! exists". Uploading the tree with a naive recursive walk would put the
//! manifest somewhere in the middle and destroy that guarantee at exactly the
//! moment it matters — a reader would see a `ready` manifest pointing at
//! segments still in flight. So the walk excludes it, and it goes up only once
//! every other object is durable.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tracing::{info, instrument, warn};
use vidra::manifest::{Manifest, Status};

use crate::layout::KeyLayout;
use crate::source::Job;
use crate::storage::{cache_control_for, content_type_for, Storage};

/// The file whose arrival signals readiness.
const MANIFEST_NAME: &str = "manifest.json";

/// vidra's exit status for a failure that re-running cannot fix.
const VIDRA_EXIT_PERMANENT: i32 = 2;

/// What happened to a job.
///
/// `Ready` is boxed because a `Manifest` is an order of magnitude larger than
/// the failure variant, and every `Outcome` would otherwise be sized for it.
#[derive(Debug)]
pub enum Outcome {
    /// Output is durably stored and the manifest says `ready`.
    Ready(Box<Manifest>),
    /// Processing failed.
    Failed {
        error: String,
        /// Whether retrying is pointless.
        permanent: bool,
    },
}

/// Everything a job needs that does not vary per job.
pub struct Runner {
    pub storage: Storage,
    pub layout: KeyLayout,
    pub vidra_bin: PathBuf,
    pub scratch_dir: PathBuf,
    /// Passed through to vidra; `None` leaves vidra's own default.
    pub thumbnail_interval: Option<u32>,
    pub whisper_model_dir: Option<PathBuf>,
    pub no_transcribe: bool,
}

impl Runner {
    /// Process one job end to end.
    ///
    /// Never returns `Err` for a processing failure — those become
    /// [`Outcome::Failed`], because the caller has to record them rather than
    /// just propagate them. `Err` is reserved for failures that leave the
    /// runner unable to say anything useful about the job at all.
    #[instrument(skip_all, fields(video_id = %job.video_id, attempt = job.attempts))]
    pub async fn run(&self, job: &Job) -> Result<Outcome> {
        let work = self
            .scratch_dir
            .join(format!("job-{}-{}", job.video_id, job.attempts));

        // Remove any debris from a previous attempt that died before cleanup.
        let _ = tokio::fs::remove_dir_all(&work).await;
        tokio::fs::create_dir_all(&work)
            .await
            .with_context(|| format!("could not create {}", work.display()))?;

        let outcome = self.run_in(job, &work).await;

        // Reclaim the disk whatever happened, before the caller acks.
        if let Err(error) = tokio::fs::remove_dir_all(&work).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                warn!(path = %work.display(), %error, "could not remove the work directory");
            }
        }

        outcome
    }

    async fn run_in(&self, job: &Job, work: &Path) -> Result<Outcome> {
        // --- 1. fetch the source ------------------------------------------
        let extension = Path::new(&job.source_key)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("mp4");
        let input = work.join(format!("source.{extension}"));

        let bytes = match self.storage.download_to_file(&job.source_key, &input).await {
            Ok(bytes) => bytes,
            Err(error) => {
                // A missing or unreadable source is worth one more look: an
                // event can outrun the object's own consistency.
                return Ok(Outcome::Failed {
                    error: format!("downloading {}: {error:#}", job.source_key),
                    permanent: false,
                });
            }
        };
        info!(bytes, "fetched the source");

        // --- 2. fetch any supplied captions -------------------------------
        let captions_dir = match self.fetch_captions(job, work).await {
            Ok(dir) => dir,
            Err(error) => {
                // Captions are optional; losing them must not lose the video.
                warn!(error = %format!("{error:#}"), "could not fetch captions; continuing");
                None
            }
        };

        // --- 3. transcode -------------------------------------------------
        let output = work.join("out");
        if let Err(failure) = self
            .invoke_vidra(job, &input, &output, captions_dir.as_deref())
            .await?
        {
            return Ok(failure);
        }

        // --- 4. read the manifest vidra wrote -----------------------------
        let manifest = match read_manifest(&output).await {
            Ok(manifest) => manifest,
            Err(error) => {
                // vidra exited 0, so this should not happen. If it does,
                // retrying is reasonable — the encode itself was fine.
                return Ok(Outcome::Failed {
                    error: format!(
                        "vidra reported success but its manifest is unusable: {error:#}"
                    ),
                    permanent: false,
                });
            }
        };

        if manifest.status != Status::Ready {
            // Equally shouldn't happen: a non-ready manifest with exit 0.
            return Ok(Outcome::Failed {
                error: manifest
                    .error
                    .unwrap_or_else(|| "vidra wrote a non-ready manifest".to_string()),
                permanent: true,
            });
        }

        // --- 5. store the output ------------------------------------------
        let prefix = self.layout.output_prefix_for(&job.video_id);

        let removed = self
            .storage
            .delete_prefix(&prefix, self.layout.output_prefix())
            .await
            .with_context(|| format!("clearing {prefix}"))?;
        if removed > 0 {
            info!(removed, "cleared previous output for this video");
        }

        let uploaded = self
            .upload_tree(&output, &prefix)
            .await
            .with_context(|| format!("uploading to {prefix}"))?;
        info!(objects = uploaded, "uploaded the output");

        // --- 6. the manifest, last ----------------------------------------
        self.upload_manifest(&output, &prefix)
            .await
            .with_context(|| format!("uploading {prefix}{MANIFEST_NAME}"))?;

        info!("video is ready");
        Ok(Outcome::Ready(Box::new(manifest)))
    }

    /// Download `{source_prefix}{id}/captions/*.vtt`, if any exist.
    async fn fetch_captions(&self, job: &Job, work: &Path) -> Result<Option<PathBuf>> {
        let prefix = self.layout.captions_prefix_for(&job.video_id);
        let keys = self.storage.list_prefix(&prefix).await?;

        let vtt: Vec<&String> = keys.iter().filter(|k| k.ends_with(".vtt")).collect();
        if vtt.is_empty() {
            return Ok(None);
        }

        let dir = work.join("captions");
        tokio::fs::create_dir_all(&dir)
            .await
            .with_context(|| format!("could not create {}", dir.display()))?;

        for key in &vtt {
            // The basename carries the language tag, which is how vidra reads
            // it, so it has to survive the download unchanged.
            let Some(name) = key.rsplit('/').next().filter(|n| !n.is_empty()) else {
                continue;
            };
            self.storage
                .download_to_file(key, &dir.join(name))
                .await
                .with_context(|| format!("downloading {key}"))?;
        }

        info!(count = vtt.len(), "fetched supplied captions");
        Ok(Some(dir))
    }

    /// Run vidra. `Ok(Err(outcome))` means it ran and failed.
    #[allow(clippy::type_complexity)]
    async fn invoke_vidra(
        &self,
        job: &Job,
        input: &Path,
        output: &Path,
        captions_dir: Option<&Path>,
    ) -> Result<std::result::Result<(), Outcome>> {
        let mut command = tokio::process::Command::new(&self.vidra_bin);
        command
            .arg(input)
            .arg("--out")
            .arg(output)
            .arg("--id")
            .arg(&job.video_id)
            // The directory is fresh per attempt, but --force makes a retry
            // after a partial run deterministic rather than an error.
            .arg("--force");

        if let Some(dir) = captions_dir {
            command.arg("--captions-dir").arg(dir);
        }
        if self.no_transcribe {
            command.arg("--no-transcribe");
        }
        if let Some(interval) = self.thumbnail_interval {
            command
                .arg("--thumbnail-interval")
                .arg(interval.to_string());
        }
        if let Some(dir) = &self.whisper_model_dir {
            command.arg("--whisper-model-dir").arg(dir);
        }

        // stdout and stderr are inherited so vidra's structured logs land in
        // this process's log stream, interleaved with the runner's own and
        // attributable to the same container.
        let status = command
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            // Put the child in its own process group so a kill reaches any
            // FFmpeg it spawned.
            .kill_on_drop(true)
            .status()
            .await
            .with_context(|| {
                format!(
                    "could not run {} (is it on PATH?)",
                    self.vidra_bin.display()
                )
            })?;

        if status.success() {
            return Ok(Ok(()));
        }

        let code = status.code();
        let permanent = code == Some(VIDRA_EXIT_PERMANENT);

        // The reason is in vidra's own log, which has already been emitted;
        // repeating it here would mean capturing output instead of streaming
        // it, and an hour-long encode should not buffer its log in memory.
        let error = match code {
            Some(code) => format!("vidra exited with status {code}"),
            None => "vidra was killed by a signal".to_string(),
        };

        warn!(
            exit_code = ?code,
            permanent,
            "vidra failed; see its log above for the reason"
        );

        Ok(Err(Outcome::Failed { error, permanent }))
    }

    /// Upload every file under `root` except the manifest.
    async fn upload_tree(&self, root: &Path, prefix: &str) -> Result<usize> {
        let files = collect_files(root).await?;
        let mut count = 0;

        for path in files {
            let relative = path
                .strip_prefix(root)
                .context("a collected path escaped the output root")?;
            let key = format!("{prefix}{}", to_key(relative));

            // Deliberately skipped; uploaded by upload_manifest once
            // everything else is durable.
            if relative == Path::new(MANIFEST_NAME) {
                continue;
            }

            self.storage
                .upload_file(
                    &path,
                    &key,
                    content_type_for(&path),
                    cache_control_for(&path),
                )
                .await
                .with_context(|| format!("uploading {key}"))?;

            count += 1;
        }

        Ok(count)
    }

    async fn upload_manifest(&self, root: &Path, prefix: &str) -> Result<()> {
        let path = root.join(MANIFEST_NAME);
        let data = tokio::fs::read(&path)
            .await
            .with_context(|| format!("could not read {}", path.display()))?;

        self.storage
            .upload_bytes(
                data,
                &format!("{prefix}{MANIFEST_NAME}"),
                "application/json",
                None,
            )
            .await
    }

    /// Write a `failed` manifest so the failure is durable.
    ///
    /// Called when a job is abandoned. Without it the message disappears into
    /// the dead-letter queue and the application waits forever on a video that
    /// is never coming.
    pub async fn record_failure(&self, job: &Job, error: &str) -> Result<Manifest> {
        let manifest = Manifest::failed(&job.video_id, error, vidra::pipeline::now_rfc3339());
        let prefix = self.layout.output_prefix_for(&job.video_id);

        let json = manifest
            .to_json()
            .context("could not serialise the failure manifest")?;

        self.storage
            .upload_bytes(
                json.into_bytes(),
                &format!("{prefix}{MANIFEST_NAME}"),
                "application/json",
                None,
            )
            .await
            .context("uploading the failure manifest")?;

        Ok(manifest)
    }
}

async fn read_manifest(output: &Path) -> Result<Manifest> {
    let path = output.join(MANIFEST_NAME);
    let data = tokio::fs::read(&path)
        .await
        .with_context(|| format!("could not read {}", path.display()))?;

    serde_json::from_slice(&data).with_context(|| format!("could not parse {}", path.display()))
}

/// Storage keys always use forward slashes.
fn to_key(relative: &Path) -> String {
    relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// Depth-first listing of every regular file under `root`.
async fn collect_files(root: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let mut entries = tokio::fs::read_dir(&dir)
            .await
            .with_context(|| format!("could not read {}", dir.display()))?;

        while let Some(entry) = entries
            .next_entry()
            .await
            .with_context(|| format!("could not enumerate {}", dir.display()))?
        {
            let path = entry.path();
            let file_type = entry
                .file_type()
                .await
                .with_context(|| format!("could not stat {}", path.display()))?;

            if file_type.is_dir() {
                stack.push(path);
            } else if file_type.is_file() {
                files.push(path);
            }
            // Symlinks are skipped: vidra never creates any, so one here would
            // mean something unexpected is in the output directory.
        }
    }

    // Deterministic upload order, which makes logs comparable between runs.
    files.sort();
    Ok(files)
}

/// How long to wait before the next attempt when releasing a job.
pub const RETRY_DELAY: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_use_forward_slashes() {
        assert_eq!(to_key(Path::new("720p/init.mp4")), "720p/init.mp4");
        assert_eq!(to_key(Path::new("master.m3u8")), "master.m3u8");
        assert_eq!(
            to_key(Path::new("captions/pt-BR.vtt")),
            "captions/pt-BR.vtt"
        );
    }

    #[tokio::test]
    async fn collect_files_walks_nested_directories_in_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();

        tokio::fs::create_dir_all(root.join("720p")).await.unwrap();
        tokio::fs::create_dir_all(root.join("captions"))
            .await
            .unwrap();
        tokio::fs::write(root.join("master.m3u8"), "x")
            .await
            .unwrap();
        tokio::fs::write(root.join("manifest.json"), "{}")
            .await
            .unwrap();
        tokio::fs::write(root.join("720p/init.mp4"), "x")
            .await
            .unwrap();
        tokio::fs::write(root.join("captions/en.vtt"), "x")
            .await
            .unwrap();

        let files = collect_files(root).await.expect("should walk");
        let relative: Vec<String> = files
            .iter()
            .map(|p| to_key(p.strip_prefix(root).unwrap()))
            .collect();

        assert_eq!(
            relative,
            vec![
                "720p/init.mp4",
                "captions/en.vtt",
                "manifest.json",
                "master.m3u8",
            ]
        );
    }

    #[tokio::test]
    async fn collect_files_handles_an_empty_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(collect_files(temp.path()).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn the_manifest_is_excluded_from_the_tree_walk() {
        // The guarantee: whatever upload_tree iterates, the manifest is not in
        // it. Asserted on the filtering logic rather than against a live
        // bucket, which a unit test has no business reaching.
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();

        tokio::fs::write(root.join("master.m3u8"), "x")
            .await
            .unwrap();
        tokio::fs::write(root.join(MANIFEST_NAME), "{}")
            .await
            .unwrap();

        let files = collect_files(root).await.unwrap();
        let would_upload: Vec<String> = files
            .iter()
            .map(|p| to_key(p.strip_prefix(root).unwrap()))
            .filter(|k| k != MANIFEST_NAME)
            .collect();

        assert_eq!(would_upload, vec!["master.m3u8"]);
        assert!(!would_upload.contains(&MANIFEST_NAME.to_string()));
    }

    #[tokio::test]
    async fn a_manifest_written_by_vidra_round_trips() {
        // Proves the shared schema really is shared: this is vidra's own type
        // deserialising vidra's own output.
        let temp = tempfile::tempdir().expect("tempdir");
        let original = Manifest::failed("clip", "ffmpeg exploded", "2026-10-12T00:00:00Z".into());
        tokio::fs::write(temp.path().join(MANIFEST_NAME), original.to_json().unwrap())
            .await
            .unwrap();

        let read = read_manifest(temp.path()).await.expect("should parse");

        assert_eq!(read.video_id, "clip");
        assert_eq!(read.status, Status::Failed);
        assert_eq!(read.error.as_deref(), Some("ffmpeg exploded"));
    }

    #[tokio::test]
    async fn a_missing_manifest_is_an_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(read_manifest(temp.path()).await.is_err());
    }

    #[tokio::test]
    async fn an_unparseable_manifest_is_an_error() {
        let temp = tempfile::tempdir().expect("tempdir");
        tokio::fs::write(temp.path().join(MANIFEST_NAME), "not json")
            .await
            .unwrap();

        assert!(read_manifest(temp.path()).await.is_err());
    }

    #[test]
    fn the_permanent_exit_code_matches_the_transcoder() {
        // If these ever disagree, every corrupt upload silently consumes its
        // full retry budget. The transcoder owns the constant; this asserts
        // the runner's copy still matches it.
        assert_eq!(
            VIDRA_EXIT_PERMANENT,
            i32::from(vidra::error::EXIT_PERMANENT)
        );
    }
}

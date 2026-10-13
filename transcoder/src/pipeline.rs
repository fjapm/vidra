//! Processing one video, end to end.
//!
//! Ordering is deliberate and load-bearing:
//!   1. probe the input;
//!   2. encode the ladder and package HLS;
//!   3. thumbnails;
//!   4. captions (copy the supplied ones, or transcribe);
//!   5. the master playlist;
//!   6. the manifest, *last*.
//!
//! The manifest is written last on purpose: it is the readiness signal, so it
//! must not claim `ready` until everything it references is on disk. A run
//! killed halfway therefore leaves an output directory with no `manifest.json`
//! — incomplete, and recognisably so.
//!
//! Encoding writes straight into the output directory rather than staging in
//! scratch and copying at the end. A staged copy would mean moving the entire
//! ladder — gigabytes — across what is usually a volume boundary, and
//! `rename` cannot be relied on between two different mounts. The readiness
//! contract above is what makes writing in place safe.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::{info, instrument, warn};

use crate::captions::{self, CaptionTrack};
use crate::cli::Resolved;
use crate::error::Permanent;
use crate::hls::{self, SubtitleTrack};
use crate::ladder::{self, Rendition};
use crate::manifest::{Manifest, ManifestCaption, ReadyManifest, SourceSummary};
use crate::probe::{self, SourceInfo};
use crate::process::run_checked;

/// Cap on sprite tiles, so a very long video cannot produce an enormous sheet.
const MAX_SPRITE_TILES: u32 = 400;

/// Outcome of a successful run.
#[derive(Debug)]
pub struct Processed {
    pub manifest: Manifest,
}

/// RFC 3339 UTC timestamp for `processed_at`.
pub fn now_rfc3339() -> String {
    use time::format_description::well_known::Rfc3339;
    time::OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "unknown".to_string())
}

/// Process one video.
///
/// `scratch_root` is a per-run directory that the caller removes afterwards,
/// whatever the outcome.
#[instrument(skip_all, fields(video_id = %options.id))]
pub async fn process_video(options: &Resolved, scratch_root: &Path) -> Result<Processed> {
    let input_path = options.input();
    let output_root = options.out();

    tokio::fs::create_dir_all(scratch_root)
        .await
        .with_context(|| format!("could not create {}", scratch_root.display()))?;

    // --- 1. probe ---------------------------------------------------------
    // The input is read in place: it is already a local, seekable file, which
    // is all FFmpeg needs to find the moov atom.
    //
    // A probe failure is permanent: ffprobe refusing the file means it is
    // corrupt, truncated or in a container we cannot read, and none of those
    // change on a second attempt.
    let info = probe::probe(input_path).await.context(Permanent)?;
    info!(
        width = info.width,
        height = info.height,
        fps = info.fps,
        duration = info.duration_seconds,
        has_audio = info.has_audio,
        "probed the source"
    );

    // --- 2. the ladder ----------------------------------------------------
    let renditions = ladder::build_ladder(&info);
    info!(count = renditions.len(), "encoding the ladder");

    for rendition in &renditions {
        encode_rendition(input_path, &info, rendition, output_root).await?;
    }

    // --- 3. thumbnails ----------------------------------------------------
    let thumbnails = crate::thumbnails::generate(
        input_path,
        output_root,
        info.duration_seconds,
        options.args.thumbnail_interval,
        MAX_SPRITE_TILES,
    )
    .await
    .context("generating thumbnails")?;

    let thumbs_vtt = hls::thumbnail_vtt(
        &thumbnails.info.sprite,
        thumbnails.info.interval_seconds,
        thumbnails.info.tile_width,
        thumbnails.info.tile_height,
        thumbnails.info.columns,
        thumbnails.info.count,
    );
    tokio::fs::write(output_root.join("thumbs.vtt"), &thumbs_vtt)
        .await
        .context("writing thumbs.vtt")?;

    // --- 4. captions ------------------------------------------------------
    let caption_tracks = resolve_captions(options, input_path, scratch_root, &info)
        .await
        .context("resolving captions")?;

    let captions_dir = output_root.join("captions");
    if !caption_tracks.is_empty() {
        tokio::fs::create_dir_all(&captions_dir)
            .await
            .context("creating the captions directory")?;
    }

    for track in &caption_tracks {
        tokio::fs::write(
            captions_dir.join(format!("{}.vtt", track.language)),
            &track.data,
        )
        .await
        .with_context(|| format!("writing the {} caption file", track.language))?;

        // Each caption track is a subtitle *rendition*, so it needs its own
        // media playlist; EXT-X-MEDIA pointing straight at a .vtt is ignored
        // by Safari.
        tokio::fs::write(
            captions_dir.join(format!("{}.m3u8", track.language)),
            hls::subtitle_playlist(&track.language, info.duration_seconds),
        )
        .await
        .with_context(|| format!("writing the {} subtitle playlist", track.language))?;
    }

    // --- 5. master playlist ----------------------------------------------
    let subtitle_tracks: Vec<SubtitleTrack> = caption_tracks
        .iter()
        .enumerate()
        .map(|(index, track)| SubtitleTrack {
            language: track.language.clone(),
            name: SubtitleTrack::display_name(&track.language),
            // Exactly one default, or players may select none.
            default: index == 0,
        })
        .collect();

    tokio::fs::write(
        output_root.join("master.m3u8"),
        hls::master_playlist(&renditions, &subtitle_tracks),
    )
    .await
    .context("writing master.m3u8")?;

    let files = count_output_files(output_root).await?;
    info!(files, "wrote the output tree");

    // --- 6. manifest, last -----------------------------------------------
    let manifest = Manifest::ready(ReadyManifest {
        video_id: &options.id,
        duration_seconds: info.duration_seconds,
        source: SourceSummary {
            width: info.width,
            height: info.height,
            fps: info.fps,
        },
        renditions: &renditions,
        captions: caption_tracks
            .iter()
            .map(|track| ManifestCaption {
                language: track.language.clone(),
                path: format!("captions/{}.vtt", track.language),
                auto_generated: track.auto_generated,
            })
            .collect(),
        poster: Some("poster.jpg".to_string()),
        thumbnails: Some(thumbnails.info.clone()),
        processed_at: now_rfc3339(),
    });

    write_manifest(output_root, &manifest).await?;

    info!("video is ready");
    Ok(Processed { manifest })
}

/// Create the output directory, and make sure it is empty.
///
/// A non-empty directory is refused unless `force` is set: re-encoding into
/// one would leave orphaned segments from the previous run that the new
/// playlists do not reference. With `force`, the *contents* are removed but
/// the directory itself is kept, since it is usually a mount point.
pub async fn prepare_output_dir(out: &Path, force: bool) -> Result<()> {
    tokio::fs::create_dir_all(out)
        .await
        .with_context(|| format!("could not create the output directory {}", out.display()))?;

    let mut entries = tokio::fs::read_dir(out)
        .await
        .with_context(|| format!("could not read {}", out.display()))?;

    let mut existing = Vec::new();
    while let Some(entry) = entries
        .next_entry()
        .await
        .with_context(|| format!("could not enumerate {}", out.display()))?
    {
        existing.push(entry.path());
    }

    if existing.is_empty() {
        return Ok(());
    }

    if !force {
        // Permanent: the directory will still be non-empty next time, so a
        // caller that retries is only burning its budget.
        return Err(crate::error::permanent(format!(
            "the output directory {} is not empty ({} entries); pass --force to overwrite it",
            out.display(),
            existing.len()
        )));
    }

    for path in &existing {
        let metadata = tokio::fs::symlink_metadata(path)
            .await
            .with_context(|| format!("could not stat {}", path.display()))?;

        if metadata.is_dir() {
            tokio::fs::remove_dir_all(path)
                .await
                .with_context(|| format!("could not remove {}", path.display()))?;
        } else {
            tokio::fs::remove_file(path)
                .await
                .with_context(|| format!("could not remove {}", path.display()))?;
        }
    }

    info!(entries = existing.len(), "cleared the output directory");
    Ok(())
}

/// Encode one rendition into `{output_root}/{height}p/`.
async fn encode_rendition(
    input_path: &Path,
    info: &SourceInfo,
    rendition: &Rendition,
    output_root: &Path,
) -> Result<()> {
    let dir = output_root.join(rendition.dir_name());
    tokio::fs::create_dir_all(&dir)
        .await
        .with_context(|| format!("could not create {}", dir.display()))?;

    let args = ladder::ffmpeg_args(
        &input_path.to_string_lossy(),
        rendition,
        info,
        &dir.to_string_lossy(),
    );

    info!(rendition = %rendition.dir_name(), "encoding");
    run_checked(
        "ffmpeg",
        &args,
        &format!("encoding the {} rendition", rendition.dir_name()),
    )
    .await?;

    Ok(())
}

/// Copy the supplied captions, or transcribe if there are none.
async fn resolve_captions(
    options: &Resolved,
    input_path: &Path,
    scratch_root: &Path,
    info: &SourceInfo,
) -> Result<Vec<CaptionTrack>> {
    let mut tracks = match &options.args.captions_dir {
        Some(dir) => read_caption_dir(dir).await?,
        None => Vec::new(),
    };

    if !tracks.is_empty() {
        // Deterministic order, so the default subtitle track does not depend
        // on the order the directory happens to enumerate in.
        tracks.sort_by(|a, b| a.language.cmp(&b.language));
        info!(count = tracks.len(), "using the supplied captions");
        return Ok(tracks);
    }

    if options.args.no_transcribe {
        info!("no caption files supplied and transcription is disabled");
        return Ok(Vec::new());
    }

    if !info.has_audio {
        info!("the source has no audio track; nothing to transcribe");
        return Ok(Vec::new());
    }

    // Fetched here rather than at startup: see captions::prepare_model.
    if !captions::prepare_model(&options.whisper_model_path()).await {
        return Ok(Vec::new());
    }

    info!("no caption files supplied; transcribing with whisper.cpp");

    match captions::transcribe(
        input_path,
        scratch_root,
        &options.whisper_model_path(),
        info.has_audio,
    )
    .await
    {
        Ok(Some(track)) => Ok(vec![track]),
        Ok(None) => Ok(Vec::new()),
        // Transcription is a nice-to-have. A whisper failure should not make a
        // perfectly good video unplayable, so it is logged and the video ships
        // without captions.
        Err(error) => {
            warn!(%error, "transcription failed; continuing without captions");
            Ok(Vec::new())
        }
    }
}

/// Read `{language}.vtt` files from a directory.
///
/// Anything that is not a usable caption file is skipped with a warning
/// rather than failing the run: the viewer is far better served by a playable
/// video with one missing subtitle track.
async fn read_caption_dir(dir: &Path) -> Result<Vec<CaptionTrack>> {
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(entries) => entries,
        // A missing directory is not an error. It means "no captions
        // supplied", which is exactly what an absent one should mean.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            warn!(path = %dir.display(), "the captions directory does not exist");
            return Ok(Vec::new());
        }
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", dir.display()))
        }
    };

    let mut tracks = Vec::new();

    while let Some(entry) = entries
        .next_entry()
        .await
        .with_context(|| format!("could not enumerate {}", dir.display()))?
    {
        let path = entry.path();

        if !entry
            .file_type()
            .await
            .with_context(|| format!("could not stat {}", path.display()))?
            .is_file()
        {
            continue;
        }

        let Some(language) = captions::language_from_caption_key(&path.to_string_lossy()) else {
            warn!(path = %path.display(), "ignoring a caption file with an unusable name");
            continue;
        };

        let data = tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading {}", path.display()))?;

        match captions::validate_vtt(&data) {
            Ok(()) => tracks.push(CaptionTrack {
                language,
                data,
                auto_generated: false,
            }),
            Err(error) => warn!(path = %path.display(), %error, "skipping an invalid caption file"),
        }
    }

    Ok(tracks)
}

/// Write `manifest.json` into the output directory.
async fn write_manifest(output_root: &Path, manifest: &Manifest) -> Result<()> {
    let json = manifest
        .to_json()
        .context("could not serialise the manifest")?;

    tokio::fs::write(output_root.join("manifest.json"), json)
        .await
        .context("writing manifest.json")
}

/// Write a `failed` manifest so the failure is durable.
///
/// Without it, a crashed run leaves a directory of half-written segments and
/// no explanation. The exit status already says the run failed; this says
/// *why*, next to the output, for whoever finds it later.
pub async fn record_failure(out: &Path, video_id: &str, error: &str) -> Result<Manifest> {
    let manifest = Manifest::failed(video_id, error, now_rfc3339());

    // The output directory may not exist if the failure happened early.
    tokio::fs::create_dir_all(out)
        .await
        .with_context(|| format!("could not create {}", out.display()))?;

    write_manifest(out, &manifest).await?;
    Ok(manifest)
}

/// Count the regular files under `root`.
///
/// Reported in the log once the tree is complete; a plausible file count is
/// the quickest check that an encode produced what it should.
async fn count_output_files(root: &Path) -> Result<usize> {
    Ok(collect_files(root).await?.len())
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
            // Symlinks are skipped: FFmpeg never creates any, so one here
            // would mean something unexpected is in the output directory.
        }
    }

    // Deterministic order, which makes logs comparable between runs.
    files.sort();
    Ok(files)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timestamp_is_rfc3339_utc() {
        let timestamp = now_rfc3339();

        assert!(timestamp.ends_with('Z'), "must be UTC: {timestamp}");
        assert!(timestamp.contains('T'), "must be RFC 3339: {timestamp}");
        // Parses back.
        assert!(time::OffsetDateTime::parse(
            &timestamp,
            &time::format_description::well_known::Rfc3339
        )
        .is_ok());
    }

    #[tokio::test]
    async fn collect_files_walks_nested_directories_in_order() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path();

        tokio::fs::create_dir_all(root.join("1080p")).await.unwrap();
        tokio::fs::create_dir_all(root.join("captions"))
            .await
            .unwrap();

        tokio::fs::write(root.join("master.m3u8"), "x")
            .await
            .unwrap();
        tokio::fs::write(root.join("1080p/init.mp4"), "x")
            .await
            .unwrap();
        tokio::fs::write(root.join("1080p/seg_00001.m4s"), "x")
            .await
            .unwrap();
        tokio::fs::write(root.join("captions/en.vtt"), "x")
            .await
            .unwrap();

        let files = collect_files(root).await.expect("should walk");
        let relative: Vec<String> = files
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();

        assert_eq!(
            relative,
            vec![
                "1080p/init.mp4",
                "1080p/seg_00001.m4s",
                "captions/en.vtt",
                "master.m3u8",
            ]
        );
    }

    #[tokio::test]
    async fn collect_files_handles_an_empty_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let files = collect_files(temp.path()).await.expect("should walk");
        assert!(files.is_empty());
    }

    // --- prepare_output_dir -----------------------------------------------

    #[tokio::test]
    async fn prepare_creates_a_missing_output_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let out = temp.path().join("nested/out");

        prepare_output_dir(&out, false)
            .await
            .expect("should create");
        assert!(out.is_dir());
    }

    #[tokio::test]
    async fn prepare_accepts_an_existing_empty_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        assert!(prepare_output_dir(temp.path(), false).await.is_ok());
    }

    #[tokio::test]
    async fn prepare_refuses_a_non_empty_directory_without_force() {
        // Encoding into a directory that already holds a previous run would
        // leave segments the new playlists never reference.
        let temp = tempfile::tempdir().expect("tempdir");
        tokio::fs::write(temp.path().join("master.m3u8"), "old")
            .await
            .unwrap();

        let error = prepare_output_dir(temp.path(), false)
            .await
            .expect_err("should refuse");

        assert!(error.to_string().contains("--force"), "got: {error}");
        // Nothing was removed.
        assert!(temp.path().join("master.m3u8").exists());
    }

    #[tokio::test]
    async fn force_clears_the_contents_but_keeps_the_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let out = temp.path();

        tokio::fs::create_dir_all(out.join("1080p")).await.unwrap();
        tokio::fs::write(out.join("1080p/seg_00001.m4s"), "x")
            .await
            .unwrap();
        tokio::fs::write(out.join("master.m3u8"), "x")
            .await
            .unwrap();

        prepare_output_dir(out, true).await.expect("should clear");

        // The directory survives, because it is usually a mount point.
        assert!(out.is_dir());
        assert!(!out.join("master.m3u8").exists());
        assert!(!out.join("1080p").exists());

        let mut entries = tokio::fs::read_dir(out).await.unwrap();
        assert!(entries.next_entry().await.unwrap().is_none());
    }

    // --- read_caption_dir -------------------------------------------------

    #[tokio::test]
    async fn caption_files_are_read_and_tagged_by_filename() {
        let temp = tempfile::tempdir().expect("tempdir");
        tokio::fs::write(temp.path().join("en.vtt"), "WEBVTT\n\n")
            .await
            .unwrap();
        tokio::fs::write(temp.path().join("pt-BR.vtt"), "WEBVTT\n\n")
            .await
            .unwrap();

        let mut tracks = read_caption_dir(temp.path()).await.expect("should read");
        tracks.sort_by(|a, b| a.language.cmp(&b.language));

        let languages: Vec<&str> = tracks.iter().map(|t| t.language.as_str()).collect();
        assert_eq!(languages, vec!["en", "pt-BR"]);
        assert!(tracks.iter().all(|t| !t.auto_generated));
    }

    #[tokio::test]
    async fn an_invalid_vtt_file_is_skipped_rather_than_failing_the_run() {
        let temp = tempfile::tempdir().expect("tempdir");
        tokio::fs::write(temp.path().join("en.vtt"), "WEBVTT\n\n")
            .await
            .unwrap();
        tokio::fs::write(temp.path().join("fr.vtt"), "not a vtt file")
            .await
            .unwrap();

        let tracks = read_caption_dir(temp.path()).await.expect("should read");

        let languages: Vec<&str> = tracks.iter().map(|t| t.language.as_str()).collect();
        assert_eq!(languages, vec!["en"]);
    }

    #[tokio::test]
    async fn files_that_are_not_language_tagged_vtt_are_ignored() {
        let temp = tempfile::tempdir().expect("tempdir");
        tokio::fs::write(temp.path().join("en.vtt"), "WEBVTT\n\n")
            .await
            .unwrap();
        tokio::fs::write(temp.path().join("notes.txt"), "x")
            .await
            .unwrap();
        tokio::fs::write(temp.path().join("subtitles.vtt"), "WEBVTT\n\n")
            .await
            .unwrap();
        tokio::fs::create_dir(temp.path().join("nested"))
            .await
            .unwrap();

        let tracks = read_caption_dir(temp.path()).await.expect("should read");

        // "subtitles" is not a language tag, and directories are not files.
        let languages: Vec<&str> = tracks.iter().map(|t| t.language.as_str()).collect();
        assert_eq!(languages, vec!["en"]);
    }

    #[tokio::test]
    async fn a_missing_captions_directory_means_no_captions() {
        let temp = tempfile::tempdir().expect("tempdir");
        let tracks = read_caption_dir(&temp.path().join("absent"))
            .await
            .expect("should not fail");

        assert!(tracks.is_empty());
    }

    // --- manifests --------------------------------------------------------

    #[tokio::test]
    async fn a_failure_manifest_is_written_even_if_the_directory_is_missing() {
        let temp = tempfile::tempdir().expect("tempdir");
        let out = temp.path().join("never-created");

        let manifest = record_failure(&out, "clip", "ffmpeg exploded")
            .await
            .expect("should write");

        assert_eq!(manifest.video_id, "clip");

        let written = tokio::fs::read_to_string(out.join("manifest.json"))
            .await
            .expect("manifest.json should exist");
        assert!(written.contains("ffmpeg exploded"), "got: {written}");
        assert!(written.contains("\"failed\""), "got: {written}");
    }
}

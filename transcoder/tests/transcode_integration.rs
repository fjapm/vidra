//! End-to-end transcode test against a real FFmpeg.
//!
//! Generates a short clip with FFmpeg's `testsrc` and `sine` sources, runs the
//! real ladder arguments over it, and then verifies with ffprobe the one
//! property that unit tests genuinely cannot check: that keyframes land at the
//! same timestamps in every rendition. That is what makes bitrate switching
//! seamless, and it is only observable in the encoded output.
//!
//! Skipped (not failed) when FFmpeg is unavailable, so `cargo test` still
//! works on a machine without it. CI installs FFmpeg, so the test runs there.
//!
//! R2 is not involved: the storage layer is a thin wrapper over the AWS SDK,
//! and standing up MinIO would test the SDK rather than this code. What is
//! exercised here is the FFmpeg contract, which is where the real risk lives.

use std::path::{Path, PathBuf};
use std::process::Command;

use vidra::hls::{self, SubtitleTrack};
use vidra::ladder::{self, GOP_SECONDS, SEGMENT_SECONDS};
use vidra::manifest::{Manifest, ManifestCaption, ReadyManifest, SourceSummary};
use vidra::probe::SourceInfo;

/// Length of the generated clip. Long enough for several segments and multiple
/// GOPs, short enough that the test stays quick.
const CLIP_SECONDS: u32 = 12;

fn tool_available(tool: &str) -> bool {
    Command::new(tool)
        .arg("-version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn require_ffmpeg() -> bool {
    if tool_available("ffmpeg") && tool_available("ffprobe") {
        return true;
    }
    eprintln!("SKIP: ffmpeg/ffprobe not on PATH; skipping the integration test");
    false
}

fn run(program: &str, args: &[String], what: &str) {
    let output = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("could not start {program} for {what}: {error}"));

    assert!(
        output.status.success(),
        "{program} failed while {what} ({}):\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Generate a test clip: colour bars plus a sine tone.
fn generate_clip(path: &Path, width: u32, height: u32, fps: u32) {
    let args: Vec<String> = vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        format!("testsrc=size={width}x{height}:rate={fps}:duration={CLIP_SECONDS}"),
        "-f".into(),
        "lavfi".into(),
        "-i".into(),
        format!("sine=frequency=440:duration={CLIP_SECONDS}"),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "ultrafast".into(),
        "-pix_fmt".into(),
        "yuv420p".into(),
        "-c:a".into(),
        "aac".into(),
        "-shortest".into(),
        path.to_string_lossy().into_owned(),
    ];

    run("ffmpeg", &args, "generating the test clip");
}

/// Keyframe timestamps of a video file, in seconds.
fn keyframe_times(path: &Path) -> Vec<f64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-skip_frame",
            "nokey",
            "-show_entries",
            "frame=pts_time",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("ffprobe should run");

    assert!(
        output.status.success(),
        "ffprobe failed on {}:\n{}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().trim_end_matches(',').parse::<f64>().ok())
        .collect()
}

/// Concatenate a rendition's init segment and media segments into one file, so
/// ffprobe can read it as a normal fMP4 stream.
fn assemble_rendition(dir: &Path, output: &Path) {
    let init = dir.join("init.mp4");
    assert!(init.is_file(), "missing init.mp4 in {}", dir.display());

    let mut segments: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("rendition directory should be readable")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().and_then(|e| e.to_str()) == Some("m4s"))
        .collect();
    segments.sort();

    assert!(
        !segments.is_empty(),
        "no .m4s segments in {}",
        dir.display()
    );

    let mut bytes = std::fs::read(&init).expect("init.mp4 should be readable");
    for segment in &segments {
        bytes.extend(std::fs::read(segment).expect("segment should be readable"));
    }
    std::fs::write(output, bytes).expect("assembled file should be writable");
}

fn source_info(width: u32, height: u32, fps: f64) -> SourceInfo {
    SourceInfo {
        width,
        height,
        fps,
        duration_seconds: f64::from(CLIP_SECONDS),
        has_audio: true,
    }
}

#[test]
fn transcodes_a_generated_clip_and_aligns_keyframes_across_renditions() {
    if !require_ffmpeg() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();

    // 720p source so the ladder has three rungs to compare (720/480/360).
    let input = root.join("source.mp4");
    generate_clip(&input, 1280, 720, 30);
    assert!(input.is_file(), "the test clip should exist");

    let info = source_info(1280, 720, 30.0);
    let renditions = ladder::build_ladder(&info);
    assert_eq!(
        renditions.len(),
        3,
        "a 720p source should yield 720p/480p/360p, got {renditions:?}"
    );

    // --- encode every rendition with the real production arguments ---------
    let output_root = root.join("out");
    for rendition in &renditions {
        let dir = output_root.join(rendition.dir_name());
        std::fs::create_dir_all(&dir).expect("rendition directory");

        let args = ladder::ffmpeg_args(
            &input.to_string_lossy(),
            rendition,
            &info,
            &dir.to_string_lossy(),
        );
        run(
            "ffmpeg",
            &args,
            &format!("encoding {}", rendition.dir_name()),
        );
    }

    // --- the expected output files exist ----------------------------------
    for rendition in &renditions {
        let dir = output_root.join(rendition.dir_name());

        assert!(
            dir.join("init.mp4").is_file(),
            "{} should contain init.mp4",
            rendition.dir_name()
        );
        assert!(
            dir.join("index.m3u8").is_file(),
            "{} should contain index.m3u8",
            rendition.dir_name()
        );

        let segments = std::fs::read_dir(&dir)
            .expect("readable")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("m4s"))
            .count();
        assert!(
            segments >= 2,
            "{} should have several segments for a {CLIP_SECONDS}s clip, got {segments}",
            rendition.dir_name()
        );

        // The playlist must declare fMP4 and VOD.
        let playlist = std::fs::read_to_string(dir.join("index.m3u8")).expect("readable");
        assert!(
            playlist.contains("#EXT-X-MAP:URI=\"init.mp4\""),
            "{} playlist should reference the init segment:\n{playlist}",
            rendition.dir_name()
        );
        assert!(
            playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"),
            "{} playlist should be VOD:\n{playlist}",
            rendition.dir_name()
        );
        assert!(
            playlist.contains("#EXT-X-ENDLIST"),
            "{} playlist should be complete:\n{playlist}",
            rendition.dir_name()
        );
    }

    // --- no downloadable MP4 anywhere in the output ------------------------
    // The only .mp4 permitted is init.mp4, which is useless on its own.
    let mut stack = vec![output_root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("readable") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) == Some("mp4") {
                assert_eq!(
                    path.file_name().and_then(|n| n.to_str()),
                    Some("init.mp4"),
                    "the only .mp4 in media/ may be init.mp4, found {}",
                    path.display()
                );
            }
        }
    }

    // --- keyframe alignment, the point of this test -----------------------
    let mut per_rendition: Vec<(String, Vec<f64>)> = Vec::new();
    for rendition in &renditions {
        let assembled = root.join(format!("{}.mp4", rendition.dir_name()));
        assemble_rendition(&output_root.join(rendition.dir_name()), &assembled);

        let mut times = keyframe_times(&assembled);
        times.sort_by(|a, b| a.partial_cmp(b).expect("no NaN timestamps"));

        assert!(
            times.len() >= 2,
            "{} should have several keyframes, got {times:?}",
            rendition.dir_name()
        );
        per_rendition.push((rendition.dir_name(), times));
    }

    // Every rendition must have keyframes at the same timestamps.
    let (reference_name, reference_times) = &per_rendition[0];
    for (name, times) in &per_rendition[1..] {
        assert_eq!(
            times.len(),
            reference_times.len(),
            "{name} has {} keyframes but {reference_name} has {} - renditions are \
             not aligned, so switching between them will glitch\n{name}: {times:?}\n\
             {reference_name}: {reference_times:?}",
            times.len(),
            reference_times.len()
        );

        for (index, (actual, expected)) in times.iter().zip(reference_times).enumerate() {
            assert!(
                (actual - expected).abs() < 0.05,
                "keyframe {index} is at {actual}s in {name} but {expected}s in \
                 {reference_name}; a player can only switch where all renditions \
                 have a keyframe"
            );
        }
    }

    // And they must be spaced at the configured GOP interval.
    //
    // Measured relative to the first keyframe, not against absolute time: the
    // assembled fMP4 carries a small container start offset (start_pts is not
    // zero once init.mp4 and the fragments are concatenated), so an absolute
    // `time % GOP == 0` check would fail on a stream whose keyframes are in
    // fact perfectly regular. What matters for switching is the cadence and
    // that it is the same cadence everywhere, both of which this checks.
    for (name, times) in &per_rendition {
        let first = times[0];

        for (index, time) in times.iter().enumerate() {
            let elapsed = time - first;
            let expected = f64::from(GOP_SECONDS) * index as f64;
            assert!(
                (elapsed - expected).abs() < 0.05,
                "{name} keyframe {index} is {elapsed}s after the first, expected \
                 {expected}s - keyframes are not on a {GOP_SECONDS}s cadence: {times:?}"
            );
        }
    }

    // The renditions must also agree on where the stream starts, or the
    // aligned keyframes would still be at different wall-clock positions.
    let first_times: Vec<f64> = per_rendition.iter().map(|(_, t)| t[0]).collect();
    for time in &first_times[1..] {
        assert!(
            (time - first_times[0]).abs() < 0.05,
            "renditions start at different times: {first_times:?}"
        );
    }

    // Segment boundaries can only fall on keyframes, so the segment length
    // must be a whole number of GOPs for the above to hold.
    assert_eq!(SEGMENT_SECONDS % GOP_SECONDS, 0);
}

#[test]
fn never_upscales_a_small_source() {
    if !require_ffmpeg() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();

    // 480p in: the ladder must not contain 720p or 1080p.
    let input = root.join("source.mp4");
    generate_clip(&input, 854, 480, 30);

    let info = source_info(854, 480, 30.0);
    let renditions = ladder::build_ladder(&info);

    let heights: Vec<u32> = renditions.iter().map(|r| r.height).collect();
    assert_eq!(heights, vec![480, 360]);

    // Encode the top rung and confirm the real output height matches.
    let dir = root.join("480p");
    std::fs::create_dir_all(&dir).expect("directory");
    let args = ladder::ffmpeg_args(
        &input.to_string_lossy(),
        &renditions[0],
        &info,
        &dir.to_string_lossy(),
    );
    run("ffmpeg", &args, "encoding 480p");

    let assembled = root.join("480p.mp4");
    assemble_rendition(&dir, &assembled);

    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(&assembled)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("ffprobe should run");

    let dimensions = String::from_utf8_lossy(&output.stdout).trim().to_string();
    assert_eq!(
        dimensions, "854,480",
        "the encoded size must match what the ladder advertises"
    );
}

#[test]
fn the_advertised_resolution_matches_the_encoded_resolution() {
    if !require_ffmpeg() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();

    // 1920x1080 is the case where rounding matters: 480p is 853.33 wide, and
    // FFmpeg's scale=-2 rounds to 854. If the ladder computed 852 the master
    // playlist would advertise a resolution the video does not have.
    let input = root.join("source.mp4");
    generate_clip(&input, 1920, 1080, 30);

    let info = source_info(1920, 1080, 30.0);

    for rendition in ladder::build_ladder(&info) {
        let dir = root.join(rendition.dir_name());
        std::fs::create_dir_all(&dir).expect("directory");

        let args = ladder::ffmpeg_args(
            &input.to_string_lossy(),
            &rendition,
            &info,
            &dir.to_string_lossy(),
        );
        run(
            "ffmpeg",
            &args,
            &format!("encoding {}", rendition.dir_name()),
        );

        let assembled = root.join(format!("{}-probe.mp4", rendition.dir_name()));
        assemble_rendition(&dir, &assembled);

        let output = Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=width,height",
                "-of",
                "csv=p=0",
            ])
            .arg(&assembled)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("ffprobe should run");

        let actual = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let advertised = format!("{},{}", rendition.width, rendition.height);

        assert_eq!(
            actual,
            advertised,
            "{} is encoded at {actual} but the playlist advertises {advertised}",
            rendition.dir_name()
        );
    }
}

#[test]
fn the_master_playlist_references_files_that_were_actually_produced() {
    if !require_ffmpeg() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();

    let input = root.join("source.mp4");
    generate_clip(&input, 1280, 720, 30);

    let info = source_info(1280, 720, 30.0);
    let renditions = ladder::build_ladder(&info);
    let output_root = root.join("out");

    for rendition in &renditions {
        let dir = output_root.join(rendition.dir_name());
        std::fs::create_dir_all(&dir).expect("directory");
        let args = ladder::ffmpeg_args(
            &input.to_string_lossy(),
            rendition,
            &info,
            &dir.to_string_lossy(),
        );
        run("ffmpeg", &args, "encoding");
    }

    // One caption track, so the subtitle plumbing is covered too.
    let captions_dir = output_root.join("captions");
    std::fs::create_dir_all(&captions_dir).expect("directory");
    std::fs::write(captions_dir.join("en.vtt"), "WEBVTT\n\n").expect("write");
    std::fs::write(
        captions_dir.join("en.m3u8"),
        hls::subtitle_playlist("en", info.duration_seconds),
    )
    .expect("write");

    let subtitles = vec![SubtitleTrack {
        language: "en".into(),
        name: SubtitleTrack::display_name("en"),
        default: true,
    }];

    let master = hls::master_playlist(&renditions, &subtitles);
    std::fs::write(output_root.join("master.m3u8"), &master).expect("write");

    // Every non-comment URI and every URI= attribute must resolve to a real
    // file. This is the check that catches a playlist/output mismatch, which
    // otherwise shows up only as a 404 in the player.
    let mut referenced = Vec::new();
    for line in master.lines() {
        if line.starts_with('#') {
            if let Some(rest) = line.split("URI=\"").nth(1) {
                if let Some(uri) = rest.split('"').next() {
                    referenced.push(uri.to_string());
                }
            }
        } else if !line.trim().is_empty() {
            referenced.push(line.trim().to_string());
        }
    }

    assert!(
        referenced.len() > renditions.len(),
        "expected a URI per rendition plus the subtitle track, got {referenced:?}"
    );

    for uri in &referenced {
        let path = output_root.join(uri);
        assert!(
            path.is_file(),
            "master.m3u8 references {uri}, which was not produced ({})",
            path.display()
        );
    }

    // And each media playlist's own references must resolve as well.
    for rendition in &renditions {
        let dir = output_root.join(rendition.dir_name());
        let playlist = std::fs::read_to_string(dir.join("index.m3u8")).expect("readable");

        for line in playlist.lines() {
            let target = if line.starts_with('#') {
                line.split("URI=\"")
                    .nth(1)
                    .and_then(|r| r.split('"').next())
            } else if line.trim().is_empty() {
                None
            } else {
                Some(line.trim())
            };

            if let Some(target) = target {
                assert!(
                    dir.join(target).is_file(),
                    "{}/index.m3u8 references {target}, which does not exist",
                    rendition.dir_name()
                );
            }
        }
    }
}

#[test]
fn a_manifest_built_from_real_output_describes_it_accurately() {
    if !require_ffmpeg() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let input = temp.path().join("source.mp4");
    generate_clip(&input, 1280, 720, 30);

    let info = source_info(1280, 720, 30.0);
    let renditions = ladder::build_ladder(&info);

    let manifest = Manifest::ready(ReadyManifest {
        video_id: "integration-test-video",
        duration_seconds: info.duration_seconds,
        source: SourceSummary {
            width: info.width,
            height: info.height,
            fps: info.fps,
        },
        renditions: &renditions,
        captions: vec![ManifestCaption {
            language: "en".into(),
            path: "captions/en.vtt".into(),
            auto_generated: true,
        }],
        poster: Some("poster.jpg".into()),
        thumbnails: None,
        processed_at: "2026-10-09T12:00:00Z".into(),
    });

    let json: serde_json::Value =
        serde_json::from_str(&manifest.to_json().expect("serialises")).expect("valid JSON");

    assert_eq!(json["status"], "ready");
    assert_eq!(json["renditions"].as_array().unwrap().len(), 3);
    assert_eq!(json["source"]["height"], 720);
    assert_eq!(json["captions"][0]["auto_generated"], true);
    assert_eq!(json["master_playlist"], "master.m3u8");
}

#[test]
fn poster_and_sprite_generation_produce_real_images() {
    if !require_ffmpeg() {
        return;
    }

    let temp = tempfile::tempdir().expect("tempdir");
    let root = temp.path();
    let input = root.join("source.mp4");
    generate_clip(&input, 1280, 720, 30);

    let plan = vidra::thumbnails::plan_sprite(f64::from(CLIP_SECONDS), 2, 400);
    assert!(
        plan.count >= 2,
        "a 12s clip at 2s should give several tiles"
    );

    let poster = root.join("poster.jpg");
    run(
        "ffmpeg",
        &vidra::thumbnails::poster_args(
            &input.to_string_lossy(),
            &poster.to_string_lossy(),
            vidra::thumbnails::poster_timestamp(f64::from(CLIP_SECONDS)),
        ),
        "generating the poster",
    );

    let sprite = root.join("thumbs.jpg");
    run(
        "ffmpeg",
        &vidra::thumbnails::sprite_args(&input.to_string_lossy(), &sprite.to_string_lossy(), &plan),
        "generating the sprite",
    );

    for (path, label) in [(&poster, "poster"), (&sprite, "sprite")] {
        assert!(path.is_file(), "{label} should exist");
        let size = std::fs::metadata(path).expect("metadata").len();
        assert!(size > 1000, "{label} looks empty ({size} bytes)");

        // JPEG SOI marker, so this is really an image and not a stub.
        let bytes = std::fs::read(path).expect("readable");
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "{label} should be a JPEG");
    }

    // The sprite must be the planned grid, so the VTT cue regions line up.
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0",
        ])
        .arg(&sprite)
        .stdin(std::process::Stdio::null())
        .output()
        .expect("ffprobe should run");

    let dimensions = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let expected = format!(
        "{},{}",
        plan.columns * vidra::thumbnails::TILE_WIDTH,
        plan.rows * vidra::thumbnails::TILE_HEIGHT
    );
    assert_eq!(
        dimensions, expected,
        "the sprite must match the grid the VTT describes"
    );
}

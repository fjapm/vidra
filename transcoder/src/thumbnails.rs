//! Poster image and the scrubbing-preview sprite.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tracing::debug;

use crate::manifest::ThumbnailInfo;
use crate::process::run_checked;

/// Sprite tile size. 160x90 keeps a 16:9 preview small enough that a sprite
/// for a long video stays a reasonable download.
pub const TILE_WIDTH: u32 = 160;
pub const TILE_HEIGHT: u32 = 90;

/// Tiles per sprite row. A roughly square sheet avoids one enormous dimension,
/// which some decoders cap.
pub const SPRITE_COLUMNS: u32 = 5;

/// Plan for a video's sprite sheet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpritePlan {
    /// Seconds between tiles.
    pub interval_seconds: u32,
    /// Number of tiles.
    pub count: u32,
    pub columns: u32,
    pub rows: u32,
}

/// Decide how many tiles to generate, and how far apart.
///
/// The requested interval is honoured unless it would produce an unreasonable
/// number of tiles; a three-hour lecture at one tile per 10s is 1080 tiles,
/// which is a 170-megapixel sheet. Above the cap the interval is stretched so
/// the sheet stays bounded, which is better than refusing to make one.
pub fn plan_sprite(duration_seconds: f64, requested_interval: u32, max_tiles: u32) -> SpritePlan {
    let interval = requested_interval.max(1);
    let max_tiles = max_tiles.max(1);

    let duration = if duration_seconds.is_finite() && duration_seconds > 0.0 {
        duration_seconds
    } else {
        0.0
    };

    // At least one tile, even for a clip shorter than the interval.
    let ideal = ((duration / f64::from(interval)).ceil() as u32).max(1);

    let (interval, count) = if ideal <= max_tiles {
        (interval, ideal)
    } else {
        // Stretch the interval so the count lands at the cap.
        let stretched = ((duration / f64::from(max_tiles)).ceil() as u32).max(1);
        let count = ((duration / f64::from(stretched)).ceil() as u32)
            .max(1)
            .min(max_tiles);
        (stretched, count)
    };

    let columns = SPRITE_COLUMNS.min(count).max(1);
    let rows = count.div_ceil(columns);

    SpritePlan {
        interval_seconds: interval,
        count,
        columns,
        rows,
    }
}

/// Timestamp for the poster frame: 10% in.
///
/// Frame zero is very often black or a title card, so a fixed early offset
/// gives a more representative still. Clamped so a very short clip does not
/// seek past its own end.
pub fn poster_timestamp(duration_seconds: f64) -> f64 {
    if !duration_seconds.is_finite() || duration_seconds <= 0.0 {
        return 0.0;
    }
    (duration_seconds * 0.10)
        .min(duration_seconds - 0.1)
        .max(0.0)
}

/// FFmpeg arguments for the poster image.
pub fn poster_args(source: &str, output: &str, timestamp: f64) -> Vec<String> {
    vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        // -ss before -i seeks by keyframe, which is far faster on a long file
        // and precise enough for a poster frame.
        "-ss".into(),
        format!("{timestamp:.3}"),
        "-i".into(),
        source.into(),
        "-frames:v".into(),
        "1".into(),
        // Cap the long edge so a 4K source does not produce a 2 MB poster.
        "-vf".into(),
        "scale='min(1280,iw)':-2".into(),
        "-q:v".into(),
        "3".into(),
        output.into(),
    ]
}

/// FFmpeg arguments for the sprite sheet.
pub fn sprite_args(source: &str, output: &str, plan: &SpritePlan) -> Vec<String> {
    // fps=1/N picks one frame every N seconds; tile builds the grid.
    let filter = format!(
        "fps=1/{interval},scale={width}:{height}:force_original_aspect_ratio=decrease,\
pad={width}:{height}:-1:-1:color=black,tile={columns}x{rows}",
        interval = plan.interval_seconds,
        width = TILE_WIDTH,
        height = TILE_HEIGHT,
        columns = plan.columns,
        rows = plan.rows,
    );

    vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-i".into(),
        source.into(),
        "-vf".into(),
        filter,
        // tile emits one image once the grid is full; without this FFmpeg can
        // also write a second, partially-filled sheet.
        "-frames:v".into(),
        "1".into(),
        "-q:v".into(),
        "5".into(),
        output.into(),
    ]
}

/// Generated thumbnail artefacts.
#[derive(Debug)]
pub struct Thumbnails {
    pub poster_path: PathBuf,
    pub sprite_path: PathBuf,
    pub info: ThumbnailInfo,
}

/// Generate the poster and sprite into `output_dir`.
pub async fn generate(
    source_path: &Path,
    output_dir: &Path,
    duration_seconds: f64,
    interval_seconds: u32,
    max_tiles: u32,
) -> Result<Thumbnails> {
    tokio::fs::create_dir_all(output_dir)
        .await
        .with_context(|| format!("could not create {}", output_dir.display()))?;

    let source = source_path.to_string_lossy().into_owned();

    // --- poster ----------------------------------------------------------
    let poster_path = output_dir.join("poster.jpg");
    run_checked(
        "ffmpeg",
        &poster_args(
            &source,
            &poster_path.to_string_lossy(),
            poster_timestamp(duration_seconds),
        ),
        "generating the poster image",
    )
    .await?;

    // --- sprite ----------------------------------------------------------
    let plan = plan_sprite(duration_seconds, interval_seconds, max_tiles);
    let sprite_path = output_dir.join("thumbs.jpg");

    run_checked(
        "ffmpeg",
        &sprite_args(&source, &sprite_path.to_string_lossy(), &plan),
        "generating the thumbnail sprite",
    )
    .await?;

    debug!(?plan, "generated thumbnails");

    Ok(Thumbnails {
        poster_path,
        sprite_path,
        info: ThumbnailInfo {
            sprite: "thumbs.jpg".to_string(),
            vtt: "thumbs.vtt".to_string(),
            interval_seconds: plan.interval_seconds,
            tile_width: TILE_WIDTH,
            tile_height: TILE_HEIGHT,
            columns: plan.columns,
            count: plan.count,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- poster timestamp -------------------------------------------------

    #[test]
    fn the_poster_is_taken_ten_percent_in() {
        assert!((poster_timestamp(100.0) - 10.0).abs() < 1e-9);
        assert!((poster_timestamp(600.0) - 60.0).abs() < 1e-9);
    }

    #[test]
    fn the_poster_timestamp_never_exceeds_the_duration() {
        // Seeking past the end yields no frame at all and fails the encode.
        for duration in [0.5, 1.0, 0.2, 2.0] {
            let timestamp = poster_timestamp(duration);
            assert!(
                timestamp < duration,
                "timestamp {timestamp} must be inside a {duration}s clip"
            );
            assert!(timestamp >= 0.0);
        }
    }

    #[test]
    fn the_poster_timestamp_survives_a_nonsense_duration() {
        assert_eq!(poster_timestamp(0.0), 0.0);
        assert_eq!(poster_timestamp(-5.0), 0.0);
        assert_eq!(poster_timestamp(f64::NAN), 0.0);
    }

    // --- sprite planning --------------------------------------------------

    #[test]
    fn a_two_minute_video_gets_one_tile_every_ten_seconds() {
        let plan = plan_sprite(120.0, 10, 400);

        assert_eq!(plan.interval_seconds, 10);
        assert_eq!(plan.count, 12);
        assert_eq!(plan.columns, 5);
        assert_eq!(plan.rows, 3, "12 tiles over 5 columns needs 3 rows");
    }

    #[test]
    fn a_short_clip_still_gets_one_tile() {
        let plan = plan_sprite(3.0, 10, 400);

        assert_eq!(plan.count, 1);
        assert_eq!(plan.columns, 1);
        assert_eq!(plan.rows, 1);
    }

    #[test]
    fn the_tile_count_is_capped_by_stretching_the_interval() {
        // A 3-hour lecture at 10s would be 1080 tiles / ~170 megapixels.
        let plan = plan_sprite(10_800.0, 10, 400);

        assert!(plan.count <= 400, "count was {}", plan.count);
        assert!(
            plan.interval_seconds > 10,
            "the interval should stretch, got {}",
            plan.interval_seconds
        );
        // And the tiles must still span the whole video.
        assert!(
            plan.count * plan.interval_seconds >= 10_800,
            "tiles must cover the duration"
        );
    }

    #[test]
    fn the_grid_always_holds_every_tile() {
        for duration in [1.0, 10.0, 59.0, 120.0, 600.0, 3600.0, 10_800.0] {
            let plan = plan_sprite(duration, 10, 400);
            assert!(
                plan.columns * plan.rows >= plan.count,
                "grid {}x{} cannot hold {} tiles (duration {duration})",
                plan.columns,
                plan.rows,
                plan.count
            );
        }
    }

    #[test]
    fn planning_survives_degenerate_input() {
        // Must not divide by zero or produce a zero-tile sheet.
        for (duration, interval, cap) in [
            (0.0, 10, 400),
            (-1.0, 10, 400),
            (f64::NAN, 10, 400),
            (100.0, 0, 400),
            (100.0, 10, 0),
        ] {
            let plan = plan_sprite(duration, interval, cap);
            assert!(plan.count >= 1, "({duration},{interval},{cap})");
            assert!(plan.interval_seconds >= 1, "({duration},{interval},{cap})");
            assert!(plan.columns >= 1 && plan.rows >= 1);
        }
    }

    #[test]
    fn columns_never_exceed_the_tile_count() {
        // A 3-tile sheet laid out 5 wide would leave two blank cells that the
        // VTT would still reference.
        let plan = plan_sprite(25.0, 10, 400);
        assert_eq!(plan.count, 3);
        assert_eq!(plan.columns, 3);
    }

    // --- ffmpeg arguments -------------------------------------------------

    #[test]
    fn poster_args_seek_before_input_and_take_one_frame() {
        let args = poster_args("/in.mp4", "/out/poster.jpg", 12.5);

        // -ss must precede -i to use the fast keyframe seek.
        let ss = args.iter().position(|a| a == "-ss").expect("-ss present");
        let input = args.iter().position(|a| a == "-i").expect("-i present");
        assert!(ss < input, "-ss must come before -i");

        assert!(args.windows(2).any(|w| w == ["-ss", "12.500"]));
        assert!(args.windows(2).any(|w| w == ["-frames:v", "1"]));
        assert_eq!(args.last().unwrap(), "/out/poster.jpg");
    }

    #[test]
    fn sprite_args_build_the_planned_grid() {
        let plan = plan_sprite(120.0, 10, 400);
        let args = sprite_args("/in.mp4", "/out/thumbs.jpg", &plan);

        let filter = args
            .iter()
            .position(|a| a == "-vf")
            .map(|i| &args[i + 1])
            .expect("-vf present");

        assert!(filter.contains("fps=1/10"), "got {filter}");
        assert!(filter.contains("tile=5x3"), "got {filter}");
        assert!(filter.contains("scale=160:90"), "got {filter}");
        // Letterbox rather than distort a non-16:9 source.
        assert!(
            filter.contains("force_original_aspect_ratio=decrease"),
            "got {filter}"
        );
        assert!(filter.contains("pad=160:90"), "got {filter}");
    }

    #[test]
    fn sprite_args_emit_exactly_one_sheet() {
        // Without -frames:v 1, a partially filled second sheet can be written
        // and overwrite the first.
        let plan = plan_sprite(120.0, 10, 400);
        let args = sprite_args("/in.mp4", "/out/thumbs.jpg", &plan);

        assert!(args.windows(2).any(|w| w == ["-frames:v", "1"]));
    }

    #[test]
    fn both_commands_close_stdin() {
        let plan = plan_sprite(120.0, 10, 400);

        assert!(poster_args("/in.mp4", "/out.jpg", 1.0)
            .iter()
            .any(|a| a == "-nostdin"));
        assert!(sprite_args("/in.mp4", "/out.jpg", &plan)
            .iter()
            .any(|a| a == "-nostdin"));
    }

    #[test]
    fn the_sprite_interval_in_the_vtt_matches_the_plan() {
        // The VTT cue times are generated from plan.interval_seconds, so if
        // the stretched interval were not reported the previews would be
        // offset from the video.
        let plan = plan_sprite(10_800.0, 10, 400);
        let args = sprite_args("/in.mp4", "/out.jpg", &plan);

        let filter = args
            .iter()
            .position(|a| a == "-vf")
            .map(|i| &args[i + 1])
            .expect("-vf present");

        assert!(
            filter.contains(&format!("fps=1/{}", plan.interval_seconds)),
            "the sprite interval and the reported interval must agree"
        );
    }
}

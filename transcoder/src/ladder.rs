//! Rendition ladder: source properties in, renditions and FFmpeg arguments out.
//!
//! Deliberately pure. Every decision that affects playback compatibility -
//! which rungs exist, what resolution each one is, where keyframes land - is
//! decided here with no I/O, so it can be unit-tested exhaustively rather than
//! inferred from a transcoded file.

use crate::probe::SourceInfo;

/// Segment length in seconds. Six seconds is the usual HLS VOD compromise:
/// few enough requests to keep overhead low, short enough that ABR can react.
pub const SEGMENT_SECONDS: u32 = 6;

/// Keyframe interval in seconds.
///
/// Every rendition uses the same value, which is what makes bitrate switching
/// seamless: a player can only switch at a segment boundary that begins with a
/// keyframe in *every* rendition. SEGMENT_SECONDS must stay an exact multiple
/// of this.
pub const GOP_SECONDS: u32 = 2;

/// AAC-LC stereo at 48 kHz, shared by every rendition so audio is switchable.
pub const AUDIO_BITRATE_KBPS: u32 = 128;
pub const AUDIO_SAMPLE_RATE: u32 = 48_000;
pub const AUDIO_CHANNELS: u32 = 2;

/// One rung of the ladder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rendition {
    /// Output height in pixels. Also the directory name: `media/{id}/{height}p/`.
    pub height: u32,
    /// Output width, derived from the source aspect ratio and forced even.
    pub width: u32,
    /// Target average video bitrate.
    pub video_bitrate_kbps: u32,
    /// VBV ceiling. 1.07x the target is the usual H.264 constrained-VBR shape.
    pub maxrate_kbps: u32,
    /// VBV buffer. 1.5x the ceiling absorbs complex scenes without drifting.
    pub bufsize_kbps: u32,
}

impl Rendition {
    /// Directory name under `media/{id}/`, e.g. "720p".
    pub fn dir_name(&self) -> String {
        format!("{}p", self.height)
    }

    /// Peak bandwidth for the master playlist's BANDWIDTH attribute.
    ///
    /// BANDWIDTH must be the peak, not the average, or a player on a marginal
    /// connection will pick a rung it cannot sustain and rebuffer.
    pub fn peak_bandwidth_bps(&self) -> u32 {
        (self.maxrate_kbps + AUDIO_BITRATE_KBPS) * 1000
    }

    /// Average bandwidth for AVERAGE-BANDWIDTH.
    pub fn average_bandwidth_bps(&self) -> u32 {
        (self.video_bitrate_kbps + AUDIO_BITRATE_KBPS) * 1000
    }

    /// RFC 6381 codec string for this rendition.
    ///
    /// H.264 High profile; the level is derived from the frame size because a
    /// level that is too low makes some decoders (notably older iOS) refuse
    /// the stream outright, and one that is too high needlessly excludes
    /// devices.
    pub fn codecs(&self) -> &'static str {
        // avc1.<profile_idc><constraints><level_idc>, hex. 64 = High profile,
        // 00 = no constraint flags set.
        match self.height {
            h if h > 1080 => "avc1.640033,mp4a.40.2", // level 5.1
            h if h > 720 => "avc1.640028,mp4a.40.2",  // level 4.0
            h if h > 480 => "avc1.64001f,mp4a.40.2",  // level 3.1
            _ => "avc1.64001e,mp4a.40.2",             // level 3.0
        }
    }
}

/// The standard rungs, highest first: (height, video kbps).
const RUNGS: &[(u32, u32)] = &[(1080, 5000), (720, 2800), (480, 1400), (360, 800)];

/// Round down to an even number, with a floor of 2.
///
/// H.264 4:2:0 chroma subsampling requires even dimensions; an odd width makes
/// FFmpeg fail outright.
fn even(value: u32) -> u32 {
    if value < 2 {
        return 2;
    }
    value - (value % 2)
}

/// Width that preserves the source aspect ratio at a given output height.
///
/// This must reproduce exactly what FFmpeg's `scale=-2:H` computes, because
/// the result is what the master playlist advertises as RESOLUTION. FFmpeg
/// rounds the scaled width to the *nearest* even value - so a 1920x1080 source
/// at 480p becomes the familiar 854, not 852. Truncating instead would declare
/// a resolution two pixels narrower than the encoded video, which is the kind
/// of mismatch that makes a player pick the wrong rung or reject the variant.
fn width_for_height(source: &SourceInfo, height: u32) -> u32 {
    if source.height == 0 {
        // Defensive only; the caller rejects a zero-height source.
        return even(height * 16 / 9);
    }

    // round(width * height / source_height / 2) * 2, in integers:
    //   round(a / b) == (a + b / 2) / b   for positive a, b
    // with a = width * height and b = 2 * source_height.
    let numerator = u64::from(source.width) * u64::from(height);
    let denominator = 2 * u64::from(source.height);
    let half_widths = (numerator + denominator / 2) / denominator;

    let width = u32::try_from(half_widths.saturating_mul(2)).unwrap_or(u32::MAX);
    width.max(2)
}

fn rendition_for(source: &SourceInfo, height: u32, video_bitrate_kbps: u32) -> Rendition {
    let maxrate_kbps = video_bitrate_kbps * 107 / 100;
    Rendition {
        height,
        width: width_for_height(source, height),
        video_bitrate_kbps,
        maxrate_kbps,
        bufsize_kbps: maxrate_kbps * 3 / 2,
    }
}

/// Build the ladder for a source.
///
/// Rungs taller than the source are skipped: upscaling spends bitrate
/// inventing detail that is not in the original. If the source is shorter than
/// every rung, a single rendition at the source height is produced instead of
/// an empty ladder, so a 240p input still plays.
///
/// Returned highest-first, which is also the order the master playlist lists
/// them in.
pub fn build_ladder(source: &SourceInfo) -> Vec<Rendition> {
    let mut ladder: Vec<Rendition> = RUNGS
        .iter()
        .filter(|(height, _)| *height <= source.height)
        .map(|(height, bitrate)| rendition_for(source, *height, *bitrate))
        .collect();

    if ladder.is_empty() {
        // Source is shorter than the lowest rung. Encode at its own height,
        // reusing the lowest rung's bitrate, which is generous for anything
        // this small.
        let (_, lowest_bitrate) = RUNGS[RUNGS.len() - 1];
        let height = even(source.height.max(2));

        ladder.push(Rendition {
            height,
            width: even(source.width.max(2)),
            video_bitrate_kbps: lowest_bitrate,
            maxrate_kbps: lowest_bitrate * 107 / 100,
            bufsize_kbps: lowest_bitrate * 107 / 100 * 3 / 2,
        });
    }

    ladder
}

/// Keyframe interval in frames for a given frame rate.
///
/// Derived from fps so that `-g` lines up with the forced keyframe times;
/// a mismatch makes x264 emit extra keyframes and bloats the output.
pub fn keyframe_interval(fps: f64) -> u32 {
    if !fps.is_finite() || fps <= 0.0 {
        // Unknown frame rate: assume 30, and rely on -force_key_frames (which
        // works in seconds, not frames) to place keyframes correctly anyway.
        return 30 * GOP_SECONDS;
    }

    let interval = (fps * f64::from(GOP_SECONDS)).round();
    // Clamp to something sane: a pathological fps from a broken file must not
    // produce a GOP of 0 (every frame a keyframe) or millions.
    interval.clamp(1.0, 600.0) as u32
}

/// FFmpeg arguments that render one rendition as fMP4/CMAF HLS.
///
/// `output_dir` must exist. Produces `init.mp4`, `seg_00001.m4s`, ... and
/// `index.m3u8` inside it.
pub fn ffmpeg_args(
    source_path: &str,
    rendition: &Rendition,
    source: &SourceInfo,
    output_dir: &str,
) -> Vec<String> {
    let keyint = keyframe_interval(source.fps);
    let mut args: Vec<String> = Vec::new();

    let mut push = |value: &str| args.push(value.to_string());

    // Fail on the first error rather than producing a truncated rendition that
    // looks like a success.
    push("-nostdin");
    push("-hide_banner");
    push("-loglevel");
    push("error");
    push("-y");

    push("-i");
    push(source_path);

    // --- video ------------------------------------------------------------
    push("-map");
    push("0:v:0");
    push("-c:v");
    push("libx264");
    push("-profile:v");
    push("high");
    push("-preset");
    push("medium");
    // Deterministic output: without this, x264 adapts to core count and two
    // runs of the same input differ, which breaks idempotent reprocessing.
    push("-x264-params");
    push("sliced-threads=0");

    push("-pix_fmt");
    push("yuv420p");

    // scale=-2:H keeps the aspect ratio and forces an even width.
    push("-vf");
    push(&format!("scale=-2:{}:flags=lanczos", rendition.height));

    push("-b:v");
    push(&format!("{}k", rendition.video_bitrate_kbps));
    push("-maxrate");
    push(&format!("{}k", rendition.maxrate_kbps));
    push("-bufsize");
    push(&format!("{}k", rendition.bufsize_kbps));

    // --- keyframe alignment ----------------------------------------------
    // The three settings together are what make renditions switchable:
    //   -g / -keyint_min  fix the GOP length in frames
    //   -sc_threshold 0   stops scene detection inserting unaligned keyframes
    //   -force_key_frames pins keyframes to exact 2s wall-clock times, which
    //                     survives fractional frame rates like 29.97
    push("-g");
    push(&keyint.to_string());
    push("-keyint_min");
    push(&keyint.to_string());
    push("-sc_threshold");
    push("0");
    push("-force_key_frames");
    push(&format!("expr:gte(t,n_forced*{GOP_SECONDS})"));

    // --- audio ------------------------------------------------------------
    if source.has_audio {
        push("-map");
        push("0:a:0");
        push("-c:a");
        push("aac");
        push("-profile:a");
        push("aac_low");
        push("-b:a");
        push(&format!("{AUDIO_BITRATE_KBPS}k"));
        push("-ar");
        push(&AUDIO_SAMPLE_RATE.to_string());
        push("-ac");
        push(&AUDIO_CHANNELS.to_string());
    } else {
        push("-an");
    }

    // --- HLS packaging ----------------------------------------------------
    push("-f");
    push("hls");
    push("-hls_time");
    push(&SEGMENT_SECONDS.to_string());
    push("-hls_playlist_type");
    push("vod");
    push("-hls_segment_type");
    push("fmp4");
    // Every segment starts with a keyframe, which lets players seek into any
    // segment without fetching the previous one.
    push("-hls_flags");
    push("independent_segments");
    push("-hls_list_size");
    push("0");
    push("-hls_fmp4_init_filename");
    push("init.mp4");
    push("-hls_segment_filename");
    push(&format!("{output_dir}/seg_%05d.m4s"));

    push(&format!("{output_dir}/index.m3u8"));

    args
}

/// FFmpeg arguments that extract 16 kHz mono WAV for whisper.cpp.
///
/// whisper.cpp requires exactly this format; anything else is silently
/// resampled badly or rejected.
pub fn audio_extract_args(source_path: &str, wav_path: &str) -> Vec<String> {
    vec![
        "-nostdin".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-i".into(),
        source_path.into(),
        "-map".into(),
        "0:a:0".into(),
        "-vn".into(),
        "-acodec".into(),
        "pcm_s16le".into(),
        "-ar".into(),
        "16000".into(),
        "-ac".into(),
        "1".into(),
        wav_path.into(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(width: u32, height: u32, fps: f64, has_audio: bool) -> SourceInfo {
        SourceInfo {
            width,
            height,
            fps,
            duration_seconds: 120.0,
            has_audio,
        }
    }

    // --- ladder selection -------------------------------------------------

    #[test]
    fn full_ladder_for_1080p_source() {
        let ladder = build_ladder(&source(1920, 1080, 30.0, true));

        let heights: Vec<u32> = ladder.iter().map(|r| r.height).collect();
        assert_eq!(heights, vec![1080, 720, 480, 360]);
    }

    #[test]
    fn never_upscales_a_720p_source() {
        let ladder = build_ladder(&source(1280, 720, 30.0, true));

        let heights: Vec<u32> = ladder.iter().map(|r| r.height).collect();
        assert_eq!(heights, vec![720, 480, 360], "1080p must be skipped");
    }

    #[test]
    fn never_upscales_a_480p_source() {
        let ladder = build_ladder(&source(854, 480, 30.0, true));

        let heights: Vec<u32> = ladder.iter().map(|r| r.height).collect();
        assert_eq!(heights, vec![480, 360]);
    }

    #[test]
    fn a_source_taller_than_1080p_still_caps_at_1080p() {
        // 4K in: the top rung is 1080p, so nothing upscales and nothing is
        // encoded at 2160p.
        let ladder = build_ladder(&source(3840, 2160, 30.0, true));

        let heights: Vec<u32> = ladder.iter().map(|r| r.height).collect();
        assert_eq!(heights, vec![1080, 720, 480, 360]);
    }

    #[test]
    fn a_source_below_the_lowest_rung_gets_one_rendition_at_its_own_height() {
        // Without the fallback this would be an empty ladder and the video
        // would have no playable renditions at all.
        let ladder = build_ladder(&source(426, 240, 30.0, true));

        assert_eq!(ladder.len(), 1);
        assert_eq!(ladder[0].height, 240);
        assert_eq!(ladder[0].width, 426);
    }

    #[test]
    fn an_odd_sized_source_below_the_lowest_rung_is_made_even() {
        let ladder = build_ladder(&source(427, 241, 30.0, true));

        assert_eq!(ladder.len(), 1);
        assert_eq!(ladder[0].height % 2, 0, "height must be even");
        assert_eq!(ladder[0].width % 2, 0, "width must be even");
    }

    #[test]
    fn a_source_exactly_at_a_rung_includes_that_rung() {
        // Boundary: <= not <, so a 360p source still gets its 360p rendition.
        let ladder = build_ladder(&source(640, 360, 30.0, true));

        let heights: Vec<u32> = ladder.iter().map(|r| r.height).collect();
        assert_eq!(heights, vec![360]);
    }

    // --- dimensions -------------------------------------------------------

    #[test]
    fn widths_preserve_a_16_by_9_aspect_ratio() {
        let ladder = build_ladder(&source(1920, 1080, 30.0, true));

        let dims: Vec<(u32, u32)> = ladder.iter().map(|r| (r.width, r.height)).collect();
        assert_eq!(
            dims,
            vec![(1920, 1080), (1280, 720), (854, 480), (640, 360)]
        );
    }

    #[test]
    fn widths_preserve_a_4_by_3_aspect_ratio() {
        let ladder = build_ladder(&source(1440, 1080, 30.0, true));

        let dims: Vec<(u32, u32)> = ladder.iter().map(|r| (r.width, r.height)).collect();
        assert_eq!(dims, vec![(1440, 1080), (960, 720), (640, 480), (480, 360)]);
    }

    #[test]
    fn widths_preserve_a_vertical_aspect_ratio() {
        // Phone video: 9:16. The ladder is driven by height, so a 1080-tall
        // portrait clip gets a 608-wide 1080p rendition, not a 1920-wide one.
        let ladder = build_ladder(&source(1080, 1920, 30.0, true));

        assert_eq!(ladder[0].height, 1080);
        assert_eq!(ladder[0].width, 608);
    }

    #[test]
    fn every_width_is_even() {
        // H.264 4:2:0 cannot encode odd dimensions at all.
        for (width, height) in [(1919, 1079), (1001, 999), (333, 777), (1439, 1081)] {
            for rendition in build_ladder(&source(width, height, 30.0, true)) {
                assert_eq!(
                    rendition.width % 2,
                    0,
                    "width {} from source {}x{} is odd",
                    rendition.width,
                    width,
                    height
                );
            }
        }
    }

    #[test]
    fn a_zero_height_source_falls_back_to_16_by_9() {
        // Defensive: ffprobe should never report this, but a divide by zero
        // here would panic the whole worker.
        let ladder = build_ladder(&source(0, 0, 30.0, true));
        assert_eq!(ladder.len(), 1);
        assert!(ladder[0].width >= 2);
    }

    // --- bitrates ---------------------------------------------------------

    #[test]
    fn bitrates_match_the_specified_ladder() {
        let ladder = build_ladder(&source(1920, 1080, 30.0, true));

        let bitrates: Vec<u32> = ladder.iter().map(|r| r.video_bitrate_kbps).collect();
        assert_eq!(bitrates, vec![5000, 2800, 1400, 800]);
    }

    #[test]
    fn maxrate_exceeds_the_target_and_bufsize_exceeds_maxrate() {
        for rendition in build_ladder(&source(1920, 1080, 30.0, true)) {
            assert!(
                rendition.maxrate_kbps > rendition.video_bitrate_kbps,
                "maxrate must leave VBV headroom"
            );
            assert!(
                rendition.bufsize_kbps > rendition.maxrate_kbps,
                "bufsize must exceed maxrate"
            );
        }
    }

    #[test]
    fn bandwidth_is_monotonically_decreasing_down_the_ladder() {
        // A player picks a rung by BANDWIDTH; a non-monotonic ladder makes
        // that choice meaningless.
        let ladder = build_ladder(&source(1920, 1080, 30.0, true));

        for pair in ladder.windows(2) {
            assert!(
                pair[0].peak_bandwidth_bps() > pair[1].peak_bandwidth_bps(),
                "{:?} should outrank {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn peak_bandwidth_exceeds_average() {
        let rendition = build_ladder(&source(1920, 1080, 30.0, true))[0];
        assert!(rendition.peak_bandwidth_bps() > rendition.average_bandwidth_bps());
    }

    #[test]
    fn bandwidth_includes_the_audio_track() {
        let rendition = build_ladder(&source(1920, 1080, 30.0, true))[0];
        assert_eq!(
            rendition.average_bandwidth_bps(),
            (5000 + AUDIO_BITRATE_KBPS) * 1000
        );
    }

    // --- codec strings ----------------------------------------------------

    #[test]
    fn codec_levels_track_resolution() {
        let ladder = build_ladder(&source(1920, 1080, 30.0, true));

        assert_eq!(ladder[0].codecs(), "avc1.640028,mp4a.40.2", "1080p -> 4.0");
        assert_eq!(ladder[1].codecs(), "avc1.64001f,mp4a.40.2", "720p -> 3.1");
        assert_eq!(ladder[2].codecs(), "avc1.64001e,mp4a.40.2", "480p -> 3.0");
        assert_eq!(ladder[3].codecs(), "avc1.64001e,mp4a.40.2", "360p -> 3.0");
    }

    #[test]
    fn every_codec_string_declares_high_profile_and_aac_lc() {
        for rendition in build_ladder(&source(1920, 1080, 30.0, true)) {
            let codecs = rendition.codecs();
            assert!(codecs.starts_with("avc1.64"), "High profile: {codecs}");
            assert!(codecs.ends_with("mp4a.40.2"), "AAC-LC: {codecs}");
        }
    }

    // --- keyframe interval ------------------------------------------------

    #[test]
    fn keyframe_interval_is_two_seconds_worth_of_frames() {
        assert_eq!(keyframe_interval(30.0), 60);
        assert_eq!(keyframe_interval(25.0), 50);
        assert_eq!(keyframe_interval(60.0), 120);
        assert_eq!(keyframe_interval(24.0), 48);
    }

    #[test]
    fn keyframe_interval_rounds_fractional_frame_rates() {
        // 29.97 and 23.976 are the common broadcast rates.
        assert_eq!(keyframe_interval(29.97), 60);
        assert_eq!(keyframe_interval(23.976), 48);
        assert_eq!(keyframe_interval(59.94), 120);
    }

    #[test]
    fn keyframe_interval_survives_a_nonsense_frame_rate() {
        // A GOP of 0 would make every frame a keyframe and balloon the output;
        // a huge GOP would break segment alignment.
        assert_eq!(keyframe_interval(0.0), 60);
        assert_eq!(keyframe_interval(-5.0), 60);
        assert_eq!(keyframe_interval(f64::NAN), 60);
        assert_eq!(keyframe_interval(f64::INFINITY), 60);
        assert!(keyframe_interval(100_000.0) <= 600);
        assert!(keyframe_interval(0.001) >= 1);
    }

    #[test]
    fn the_segment_length_is_a_whole_number_of_gops() {
        // If this fails, segment boundaries stop landing on keyframes and
        // switching between renditions glitches.
        assert_eq!(SEGMENT_SECONDS % GOP_SECONDS, 0);
    }

    // --- ffmpeg arguments -------------------------------------------------

    #[test]
    fn ffmpeg_args_request_fmp4_hls_and_vod() {
        let src = source(1920, 1080, 30.0, true);
        let args = ffmpeg_args(
            "/scratch/in.mp4",
            &build_ladder(&src)[0],
            &src,
            "/out/1080p",
        );

        assert!(args.windows(2).any(|w| w == ["-f", "hls"]));
        assert!(args.windows(2).any(|w| w == ["-hls_segment_type", "fmp4"]));
        assert!(args.windows(2).any(|w| w == ["-hls_playlist_type", "vod"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["-hls_flags", "independent_segments"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["-hls_time", &SEGMENT_SECONDS.to_string()]));
    }

    #[test]
    fn ffmpeg_args_pin_keyframes_three_ways() {
        let src = source(1920, 1080, 30.0, true);
        let args = ffmpeg_args(
            "/scratch/in.mp4",
            &build_ladder(&src)[0],
            &src,
            "/out/1080p",
        );

        assert!(args.windows(2).any(|w| w == ["-g", "60"]));
        assert!(args.windows(2).any(|w| w == ["-keyint_min", "60"]));
        assert!(args.windows(2).any(|w| w == ["-sc_threshold", "0"]));
        assert!(args
            .windows(2)
            .any(|w| w == ["-force_key_frames", "expr:gte(t,n_forced*2)"]));
    }

    #[test]
    fn every_rendition_uses_the_same_keyframe_settings() {
        // This is the actual cross-rendition alignment guarantee: the GOP is a
        // function of the source frame rate only, never of the rung.
        let src = source(1920, 1080, 29.97, true);
        let ladder = build_ladder(&src);

        let gops: Vec<String> = ladder
            .iter()
            .map(|r| {
                let args = ffmpeg_args("/in.mp4", r, &src, "/out");
                let index = args.iter().position(|a| a == "-g").expect("-g present");
                args[index + 1].clone()
            })
            .collect();

        assert!(
            gops.windows(2).all(|w| w[0] == w[1]),
            "GOP must be identical across renditions, got {gops:?}"
        );
    }

    #[test]
    fn ffmpeg_args_encode_h264_high_and_aac_lc() {
        let src = source(1920, 1080, 30.0, true);
        let args = ffmpeg_args("/in.mp4", &build_ladder(&src)[0], &src, "/out");

        assert!(args.windows(2).any(|w| w == ["-c:v", "libx264"]));
        assert!(args.windows(2).any(|w| w == ["-profile:v", "high"]));
        assert!(args.windows(2).any(|w| w == ["-c:a", "aac"]));
        assert!(args.windows(2).any(|w| w == ["-profile:a", "aac_low"]));
        assert!(args.windows(2).any(|w| w == ["-b:a", "128k"]));
        assert!(args.windows(2).any(|w| w == ["-ar", "48000"]));
        assert!(args.windows(2).any(|w| w == ["-ac", "2"]));
    }

    #[test]
    fn ffmpeg_args_drop_audio_when_the_source_has_none() {
        // Mapping 0:a:0 on a silent source makes FFmpeg fail outright.
        let src = source(1920, 1080, 30.0, false);
        let args = ffmpeg_args("/in.mp4", &build_ladder(&src)[0], &src, "/out");

        assert!(args.iter().any(|a| a == "-an"));
        assert!(!args.windows(2).any(|w| w == ["-map", "0:a:0"]));
        assert!(!args.windows(2).any(|w| w == ["-c:a", "aac"]));
    }

    #[test]
    fn ffmpeg_args_scale_by_height_and_keep_width_even() {
        let src = source(1920, 1080, 30.0, true);
        let args = ffmpeg_args("/in.mp4", &build_ladder(&src)[1], &src, "/out");

        // scale=-2:H, not scale=W:H: -2 derives an even width from the real
        // aspect ratio, which also handles anamorphic sources correctly.
        assert!(args.iter().any(|a| a == "scale=-2:720:flags=lanczos"));
    }

    #[test]
    fn ffmpeg_args_place_outputs_in_the_given_directory() {
        let src = source(1920, 1080, 30.0, true);
        let args = ffmpeg_args("/in.mp4", &build_ladder(&src)[0], &src, "/out/1080p");

        assert!(args.iter().any(|a| a == "/out/1080p/index.m3u8"));
        assert!(args.iter().any(|a| a == "/out/1080p/seg_%05d.m4s"));
        assert!(args
            .windows(2)
            .any(|w| w == ["-hls_fmp4_init_filename", "init.mp4"]));
    }

    #[test]
    fn ffmpeg_args_never_write_a_standalone_mp4() {
        // The whole point of the fMP4 ladder is that there is no downloadable
        // single-file MP4 in media/.
        let src = source(1920, 1080, 30.0, true);
        for rendition in build_ladder(&src) {
            let args = ffmpeg_args("/in.mp4", &rendition, &src, "/out");

            // Skip the input path, which is of course an .mp4 - only outputs
            // matter here. The input is the argument right after -i.
            let input_index = args.iter().position(|a| a == "-i").expect("-i present") + 1;

            for (index, arg) in args.iter().enumerate() {
                if index == input_index {
                    continue;
                }
                assert!(
                    !(arg.ends_with(".mp4") && arg != "init.mp4"),
                    "unexpected mp4 output: {arg}"
                );
            }
        }
    }

    #[test]
    fn ffmpeg_args_do_not_read_stdin() {
        // Without -nostdin a backgrounded FFmpeg can steal the worker's stdin
        // and hang.
        let src = source(1920, 1080, 30.0, true);
        let args = ffmpeg_args("/in.mp4", &build_ladder(&src)[0], &src, "/out");
        assert!(args.iter().any(|a| a == "-nostdin"));
    }

    #[test]
    fn dir_name_is_the_height_with_a_p() {
        let ladder = build_ladder(&source(1920, 1080, 30.0, true));
        assert_eq!(ladder[0].dir_name(), "1080p");
        assert_eq!(ladder[1].dir_name(), "720p");
    }

    // --- audio extraction -------------------------------------------------

    #[test]
    fn audio_extract_args_produce_16k_mono_pcm_for_whisper() {
        // whisper.cpp accepts nothing else.
        let args = audio_extract_args("/in.mp4", "/out/audio.wav");

        assert!(args.windows(2).any(|w| w == ["-ar", "16000"]));
        assert!(args.windows(2).any(|w| w == ["-ac", "1"]));
        assert!(args.windows(2).any(|w| w == ["-acodec", "pcm_s16le"]));
        assert!(args.iter().any(|a| a == "-vn"));
        assert_eq!(args.last().unwrap(), "/out/audio.wav");
    }
}

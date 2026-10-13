//! ffprobe wrapper: source duration, resolution, frame rate, audio presence.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::Path;
use tokio::process::Command;

/// What the rest of the pipeline needs to know about the input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceInfo {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub duration_seconds: f64,
    pub has_audio: bool,
}

// --- ffprobe JSON ---------------------------------------------------------

#[derive(Debug, Deserialize)]
struct ProbeOutput {
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    format: Option<ProbeFormat>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    #[serde(default)]
    codec_type: Option<String>,
    #[serde(default)]
    width: Option<u32>,
    #[serde(default)]
    height: Option<u32>,
    /// Average frame rate as a rational string, e.g. "30000/1001".
    #[serde(default)]
    avg_frame_rate: Option<String>,
    #[serde(default)]
    r_frame_rate: Option<String>,
    #[serde(default)]
    duration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeFormat {
    #[serde(default)]
    duration: Option<String>,
}

/// Parse an ffprobe rational like "30000/1001" or "25/1".
///
/// ffprobe reports "0/0" for streams where it cannot determine the rate, which
/// must not become NaN or a division by zero.
fn parse_rational(value: &str) -> Option<f64> {
    let (numerator, denominator) = value.split_once('/')?;
    let numerator: f64 = numerator.trim().parse().ok()?;
    let denominator: f64 = denominator.trim().parse().ok()?;

    if denominator == 0.0 || !numerator.is_finite() || numerator <= 0.0 {
        return None;
    }

    let fps = numerator / denominator;
    if fps.is_finite() && fps > 0.0 {
        Some(fps)
    } else {
        None
    }
}

/// Turn parsed ffprobe output into a [`SourceInfo`].
///
/// Split out from the subprocess call so it can be tested against captured
/// ffprobe JSON without running FFmpeg.
fn source_info_from(probe: &ProbeOutput) -> Result<SourceInfo> {
    let video = probe
        .streams
        .iter()
        .find(|stream| stream.codec_type.as_deref() == Some("video"))
        .ok_or_else(|| anyhow!("no video stream found; not a usable video file"))?;

    let width = video
        .width
        .ok_or_else(|| anyhow!("video stream has no width"))?;
    let height = video
        .height
        .ok_or_else(|| anyhow!("video stream has no height"))?;

    if width == 0 || height == 0 {
        bail!("video stream reports a zero dimension ({width}x{height})");
    }

    // avg_frame_rate is the honest average; r_frame_rate is the container's
    // nominal rate and is wrong for variable frame rate content. Prefer the
    // average and fall back only if it is unusable.
    let fps = video
        .avg_frame_rate
        .as_deref()
        .and_then(parse_rational)
        .or_else(|| video.r_frame_rate.as_deref().and_then(parse_rational))
        .unwrap_or(30.0);

    // Format duration is usually present and more reliable than the stream's;
    // fall back to the video stream for containers that omit it.
    let duration_seconds = probe
        .format
        .as_ref()
        .and_then(|format| format.duration.as_deref())
        .and_then(|value| value.parse::<f64>().ok())
        .or_else(|| {
            video
                .duration
                .as_deref()
                .and_then(|value| value.parse::<f64>().ok())
        })
        .filter(|value| value.is_finite() && *value > 0.0)
        .ok_or_else(|| anyhow!("could not determine the source duration"))?;

    let has_audio = probe
        .streams
        .iter()
        .any(|stream| stream.codec_type.as_deref() == Some("audio"));

    Ok(SourceInfo {
        width,
        height,
        fps,
        duration_seconds,
        has_audio,
    })
}

/// Run `ffprobe` against a local file.
pub async fn probe(path: &Path) -> Result<SourceInfo> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(path)
        // ffprobe should never need stdin; leaving it attached risks a hang.
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .with_context(|| {
            format!(
                "failed to run ffprobe on {} (is ffprobe on PATH?)",
                path.display()
            )
        })?;

    if !output.status.success() {
        // ffprobe writes diagnostics to stderr; surfacing it is the difference
        // between a usable error and "exit status 1".
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!(
            "ffprobe failed for {} with {}: {}",
            path.display(),
            output.status,
            stderr.trim()
        );
    }

    let parsed: ProbeOutput =
        serde_json::from_slice(&output.stdout).context("could not parse ffprobe JSON output")?;

    source_info_from(&parsed).with_context(|| format!("probing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<SourceInfo> {
        let probe: ProbeOutput = serde_json::from_str(json).expect("valid JSON");
        source_info_from(&probe)
    }

    #[test]
    fn parses_a_typical_1080p_file() {
        let info = parse(
            r#"{
              "streams": [
                {"codec_type":"video","width":1920,"height":1080,
                 "avg_frame_rate":"30/1","r_frame_rate":"30/1"},
                {"codec_type":"audio"}
              ],
              "format": {"duration":"123.456"}
            }"#,
        )
        .expect("should parse");

        assert_eq!(info.width, 1920);
        assert_eq!(info.height, 1080);
        assert!((info.fps - 30.0).abs() < 1e-9);
        assert!((info.duration_seconds - 123.456).abs() < 1e-9);
        assert!(info.has_audio);
    }

    #[test]
    fn detects_a_video_with_no_audio_stream() {
        let info = parse(
            r#"{
              "streams": [{"codec_type":"video","width":640,"height":360,
                           "avg_frame_rate":"25/1"}],
              "format": {"duration":"10"}
            }"#,
        )
        .expect("should parse");

        assert!(!info.has_audio);
    }

    #[test]
    fn parses_ntsc_fractional_frame_rates() {
        let info = parse(
            r#"{
              "streams": [{"codec_type":"video","width":1920,"height":1080,
                           "avg_frame_rate":"30000/1001"}],
              "format": {"duration":"60"}
            }"#,
        )
        .expect("should parse");

        assert!((info.fps - 29.97002997).abs() < 1e-6, "got {}", info.fps);
    }

    #[test]
    fn falls_back_to_r_frame_rate_when_the_average_is_unusable() {
        // ffprobe emits "0/0" when it cannot determine the average rate.
        let info = parse(
            r#"{
              "streams": [{"codec_type":"video","width":1280,"height":720,
                           "avg_frame_rate":"0/0","r_frame_rate":"24/1"}],
              "format": {"duration":"5"}
            }"#,
        )
        .expect("should parse");

        assert!((info.fps - 24.0).abs() < 1e-9);
    }

    #[test]
    fn defaults_to_30fps_when_no_rate_is_reported() {
        let info = parse(
            r#"{
              "streams": [{"codec_type":"video","width":1280,"height":720,
                           "avg_frame_rate":"0/0","r_frame_rate":"0/0"}],
              "format": {"duration":"5"}
            }"#,
        )
        .expect("should parse");

        assert!((info.fps - 30.0).abs() < 1e-9);
    }

    #[test]
    fn falls_back_to_the_stream_duration() {
        let info = parse(
            r#"{
              "streams": [{"codec_type":"video","width":640,"height":480,
                           "avg_frame_rate":"30/1","duration":"42.5"}],
              "format": {}
            }"#,
        )
        .expect("should parse");

        assert!((info.duration_seconds - 42.5).abs() < 1e-9);
    }

    #[test]
    fn ignores_non_video_streams_when_picking_dimensions() {
        // Subtitle and data streams come first in some containers.
        let info = parse(
            r#"{
              "streams": [
                {"codec_type":"subtitle"},
                {"codec_type":"data"},
                {"codec_type":"video","width":1280,"height":720,"avg_frame_rate":"30/1"},
                {"codec_type":"audio"}
              ],
              "format": {"duration":"10"}
            }"#,
        )
        .expect("should parse");

        assert_eq!((info.width, info.height), (1280, 720));
        assert!(info.has_audio);
    }

    #[test]
    fn rejects_a_file_with_no_video_stream() {
        let error = parse(r#"{"streams":[{"codec_type":"audio"}],"format":{"duration":"10"}}"#)
            .expect_err("audio-only input must be rejected");

        assert!(
            error.to_string().contains("no video stream"),
            "got: {error}"
        );
    }

    #[test]
    fn rejects_a_video_stream_with_zero_dimensions() {
        let error = parse(
            r#"{
              "streams":[{"codec_type":"video","width":0,"height":0,
                          "avg_frame_rate":"30/1"}],
              "format":{"duration":"10"}
            }"#,
        )
        .expect_err("zero dimensions must be rejected");

        assert!(error.to_string().contains("zero dimension"), "got: {error}");
    }

    #[test]
    fn rejects_input_with_no_usable_duration() {
        let error = parse(
            r#"{
              "streams":[{"codec_type":"video","width":640,"height":480,
                          "avg_frame_rate":"30/1"}],
              "format":{}
            }"#,
        )
        .expect_err("missing duration must be rejected");

        assert!(error.to_string().contains("duration"), "got: {error}");
    }

    #[test]
    fn rejects_a_zero_or_negative_duration() {
        for duration in ["0", "-1", "0.0"] {
            let json = format!(
                r#"{{"streams":[{{"codec_type":"video","width":640,"height":480,
                    "avg_frame_rate":"30/1"}}],"format":{{"duration":"{duration}"}}}}"#
            );
            assert!(
                parse(&json).is_err(),
                "duration {duration} should be rejected"
            );
        }
    }

    #[test]
    fn parse_rational_handles_edge_cases() {
        assert_eq!(parse_rational("30/1"), Some(30.0));
        assert_eq!(parse_rational("0/0"), None);
        assert_eq!(parse_rational("30/0"), None);
        assert_eq!(parse_rational("0/1"), None);
        assert_eq!(parse_rational("-30/1"), None);
        assert_eq!(parse_rational("garbage"), None);
        assert_eq!(parse_rational(""), None);
        assert_eq!(parse_rational("30"), None);
    }
}

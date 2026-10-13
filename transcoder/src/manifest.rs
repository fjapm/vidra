//! `media/{id}/manifest.json`: what the application reads to find out whether
//! a video is playable, and what it contains.
//!
//! This is also the only durable record of a failure. The exit status tells
//! whoever invoked vidra *that* a run failed, but it does not survive the
//! process, and anyone who later finds the output directory has nothing else
//! to go on. So a `failed` manifest is written in its place, carrying the
//! error, which is what lets an application show a reason rather than a
//! spinner.

use crate::ladder::Rendition;
use serde::{Deserialize, Serialize};

/// Processing outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Ready,
    Failed,
}

/// One entry of the rendition list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestRendition {
    pub height: u32,
    pub width: u32,
    /// Average video bitrate in kbps.
    pub bitrate_kbps: u32,
    /// Media playlist path, relative to `media/{id}/`.
    pub playlist: String,
}

impl From<&Rendition> for ManifestRendition {
    fn from(rendition: &Rendition) -> Self {
        Self {
            height: rendition.height,
            width: rendition.width,
            bitrate_kbps: rendition.video_bitrate_kbps,
            playlist: format!("{}/index.m3u8", rendition.dir_name()),
        }
    }
}

/// One caption track.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestCaption {
    pub language: String,
    /// Path to the WebVTT file, relative to `media/{id}/`.
    pub path: String,
    /// True when produced by whisper.cpp rather than supplied by the uploader.
    ///
    /// Worth surfacing in the UI: machine transcription is good but not
    /// authoritative, and an auto-generated track should be labelled as such.
    pub auto_generated: bool,
}

/// Source properties, echoed so the application does not need to re-probe.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SourceSummary {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
}

/// `media/{id}/manifest.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Manifest {
    pub video_id: String,
    pub status: Status,

    /// Duration in seconds. Absent on a failure that happened before probing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_seconds: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<SourceSummary>,

    /// Highest-bandwidth rendition first, matching the master playlist.
    #[serde(default)]
    pub renditions: Vec<ManifestRendition>,

    #[serde(default)]
    pub captions: Vec<ManifestCaption>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub poster: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumbnails: Option<ThumbnailInfo>,

    /// Master playlist path, relative to `media/{id}/`. Absent on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub master_playlist: Option<String>,

    /// RFC 3339 UTC timestamp.
    pub processed_at: String,

    /// Human-readable failure reason. Present if and only if status is failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Scrubbing-preview sprite metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThumbnailInfo {
    /// Sprite image path, relative to `media/{id}/`.
    pub sprite: String,
    /// WebVTT track mapping times to sprite regions.
    pub vtt: String,
    pub interval_seconds: u32,
    pub tile_width: u32,
    pub tile_height: u32,
    pub columns: u32,
    pub count: u32,
}

/// Inputs for a successful manifest.
///
/// A struct rather than eight positional parameters: at the call site
/// `poster`, `thumbnails` and `processed_at` are all easy to transpose, and
/// two of them are `Option<String>`, so the compiler would not catch it.
#[derive(Debug)]
pub struct ReadyManifest<'a> {
    pub video_id: &'a str,
    pub duration_seconds: f64,
    pub source: SourceSummary,
    pub renditions: &'a [Rendition],
    pub captions: Vec<ManifestCaption>,
    pub poster: Option<String>,
    pub thumbnails: Option<ThumbnailInfo>,
    /// RFC 3339 UTC timestamp.
    pub processed_at: String,
}

impl Manifest {
    /// Build a `ready` manifest.
    pub fn ready(input: ReadyManifest<'_>) -> Self {
        Self {
            video_id: input.video_id.to_string(),
            status: Status::Ready,
            duration_seconds: Some(input.duration_seconds),
            source: Some(input.source),
            renditions: input
                .renditions
                .iter()
                .map(ManifestRendition::from)
                .collect(),
            captions: input.captions,
            poster: input.poster,
            thumbnails: input.thumbnails,
            master_playlist: Some("master.m3u8".to_string()),
            processed_at: input.processed_at,
            error: None,
        }
    }

    /// Build a `failed` manifest.
    ///
    /// Everything optional is omitted: a failed manifest must not advertise a
    /// master playlist, because the application would then try to play it.
    pub fn failed(video_id: &str, error: &str, processed_at: String) -> Self {
        Self {
            video_id: video_id.to_string(),
            status: Status::Failed,
            duration_seconds: None,
            source: None,
            renditions: Vec::new(),
            captions: Vec::new(),
            poster: None,
            thumbnails: None,
            master_playlist: None,
            processed_at,
            error: Some(error.to_string()),
        }
    }

    /// Serialise for upload. Pretty-printed because it is read by humans at
    /// least as often as by code.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ladder::build_ladder;
    use crate::probe::SourceInfo;
    use serde_json::Value;

    const WHEN: &str = "2026-10-09T12:00:00Z";

    fn ladder() -> Vec<Rendition> {
        build_ladder(&SourceInfo {
            width: 1920,
            height: 1080,
            fps: 30.0,
            duration_seconds: 120.0,
            has_audio: true,
        })
    }

    fn summary() -> SourceSummary {
        SourceSummary {
            width: 1920,
            height: 1080,
            fps: 30.0,
        }
    }

    fn ready_manifest() -> Manifest {
        Manifest::ready(ReadyManifest {
            video_id: "20260101120000-deadbeef",
            duration_seconds: 123.45,
            source: summary(),
            renditions: &ladder(),
            captions: vec![
                ManifestCaption {
                    language: "en".into(),
                    path: "captions/en.vtt".into(),
                    auto_generated: false,
                },
                ManifestCaption {
                    language: "fr".into(),
                    path: "captions/fr.vtt".into(),
                    auto_generated: true,
                },
            ],
            poster: Some("poster.jpg".into()),
            thumbnails: Some(ThumbnailInfo {
                sprite: "thumbs.jpg".into(),
                vtt: "thumbs.vtt".into(),
                interval_seconds: 10,
                tile_width: 160,
                tile_height: 90,
                columns: 5,
                count: 13,
            }),
            processed_at: WHEN.to_string(),
        })
    }

    // --- ready ------------------------------------------------------------

    #[test]
    fn a_ready_manifest_records_everything_the_app_needs() {
        let json: Value = serde_json::from_str(&ready_manifest().to_json().unwrap()).unwrap();

        assert_eq!(json["status"], "ready");
        assert_eq!(json["video_id"], "20260101120000-deadbeef");
        assert_eq!(json["duration_seconds"], 123.45);
        assert_eq!(json["master_playlist"], "master.m3u8");
        assert_eq!(json["poster"], "poster.jpg");
        assert_eq!(json["processed_at"], WHEN);
        assert_eq!(json["source"]["width"], 1920);
        assert_eq!(json["source"]["height"], 1080);
    }

    #[test]
    fn a_ready_manifest_has_no_error_field_at_all() {
        // Not `"error": null`: a consumer checking for the key's presence must
        // not see one on success.
        let json = ready_manifest().to_json().unwrap();
        assert!(!json.contains("\"error\""), "got:\n{json}");
    }

    #[test]
    fn renditions_are_listed_highest_first_with_their_playlists() {
        let manifest = ready_manifest();

        let heights: Vec<u32> = manifest.renditions.iter().map(|r| r.height).collect();
        assert_eq!(heights, vec![1080, 720, 480, 360]);

        assert_eq!(manifest.renditions[0].playlist, "1080p/index.m3u8");
        assert_eq!(manifest.renditions[0].width, 1920);
        assert_eq!(manifest.renditions[0].bitrate_kbps, 5000);
        assert_eq!(manifest.renditions[3].playlist, "360p/index.m3u8");
    }

    #[test]
    fn captions_record_their_auto_generated_flag() {
        let manifest = ready_manifest();

        assert_eq!(manifest.captions.len(), 2);
        assert_eq!(manifest.captions[0].language, "en");
        assert!(!manifest.captions[0].auto_generated, "uploaded by the user");
        assert_eq!(manifest.captions[1].language, "fr");
        assert!(manifest.captions[1].auto_generated, "produced by whisper");
    }

    #[test]
    fn caption_paths_are_relative_to_the_video_directory() {
        // The delivery Worker maps <videoId>/<path>, so an absolute path or
        // one containing the media/ prefix would 404.
        for caption in ready_manifest().captions {
            assert!(!caption.path.starts_with('/'), "{}", caption.path);
            assert!(!caption.path.starts_with("media/"), "{}", caption.path);
            assert!(caption.path.starts_with("captions/"), "{}", caption.path);
        }
    }

    #[test]
    fn thumbnail_info_round_trips() {
        let json: Value = serde_json::from_str(&ready_manifest().to_json().unwrap()).unwrap();

        assert_eq!(json["thumbnails"]["sprite"], "thumbs.jpg");
        assert_eq!(json["thumbnails"]["vtt"], "thumbs.vtt");
        assert_eq!(json["thumbnails"]["interval_seconds"], 10);
        assert_eq!(json["thumbnails"]["columns"], 5);
        assert_eq!(json["thumbnails"]["count"], 13);
    }

    #[test]
    fn a_ready_manifest_with_no_captions_serialises_an_empty_list() {
        // An empty array, not a missing key: a consumer can iterate without a
        // null check.
        let manifest = Manifest::ready(ReadyManifest {
            video_id: "abc",
            duration_seconds: 10.0,
            source: summary(),
            renditions: &ladder(),
            captions: Vec::new(),
            poster: None,
            thumbnails: None,
            processed_at: WHEN.to_string(),
        });
        let json: Value = serde_json::from_str(&manifest.to_json().unwrap()).unwrap();

        assert!(json["captions"].is_array());
        assert_eq!(json["captions"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn optional_media_is_omitted_when_absent() {
        let manifest = Manifest::ready(ReadyManifest {
            video_id: "abc",
            duration_seconds: 10.0,
            source: summary(),
            renditions: &ladder(),
            captions: Vec::new(),
            poster: None,
            thumbnails: None,
            processed_at: WHEN.to_string(),
        });
        let json = manifest.to_json().unwrap();

        assert!(!json.contains("\"poster\""));
        assert!(!json.contains("\"thumbnails\""));
    }

    // --- failed -----------------------------------------------------------

    #[test]
    fn a_failed_manifest_records_the_error() {
        let manifest = Manifest::failed("abc", "ffmpeg exited with status 1", WHEN.to_string());
        let json: Value = serde_json::from_str(&manifest.to_json().unwrap()).unwrap();

        assert_eq!(json["status"], "failed");
        assert_eq!(json["video_id"], "abc");
        assert_eq!(json["error"], "ffmpeg exited with status 1");
        assert_eq!(json["processed_at"], WHEN);
    }

    #[test]
    fn a_failed_manifest_advertises_no_playable_media() {
        // If master_playlist were present, the application would try to play a
        // video that does not exist.
        let manifest = Manifest::failed("abc", "boom", WHEN.to_string());
        let json = manifest.to_json().unwrap();

        assert!(!json.contains("\"master_playlist\""));
        assert!(!json.contains("\"poster\""));
        assert!(!json.contains("\"duration_seconds\""));
        assert!(manifest.renditions.is_empty());
        assert!(manifest.captions.is_empty());
    }

    // --- round-tripping ---------------------------------------------------

    #[test]
    fn a_ready_manifest_round_trips_through_json() {
        let original = ready_manifest();
        let decoded: Manifest = serde_json::from_str(&original.to_json().unwrap()).unwrap();

        assert_eq!(original, decoded);
    }

    #[test]
    fn a_failed_manifest_round_trips_through_json() {
        let original = Manifest::failed("abc", "the disk filled up", WHEN.to_string());
        let decoded: Manifest = serde_json::from_str(&original.to_json().unwrap()).unwrap();

        assert_eq!(original, decoded);
    }

    #[test]
    fn status_serialises_as_a_lowercase_string() {
        // The acceptance criteria check for `"status": "ready"` literally.
        assert_eq!(serde_json::to_string(&Status::Ready).unwrap(), "\"ready\"");
        assert_eq!(
            serde_json::to_string(&Status::Failed).unwrap(),
            "\"failed\""
        );
    }

    #[test]
    fn an_error_message_with_quotes_and_newlines_stays_valid_json() {
        // FFmpeg stderr is multi-line and full of quotes; it is embedded
        // verbatim, so this must not corrupt the manifest.
        let nasty = "ffmpeg said: \"no such file\"\n\tat line 2\\x00";
        let manifest = Manifest::failed("abc", nasty, WHEN.to_string());

        let json = manifest.to_json().unwrap();
        let decoded: Manifest = serde_json::from_str(&json).unwrap();

        assert_eq!(decoded.error.as_deref(), Some(nasty));
    }

    #[test]
    fn manifest_json_is_pretty_printed() {
        // It is read by hand during incidents often enough to matter.
        assert!(ready_manifest().to_json().unwrap().contains("\n  "));
    }
}

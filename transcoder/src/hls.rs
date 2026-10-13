//! HLS playlist generation.
//!
//! FFmpeg writes each rendition's own `index.m3u8`, but not the master
//! playlist that ties the renditions and subtitle tracks together, and not the
//! WebVTT subtitle playlists. Both are written here.
//!
//! Pure string generation, so the exact bytes a player will parse are
//! unit-testable without transcoding anything.

use crate::ladder::Rendition;

/// A caption track to advertise in the master playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubtitleTrack {
    /// BCP 47-ish language tag, e.g. "en" or "pt-BR".
    pub language: String,
    /// Human-readable name shown in the player's caption menu.
    pub name: String,
    /// True for the track selected when captions are turned on.
    pub default: bool,
}

impl SubtitleTrack {
    /// Best-effort display name for a language tag.
    ///
    /// Only the languages likely to appear are mapped; anything else falls
    /// back to the tag itself, which is still better than a blank menu entry.
    pub fn display_name(language: &str) -> String {
        let base = language.split('-').next().unwrap_or(language);
        let name = match base {
            "en" => "English",
            "es" => "Spanish",
            "fr" => "French",
            "de" => "German",
            "it" => "Italian",
            "pt" => "Portuguese",
            "nl" => "Dutch",
            "pl" => "Polish",
            "ru" => "Russian",
            "ja" => "Japanese",
            "ko" => "Korean",
            "zh" => "Chinese",
            "ar" => "Arabic",
            "hi" => "Hindi",
            "tr" => "Turkish",
            "sv" => "Swedish",
            "da" => "Danish",
            "no" | "nb" => "Norwegian",
            "fi" => "Finnish",
            "cs" => "Czech",
            "el" => "Greek",
            "he" => "Hebrew",
            "uk" => "Ukrainian",
            "vi" => "Vietnamese",
            "th" => "Thai",
            "id" => "Indonesian",
            other => return other.to_string(),
        };

        // Keep a region suffix visible so "pt" and "pt-BR" are distinguishable
        // in the caption menu.
        match language.split_once('-') {
            Some((_, region)) => format!("{name} ({region})"),
            None => name.to_string(),
        }
    }
}

/// Escape a value for an HLS quoted-string attribute.
///
/// Quoted strings in HLS cannot contain double quotes, carriage returns or
/// line feeds. Caption names come from language tags rather than user input,
/// but a malformed playlist is a total playback failure, so it is cheap to be
/// strict here.
fn escape_attribute(value: &str) -> String {
    value
        .chars()
        .filter(|c| *c != '"' && *c != '\r' && *c != '\n')
        .collect()
}

/// The subtitle GROUP-ID referenced by every variant.
const SUBTITLE_GROUP: &str = "subs";

/// Write the master playlist.
///
/// Renditions must be ordered highest-bandwidth first. Subtitle tracks, if
/// any, are advertised as EXT-X-MEDIA entries and joined to every variant via
/// SUBTITLES - without that join, players show no caption menu at all.
pub fn master_playlist(renditions: &[Rendition], subtitles: &[SubtitleTrack]) -> String {
    let mut out = String::new();

    out.push_str("#EXTM3U\n");
    // Version 7 is the floor for fMP4 segments (EXT-X-MAP with fMP4).
    out.push_str("#EXT-X-VERSION:7\n");
    // Every segment starts with a keyframe, so a player may start at any one.
    out.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");

    if !subtitles.is_empty() {
        out.push('\n');
        for track in subtitles {
            out.push_str(&format!(
                "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"{group}\",NAME=\"{name}\",\
LANGUAGE=\"{language}\",DEFAULT={default},AUTOSELECT={autoselect},FORCED=NO,\
URI=\"captions/{language}.m3u8\"\n",
                group = SUBTITLE_GROUP,
                name = escape_attribute(&track.name),
                language = escape_attribute(&track.language),
                default = if track.default { "YES" } else { "NO" },
                autoselect = if track.default { "YES" } else { "NO" },
            ));
        }
    }

    for rendition in renditions {
        out.push('\n');
        out.push_str(&format!(
            "#EXT-X-STREAM-INF:BANDWIDTH={bandwidth},AVERAGE-BANDWIDTH={average},\
RESOLUTION={width}x{height},CODECS=\"{codecs}\"",
            bandwidth = rendition.peak_bandwidth_bps(),
            average = rendition.average_bandwidth_bps(),
            width = rendition.width,
            height = rendition.height,
            codecs = rendition.codecs(),
        ));

        if !subtitles.is_empty() {
            out.push_str(&format!(",SUBTITLES=\"{SUBTITLE_GROUP}\""));
        }

        out.push('\n');
        out.push_str(&format!("{}/index.m3u8\n", rendition.dir_name()));
    }

    out
}

/// Write the WebVTT media playlist for one caption track.
///
/// A subtitle rendition in HLS is a playlist, not a bare .vtt file, even when
/// the whole track is a single segment. Pointing EXT-X-MEDIA straight at a
/// .vtt makes Safari ignore the track.
pub fn subtitle_playlist(language: &str, duration_seconds: f64) -> String {
    // The single VTT segment spans the whole programme, so TARGETDURATION must
    // cover it; rounding down would make the playlist invalid.
    let duration = if duration_seconds.is_finite() && duration_seconds > 0.0 {
        duration_seconds
    } else {
        0.0
    };
    let target = duration.ceil().max(1.0) as u64;

    format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-TARGETDURATION:{target}\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXTINF:{duration:.3},\n\
         {language}.vtt\n\
         #EXT-X-ENDLIST\n"
    )
}

/// Write `thumbs.vtt`, the scrubbing-preview track.
///
/// Each cue points at a tile of the sprite with a spatial media fragment
/// (`#xywh=`), which is the convention hls.js, Video.js and Vidstack all
/// understand for thumbnail tracks.
pub fn thumbnail_vtt(
    sprite_name: &str,
    interval_seconds: u32,
    tile_width: u32,
    tile_height: u32,
    columns: u32,
    tile_count: u32,
) -> String {
    let mut out = String::from("WEBVTT\n");

    if interval_seconds == 0 || columns == 0 || tile_count == 0 {
        return out;
    }

    for index in 0..tile_count {
        let start = index * interval_seconds;
        let end = start + interval_seconds;
        let x = (index % columns) * tile_width;
        let y = (index / columns) * tile_height;

        out.push('\n');
        out.push_str(&format!(
            "{}  --> {}\n",
            format_timestamp(f64::from(start)),
            format_timestamp(f64::from(end)),
        ));
        out.push_str(&format!(
            "{sprite_name}#xywh={x},{y},{tile_width},{tile_height}\n"
        ));
    }

    out
}

/// Format seconds as a WebVTT timestamp: HH:MM:SS.mmm.
pub fn format_timestamp(seconds: f64) -> String {
    let total = if seconds.is_finite() && seconds > 0.0 {
        seconds
    } else {
        0.0
    };

    let hours = (total / 3600.0).floor() as u64;
    let minutes = ((total % 3600.0) / 60.0).floor() as u64;
    let secs = total % 60.0;

    format!("{hours:02}:{minutes:02}:{secs:06.3}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ladder::build_ladder;
    use crate::probe::SourceInfo;

    fn ladder_1080p() -> Vec<Rendition> {
        build_ladder(&SourceInfo {
            width: 1920,
            height: 1080,
            fps: 30.0,
            duration_seconds: 120.0,
            has_audio: true,
        })
    }

    fn track(language: &str, default: bool) -> SubtitleTrack {
        SubtitleTrack {
            language: language.to_string(),
            name: SubtitleTrack::display_name(language),
            default,
        }
    }

    // --- master playlist structure ----------------------------------------

    #[test]
    fn master_playlist_starts_with_the_required_tags() {
        let playlist = master_playlist(&ladder_1080p(), &[]);
        let mut lines = playlist.lines();

        // #EXTM3U must be the very first line or players reject the playlist.
        assert_eq!(lines.next(), Some("#EXTM3U"));
        assert_eq!(lines.next(), Some("#EXT-X-VERSION:7"));
        assert_eq!(lines.next(), Some("#EXT-X-INDEPENDENT-SEGMENTS"));
    }

    #[test]
    fn master_playlist_declares_version_7_for_fmp4() {
        // fMP4 segments require version 7; a lower version makes Safari and
        // hls.js refuse the stream.
        assert!(master_playlist(&ladder_1080p(), &[]).contains("#EXT-X-VERSION:7"));
    }

    #[test]
    fn master_playlist_lists_every_rendition_with_its_path() {
        let playlist = master_playlist(&ladder_1080p(), &[]);

        for height in [1080, 720, 480, 360] {
            assert!(
                playlist.contains(&format!("{height}p/index.m3u8")),
                "missing {height}p variant in:\n{playlist}"
            );
        }
    }

    #[test]
    fn every_stream_inf_is_followed_immediately_by_its_uri() {
        // A tag/URI pair split by anything else is a malformed playlist.
        let playlist = master_playlist(&ladder_1080p(), &[track("en", true)]);
        let lines: Vec<&str> = playlist.lines().collect();

        let mut pairs = 0;
        for (index, line) in lines.iter().enumerate() {
            if line.starts_with("#EXT-X-STREAM-INF:") {
                let next = lines.get(index + 1).expect("a URI must follow");
                assert!(
                    next.ends_with("/index.m3u8") && !next.starts_with('#'),
                    "STREAM-INF must be followed by its URI, got {next:?}"
                );
                pairs += 1;
            }
        }
        assert_eq!(pairs, 4);
    }

    #[test]
    fn master_playlist_includes_bandwidth_resolution_and_codecs() {
        let playlist = master_playlist(&ladder_1080p(), &[]);

        assert!(playlist.contains("RESOLUTION=1920x1080"));
        assert!(playlist.contains("RESOLUTION=1280x720"));
        assert!(playlist.contains("RESOLUTION=854x480"));
        assert!(playlist.contains("RESOLUTION=640x360"));

        assert!(playlist.contains("CODECS=\"avc1.640028,mp4a.40.2\""));
        assert!(playlist.contains("BANDWIDTH="));
        assert!(playlist.contains("AVERAGE-BANDWIDTH="));
    }

    #[test]
    fn bandwidth_values_match_the_rendition() {
        let ladder = ladder_1080p();
        let playlist = master_playlist(&ladder, &[]);

        for rendition in &ladder {
            assert!(
                playlist.contains(&format!("BANDWIDTH={}", rendition.peak_bandwidth_bps())),
                "missing BANDWIDTH for {rendition:?}"
            );
            assert!(
                playlist.contains(&format!(
                    "AVERAGE-BANDWIDTH={}",
                    rendition.average_bandwidth_bps()
                )),
                "missing AVERAGE-BANDWIDTH for {rendition:?}"
            );
        }
    }

    #[test]
    fn renditions_are_listed_highest_bandwidth_first() {
        let playlist = master_playlist(&ladder_1080p(), &[]);

        let order: Vec<usize> = [1080, 720, 480, 360]
            .iter()
            .map(|h| {
                playlist
                    .find(&format!("{h}p/index.m3u8"))
                    .expect("variant present")
            })
            .collect();

        assert!(
            order.windows(2).all(|w| w[0] < w[1]),
            "variants out of order: {order:?}"
        );
    }

    // --- subtitles --------------------------------------------------------

    #[test]
    fn master_playlist_has_no_subtitle_tags_when_there_are_no_captions() {
        let playlist = master_playlist(&ladder_1080p(), &[]);

        assert!(!playlist.contains("EXT-X-MEDIA"));
        assert!(!playlist.contains("SUBTITLES="));
    }

    #[test]
    fn master_playlist_advertises_each_subtitle_track() {
        let playlist =
            master_playlist(&ladder_1080p(), &[track("en", true), track("pt-BR", false)]);

        assert!(playlist.contains("#EXT-X-MEDIA:TYPE=SUBTITLES"));
        assert!(playlist.contains("LANGUAGE=\"en\""));
        assert!(playlist.contains("LANGUAGE=\"pt-BR\""));
        assert!(playlist.contains("URI=\"captions/en.m3u8\""));
        assert!(playlist.contains("URI=\"captions/pt-BR.m3u8\""));
    }

    #[test]
    fn subtitle_tracks_are_joined_to_every_variant() {
        // Without SUBTITLES on the variants, the EXT-X-MEDIA entries exist but
        // no player offers a caption menu.
        let playlist = master_playlist(&ladder_1080p(), &[track("en", true)]);

        let variants = playlist
            .lines()
            .filter(|line| line.starts_with("#EXT-X-STREAM-INF:"))
            .count();
        let joined = playlist
            .lines()
            .filter(|line| {
                line.starts_with("#EXT-X-STREAM-INF:") && line.contains("SUBTITLES=\"subs\"")
            })
            .count();

        assert_eq!(variants, 4);
        assert_eq!(joined, variants, "every variant must reference the group");
    }

    #[test]
    fn exactly_one_subtitle_track_is_default() {
        let playlist = master_playlist(&ladder_1080p(), &[track("en", true), track("fr", false)]);

        assert_eq!(playlist.matches("DEFAULT=YES").count(), 1);
        assert_eq!(playlist.matches("DEFAULT=NO").count(), 1);
    }

    #[test]
    fn subtitle_group_id_is_consistent_between_media_and_variants() {
        let playlist = master_playlist(&ladder_1080p(), &[track("en", true)]);

        let media_group = playlist
            .lines()
            .find(|l| l.starts_with("#EXT-X-MEDIA:"))
            .and_then(|l| l.split("GROUP-ID=\"").nth(1))
            .and_then(|s| s.split('"').next())
            .expect("GROUP-ID present");

        assert!(playlist.contains(&format!("SUBTITLES=\"{media_group}\"")));
    }

    #[test]
    fn attribute_values_cannot_break_out_of_their_quotes() {
        let playlist = master_playlist(
            &ladder_1080p(),
            &[SubtitleTrack {
                language: "en".into(),
                name: "Bad\"Name\r\nInjected".into(),
                default: true,
            }],
        );

        assert!(playlist.contains("NAME=\"BadNameInjected\""));
        // The injected newline must not have produced an extra line.
        assert!(!playlist.contains("Injected\n"));
    }

    // --- language names ---------------------------------------------------

    #[test]
    fn display_name_maps_known_languages() {
        assert_eq!(SubtitleTrack::display_name("en"), "English");
        assert_eq!(SubtitleTrack::display_name("fr"), "French");
        assert_eq!(SubtitleTrack::display_name("zh"), "Chinese");
    }

    #[test]
    fn display_name_keeps_the_region_suffix() {
        // "Portuguese" and "Portuguese (BR)" must be distinguishable.
        assert_eq!(SubtitleTrack::display_name("pt-BR"), "Portuguese (BR)");
        assert_eq!(SubtitleTrack::display_name("en-GB"), "English (GB)");
    }

    #[test]
    fn display_name_falls_back_to_the_tag() {
        assert_eq!(SubtitleTrack::display_name("xyz"), "xyz");
    }

    // --- subtitle playlists -----------------------------------------------

    #[test]
    fn subtitle_playlist_is_a_valid_vod_playlist() {
        let playlist = subtitle_playlist("en", 123.4);

        assert!(playlist.starts_with("#EXTM3U\n"));
        assert!(playlist.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(playlist.contains("#EXT-X-ENDLIST"));
        assert!(playlist.contains("en.vtt"));
    }

    #[test]
    fn subtitle_playlist_target_duration_covers_the_segment() {
        // TARGETDURATION must be >= every EXTINF, so it rounds up.
        let playlist = subtitle_playlist("en", 123.4);

        assert!(playlist.contains("#EXT-X-TARGETDURATION:124"));
        assert!(playlist.contains("#EXTINF:123.400,"));
    }

    #[test]
    fn subtitle_playlist_handles_a_whole_number_duration() {
        let playlist = subtitle_playlist("en", 60.0);
        assert!(playlist.contains("#EXT-X-TARGETDURATION:60"));
    }

    #[test]
    fn subtitle_playlist_survives_a_nonsense_duration() {
        for duration in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let playlist = subtitle_playlist("en", duration);
            // TARGETDURATION:0 would be invalid, so it floors at 1.
            assert!(
                playlist.contains("#EXT-X-TARGETDURATION:1"),
                "duration {duration} produced:\n{playlist}"
            );
        }
    }

    // --- thumbnail track --------------------------------------------------

    #[test]
    fn thumbnail_vtt_emits_one_cue_per_tile() {
        let vtt = thumbnail_vtt("thumbs.jpg", 10, 160, 90, 5, 12);

        assert!(vtt.starts_with("WEBVTT\n"));
        assert_eq!(vtt.matches("thumbs.jpg#xywh=").count(), 12);
    }

    #[test]
    fn thumbnail_vtt_cues_are_contiguous_and_in_order() {
        let vtt = thumbnail_vtt("thumbs.jpg", 10, 160, 90, 5, 3);

        assert!(vtt.contains("00:00:00.000  --> 00:00:10.000"));
        assert!(vtt.contains("00:00:10.000  --> 00:00:20.000"));
        assert!(vtt.contains("00:00:20.000  --> 00:00:30.000"));
    }

    #[test]
    fn thumbnail_vtt_walks_the_sprite_grid_row_by_row() {
        let vtt = thumbnail_vtt("thumbs.jpg", 10, 160, 90, 5, 7);

        // First row.
        assert!(vtt.contains("#xywh=0,0,160,90"));
        assert!(vtt.contains("#xywh=160,0,160,90"));
        assert!(vtt.contains("#xywh=640,0,160,90"));
        // Second row begins after `columns` tiles.
        assert!(vtt.contains("#xywh=0,90,160,90"));
        assert!(vtt.contains("#xywh=160,90,160,90"));
    }

    #[test]
    fn thumbnail_vtt_is_empty_for_degenerate_input() {
        for (interval, columns, count) in [(0, 5, 10), (10, 0, 10), (10, 5, 0)] {
            let vtt = thumbnail_vtt("thumbs.jpg", interval, 160, 90, columns, count);
            assert_eq!(vtt, "WEBVTT\n", "({interval},{columns},{count})");
        }
    }

    // --- timestamps -------------------------------------------------------

    #[test]
    fn format_timestamp_matches_the_webvtt_grammar() {
        assert_eq!(format_timestamp(0.0), "00:00:00.000");
        assert_eq!(format_timestamp(1.5), "00:00:01.500");
        assert_eq!(format_timestamp(59.999), "00:00:59.999");
        assert_eq!(format_timestamp(60.0), "00:01:00.000");
        assert_eq!(format_timestamp(3600.0), "01:00:00.000");
        assert_eq!(format_timestamp(3661.25), "01:01:01.250");
        assert_eq!(format_timestamp(7325.5), "02:02:05.500");
    }

    #[test]
    fn format_timestamp_zero_pads_seconds() {
        // "0:0:5.5" would be rejected by a strict WebVTT parser.
        assert_eq!(format_timestamp(5.5), "00:00:05.500");
        assert_eq!(format_timestamp(65.0), "00:01:05.000");
    }

    #[test]
    fn format_timestamp_clamps_nonsense_values() {
        assert_eq!(format_timestamp(-1.0), "00:00:00.000");
        assert_eq!(format_timestamp(f64::NAN), "00:00:00.000");
        assert_eq!(format_timestamp(f64::INFINITY), "00:00:00.000");
    }
}

//! Where things live in the bucket.
//!
//! The runner reads sources from one prefix and writes output under another.
//! Both are configuration rather than constants, because they are properties
//! of whoever owns the bucket, not of transcoding.
//!
//! The *shape* below the prefix is fixed:
//!
//! ```text
//!   {source_prefix}{id}/source.{ext}        the video to transcode
//!   {source_prefix}{id}/captions/{lang}.vtt optional supplied captions
//!   {output_prefix}{id}/…                   everything vidra produced
//! ```
//!
//! Fixed rather than a template, because a template language here would be a
//! lot of machinery for a layout nobody has yet needed to vary.

use anyhow::{bail, Result};

/// Extensions a source video may use.
const SUPPORTED_EXTENSIONS: &[&str] = &["mp4", "mov", "mkv", "webm", "m4v"];

/// A source key that identifies a video to transcode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceKey {
    pub video_id: String,
    /// Lowercase extension without the dot, e.g. "mp4".
    pub extension: String,
    /// The full storage key, as delivered.
    pub key: String,
}

/// Prefixes for the source and output trees.
#[derive(Debug, Clone)]
pub struct KeyLayout {
    source_prefix: String,
    output_prefix: String,
}

impl Default for KeyLayout {
    fn default() -> Self {
        Self {
            source_prefix: "source/".to_string(),
            output_prefix: "media/".to_string(),
        }
    }
}

impl KeyLayout {
    /// Build a layout, normalising both prefixes to end in `/`.
    pub fn new(source_prefix: &str, output_prefix: &str) -> Result<Self> {
        let source_prefix = normalise(source_prefix, "source prefix")?;
        let output_prefix = normalise(output_prefix, "output prefix")?;

        // Overlapping prefixes would let output be mistaken for a new source
        // and transcoded again, forever.
        if source_prefix.starts_with(&output_prefix) || output_prefix.starts_with(&source_prefix) {
            bail!(
                "the source prefix {source_prefix:?} and output prefix {output_prefix:?} \
                 must not nest; output would be re-ingested as new input"
            );
        }

        Ok(Self {
            source_prefix,
            output_prefix,
        })
    }

    pub fn source_prefix(&self) -> &str {
        &self.source_prefix
    }

    pub fn output_prefix(&self) -> &str {
        &self.output_prefix
    }

    /// Where this video's output goes, e.g. `media/clip/`.
    pub fn output_prefix_for(&self, video_id: &str) -> String {
        format!("{}{}/", self.output_prefix, video_id)
    }

    /// Where supplied captions live, e.g. `source/clip/captions/`.
    pub fn captions_prefix_for(&self, video_id: &str) -> String {
        format!("{}{}/captions/", self.source_prefix, video_id)
    }

    /// Identifiers that would collide with a prefix name.
    ///
    /// `source/source/source.mp4` parses to the id "source", whose output
    /// prefix is `media/source/` — still inside the output tree, so nothing
    /// leaks, but it makes the layout confusing for no benefit. Rejected so
    /// the guarantee holds by construction.
    fn is_reserved(&self, video_id: &str) -> bool {
        let trim = |p: &str| p.trim_end_matches('/').to_string();
        video_id == trim(&self.source_prefix) || video_id == trim(&self.output_prefix)
    }

    /// Match `{source_prefix}{id}/source.{ext}` and nothing else.
    ///
    /// Returns `None` for any other key, including keys that merely look
    /// close: a nested path, a different basename, or an unsupported
    /// extension. This is what keeps the sidecar files out — `meta.json` and
    /// `captions/*.vtt` are written *before* the video by design, so treating
    /// one as a job would start transcoding a file that is still uploading.
    pub fn parse_source_key(&self, key: &str) -> Option<SourceKey> {
        let rest = key.strip_prefix(&self.source_prefix)?;

        // Exactly two segments: "{id}" and "source.{ext}". The explicit check
        // on the remainder rejects deeper paths.
        let (video_id, filename) = rest.split_once('/')?;
        if filename.contains('/') {
            return None;
        }

        if !is_valid_video_id(video_id) || self.is_reserved(video_id) {
            return None;
        }

        let extension = filename.strip_prefix("source.")?;
        // "source.tar.gz" would leave "tar.gz" here.
        if extension.contains('.') || extension.is_empty() {
            return None;
        }

        let lowered = extension.to_ascii_lowercase();
        if !SUPPORTED_EXTENSIONS.contains(&lowered.as_str()) {
            return None;
        }

        Some(SourceKey {
            video_id: video_id.to_string(),
            extension: lowered,
            key: key.to_string(),
        })
    }
}

fn normalise(prefix: &str, what: &str) -> Result<String> {
    let trimmed = prefix.trim();
    if trimmed.is_empty() {
        bail!("the {what} must not be empty");
    }
    if trimmed.starts_with('/') {
        bail!("the {what} must not start with a slash, got {trimmed:?}");
    }
    if trimmed.contains("..") || trimmed.contains("//") {
        bail!("the {what} must not contain .. or //, got {trimmed:?}");
    }

    Ok(if trimmed.ends_with('/') {
        trimmed.to_string()
    } else {
        format!("{trimmed}/")
    })
}

/// The video ID convention: lowercase alphanumerics and hyphens, 3–64 chars,
/// not starting with a hyphen.
///
/// Deliberately the same rule the delivery layer enforces on the first path
/// segment. An ID that passes here but not there produces output nobody can
/// ever fetch, so the two must agree.
pub fn is_valid_video_id(id: &str) -> bool {
    if !(3..=64).contains(&id.len()) {
        return false;
    }

    let mut chars = id.chars();
    let first = chars.next().unwrap_or('-');
    if !(first.is_ascii_lowercase() || first.is_ascii_digit()) {
        return false;
    }

    id.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout() -> KeyLayout {
        KeyLayout::default()
    }

    // --- keys that are jobs -----------------------------------------------

    #[test]
    fn matches_every_supported_extension() {
        for extension in ["mp4", "mov", "mkv", "webm", "m4v"] {
            let key = format!("source/20260101120000-deadbeef/source.{extension}");
            let parsed = layout().parse_source_key(&key).expect("should match");

            assert_eq!(parsed.video_id, "20260101120000-deadbeef");
            assert_eq!(parsed.extension, extension);
            assert_eq!(parsed.key, key);
        }
    }

    #[test]
    fn lowercases_the_extension() {
        let parsed = layout()
            .parse_source_key("source/abc/source.MP4")
            .expect("should match");
        assert_eq!(parsed.extension, "mp4");
    }

    #[test]
    fn accepts_the_shortest_and_longest_valid_ids() {
        assert!(layout().parse_source_key("source/abc/source.mp4").is_some());

        let longest = "a".repeat(64);
        assert!(layout()
            .parse_source_key(&format!("source/{longest}/source.mp4"))
            .is_some());
    }

    // --- keys that are not jobs -------------------------------------------

    #[test]
    fn skips_the_sidecar_files_written_before_the_video() {
        // These arrive first, by design. Treating one as a job would start
        // transcoding a file that has not finished uploading.
        for key in [
            "source/abc/meta.json",
            "source/abc/captions/en.vtt",
            "source/abc/captions/pt-BR.vtt",
        ] {
            assert!(layout().parse_source_key(key).is_none(), "{key}");
        }
    }

    #[test]
    fn skips_keys_outside_the_source_prefix() {
        for key in [
            "media/abc/master.m3u8",
            "abc/source.mp4",
            "sources/abc/source.mp4",
            "",
            "source/",
        ] {
            assert!(layout().parse_source_key(key).is_none(), "{key:?}");
        }
    }

    #[test]
    fn skips_a_wrong_basename() {
        for key in [
            "source/abc/original.mp4",
            "source/abc/Source.mp4",
            "source/abc/source.mp4.part",
            "source/abc/source.",
            "source/abc/source",
        ] {
            assert!(layout().parse_source_key(key).is_none(), "{key}");
        }
    }

    #[test]
    fn skips_unsupported_extensions() {
        for extension in ["avi", "wmv", "flv", "txt", "json", "vtt", "exe"] {
            assert!(
                layout()
                    .parse_source_key(&format!("source/abc/source.{extension}"))
                    .is_none(),
                "{extension} should not be transcoded"
            );
        }
    }

    #[test]
    fn skips_nested_paths_and_traversal() {
        for key in [
            "source/abc/nested/source.mp4",
            "source/a/b/c/source.mp4",
            "source/../source.mp4",
            "source/abc/../../source.mp4",
        ] {
            assert!(layout().parse_source_key(key).is_none(), "{key}");
        }
    }

    #[test]
    fn skips_invalid_video_ids() {
        for id in [
            "ab",
            "-abc",
            "ABC",
            "abc_def",
            "abc.def",
            "abc def",
            &"a".repeat(65),
        ] {
            assert!(
                layout()
                    .parse_source_key(&format!("source/{id}/source.mp4"))
                    .is_none(),
                "id {id:?} should be rejected"
            );
        }
    }

    #[test]
    fn skips_ids_colliding_with_a_prefix_name() {
        assert!(layout()
            .parse_source_key("source/source/source.mp4")
            .is_none());
        assert!(layout()
            .parse_source_key("source/media/source.mp4")
            .is_none());
    }

    // --- prefixes ---------------------------------------------------------

    #[test]
    fn prefixes_gain_a_trailing_slash() {
        let layout = KeyLayout::new("incoming", "ready").expect("should build");

        assert_eq!(layout.source_prefix(), "incoming/");
        assert_eq!(layout.output_prefix(), "ready/");
        assert_eq!(layout.output_prefix_for("clip"), "ready/clip/");
        assert_eq!(
            layout.captions_prefix_for("clip"),
            "incoming/clip/captions/"
        );
    }

    #[test]
    fn a_custom_layout_parses_its_own_keys_and_not_the_default_ones() {
        let layout = KeyLayout::new("incoming/", "ready/").unwrap();

        assert!(layout
            .parse_source_key("incoming/clip/source.mp4")
            .is_some());
        assert!(layout.parse_source_key("source/clip/source.mp4").is_none());
    }

    #[test]
    fn nested_prefixes_are_rejected() {
        // Output under the source tree would be re-ingested as new input and
        // transcoded forever.
        assert!(KeyLayout::new("media/", "media/out/").is_err());
        assert!(KeyLayout::new("a/b/", "a/").is_err());
        assert!(KeyLayout::new("same/", "same/").is_err());
    }

    #[test]
    fn malformed_prefixes_are_rejected() {
        for (source, output) in [
            ("", "media/"),
            ("source/", ""),
            ("/source/", "media/"),
            ("../source/", "media/"),
            ("sou//rce/", "media/"),
        ] {
            assert!(
                KeyLayout::new(source, output).is_err(),
                "({source:?}, {output:?}) should be rejected"
            );
        }
    }

    #[test]
    fn a_custom_layout_reserves_its_own_prefix_names() {
        let layout = KeyLayout::new("incoming/", "ready/").unwrap();

        assert!(layout
            .parse_source_key("incoming/incoming/source.mp4")
            .is_none());
        assert!(layout
            .parse_source_key("incoming/ready/source.mp4")
            .is_none());
        // "source" is no longer special in this layout.
        assert!(layout
            .parse_source_key("incoming/source/source.mp4")
            .is_some());
    }
}

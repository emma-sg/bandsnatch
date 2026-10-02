//! Library folder layout.
//!
//! The layout is a template relative to the output folder. The default keeps
//! the release ID in the album folder name: Bandcamp sells releases by the same
//! artist with the same title and year, and without a distinguishing element
//! they collide into one folder.

use crate::util;
use std::error::Error;
use std::path::{Path, PathBuf};

/// Default folder layout: `<artist>/<album> (<year>) [<id>]`.
pub const DEFAULT_ALBUM_PATH: &str = "{artist}/{album} ({year}) [{id}]";

const PLACEHOLDERS: &[&str] = &["artist", "album", "year", "id"];

/// A validated album path template.
#[derive(Clone)]
pub struct AlbumPath {
    template: String,
}

impl Default for AlbumPath {
    fn default() -> Self {
        Self {
            template: DEFAULT_ALBUM_PATH.to_string(),
        }
    }
}

impl AlbumPath {
    /// Validate a user-supplied template.
    ///
    /// Unknown placeholders are rejected when the template is parsed, because a
    /// typo would otherwise be written literally into every folder name.
    pub fn new(template: &str) -> Result<Self, Box<dyn Error>> {
        let template = template.trim();
        if template.is_empty() {
            return Err("album path template must not be empty".into());
        }
        if template.starts_with('/') || template.starts_with('\\') {
            return Err("album path template must be relative to the output folder".into());
        }
        if template.split(['/', '\\']).any(|segment| segment == "..") {
            return Err("album path template must not contain `..`".into());
        }

        for name in brace_names(template) {
            if !PLACEHOLDERS.contains(&name.as_str()) {
                let valid = PLACEHOLDERS
                    .iter()
                    .map(|name| format!("{{{name}}}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(format!(
                    "unknown placeholder `{{{name}}}` in album path; valid placeholders are {valid}"
                )
                .into());
            }
        }

        Ok(Self {
            template: template.to_string(),
        })
    }

    /// Render the full destination folder for a release.
    pub fn render(
        &self,
        root: &Path,
        artist: &str,
        album: &str,
        year: Option<&str>,
        id: &str,
    ) -> PathBuf {
        let mut rendered = self.template.clone();
        for (name, value) in [
            ("artist", artist),
            ("album", album),
            // A missing year renders as nothing at all, so that a release with
            // no reported date becomes `Album` rather than `Album (0000)`.
            ("year", year.unwrap_or("")),
            ("id", id),
        ] {
            // Values are sanitised *before* substitution. Sanitising afterwards
            // would not work: the template is split on `/` to build the path, so
            // a separator inside a value (an album title containing a slash)
            // would already have become an extra directory level by then, and a
            // crafted title could escape the output folder entirely.
            rendered = rendered.replace(&format!("{{{name}}}"), &util::make_string_fs_safe(value));
        }
        let rendered = collapse_empty_groups(&rendered);

        let mut path = root.to_path_buf();
        for segment in rendered.split(['/', '\\']) {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }
            // Catches separators and illegal characters in the template text
            // itself, which the substitution above does not see.
            path.push(util::make_string_fs_safe(segment));
        }
        path
    }
}

/// The names appearing inside `{...}` in a template.
fn brace_names(template: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('}') else {
            break;
        };
        names.push(after[..close].to_string());
        rest = &after[close + 1..];
    }
    names
}

/// Drop bracket groups that a missing placeholder left empty, then tidy the
/// whitespace they leave behind.
///
/// Without this, `{album} ({year}) [{id}]` with no year would render as
/// `Album () [p1]` instead of `Album [p1]`.
fn collapse_empty_groups(input: &str) -> String {
    let mut collapsed = input.to_string();
    loop {
        let previous = collapsed.clone();
        collapsed = collapsed.replace("()", "").replace("[]", "").replace("{}", "");
        if collapsed == previous {
            break;
        }
    }

    // Squeeze runs of spaces, then trim each path segment.
    let mut squeezed = String::with_capacity(collapsed.len());
    let mut last_was_space = false;
    for ch in collapsed.chars() {
        if ch == ' ' {
            if !last_was_space {
                squeezed.push(ch);
            }
            last_was_space = true;
        } else {
            last_was_space = false;
            squeezed.push(ch);
        }
    }

    squeezed
        .split('/')
        .map(|segment| segment.trim())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        PathBuf::from("/music")
    }

    #[test]
    fn default_layout_matches_the_documented_shape() {
        let layout = AlbumPath::default();
        assert_eq!(
            layout.render(&root(), "Some Artist", "Some Album", Some("2020"), "p1234"),
            PathBuf::from("/music/Some Artist/Some Album (2020) [p1234]")
        );
    }

    #[test]
    fn a_missing_year_leaves_no_empty_brackets() {
        // The bug this replaced produced `Some Album (0000) [p1234]`, which media
        // servers read as year zero.
        let layout = AlbumPath::default();
        assert_eq!(
            layout.render(&root(), "Some Artist", "Some Album", None, "p1234"),
            PathBuf::from("/music/Some Artist/Some Album [p1234]")
        );
    }

    #[test]
    fn a_plex_style_template_drops_the_identifier() {
        let layout = AlbumPath::new("{artist}/{album} ({year})").unwrap();
        assert_eq!(
            layout.render(&root(), "Some Artist", "Some Album", Some("2020"), "p1234"),
            PathBuf::from("/music/Some Artist/Some Album (2020)")
        );
        // Still correct with no year.
        assert_eq!(
            layout.render(&root(), "Some Artist", "Some Album", None, "p1234"),
            PathBuf::from("/music/Some Artist/Some Album")
        );
    }

    #[test]
    fn equally_named_releases_keep_their_files_separate() {
        // Bandcamp sells releases that share artist, title and year. The default
        // template must keep them in distinct folders, which is why `{id}` is in
        // it by default.
        let layout = AlbumPath::default();
        let first = layout.render(&root(), "Same artist", "Same album", Some("2024"), "p42");
        let second = layout.render(&root(), "Same artist", "Same album", Some("2024"), "p43");
        assert_ne!(first, second);
        assert_eq!(
            first,
            layout.render(&root(), "Same artist", "Same album", Some("2024"), "p42")
        );
    }

    #[test]
    fn templates_are_sanitised_per_segment_not_per_path() {
        let layout = AlbumPath::new("{artist}/{album}").unwrap();
        // A separator inside a *value* must not become an extra directory.
        assert_eq!(
            layout.render(&root(), "Artist", "Album / Deluxe", None, "p1"),
            PathBuf::from("/music/Artist/Album ／ Deluxe")
        );

        // ...and must not let a release title escape the output folder.
        let escaped = layout.render(&root(), "Artist", "../../etc", None, "p1");
        assert_eq!(escaped, PathBuf::from("/music/Artist/..／..／etc"));
        assert_eq!(escaped.components().count(), 4);
    }

    #[test]
    fn invalid_templates_are_rejected_before_any_download() {
        assert!(AlbumPath::new("").is_err());
        assert!(AlbumPath::new("/absolute/{album}").is_err());
        assert!(AlbumPath::new("{artist}/../{album}").is_err());
        assert!(AlbumPath::new("{artist}/{arists}").is_err());
        assert!(AlbumPath::new("{artist}/{album}").is_ok());
    }
}

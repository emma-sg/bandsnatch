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

    /// Render the destination folder for a release.
    ///
    /// Returns an error rather than a path when the template and values produce
    /// no folder below `root`. Replacement renames its target aside and deletes
    /// it, so a template that collapsed to `root` would delete the entire
    /// library. A release whose artist and album both render empty (a missing
    /// or bracket-only title, or a coarse template such as `{artist}`) reaches
    /// it, so the guard lives here rather than at the call sites.
    ///
    /// The rendered path is always a strict descendant of `root`.
    pub fn render(
        &self,
        root: &Path,
        artist: &str,
        album: &str,
        year: Option<&str>,
        id: &str,
    ) -> Result<PathBuf, Box<dyn Error>> {
        let mut rendered = self.template.clone();
        for (name, value) in [
            ("artist", artist),
            ("album", album),
            // A missing year renders as an empty string, so a release with no
            // reported date becomes `Album`, not `Album (0000)`.
            ("year", year.unwrap_or("")),
            ("id", id),
        ] {
            // Values are sanitised before substitution. Sanitising afterwards
            // would not work: the rendered template is split on `/` to build the
            // path, so a separator inside a value (an album title containing a
            // slash) would already have become an extra directory level, and a
            // crafted title could escape the output folder.
            rendered = rendered.replace(&format!("{{{name}}}"), &util::make_string_fs_safe(value));
        }
        let rendered = collapse_empty_groups(&rendered);

        let mut path = root.to_path_buf();
        let mut segments = 0usize;
        for segment in rendered.split(['/', '\\']) {
            let segment = segment.trim();
            if segment.is_empty() {
                continue;
            }
            // `new` rejects `.` or `..` in template text and the sanitiser
            // neutralises both in values, but this is the last point before a
            // path reaches the filesystem and the check is cheap.
            if segment == "." || segment == ".." {
                return Err(format!(
                    "album path template `{}` produced an unsafe path component `{segment}`",
                    self.template
                )
                .into());
            }
            // Catches separators and illegal characters in the template text
            // itself, which the substitution above does not see.
            path.push(util::make_string_fs_safe(segment));
            segments += 1;
        }

        if segments == 0 {
            return Err(format!(
                "album path template `{}` produced no folder below the output folder for artist {artist:?} / album {album:?}; \
                 check --album-path, and note that a placeholder which renders empty is dropped along with any brackets it leaves empty",
                self.template
            )
            .into());
        }

        Ok(path)
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
        collapsed = collapsed
            .replace("()", "")
            .replace("[]", "")
            .replace("{}", "");
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
    fn render_must_never_return_the_root_or_a_path_outside_it() {
        // Values that collapse to nothing must not make render return the output
        // root: replacement would then rename the whole library aside and delete
        // it recursively. Every render must be an error or a folder strictly
        // below the root.
        let layout = AlbumPath::new("{artist}/{album} ({year})").unwrap();
        for (artist, album) in [
            ("", ""),
            ("()", ""),
            ("", "[]"),
            ("{}", "{}"),
            ("()", "()"),
        ] {
            let rendered = layout.render(&root(), artist, album, None, "p1");
            assert!(
                rendered.is_err(),
                "artist={artist:?} album={album:?} rendered {rendered:?} instead of erroring"
            );
        }

        // A coarse template with an empty value is the same hazard.
        let coarse = AlbumPath::new("{artist}").unwrap();
        assert!(coarse.render(&root(), "", "Some Album", None, "p1").is_err());

        // Any successful render is a strict descendant of the root.
        let rendered = layout
            .render(&root(), "Some Artist", "Some Album", Some("2020"), "p1")
            .unwrap();
        assert!(rendered.starts_with(root()));
        assert_ne!(rendered, root());
    }

    #[test]
    fn default_layout_matches_the_documented_shape() {
        let layout = AlbumPath::default();
        assert_eq!(
            layout
                .render(&root(), "Some Artist", "Some Album", Some("2020"), "p1234")
                .unwrap(),
            PathBuf::from("/music/Some Artist/Some Album (2020) [p1234]")
        );
    }

    #[test]
    fn a_missing_year_leaves_no_empty_brackets() {
        // A missing year must not render as `0000`: media servers read that as
        // year zero.
        let layout = AlbumPath::default();
        assert_eq!(
            layout
                .render(&root(), "Some Artist", "Some Album", None, "p1234")
                .unwrap(),
            PathBuf::from("/music/Some Artist/Some Album [p1234]")
        );
    }

    #[test]
    fn equally_named_releases_keep_their_files_separate() {
        // Bandcamp sells releases that share artist, title and year, so the
        // default template must keep them in distinct folders; `{id}` is what
        // does it.
        let layout = AlbumPath::default();
        let first = layout
            .render(&root(), "Same artist", "Same album", Some("2024"), "p42")
            .unwrap();
        let second = layout
            .render(&root(), "Same artist", "Same album", Some("2024"), "p43")
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(
            first,
            layout
                .render(&root(), "Same artist", "Same album", Some("2024"), "p42")
                .unwrap()
        );
    }

    #[test]
    fn templates_are_sanitised_per_segment_not_per_path() {
        let layout = AlbumPath::new("{artist}/{album}").unwrap();
        // A separator inside a *value* must not become an extra directory.
        assert_eq!(
            layout
                .render(&root(), "Artist", "Album / Deluxe", None, "p1")
                .unwrap(),
            PathBuf::from("/music/Artist/Album ／ Deluxe")
        );

        // ...and must not let a release title escape the output folder.
        let escaped = layout
            .render(&root(), "Artist", "../../etc", None, "p1")
            .unwrap();
        assert_eq!(escaped, PathBuf::from("/music/Artist/..／..／etc"));
        assert_eq!(escaped.components().count(), 4);

        // A bare `..` value is neutralised to a name that cannot traverse.
        let dotdot = layout.render(&root(), "Artist", "..", None, "p1").unwrap();
        assert_eq!(dotdot, PathBuf::from("/music/Artist/.._"));
    }

    #[test]
    fn invalid_templates_are_rejected_at_parse_time() {
        assert!(AlbumPath::new("").is_err());
        assert!(AlbumPath::new("/absolute/{album}").is_err());
        assert!(AlbumPath::new("{artist}/../{album}").is_err());
        assert!(AlbumPath::new("{artist}/{arists}").is_err());
        assert!(AlbumPath::new("{artist}/{album}").is_ok());
    }
}

use phf::phf_map;
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

// From https://github.com/Ezwen/bandcamp-collection-downloader/blob/master/src/main/kotlin/bandcampcollectiondownloader/core/Constants.kt#L7
static REPLACEMENT_CHARS: phf::Map<&str, &str> = phf_map! {
    ":" => "꞉",
    "/" => "／",
    "\\" => "⧹",
    "\"" => "＂",
    "*" => "⋆",
    "<" => "＜",
    ">" => "＞",
    "?" => "？",
    "|" => "∣"
};

// NTFS doesn't like these and pretty much shits itself if you try to do
// anything to files/folders containing em.
static UNSAFE_NTFS_ENDINGS: &[char] = &['.', ' '];

pub fn make_string_fs_safe(s: &str) -> String {
    let mut str = s.to_string();

    for (from, to) in REPLACEMENT_CHARS.entries() {
        str = str.replace(from, to);
    }

    // Callers pass empty strings: a release with no reported year renders an
    // empty `{year}` value. An empty name has no trailing character
    // to inspect, so the check must handle it rather than unwrap.
    if str
        .chars()
        .last()
        .is_some_and(|last| UNSAFE_NTFS_ENDINGS.contains(&last))
    {
        str.push('_');
    }

    str
}

/// Remove characters that could control a terminal or fake a log line.
///
/// Release titles, artist names and track names come from Bandcamp metadata,
/// which an artist controls. Printed raw, an escape sequence can clear the
/// screen, set the window title, or set the operator's clipboard via OSC 52,
/// and a newline can add a log line that looks like a success. Bidi overrides
/// are removed because they reorder displayed text, so a name can read as
/// something it is not.
///
/// This is for text bound for a human or a log. It is not applied to filenames:
/// those go through `make_string_fs_safe`, and rewriting a name on disk would
/// orphan folders.
pub fn display_safe(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control()
                && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

// Thanks to https://gist.github.com/NoraCodes/e6d40782b05dc8ac40faf3a0405debd3
const DEFAULT_BUF_SIZE: usize = 8192;

// `std::io::copy` slightly modified to update a progress bar as it copies
// https://doc.rust-lang.org/1.8.0/src/std/up/src/libstd/io/util.rs.html#46-61
pub fn copy_with_progress<R: ?Sized, W: ?Sized>(
    reader: &mut R,
    writer: &mut W,
    pb: &indicatif::ProgressBar,
) -> io::Result<u64>
where
    R: Read,
    W: Write,
{
    let mut buf = [0; DEFAULT_BUF_SIZE];
    let mut written = 0;
    loop {
        let len = match reader.read(&mut buf) {
            Ok(0) => return Ok(written),
            Ok(len) => len,
            Err(ref e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        writer.write_all(&buf[..len])?;
        written += len as u64;
        pb.set_position(written);
    }
}

/// Atomically replace `target` with the fully prepared `staging` directory.
///
/// The two must share a parent so the rename stays within one filesystem. If the
/// swap fails, any pre-existing `target` is put back, so a failed re-download
/// cannot leave the library missing a release it already had.
///
/// `token` must be the one the caller staged `staging` under, so two concurrent
/// swaps of the same target cannot remove the copy the other swap moved aside.
pub fn replace_directory(target: &Path, staging: &Path, token: &str) -> io::Result<()> {
    let previous = previous_dir(target, token)?;
    if previous.exists() {
        fs::remove_dir_all(&previous)?;
    }

    let had_previous = target.exists();
    if had_previous {
        fs::rename(target, &previous)?;
    }

    if let Err(err) = fs::rename(staging, target) {
        if had_previous {
            // Best effort: the original rename failure is the more useful error.
            let _ = fs::rename(&previous, target);
        }
        return Err(err);
    }

    if had_previous {
        fs::remove_dir_all(&previous)?;
    }
    Ok(())
}

/// The hidden sibling directory a download is unpacked into before being swapped
/// into place.
///
/// `token` - normally the release id - makes the name unique per release. Two
/// purchases can resolve to one output path when `--album-path` omits `{id}`,
/// and a shared staging name would let one worker delete another's half-written
/// download mid-transfer.
pub fn staging_dir(target: &Path, token: &str) -> io::Result<PathBuf> {
    hidden_sibling(target, token, "staging")
}

/// The hidden sibling directory the old copy is moved into while the new
/// download is put in place.
pub fn previous_dir(target: &Path, token: &str) -> io::Result<PathBuf> {
    hidden_sibling(target, token, "previous")
}

/// Built in one place so the staging and previous names always match.
///
/// The token is made filesystem-safe, so a caller cannot put a path separator
/// in it and escape the album's parent directory.
fn hidden_sibling(target: &Path, token: &str, suffix: &str) -> io::Result<PathBuf> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("target path has no parent directory"))?;
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::other("target path has no final component"))?
        .to_string_lossy();

    Ok(parent.join(format!(
        ".{name}.{}.bandsnatch-{suffix}",
        make_string_fs_safe(token)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bandsnatch-util-{label}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn display_safe_strips_terminal_and_log_control_characters() {
        // An artist controls these strings. Printed raw, ESC starts an escape
        // sequence (here OSC 52, which sets the clipboard in many terminals) and
        // a newline can add a fake log line.
        assert_eq!(
            display_safe("Album\x1b]52;c;cGF3bmVk\x07"),
            "Album]52;c;cGF3bmVk"
        );
        assert_eq!(
            display_safe("Album\nINFO bandsnatch: Imported 99 entries"),
            "AlbumINFO bandsnatch: Imported 99 entries"
        );
        assert_eq!(display_safe("Album\tName"), "AlbumName");
        // Bidi overrides reorder displayed text, so a name can read as another.
        assert_eq!(display_safe("Album\u{202e}gnp.txt"), "Albumgnp.txt");
        assert_eq!(display_safe("Album\u{2066}x\u{2069}"), "Albumx");
        // Ordinary text is untouched, including characters outside ASCII.
        assert_eq!(display_safe("Björk – Utopía (2022)"), "Björk – Utopía (2022)");
    }

    #[test]
    fn make_string_fs_safe_tolerates_empty_input() {
        // An empty string has no trailing character to test.
        assert_eq!(make_string_fs_safe(""), "");
    }

    #[test]
    fn make_string_fs_safe_appends_an_underscore_for_ntfs_unsafe_endings() {
        assert_eq!(make_string_fs_safe("Album."), "Album._");
        assert_eq!(make_string_fs_safe("Album "), "Album _");
        assert_eq!(make_string_fs_safe("Album"), "Album");
    }

    #[test]
    fn make_string_fs_safe_replaces_path_separators() {
        assert_eq!(make_string_fs_safe("AC/DC"), "AC／DC");
    }

    #[test]
    fn replace_directory_swaps_in_new_content_and_leaves_no_debris() {
        let root = temp_dir("swap");
        let target = root.join("Album");
        let staging = staging_dir(&target, "p1").unwrap();

        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old.flac"), b"old").unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("new.flac"), b"new").unwrap();

        replace_directory(&target, &staging, "p1").unwrap();

        assert_eq!(fs::read(target.join("new.flac")).unwrap(), b"new");
        assert!(!target.join("old.flac").exists());
        assert!(!staging.exists());
        assert!(!previous_dir(&target, "p1").unwrap().exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn replace_directory_creates_the_target_when_there_was_nothing_before() {
        let root = temp_dir("fresh");
        let target = root.join("Album");
        let staging = staging_dir(&target, "p1").unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("new.flac"), b"new").unwrap();

        replace_directory(&target, &staging, "p1").unwrap();

        assert_eq!(fs::read(target.join("new.flac")).unwrap(), b"new");
        assert!(!staging.exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_swap_leaves_the_existing_release_intact() {
        // Downloads are staged so that a re-download that fails does not cost
        // the user the copy they already had.
        let root = temp_dir("failed");
        let target = root.join("Album");
        let staging = staging_dir(&target, "p1").unwrap();

        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old.flac"), b"old").unwrap();
        // `staging` is not created, so the final rename fails.

        assert!(replace_directory(&target, &staging, "p1").is_err());

        assert_eq!(fs::read(target.join("old.flac")).unwrap(), b"old");
        assert!(!previous_dir(&target, "p1").unwrap().exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn staging_names_are_unique_per_release_and_cannot_escape_the_parent() {
        // Two releases resolve to one output path when `--album-path` omits
        // `{id}`. A shared staging directory would let one worker delete
        // another's half-written download, and a shared previous directory would
        // destroy a release mid-swap.
        let target = PathBuf::from("/music/Some Artist/Some Album");
        assert_ne!(
            staging_dir(&target, "p1").unwrap(),
            staging_dir(&target, "p2").unwrap()
        );
        assert_ne!(
            previous_dir(&target, "p1").unwrap(),
            previous_dir(&target, "p2").unwrap()
        );
        assert_ne!(
            staging_dir(&target, "p1").unwrap(),
            previous_dir(&target, "p1").unwrap()
        );

        // The token is caller-supplied, so it must not be able to walk out of
        // the album's parent directory.
        let escaped = staging_dir(&target, "../../evil").unwrap();
        assert_eq!(escaped.parent().unwrap(), target.parent().unwrap());
        assert_eq!(escaped.components().count(), 4);
    }
}

use phf::phf_map;
use std::{
    collections::VecDeque,
    fs,
    io::{self, Read, Write},
    path::Path,
    sync::{Arc, Mutex},
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

pub fn slice_string(s: &str, amt: usize) -> &str {
    match s.char_indices().nth(amt) {
        Some((pos, _)) => &s[pos..],
        None => "",
    }
}

// Thanks to https://gist.github.com/NoraCodes/e6d40782b05dc8ac40faf3a0405debd3
#[derive(Clone)]
pub struct WorkQueue<T> {
    inner: Arc<Mutex<VecDeque<T>>>,
}

impl<T> WorkQueue<T> {
    // pub fn new() -> Self {
    //     Self {
    //         inner: Arc::new(Mutex::new(VecDeque::new())),
    //     }
    // }

    pub fn from_vec(vec: Vec<T>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(VecDeque::from(vec))),
        }
    }

    pub fn get_work(&self) -> Option<T> {
        // Try to get a lock on the Mutex. If this fails, there is a
        // problem with the mutex - it's poisoned, meaning that a thread that
        // held the mutex lock panicked before releasing it. There is no way
        // to guarantee that all its invariants are upheld, so we need to not
        // use it in that case.
        let maybe_queue = self.inner.lock();
        // A lot is going on here. self.inner is an Arc of Mutex. Arc can deref
        // into its internal type, so we can call the methods of that inner
        // type (Mutex) without dereferencing, so this is like
        //      *(self.inner).lock()
        // but doesn't look awful. Mutex::lock() returns a
        // Result<MutexGuard<VecDeque<T>>>.

        // Unwrapping with if let, we get a MutexGuard, which is an RAII guard
        // that unlocks the Mutex when it goes out of scope.
        if let Ok(mut queue) = maybe_queue {
            // queue is a MutexGuard<VecDeque>, so this is like
            //      (*queue).pop_front()
            // Returns Some(item) or None if there are no more items.
            queue.pop_front()

            // The function has returned, so queue goes out of scope and the
            // mutex unlocks.
        } else {
            // There's a problem with the mutex.
            panic!("WorkQueue::get_work() tried to lock a poisoned mutex");
        }
    }
}

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
/// swap fails, any pre-existing `target` is put back: a failed re-download must
/// never leave the library missing a release it previously had.
pub fn replace_directory(target: &Path, staging: &Path) -> io::Result<()> {
    let parent = target
        .parent()
        .ok_or_else(|| io::Error::other("target path has no parent directory"))?;
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::other("target path has no final component"))?
        .to_string_lossy();

    let previous = parent.join(format!(".{name}.bandsnatch-previous"));
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
    fn make_string_fs_safe_tolerates_empty_input() {
        // Used to panic: there is no trailing character to test.
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
        let staging = root.join(".Album.bandsnatch-staging");

        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old.flac"), b"old").unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("new.flac"), b"new").unwrap();

        replace_directory(&target, &staging).unwrap();

        assert_eq!(fs::read(target.join("new.flac")).unwrap(), b"new");
        assert!(!target.join("old.flac").exists());
        assert!(!staging.exists());
        assert!(!root.join(".Album.bandsnatch-previous").exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn replace_directory_creates_the_target_when_there_was_nothing_before() {
        let root = temp_dir("fresh");
        let target = root.join("Album");
        let staging = root.join(".Album.bandsnatch-staging");
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("new.flac"), b"new").unwrap();

        replace_directory(&target, &staging).unwrap();

        assert_eq!(fs::read(target.join("new.flac")).unwrap(), b"new");
        assert!(!staging.exists());

        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn a_failed_swap_leaves_the_existing_release_intact() {
        // This is the entire reason downloads are staged: a re-download that
        // fails must not cost the user the copy they already had.
        let root = temp_dir("failed");
        let target = root.join("Album");
        let staging = root.join(".Album.bandsnatch-staging");

        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("old.flac"), b"old").unwrap();
        // Deliberately do not create `staging`, so the final rename fails.

        assert!(replace_directory(&target, &staging).is_err());

        assert_eq!(fs::read(target.join("old.flac")).unwrap(), b"old");
        assert!(!root.join(".Album.bandsnatch-previous").exists());

        fs::remove_dir_all(&root).unwrap();
    }
}

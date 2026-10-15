//! Persistent per-release state.
//!
//! The legacy append-only `bandcamp-collection-downloader.cache` text file
//! kept bookkeeping inside a free-text description field shared with
//! user-controlled titles and artists, so structured metadata (a content
//! fingerprint, a check timestamp) could not be recovered from it without
//! separating tool data from arbitrary text. This store uses typed columns,
//! atomic per-release updates, and allows concurrent access from the download
//! workers.

use crate::util;

use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use std::error::Error;
use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

/// Default state database name, created inside the output folder.
pub const STATE_FILENAME: &str = ".bandsnatch-state.db";

/// Cache file written by bandsnatch <= 0.3 and by Ezwen's
/// `bandcamp-collection-downloader`; imported once.
pub const LEGACY_CACHE_FILENAME: &str = "bandcamp-collection-downloader.cache";

/// Marker the legacy format uses to record a release that was still a preorder
/// when it was last seen.
const LEGACY_PREORDER_MARKER: &str = "@bandsnatch:preorder";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS items (
    id             TEXT PRIMARY KEY NOT NULL,
    artist         TEXT,
    title          TEXT,
    release_year   TEXT,
    format         TEXT,
    state          TEXT NOT NULL,
    size_mb        TEXT,
    content_length INTEGER,
    tracks         INTEGER,
    description    TEXT NOT NULL DEFAULT '',
    downloaded_at  TEXT,
    checked_at     TEXT
);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);
";

/// Columns added after the first release, with the statement that adds each.
///
/// `CREATE TABLE IF NOT EXISTS` leaves an existing table alone, so a database
/// written by an earlier version needs its missing columns added separately.
const ADDED_COLUMNS: &[(&str, &str)] = &[("tracks", "ALTER TABLE items ADD COLUMN tracks INTEGER")];

/// Add any column an older database is missing.
fn migrate(conn: &Connection) -> Result<(), Box<dyn Error>> {
    let mut statement = conn.prepare("PRAGMA table_info(items)")?;
    let existing = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<String>, rusqlite::Error>>()?;
    drop(statement);

    for (column, addition) in ADDED_COLUMNS {
        if !existing.iter().any(|name| name == column) {
            conn.execute(addition, [])?;
        }
    }
    Ok(())
}

/// What is known about a purchase.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ItemState {
    /// Downloaded successfully.
    Complete,
    /// Seen while Bandcamp still reported it as a preorder. Downloaded content
    /// may be partial, so this is retried until Bandcamp stops saying preorder.
    Preorder,
    /// Bandcamp returned no item or no download for this purchase. Not retried,
    /// because retrying cannot succeed; a recheck sweep still revisits it.
    Skipped,
}

impl ItemState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Preorder => "preorder",
            Self::Skipped => "skipped",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "complete" => Some(Self::Complete),
            "preorder" => Some(Self::Preorder),
            "skipped" => Some(Self::Skipped),
            _ => None,
        }
    }

    /// Whether a stored entry should be downloaded again.
    ///
    /// Only preorders are retried, and only once Bandcamp stops reporting the
    /// release as a preorder: at that point the full album is available and the
    /// stored download may be partial.
    pub fn needs_download(self, is_preorder: bool) -> bool {
        matches!(self, Self::Preorder) && !is_preorder
    }
}

/// A row of the `items` table.
#[derive(Clone, Debug)]
pub struct StateEntry {
    pub id: String,
    pub artist: Option<String>,
    pub title: Option<String>,
    pub release_year: Option<String>,
    pub format: Option<String>,
    pub state: ItemState,
    /// Size Bandcamp advertised for the archive at download time. It comes
    /// from the download page, so comparing it costs one request and no
    /// transfer.
    pub size_mb: Option<String>,
    /// Exact byte count of the archive transferred. Recorded for diagnostics;
    /// `size_mb` drives change detection because it can be read without
    /// downloading.
    pub content_length: Option<i64>,
    /// Tracks Bandcamp had audio for when this release was last downloaded, as
    /// the collection listing reported it. An artist adding a track raises it,
    /// and the listing a run already fetches carries the current count, so
    /// noticing costs no request.
    pub tracks: Option<u32>,
    pub description: String,
    pub downloaded_at: Option<String>,
    pub checked_at: Option<String>,
}

impl StateEntry {
    /// A freshly downloaded release, with the fingerprint Bandcamp advertised
    /// at download time.
    #[allow(clippy::too_many_arguments)]
    pub fn downloaded(
        id: &str,
        artist: &str,
        title: &str,
        release_year: Option<&str>,
        format: &str,
        size_mb: Option<String>,
        content_length: Option<u64>,
        tracks: Option<u32>,
        is_preorder: bool,
        now: DateTime<Utc>,
    ) -> Self {
        // The structured columns hold the metadata as reported; the description
        // is the human-readable field, so it is the one that gets
        // display-sanitised.
        let display_title = util::display_safe(title);
        let display_artist = util::display_safe(artist);

        Self {
            id: id.to_string(),
            artist: Some(artist.to_string()),
            title: Some(title.to_string()),
            release_year: release_year.map(str::to_string),
            format: Some(format.to_string()),
            state: if is_preorder {
                ItemState::Preorder
            } else {
                ItemState::Complete
            },
            size_mb,
            content_length: content_length.map(|len| len as i64),
            tracks,
            description: match release_year {
                Some(year) => format!("{display_title} ({year}) by {display_artist}"),
                None => format!("{display_title} by {display_artist}"),
            },
            downloaded_at: Some(now.to_rfc3339()),
            checked_at: Some(now.to_rfc3339()),
        }
    }

    /// A release Bandcamp would not serve. Kept out of the "downloaded" set so
    /// a later recheck sweep can revisit it, but not retried every run.
    pub fn unavailable(id: &str, description: &str, is_preorder: bool, now: DateTime<Utc>) -> Self {
        Self {
            id: id.to_string(),
            artist: None,
            title: None,
            release_year: None,
            format: None,
            state: if is_preorder {
                ItemState::Preorder
            } else {
                ItemState::Skipped
            },
            size_mb: None,
            content_length: None,
            tracks: None,
            description: description.to_string(),
            downloaded_at: None,
            checked_at: Some(now.to_rfc3339()),
        }
    }

    /// When this release was last looked at and found unchanged.
    ///
    /// `None` covers both a row that was never compared and one whose recorded
    /// timestamp cannot be read. Callers treat either as due, so a damaged value
    /// costs one comparison rather than hiding a release for good.
    pub fn last_checked(&self) -> Option<DateTime<Utc>> {
        let raw = self.checked_at.as_deref()?;
        DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|parsed| parsed.with_timezone(&Utc))
    }
}

/// What to do with a release during a run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Already downloaded and nothing suggests it changed.
    Skip,
    /// Download it: new purchase, forced, or a preorder that has been released.
    Download,
    /// Already downloaded, but due for a content fingerprint check. This costs
    /// one download-page request and only transfers bytes if the advertised
    /// size differs from the recorded one.
    Recheck,
}

/// How aggressively to look for in-place updates to already-downloaded
/// releases. Bandcamp does not renumber a purchase when an artist replaces the
/// audio files, so change detection has to be polled.
#[derive(Clone, Copy, Debug, Default)]
pub struct RecheckPolicy {
    /// Download every release again regardless of state, like `--force`.
    pub force: bool,
    /// Re-check a downloaded release once this many days have passed since it
    /// was last checked.
    pub after_days: Option<i64>,
    /// Compare a release that Bandcamp still lists as a preorder once this many
    /// minutes have passed since its last comparison.
    ///
    /// Preorders gain tracks while the purchase itself stays the same, so they
    /// are compared on their own interval. Runs can be far more frequent than
    /// comparisons should be: a run every 15 minutes would otherwise mean 96
    /// requests a day for each preorder. `None` turns preorder comparisons off,
    /// leaving them to `after_days`.
    pub preorder_after_minutes: Option<i64>,
}

impl RecheckPolicy {
    pub fn decide(
        &self,
        entry: Option<&StateEntry>,
        is_preorder: bool,
        now: DateTime<Utc>,
    ) -> Action {
        if self.force {
            return Action::Download;
        }
        let Some(entry) = entry else {
            return Action::Download;
        };
        if entry.state.needs_download(is_preorder) {
            return Action::Download;
        }
        if self.is_preorder_due(entry, is_preorder, now) || self.is_due(entry, now) {
            return Action::Recheck;
        }
        Action::Skip
    }

    /// Whether a release Bandcamp still lists as a preorder is due for another
    /// comparison.
    ///
    /// Public so a caller can tell a preorder comparison apart from one the
    /// recheck window asked for, which is what decides whether it counts against
    /// a run's budget for them.
    pub fn is_preorder_due(
        &self,
        entry: &StateEntry,
        is_preorder: bool,
        now: DateTime<Utc>,
    ) -> bool {
        if entry.state != ItemState::Preorder || !is_preorder {
            return false;
        }
        let Some(minutes) = self.preorder_after_minutes else {
            return false;
        };
        is_due_once(entry.last_checked(), now, Duration::minutes(minutes))
    }

    fn is_due(&self, entry: &StateEntry, now: DateTime<Utc>) -> bool {
        match self.after_days {
            Some(days) => is_due_once(entry.last_checked(), now, Duration::days(days)),
            None => false,
        }
    }
}

/// Whether enough time has passed since `last_checked` for another comparison.
///
/// A release that was never compared - one imported from the legacy cache, or
/// one recorded before fingerprints existed - is due now.
fn is_due_once(
    last_checked: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    interval: Duration,
) -> bool {
    match last_checked {
        Some(checked) => now.signed_duration_since(checked) >= interval,
        None => true,
    }
}

/// True when the size Bandcamp advertises now differs from the size recorded at
/// download time.
///
/// A missing size on either side means "cannot tell" and is reported as
/// unchanged: treating it as a change would re-download every release whose
/// page omits a size on every sweep.
pub fn fingerprint_changed(entry: &StateEntry, advertised_size_mb: Option<&str>) -> bool {
    match (entry.size_mb.as_deref(), advertised_size_mb) {
        (Some(known), Some(current)) => known != current,
        _ => false,
    }
}

/// True when the collection listing reports a different number of tracks from
/// the one recorded when this release was downloaded.
///
/// A count missing on either side means "cannot tell", for the same reason as
/// the size comparison: a database written before this column existed must not
/// look like a change, or the first run after an upgrade would re-download the
/// whole library.
pub fn track_count_changed(entry: &StateEntry, listed: Option<u32>) -> bool {
    match (entry.tracks, listed) {
        (Some(known), Some(current)) => known != current,
        _ => false,
    }
}

/// The SQLite state store.
///
/// Access is serialised internally, so one `State` can be shared across the
/// download workers as an `Arc<State>`; callers do not need to know that
/// `rusqlite::Connection` is `Send` but not `Sync`.
///
/// Convention for callers: state operations are advisory. Log a failed write and
/// carry on with the download, because the library on disk is the source of
/// truth, and a missing or stale row costs at worst a redundant download.
pub struct State {
    conn: Mutex<Connection>,
}

impl State {
    pub fn open(path: &Path) -> Result<Self, Box<dyn Error>> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        // WAL plus a busy timeout keep a second invocation safe: readers never
        // block writers, and a writer queues instead of failing immediately
        // with SQLITE_BUSY.
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 10000;",
        )?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> Result<MutexGuard<'_, Connection>, Box<dyn Error>> {
        self.conn
            .lock()
            .map_err(|_| "the state database lock is poisoned; a worker thread panicked".into())
    }

    /// Read a `meta` value using an already-held connection.
    ///
    /// Associated rather than a method so a caller can hold the lock across
    /// several statements: the mutex is not reentrant, so a method that locked
    /// again would deadlock.
    fn meta_get(conn: &Connection, key: &str) -> Result<Option<String>, Box<dyn Error>> {
        Ok(conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()?)
    }

    fn meta_set(conn: &Connection, key: &str, value: &str) -> Result<(), Box<dyn Error>> {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Row count, for the log line that reports which state store a run is
    /// using, and for tests asserting that imports and upserts do not duplicate
    /// releases.
    pub fn len(&self) -> Result<i64, Box<dyn Error>> {
        let conn = self.conn()?;
        Ok(conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))?)
    }

    pub fn get(&self, id: &str) -> Result<Option<StateEntry>, Box<dyn Error>> {
        let conn = self.conn()?;
        Self::get_with(&conn, id)
    }

    /// Read one row using an already-held connection.
    ///
    /// Associated rather than a method because the mutex is not reentrant, so a
    /// method that locked again while a lock was held would deadlock.
    fn get_with(conn: &Connection, id: &str) -> Result<Option<StateEntry>, Box<dyn Error>> {
        Ok(conn
            .query_row(
                "SELECT id, artist, title, release_year, format, state, size_mb,
                        content_length, tracks, description, downloaded_at, checked_at
                 FROM items WHERE id = ?1",
                params![id],
                |row| {
                    let raw_state: String = row.get(5)?;
                    Ok(StateEntry {
                        id: row.get(0)?,
                        artist: row.get(1)?,
                        title: row.get(2)?,
                        release_year: row.get(3)?,
                        format: row.get(4)?,
                        // An unrecognised state (hand-edited database, future
                        // schema) is treated as skipped rather than
                        // triggering a re-download of the whole library.
                        state: ItemState::parse(&raw_state).unwrap_or(ItemState::Skipped),
                        size_mb: row.get(6)?,
                        content_length: row.get(7)?,
                        tracks: row.get(8)?,
                        description: row.get(9)?,
                        downloaded_at: row.get(10)?,
                        checked_at: row.get(11)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn upsert(&self, entry: &StateEntry) -> Result<(), Box<dyn Error>> {
        let conn = self.conn()?;
        Self::upsert_with(&conn, entry)
    }

    fn upsert_with(conn: &Connection, entry: &StateEntry) -> Result<(), Box<dyn Error>> {
        conn.execute(
            "INSERT INTO items (id, artist, title, release_year, format, state,
                                size_mb, content_length, tracks, description,
                                downloaded_at, checked_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(id) DO UPDATE SET
                artist = excluded.artist,
                title = excluded.title,
                release_year = excluded.release_year,
                format = excluded.format,
                state = excluded.state,
                size_mb = excluded.size_mb,
                content_length = excluded.content_length,
                tracks = excluded.tracks,
                description = excluded.description,
                downloaded_at = excluded.downloaded_at,
                checked_at = excluded.checked_at",
            params![
                entry.id,
                entry.artist,
                entry.title,
                entry.release_year,
                entry.format,
                entry.state.as_str(),
                entry.size_mb,
                entry.content_length,
                entry.tracks,
                entry.description,
                entry.downloaded_at,
                entry.checked_at,
            ],
        )?;
        Ok(())
    }

    /// Record that Bandcamp had no usable download for a purchase.
    ///
    /// Does not overwrite a row that already records a completed download.
    /// `get_digital_item` returning nothing is often transient - a hiccup, an
    /// expired session, an interstitial page - and demoting a downloaded release
    /// to `Skipped` would erase its size fingerprint and drop it out of change
    /// detection permanently, because `Skipped` is never retried.
    ///
    /// Returns whether a row was written.
    pub fn record_unavailable(&self, entry: &StateEntry) -> Result<bool, Box<dyn Error>> {
        let conn = self.conn()?;

        if let Some(existing) = Self::get_with(&conn, &entry.id)? {
            // `downloaded_at` is only ever set by a successful download.
            if existing.downloaded_at.is_some() {
                return Ok(false);
            }
        }

        Self::upsert_with(&conn, entry)?;
        Ok(true)
    }

    /// Record that a release was checked and its fingerprint had not changed.
    pub fn touch_checked(&self, id: &str, now: DateTime<Utc>) -> Result<(), Box<dyn Error>> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE items SET checked_at = ?2 WHERE id = ?1",
            params![id, now.to_rfc3339()],
        )?;
        Ok(())
    }

    /// Record the track count the listing reports for a release that has none.
    ///
    /// A database written before this column existed has no count for anything,
    /// so the first run after an upgrade fills them in as it walks the
    /// collection. Nothing is downloaded: the count describes the library as it
    /// stands, and there is nothing to compare it against yet.
    ///
    /// Only fills a missing value. A release that already has a count is left to
    /// the download path, which records the count of the content it fetched.
    pub fn record_track_count(&self, id: &str, tracks: u32) -> Result<(), Box<dyn Error>> {
        let conn = self.conn()?;
        conn.execute(
            "UPDATE items SET tracks = ?2 WHERE id = ?1 AND tracks IS NULL",
            params![id, tracks],
        )?;
        Ok(())
    }

    /// One-time import of the legacy text cache, so a library that already
    /// exists is not downloaded again. The file is left untouched: it may
    /// belong to another tool, and deleting it is out of scope.
    ///
    /// Call this before the first [`RecheckPolicy`] decision of a run. The
    /// import only inserts rows, so running it after the decisions were taken
    /// would let the releases it covers be treated as unknown and downloaded
    /// again. A `meta` row enforces the once-only behaviour, so calling it more
    /// than once is safe.
    pub fn import_legacy_cache(&self, dir: &Path) -> Result<usize, Box<dyn Error>> {
        // One lock for the whole import. The mutex is not reentrant, and the
        // check-then-insert sequence should not interleave with another writer.
        let conn = self.conn()?;

        if Self::meta_get(&conn, "legacy_imported")?.is_some() {
            return Ok(0);
        }
        let already_populated: i64 =
            conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))?;
        if already_populated > 0 {
            return Ok(0);
        }

        let legacy = dir.join(LEGACY_CACHE_FILENAME);
        if !legacy.is_file() {
            return Ok(0);
        }

        let contents = fs::read_to_string(&legacy)?;
        let mut imported = 0usize;
        for line in contents.lines() {
            let (raw_id, raw_description) = line.split_once('|').unwrap_or((line, ""));
            let id = raw_id.trim();
            if id.is_empty() {
                continue;
            }
            let description = raw_description.trim();
            let state = if description.starts_with(LEGACY_PREORDER_MARKER) {
                ItemState::Preorder
            } else {
                ItemState::Complete
            };
            let description = description
                .strip_prefix(LEGACY_PREORDER_MARKER)
                .unwrap_or(description)
                .trim();

            let inserted = conn.execute(
                "INSERT OR IGNORE INTO items (id, state, description) VALUES (?1, ?2, ?3)",
                params![id, state.as_str(), description],
            )?;
            imported += inserted;
        }
        // Only mark the import done if it produced rows. A file that exists but
        // is empty or half-written (another tool still appending to it) would
        // otherwise be ignored forever once the marker is set.
        if imported > 0 {
            Self::meta_set(&conn, "legacy_imported", &Utc::now().to_rfc3339())?;
        }
        Ok(imported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "bandsnatch-state-{label}-{}-{}",
            std::process::id(),
            rand::random::<u64>()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn entry(id: &str, state: ItemState) -> StateEntry {
        StateEntry {
            id: id.to_string(),
            artist: None,
            title: None,
            release_year: None,
            format: None,
            state,
            size_mb: None,
            content_length: None,
            tracks: None,
            description: String::new(),
            downloaded_at: None,
            checked_at: None,
        }
    }

    #[test]
    fn preorder_is_retried_only_after_it_is_released() {
        // The behaviour that matters for pre-orders: while Bandcamp still calls
        // it a preorder it is left alone, and once that changes it is fetched
        // again, because the stored download was only ever a partial release.
        assert!(!ItemState::Preorder.needs_download(true));
        assert!(ItemState::Preorder.needs_download(false));
        // Everything else is stable.
        assert!(!ItemState::Complete.needs_download(false));
        assert!(!ItemState::Complete.needs_download(true));
        assert!(!ItemState::Skipped.needs_download(false));
    }

    #[test]
    fn legacy_cache_is_imported_once_and_marks_preorders() {
        let dir = temp_dir("legacy");
        fs::write(
            dir.join(LEGACY_CACHE_FILENAME),
            "p1| Album One (2020) by Someone\n\
             p2| @bandsnatch:preorder Album Two (2021) by Someone\n\
             p3| No downloads\n",
        )
        .unwrap();

        let state = State::open(&dir.join(STATE_FILENAME)).unwrap();
        assert_eq!(state.import_legacy_cache(&dir).unwrap(), 3);
        assert_eq!(state.len().unwrap(), 3);

        assert_eq!(state.get("p1").unwrap().unwrap().state, ItemState::Complete);
        assert_eq!(state.get("p2").unwrap().unwrap().state, ItemState::Preorder);
        assert_eq!(state.get("p3").unwrap().unwrap().state, ItemState::Complete);
        // The marker must not survive into the human-readable description.
        assert_eq!(
            state.get("p2").unwrap().unwrap().description,
            "Album Two (2021) by Someone"
        );

        // Importing again must not duplicate rows.
        assert_eq!(state.import_legacy_cache(&dir).unwrap(), 0);
        assert_eq!(state.len().unwrap(), 3);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn recheck_only_revisits_releases_older_than_the_window() {
        let now = Utc::now();
        let policy = RecheckPolicy {
            force: false,
            after_days: Some(7),
            preorder_after_minutes: None,
        };

        // Unknown release: always download.
        assert_eq!(policy.decide(None, false, now), Action::Download);

        // Checked just now: nothing to do.
        let mut fresh = entry("p1", ItemState::Complete);
        fresh.checked_at = Some(now.to_rfc3339());
        assert_eq!(policy.decide(Some(&fresh), false, now), Action::Skip);

        // Checked eight days ago: due for a fingerprint comparison.
        let mut stale = entry("p2", ItemState::Complete);
        stale.checked_at = Some((now - Duration::days(8)).to_rfc3339());
        assert_eq!(policy.decide(Some(&stale), false, now), Action::Recheck);

        // Never checked (imported from the legacy cache): due immediately.
        let never = entry("p3", ItemState::Complete);
        assert_eq!(policy.decide(Some(&never), false, now), Action::Recheck);

        // A released preorder outranks the window and downloads outright.
        let preorder = entry("p4", ItemState::Preorder);
        assert_eq!(policy.decide(Some(&preorder), false, now), Action::Download);

        // Force wins over everything.
        let forced = RecheckPolicy {
            force: true,
            after_days: None,
            preorder_after_minutes: None,
        };
        assert_eq!(forced.decide(Some(&fresh), false, now), Action::Download);
    }

    #[test]
    fn a_preorder_is_compared_on_its_own_interval() {
        let now = Utc::now();
        let policy = RecheckPolicy {
            force: false,
            after_days: None,
            preorder_after_minutes: Some(360),
        };

        // Downloaded a moment ago, so there is nothing new to find yet.
        let mut fresh = entry("p1", ItemState::Preorder);
        fresh.checked_at = Some(now.to_rfc3339());
        assert_eq!(policy.decide(Some(&fresh), true, now), Action::Skip);

        // Compared longer ago than the interval.
        let mut stale = entry("p2", ItemState::Preorder);
        stale.checked_at = Some((now - Duration::minutes(361)).to_rfc3339());
        assert_eq!(policy.decide(Some(&stale), true, now), Action::Recheck);

        // Never compared, as after a legacy cache import.
        let never = entry("p3", ItemState::Preorder);
        assert_eq!(policy.decide(Some(&never), true, now), Action::Recheck);

        // The preorder interval says nothing about a finished release: without
        // a recheck window there is still nothing to do.
        let mut complete = entry("p4", ItemState::Complete);
        complete.checked_at = Some((now - Duration::days(400)).to_rfc3339());
        assert_eq!(policy.decide(Some(&complete), false, now), Action::Skip);

        // Switched off, so a preorder is left to the recheck window.
        let off = RecheckPolicy {
            force: false,
            after_days: None,
            preorder_after_minutes: None,
        };
        assert_eq!(off.decide(Some(&stale), true, now), Action::Skip);
    }

    #[test]
    fn fingerprint_change_is_only_reported_when_both_sides_are_known() {
        let mut e = entry("p1", ItemState::Complete);
        e.size_mb = Some("612.34".to_string());

        assert!(fingerprint_changed(&e, Some("612.35")));
        assert!(!fingerprint_changed(&e, Some("612.34")));
        // Unknown on either side must not look like a change, or every release
        // whose page omits a size would be re-downloaded on every sweep.
        assert!(!fingerprint_changed(&e, None));
        let unknown = entry("p2", ItemState::Complete);
        assert!(!fingerprint_changed(&unknown, Some("612.34")));
    }

    #[test]
    fn a_track_count_is_only_a_change_when_both_sides_are_known() {
        let mut e = entry("p1", ItemState::Complete);
        e.tracks = Some(9);

        assert!(track_count_changed(&e, Some(10)));
        assert!(!track_count_changed(&e, Some(9)));
        // A row written before the count was recorded has nothing to compare,
        // and treating that as a change would re-download the whole library on
        // the first run after an upgrade.
        assert!(!track_count_changed(&e, None));
        let unknown = entry("p2", ItemState::Complete);
        assert!(!track_count_changed(&unknown, Some(10)));
    }

    #[test]
    fn a_missing_track_count_is_filled_in_and_a_known_one_is_left_alone() {
        let dir = temp_dir("tracks");
        let state = State::open(&dir.join(STATE_FILENAME)).unwrap();

        let fresh = entry("p1", ItemState::Complete);
        assert_eq!(fresh.tracks, None);
        state.upsert(&fresh).unwrap();

        state.record_track_count("p1", 9).unwrap();
        assert_eq!(state.get("p1").unwrap().unwrap().tracks, Some(9));

        // A count that is already recorded belongs to the content on disk. The
        // difference from a later listing is what marks the release as changed,
        // so it must survive.
        state.record_track_count("p1", 11).unwrap();
        assert_eq!(state.get("p1").unwrap().unwrap().tracks, Some(9));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_older_database_gains_the_track_count_column() {
        let dir = temp_dir("migration");
        let path = dir.join(STATE_FILENAME);
        {
            let conn = Connection::open(&path).unwrap();
            // The schema as it stood before this column existed, with a row in
            // it: `CREATE TABLE IF NOT EXISTS` would leave all of this alone.
            conn.execute_batch(
                "CREATE TABLE items (
                     id             TEXT PRIMARY KEY NOT NULL,
                     artist         TEXT,
                     title          TEXT,
                     release_year   TEXT,
                     format         TEXT,
                     state          TEXT NOT NULL,
                     size_mb        TEXT,
                     content_length INTEGER,
                     description    TEXT NOT NULL DEFAULT '',
                     downloaded_at  TEXT,
                     checked_at     TEXT
                 );
                 CREATE TABLE meta (key TEXT PRIMARY KEY NOT NULL, value TEXT NOT NULL);
                 INSERT INTO items (id, state, size_mb) VALUES ('p1', 'complete', '612.34');",
            )
            .unwrap();
        }

        let state = State::open(&path).unwrap();
        let old = state.get("p1").unwrap().unwrap();
        assert_eq!(old.tracks, None);
        assert_eq!(old.size_mb.as_deref(), Some("612.34"));
        assert!(!track_count_changed(&old, Some(12)));

        // The added column is usable.
        state.record_track_count("p1", 12).unwrap();
        assert_eq!(state.get("p1").unwrap().unwrap().tracks, Some(12));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_transient_unavailable_does_not_demote_a_downloaded_release() {
        let dir = temp_dir("demote");
        let state = State::open(&dir.join(STATE_FILENAME)).unwrap();
        let now = Utc::now();

        let downloaded = StateEntry::downloaded(
            "p1",
            "Some Artist",
            "Some Album",
            Some("2024"),
            "flac",
            Some("612.34".to_string()),
            Some(1_000),
            Some(9),
            false,
            now,
        );
        state.upsert(&downloaded).unwrap();

        // Bandcamp momentarily reports no item, or no downloads, for a release
        // that has already been downloaded. Demoting it would erase the
        // fingerprint, and since Skipped is never retried, change detection would
        // be disabled permanently.
        let unavailable = StateEntry::unavailable("p1", "UNKNOWN", false, now);
        assert!(!state.record_unavailable(&unavailable).unwrap());

        let read = state.get("p1").unwrap().unwrap();
        assert_eq!(read.state, ItemState::Complete);
        assert_eq!(read.size_mb.as_deref(), Some("612.34"));
        assert!(read.downloaded_at.is_some());

        // A never-downloaded release is still recorded as unavailable.
        let fresh = StateEntry::unavailable("p2", "No downloads", false, now);
        assert!(state.record_unavailable(&fresh).unwrap());
        assert_eq!(state.get("p2").unwrap().unwrap().state, ItemState::Skipped);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn upsert_round_trips_a_fingerprint() {
        let dir = temp_dir("roundtrip");
        let state = State::open(&dir.join(STATE_FILENAME)).unwrap();
        let now = Utc::now();

        let record = StateEntry::downloaded(
            "p1234",
            "Some Artist",
            "Some Album",
            Some("2024"),
            "flac",
            Some("612.34".to_string()),
            Some(642_000_000),
            Some(12),
            false,
            now,
        );
        state.upsert(&record).unwrap();

        let read = state.get("p1234").unwrap().unwrap();
        assert_eq!(read.state, ItemState::Complete);
        assert_eq!(read.size_mb.as_deref(), Some("612.34"));
        assert_eq!(read.content_length, Some(642_000_000));
        assert_eq!(read.tracks, Some(12));
        assert_eq!(read.format.as_deref(), Some("flac"));

        // A second upsert for the same release must update in place.
        let updated = StateEntry::downloaded(
            "p1234",
            "Some Artist",
            "Some Album",
            Some("2024"),
            "flac",
            Some("700.00".to_string()),
            Some(734_000_000),
            Some(13),
            false,
            now,
        );
        state.upsert(&updated).unwrap();
        assert_eq!(state.len().unwrap(), 1);
        assert_eq!(
            state.get("p1234").unwrap().unwrap().size_mb.as_deref(),
            Some("700.00")
        );

        fs::remove_dir_all(&dir).unwrap();
    }
}

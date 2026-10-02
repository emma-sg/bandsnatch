use crate::{
    api,
    cmds::AUDIO_FORMATS,
    cookies,
    library::AlbumPath,
    lock,
    state::{self, Action, RecheckPolicy, State, StateEntry},
    util,
};
use chrono::Utc;
use clap::{builder::PossibleValuesParser, Args as ClapArgs};
use crossbeam_utils::thread;
use indicatif::MultiProgress;
use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

/// Shared handle to the state database. `rusqlite::Connection` is `Send` but not
/// `Sync`, so worker threads serialise their (microsecond) writes through this.
type SharedState = Arc<Mutex<State>>;

/// A release to act on this run.
#[derive(Clone, Debug)]
struct Work {
    id: String,
    download: api::CollectionDownload,
    action: Action,
}

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[arg(long, env = "BS_ALBUM")]
    album: Option<String>,

    #[arg(long, env = "BS_ARTIST")]
    artist: Option<String>,

    /// The audio format to download the files in.
    #[arg(short = 'f', long = "format", value_parser = PossibleValuesParser::new(AUDIO_FORMATS), env = "BS_FORMAT")]
    audio_format: String,

    #[arg(short, long, value_name = "COOKIES_FILE", env = "BS_COOKIES")]
    cookies: Option<String>,

    /// Enables some extra debug output in certain scenarios.
    #[arg(long, env = "BS_DEBUG")]
    debug: bool,

    /// Return a list of all tracks to be downloaded, without actually downloading them.
    #[arg(short = 'd', long = "dry-run")]
    dry_run: bool,

    /// Ignores all recorded state and downloads every release again.
    #[arg(short = 'F', long, env = "BS_FORCE")]
    force: bool,

    /// Re-check an already-downloaded release for an in-place update once this
    /// many days have passed since it was last checked.
    ///
    /// Bandcamp does not change a purchase ID when an artist replaces a
    /// release's audio, so the only way to notice is to poll the download page
    /// and compare the advertised archive size.
    #[arg(long, value_name = "DAYS", env = "BS_RECHECK_AFTER")]
    recheck_after: Option<i64>,

    /// Re-check every downloaded release during this run, downloading only the
    /// ones whose advertised size changed. Cheaper than `--force`, which
    /// re-downloads unconditionally.
    #[arg(long, env = "BS_RECHECK_ALL")]
    recheck_all: bool,

    /// The amount of parallel jobs (threads) to use.
    #[arg(short, long, default_value_t = 4, env = "BS_JOBS")]
    jobs: u8,

    /// Maximum number of releases to process. Useful for testing.
    #[arg(short = 'n', long, env = "BS_LIMIT")]
    limit: Option<usize>,

    /// Fail immediately instead of waiting when another run holds the lock.
    #[arg(long, env = "BS_NO_WAIT")]
    no_wait: bool,

    /// The folder to extract downloaded releases to.
    #[arg(
        short,
        long = "output-folder",
        value_name = "FOLDER",
        default_value = "./",
        env = "BS_OUTPUT_FOLDER"
    )]
    output_folder: String,

    /// Folder layout for each release, relative to the output folder.
    ///
    /// Placeholders: {artist}, {album}, {year}, {id}. A placeholder with no
    /// value is omitted along with any brackets it leaves empty, so a release
    /// with no reported date is not named `Album ()`.
    #[arg(
        long,
        value_name = "TEMPLATE",
        default_value = crate::library::DEFAULT_ALBUM_PATH,
        env = "BS_ALBUM_PATH"
    )]
    album_path: String,

    /// Path to the state database. Defaults to `.bandsnatch-state.db` inside the
    /// output folder.
    #[arg(long, value_name = "PATH", env = "BS_STATE")]
    state: Option<String>,

    /// Name of the user to download releases from (must be logged in through cookies).
    #[arg(env = "BS_USER")]
    user: String,
}

/// Record that Bandcamp has no usable download for a purchase.
///
/// These are deliberately not recorded as downloaded: a recheck sweep can
/// revisit them, but no run retries them automatically, because retrying cannot
/// succeed. Preorders are the exception - they become downloadable on release.
fn record_unavailable(state: &SharedState, id: &str, description: &str, is_preorder: bool) {
    let record = StateEntry::unavailable(id, description, is_preorder, Utc::now());
    match state.lock() {
        Ok(guard) => {
            if let Err(e) = guard.upsert(&record) {
                warn!("failed to record state for {id}: {e}");
            }
        }
        Err(_) => warn!("state lock poisoned, not recording {id}"),
    }
}

pub fn command(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let Args {
        album,
        artist,
        audio_format,
        cookies,
        debug,
        dry_run,
        force,
        recheck_after,
        recheck_all,
        jobs,
        limit,
        no_wait,
        output_folder,
        album_path,
        state: state_option,
        user,
    } = args;

    let cookies_file = cookies.map(|p| {
        let expanded = shellexpand::tilde(&p);
        expanded.into_owned()
    });
    let root = shellexpand::tilde(&output_folder);
    let root = Path::new(root.as_ref());
    let limit = limit.unwrap_or(usize::MAX);

    // Validate the layout before doing any work, so a typo fails immediately
    // rather than being written into every folder name on disk.
    let album_path = AlbumPath::new(&album_path)?;

    let root_exists = match fs::metadata(root) {
        Ok(d) => Some(d.is_dir()),
        Err(_) => None,
    };

    match root_exists {
        Some(true) => (),
        Some(false) => {
            error!("Cannot use `output-folder`, as it is not a folder. Please delete it and create as a directory, or try a different path.");
            std::process::exit(1);
        }
        None => fs::create_dir_all(root)?,
    }

    let state_path = state_option
        .map(|p| PathBuf::from(shellexpand::tilde(&p).into_owned()))
        .unwrap_or_else(|| root.join(state::STATE_FILENAME));

    // Held for the whole run. Bound to a name rather than discarded so that the
    // lock lives until this function returns; SQLite protects the database, this
    // protects the library on disk from a concurrent run. Keyed to the output
    // folder rather than the state database, because the output folder is the
    // resource being protected.
    let _lock = lock::RunLock::acquire(&lock::lock_path_for(root), !no_wait)?;

    let state: SharedState = Arc::new(Mutex::new(State::open(&state_path)?));
    {
        let guard = state
            .lock()
            .map_err(|_| io::Error::other("state lock poisoned"))?;
        let imported = guard.import_legacy_cache(root)?;
        if imported > 0 {
            info!(
                "Imported {imported} entries from the legacy `{}` cache; it is no longer read.",
                state::LEGACY_CACHE_FILENAME
            );
        }
    }

    // `--recheck-all` means "every release is due for a comparison now";
    // `--force` means "download everything without comparing".
    let policy = RecheckPolicy {
        force,
        after_days: if recheck_all { Some(0) } else { recheck_after },
    };

    let cookies = cookies::get_bandcamp_cookies(cookies_file.as_deref())?;
    let api = Arc::new(api::Api::new(cookies));

    let download_urls = api
        .get_download_urls(&user, artist.as_ref(), album.as_ref())?
        .download_urls;

    let now = Utc::now();
    let (items, to_download, to_recheck, up_to_date) = {
        let guard = state
            .lock()
            .map_err(|_| io::Error::other("state lock poisoned"))?;
        let mut items = Vec::new();
        let mut to_download = 0usize;
        let mut to_recheck = 0usize;
        let mut up_to_date = 0usize;

        for (id, download) in download_urls {
            if items.len() >= limit {
                break;
            }
            let entry = guard.get(&id)?;
            match policy.decide(entry.as_ref(), download.is_preorder, now) {
                Action::Skip => up_to_date += 1,
                Action::Download => {
                    to_download += 1;
                    items.push(Work {
                        id,
                        download,
                        action: Action::Download,
                    });
                }
                Action::Recheck => {
                    to_recheck += 1;
                    items.push(Work {
                        id,
                        download,
                        action: Action::Recheck,
                    });
                }
            }
        }
        (items, to_download, to_recheck, up_to_date)
    };

    let total = items.len();
    if dry_run {
        println!(
            "Checking {total} releases ({to_download} to download, {to_recheck} to re-check, {up_to_date} up to date)"
        );
    } else {
        println!(
            "Processing {total} releases ({to_download} to download, {to_recheck} to re-check, {up_to_date} up to date)"
        );
    }

    if items.is_empty() {
        println!("Everything is up to date.");
        return Ok(());
    }

    let queue = util::WorkQueue::from_vec(items);
    let m = Arc::new(MultiProgress::new());
    let dry_run_results = Arc::new(Mutex::new(Vec::<String>::new()));
    let updated = Arc::new(Mutex::new(Vec::<String>::new()));

    thread::scope(|scope| {
        for i in 0..jobs {
            let api = api.clone();
            let state = state.clone();
            let m = m.clone();
            let queue = queue.clone();
            let audio_format = audio_format.clone();
            let album_path = album_path.clone();
            let dry_run_results = dry_run_results.clone();
            let updated = updated.clone();

            // somehow re-create thread if it panics
            scope.spawn(move |_| {
                while let Some(Work {
                    id,
                    download,
                    action,
                }) = queue.get_work()
                {
                    m.suspend(|| debug!("thread {i} taking {id}"));

                    let item = match api.get_digital_item(&download.url, &debug) {
                        Ok(Some(item)) => item,
                        Ok(None) => {
                            warn!("Could not find digital item for {id}");
                            record_unavailable(&state, &id, "UNKNOWN", download.is_preorder);
                            continue;
                        }
                        Err(e) => {
                            warn!("Failed to read release info for {id}: {e}; skipped.");
                            continue;
                        }
                    };

                    if item.downloads.is_none() {
                        warn!("Skipping {id}, does not have any downloads");
                        record_unavailable(&state, &id, "No downloads", download.is_preorder);
                        continue;
                    }

                    let advertised = item.advertised_size(&audio_format);

                    // A re-check downloads only when the advertised size no
                    // longer matches what was recorded for this release.
                    if action == Action::Recheck {
                        let Some(changed) =
                            recheck_needed(&state, &id, advertised.as_deref(), !dry_run)
                        else {
                            continue;
                        };
                        if !changed {
                            m.suspend(|| debug!("{id} unchanged"));
                            continue;
                        }
                        m.suspend(|| {
                            info!(
                                "{id} advertised size changed, re-downloading: {} - {}",
                                util::display_safe(&item.title),
                                util::display_safe(&item.artist)
                            )
                        });
                        if let Ok(mut list) = updated.lock() {
                            list.push(format!(
                                "{id}, {} - {}",
                                util::display_safe(&item.title),
                                util::display_safe(&item.artist)
                            ));
                        }
                    }

                    if dry_run {
                        if let Ok(mut results) = dry_run_results.lock() {
                            results.push(format!("{id}, {} - {}", item.title, item.artist));
                        }
                        continue;
                    }

                    m.println(format!(
                        "Trying {id}, {} - {} ({:?})",
                        util::display_safe(&item.title),
                        util::display_safe(&item.artist),
                        item.is_single(),
                    ))
                    .unwrap();

                    // A layout that cannot produce a safe folder is a per-release
                    // problem, so skip that release rather than abandoning the run.
                    let path = match album_path.render(
                        root,
                        &item.artist,
                        &item.title,
                        item.release_year().as_deref(),
                        &id,
                    ) {
                        Ok(path) => path,
                        Err(e) => {
                            warn!("Skipping {id}, cannot build a destination path: {e}");
                            continue;
                        }
                    };

                    let content_length = match api.download_item(&item, &path, &audio_format, &m) {
                        Ok(len) => len,
                        Err(e) => {
                            // A failed download is deliberately not recorded, so
                            // the next run retries it.
                            warn!("Failed to download {id}: {e}; skipped.");
                            continue;
                        }
                    };

                    let record = StateEntry::downloaded(
                        &id,
                        &item.artist,
                        &item.title,
                        item.release_year().as_deref(),
                        &audio_format,
                        advertised,
                        Some(content_length),
                        download.is_preorder,
                        Utc::now(),
                    );
                    match state.lock() {
                        Ok(guard) => {
                            if let Err(e) = guard.upsert(&record) {
                                warn!("failed to record state for {id}: {e}");
                            }
                        }
                        Err(_) => warn!("state lock poisoned, not recording {id}"),
                    }
                }
            });
        }
    })
    .unwrap();

    if dry_run {
        let results = dry_run_results
            .lock()
            .map_err(|_| io::Error::other("dry-run results lock poisoned"))?;
        println!("{}", results.join("\n"));
        return Ok(());
    }

    if let Ok(list) = updated.lock() {
        if !list.is_empty() {
            println!("Re-downloaded {} updated release(s):", list.len());
            for line in list.iter() {
                println!("  {line}");
            }
        }
    }

    println!("Finished!");

    Ok(())
}

/// Whether a re-check should turn into a download.
///
/// `None` means the state was unavailable, so the caller should skip the
/// release rather than guess.
///
/// `record` is false for a dry run, which must leave the state database
/// untouched.
fn recheck_needed(
    state: &SharedState,
    id: &str,
    advertised: Option<&str>,
    record: bool,
) -> Option<bool> {
    let guard = match state.lock() {
        Ok(guard) => guard,
        Err(_) => {
            warn!("state lock poisoned, skipping re-check of {id}");
            return None;
        }
    };

    match guard.get(id) {
        // A missing row is unexpected during a re-check, so treat it as changed.
        Ok(None) => Some(true),
        Ok(Some(entry)) => {
            let changed = state::fingerprint_changed(&entry, advertised);
            if !changed && record {
                if let Err(e) = guard.touch_checked(id, Utc::now()) {
                    warn!("failed to record check for {id}: {e}");
                }
            }
            Some(changed)
        }
        Err(e) => {
            warn!("failed to read state for {id}: {e}");
            None
        }
    }
}

use crate::{
    api::{self, CollectionDownload},
    cmds::{CommonArgs, AUDIO_FORMATS},
    state::{self, Action, RecheckPolicy, State, StateEntry},
    util,
};
use chrono::Utc;
use clap::{builder::PossibleValuesParser, Args as ClapArgs};
use crossbeam_utils::thread;
use indicatif::MultiProgress;
use std::{
    collections::VecDeque,
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

/// Shared handle to the state store. `State` serialises access internally, so
/// callers only need this `Arc`.
type SharedState = Arc<State>;

/// A release this run will act on.
#[derive(Clone, Debug)]
struct Work {
    id: String,
    download: api::CollectionDownload,
    action: Action,
}

#[derive(Debug, ClapArgs)]
pub struct Args {
    #[command(flatten)]
    common: CommonArgs,

    #[arg(long, env = "BS_ALBUM")]
    album: Option<String>,

    #[arg(long, env = "BS_ARTIST")]
    artist: Option<String>,

    /// The audio format to download the files in.
    #[arg(short = 'f', long = "format", value_parser = PossibleValuesParser::new(AUDIO_FORMATS), env = "BS_FORMAT")]
    audio_format: String,

    /// Ignores all recorded state and downloads every release again.
    #[arg(short = 'F', long, env = "BS_FORCE")]
    force: bool,

    /// Re-check an already-downloaded release for an in-place update once this
    /// many days have passed since it was last checked.
    ///
    /// Bandcamp does not change a purchase ID when an artist replaces a
    /// release's audio, so the only way to notice is to poll the download page
    /// and compare the advertised archive size.
    ///
    /// Bounded to reject negatives, which would make every release due on every
    /// run, and values large enough to overflow chrono's `Duration::days`, which
    /// panics.
    #[arg(
        long,
        value_name = "DAYS",
        env = "BS_RECHECK_AFTER",
        value_parser = clap::value_parser!(i64).range(0..=36500)
    )]
    recheck_after: Option<i64>,

    /// Re-check every downloaded release during this run, downloading only the
    /// ones whose advertised size changed. Cheaper than `--force`, which
    /// re-downloads unconditionally.
    #[arg(long, env = "BS_RECHECK_ALL")]
    recheck_all: bool,

    /// The amount of parallel jobs (threads) to use.
    ///
    /// At least one: zero workers would process nothing and still report
    /// success.
    #[arg(
        short,
        long,
        default_value_t = 4,
        env = "BS_JOBS",
        value_parser = clap::value_parser!(u8).range(1..=255)
    )]
    jobs: u8,

    /// Maximum number of releases to process, newest purchase first. Useful for
    /// testing.
    #[arg(short = 'n', long, env = "BS_LIMIT")]
    limit: Option<usize>,

    /// Name of the user to download releases from (must be logged in through cookies).
    #[arg(env = "BS_USER")]
    user: String,
}

/// Record that Bandcamp has no usable download for a purchase.
///
/// These are not recorded as downloaded: a recheck sweep can revisit them, but
/// no run retries them automatically because retrying cannot succeed.
/// Preorders are the exception - they become downloadable on release.
///
/// The state store holds the rule against demoting a downloaded release.
fn record_unavailable(state: &SharedState, id: &str, description: &str, is_preorder: bool) {
    let record = StateEntry::unavailable(id, description, is_preorder, Utc::now());
    match state.record_unavailable(&record) {
        Ok(true) => (),
        Ok(false) => {
            debug!("not recording {id} as unavailable: it is already recorded as downloaded")
        }
        Err(e) => warn!("failed to record state for {id}: {e}"),
    }
}

pub fn command(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    let Args {
        common,
        album,
        artist,
        audio_format,
        force,
        recheck_after,
        recheck_all,
        jobs,
        limit,
        user,
    } = args;

    let context = common.build()?;
    let root = context.root.as_path();
    let limit = limit.unwrap_or(usize::MAX);
    let debug = common.debug;
    let dry_run = common.dry_run;

    // `--recheck-all` makes every release due for a comparison now; `--force`
    // downloads everything without comparing.
    let policy = RecheckPolicy {
        force,
        after_days: if recheck_all { Some(0) } else { recheck_after },
    };

    let download_urls = context
        .api
        .get_download_urls(&user, artist.as_ref(), album.as_ref())?
        .download_urls;

    let mut download_urls: Vec<(String, CollectionDownload)> = download_urls.into_iter().collect();
    let undated = download_urls
        .iter()
        .filter(|(_, download)| download.purchased.is_none())
        .count();
    if undated > 0 {
        debug!("{undated} releases have no readable purchase date; they come last");
    }
    order_by_purchase(&mut download_urls);

    let now = Utc::now();
    let (items, to_download, to_recheck, up_to_date) = {
        let mut items = Vec::new();
        let mut to_download = 0usize;
        let mut to_recheck = 0usize;
        let mut up_to_date = 0usize;

        for (id, download) in download_urls {
            if items.len() >= limit {
                break;
            }
            let entry = context.state.get(&id)?;
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

    // A plain queue. Popping is a short critical section, and the download that
    // follows runs without holding the lock.
    let queue = Arc::new(Mutex::new(VecDeque::from(items)));
    let m = Arc::new(MultiProgress::new());
    let dry_run_results = Arc::new(Mutex::new(Vec::<String>::new()));
    let updated = Arc::new(Mutex::new(Vec::<String>::new()));

    // Per-thread clones of the shared handles.
    let state: SharedState = context.state.clone();
    let api = context.api.clone();
    let album_path = context.album_path.clone();

    // Counted for the summary at the end of the run. Relaxed ordering is
    // enough: the values are only read after every worker has joined.
    let downloaded = AtomicUsize::new(0);
    let re_downloaded = AtomicUsize::new(0);
    let unchanged = AtomicUsize::new(0);
    let skipped = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);

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
            let downloaded = &downloaded;
            let re_downloaded = &re_downloaded;
            let unchanged = &unchanged;
            let skipped = &skipped;
            let failed = &failed;

            // somehow re-create thread if it panics
            scope.spawn(move |_| {
                loop {
                    let next = match queue.lock() {
                        Ok(mut queue) => queue.pop_front(),
                        // A panicked worker poisoned the queue. Treat it as
                        // empty so the remaining workers finish.
                        Err(_) => None,
                    };
                    let Some(Work {
                        id,
                        download,
                        action,
                    }) = next
                    else {
                        break;
                    };

                    m.suspend(|| debug!("thread {i} taking {id}"));

                    let item = match api.get_digital_item(&download.url, &debug) {
                        Ok(Some(item)) => item,
                        Ok(None) => {
                            // Nothing to retry: Bandcamp has no digital item for
                            // this purchase.
                            warn!("Skipping {id}, Bandcamp has no digital item for it");
                            skipped.fetch_add(1, Ordering::Relaxed);
                            record_unavailable(&state, &id, "UNKNOWN", download.is_preorder);
                            continue;
                        }
                        Err(e) => {
                            warn!("Failed to read release info for {id}: {e}; will retry next run");
                            failed.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    };

                    if item.downloads.is_none() {
                        warn!("Skipping {id}, does not have any downloads");
                        skipped.fetch_add(1, Ordering::Relaxed);
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
                            failed.fetch_add(1, Ordering::Relaxed);
                            continue;
                        };
                        if !changed {
                            m.suspend(|| debug!("{id} unchanged"));
                            unchanged.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                        m.suspend(|| {
                            info!(
                                "{id} advertised size changed, re-downloading: {} - {}",
                                util::display_safe(&item.title),
                                util::display_safe(&item.artist)
                            )
                        });
                    }

                    if dry_run {
                        if let Ok(mut results) = dry_run_results.lock() {
                            results.push(format!(
                                "{id}, {} - {}",
                                util::display_safe(&item.title),
                                util::display_safe(&item.artist)
                            ));
                        }
                        continue;
                    }

                    m.suspend(|| {
                        info!(
                            "Downloading {id}, {} - {}",
                            util::display_safe(&item.title),
                            util::display_safe(&item.artist)
                        )
                    });

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
                            skipped.fetch_add(1, Ordering::Relaxed);
                            continue;
                        }
                    };

                    let content_length =
                        match api.download_item(&item, &path, &id, &audio_format, &m) {
                            Ok(len) => len,
                            Err(e) => {
                                // A failed download is not recorded, so the next run
                                // retries it.
                                warn!("Failed to download {id}: {e}; will retry next run");
                                failed.fetch_add(1, Ordering::Relaxed);
                                continue;
                            }
                        };

                    let finished = format!(
                        "{id}, {} - {} ({:.1} MB)",
                        util::display_safe(&item.title),
                        util::display_safe(&item.artist),
                        content_length as f64 / 1_048_576.0
                    );

                    // Recorded only now: a re-download that failed must not be
                    // reported as having been updated.
                    if action == Action::Recheck {
                        re_downloaded.fetch_add(1, Ordering::Relaxed);
                        m.suspend(|| info!("Re-downloaded {finished}"));
                        if let Ok(mut list) = updated.lock() {
                            list.push(finished);
                        }
                    } else {
                        downloaded.fetch_add(1, Ordering::Relaxed);
                        m.suspend(|| info!("Downloaded {finished}"));
                    }

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
                    if let Err(e) = state.upsert(&record) {
                        warn!("failed to record state for {id}: {e}");
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

    println!(
        "Run summary: {} downloaded, {} re-downloaded, {} unchanged, {} skipped, {} failed",
        downloaded.load(Ordering::Relaxed),
        re_downloaded.load(Ordering::Relaxed),
        unchanged.load(Ordering::Relaxed),
        skipped.load(Ordering::Relaxed),
        failed.load(Ordering::Relaxed),
    );

    Ok(())
}

/// Put the newest purchases first.
///
/// `--limit` then takes the releases bought most recently, which is what a
/// first run or a quick check wants. Releases whose purchase date Bandcamp did
/// not report, or reported in a format this tool cannot read, come last, and
/// equal dates fall back to the release id so two runs over the same collection
/// work through it in the same order.
fn order_by_purchase(items: &mut [(String, CollectionDownload)]) {
    items
        .sort_by(|(a_id, a), (b_id, b)| b.purchased.cmp(&a.purchased).then_with(|| a_id.cmp(b_id)));
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
    match state.get(id) {
        // A missing row is unexpected during a re-check, so treat it as changed.
        Ok(None) => Some(true),
        Ok(Some(entry)) => {
            let changed = state::fingerprint_changed(&entry, advertised);
            if !changed && record {
                if let Err(e) = state.touch_checked(id, Utc::now()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDateTime;

    fn download(purchased: Option<&str>) -> CollectionDownload {
        CollectionDownload {
            url: format!("https://example.invalid/{purchased:?}"),
            is_preorder: false,
            purchased: purchased
                .map(|raw| NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H:%M:%S").unwrap()),
        }
    }

    #[test]
    fn the_newest_purchase_comes_first_and_an_undated_release_comes_last() {
        let mut items = vec![
            ("p1".to_owned(), download(Some("2026-06-06T14:23:01"))),
            ("p2".to_owned(), download(None)),
            ("p3".to_owned(), download(Some("2026-10-02T17:39:55"))),
            ("p4".to_owned(), download(Some("2026-08-04T23:43:59"))),
        ];

        order_by_purchase(&mut items);

        let ids: Vec<&str> = items.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["p3", "p4", "p1", "p2"]);
    }

    #[test]
    fn releases_bought_at_the_same_time_keep_a_stable_order() {
        let mut items = vec![
            ("p9".to_owned(), download(Some("2026-10-02T17:39:55"))),
            ("p1".to_owned(), download(Some("2026-10-02T17:39:55"))),
        ];

        order_by_purchase(&mut items);

        let ids: Vec<&str> = items.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["p1", "p9"]);
    }
}

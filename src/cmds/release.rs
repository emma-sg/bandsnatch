use crate::{
    api,
    cmds::AUDIO_FORMATS,
    cookies,
    library::AlbumPath,
    lock,
    state::{self, ItemState, State, StateEntry},
    util,
};
use chrono::Utc;
use clap::{builder::PossibleValuesParser, Args as ClapArgs};
use indicatif::MultiProgress;
use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
};

/// Re-download a single purchase, bypassing the state cache.
///
/// This exists because Bandcamp has no change feed. When an artist replaces a
/// release's audio, or a pre-order finally ships, nothing about the purchase or
/// its ID changes - so picking up an update on demand means asking for that one
/// release again rather than waiting for a comparison sweep.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// A Bandcamp download page URL (https://bandcamp.com/download/...), or a
    /// collection sale-item key such as `p1234`. The key is the value inside the
    /// `[p1234]` suffix of each release folder.
    target: String,

    #[arg(short, long, value_name = "COOKIES_FILE", env = "BS_COOKIES")]
    cookies: Option<String>,

    /// Bandcamp username. Required when resolving a sale-item key, because the
    /// key can only be found by reading your collection listing.
    #[arg(short, long, env = "BS_USER")]
    user: Option<String>,

    /// The audio format to download the release in.
    #[arg(
        short = 'f',
        long = "format",
        value_parser = PossibleValuesParser::new(AUDIO_FORMATS),
        default_value = "flac",
        env = "BS_FORMAT"
    )]
    audio_format: String,

    /// The folder to extract the release to.
    #[arg(
        short,
        long = "output-folder",
        value_name = "FOLDER",
        default_value = "./",
        env = "BS_OUTPUT_FOLDER"
    )]
    output_folder: String,

    /// Folder layout for the release, relative to the output folder.
    ///
    /// Placeholders: {artist}, {album}, {year}, {id}. Must match the layout the
    /// release was originally downloaded with, or the re-download will land
    /// beside the existing folder instead of replacing it.
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

    /// Enables some extra debug output in certain scenarios.
    #[arg(long, env = "BS_DEBUG")]
    debug: bool,

    /// Report what would happen without downloading or writing anything.
    #[arg(short = 'd', long = "dry-run")]
    dry_run: bool,

    /// Fail immediately instead of waiting when another run holds the lock.
    #[arg(long, env = "BS_NO_WAIT")]
    no_wait: bool,
}

pub fn command(args: Args) -> Result<(), Box<dyn Error>> {
    let Args {
        target,
        cookies,
        user,
        audio_format,
        output_folder,
        album_path,
        state: state_option,
        debug,
        dry_run,
        no_wait,
    } = args;

    let cookies_file = cookies.map(|p| {
        let expanded = shellexpand::tilde(&p);
        expanded.into_owned()
    });
    let root = shellexpand::tilde(&output_folder);
    let root = Path::new(root.as_ref());
    fs::create_dir_all(root)?;

    let album_path = AlbumPath::new(&album_path)?;

    let state_path = state_option
        .map(|p| PathBuf::from(shellexpand::tilde(&p).into_owned()))
        .unwrap_or_else(|| root.join(state::STATE_FILENAME));

    // Keyed to the output folder, the resource this lock actually protects.
    let _lock = lock::RunLock::acquire(&lock::lock_path_for(root), !no_wait)?;

    let cookies = cookies::get_bandcamp_cookies(cookies_file.as_deref())?;
    let api = api::Api::new(cookies);

    // A sale-item key can only be resolved through the collection listing; a
    // direct download page URL needs no lookup at all.
    let is_url = target.starts_with("http://") || target.starts_with("https://");
    let (release_key, page_url, listed_as_preorder) = if is_url {
        (None, target.clone(), false)
    } else {
        let user = user.as_deref().ok_or(
            "resolving a sale-item key needs --user (or BS_USER); pass a full download URL instead",
        )?;
        let page = api.get_download_urls(user, None, None)?;
        let download = page
            .download_urls
            .get(&target)
            .ok_or_else(|| format!("no release `{target}` in {user}'s collection"))?;
        (
            Some(target.clone()),
            download.url.clone(),
            download.is_preorder,
        )
    };

    let item = api
        .get_digital_item(&page_url, &debug)?
        .ok_or_else(|| format!("could not read any digital item from {page_url}"))?;

    if item.downloads.is_none() {
        return Err(format!(
            "no downloads are available for {} - {}",
            item.artist, item.title
        )
        .into());
    }

    // Prefer the collection key. Falling back to the numeric item id keeps a
    // URL-driven re-download landing in the same stable folder as a collection
    // run would have used.
    //
    // Whether the release came from the listing matters beyond naming: only the
    // listing reports preorder status.
    let from_listing = release_key.is_some();
    let id = release_key
        .or_else(|| item.item_id.map(|id| format!("p{id}")))
        .ok_or("could not determine an identifier for this release; pass its sale-item key instead")?;

    let state = State::open(&state_path)?;

    // Without a collection listing we cannot tell whether this is still a
    // preorder, so preserve whatever we already recorded rather than guessing.
    let is_preorder = if from_listing {
        listed_as_preorder
    } else {
        state
            .get(&id)?
            .map(|entry| entry.state == ItemState::Preorder)
            .unwrap_or(false)
    };

    let path = album_path.render(
        root,
        &item.artist,
        &item.title,
        item.release_year().as_deref(),
        &id,
    )?;
    println!(
        "{} {} - {} ({}) -> {}",
        if dry_run {
            "Would re-download"
        } else {
            "Re-downloading"
        },
        util::display_safe(&item.artist),
        util::display_safe(&item.title),
        audio_format,
        path.display()
    );

    if dry_run {
        return Ok(());
    }

    let m = MultiProgress::new();
    let content_length = api.download_item(&item, &path, &audio_format, &m)?;

    let record = StateEntry::downloaded(
        &id,
        &item.artist,
        &item.title,
        item.release_year().as_deref(),
        &audio_format,
        item.advertised_size(&audio_format),
        Some(content_length),
        is_preorder,
        Utc::now(),
    );
    state.upsert(&record)?;

    println!("Done.");

    Ok(())
}

use crate::{
    cmds::{CommonArgs, AUDIO_FORMATS},
    state::{ItemState, StateEntry},
    util,
};
use chrono::Utc;
use clap::{builder::PossibleValuesParser, Args as ClapArgs};
use indicatif::MultiProgress;
use std::error::Error;

/// Re-download a single purchase, bypassing the state cache.
///
/// Bandcamp has no change feed, and when an artist replaces a release's audio,
/// or a pre-order ships, neither the purchase nor its ID changes. Picking up
/// such an update on demand means requesting that one release again, without
/// waiting for a comparison sweep.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// A Bandcamp download page URL (https://bandcamp.com/download/...), or a
    /// collection sale-item key such as `p1234` (the `[p1234]` suffix of a
    /// release folder).
    target: String,

    #[command(flatten)]
    common: CommonArgs,

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
}

pub fn command(args: Args) -> Result<(), Box<dyn Error>> {
    let Args {
        target,
        common,
        user,
        audio_format,
    } = args;

    let context = common.build()?;

    // A sale-item key can only be resolved through the collection listing; a
    // direct download page URL needs no lookup at all.
    let is_url = target.starts_with("http://") || target.starts_with("https://");
    let (release_key, page_url, listed_as_preorder) = if is_url {
        (None, target.clone(), false)
    } else {
        let user = user.as_deref().ok_or(
            "resolving a sale-item key needs --user (or BS_USER); pass a full download URL instead",
        )?;
        let page = context.api.get_download_urls(user, None, None)?;
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

    let item = context
        .api
        .get_digital_item(&page_url, &common.debug)?
        .ok_or_else(|| format!("could not read any digital item from {page_url}"))?;

    if item.downloads.is_none() {
        return Err(format!(
            "no downloads are available for {} - {}",
            util::display_safe(&item.artist),
            util::display_safe(&item.title)
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

    // Without a collection listing there is no way to tell whether this is still
    // a preorder, so preserve whatever was recorded rather than guessing.
    let is_preorder = if from_listing {
        listed_as_preorder
    } else {
        context
            .state
            .get(&id)?
            .map(|entry| entry.state == ItemState::Preorder)
            .unwrap_or(false)
    };

    let path = context.album_path.render(
        &context.root,
        &item.artist,
        &item.title,
        item.release_year().as_deref(),
        &id,
    )?;
    println!(
        "{} {} - {} ({}) -> {}",
        if common.dry_run {
            "Would re-download"
        } else {
            "Re-downloading"
        },
        util::display_safe(&item.artist),
        util::display_safe(&item.title),
        audio_format,
        path.display()
    );

    if common.dry_run {
        return Ok(());
    }

    let m = MultiProgress::new();
    let content_length = context
        .api
        .download_item(&item, &path, &audio_format, &m)?;

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
    context.state.upsert(&record)?;

    println!("Done.");

    Ok(())
}

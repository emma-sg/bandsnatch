use crate::{
    api::CollectionDownload,
    cmds::{CommonArgs, AUDIO_FORMATS},
    state::{ItemState, StateEntry},
    util,
};
use chrono::Utc;
use clap::{builder::PossibleValuesParser, Args as ClapArgs};
use indicatif::MultiProgress;
use std::collections::HashMap;
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
    ///
    /// An album or track page URL also works, and is looked up in your
    /// collection to find its sale item, so it needs `--user`.
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

    // A sale-item key can only be resolved through the collection listing. An
    // album or track page needs the same lookup: it names the release, but the
    // sale item is what names its folder and what the download URL is built
    // from. A direct download page URL needs no lookup at all.
    let is_url = target.starts_with("http://") || target.starts_with("https://");
    let is_page = target.contains("/album/") || target.contains("/track/");
    let (release_key, page_url, listed_as_preorder) = if is_page {
        let page = context.api.get_tralbum_page(&target)?;
        let user = user.as_deref().ok_or(
            "resolving an album or track page needs --user (or BS_USER); pass the sale-item key instead",
        )?;
        let listing = context
            .api
            .get_download_urls(user, Some(&page.artist), Some(&page.title))?;
        let (key, download) = single_match(listing.download_urls, &page.title, &page.artist, user)?;

        (Some(key), download.url, download.is_preorder)
    } else if is_url {
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
        .ok_or(
            "could not determine an identifier for this release; pass its sale-item key instead",
        )?;

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
        .download_item(&item, &path, &id, &audio_format, &m)?;

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

/// The one collection entry that matches an album or track page.
///
/// An album and a track can share a title, so more than one entry can match;
/// that needs a sale-item key rather than a guess about which was meant.
fn single_match(
    matches: HashMap<String, CollectionDownload>,
    title: &str,
    artist: &str,
    user: &str,
) -> Result<(String, CollectionDownload), Box<dyn Error>> {
    let mut matches = matches.into_iter();
    let first = matches.next().ok_or_else(|| {
        format!(
            "no release `{}` by {} in {user}'s collection",
            util::display_safe(title),
            util::display_safe(artist)
        )
    })?;

    if matches.next().is_some() {
        return Err(format!(
            "more than one release in {user}'s collection matches `{}` by {}; pass its sale-item key instead",
            util::display_safe(title),
            util::display_safe(artist)
        )
        .into());
    }

    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn download(url: &str) -> CollectionDownload {
        CollectionDownload {
            url: url.to_owned(),
            is_preorder: false,
            purchased: None,
        }
    }

    #[test]
    fn a_page_with_one_matching_release_uses_that_sale_item() {
        let mut matches = HashMap::new();
        matches.insert("p1".to_owned(), download("https://example.invalid/1"));

        let (key, download) =
            single_match(matches, "humblewrap.", "cali cartier", "emmasg").unwrap();

        assert_eq!(key, "p1");
        assert_eq!(download.url, "https://example.invalid/1");
    }

    #[test]
    fn a_page_with_no_matching_release_names_it_and_the_user() {
        let err = single_match(HashMap::new(), "humblewrap.", "cali cartier", "emmasg")
            .unwrap_err()
            .to_string();

        assert!(err.contains("humblewrap."), "{err}");
        assert!(err.contains("emmasg"), "{err}");
    }

    #[test]
    fn a_page_matching_two_releases_asks_for_the_sale_item_key() {
        let mut matches = HashMap::new();
        matches.insert("p1".to_owned(), download("https://example.invalid/1"));
        matches.insert("p2".to_owned(), download("https://example.invalid/2"));

        let err = single_match(matches, "x", "y", "emmasg")
            .unwrap_err()
            .to_string();

        assert!(err.contains("sale-item key"), "{err}");
    }
}

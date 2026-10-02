use ::reqwest::IntoUrl;
use governor::{Quota, RateLimiter};
use http::header::CONTENT_DISPOSITION;
use http::Method;
use indicatif::ProgressStyle;
use nonzero_ext::*;
use pollster::FutureExt as _;
use reqwest::blocking as reqwest;
use serde::Serialize;
use soup::prelude::*;
use std::collections::HashMap;
use std::error::Error;
use std::fs::{self, File};
use std::io::BufReader;
use std::path::Path;
use std::str;
use std::sync::Arc;

pub mod structs;
use crate::api::structs::*;
use crate::cookies;
use crate::util;

pub struct BandcampPage {
    pub download_urls: HashMap<String, CollectionDownload>,
}

#[derive(Clone, Debug)]
pub struct CollectionDownload {
    pub url: String,
    pub is_preorder: bool,
}

/// Body used to paginate through Bandcamp's collection API.
#[derive(Serialize, Debug)]
struct PostCollectionBody<'a> {
    fan_id: &'a str,
    older_than_token: &'a str,
}

const MAX_RETRIES: u8 = 5;

pub struct Api {
    pub client: reqwest::Client,
    ratelimiter: governor::DefaultDirectRateLimiter,
}

impl Api {
    pub fn new(cookies: Vec<cookies::RawCookie>) -> Self {
        let cookie_jar = cookies::fill_cookie_jar(cookies);
        let client = reqwest::ClientBuilder::new()
            .cookie_provider(Arc::new(cookie_jar))
            .build()
            .unwrap();
        let ratelimiter = RateLimiter::direct(Quota::per_second(nonzero!(3u32)));

        Self {
            client,
            ratelimiter,
        }
    }

    fn bc_path(path: &str) -> String {
        format!("https://bandcamp.com/{path}")
    }

    fn request<U: IntoUrl + Copy>(
        &self,
        method: Method,
        url: U,
    ) -> Result<reqwest::Response, Box<dyn Error>> {
        self.request_with_retry(method, url, 0)
    }

    fn request_with_retry<U: IntoUrl + Copy>(
        &self,
        method: Method,
        url: U,
        retry_attempt: u8,
    ) -> Result<reqwest::Response, Box<dyn Error>> {
        self.ratelimiter.until_ready().block_on();

        let response = self.client.request(method.clone(), url.clone()).send()?;
        let status: http::StatusCode = response.status();

        if !status.is_success() {
            if status != http::StatusCode::TOO_MANY_REQUESTS {
                bail!(
                    "request failed with status {status} for url {}",
                    url.as_str()
                );
            }

            if retry_attempt >= MAX_RETRIES {
                bail!(format!("reached maximum retries for url {}", url.as_str()));
            }

            warn!("hit ratelimit from Bandcamp, sleeping for 10 seconds");
            std::thread::sleep(std::time::Duration::from_secs(10));
            return self.request_with_retry(method, url, retry_attempt + 1);
        }

        Ok(response)
    }

    /// Filters the download map by optional artist or album filters.
    fn filter_download_map(
        unfiltered: Option<DownloadsMap>,
        items: &[&Item],
        album: Option<&String>,
        artist: Option<&String>,
    ) -> HashMap<String, CollectionDownload> {
        unfiltered
            .into_iter()
            .flatten()
            .filter_map(|(id, url)| {
                items
                    .iter()
                    .find(|item| format!("{}{}", item.sale_item_type, item.sale_item_id) == id)
                    .filter(|item| artist.is_none_or(|v| item.band_name.eq_ignore_ascii_case(v)))
                    .filter(|item| album.is_none_or(|v| item.item_title.eq_ignore_ascii_case(v)))
                    .map(|item| {
                        (
                            id,
                            CollectionDownload {
                                url,
                                is_preorder: item.is_preorder,
                            },
                        )
                    })
            })
            .collect()
    }

    /// Scrape a user's Bandcamp page to find download urls
    pub fn get_download_urls(
        &self,
        name: &str,
        artist: Option<&String>,
        album: Option<&String>,
    ) -> Result<BandcampPage, Box<dyn Error>> {
        debug!("`get_download_urls` for Bandcamp page '{name}'");

        let body = self.request(Method::GET, &Self::bc_path(name))?.text()?;
        let soup = Soup::new(&body);

        let data_el = soup
            .attr("id", "pagedata")
            .find()
            .expect("Failed to extract data from collection page.");
        let data_blob = data_el
            .get("data-blob")
            .expect("Failed to extract data from element on collection page.");
        let mut fanpage_data: ParsedFanpageData = serde_json::from_str(&data_blob)
            .expect("Failed to deserialise collection page data blob.");
        debug!("Successfully fetched Bandcamp page, and found + deserialised data blob");

        let items = fanpage_data
            .item_cache
            .collection
            .values()
            .collect::<Vec<&Item>>();

        match fanpage_data.fan_data.is_own_page {
            Some(true) => (),
            _ => bail!(format!(
                r#"Failed to scrape collection data for "{name}" (`is_own_page` is false). Perhaps check your cookies, or your spelling."#
            )),
        }

        // TODO: make sure this exists
        let mut collection = Self::filter_download_map(
            fanpage_data.collection_data.redownload_urls.take(),
            &items,
            album,
            artist,
        );

        let skip_hidden_items = true;
        if skip_hidden_items {
            debug!("Skipping hidden collection items");
            // TODO: filter `collection` to remove items that have their value containing a `sale_item_id` from `fanpage_data.item_cache.hidden`
            // collection.iter().filter(|&(k, v)| !fanpage_data.item_cache.hidden.contains_key(k))
        }

        if fanpage_data.collection_data.item_count > fanpage_data.collection_data.batch_size {
            debug!(
                "Too many in `collection_data`, so we need to paginate ({} total)",
                // This should never be `None` thanks to the comparison above.
                fanpage_data.collection_data.item_count.unwrap()
            );
            let rest = self.get_rest_downloads_in_collection(
                &fanpage_data,
                "collection_items",
                album,
                artist,
            )?;
            collection.extend(rest);
        }

        if !skip_hidden_items
            && (fanpage_data.hidden_data.item_count > fanpage_data.hidden_data.batch_size)
        {
            debug!(
                "Too many in `hidden_data`, and we're told not to skip, so we need to paginate ({} total)",
                fanpage_data.hidden_data.item_count.unwrap()
            );
            let rest = self.get_rest_downloads_in_collection(
                &fanpage_data,
                "hidden_items",
                album,
                artist,
            )?;
            collection.extend(rest);
        }

        // let title = soup.tag("title").find().unwrap().text();

        debug!("Successfully retrieved all download URLs");
        Ok(BandcampPage {
            // page_name: title,
            download_urls: collection,
        })
    }

    /// Loop over a user's collection to retrieve all paginated items.
    fn get_rest_downloads_in_collection(
        &self,
        data: &ParsedFanpageData,
        collection_name: &str,
        album: Option<&String>,
        artist: Option<&String>,
    ) -> Result<HashMap<String, CollectionDownload>, Box<dyn Error>> {
        debug!("Paginating results for {collection_name}");
        let collection_data = match collection_name {
            "collection_items" => &data.collection_data,
            "hidden_items" => &data.hidden_data,
            x => bail!(format!(r#"unexpected value for `collection_name`: "{x}""#)),
        };

        let mut last_token = collection_data.last_token.clone().unwrap();
        let mut more_available = true;
        let mut collection = HashMap::new();

        while more_available {
            trace!("More items to collect, looping...");
            // retries
            let body = PostCollectionBody {
                fan_id: &data.fan_data.fan_id,
                older_than_token: &last_token,
            };
            let body = self
                .client
                .post(&Self::bc_path(&format!(
                    "api/fancollection/1/{collection_name}"
                )))
                .json(&body)
                .send()?
                .json::<ParsedCollectionItems>()?;

            let items = body.items.iter().by_ref().collect::<Vec<_>>();
            let redownload_urls =
                Self::filter_download_map(Some(body.redownload_urls), &items, album, artist);
            trace!("Collected {} items", redownload_urls.len());

            collection.extend(redownload_urls);
            more_available = body.more_available;
            last_token = body.last_token;
        }

        debug!("Finished paginating results for {collection_name}");
        Ok(collection)
    }

    pub fn get_digital_item(
        &self,
        url: &str,
        debug: &bool,
    ) -> Result<Option<DigitalItem>, Box<dyn Error>> {
        debug!("Retrieving digital item information for {url}");
        let text = self.request(Method::GET, url)?.text()?;
        let soup = Soup::new(&text);

        let download_page_blob = soup
            .attr("id", "pagedata")
            .find()
            .expect(&format!(
                "could not find `pagedata` element for digital item {url}"
            ))
            .get("data-blob")
            .expect(&format!(
                "could not extract `data-blob` from the pagedata element for digital item {url}"
            ));

        let item_result = std::panic::catch_unwind(|| {
            serde_json::from_str::<ParsedItemsData>(&download_page_blob).unwrap()
        });

        if item_result.is_err() {
            println!("Failed to get item info for {url}.");
            if *debug {
                println!("\n{download_page_blob}\n");
            } else {
                println!("Run with `--debug` to see the full JSON blob.\n")
            }

            bail!(format!("failed parsing {url}"))
        }

        let item = item_result.unwrap().digital_items.first().cloned();

        Ok(item)
    }

    /// Unpack a download into a staging directory.
    ///
    /// Albums arrive as a zip that is expanded in place; singles are kept as
    /// the transferred file. Everything happens inside `staging` so that a
    /// failure never touches the live library.
    fn unpack_into(
        &self,
        staging: &Path,
        stream: &mut reqwest::Response,
        pb: &indicatif::ProgressBar,
        item: &DigitalItem,
        filename: &str,
    ) -> Result<(), Box<dyn Error>> {
        let full_path = staging.join(filename);
        {
            let mut file = File::create(&full_path)?;
            util::copy_with_progress(stream, &mut file, pb)?;
        }

        if !item.is_single() {
            let file = File::open(&full_path)?;
            let reader = BufReader::new(file);
            let mut archive = zip::ZipArchive::new(reader)?;
            archive.extract(staging)?;
            fs::remove_file(&full_path)?;
        }
        Ok(())
    }

    pub fn download_item(
        &self,
        item: &DigitalItem,
        path: &Path,
        audio_format: &str,
        m: &indicatif::MultiProgress,
    ) -> Result<u64, Box<dyn Error>> {
        let download_url = &item
            .downloads
            .as_ref()
            .expect("cannot download a release with no downloads")
            .get(audio_format)
            .unwrap()
            .url;
        let res = self.request(Method::GET, download_url)?;

        // Exact transferred size. Recorded alongside the size Bandcamp
        // advertises, which is the value used for change detection.
        let len = res.content_length().unwrap_or(0);
        let full_title = format!("{} - {}", item.title, item.artist);
        let pb = m.add(
            indicatif::ProgressBar::new(len)
                .with_message(full_title.clone())
                .with_style(
                    ProgressStyle::with_template("{bar:10} ({bytes}/{total_bytes}) {wide_msg}")
                        .unwrap(),
                ),
        );

        let disposition = res.headers().get(CONTENT_DISPOSITION);

        if let None = disposition {
            pb.finish_and_clear();
            return Err(
                format!("could not download {full_title} when using url `{download_url}`").into(),
            );
        }

        // `HeaderValue::to_str` only handles valid ASCII bytes, and Bandcamp
        // chooses to put Unicode into the content-disposition for some reason,
        // so need to handle ourselves.
        let content = str::from_utf8(disposition.unwrap().as_bytes())?;
        // Should probably use a thing to properly parse the content of content disposition.
        let filename = util::slice_string(
            content
                .split("; ")
                .find(|x| x.starts_with("filename="))
                .unwrap(),
            9,
        )
        .trim_matches('"')
        .to_string();

        let target = path;
        let parent = target
            .parent()
            .ok_or_else(|| format!("output path `{}` has no parent directory", path.display()))?;
        let name = target
            .file_name()
            .ok_or_else(|| format!("output path `{}` has no final component", path.display()))?
            .to_string_lossy()
            .into_owned();

        m.suspend(|| debug!("Downloading as `{filename}` for `{}`", path.display()));

        // Download and unpack into a sibling staging directory, then swap it
        // into place. A first download has nothing to lose, but re-downloading
        // an updated release must not destroy the existing copy if the transfer
        // or the unzip fails part-way through.
        //
        // Staging is a sibling of the target rather than a system temp path so
        // that the final rename stays within one filesystem.
        fs::create_dir_all(parent)?;
        let staging = parent.join(format!(".{name}.bandsnatch-staging"));
        if staging.exists() {
            fs::remove_dir_all(&staging)?;
        }
        fs::create_dir_all(&staging)?;

        let mut stream = res;
        m.suspend(|| debug!("Starting download"));

        if let Err(err) = self.unpack_into(&staging, &mut stream, &pb, item, &filename) {
            pb.finish_and_clear();
            let _ = fs::remove_dir_all(&staging);
            return Err(err);
        }

        pb.set_position(len);

        if let Err(err) = util::replace_directory(target, &staging) {
            // Leave no staging debris behind. The existing release is intact.
            let _ = fs::remove_dir_all(&staging);
            pb.finish_and_clear();
            return Err(err.into());
        }
        m.suspend(|| debug!("Replaced `{}` with the new download", path.display()));

        pb.finish_and_clear();
        m.println(format!("(Done) {full_title}"))?;

        Ok(len)
    }
}

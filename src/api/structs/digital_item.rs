use chrono::{Datelike, NaiveDateTime};
use serde::{self, Deserialize};
use std::collections::HashMap;

const FORMAT: &str = "%d %b %Y %T %Z";

// #[derive(Clone, Deserialize, Debug)]
// #[serde(untagged)]
// pub enum ArtId {
//     Str(String),
//     Num(i64),
// }

#[derive(Clone, Deserialize, Debug)]
pub struct DigitalItem {
    pub downloads: Option<HashMap<String, DigitalItemDownload>>,
    pub package_release_date: Option<String>,
    pub title: String,
    pub artist: String,
    pub download_type: Option<String>,
    pub download_type_str: String,
    pub item_type: String,
    /// Numeric Bandcamp item id. Used to derive a stable release identifier when
    /// a release is fetched by URL rather than from the collection listing,
    /// where the sale-item key is unknown.
    #[serde(default)]
    pub item_id: Option<i64>,
    // pub art_id: Option<ArtId>,
}

/// Bandcamp is inconsistent about the JSON type of these fields: the same field
/// arrives as a string for some releases and as a bare number for others.
/// Deserialising into `String` rejects the numeric case and fails the whole
/// item.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum Numericish {
    String(String),
    Integer(i64),
    Float(f64),
}

impl Numericish {
    pub fn as_text(&self) -> String {
        match self {
            Self::String(s) => s.trim().to_string(),
            Self::Integer(i) => i.to_string(),
            Self::Float(f) => format!("{f}"),
        }
    }
}

#[derive(Clone, Deserialize, Debug)]
pub struct DigitalItemDownload {
    /// Advertised archive size. Compared against the size recorded at download
    /// time to detect an artist replacing a release's audio without changing the
    /// purchase ID.
    #[serde(default)]
    pub size_mb: Option<Numericish>,
    // pub description: String,
    // pub encoding_name: String, // Download is chosen by comparing this field and the `format` option.
    pub url: String,
}

impl DigitalItem {
    /// The archive size Bandcamp advertises for `format`, normalised to text.
    ///
    /// `None` means no size was reported, which callers must treat as "cannot
    /// tell" rather than as a change.
    pub fn advertised_size(&self, format: &str) -> Option<String> {
        self.downloads
            .as_ref()?
            .get(format)?
            .size_mb
            .as_ref()
            .map(Numericish::as_text)
    }

    /// The release year, when Bandcamp reports a date that parses.
    ///
    /// A placeholder year is worse than none: `Album (0000)` is read by media
    /// servers as year zero.
    pub fn release_year(&self) -> Option<String> {
        let raw = self.package_release_date.as_deref()?;
        match NaiveDateTime::parse_from_str(raw, FORMAT) {
            Ok(dt) => Some(dt.and_utc().year().to_string()),
            Err(err) => {
                debug!("Failed to parse date time: {}", err);
                None
            }
        }
    }

    pub fn is_single(&self) -> bool {
        (self.download_type.is_some() && self.download_type.as_ref().unwrap() == "t")
            || self.download_type_str == "track"
            || self.item_type == "track"
    }

    // pub fn cover_url(&self) -> String {
    //     let art_id = &self.art_id;
    //     format!("https://f4.bcbits.com/img/a{art_id}")
    // }
}

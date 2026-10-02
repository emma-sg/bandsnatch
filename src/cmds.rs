pub mod debug_collection;
pub mod release;
pub mod run;

/// Audio formats Bandcamp offers, in the order they are presented to the user.
/// Shared so `run` and `release` cannot drift apart.
pub const AUDIO_FORMATS: &[&str] = &[
    "flac",
    "wav",
    "aac-hi",
    "mp3-320",
    "aiff-lossless",
    "vorbis",
    "mp3-v0",
    "alac",
];

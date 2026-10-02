pub mod debug_collection;
pub mod release;
pub mod run;

use crate::{
    api::Api,
    cookies,
    library::AlbumPath,
    lock::{self, RunLock},
    state::{self, State},
};
use clap::Args as ClapArgs;
use std::{
    error::Error,
    fs,
    path::PathBuf,
    sync::Arc,
};

/// Audio formats Bandcamp offers, in the order presented to the user.
/// Shared so `run` and `release` always match.
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

/// Arguments every command that touches the library shares.
///
/// Flattened into each command's own arguments so the flags are declared once.
/// Their ordering constraints are enforced by [`CommonArgs::build`].
#[derive(Debug, ClapArgs)]
pub struct CommonArgs {
    #[arg(short, long, value_name = "COOKIES_FILE", env = "BS_COOKIES")]
    pub cookies: Option<String>,

    /// Enables some extra debug output in certain scenarios.
    #[arg(long, env = "BS_DEBUG")]
    pub debug: bool,

    /// Report what would happen without downloading or writing anything.
    #[arg(short = 'd', long = "dry-run")]
    pub dry_run: bool,

    /// Fail immediately instead of waiting when another run holds the lock.
    #[arg(long, env = "BS_NO_WAIT")]
    pub no_wait: bool,

    /// The folder to extract downloaded releases to.
    #[arg(
        short,
        long = "output-folder",
        value_name = "FOLDER",
        default_value = "./",
        env = "BS_OUTPUT_FOLDER"
    )]
    pub output_folder: String,

    /// Folder layout for each release, relative to the output folder.
    ///
    /// Placeholders: {artist}, {album}, {year}, {id}. A placeholder with no
    /// value is omitted along with any brackets it leaves empty, so a release
    /// with no reported date is not named `Album ()`. Must match the layout a
    /// release was originally downloaded with, or a re-download lands beside the
    /// existing folder instead of replacing it.
    #[arg(
        long,
        value_name = "TEMPLATE",
        default_value = crate::library::DEFAULT_ALBUM_PATH,
        env = "BS_ALBUM_PATH"
    )]
    pub album_path: String,

    /// Path to the state database. Defaults to `.bandsnatch-state.db` inside the
    /// output folder.
    #[arg(long, value_name = "PATH", env = "BS_STATE")]
    pub state: Option<String>,
}

/// Everything a library-touching command needs.
///
/// Held as a single value rather than destructured, so the run lock stays alive
/// for as long as the command runs.
pub struct Context {
    /// Output folder as given, with any leading tilde expanded.
    pub root: PathBuf,
    pub album_path: AlbumPath,
    /// Shared state store; `State` serialises access internally.
    pub state: Arc<State>,
    /// Shared HTTP client and rate limiter, cloned per worker thread.
    pub api: Arc<Api>,
    /// Not read, only held: dropping it releases the lock.
    _lock: RunLock,
}

impl CommonArgs {
    /// Build the shared context from these arguments.
    ///
    /// The steps run in the order the commands depend on: validate the layout,
    /// ensure the output folder exists, resolve paths, take the lock, open the
    /// state store and import any legacy cache, then build the HTTP client.
    /// The import has to come before the first recheck decision, or releases it
    /// covers are treated as unknown and downloaded again.
    pub fn build(&self) -> Result<Context, Box<dyn Error>> {
        // Validate the layout first, so a bad template fails before anything is
        // created, locked, or fetched.
        let album_path = AlbumPath::new(&self.album_path)?;

        let root = PathBuf::from(shellexpand::tilde(&self.output_folder).as_ref());
        match fs::metadata(&root) {
            Ok(metadata) if !metadata.is_dir() => {
                // Plain `Err` rather than `bail!`: `SimpleError`'s `Display`
                // prints as `SimpleError { err: .. }`, which reads badly for a
                // message aimed at the user.
                return Err(format!(
                    "cannot use `{}` as the output folder: it exists and is not a directory",
                    root.display()
                )
                .into());
            }
            Ok(_) => (),
            Err(_) => fs::create_dir_all(&root)?,
        }

        let state_path = self
            .state
            .as_ref()
            .map(|path| PathBuf::from(shellexpand::tilde(path).into_owned()))
            .unwrap_or_else(|| root.join(state::STATE_FILENAME));

        // Lock before opening the database. The lock protects the library, and
        // nothing should write to the state store before mutual exclusion.
        let lock = RunLock::acquire(&lock::lock_path_for(&root), !self.no_wait)?;

        let state = Arc::new(State::open(&state_path)?);
        // Before any RecheckPolicy decision, so imported releases are not
        // treated as unknown and downloaded again.
        let imported = state.import_legacy_cache(&root)?;
        if imported > 0 {
            info!(
                "Imported {imported} entries from the legacy `{}` cache; it is no longer read.",
                state::LEGACY_CACHE_FILENAME
            );
        }

        let cookies_file = self
            .cookies
            .as_ref()
            .map(|path| shellexpand::tilde(path).into_owned());
        let cookies = cookies::get_bandcamp_cookies(cookies_file.as_deref())?;

        Ok(Context {
            root,
            album_path,
            state,
            api: Arc::new(Api::new(cookies)),
            _lock: lock,
        })
    }
}

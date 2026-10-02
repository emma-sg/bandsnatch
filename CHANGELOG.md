# Changelog

All notable changes to Bandsnatch will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Add `--filter` flag to filter downloads by purchase date.
- Track per-release state in a SQLite database instead of the plain-text cache.
  An existing `bandcamp-collection-downloader.cache` is imported on first run.
- Detect releases whose audio was replaced after purchase: the advertised
  archive size and the transferred byte count are recorded, and
  `--recheck-after` / `--recheck-all` re-download only what changed.
- Add the `release` subcommand, which re-downloads a single release by
  sale-item key or download URL, bypassing the state cache.
- Add `--album-path` to configure the output folder layout. For example,
  `--album-path '{artist}/{album} ({year})'` drops the release ID for media
  servers that do not need it for matching.
- Add a Dockerfile and scheduled entrypoint, with `PUID`/`PGID`, `RUN_AT`,
  `INTERVAL`, `RUN_ONCE` and `JITTER`.
- Add a run lock so a scheduled run and a manual re-download cannot race over
  the same library.

### Changed

- Downloads are staged in a sibling directory and swapped into place, so a
  re-download that fails part-way through no longer risks the existing copy.
  Re-downloading a release now replaces its folder in place rather than leaving
  a second ID-suffixed folder beside it.
- `--force` ignores all recorded state rather than only the cache file.

### Fixed

- Make download titles filesystem safe (PR #21).
- Redownload cached preorders when Bandcamp marks them as released (#29).
- Add release IDs to download folder names to avoid duplicately-named releases
  conflicting (#23).
- Stop emitting a fake `(0000)` year for releases that Bandcamp reports no date
  for, which media servers read as year zero.
- Fix a panic in `make_string_fs_safe` when it was passed an empty string.

### Security

- Reduce the `Content-Disposition` download filename to a single safe path
  component. The value comes from the remote server, and both `Path::join` with
  an absolute argument and `..` components could previously make a download
  write - and, for albums, delete - a file anywhere the process could reach. A
  missing or unusable filename is now an error rather than a panic.
- Refuse an `--album-path` template that does not produce a folder strictly
  below the output folder. A template whose placeholders all rendered empty (for
  example `{artist}/{album} ({year})` for a release with an empty artist and
  album) made the output folder itself the download target, which the atomic
  swap then renamed aside and deleted recursively, destroying the library.
- Key the run lock to the output folder instead of the state database path, so
  two runs sharing a library but resolving different `--state` values are still
  mutually exclusive.

## [0.3.3] - 2024-09-07

### Fixed

- Skip over releases that don't have any downloads.
- Warn when failing to get `Content-Disposition` header, indicating the download
  is bad.

## [0.3.2] - 2024-07-16

### Fixed

- Force folders to end with an underscore if they would usually end with a space
  or full stop, due to issues with NTFS (#11).
- Add ratelimiting to mitigate crashes that would occur when attempting dry runs
  sometimes.
- Fix URL parsing error that would occur when using `cookies.txt`.

## [0.3.1] - 2023-10-07

### Fixed

- Fix crash that would occur if `batch_size` or `item_count` were null in a
  user's collection data for whatever reason.

## [0.3.0] - 2023-09-30

### Added

- New `debug-collection` subcommand, helpful for testing weird cases where some
  data is wrong on the user's collection page.

## [0.2.1] - 2023-03-13

### Fixed

- Some more fixes for some releases that don't have the exact same data
  structure as others.

## [0.2.0] - 2023-03-12

### Breaking Change

The previous behaviour of running the download job with the base command has
been moved into its own subcommand `run` in order to accommodate some features I
plan to add in the future.

### Added

- `--dry-run` flag to get a list of releases Bandsnatch would try to download,
  without actually downloading them.
- `--debug` flag to get some extra information in certain circumstances (Might
  be changed to `--verbose` in the future if I change my mind).

### Fixed

- Fix problem where some releases could crash a thread with
  ``missing field `download_type` ``.

### Changed

- New `run` subcommand which replaces the previous functionality of running the
  downloader on the base command.

## [0.1.1] - 2022-10-29

### Added

- Create output folder if it doesn't exist, and warn user if it's a file.

### Fixed

- Replace certain characters in the folder structure which may conflict with
  what filesystems allow (e.g. `:`, `\`, `/`)

### Changed

- Upgrade to `clap` 4.0.

## [0.1.0] - 2022-10-02

Initial public release of Bandsnatch.

[unreleased]: https://github.com/Ovyerus/bandsnatch/compare/v0.3.3...HEAD
[0.3.3]: https://github.com/Ovyerus/bandsnatch/releases/tag/v0.3.3
[0.3.2]: https://github.com/Ovyerus/bandsnatch/releases/tag/v0.3.2
[0.3.1]: https://github.com/Ovyerus/bandsnatch/releases/tag/v0.3.1
[0.3.0]: https://github.com/Ovyerus/bandsnatch/releases/tag/v0.3.0
[0.2.1]: https://github.com/Ovyerus/bandsnatch/releases/tag/v0.2.1
[0.2.0]: https://github.com/Ovyerus/bandsnatch/releases/tag/v0.2.0
[0.1.1]: https://github.com/Ovyerus/bandsnatch/releases/tag/v0.1.1
[0.1.0]: https://github.com/Ovyerus/bandsnatch/releases/tag/v0.1.0

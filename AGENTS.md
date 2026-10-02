# Repository Guidelines

## Project Overview

Bandsnatch is a Rust CLI that downloads a logged-in user's Bandcamp collection in a chosen audio format. Repeated runs consult a local SQLite state database to avoid downloading the same purchases again, and to notice releases whose audio was replaced after purchase. Authentication comes from exported Bandcamp cookies; the tool does not log in for the user.

## Architecture & Data Flow

- `src/main.rs` initializes logging, parses clap subcommands, and dispatches `run`, `release`, or `debug-collection`.
- `src/cmds.rs` declares the subcommands, the flags they share (`CommonArgs`, flattened into each command's own arguments), the audio format list, and `Context::build`, which performs the shared setup in the one order that is correct: validate the album path, ensure the output folder exists, resolve the state path, take the run lock, open the state store (importing any legacy cache before the first recheck decision), then build the HTTP client. Keep that sequence in `build` rather than in a command.
- `src/cmds/run.rs` obtains collection release IDs/URLs, applies a `RecheckPolicy` (`--force` being its unconditional case) and `--limit`, and distributes releases to scoped worker threads.
- `src/cmds/release.rs` re-downloads a single release by sale-item key or download URL, bypassing the state cache for that release.
- `src/api/mod.rs` uses blocking `reqwest`: scrape Bandcamp page data, paginate the collection, resolve digital items, then download the requested format. Albums are ZIP-extracted; single tracks are retained as files. Downloads are unpacked into a staging directory beside the target and swapped into place by `util::replace_directory`, so a failed download cannot destroy an existing release. `src/api/structs/` holds the serde models; destination paths come from `src/library.rs`.
- `src/state.rs` is the SQLite state store (`<output-folder>/.bandsnatch-state.db`, overridable with `--state`): per-release state, the advertised archive size used for change detection, and a one-time import of the legacy `bandcamp-collection-downloader.cache`. It serialises access internally, so callers share it as `Arc<State>` and never hold a lock themselves. Preserve the distinctions it encodes: failed downloads must not be recorded; successful downloads record a fingerprint; missing items and items with no downloads are recorded as skipped; a release seen while Bandcamp still reported it as a preorder stays `preorder` so it is retried once released.
- `src/lock.rs` takes an exclusive `flock` on `<output-folder>/.bandsnatch.lock`, keyed to the output folder because that is the resource being protected, not the state database. Advisory locking is a documented no-op on non-unix.
- `src/library.rs` validates and renders the `--album-path` template, and refuses to produce a path that is not a strict descendant of the output folder.
- `src/util.rs` provides filename sanitization, display sanitization for remote metadata, progress-aware copying, and `replace_directory`. Workers share the API (`Arc<Api>`), state (`Arc<State>`), queue, and results with `Arc`/`Mutex`; there is no async runtime in the active CLI. Most API requests go through a rate-limited retry helper; collection pagination issues its own requests.

## Key Directories

- `src/cmds/`: user-facing command arguments and workflows (`cmds.rs` for the shared flags and context, plus `run.rs`, `release.rs`, `debug_collection.rs`).
- `src/api/`: Bandcamp HTTP/page parsing, downloads, and serde models in `structs/`.
- `test/`: saved download/cache data, **not** an automated test suite; it is ignored by Git.
- `.github/workflows/`: cross-platform build and release CI.

## Development Commands

```sh
cargo build --release             # source build; binary in target/release/
cargo run -- --help               # inspect top-level CLI
cargo run -- run --help           # inspect download options
cargo test                        # standard Rust test harness
cargo fmt --check                 # formatting check
cargo clippy --all-targets        # optional local lint check
nix build                         # CI-style Linux/macOS flake build
```

For a real authenticated run: `cargo run -- run -c ./cookies.json -f flac -o ./Music <username>`. Use `--dry-run --limit 1` for a limited manual check. A dry run still queries Bandcamp and still records releases Bandcamp reports as unavailable, matching the deliberate-skip behaviour above, but it does not record check timestamps. No standalone project scripts were found.

## Code Conventions & Common Patterns

- Rust 2021; modules/functions/fields use `snake_case`, types `PascalCase`. Follow neighboring clap derive `#[arg(..., env = "BS_...")]` and serde model definitions rather than inventing parallel configuration paths.
- Commands and API methods generally return `Result<_, Box<dyn std::error::Error>>`; worker loops match on the result, log a warning and continue rather than aborting the run. Some malformed input/response paths still use `unwrap`/`expect` or skip errors silently: inspect the actual caller before changing failure behavior.
- I/O is synchronous (`reqwest::blocking`); bounded scoped threads and a mutex-backed queue provide parallelism. Keep the distinctions the state store encodes (successful downloads, deliberate skips, failures) and the run lock's guarantee that two invocations never touch one output folder at the same time.
- Cookie loading lives in `src/cookies.rs` (JSON exports or Netscape-style text); output paths are built by `src/library.rs`, and the state database path is resolved in `run.rs`/`release.rs`. Values taken from remote metadata must be sanitized before they reach the filesystem: `util::make_string_fs_safe` for names, `safe_download_filename` for the `Content-Disposition` filename. Avoid committing real cookies, downloaded audio, or generated state files.

## Important Files

`src/main.rs` (entry/dispatch), `src/cmds.rs` (shared flags and setup order), `src/cmds/run.rs` (collection workflow/options), `src/cmds/release.rs` (single-release re-download), `src/api/mod.rs` (Bandcamp requests/downloads), `src/api/structs/digital_item.rs` (release metadata), `src/state.rs` (SQLite state and change-detection fingerprints), `src/library.rs` (album path template), `src/lock.rs` (run lock), `src/cookies.rs` (authentication), `src/util.rs` (sanitization, copy, atomic swap), `README.md` (user-facing usage), `CHANGELOG.md` (Keep a Changelog/SemVer history).

## Runtime/Tooling Preferences

Cargo is the package manager; `Cargo.toml` sets minimum Rust 1.82.0 and `rust-toolchain.toml` selects stable. `.envrc` uses the `flake.nix` dev shell; the flake packages Linux/macOS targets, while Windows CI builds with Cargo/MSVC. This is a compiled CLI, not a Node/Bun project.

## Testing & QA

Inline `#[cfg(test)]` unit tests cover the state store and its legacy import, the recheck/fingerprint decision, the album path template, the atomic directory swap, and the run lock; run them with `cargo test`. CI builds binaries but does not run tests, clippy, or rustfmt (the workflow notes lint/format work as TODO; Nix packaging disables checks). For behavioral changes, exercise the changed CLI path: prefer `--dry-run --limit 1` when network/authentication are available, and add or extend a focused unit test for pure logic. Never treat files in `test/` as a test harness or commit private cookie fixtures.

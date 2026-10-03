# bandsnatch

> A CLI batch downloader for your Bandcamp collection.

Bandsnatch is a Rust tool for downloading all of your Bandcamp purchases all at
once in your desired format, and being able to be run multiple times when you
buy new releases.

This project is heavily inspired by Ezwen's
[bandcamp-collection-downloader](https://framagit.org/Ezwen/bandcamp-collection-downloader),
which I used myself before this, specifically existing to help me learn Rust,
but also to add some improvements over it that I've wanted.

## State of the Project

This tool is still currently a work in progress, so bugs and other weirdness may
occur. If anything weird happens or something breaks, please open an issue about
it with information and reproduction steps if possible. Specifically testing use
of this with large collections would be very helpful to see if there's any areas
that I need to improve in.

If you're a developer poking around in the code, please note that this is my
first proper project written using Rust, so code quality may be subpar,
especially in terms of memory usage. If you have any ideas to improve the
project in general I'd love to hear them.

## Usage

The most basic usage is along the lines of
`bandsnatch run -f <format> <username>`, as it will try to automatically fetch
cookies from a local `cookies.json`<!-- or from Firefox (TODO)-->. But if this
fails you can provide the `-c` option with a path to a cookies file to use.

For more advanced usage, you can run `bandsnatch run -h` to get output similar
to the following.

```
Run Bandsnatch to download your collection

Usage: bandsnatch run [OPTIONS] --format <AUDIO_FORMAT> <USER>

Arguments:
  <USER>  Name of the user to download releases from (must be logged in through cookies) [env: BS_USER=]

Options:
      --album <ALBUM>           [env: BS_ALBUM=]
      --artist <ARTIST>         [env: BS_ARTIST=]
  -f, --format <AUDIO_FORMAT>   The audio format to download the files in [env: BS_FORMAT=] [possible values: flac, wav, aac-hi, mp3-320, aiff-lossless, vorbis, mp3-v0, alac]
  -c, --cookies <COOKIES_FILE>  [env: BS_COOKIES=]
      --debug                   Enables some extra debug output in certain scenarios [env: BS_DEBUG=]
  -d, --dry-run                 Return a list of all tracks to be downloaded, without actually downloading them
  -F, --force                   Ignores all recorded state and downloads every release again [env: BS_FORCE=]
      --recheck-after <DAYS>    Re-check an already-downloaded release for an in-place update once this many days have passed since it was last checked [env: BS_RECHECK_AFTER=]
      --recheck-all             Re-check every downloaded release during this run, downloading only the ones whose advertised size changed. Cheaper than `--force`, which re-downloads unconditionally [env: BS_RECHECK_ALL=]
  -j, --jobs <JOBS>             The amount of parallel jobs (threads) to use [env: BS_JOBS=] [default: 4]
  -n, --limit <LIMIT>           Maximum number of releases to process. Useful for testing [env: BS_LIMIT=]
      --no-wait                 Fail immediately instead of waiting when another run holds the lock [env: BS_NO_WAIT=]
  -o, --output-folder <FOLDER>  The folder to extract downloaded releases to [env: BS_OUTPUT_FOLDER=] [default: ./]
      --album-path <TEMPLATE>   Folder layout for each release, relative to the output folder [env: BS_ALBUM_PATH=] [default: "{artist}/{album} ({year}) [{id}]"]
      --state <PATH>            Path to the state database. Defaults to `.bandsnatch-state.db` inside the output folder [env: BS_STATE=]
  -h, --help                    Print help (see more with '--help')
```

Besides these options, you can also use environment variables with the option
name in `SCREAMING_SNAKE_CASE`, prefixed with `BS_`, so that if set up correctly
you can just run `bandsnatch run` and have it automatically download your
collection to the folder you want.

### Example

```
bandsnatch run -c ./cookies.json -f flac -o ./Music ovyerus
```

This would download my entire music collection into a local "Music" folder, and
also create a `.bandsnatch-state.db` SQLite database recording what was
retrieved; later runs read it to skip releases that are already downloaded.

If a `.cache` file from an earlier version - or from Ezwen's tool, which uses the
same filename - is present, it is imported into the database once and the old
file is left untouched.

### Output folders

Downloads are stored under `<output>/<artist>/<title> (<year>) [<collection-id>]`.
The collection ID keeps releases with the same artist, title, and year in
separate folders, including when downloads run concurrently.

The layout is configurable with `--album-path`, which accepts `{artist}`,
`{album}`, `{year}` and `{id}`:

```
bandsnatch run -f flac -o ./Music --album-path '{artist}/{album} ({year})' you
```

A placeholder with no value is dropped along with any brackets it leaves empty,
so a release Bandcamp reports no date for becomes `Album [p1234]` rather than
`Album () [p1234]`. Path separators inside a title are replaced, so a title
cannot create extra directories or escape the output folder.

A template that produces no folder below the output folder at all - for example
`{artist}` for a release with an empty artist - is refused: naming the output
folder itself would be destructive.

Re-downloading a release replaces its folder in place, once the new copy has
been transferred and unpacked successfully. A download that fails part-way
through leaves the folder you already had untouched.

Changing `--album-path` neither moves existing folders nor re-downloads
anything: state is keyed by release ID, not by path. New downloads land in the
new layout while old folders keep their old names, so use `--force` if you want
a library rewritten consistently.

If an earlier run merged same-named releases into one folder, use
`--force --album "<title>"` and `--artist "<artist>"` with your usual arguments
to download them again; the `{id}` in the default layout keeps them separate.

## Keeping releases up to date

Bandcamp does not change a purchase ID when an artist replaces a release's
audio, and a pre-order turns into a full download without the purchase changing
either. For each release, Bandsnatch records the archive size Bandcamp advertises
for the format you downloaded, alongside the number of bytes it transferred.

- **Pre-orders are re-checked automatically.** A release downloaded while
  Bandcamp still reported it as a pre-order is retried on every run until
  Bandcamp stops saying pre-order, at which point the full release is fetched.
- **`--recheck-after DAYS`** re-checks a release once that many days have passed
  since it was last checked, and re-downloads only those whose advertised size
  changed. This costs one request per due release, and no transfer for releases
  that have not changed.
- **`--recheck-all`** does the same for every release in one pass.
- **`--force`** re-downloads everything unconditionally, without comparing.

A re-check needs the release's download page to read the advertised size, so one
request per due release is unavoidable: the URLs Bandcamp hands out are signed
and rotate, so there is no cheaper stable signal to compare.

## Re-downloading a single release

```
bandsnatch release p1234 -c ./cookies.json -f flac -o ./Music --user you
bandsnatch release 'https://bandcamp.com/download/...' -c ./cookies.json -o ./Music
```

`release` ignores the recorded state for that one release: use it after an artist
re-uploads a track, or when a pre-order ships. The target is either a collection
sale-item key - the value inside the `[p1234]` suffix of each folder, so it is
already visible in your library - or a full download page URL. A key needs
`--user`, because keys can only be found by reading your collection listing; a
URL does not.

It honours the same `--album-path` and the same state database as `run`, so the
release is recorded afterwards and no later run fetches it again.

## Docker

A multi-stage [Dockerfile](./Dockerfile) builds a static musl binary and runs it
under a small supervisor, so the schedule does not have to live on the host.

```
docker run -d \
  --name bandsnatch \
  --restart unless-stopped \
  -e PUID=1000 -e PGID=1000 \
  -e BS_USER=your-bandcamp-username \
  -e BS_COOKIES=/config/cookies.txt \
  -e BS_FORMAT=flac \
  -e BS_OUTPUT_FOLDER=/music \
  -e BS_STATE=/config/state.db \
  -e RUN_AT=3 \
  -v /path/to/appdata/bandsnatch:/config \
  -v /path/to/music:/music \
  bandsnatch:local
```

A prebuilt image is published to GHCR on every push to `main` and on version
tags, so a NAS can pull one instead of building anything:

```
docker pull ghcr.io/emma-sg/bandsnatch:latest
```

`:latest` tracks `main`. Version tags also publish `1`, `1.2` and `1.2.3`, so pin
one of those if you would rather the image only change when you say so. The
package has to be public for an unauthenticated pull; if `docker pull` asks for
credentials, set its visibility in the repository's package settings. On Unraid,
the image goes in the template's Repository field.

The container only supervises the same one-shot CLI, so one-off runs and
on-demand re-downloads work without a second entry point:

```
docker run --rm bandsnatch:local run --dry-run -f flac -o /music you
docker exec bandsnatch release p1234
```

| Variable | Purpose |
| --- | --- |
| `RUN_ONCE` | `1` runs once and exits, for cron or a Kubernetes `CronJob` (the default is to keep scheduling). |
| `RUN_AT` | Local hour (0-23) to run at, daily. |
| `INTERVAL` | Seconds between runs, used when `RUN_AT` is unset. Defaults to `86400`. |
| `JITTER` | Extra random seconds added to the delay, so many containers do not all hit Bandcamp on the hour. |
| `PUID` / `PGID` | Run downloads as these ids, matching the owner of your media share. |
| `CHOWN_RECURSIVE` | `1` takes ownership of every file in the output folder. Off by default because it is slow on a large library. |
| `EXTRA_ARGS` | Extra CLI flags, for anything that has no `BS_` environment variable, e.g. `--recheck-after 7`. |

Every `BS_*` variable maps to the CLI flag of the same name. A failed run is
retried at the next tick rather than stopping the container.

### Unraid

Unraid's Docker manager passes environment variables and path mappings straight
through, so no compose file is needed - set the variables above in the template.
Some settings differ there:

- **Keep the state database off the array**, with
  `BS_STATE=/config/state.db` on your appdata share. It is a SQLite database
  written on every run, and keeping it on cache storage avoids waking spun-down
  array disks.
- Set `PUID`/`PGID` to the owner of your media share rather than letting the
  container run as root.
- One small file stays on the array: the run lock, `.bandsnatch.lock`, inside the
  output folder. It has to be there, because it names the resource it protects -
  moving it to appdata would let two containers sharing a library run at the same
  time. A run that downloads anything is writing to the array anyway.

## Authentication

Because Bandsnatch does not manage logging into Bandcamp itself, you need to
provide it the authentication cookies. For Firefox users, you can extract a
`cookies.json` with the
[Cookie Quick Manager extension](https://addons.mozilla.org/en-US/firefox/addon/cookie-quick-manager/),
and on Chrome, you can use the
[Get cookies.txt LOCALLY extension](https://chromewebstore.google.com/detail/cclelndahbckbenkjhflpdbgdldlbecc),
to extract the cookies in the Netscape format, which Bandsnatch also supports.

If you don't provide the `--cookies` option, Bandsnatch will attempt to
automatically find a file named `cookies.json` or `cookies.txt` in the local
directory and load it.

These cookies are full session credentials: anything that can read the file can
log in as you, including to make purchases. Keep it out of shared directories
and backups, and set it to `0600` (`chmod 600 cookies.txt`). Bandsnatch reads
the file but does not, and cannot, enforce its permissions.

<!-- Failing that, if you use Firefox on Windows or Linux,
bandsnatch will try to automatically load the cookies from there if possible
(TODO). -->

## Installing

Binary builds of Bandsnatch are available on our
[releases page](https://github.com/Ovyerus/bandsnatch/releases) for Windows, Mac
(both ARM & Intel), and Linux (various architectures).

### Nix flake

If you use [Nix](https://nixos.org), Bandsnatch is available as a flake. You can
try it out without installing via `nix run` or `nix shell`:

```
nix run github:ovyerus/bandsnatch -- --help
nix shell github:ovyerus/bandsnatch
```

You can install it permanently with `nix profile install`, or by adding it to
your NixOS/Home Manager configuration.

### Homebrew

`brew install ovyerus/tap/bandsnatch`

### Scoop

```
scoop bucket add ovyerus https://github.com/Ovyerus/bucket
scoop install bandsnatch
```

### AUR

Bandsnatch is also available on the
[AUR](https://aur.archlinux.org/packages/bandsnatch). Either use your favourite
AUR helper, or you can install it manually via the following:

```
git clone https://aur.archlinux.org/bandsnatch.git
cd bandsnatch
makepkg -si
```

### NetBSD (unofficial)

Bandsnatch is also available from the
[official NetBSD repositories](https://pkgsrc.se/net/bandsnatch/), but is not
maintaned by myself.

```
pkgin install bandsnatch
```

### Crate

`cargo install bandsnatch`

### From source

Pull this repository and run `cargo build --release`, and look for the
`bandsnatch` binary in `./target/release/`.

## Developer reference

The [Bandcamp collection JSON field map](docs/collection-json.md) documents the
`pagedata` blob and the fields Bandsnatch reads from it.

## License

This program is licensed under the MIT license (see [LICENSE](./LICENSE) or
https://opensource.org/licenses/MIT).

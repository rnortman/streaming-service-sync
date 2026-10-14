# streaming-service-sync

Move a family's music from Pandora to Spotify without losing what matters:
stations and their seeds, thumbs up **and** thumbs down, Premium playlists and
the on-demand collection.

`sss`, the command-line tool this repository builds, will:

1. **Preserve** — export everything a Pandora account holds into a local data
   repository of plain, deterministic JSON that you own and can keep in
   version control, independent of any streaming service. Every Pandora
   response is stored verbatim before it is parsed.
2. **Match** — find each Pandora track, artist and album on Spotify, deciding
   exact matches in code and handing only the ambiguous ones to an LLM.
3. **Push** — recreate the material in a Spotify account (a private playlist
   per station, playlists, saved library), idempotently, recording what was
   done next to the export.

It never asks for a Pandora password: it uses the session of a browser where
you are already logged in.

## Status

Early development. `sss init`, `sss pandora auth`, `sss pandora export`,
`sss pandora normalize` and `sss status` work; every other subcommand is in
place but reports that it is not implemented yet. Matching and push land in
later releases. See [CHANGELOG.md](CHANGELOG.md).

## Usage

```sh
sss init                     # create the data repository
sss --repo ~/music-data init # ... somewhere other than the default
sss --help                   # every subcommand
```

The data repository defaults to `$XDG_DATA_HOME/streaming-service-sync`
(usually `~/.local/share/streaming-service-sync`); `--repo DIR` selects
another. `init` writes `config.toml`, with every setting at its default and
commented, and a `README.md` describing the layout; it never overwrites
either. Keep the data repository private. Commands that act on one family
member's accounts take `--profile NAME` (lowercase letters, digits, `-` and
`_`).

### Pandora session

`sss pandora auth --profile NAME` stores the Pandora session that profile's
commands use. It needs the `X-AuthToken` value the Pandora web player sends
with each API request:

1. Log in at <https://www.pandora.com> as that family member.
2. Open the browser's developer tools (F12) and select the **Network** tab.
3. Reload the page, then pick any request whose URL contains `/api/`.
4. Under **Request Headers**, copy the value of `X-AuthToken`.
5. Run `sss pandora auth --profile NAME` and paste it at the prompt, or save
   it as the first line of a file and pass `--from-file FILE`.

Copy the request header, not a cookie: the browser's `at` cookie looks
similar but is not the API token, and Pandora refuses it. `sss` checks the
token with one request before storing it, and stores it, mode 0600, under
`$XDG_STATE_HOME/streaming-service-sync/NAME/` (usually
`~/.local/state/…`), never in the data repository. When Pandora later
rejects the stored token, commands say so; run `sss pandora auth` again.

### Pandora export

`sss pandora export --profile NAME` captures that member's stations (with
seeds and thumbs up and down), playlists, on-demand collection and
bookmarks. Every response is stored verbatim under
`profiles/NAME/pandora/raw/<run-id>/` with a `manifest.json` recording each
step's outcome. A run in which every step succeeded is then normalized into
`stations.json`, `playlists.json`, `collection.json`, `bookmarks.json` and
`tracks.json` beside it; `sss pandora normalize` re-derives those files
from the latest complete run without contacting Pandora. `sss status`
prints each profile's latest run, step by step, so you can compare the
playlist and collection counts with what the Pandora app shows.
[docs/pandora-web-api.md](docs/pandora-web-api.md) describes the requests.

## Build from source

Requires [rustup](https://rustup.rs/); the toolchain is pinned in
`rust-toolchain.toml` and installed on first use.

```sh
git clone https://github.com/rnortman/streaming-service-sync.git
cd streaming-service-sync
cargo build --release --locked
./target/release/sss --version
```

## Contributing

`make check` runs the full local check suite (formatting, clippy, tests, a
dependency license audit, and the repository's secret-scanning kit). The pre-commit hook in `.githooks/`
runs it; enable the hooks once per clone:

```sh
git config core.hooksPath .githooks
```

The hooks and `make check` need
[`brenn-scrub`](https://github.com/rnortman/brenn-scrub) and the gitleaks
release it pins, and
[`cargo-deny`](https://github.com/EmbarkStudios/cargo-deny), on `PATH`;
`make rust-check` runs everything but the license audit and the scrub kit
check.

## License

Apache-2.0. See [LICENSE](LICENSE) and [NOTICE](NOTICE).

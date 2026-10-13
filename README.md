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

Early development. `sss init` works; every other subcommand is in place but
reports that it is not implemented yet. Export, matching and push land in
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

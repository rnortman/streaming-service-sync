# Changelog

Notable changes to streaming-service-sync. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Pandora export.** `sss pandora export --profile NAME` captures a family
  member's stations (with their seeds and thumbs up and down), playlists
  and their tracks, on-demand collection and bookmarks. Every Pandora
  response is saved unaltered in a new, dated "run" folder under
  `profiles/NAME/pandora/raw/`, with a `manifest.json` recording how each
  step went. If any step fails, the run is still kept but reported as
  incomplete; a complete run is turned into readable summary files
  (see `sss pandora normalize` below).
- **Pandora sign-in.** `sss pandora auth --profile NAME` stores the
  family member's Pandora session: the `X-AuthToken` value copied from the
  browser's developer tools (the README walks through it). The token is
  checked against Pandora before it is kept and is stored outside the data
  repository, readable only by you. No Pandora password is ever asked for.
  If Pandora is down or rate-limiting, the command says so and asks you to
  try again later rather than blaming the token.
- `sss init` creates the local data repository — the private directory where
  everything exported, matched and synced will be kept — with a commented
  `config.toml` and a README describing its layout. It defaults to
  `~/.local/share/streaming-service-sync`; `--repo DIR` picks another place.
  Existing files are never overwritten.
- `sss status` shows, per profile, how the latest export run went step by
  step, so its counts can be compared with what the Pandora app shows.
- `sss pandora normalize` rebuilds `stations.json`, `playlists.json`,
  `collection.json`, `bookmarks.json` and a deduplicated `tracks.json` from
  the latest complete run, without contacting Pandora. Tracks include the
  ISRC (a standard recording identifier, useful for matching) and explicit
  flag when Pandora provides them. Collection items of an unrecognised kind
  are skipped with a warning; the saved raw responses still hold them.
- Project skeleton: the `sss` command-line tool, released under the
  Apache-2.0 license. The remaining subcommands (`spotify`, `match`,
  `push`, `report`) are defined but not implemented yet.
- Build and CI: `make check` runs the full pre-commit suite; GitHub CI runs
  the Rust checks and a secret scan on every push.
- Git hooks that block commits and pushes containing secrets or personal
  data.

### Removed

- The planned `sss pandora import` subcommand is gone; export talks to
  Pandora directly.

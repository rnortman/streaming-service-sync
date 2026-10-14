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
  try again later rather than blaming the token. The prompt explains where
  in the browser to find the token, and a token that pastes as two lines is
  joined back together (end the paste with an empty line). The token can
  also be piped in on stdin or read with `--from-file`.
- **`--json` on every command.** With `--json`, a command prints exactly
  one JSON document on stdout — its result, or on failure an error object
  whose `kind` (`auth`, `unavailable`, `incomplete`, `input`, `config`,
  `not_implemented`, `other`) tells a script or an LLM what to do next.
  Progress and prompts go to stderr, so every command, signing in
  included, can be driven by another program with the token piped on
  stdin. `docs/json-output.md` describes every document.
- When `--profile` is left out and the only profile is used, `sss` says
  which one on stderr.
- `sss init` creates the local data repository — the private directory where
  everything exported, matched and synced will be kept — with a commented
  `config.toml` and a README describing its layout. It defaults to
  `~/.local/share/streaming-service-sync`; `--repo DIR` picks another place.
  Existing files are never overwritten.
- `sss status` shows, per profile, whether a Pandora session is stored and
  how the latest export run went step by step, so its counts can be
  compared with what the Pandora app shows. A profile is listed as soon as
  `sss pandora auth` has stored its session.
- `--profile` may be left out while only one profile exists; that profile
  is then used.
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

### Changed

- `sss pandora auth` with a token for a different Pandora listener than
  the one already signed in under that profile now replaces the stored
  session and names the previous listener in a warning, instead of
  refusing the token.

### Removed

- The planned `sss pandora import` subcommand is gone; export talks to
  Pandora directly.

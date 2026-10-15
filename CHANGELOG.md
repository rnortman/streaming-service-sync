# Changelog

Notable changes to streaming-service-sync. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Push to Spotify.** `sss push --profile NAME` copies a family member's
  Pandora listening into their Spotify account. It follows the matched
  artists, makes a private playlist for each station and each Pandora
  playlist, and saves the collection and bookmarks to the library. Two
  `[push]` settings in `config.toml` add more: saving thumbs-up as liked
  songs, and keeping a reference playlist of thumbs-down tracks. Start
  with `--dry-run`, which prints the plan and contacts nothing; `--only`
  runs one part. Everything sent is recorded in
  `profiles/NAME/spotify/state.json`, so a second push adds only what is
  new or missing and an interrupted push carries on where it stopped. A
  playlist deleted in the Spotify app is not noticed (Spotify only
  unfollows it); remove its entry from `state.json` to have it made again.
- **Matching to Spotify.** `sss match --profile NAME` finds the Spotify
  counterpart of every track, artist and album the push would send. A
  track is matched automatically when Spotify knows its ISRC (the
  standard recording code), or when title, artist and length agree
  closely with exactly one release. What the search cannot decide goes,
  in batches, to an LLM (a `music-matcher` agent run through the `ccc`
  tool, allowed no tool but file listing) that picks the same recording
  or none; `--no-llm` skips that step. Results go to
  `profiles/NAME/matches/`, as they are made, so runs can be stopped and
  resumed, and are bounded with `--limit N` and `--only`. A failed LLM
  batch is retried on the next run without searching Spotify again.
  `sss match review` lists what needs a person, with search links;
  `sss match set` and `sss match reject` settle an item by hand, and no
  later run overrides them. `[push] linked_playlists` (default off) also
  includes the playlists Pandora made.
- **Spotify sign-in.** `sss spotify auth --profile NAME` prints an address
  to open in the browser where the family member is logged in to Spotify;
  after they approve, `sss spotify auth --callback --profile NAME` takes
  the address the browser landed on (pasted, or with `--from-file`) and
  stores the token outside the data repository, readable only by you. No
  Spotify password or app secret is ever asked for. `sss status` shows
  whether each profile has a token and when it expires.
  `docs/spotify-app-setup.md` walks through registering the Spotify app
  the sign-in needs.
- **Report.** `sss report --profile NAME` writes `profiles/NAME/report.md`,
  a page for the family member: what was exported, how much is matched,
  every item that needs a person (with search links and the commands to
  settle it), the thumbs-down by station with how to hide them in
  Spotify, what has no Spotify counterpart, and what push made, with
  links. It contacts no service.
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
  whose `kind` (`auth`, `unavailable`, `quota`, `incomplete`, `input`,
  `config`, `other`) tells a script or an LLM what to do next.
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
  Apache-2.0 license.
- Build and CI: `make check` runs the full pre-commit suite; GitHub CI runs
  the Rust checks and a secret scan on every push.
- Git hooks that block commits and pushes containing secrets or personal
  data.

### Changed

- A match file whose last line was cut off (by a crash mid-write or a hand
  edit) is no longer repaired silently: every command that reads or adds to
  it stops with an error naming the file, the line and its full text, so
  the line can be checked and removed by hand.
- Playlists you made yourself on Pandora are now recorded with no
  `linked_type` (Pandora reports them as `"None"`); run
  `sss pandora normalize` once per profile to update existing exports.
- `sss pandora auth` with a token for a different Pandora listener than
  the one already signed in under that profile now replaces the stored
  session and names the previous listener in a warning, instead of
  refusing the token.

### Removed

- The planned `sss pandora import` subcommand is gone; export talks to
  Pandora directly.

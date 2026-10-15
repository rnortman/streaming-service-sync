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
`sss pandora normalize`, `sss spotify auth`, `sss match`,
`sss match review|questions|answer|set|reject`, `sss push`, `sss report`,
`sss status` and `sss guide` work, and each also takes `--json` for one machine-readable document on
stdout (see
[Driving `sss` from a script or an LLM](#driving-sss-from-a-script-or-an-llm)).
See [CHANGELOG.md](CHANGELOG.md).

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
`_`); it may be omitted while only one profile exists.

### Pandora session

`sss pandora auth --profile NAME` stores the Pandora session that profile's
commands use. It needs the `X-AuthToken` value the Pandora web player sends
with each API request:

1. Log in at <https://www.pandora.com> as that family member.
2. Open the browser's developer tools (F12) and select the **Network** tab.
3. Reload the page, then pick any request whose URL contains `/api/`.
4. Under **Request Headers**, copy the value of `X-AuthToken`.
5. Run `sss pandora auth --profile NAME` and paste it at the prompt. The
   value may paste as two lines; that is fine. Press Enter on an empty line
   to end the paste. Or save it in a file and pass `--from-file FILE`.

Copy the request header, not a cookie: the browser's `at` cookie looks
similar but is not the API token, and Pandora refuses it. `sss` checks the
token with one request before storing it, and stores it, mode 0600, under
`$XDG_STATE_HOME/streaming-service-sync/NAME/` (usually
`~/.local/state/…`), never in the data repository. When Pandora later
rejects the stored token, commands say so; run `sss pandora auth` again.
A token for a different listener than the profile's stored session (by
Pandora webname) replaces that session, and `sss` names the previous
webname in a warning on stderr; a second listener belongs under a second
`--profile`. To sign in from a script or an LLM rather than at a prompt,
see [Driving `sss` from a script or an LLM](#driving-sss-from-a-script-or-an-llm).

### Pandora export

`sss pandora export --profile NAME` captures that member's stations (with
seeds and thumbs up and down), playlists, on-demand collection and
bookmarks. Every response is stored verbatim under
`profiles/NAME/pandora/raw/<run-id>/` with a `manifest.json` recording each
step's outcome. A run in which every step succeeded is then normalized into
`stations.json`, `playlists.json`, `collection.json`, `bookmarks.json` and
`tracks.json` beside it; `sss pandora normalize` re-derives those files
from the latest complete run without contacting Pandora. `sss status`
prints each profile's stored Pandora session (when it was stored, never the
token) and its latest run, step by step, so you can compare the playlist
and collection counts with what the Pandora app shows; `sss status --json`
gives the same as one JSON document.
[docs/pandora-web-api.md](docs/pandora-web-api.md) describes the requests.

### Spotify sign-in

`sss` reaches Spotify through an app you register once on Spotify's
developer dashboard; [docs/spotify-app-setup.md](docs/spotify-app-setup.md)
walks through it and through putting the app's client ID in `config.toml`.
Then each profile signs in, in two steps:

1. `sss spotify auth --profile NAME` prints an address. Open it in a
   browser where that family member is logged in to Spotify and approve.
2. The browser lands on `https://127.0.0.1/callback?code=…` and shows an
   error such as "can't connect"; that is expected. Copy the whole address
   and run `sss spotify auth --callback --profile NAME`, pasting it and
   pressing Enter on an empty line (or pass `--from-file FILE`).

The token is stored, mode 0600, in the same per-profile state directory as
the Pandora session and renewed automatically; `sss status` shows when it
was stored and when the current access token expires.

### Matching

`sss match --profile NAME` looks up, on Spotify, every track,
artist and album the profile's push would send: thumbs-up tracks, track
and artist seeds, the member's own playlists, the collection and
bookmarks (thumbs-down tracks too when `[push] thumbs_down_playlist =
true`). A track with an ISRC is searched by it first; otherwise, and when
that finds nothing certain, by title, artist and album. A match is taken
automatically only when title, artist and length agree closely and no
other release scores as well; everything else is recorded as `ambiguous`
or `unmatched`. The ambiguous ones then go, in batches of `[matcher]
batch_size`, to an LLM that picks the candidate that is the same
recording, or none. Decisions are
appended to `profiles/NAME/matches/*.jsonl` as they are made, so an
interrupted run loses nothing and a re-run continues; `--only
tracks|artists|albums` and `--limit N` bound a run. Progress goes to
stderr.

The LLM step runs the `music-matcher` agent through
`ccc`, found on `PATH` (or set
`SSS_CCC` to another executable). `sss` writes the agent's definition and
a `ccc` agents config under `$XDG_STATE_HOME/streaming-service-sync/ccc/`
and points every `ccc` call at them; the agent may use no tool but `Glob`
and runs in an empty directory. `[matcher]` in `config.toml` sets the
batch size, the model and effort, how often a malformed reply is sent
back (`max_format_retries`), and a time limit per call (`timeout_s`);
`agents_config` names your own `ccc` agents config instead. Each batch's
prompt and replies are kept in `profiles/NAME/matches/llm/`. A batch that
fails is recorded as `llm_failed` and offered again by the next run, from
the stored candidates, without searching Spotify again. `--no-llm` skips
the step and leaves ambiguous items for a later run.

Without `ccc`, the ambiguous items can be decided from outside, by an LLM
agent or a person, in a loop: `sss match questions` prints the items the
LLM step would be asked about, with every candidate, as one JSON document
and contacts nothing; `sss match answer --from-file decisions.json` takes
back a `{"decisions":[…]}` document of the same form the `music-matcher`
agent answers with, checks the whole of it against the open items, and
records its decisions with method `external`, or records nothing and lists
every problem. Either path can take over from the other.
`sss guide matching` prints the rules and the document formats.

- `sss match review` lists the ambiguous, failed, unmatched and
  low-confidence items with their best three candidates and a Spotify search link.
- `sss match set TR:… <Spotify link, URI or id>` records a match by hand;
  `sss match reject TR:…` records that an item has no Spotify counterpart.
  Neither touches the network, and a later `match` run never overrides
  them.

### Push

`sss push --profile NAME` sends what is matched to the member's Spotify
account: it follows the matched artists, makes one private playlist per
station (its matched seeds, then its thumbs-up by date) and one per
Pandora playlist, saves the collection and bookmarks to the library, and,
when switched on in `[push]`, saves every thumbs-up as a liked song
(`liked_songs`) and keeps the thumbs-down in a reference playlist
(`thumbs_down_playlist`). `playlist_prefix` goes in front of station
playlists' names and `public_playlists = true` makes the playlists public.
Each playlist's description ends with `sss:<key>`, so you can tell the
ones `sss` made from your own.

Start with `sss push --dry-run`: it prints the plan and sends nothing.
`--only artists|stations|playlists|collection|liked|thumbs-down` runs one
part and nothing else. Every change is recorded in
`profiles/NAME/spotify/state.json` as it lands, so pushing again adds only
what is new or missing, and an interrupted push continues where it
stopped. Items you reorder or remove in a playlist `sss` made are left
alone, except that a removed item is added again on the next push.
Deleting a playlist in Spotify only unfollows it, so `sss` cannot tell and
keeps adding to it; to have it made again, remove its entry from
`state.json`.

A playlist reported as `pending` may have been made by an earlier push
that stopped before recording it: the create request got no answer, or an
answer `sss` could not read. `sss` leaves it alone and prints what to
check in Spotify and which `state.json` entry to edit. A create Spotify
refused leaves nothing behind and is tried again on the next push.

### Report

`sss report --profile NAME` writes `profiles/NAME/report.md`, the page a
family member reads, and prints its path. It contacts no service, so run
it whenever you like, typically after `sss match` and again after
`sss push`. It holds:

- what the export captured: stations, thumbs, playlists by kind,
  collection and bookmarks;
- how many tracks, artists and albums are matched, and how;
- every item that needs a person, with its best candidates, a Spotify
  search link and the `sss match set` / `sss match reject` commands to
  settle it;
- the thumbs-down, station by station, with a search link (and the
  Spotify link once matched) and how to hide a song in Spotify;
- seeds Spotify has no counterpart for (genres, unmatched artists) and the
  Pandora-made playlists that were not pushed;
- what push did: links to the playlists it made, how many items it added,
  what the library holds, and what to do about any `pending` playlist.

### Driving `sss` from a script or an LLM

An LLM agent that operates `sss` on a person's behalf starts with
[docs/llm-guide.md](docs/llm-guide.md), which `sss guide` also prints: the
whole job in order, from installing to pushing, and what to leave to the
person. `sss guide matching` prints
[docs/matching-protocol.md](docs/matching-protocol.md), the rules for
deciding a match.

Every command can be run by another program — a shell script, or an LLM
through its shell tool — with nothing typed at a terminal:

- **Input** comes from arguments, files, or stdin. A value read from
  stdin (the Pandora token) is its lines up to an empty line or the end of
  the input, each trimmed and joined, so piping it works:

  ```sh
  echo "$TOKEN" | sss pandora auth --profile alice --json
  ```

  The Spotify sign-in's second step reads the redirected address the same
  way (`… | sss spotify auth --callback --profile alice --json`); the
  first step's address is in its document's `authorize_url`. Approving
  the app in the browser is the one step only a person can do.

  A token piped in by a program is visible to that program, and so to an
  LLM's transcript; secrets are never accepted as command-line arguments,
  which would also put them in shell history.
- **Nothing interactive.** `sss` never reads the terminal device, opens a
  browser, pager or editor, or asks a question; behaviour never depends on
  whether a stream is a terminal.
- **Output.** stdout carries the result and nothing else; prompts,
  progress, warnings and the `error: …` line go to stderr. The exit status
  is 0 on success and 1 on failure.
- **`--json`** (anywhere on the command line) makes stdout exactly one JSON
  document. On success:

  ```json
  {
    "ok": true,
    "command": "init",
    "repo": "/srv/music-data",
    "created": ["/srv/music-data/config.toml", "/srv/music-data/README.md"],
    "kept": []
  }
  ```

  On failure, with the same text on stderr:

  ```json
  {
    "ok": false,
    "command": "pandora export",
    "kind": "auth",
    "error": "no Pandora session for profile alice; run `sss pandora auth`"
  }
  ```

  A usage error (an unknown flag) prints no document: stdout is empty and
  the exit status is 1. `kind` says what to do about a failure:

  | `kind` | Meaning |
  |---|---|
  | `auth` | no stored session, or Pandora refused it or the token given: sign in again |
  | `unavailable` | Pandora or Spotify did not answer after every retry: try again later |
  | `quota` | Spotify's rate or quota limit, or the LLM's usage limit, was reached; what was done is kept: run again later |
  | `incomplete` | an export run ended with failed steps; the run is kept |
  | `input` | what was piped or pasted is unusable |
  | `config` | the data repository, `config.toml`, the profile or the environment needs fixing |
  | `other` | anything else, such as a file that cannot be written |

  [docs/json-output.md](docs/json-output.md) lists every field of every
  command's document.
- **The profile `sss` picks** when `--profile` is left out (the only one)
  is named on stderr, for example
  `profile alice (the only one; pass --profile NAME to choose)`.

## Build from source

Requires [rustup](https://rustup.rs/); the toolchain is pinned in
`rust-toolchain.toml` and installed on first use.

```sh
git clone https://github.com/rnortman/streaming-service-sync.git
cd streaming-service-sync
cargo build --release --locked
./target/release/sss --version
```

`cargo install --path . --locked` builds it and puts `sss` on `PATH`
(in `~/.cargo/bin`).

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

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
`sss pandora normalize` and `sss status` work, and each also takes `--json`
for one machine-readable document on stdout (see
[Driving `sss` from a script or an LLM](#driving-sss-from-a-script-or-an-llm));
every other subcommand is in place but reports that it is not implemented
yet. Matching and push land in later releases. See
[CHANGELOG.md](CHANGELOG.md).

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

### Driving `sss` from a script or an LLM

Every command can be run by another program — a shell script, or an LLM
through its shell tool — with nothing typed at a terminal:

- **Input** comes from arguments, files, or stdin. A value read from
  stdin (the Pandora token) is its lines up to an empty line or the end of
  the input, each trimmed and joined, so piping it works:

  ```sh
  echo "$TOKEN" | sss pandora auth --profile alice --json
  ```

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
  | `unavailable` | Pandora did not answer after every retry: try again later |
  | `incomplete` | an export run ended with failed steps; the run is kept |
  | `input` | what was piped or pasted is unusable |
  | `config` | the data repository, `config.toml`, the profile or the environment needs fixing |
  | `not_implemented` | the command is not built yet |
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

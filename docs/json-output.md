# `--json` output

Every `sss` command accepts `--json` (before or after the subcommand). With
it, stdout carries exactly one JSON document, pretty-printed and followed
by a newline, written when the command ends. Prompts, progress lines and
warnings go to stderr, as does the `error: …` line of a failure; the exit
status is `0` on success and `1` on failure, as without `--json`. A failed
write of a prompt, progress line or warning does not end the command.

Two cases produce no document:

- **A command line `sss` does not accept** (an unknown flag, a missing
  argument). The usage error goes to stderr and the exit status is 1 with
  stdout empty. Under `--json`, empty stdout with exit status 1 means "usage
  error; read stderr".
- **`--help` and `--version`**, which print text.

Keys appear in the order listed below. A field shown as optional is
present with the value `null` when it has no value, unless stated otherwise.
Paths are absolute when the data repository was given as an absolute path.
JSON cannot carry a path that is not UTF-8: a result holding one is
reported as a failure of kind `other`, after the command has done its work.

## Success documents

Every success document starts with `"ok": true` and `"command"`, the command
as typed (`"init"`, `"pandora auth"`, `"pandora export"`, `"pandora
normalize"`, `"status"`). The rest is the command's report.

### `init`

| Field | Type | Meaning |
|---|---|---|
| `repo` | path | the data repository |
| `created` | [path] | files this run created |
| `kept` | [path] | files that existed and were left as they were |

```json
{
  "ok": true,
  "command": "init",
  "repo": "/srv/music-data",
  "created": [
    "/srv/music-data/config.toml",
    "/srv/music-data/README.md"
  ],
  "kept": []
}
```

### `pandora auth`

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | the profile the session was stored for |
| `webname` | string, optional | the Pandora listener the token belongs to, when Pandora names it |
| `source` | `"prompt"` \| `"file"` | where the token came from: stdin or `--from-file` |
| `stored_at` | RFC 3339 UTC | when the session was stored |
| `session_path` | path | the session file in the state directory |
| `replaced_webname` | string, optional | the listener of the session this one replaced, when it was a different one |

The token itself never appears in the document.

```json
{
  "ok": true,
  "command": "pandora auth",
  "profile": "alice",
  "webname": "listener",
  "source": "prompt",
  "stored_at": "2026-10-13T18:02:11Z",
  "session_path": "/home/alice/.local/state/streaming-service-sync/alice/pandora-session.json",
  "replaced_webname": null
}
```

### `pandora normalize`

The counts of what the latest complete run normalized into.

| Field | Type | Meaning |
|---|---|---|
| `run_id` | string | the run normalized |
| `stations` | number | stations |
| `thumbs_up`, `thumbs_down` | number | feedback records over all stations |
| `playlists` | number | playlists |
| `collection_tracks`, `collection_albums`, `collection_artists` | number | on-demand collection items |
| `bookmarks` | number | bookmarked tracks and albums |
| `tracks` | number | distinct tracks in `tracks.json` |

```json
{
  "ok": true,
  "command": "pandora normalize",
  "run_id": "20261013T180500Z",
  "stations": 1,
  "thumbs_up": 2,
  "thumbs_down": 1,
  "playlists": 1,
  "collection_tracks": 1,
  "collection_albums": 0,
  "collection_artists": 0,
  "bookmarks": 0,
  "tracks": 3
}
```

### `pandora export`

A success document means the run is complete: every step `ok`.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | the profile exported |
| `run_id` | string | the new run |
| `run_dir` | path | `profiles/<name>/pandora/raw/<run-id>/` |
| `manifest` | Manifest | the run's `manifest.json`, below |
| `normalized` | object | the `pandora normalize` report of the run (without `ok`/`command`) |

A **Manifest** is:

| Field | Type | Meaning |
|---|---|---|
| `run_id` | string | the run |
| `started`, `finished` | RFC 3339 UTC | when the run started and ended |
| `requests` | number | HTTP responses received, retried attempts included |
| `steps` | object | one Step per name: `stations`, `station_details`, `feedback_up`, `feedback_down`, `playlists`, `playlist_tracks`, `collection`, `bookmarks` |
| `complete` | bool | `true` only when every step is `ok` |

A **Step** is `{ "status": "ok" | "failed", "count": number, "error": string }`,
where `count` is the records captured and `error`, present only on a failed
step, says why.

```json
{
  "ok": true,
  "command": "pandora export",
  "profile": "alice",
  "run_id": "20261013T180500Z",
  "run_dir": "/srv/music-data/profiles/alice/pandora/raw/20261013T180500Z",
  "manifest": {
    "run_id": "20261013T180500Z",
    "started": "2026-10-13T18:05:00Z",
    "finished": "2026-10-13T18:05:09Z",
    "requests": 11,
    "steps": {
      "stations": { "status": "ok", "count": 1 },
      "station_details": { "status": "ok", "count": 1 },
      "feedback_up": { "status": "ok", "count": 2 },
      "feedback_down": { "status": "ok", "count": 1 },
      "playlists": { "status": "ok", "count": 1 },
      "playlist_tracks": { "status": "ok", "count": 1 },
      "collection": { "status": "ok", "count": 1 },
      "bookmarks": { "status": "ok", "count": 0 }
    },
    "complete": true
  },
  "normalized": {
    "run_id": "20261013T180500Z",
    "stations": 1,
    "thumbs_up": 2,
    "thumbs_down": 1,
    "playlists": 1,
    "collection_tracks": 1,
    "collection_albums": 0,
    "collection_artists": 0,
    "bookmarks": 0,
    "tracks": 3
  }
}
```

The `run <id>: <dir>` line and the per-step ledger lines that `export`
prints as it goes are on stderr under `--json`.

### `status`

| Field | Type | Meaning |
|---|---|---|
| `profiles` | [Profile] | every known profile, or the one named by `--profile`; `[]` when there are none |

A **Profile** is:

| Field | Type | Meaning |
|---|---|---|
| `name` | string | the profile |
| `pandora_session` | object, optional | `{ stored_at, webname (optional), source }` of the stored session; never its token |
| `export.latest_run` | object, optional | `{ run_id, manifest }` of the newest run; `manifest` is `null` for a run that did not finish |
| `export.latest_complete_run` | string, optional | the newest run whose manifest says `complete` |
| `matches.tracks`, `matches.artists`, `matches.albums` | object | the count of each match status, over the latest entry per Pandora id |

```json
{
  "ok": true,
  "command": "status",
  "profiles": [
    {
      "name": "alice",
      "pandora_session": {
        "stored_at": "2026-10-13T18:02:11Z",
        "webname": "listener",
        "source": "prompt"
      },
      "export": {
        "latest_run": {
          "run_id": "20261013T180500Z",
          "manifest": { "run_id": "20261013T180500Z", "…": "as in pandora export" }
        },
        "latest_complete_run": "20261013T180500Z"
      },
      "matches": {
        "tracks": {},
        "artists": {},
        "albums": {}
      }
    }
  ]
}
```

## Error documents

```json
{
  "ok": false,
  "command": "pandora export",
  "kind": "auth",
  "error": "no Pandora session for profile alice; run `sss pandora auth`"
}
```

| Field | Type | Meaning |
|---|---|---|
| `command` | string | the command as typed |
| `kind` | string | what to do about it, below |
| `error` | string | the text `sss` also prints to stderr after `error: ` |
| `details` | object | present only where the table under *Details* names one |

### Kinds

| `kind` | Meaning | What a caller does |
|---|---|---|
| `auth` | no stored session, or Pandora refused the session or the token given to sign in | run `sss pandora auth` again with a fresh token |
| `unavailable` | Pandora did not answer, after every retry | wait and run the command again |
| `incomplete` | an export run ended with failed steps | look at `details.failed_steps`; the run is kept |
| `input` | what was pasted or piped is unusable (empty, or not a token) | supply the value again |
| `config` | the data repository is not initialized, `config.toml` is invalid, a profile name is invalid or `--profile` is needed, or neither `$XDG_STATE_HOME`/`$XDG_DATA_HOME` nor `$HOME` is set | fix the setup the message names |
| `not_implemented` | the command is not built yet | — |
| `other` | anything else: a file that cannot be read or written, unparseable data | read `error` |

### Details

Only `pandora export` gives `details`, in two cases.

- **The run ended with failed steps** (`kind` `incomplete`):
  `{ run_id, run_dir, failed_steps: [step name], manifest }`.

  ```json
  {
    "ok": false,
    "command": "pandora export",
    "kind": "incomplete",
    "error": "run 20261013T180500Z is incomplete: failed steps playlists, playlist_tracks; its raw responses are kept",
    "details": {
      "run_id": "20261013T180500Z",
      "run_dir": "/srv/music-data/profiles/alice/pandora/raw/20261013T180500Z",
      "failed_steps": ["playlists", "playlist_tracks"],
      "manifest": { "run_id": "20261013T180500Z", "…": "as in a success document, complete: false" }
    }
  }
  ```

- **The run stopped part-way** (Pandora rejected the session, stopped
  answering, or a file could not be written): `{ run_id, run_dir }`, with
  the `kind` of what stopped it (`auth`, `unavailable` or `other`). The
  run's `manifest.json` is still written, with every unfinished step
  failed.

  ```json
  {
    "ok": false,
    "command": "pandora export",
    "kind": "auth",
    "error": "Pandora rejected the session (HTTP 401); re-run `sss pandora auth`",
    "details": {
      "run_id": "20261013T180500Z",
      "run_dir": "/srv/music-data/profiles/alice/pandora/raw/20261013T180500Z"
    }
  }
  ```

A failure before the run directory exists, a run directory whose path is
not UTF-8, and every failure of the other commands, have no `details`.

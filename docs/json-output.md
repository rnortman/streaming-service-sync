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
normalize"`, `"spotify auth"`, `"match"`, `"match review"`, `"match
questions"`, `"match answer"`, `"match set"`, `"match reject"`, `"push"`,
`"status"`, `"report"`, `"guide"`, `"search"`, `"devices"`, `"player"`,
`"play"`, `"pause"`, `"next"`, `"previous"`, `"queue"`, `"shuffle"`,
`"repeat"`, `"volume"`, `"seek"`). The rest is the command's report.

### Shapes

These objects appear in several documents, always with the same fields.

An **Offered** is a Spotify track, artist or album: `id` (its Spotify URI,
the value `sss match set`, `sss play`, `sss queue` and a decision take)
and `name`; a track's adds `artists`, `album`, `album_type`,
`release_date`, `duration_ms`, `explicit` and `isrc`, an album's
`artists`, `album_type` and `release_date`, each only where Spotify gave
it. A playing podcast episode has only `id` and `name`. There is no
`score`. Match candidates (`match questions`), search results, the playing
item and the queue are all Offered.

A **Device** is a Spotify player the account has open:

| Field | Type | Meaning |
|---|---|---|
| `id` | string | the value `--device` takes |
| `name` | string | as the player calls itself; `--device` takes it too |
| `type` | string | Spotify's device type: `Computer`, `Smartphone`, `Speaker`, … |
| `is_active` | boolean | the player Spotify sends commands to by default |
| `is_restricted` | boolean | Spotify accepts no command for it through its Web API (some speakers) |
| `volume_percent` | number, optional | |

A **PlayerState** is the player as Spotify reports it:

| Field | Type | Meaning |
|---|---|---|
| `device` | Device, optional | the player it is on |
| `is_playing` | boolean | `false` when paused, or loaded but silent |
| `item` | Offered, optional | what is playing |
| `progress_ms` | number, optional | position in `item` |
| `context` | Context, optional | the playlist, album or artist it plays from |
| `shuffle` | boolean | |
| `repeat` | `"off"` \| `"track"` \| `"context"` | |

A **Context** is `{ type, uri }`: `type` is `playlist`, `album`, `artist`
or another kind Spotify names, `uri` its Spotify URI.

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

### `spotify auth`

Without `--callback` (the first step):

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | the profile being signed in |
| `authorize_url` | string | the address to open in the browser |
| `redirect_uri` | string | where the browser lands after approval: `https://127.0.0.1/callback` |

The instructions go to stderr. A sign-in in progress is kept in the
profile's state directory until the second step.

```json
{
  "ok": true,
  "command": "spotify auth",
  "profile": "alice",
  "authorize_url": "https://accounts.spotify.com/authorize?client_id=…&response_type=code&…",
  "redirect_uri": "https://127.0.0.1/callback"
}
```

With `--callback` (the second step, the redirected address on stdin or in
`--from-file`):

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | the profile signed in |
| `token_path` | path | the token file in the state directory |
| `scopes` | [string] | the scopes Spotify granted |
| `expires_at` | RFC 3339 UTC | when the access token expires; it is renewed automatically |

Neither token appears in the document.

```json
{
  "ok": true,
  "command": "spotify auth",
  "profile": "alice",
  "token_path": "/home/alice/.local/state/streaming-service-sync/alice/spotify-token.json",
  "scopes": ["playlist-read-private", "playlist-modify-private", "…"],
  "expires_at": "2026-10-15T13:02:11Z"
}
```

### `match`

`sss match` decides every item that has no match entry yet and appends
each decision to `profiles/<name>/matches/{tracks,artists,albums}.jsonl`
as it is made. Items left `ambiguous`, by this run or an earlier one, and
`llm_failed` ones then go to the LLM in batches, from their stored
candidates; with `--no-llm` they are left as they are. `--limit N` bounds
the items taken this run, searched or sent to the LLM. Progress lines
(`track TR:1 "Song" by Artist: matched (isrc)`, `asking the LLM about 25
tracks...`) go to stderr. `ccc` that cannot be run or cannot resolve the
agent fails the command with kind `config` before any Spotify request;
`ccc`'s usage limit stops it with kind `quota`, every decision before it
kept.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | the profile matched |
| `kinds.tracks`, `kinds.artists`, `kinds.albums` | Kind | what this run decided, per kind; zeros for a kind `--only` left out |
| `stopped` | `"limit"`, optional | `"limit"`: `--limit` items were decided and undecided ones remain |

A **Kind** is:

| Field | Type | Meaning |
|---|---|---|
| `processed` | number | items decided this run |
| `by_status` | object | count per status decided: `matched`, `unmatched`, `ambiguous`, `llm_failed` |
| `by_method` | object | count per method of the matches: `isrc`, `exact`, `llm` (`manual` and `external` decisions are not made by `match`) |
| `llm_batches` | number | LLM batches run (0 with `--no-llm`) |
| `llm_failed_ids` | [string] | Pandora ids left `llm_failed` |
| `unmatched_ids` | [string] | Pandora ids decided `unmatched` |

A run that hits Spotify's quota fails with kind `quota`; the decisions
made before it are kept and the next run continues after them.

```json
{
  "ok": true,
  "command": "match",
  "profile": "alice",
  "kinds": {
    "tracks": {
      "processed": 25,
      "by_status": { "matched": 21, "unmatched": 1, "ambiguous": 3 },
      "by_method": { "isrc": 15, "exact": 6 },
      "llm_batches": 0,
      "llm_failed_ids": [],
      "unmatched_ids": ["TR:123"]
    },
    "artists": { "processed": 0, "by_status": {}, "by_method": {}, "llm_batches": 0, "llm_failed_ids": [], "unmatched_ids": [] },
    "albums": { "processed": 0, "by_status": {}, "by_method": {}, "llm_batches": 0, "llm_failed_ids": [], "unmatched_ids": [] }
  },
  "stopped": "limit"
}
```

### `match review`

Every item (of the `--only` kind) whose latest entry is `unmatched`,
`llm_failed`, `ambiguous`, or a match of `low` confidence.

| Field | Type | Meaning |
|---|---|---|
| `items` | [Item] | in kind order (tracks, artists, albums), then Pandora id |

An **Item** is:

| Field | Type | Meaning |
|---|---|---|
| `pandora_id` | string | `TR:…`, `AR:…`/`CO:…` or `AL:…` |
| `kind` | `"track"` \| `"artist"` \| `"album"` | |
| `title` | string, optional | the track title, or the artist's or album's name |
| `artist` | string, optional | the artist of a track or album |
| `album` | string, optional | a track's album |
| `status` | string | the entry's status |
| `confidence` | `"high"` \| `"medium"` \| `"low"`, optional | |
| `candidates` | [Candidate] | the best three offered, as stored in the entry |
| `search_url` | string | `https://open.spotify.com/search/<title> <artist>`, encoded |

A **Candidate** has `uri`, `name` and `score`; a track's adds `artists`,
`album`, `album_type`, `release_date`, `duration_ms`, `explicit` and `isrc`
where Spotify gave them, an album's `artists`, `album_type` and
`release_date`.

```json
{
  "ok": true,
  "command": "match review",
  "items": [
    {
      "pandora_id": "TR:123",
      "kind": "track",
      "title": "Song",
      "artist": "Artist",
      "album": "Album",
      "status": "ambiguous",
      "confidence": null,
      "candidates": [
        { "uri": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "name": "Song (Live)",
          "artists": ["Artist"], "album": "Live Album", "score": 0.81 }
      ],
      "search_url": "https://open.spotify.com/search/Song%20Artist"
    }
  ]
}
```

### `match questions`

The items whose latest entry is `ambiguous` or `llm_failed` (not set by
hand) among those push would send: what `sss match` would ask its LLM
about. Contacts no service. Without `--json` stdout is the same object
without `ok` and `command`, pretty-printed: the one command whose text
form is JSON.

| Field | Type | Meaning |
|---|---|---|
| `items` | [Question] | in kind order (tracks, artists, albums), then Pandora id; `--only` keeps one kind, `--limit N` the first N |
| `remaining` | number | open items of the selected kinds `--limit` left out; 0 without it |

A **Question** is:

| Field | Type | Meaning |
|---|---|---|
| `item` | string | the Pandora id |
| `kind` | `"track"` \| `"artist"` \| `"album"` | |
| `pandora` | object | a track's `title`, `artist`, `album`, `duration_s`, `explicit_hint`, `flags`, `station_names`, `playlist_names`; an artist's `name`, `station_names`, `station_thumbs_up`; an album's `name`, `artist` |
| `candidates` | [Offered] | every stored candidate, best score first; Offered is under *Shapes* |

```json
{
  "ok": true,
  "command": "match questions",
  "items": [
    {
      "item": "TR:123",
      "kind": "track",
      "pandora": { "title": "Song", "artist": "Artist", "album": "Album",
        "duration_s": 215, "explicit_hint": false, "flags": [],
        "station_names": ["Artist Radio"], "playlist_names": [] },
      "candidates": [
        { "id": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "name": "Song",
          "artists": ["Artist"], "album": "Album", "duration_ms": 215000 }
      ]
    }
  ],
  "remaining": 0
}
```

### `match answer`

Reads a decisions document, `{"decisions": [{"item", "choice",
"confidence", "reason"}]}`, from `--from-file` or stdin (up to an empty
line), checks all of it against the open items of `match questions`, and
appends one entry per decision with method `external`. Any problem
refuses the whole document with kind `input` and nothing recorded (see
*Details*). `--limit N` records the first N decisions of a valid document.

| Field | Type | Meaning |
|---|---|---|
| `recorded` | [Recorded] | the decisions recorded, in document order |
| `not_recorded` | [string] | the Pandora ids of the valid decisions `--limit` left out, in order |

A **Recorded** is `{ item, status, spotify_uri, confidence }`: `status`
`matched` with the chosen URI, or `unmatched` with `spotify_uri` `null`
for a `null` choice.

```json
{
  "ok": true,
  "command": "match answer",
  "recorded": [
    { "item": "TR:123", "status": "matched",
      "spotify_uri": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "confidence": "high" }
  ],
  "not_recorded": []
}
```

A failed write part-way is kind `other`, naming the file and the item it
stopped at; the decisions before it are recorded.

### `match set` and `match reject`

`sss match set <pandora-id> <spotify>` takes a Spotify URI
(`spotify:track:<id>`), an `https://open.spotify.com/track/<id>` address or
a bare id; its type must be the one the Pandora id's prefix calls for
(`TR:` a track, `AR:`/`CO:` an artist, `AL:` an album), else kind `input`.
`sss match reject <pandora-id>` records that the item has no Spotify
counterpart. Neither makes a network request. Their entries have method
`manual`; the other methods an entry can carry are `isrc`, `exact`, `llm`
(`sss match`) and `external` (`sss match answer`).

| Field | Type | Meaning |
|---|---|---|
| `entry` | object | the match entry appended, as in the `matches/*.jsonl` files |

```json
{
  "ok": true,
  "command": "match set",
  "entry": {
    "pandora_id": "TR:123",
    "status": "matched",
    "spotify_uri": "spotify:track:4uLU6hMCjMI75M1A2tKUQC",
    "method": "manual",
    "confidence": "high",
    "candidates": [],
    "queries": [],
    "decided_at": "2026-10-15T12:00:00Z",
    "session": null,
    "reason": null
  }
}
```

### `push`

`sss push` sends the profile's matched items to its Spotify account, in
this order: artist follows, station playlists, Pandora playlists, the
collection and bookmarks, liked songs, the thumbs-down reference playlist.
`--only artists|stations|playlists|collection|liked|thumbs-down` runs that
one part and nothing else. `--dry-run` computes the same plan and makes no
Spotify request at all; it needs no token. Every write is recorded in
`profiles/<name>/spotify/state.json` as it lands, so a second push adds
only what is missing. Progress lines go to stderr.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | the profile pushed |
| `dry_run` | boolean | `true`: the plan, nothing sent |
| `parts` | object | one key per part run: `artists`, `stations`, `playlists`, `collection`, `liked`, `thumbs_down`; a part `--only` left out is absent |

`parts.artists` is:

| Field | Type | Meaning |
|---|---|---|
| `to_follow` | number | matched artists (seeds and collection) not yet known to be followed |
| `followed` | number | followed this run |
| `skipped` | number | already followed: recorded earlier, or found followed |

`parts.stations`, `parts.playlists` and `parts.thumbs_down` are:

| Field | Type | Meaning |
|---|---|---|
| `enabled` | boolean | `false` when the part's `[push]` switch is off (`thumbs_down_playlist`); `playlists` is then empty |
| `playlists` | [Playlist] | one per Spotify playlist the part makes or fills |

A **Playlist** is:

| Field | Type | Meaning |
|---|---|---|
| `key` | string | its key in `state.json`: `ST:…` (or `station:<id>`) for a station, `PL:…` for a Pandora playlist, `thumbs-down` |
| `name` | string | the Spotify playlist's name |
| `action` | `"create"` \| `"update"` \| `"skip"` \| `"pending"` | made now; items missing from one made earlier; nothing to add; left alone (below) |
| `to_add` | number | matched tracks to add |
| `added` | number | added this run (0 in a dry run) |
| `at` | string, optional | a `pending` entry's time |
| `repeats` | number, optional | tracks the Pandora playlist repeats; Spotify gets each once |
| `spotify_id` | string, optional | the Spotify playlist's id, once it exists |

`pending` means an earlier push may have made the playlist without
recording it. `sss` makes no request for it; the text output, and stderr
under `--json`, say what to look for in Spotify and how to edit
`state.json`. The rest of the push goes ahead and the command exits 0.

`parts.collection` and `parts.liked` are:

| Field | Type | Meaning |
|---|---|---|
| `enabled` | boolean | `false` for `liked` when `[push] liked_songs` is off (it is on by default) |
| `to_save` | number | matched items not yet known to be saved |
| `saved` | number | saved this run |
| `skipped` | number | already saved: recorded earlier, or found in the library |

A push that hits Spotify's quota fails with kind `quota`; what landed
before it is recorded and the next push continues. The one exception is
a playlist create that got no answer (kind `unavailable`) or an unreadable
one: Spotify may have made the playlist, so the next push reports it as
`pending` instead of making it again.

```json
{
  "ok": true,
  "command": "push",
  "profile": "alice",
  "dry_run": false,
  "parts": {
    "artists": { "to_follow": 2, "followed": 2, "skipped": 0 },
    "stations": {
      "enabled": true,
      "playlists": [
        { "key": "ST:1", "name": "Station", "action": "create",
          "to_add": 40, "added": 40, "spotify_id": "3cEYpjA9oz9GiPac4AsH4n" }
      ]
    },
    "playlists": { "enabled": true, "playlists": [] },
    "collection": { "enabled": true, "to_save": 5, "saved": 5, "skipped": 0 },
    "liked": { "enabled": true, "to_save": 12, "saved": 12, "skipped": 0 },
    "thumbs_down": { "enabled": false, "playlists": [] }
  }
}
```

### `status`

| Field | Type | Meaning |
|---|---|---|
| `profiles` | [Profile] | every known profile, or the one named by `--profile`; `[]` when there are none |

A **Profile** is:

| Field | Type | Meaning |
|---|---|---|
| `name` | string | the profile |
| `pandora_session` | object, optional | `{ stored_at, webname (optional), source }` of the stored session; never its token |
| `spotify_token` | object, optional | `{ stored_at, expires_at, scopes }` of the stored Spotify token; never its values |
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
      "spotify_token": {
        "stored_at": "2026-10-15T12:02:11Z",
        "expires_at": "2026-10-15T13:02:11Z",
        "scopes": ["playlist-read-private", "…"]
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

### `guide`

Needs no data repository or profile and reads nothing.

| Field | Type | Meaning |
|---|---|---|
| `topic` | `"agent"` \| `"matching"` | the document: `sss guide` or `sss guide matching` |
| `text` | string | the document, exactly as printed without `--json` |

```json
{
  "ok": true,
  "command": "guide",
  "topic": "agent",
  "text": "# Operating `sss` for a person: a guide for an agent\n…"
}
```

### `report`

Writes `profiles/<name>/report.md` from the normalized export, the match
entries and `spotify/state.json`, without contacting any service, and
reports where. Its match table counts the matches of each kind by method:
`isrc`, `exact`, `llm`, `manual` and `external`.

| Field | Type | Meaning |
|---|---|---|
| `path` | string | the report file, replaced on every run |

```json
{
  "ok": true,
  "command": "report",
  "path": "/home/me/.local/share/streaming-service-sync/profiles/alice/report.md"
}
```

### `search`

`sss search QUERY...` sends one Spotify search per query, for tracks,
artists and albums, or for the `--only tracks|artists|albums` kind alone;
`--limit N` (1 to 10, default 10) caps each kind's results. A blank query
is kind `input`; a query whose request fails ends the command with that
failure and no results. Needs no playback scope.

| Field | Type | Meaning |
|---|---|---|
| `results` | [Result] | one per query, in the order given |

A **Result** is `{ query, tracks, artists, albums }`: the query as sent
(trimmed) and three lists of Offered (*Shapes*) in Spotify's order; a kind
not searched, or with no match, is `[]`.

```json
{
  "ok": true,
  "command": "search",
  "results": [
    {
      "query": "Song Artist",
      "tracks": [
        { "id": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "name": "Song",
          "artists": ["Artist"], "album": "Album", "duration_ms": 215000 }
      ],
      "artists": [{ "id": "spotify:artist:0OdUWJ0sBjDrqHygGUXeCF", "name": "Artist" }],
      "albums": []
    }
  ]
}
```

### Playback commands

`devices`, `player`, `play`, `pause`, `next`, `previous`, `queue`,
`shuffle`, `repeat`, `volume` and `seek` act on the profile's Spotify
player. They need the token's playback scopes —
`user-read-playback-state` for `devices` and `player`, it and
`user-modify-playback-state` for the rest — and fail with kind `auth`
before any request when the stored token lacks one. Every command that
changes playback takes `--device NAME|ID`; without it the active player is
used, else the only one Spotify accepts commands for; none, or several,
is kind `player` (see *Details*). It then reads the player's state back,
after a short wait, and reports it.

#### `devices`

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | |
| `devices` | [Device] | the open players; `[]` is a success |

```json
{
  "ok": true,
  "command": "devices",
  "profile": "alice",
  "devices": [
    { "id": "0d1841b0976bae2a3a310dd74c0f3df354899bc8", "name": "Firefox",
      "type": "Computer", "is_active": false, "is_restricted": false,
      "volume_percent": 100 }
  ]
}
```

#### `player`

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | |
| `state` | PlayerState, optional | `null` when no player is active |
| `queue` | [Offered] | what plays next; `[]` when `state` is `null` |

#### `play`, `pause`, `next`, `previous`, `shuffle`, `repeat`, `volume`, `seek`

`sss play` takes any number of tracks, or one playlist, album or artist
(URI, link or id; a bare id is a track); `--station NAME` plays the
playlist `sss push` made for the export's station of that name;
`--item TR:…` plays that track's stored candidates in stored order;
with none of them it resumes. Only one of the three may be given; a
station with no pushed playlist, an item with no stored candidates, or a
target of the wrong type is kind `input`. `--shuffle`/`--no-shuffle` sets
shuffle first; with `--shuffle` a playlist or album starts at a random
track. `shuffle on|off`, `repeat off|track|context`, `volume 0..100` and
`seek SECONDS` set what they name.

Once Spotify has accepted a command that changes playback, the command
has happened: it exits 0 even when the player cannot be read afterwards.
`state` is then `null` and `hint` says to run `sss player`. A command
whose failure says Spotify *may have applied it* (a skip or queue add
that got no answer or a 5xx, which `sss` does not resend) may have
happened too: run `sss player` to see before running it again, whatever
the `kind`.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | |
| `action` | string | the command: `play`, `pause`, … |
| `device` | Device, optional | the player the command went to |
| `state` | PlayerState, optional | read after the command; `null` when none is active, or when it could not be read (then `hint` says so) |
| `hint` | string, optional | the player could not be read after the command: run `sss player`; for `play` also: Spotify loaded the music but is not playing (a browser blocking autoplay: click play once, or allow autoplay for the tab), or would not give a shuffled playlist's or album's length, so it starts at its first track |

```json
{
  "ok": true,
  "command": "play",
  "profile": "alice",
  "action": "play",
  "device": { "id": "0d1841b0976bae2a3a310dd74c0f3df354899bc8", "name": "Firefox",
    "type": "Computer", "is_active": false, "is_restricted": false,
    "volume_percent": 100 },
  "state": {
    "device": { "id": "0d1841b0976bae2a3a310dd74c0f3df354899bc8", "name": "Firefox",
      "type": "Computer", "is_active": true, "is_restricted": false,
      "volume_percent": 100 },
    "is_playing": true,
    "item": { "id": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "name": "Song",
      "artists": ["Artist"], "album": "Album", "duration_ms": 215000 },
    "progress_ms": 1200,
    "context": null,
    "shuffle": false,
    "repeat": "off"
  },
  "hint": null
}
```

#### `queue`

`sss queue TARGET...` adds tracks (URI, link or id) to the queue, in
order; any other target is kind `input` and nothing is queued. A queued
track cannot be taken off the queue. When a request fails after some
tracks were queued, the failure keeps its own `kind` and `details`, and
`error` begins `queued N of M (<uris>), then:` naming the ones that
landed; nothing landed gives the failure alone. When that failure says
Spotify may have applied it, the track after the named ones may be
queued as well.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | |
| `device` | Device, optional | the player the tracks went to |
| `queued` | [string] | the track URIs added, in order |
| `queue` | [Offered] | the queue afterwards, as Spotify gives it |
| `state` | PlayerState, optional | |
| `hint` | string, optional | the queue or the player could not be read after the tracks were added: run `sss player` |

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
| `auth` | no stored Pandora session or Spotify token, or the service refused it or the credential given to sign in | sign in again: `sss pandora auth` with a fresh token, or `sss spotify auth` |
| `unavailable` | Pandora or Spotify did not answer, after every retry | wait and run the command again; a playback command whose `error` says Spotify may have applied it: run `sss player` first |
| `quota` | Spotify's rate or quota limit, or `ccc`'s usage limit, was reached; everything done so far is recorded | run the command again later; it continues where it stopped |
| `incomplete` | an export run ended with failed steps | look at `details.failed_steps`; the run is kept |
| `input` | what was pasted or piped is unusable (empty, or not a token), or an argument is (a Pandora or Spotify id of the wrong form or type, a target of the wrong type, an unknown device, station or item), or a decisions document `sss match answer` refused | supply the value again; for `match answer`, fix the items in `details.problems` and resubmit the whole document |
| `config` | the data repository is not initialized, `config.toml` is invalid, a profile name is invalid or `--profile` is needed, or neither `$XDG_STATE_HOME`/`$XDG_DATA_HOME` nor `$HOME` is set, or Spotify refused access (HTTP 403: the member is not on the app's user allowlist, a scope is missing, or the app owner's Premium lapsed), or `ccc` cannot be run or refuses the matcher agent | fix the setup the message names |
| `player` | the Spotify player cannot take the command: no player open, several open and none active, Premium missing, or Spotify refused the control | relay the message, then run again |
| `other` | anything else: a file that cannot be read or written, unparseable data | read `error` |

### Details

`pandora export` gives `details` in two cases, `match answer` in one, and
the playback commands in one.

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

- **`match answer` refused the decisions document** (`kind` `input`):
  `{ problems: [{ item, problem }] }`, one per problem in document order;
  `item` is the Pandora id concerned, `null` for the document as a whole,
  and `problem` is the line as it appears in `error`. Nothing was recorded.

  ```json
  {
    "ok": false,
    "command": "match answer",
    "kind": "input",
    "error": "2 problems with the decisions; nothing was recorded: item TR:1: choice \"spotify:track:z\" was not offered for it; decision 2 has no \"item\" string",
    "details": {
      "problems": [
        { "item": "TR:1", "problem": "item TR:1: choice \"spotify:track:z\" was not offered for it" },
        { "item": null, "problem": "decision 2 has no \"item\" string" }
      ]
    }
  }
  ```

- **The player cannot take the command** (`kind` `player`): `{ reason,
  devices: [Device] }`. `reason` is Spotify's (`NO_ACTIVE_DEVICE`,
  `PREMIUM_REQUIRED`, `RESTRICTION_VIOLATED`, `DEVICE_NOT_CONTROLLABLE`, …)
  or `sss`'s own `SEVERAL_DEVICES`; `devices` lists the open players when
  `sss` refused before sending anything (no player it could use, or
  several), and is `[]` when Spotify refused.

  ```json
  {
    "ok": false,
    "command": "play",
    "kind": "player",
    "error": "play: several Spotify players are open and none is active (Firefox, Kitchen); pass --device NAME",
    "details": {
      "reason": "SEVERAL_DEVICES",
      "devices": [
        { "id": "0d1841b0976bae2a3a310dd74c0f3df354899bc8", "name": "Firefox",
          "type": "Computer", "is_active": false, "is_restricted": false,
          "volume_percent": 100 },
        { "id": "5fbb3b0e3b6c4d1e9f0a2b7c8d9e0f1a2b3c4d5e", "name": "Kitchen",
          "type": "Speaker", "is_active": false, "is_restricted": false,
          "volume_percent": 40 }
      ]
    }
  }
  ```

A failure before the run directory exists, a run directory whose path is
not UTF-8, and every other failure, have no `details`.

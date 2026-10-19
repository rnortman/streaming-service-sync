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
`"status"`, `"report"`, `"guide"`, `"search"`, `"history"`, `"top"`,
`"playlists"`, `"playlist"`, `"playlist create"`, `"playlist edit"`,
`"playlist add"`, `"playlist remove"`, `"playlist move"`, `"playlist replace"`,
`"playlist delete"`, `"library tracks"`, `"library albums"`,
`"library artists"`, `"library add"`, `"library remove"`, `"devices"`, `"player"`,
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

A **Played** is one finished play from the member's history:

| Field | Type | Meaning |
|---|---|---|
| `played_at` | string | when the play finished, RFC 3339, as Spotify gives it |
| `track` | Offered | what was played |
| `context` | Context, optional | the playlist, album or artist it was played from; `null` when none |

A **Saved** is one entry of the member's library:

| Field | Type | Meaning |
|---|---|---|
| `added_at` | string, optional | when it was saved, RFC 3339, as Spotify gives it; `null` for a followed artist (Spotify gives no date for a follow) |
| `item` | Offered | the track, album or artist |

A **User** is the signed-in Spotify account, an **Owner** a playlist's
owner; both are `{ id, name }`: the Spotify user id, and the display name,
else the id.

A **Playlist** is a playlist on the member's account, or one read by URI:

| Field | Type | Meaning |
|---|---|---|
| `id` | string | its Spotify URI, the value `sss play` and `sss playlist` take |
| `name` | string | |
| `owner` | Owner | its owner; not `user` (`sss playlists`) for one shared with the member |
| `public` | boolean, optional | Spotify's flag; `null` when Spotify gives none |
| `collaborative` | boolean | |
| `total` | number, optional | the item count: a number, or `null` when Spotify does not expose the playlist's items to the member — a playlist they neither own nor collaborate on |
| `description` | string, optional | absent when empty; a playlist `sss push` made ends it with `sss:<key>` |

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

### `youtube import`

`sss youtube import DIR` files an extracted Google Takeout archive as a new
raw run and, when the run is complete, normalizes (`youtube normalize`,
below). A success document means the run is complete: the history was
read and no step failed. A run that cannot copy a file or write its
manifest is removed again, and the error names it.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | the profile imported into |
| `run_id` | string | the new run |
| `run_dir` | path | `profiles/<name>/youtube/raw/<run-id>/` |
| `manifest` | ImportManifest | the run's `manifest.json`, below |
| `normalized` | object | the `youtube normalize` report (without `ok`/`command`) |

An **ImportManifest** is:

| Field | Type | Meaning |
|---|---|---|
| `run_id` | string | the run |
| `started`, `finished` | RFC 3339 UTC | when the run started and ended |
| `source_dir` | path | the directory imported from |
| `files` | [{ `path`, `bytes`, `sha256` }] | every file copied, by its path in the run (`takeout/history/watch-history.json`, `takeout/playlists/…`, `takeout/subscriptions/subscriptions.csv`, `takeout/music (library and uploads)/…`), its size and SHA-256 |
| `steps` | object | one ImportStep per name: `history`, `playlists`, `subscriptions`, `library` |
| `complete` | bool | `true` when `history` is `ok` and no step `failed` |

An **ImportStep** is `{ "status": "ok" | "missing" | "failed", "count":
number, "skipped": number, "error": string }`: `missing` means the archive
had no such file, `failed` that it was there and did not parse (`error`,
present only then, says why); `count` is the records parsed and `skipped`
the rows dropped for a malformed id or time.

```json
{
  "ok": true,
  "command": "youtube import",
  "profile": "alice",
  "run_id": "20261017T120000Z",
  "run_dir": "/srv/music-data/profiles/alice/youtube/raw/20261017T120000Z",
  "manifest": {
    "run_id": "20261017T120000Z",
    "started": "2026-10-17T12:00:00Z",
    "finished": "2026-10-17T12:00:01Z",
    "source_dir": "/home/alice/Downloads/takeout",
    "files": [
      { "path": "takeout/history/watch-history.json", "bytes": 1024, "sha256": "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08" }
    ],
    "steps": {
      "history": { "status": "ok", "count": 3, "skipped": 0 },
      "playlists": { "status": "missing", "count": 0, "skipped": 0 },
      "subscriptions": { "status": "missing", "count": 0, "skipped": 0 },
      "library": { "status": "missing", "count": 0, "skipped": 0 }
    },
    "complete": true
  },
  "normalized": {
    "run_id": "20261017T120000Z",
    "history_runs": ["20261017T120000Z"],
    "records": 3,
    "plays": 3,
    "removed_plays": 0,
    "videos": 2,
    "songs": 2,
    "music_videos": 0,
    "unknown": 0,
    "playlists": 0,
    "subscriptions": 0,
    "artist_channels": 0,
    "library_songs": 0,
    "liked": 0
  }
}
```

The files found, the `run <id>: <dir>` line, warnings (a playlist with no
items file, playlists whose titles share one items file name, a history in
which no entry begins with `Watched `, as in a localized archive) and the
ledger lines are on stderr.

### `youtube normalize`

Re-derives `profiles/<name>/youtube/{history,videos,playlists,subscriptions,library}.json`
from the copies of every complete import run, without reading the archive
again: the history is the union of every run's, the catalogue comes from
the newest archive (the run whose newest history entry is latest). A
profile with no complete run fails with kind `other`; a complete run whose
copies no longer parse fails with kind `config` and leaves the files as
they were.

| Field | Type | Meaning |
|---|---|---|
| `run_id` | string | the run the playlists, subscriptions and library come from |
| `history_runs` | [string] | every run the history is the union of, oldest archive first |
| `records` | number | entries in `history.json`, of every kind (ads, posts, surveys included) |
| `plays` | number | entries that name a video and are not ads |
| `removed_plays` | number | of those, plays whose title and channel Takeout withheld (a removed or private video) and no run supplied |
| `videos` | number | entries in `videos.json`: every video history, a playlist or the library names |
| `songs`, `music_videos`, `unknown` | number | videos of each kind (the rest are `video`) |
| `playlists` | number | playlists |
| `subscriptions` | number | subscribed channels |
| `artist_channels` | number | of those, the ones classified `artist` |
| `library_songs` | number | songs in the music library |
| `liked` | number | liked videos (from a `Liked videos` or `Liked Music` playlist, when the archive has one) |

```json
{
  "ok": true,
  "command": "youtube normalize",
  "run_id": "20261017T120000Z",
  "history_runs": ["20261017T120000Z"],
  "records": 3,
  "plays": 3,
  "removed_plays": 0,
  "videos": 2,
  "songs": 2,
  "music_videos": 0,
  "unknown": 0,
  "playlists": 0,
  "subscriptions": 0,
  "artist_channels": 0,
  "library_songs": 0,
  "liked": 0
}
```

### `youtube api-key`

`sss youtube api-key [--from-file FILE]` reads the YouTube Data API key
from stdin (up to an empty line) or a file, checks it with one
`videos.list` request for a public video, and stores it. It needs no
profile: the key is shared by every profile. A key Google refuses as
invalid fails with kind `input`, a project without YouTube Data API v3
enabled with kind `config`, a spent quota with kind `quota`; nothing is
stored then.

| Field | Type | Meaning |
|---|---|---|
| `stored_at` | RFC 3339 UTC | when the key was stored |
| `key_path` | path | `youtube-api-key.json` at the state root |

The key itself never appears in the document.

```json
{
  "ok": true,
  "command": "youtube api-key",
  "stored_at": "2026-10-17T12:30:00Z",
  "key_path": "/home/alice/.local/state/streaming-service-sync/youtube-api-key.json"
}
```

### `youtube enrich`

`sss youtube enrich [--limit N]` asks the YouTube Data API about every
video in `videos.json` it has not answered yet (at most N with `--limit`),
50 ids per request, each response written to
`profiles/<name>/youtube/api/NNNN-videos-list.json` before it is read, and
then normalizes as `youtube normalize` does. No stored key fails with kind
`auth`; a profile with no YouTube export with kind `config`. A spent
quota fails with kind `quota`: the responses fetched before it are kept
and normalized, and a re-run asks only for the rest.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | the profile enriched |
| `requested` | number | video ids asked about this run |
| `found` | number | of those, the ones the API returned |
| `unavailable` | number | of those, the ones it did not (removed or private; `available: false` in `videos.json`) |
| `remaining` | number | videos still never asked about, left by `--limit` |
| `normalized` | object | the `youtube normalize` report (without `ok`/`command`) |

```json
{
  "ok": true,
  "command": "youtube enrich",
  "profile": "alice",
  "requested": 2,
  "found": 1,
  "unavailable": 1,
  "remaining": 0,
  "normalized": {
    "run_id": "20261017T120000Z",
    "…": "as in youtube normalize"
  }
}
```

The `asking the YouTube Data API about …` line is on stderr under
`--json`.

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
the items taken this run, searched or sent to the LLM; `--source pandora`
or `--source youtube` takes only that source's items (`TR:`/`AR:`/`CO:`/
`AL:`, or `YV:`/`YC:`). Progress lines
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
| `llm_failed_ids` | [string] | item ids left `llm_failed` |
| `unmatched_ids` | [string] | item ids decided `unmatched` |

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
| `items` | [Item] | in kind order (tracks, artists, albums), then item id |

An **Item** is:

| Field | Type | Meaning |
|---|---|---|
| `item_id` | string | `TR:…`, `AR:…`/`CO:…` or `AL:…` from Pandora; `YV:…` (a video, a track) or `YC:…` (a channel, an artist) from YouTube |
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
      "item_id": "TR:123",
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
| `items` | [Question] | in kind order (tracks, artists, albums), then item id; `--only` keeps one kind, `--limit N` the first N |
| `remaining` | number | open items of the selected kinds `--limit` left out; 0 without it |

A **Question** is:

| Field | Type | Meaning |
|---|---|---|
| `item` | string | the item id |
| `kind` | `"track"` \| `"artist"` \| `"album"` | |
| `source` | `"pandora"` \| `"youtube"` | the source the record comes from |
| `record` | object | what the source knows; from Pandora a track's `title`, `artist`, `album`, `duration_s`, `explicit_hint`, `flags`, `station_names`, `playlist_names`, an artist's `name`, `station_names`, `station_thumbs_up`, an album's `name`, `artist`; from YouTube a track's `title`, `artist`, `album`, `duration_s`, `flags`, `youtube_title`, `channel`, `kind`, `plays`, `playlist_names`, an artist's `name`, `channel`, `most_played_titles` (`sss guide matching` describes each field) |
| `candidates` | [Offered] | every stored candidate, best score first; Offered is under *Shapes* |

```json
{
  "ok": true,
  "command": "match questions",
  "items": [
    {
      "item": "TR:123",
      "kind": "track",
      "source": "pandora",
      "record": { "title": "Song", "artist": "Artist", "album": "Album",
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
| `not_recorded` | [string] | the item ids of the valid decisions `--limit` left out, in order |

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

`sss match set <item-id> <spotify>` takes a Spotify URI
(`spotify:track:<id>`), an `https://open.spotify.com/track/<id>` address or
a bare id; its type must be the one the item id's prefix calls for
(`TR:` and `YV:` a track, `AR:`, `CO:` and `YC:` an artist, `AL:` an
album), else kind `input`. `sss match reject <item-id>` records that the item has no Spotify
counterpart. Neither makes a network request. Their entries have method
`manual`; the other methods an entry can carry are `isrc`, `exact`, `llm`
(`sss match`) and `external` (`sss match answer`).

| Field | Type | Meaning |
|---|---|---|
| `entry` | object | the match entry appended, as in the `matches/*.jsonl` files |

A match file line names its item as `item_id`; lines written by `sss`
0.4 and earlier name it `pandora_id`, and both are read.

```json
{
  "ok": true,
  "command": "match set",
  "entry": {
    "item_id": "TR:123",
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
this order: artist follows (Pandora seeds and collection, YouTube artist
channels), station playlists, Pandora and YouTube playlists, the
collection and bookmarks with the YouTube library songs, liked songs, the
YouTube "most played" playlist, the thumbs-down reference playlist.
`--only artists|stations|playlists|collection|liked|most-played|thumbs-down`
runs that one part and nothing else; `--source pandora|youtube` plans only
what that source contributes. `--dry-run` computes the same plan and makes no
Spotify request at all, so it reports a pushed playlist that was deleted in
Spotify as the live run would without the deletion; the live run checks and
leaves it alone. It needs no token. Every write is recorded in
`profiles/<name>/spotify/state.json` as it lands, so a second push adds
only what is missing. A track push added or found in a playlist is
recorded there and never added again, so a track removed from a pushed
playlist — in Spotify or with `sss playlist remove` — stays removed; a
playlist deleted in Spotify or with `sss playlist delete` is left alone
until it is back on the account (Spotify keeps it recoverable for 90 days),
and removing its entry from `state.json` has push make it again from
scratch. Before it reads any playlist it made, a live push asks the
account, in one request per 40, whether those playlists are still on it;
that needs `user-library-read`, `user-follow-read`, `playlist-read-private`
and `playlist-read-collaborative` (kind `auth` before any request when the
token lacks one). Progress lines go to stderr.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | the profile pushed |
| `dry_run` | boolean | `true`: the plan, nothing sent |
| `parts` | object | one key per part run: `artists`, `stations`, `playlists`, `collection`, `liked`, `most_played`, `thumbs_down`; a part `--only` left out is absent |

`parts.artists` is:

| Field | Type | Meaning |
|---|---|---|
| `to_follow` | number | matched artists (seeds, collection, YouTube artist channels) not yet known to be followed |
| `followed` | number | followed this run |
| `skipped` | number | already followed: recorded earlier, or found followed |

`parts.stations`, `parts.playlists`, `parts.most_played` and
`parts.thumbs_down` are:

| Field | Type | Meaning |
|---|---|---|
| `enabled` | boolean | `false` when the part's switch is off (`[push] thumbs_down_playlist`, `[youtube] most_played = 0`); `playlists` is then empty |
| `playlists` | [Playlist] | one per Spotify playlist the part makes or fills |

A **Playlist** is:

| Field | Type | Meaning |
|---|---|---|
| `key` | string | its key in `state.json`: `ST:…` (or `station:<id>`) for a station, `PL:…` for a Pandora playlist, `YP:PL…` for a YouTube playlist, `youtube:most-played`, `thumbs-down` |
| `name` | string | the Spotify playlist's name |
| `action` | `"create"` \| `"update"` \| `"skip"` \| `"pending"` \| `"deleted"` | made now; matched tracks never recorded and not in one made earlier; nothing to add; left alone (below); not on the account: deleted or unfollowed in Spotify or with `sss playlist delete`; left alone until it is back |
| `to_add` | number | matched tracks to add (0 for `deleted`) |
| `added` | number | added this run (0 in a dry run) |
| `at` | string, optional | a `pending` entry's time |
| `repeats` | number, optional | tracks the source playlist repeats; Spotify gets each once |
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
    "most_played": { "enabled": true, "playlists": [] },
    "thumbs_down": { "enabled": false, "playlists": [] }
  }
}
```

### `status`

| Field | Type | Meaning |
|---|---|---|
| `profiles` | [Profile] | every known profile, or the one named by `--profile`; `[]` when there are none |
| `youtube_api_key` | object, optional | `{ stored_at }` of the stored YouTube Data API key; never the key |

A **Profile** is:

| Field | Type | Meaning |
|---|---|---|
| `name` | string | the profile |
| `pandora_session` | object, optional | `{ stored_at, webname (optional), source }` of the stored session; never its token |
| `spotify_token` | object, optional | `{ stored_at, expires_at, scopes, missing_scopes }` of the stored Spotify token; never its values. `missing_scopes` lists the scopes this `sss` signs in with that the token was not granted — `[]` when none; a command that needs one fails with kind `auth` until the member runs `sss spotify auth` again |
| `export.latest_run` | object, optional | `{ run_id, manifest }` of the newest run; `manifest` is `null` for a run that did not finish |
| `export.latest_complete_run` | string, optional | the newest run whose manifest says `complete` |
| `youtube.latest_run` | object, optional | `{ run_id, manifest }` of the newest `youtube import` run; `manifest` (an ImportManifest, as in `youtube import`) is `null` for a run that did not finish |
| `youtube.latest_complete_run` | string, optional | the newest import run whose manifest says `complete` |
| `youtube.videos`, `youtube.enriched`, `youtube.unknown` | number | videos in `videos.json`; of those, the ones the YouTube Data API has been asked about, and the ones of kind `unknown` |
| `youtube.liked` | number | liked videos in `library.json`; 0 when the archive held no liked list (the text form says "no likes in this archive") |
| `matches.tracks`, `matches.artists`, `matches.albums` | object | the count of each match status, over the latest entry per item id |

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
        "scopes": ["playlist-read-private", "…"],
        "missing_scopes": ["user-read-recently-played", "user-top-read"]
      },
      "export": {
        "latest_run": {
          "run_id": "20261013T180500Z",
          "manifest": { "run_id": "20261013T180500Z", "…": "as in pandora export" }
        },
        "latest_complete_run": "20261013T180500Z"
      },
      "youtube": {
        "latest_run": null,
        "latest_complete_run": null,
        "videos": 0,
        "enriched": 0,
        "unknown": 0,
        "liked": 0
      },
      "matches": {
        "tracks": {},
        "artists": {},
        "albums": {}
      }
    }
  ],
  "youtube_api_key": null
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

Writes `profiles/<name>/report.md` from the normalized exports of both
sources, the match entries and `spotify/state.json`, without contacting
any service, and reports where. Its match table counts the matches of each kind by method:
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

### `history`

`sss history [--limit N]` reads the member's last finished plays (`--limit`
1 to 50, default 50), newest first. Spotify keeps only the last 50
completed tracks: a track playing now is not in it until it ends, and
podcast episodes never are. Needs `user-read-recently-played`; a token
without it is kind `auth` before any request.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | |
| `items` | [Played] | newest first; `[]` is a success |

```json
{
  "ok": true,
  "command": "history",
  "profile": "alice",
  "items": [
    { "played_at": "2026-10-16T13:02:11.483Z",
      "track": { "id": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "name": "Song",
                 "artists": ["Artist"], "album": "Album", "duration_ms": 215000 },
      "context": { "type": "playlist", "uri": "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M" } }
  ]
}
```

### `top`

`sss top tracks|artists [--range short|medium|long] [--limit N]` reads
Spotify's ranking of the member's most played tracks or artists over
about the last four weeks, about the last six months (the default) or
several years; `--limit` 1 to 50, default 50. Needs `user-top-read`; a
token without it is kind `auth` before any request.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | |
| `kind` | `"tracks"` \| `"artists"` | |
| `range` | `"short"` \| `"medium"` \| `"long"` | |
| `items` | [Offered] | ranked, first place first; an artist has only `id` and `name`; `[]` is a success |

```json
{
  "ok": true,
  "command": "top",
  "profile": "alice",
  "kind": "artists",
  "range": "medium",
  "items": [{ "id": "spotify:artist:0OdUWJ0sBjDrqHygGUXeCF", "name": "Artist" }]
}
```

### `playlists`

`sss playlists` reads the account, then every playlist on it — the
member's own and those they follow — in Spotify's order. Needs
`playlist-read-private` and `playlist-read-collaborative`; a token without
either is kind `auth` before any request.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | |
| `user` | User | the account; a playlist whose `owner.id` differs is shared with the member |
| `playlists` | [Playlist] | `[]` is a success |

```json
{
  "ok": true,
  "command": "playlists",
  "profile": "alice",
  "user": { "id": "alice-spotify", "name": "Alice" },
  "playlists": [
    { "id": "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M", "name": "Morning",
      "owner": { "id": "alice-spotify", "name": "Alice" }, "public": false,
      "collaborative": false, "total": 42 },
    { "id": "spotify:playlist:4uLU6hMCjMI75M1A2tKUQC", "name": "Road trip",
      "owner": { "id": "bob-spotify", "name": "Bob" }, "public": true,
      "collaborative": false, "total": 17, "description": "Songs for the drive" }
  ]
}
```

### `playlist`

`sss playlist TARGET...` reads each playlist named, in the order given:
its details, then its contents. A target is, trimmed:

- a playlist URI (`spotify:playlist:<id>`) or an
  `https://open.spotify.com/playlist/<id>` address — recognised by the
  `spotify:`, `http://` or `https://` prefix; another kind or a malformed
  address is kind `input`;
- never a bare 22-character id: kind `input`;
- the key of a playlist `sss push` made — `ST:…`, `station:…`, `PL:…`,
  `YP:…` (no whitespace), `thumbs-down` or `youtube:most-played` — looked up
  in the profile's `spotify/state.json`: a created entry gives its
  playlist; a pending one, or none, is kind `input` ("… has no Spotify
  playlist yet; run `sss push --only …`", "… has no Spotify playlist; run
  `sss push`") with no request made. A key-shaped text is never taken as a
  name; a playlist literally named `ST:1` is reachable by URI;
- otherwise the name of a playlist on the account, own or followed,
  compared trimmed and case-insensitively against one listing of the
  account's playlists (shared by every name in the call). None of that
  name, or several, is kind `input`, the message listing up to 20 names,
  or each match's URI and owner.

Every target is parsed before any request. Spotify shows a playlist's
items only to its owner and collaborators, so a playlist the member only
follows is reported with `items: null` and the command goes on. A playlist
Spotify does not know ends the command with kind `other`, quoting Spotify.
Needs the same scopes as `playlists`.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | |
| `playlists` | [Contents] | one per target, in the order given |

A **Contents** is `{ playlist, pushed_as, items, unavailable }`: the
Playlist (*Shapes*); `pushed_as`, the `state.json` key of the push that
made the playlist (`"ST:123"`), or `null`; `items`, its tracks and
episodes as Offered in playlist order, or `null` for a playlist whose
`total` is `null` — one the member neither owns nor collaborates on; and
`unavailable`, the entries `items` leaves out — removed items and ones
that read as neither track nor episode — that is `total` minus the length
of `items`, or `null` when `items` is. A local file is an Offered whose
`id` is `spotify:local:…` and counts as shown. When `unavailable` is
above zero, item `n` of `items` is not Spotify's position `n`; the text
ends the list with `(n unavailable items not shown)`.

```json
{
  "ok": true,
  "command": "playlist",
  "profile": "alice",
  "playlists": [
    { "playlist": { "id": "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M", "name": "Morning",
                    "owner": { "id": "alice-spotify", "name": "Alice" }, "public": false,
                    "collaborative": false, "total": 1 },
      "pushed_as": "ST:123",
      "items": [
        { "id": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "name": "Song",
          "artists": ["Artist"], "album": "Album", "duration_ms": 215000 }
      ],
      "unavailable": 0 },
    { "playlist": { "id": "spotify:playlist:4uLU6hMCjMI75M1A2tKUQC", "name": "Road trip",
                    "owner": { "id": "bob-spotify", "name": "Bob" }, "public": true,
                    "collaborative": false, "total": null },
      "pushed_as": null,
      "items": null,
      "unavailable": null }
  ]
}
```

### `playlist create`

`sss playlist create NAME [--description TEXT] [--public]
[--collaborative] [--duplicate] [--dry-run]` makes a playlist of the
member's, private unless `--public`. `NAME` is trimmed and must be 1 to
100 characters, else kind `input`; a description over 300 characters is
cut, with a warning on stderr. `--collaborative` cannot be combined with
`--public` (Spotify keeps a collaborative playlist private). The command
reads the account and its playlists first: when the member already owns a
playlist of that name (compared trimmed and case-insensitively) it is
kind `input`, naming the URIs, unless `--duplicate`; a followed playlist
of that name does not count. Needs `playlist-modify-private`,
`playlist-modify-public`, `playlist-read-private` and
`playlist-read-collaborative`, as every `playlist` write does; a token
without one is kind `auth` before any request.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | |
| `dry_run` | boolean | `true`: nothing was written |
| `to_create` | object | `{ name, public, collaborative, description }`, what is sent (`description` is `""` when none was given) |
| `created` | Playlist, optional | the new playlist; `null` in a dry run |
| `existing` | [Playlist] | the member's own playlists of that name; empty unless `--duplicate` let the create through |
| `snapshot_id` | string, optional | after a live run, the version Spotify gave the playlist; absent in a dry run |

```json
{
  "ok": true,
  "command": "playlist create",
  "profile": "alice",
  "dry_run": false,
  "to_create": { "name": "Kids Songs", "public": false, "collaborative": false, "description": "" },
  "created": { "id": "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M", "name": "Kids Songs",
               "owner": { "id": "alice-spotify", "name": "Alice" }, "public": false,
               "collaborative": false, "total": 0 },
  "existing": [],
  "snapshot_id": "AAAAAQ"
}
```

### `playlist edit`, `playlist add`, `playlist remove`, `playlist move`, `playlist replace`, `playlist delete`

Each takes one playlist, in the grammar of `playlist` (URI, address,
ledger key or name), then reads the account and the playlist's details,
and checks what Spotify would refuse before any write. Items are given as
for `library add` (*`library add`, `library remove`*) but must be tracks:
another kind, an episode or a local file is kind `input`. Items and the
ledger key are resolved before any request, so a bad item or an unknown
key makes none. Every one takes `--dry-run`: every read, no write. Every
report has `profile`, `dry_run`, `playlist` (the Playlist, *Shapes*) and
`pushed_as` (the `state.json` key of the push that made it, or `null`);
those given items have `resolved` and `duplicates_dropped` as `library
add` does; those that read the contents have `unavailable` (as in
`playlist`); after a live run `snapshot_id` is the last version Spotify
returned (`null` when it returned none), absent in a dry run.

- **`playlist edit PLAYLIST [--name N] [--description T]
  [--public|--private] [--collaborative|--no-collaborative]`** — at least
  one flag. Only the playlist's owner can change its details (else kind
  `input`); a change that would leave the playlist both public and
  collaborative (`--collaborative` on a public playlist without
  `--private`, or `--public` on a collaborative one without
  `--no-collaborative`) is kind `input`. A pushed playlist's description
  keeps one `sss:<key>` marker (a description read back from the
  playlist, marker included, is sent with the marker once), and `--name`
  renames its `state.json` entry once Spotify has the change. Adds `changes` (what is sent: only
  the details asked for) and `before` (the Playlist as read); `playlist`
  is the details with the changes applied. Requests: `me`, the details,
  `PUT playlists/{id}`.
- **`playlist add PLAYLIST [ITEM...] [--from-file F] [--at N]
  [--duplicates]`** — appends the tracks, or inserts them before position
  `N` (1-based, as `sss playlist` numbers them; 1 to the item count plus
  one, else kind `input`), skipping the ones the playlist already holds
  unless `--duplicates`. The account must own the playlist or the
  playlist must be collaborative, else kind `input`. `--at` on a playlist
  with `unavailable` above zero is kind `input`: positions there are not
  Spotify's. Adds `to_add` (the URIs added, or that would be, in order),
  `already_present` and `position` (`N`, or `null`). One request per 100
  tracks; a failure after one landed carries `details.done`.
- **`playlist remove PLAYLIST [ITEM...] [--from-file F]`** — every
  occurrence of each track leaves the playlist, under the same ownership
  rule. Adds `to_remove` (`[{ item: Offered, occurrences }]`, the tracks
  the playlist holds, as its contents show them) and `not_present` (the
  URIs it does not). Nothing to remove is a success. The removal carries
  the version the details read gave; Spotify applies it to the playlist
  as it is when the request arrives. One request per 100 tracks; a
  failure after one landed carries `details.done`.
- **`playlist move PLAYLIST --from N [--count K] (--before M |
  --to-end)`** — the `K` items (default 1) from position `N` move before
  the item at position `M`, or to the end; positions are 1-based, as `sss
  playlist` numbers them. The range must lie within the playlist, `M`
  within 1 to the item count plus one and outside the range, else kind
  `input`; on a playlist with `unavailable` above zero it is kind `input`
  before any other check (positions there are not Spotify's; reorder it
  with `replace`). Same ownership rule. Adds `from` (`N`), `count` (`K`),
  `to_move` (`[Offered]`, the items of the range) and `before` (the
  Offered they land before, `null` for the end). One request, carrying the
  version the details read gave; it is never sent twice, so one that got
  no answer is kind `unavailable` and may have applied: read the playlist
  before running it again.
- **`playlist replace PLAYLIST [ITEM...] [--from-file F]`** — the
  playlist's contents become exactly the tracks given, in order; a track
  given twice is held twice, and no tracks empties the playlist. Same
  ownership rule. Adds `before` (the item count now), `after` (the
  count after), `removed` (`[Offered]`, every current item whose URI is
  not in the list), `added` (the URIs of the list the playlist does not
  hold, each once) and `reordered` (whether the items kept change their
  relative order); `unavailable` entries are erased too. It has no
  `duplicates_dropped`. A local file in the playlist cannot be given back
  through the API, so it is in `removed`. One request for the first 100
  tracks, then one per further 100; a failure after one landed carries
  `details.done`.
- **`playlist delete PLAYLIST`** — removes the playlist from the account:
  Spotify has no other delete, so the member's own playlist is deleted
  (`action: "deleted"`; Spotify keeps it recoverable for 90 days at
  https://www.spotify.com/account/recover-playlists/) and another's is
  unfollowed (`action: "unfollowed"`). A playlist the account does not
  hold is kind `input`. Adds `action` and `items` (the item count, or
  `null`). `state.json` is not changed. Needs the `library add` scopes of
  a playlist (*`library add`, `library remove`*).

```json
{
  "ok": true,
  "command": "playlist add",
  "profile": "alice",
  "dry_run": true,
  "playlist": { "id": "spotify:playlist:37i9dQZF1DXcBWIGoYBM5M", "name": "Kids Songs",
                "owner": { "id": "alice-spotify", "name": "Alice" }, "public": false,
                "collaborative": false, "total": 1 },
  "pushed_as": null,
  "resolved": { "TR:123": "spotify:track:6rqhFgbbKwnb9MLmUQDhG6" },
  "unavailable": 0,
  "to_add": ["spotify:track:6rqhFgbbKwnb9MLmUQDhG6"],
  "already_present": ["spotify:track:4uLU6hMCjMI75M1A2tKUQC"],
  "position": null
}
```

### `library tracks`, `library albums`, `library artists`

`sss library tracks|albums|artists [--limit N]` lists the saved tracks,
the saved albums or the followed artists in Spotify's order (most
recently saved first), one request per 50 entries; `--limit N` (1 or
more) stops after the first `N`. Needs `user-library-read` and
`user-follow-read`; a token without either is kind `auth` before any
request.

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | |
| `kind` | `"tracks"` \| `"albums"` \| `"artists"` | which list |
| `items` | [Saved] | `[]` is a success |

```json
{
  "ok": true,
  "command": "library tracks",
  "profile": "alice",
  "kind": "tracks",
  "items": [
    { "added_at": "2026-10-16T13:02:11Z",
      "item": { "id": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "name": "Song",
                "artists": ["Artist"], "album": "Album", "duration_ms": 225000 } }
  ]
}
```

### `library add`, `library remove`

`sss library add [ITEM...] [--from-file FILE] [--dry-run]` saves tracks
and albums and follows artists and playlists; `sss library remove` with
the same arguments removes and unfollows them. An item is a Spotify URI
(`spotify:track:…`, `spotify:album:…`, `spotify:artist:…`,
`spotify:playlist:…`), an open.spotify.com address, a bare 22-character
track id, or a source item id (`TR:…`, `YV:…`, `AR:…`, `AL:…`, …) whose
latest match is `matched`, which stands for the matched URI. `--from-file
FILE` (`-` for stdin, read to its end) adds one item per line after those
on the command line; lines are trimmed, and blank lines and lines starting
with `#` skipped. Every item is resolved before any request: the first one
that does not resolve — an item id whose match is not `matched` or that
has none, a kind the command does not take (an episode, a show, a local
file), a malformed value — is kind `input` and nothing is sent; a missing
file is kind `other`. No items at all is kind `input`. An item given more
than once is kept at its first occurrence.

The command then asks Spotify which items the library holds (one request
per 40) and writes only what changes: `add` the ones not held, `remove`
the ones held, one request per 40. `--dry-run` makes the check and no
write. Needs `user-library-modify`, `user-follow-modify`,
`user-library-read` and `user-follow-read`, and, when an item is a
playlist, `playlist-modify-private`, `playlist-modify-public`,
`playlist-read-private` and `playlist-read-collaborative` as well; a
token without one is kind `auth` before any request. Neither command
reads or writes `spotify/state.json`. A write that fails after an
earlier chunk landed carries `details.done` (*Details*).

| Field | Type | Meaning |
|---|---|---|
| `profile` | string | |
| `dry_run` | boolean | `true`: nothing was written |
| `resolved` | object, optional | each source item id given and the URI it stood for; absent when none was given |
| `duplicates_dropped` | number, optional | items given more than once; absent when none was |
| `to_save` (`add`) | [string] | the URIs not in the library, in the order given: what was saved, or would be |
| `already_saved` (`add`) | [string] | the URIs already in it, left as they are |
| `to_remove` (`remove`) | [string] | the URIs in the library, in the order given: what was removed, or would be |
| `not_saved` (`remove`) | [string] | the URIs that were not in it |

```json
{
  "ok": true,
  "command": "library add",
  "profile": "alice",
  "dry_run": false,
  "resolved": { "TR:123": "spotify:track:4uLU6hMCjMI75M1A2tKUQC" },
  "to_save": ["spotify:track:4uLU6hMCjMI75M1A2tKUQC"],
  "already_saved": ["spotify:album:6akEvsycLGftJxYudPjmqK"]
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
`--item TR:…` (or `YV:…`) plays that track's stored candidates in stored order;
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
| `incomplete` | an export run ended with failed steps, or a `youtube import` run with failed steps or without the history | look at `details.failed_steps` and `details.manifest`; the run is kept |
| `input` | what was pasted or piped is unusable (empty, or not a token), or an argument is (a Pandora or Spotify id of the wrong form or type, a target of the wrong type, a `playlist` target of the wrong form or type, a playlist name that matches none or several, a ledger key whose playlist is pending or absent, a playlist whose contents or details the account may not change or that is not on the account, a playlist name already owned, a position outside the playlist or on a playlist with unavailable items, an item id without a recorded match, an unknown device, station or item, an empty item list), a `youtube import` directory that is not one, holds no Takeout files, holds the history only as HTML or one file twice, or a decisions document `sss match answer` refused | supply the value again; for `match answer`, fix the items in `details.problems` and resubmit the whole document |
| `config` | the data repository is not initialized, `config.toml` is invalid, a profile name is invalid or `--profile` is needed, the profile has no export (`match`, `push`, `report`) or no Pandora export (`play --station`), or neither `$XDG_STATE_HOME`/`$XDG_DATA_HOME` nor `$HOME` is set, or Spotify refused access (HTTP 403: the member is not on the app's user allowlist, a scope is missing, or the app owner's Premium lapsed), or `ccc` cannot be run or refuses the matcher agent | fix the setup the message names |
| `player` | the Spotify player cannot take the command: no player open, several open and none active, Premium missing, or Spotify refused the control | relay the message, then run again |
| `other` | anything else: a file that cannot be read or written, unparseable data | read `error` |

### Details

`pandora export` gives `details` in two cases, `youtube import` and `match
answer` in one each, the playback commands in one, and every write of
several requests in one.

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

- **A `youtube import` run is not complete** (`kind` `incomplete`): the
  same `{ run_id, run_dir, failed_steps, manifest }`, with an
  ImportManifest. `failed_steps` is empty when the run is incomplete only
  because the archive had no history; `manifest.steps.history.status` is
  then `missing`.

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
  `item` is the item id concerned, `null` for the document as a whole,
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

- **A write of several requests failed after some landed** (`playlist
  add`, `playlist remove`, `playlist replace`, `library add`, `library
  remove`; the `kind` is
  the failure's — `quota`, `unavailable`, `other`, …): `{ done: [string]
  }`, the URIs the landed requests covered, in order. They are written: do
  not send them again. `error` begins `<added|removed|wrote|saved> <n> of
  <total> (<the URIs>), then:` and
  goes on with the failure. A write that fails on its first request has
  no `details`.

  ```json
  {
    "ok": false,
    "command": "library add",
    "kind": "other",
    "error": "saved 40 of 90 (spotify:track:4uLU6hMCjMI75M1A2tKUQC, …), then: PUT me/library: HTTP 400: Invalid id",
    "details": { "done": ["spotify:track:4uLU6hMCjMI75M1A2tKUQC", "…"] }
  }
  ```

A failure before the run directory exists, a run directory whose path is
not UTF-8, and every other failure, have no `details`.

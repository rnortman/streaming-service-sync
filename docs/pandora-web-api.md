# The Pandora web API as `sss` uses it

Pandora's web player talks to a JSON API at `https://www.pandora.com/api/`.
It is not documented by Pandora; this file records the requests `sss`
sends and the response fields it reads, as observed from the web player.
The older endpoints are also described at
<https://6xq.net/pandora-apidoc/rest/>. Everything below is endpoint shape
and field names; no account data.

## Requests and authentication

Every call is `POST https://www.pandora.com/api/<path>` with a JSON body and
these headers:

| Header | Value |
|---|---|
| `Content-Type` | `application/json` |
| `X-AuthToken` | the session's API token |
| `X-CsrfToken` | any value, identical to the `csrftoken` cookie |
| `Cookie` | `csrftoken=<the same value>` |

**The API token.** The web player mints a fresh API token on every page
load and embeds it in the page, in
`window._store["v1/auth/login"][0].authToken`, then sends it as
`X-AuthToken` on each API request. The browser's `at` cookie is the login
session (the page sends it to the login call as `existingAuthToken`); it is
*not* the API token, and sending it as `X-AuthToken` fails with HTTP 502.
Any `X-AuthToken` value copied from a request in the browser's developer
tools is a minted token and works from another HTTP client.

**CSRF.** Pandora checks only that the `X-CsrfToken` header equals the
`csrftoken` cookie, so `sss` generates its own random value and sends it as
both. The browser's cookie is never needed.

**Password login** (`v1/auth/login` with a password) requires bot-mitigation
parameters only a browser produces; `sss` never uses it.

## Errors

Pandora reports failures in three ways, and `sss` treats them as follows:

- HTTP 401 or 403, or a body with `errorCode` 1001 (invalid auth token):
  the session is gone. The run stops; run `sss pandora auth` again.
- HTTP 400, 404 or 502 with a body such as `{"errorCode": 0, …}`: the
  request was wrong. Not retried; the step it belongs to fails.
- HTTP 200 with an `errorCode` body (for example 99000
  `PLAYLIST_VERSION_MISMATCH`, 99022): also a wrong request; the step fails.

HTTP 429, and 5xx responses whose body is not a JSON object with
`errorCode`, are treated as transient and retried with exponential backoff,
up to five attempts; when all five are transient the step fails, and
`sss pandora auth` reports Pandora as not answering rather than the token
as refused. Requests are paced at no more than two per second.

## Stations

`v1/station/getStations`

```json
{"pageSize": 100, "startIndex": 0}
```

Paged by `startIndex` until `totalStations` is reached. The response is
`{"totalStations": N, "stations": [ … ]}`; `sss` reads `stationId`,
`pandoraId` (`ST:…`), `name`, `dateCreated`, `lastPlayed`, `isThumbprint`,
`isShuffle` and `genre`. A page size above 100 is not known to be accepted.

`v1/station/getStationDetails`

```json
{"stationId": "<stationId>", "isCurrentStation": false}
```

The response carries `positiveFeedbackCount`, `negativeFeedbackCount` and
`seeds[]`. Each seed holds either a `song` object (`songTitle`,
`artistSummary`, `pandoraId` `TR:…`) or an `artist` object (`artistName`,
`pandoraId` `AR:…`).

`v1/station/getStationFeedback`

```json
{"stationId": "<stationId>", "positive": true, "pageSize": 100, "startIndex": 0}
```

Requested once with `"positive": true` and once with `false`, paged by
`startIndex` until an empty page or `total`. `pageSize` above 100 is
refused (HTTP 400, "pageSize is too large"). The response is
`{"total": N, "feedback": [ … ]}`, newest first. Each record carries
`feedbackId`, `isPositive`, `stationId`, `pandoraId` (the track), `musicId`,
`songTitle`, `artistName`, `albumTitle`, `trackLength` (seconds),
`trackNum` and `feedbackDateCreated` (ISO 8601 UTC with milliseconds). There
is no `discNum`, ISRC or explicit flag; an `(Explicit)` suffix on
`albumTitle` is the only hint. Shuffle stations have no feedback of their
own and are skipped.

## Playlists

`v6/collections/getSortedPlaylists`

```json
{"request": {"sortOrder": "MOST_RECENT_MODIFIED", "offset": 0, "limit": 100, "annotationLimit": 100}}
```

Paged by `offset` until a page returns fewer than `limit` items. The
response is `items[]` (each with `pandoraId` `PL:…`) and
`annotations{"PL:…": {…}}`. The annotation carries the playlist's `name`,
`description`, `version`, its creation time `timeCreated`, and
`linkedType`. For a playlist the listener assembled, `linkedType` is the
string `"None"`, which `sss` normalizes to `null`; for one Pandora
maintains it names the kind: `StationThumbs` (one station's thumbs up),
`MyThumbsUp` (all thumbs up), `Personalized`, `SharedListening`,
`Curated`, and possibly others.

`v7/playlists/getTracks`

```json
{"request": {"pandoraId": "PL:…", "offset": 0, "limit": 100, "annotationLimit": 100}}
```

Paged by `offset` until `totalTracks` is reached or a page is empty. The
response carries the playlist's current `version`, `totalTracks`,
`tracks[]` and `annotations{"TR:…": {…}}`. `sss` takes each entry's track
id from `trackPandoraId`, or from `pandoraId` when that is absent.

Pitfall: every page after the first must add
`"playlistVersion": <the version the first page returned>`; without it the
reply is `errorCode` 99000 (`PLAYLIST_VERSION_MISMATCH`). The first page
must be sent **without** it: sending the current version at offset 0
returns `notModified` and no tracks.

## Collection

`v6/collections/getItems`

```json
{"request": {"limit": 100, "cursor": "<cursor from the previous page; absent on the first>"}}
```

Paged by the returned `cursor` until it is absent. Items are typed by their
id prefix: `PL` (playlists, already captured above), `TR`, `AL` and
possibly `AR`. Whether the collection can also hold other kinds (podcasts,
episodes, stations) is unverified; the export captures any item it lists,
and normalization leaves an item of any other type out of
`collection.json` with a warning naming its id.

`v4/catalog/annotateObjects`

```json
{"pandoraIds": ["TR:…", "AL:…"]}
```

Note the body has no `request` wrapper. Up to 100 ids per call. The
response is an object keyed by id, each value a catalog annotation.

## Catalog annotations

Track annotations (`TR:…`) arrive in `v7/playlists/getTracks` and
`v4/catalog/annotateObjects` responses. Playlist annotations have been seen
to carry the track's ISRC, duration and explicitness, which feedback
records lack. `sss` reads these fields from a track annotation:

| Field | Meaning |
|---|---|
| `name` | track title |
| `artistName`, `artistId` | artist name and `AR:…` id |
| `albumName`, `albumId` | album name and `AL:…` id |
| `trackNumber` | position on the album |
| `duration` | length in seconds (`durationMillis` accompanies it in milliseconds) |
| `isrc` | the recording's ISRC |
| `explicitness` | `NONE`, `EXPLICIT` or `CLEAN`; only `EXPLICIT` is explicit |

Every field in this table has been seen in live `getTracks` and
`annotateObjects` responses; `annotateObjects` track annotations carry
`isrc` too. Album annotations (`AL:…`) supply `name`, `artistName` and
`artistId`; artist annotations (`AR:…`) supply `name`. The raw run keeps
every annotation in full, so a correction needs only `sss pandora
normalize`.

## Bookmarks

`v1/bookmark/getBookmarks`

```json
{"type": "track"}
```

and `{"type": "album"}`. `"artist"` is refused. The current web player no
longer calls this endpoint. `sss` reads `pandoraId`, `songTitle` (tracks) or
`albumTitle` (albums), `artistName` and `dateCreated` from each record of
every array in the response.

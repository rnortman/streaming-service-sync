# The matching protocol

What an agent reads before deciding which Spotify item, if any, is the same
as one from an import source (Pandora or YouTube Music). `sss guide
matching` prints it. The workflow around it is in `sss guide`, section 6.

## 1. The questions

`sss match questions --profile NAME --json` lists the open items: every
track, artist and album whose search found candidates but no certain match
(status `ambiguous`), or whose LLM batch failed (`llm_failed`). These are
exactly the items `sss match` would send to its own LLM matcher. `--limit
N` lists the first N, `--only tracks|artists|albums` one kind; `remaining`
counts the open items the limit left out. It contacts no service.

The document is `{"items": [Item], "remaining": number}` (under `--json`
inside the usual `ok`/`command` envelope). An **Item** is:

| Field | Meaning |
|---|---|
| `item` | the item id: from Pandora `TR:…` a track, `AR:…` or `CO:…` an artist, `AL:…` an album; from YouTube `YV:…` a video (a track), `YC:…` a channel (an artist) |
| `kind` | `"track"`, `"artist"` or `"album"` |
| `source` | `"pandora"` or `"youtube"`: which source the record comes from |
| `record` | what that source knows about it, below |
| `candidates` | what the Spotify search found, in its score order, best first; possibly empty |

A Pandora `record` for a track:

| Field | Meaning |
|---|---|
| `title`, `artist`, `album` | as Pandora names them; `album` is `null` when Pandora gave none |
| `duration_s` | length in seconds, or `null` |
| `explicit_hint` | whether Pandora marks it explicit |
| `flags` | version markers read from the title: `explicit`, `clean`, `album_version`, `single_version`, `remaster`, `live`, `acoustic`, `demo`, `radio_edit` |
| `station_names` | the person's stations it appears on |
| `playlist_names` | the person's playlists it appears on |

A Pandora `record` for an artist: `name`; `station_names`, the stations it
seeds; `station_thumbs_up`, up to five titles the person gave a thumbs-up
on those stations — context for telling apart two artists of one name.

A Pandora `record` for an album: `name` and `artist`.

A YouTube `record` for a track (a video):

| Field | Meaning |
|---|---|
| `title`, `artist`, `album` | the song as `sss` reads it from the video: from the person's music library, else split from the video title and channel; `album` is `null` unless the library names one |
| `duration_s` | length in seconds, or `null` (known only after `sss youtube enrich`) |
| `flags` | version markers read from `title`, as for Pandora |
| `youtube_title` | the video's own title, unaltered |
| `channel` | the uploading channel's name (`… - Topic` for YouTube's own song entries) |
| `kind` | `song` or `music_video`, as `sss` classified the video (`unknown` when the person chose to match unclassified videos too) |
| `plays` | how many times the person played it |
| `playlist_names` | the person's playlists it appears on |

A YouTube `record` for an artist (a channel): `name`, the artist as the
channel names it; `channel`, the channel's own name; `most_played_titles`,
up to five titles the person played most from it — context for telling
apart two artists of one name.

YouTube has no album items.

A **candidate** has `id`, its Spotify URI, which is what you answer with,
and `name`; a track's adds `artists`, `album`, `album_type`,
`release_date`, `duration_ms`, `explicit` and `isrc`, an artist's nothing
more, an album's `artists`, `album_type` and `release_date` — each only
where Spotify gave it.

## 2. The rules

This is the text the `music-matcher` agent of `sss match` is given,
unchanged, so that both paths decide by one set of rules. Read "the prompt"
as the questions document. "Exactly one decision per item" applies to each
item you answer; you may answer any subset. "Reply with only this JSON
object" and "use no tools" are for that agent, which answers in one turn:
you write the same object to a file and submit it with `sss match answer
--from-file`, as section 3 describes.

You match source records — from Pandora or YouTube Music — to Spotify
catalogue entries. The prompt gives a batch of items. Each item has a
`source` (`pandora` or `youtube`), a `record` of what that source knows (a
track, an artist or an album) and the Spotify candidates a search found for
it, each with an `id`. One batch may mix both sources.

For each item, pick the candidate that is the same thing as the record:

- For a track, the same recording. Prefer the studio original. Accept a
  remaster of it. Reject covers, live versions, karaoke, tribute and "made
  famous by" versions, unless the record itself is one. Prefer the
  candidate whose explicit or clean version agrees with the record's.
- For an artist, the same artist. The context that comes with the record —
  a Pandora artist's station names and thumbed-up titles, a YouTube
  channel's most played titles — tells you which of several artists of
  one name the listener meant.
- For an album, the same release by the same artist.

A YouTube record's `youtube_title` is the video's own title and `channel`
its uploader; when they disagree with `title`/`artist`, the video title is
the better witness. A `music_video` may be the same recording as a studio
track; reject it only if it is live, a cover or a different version. A
YouTube record often has no `duration_s`; judge it by the titles then.

Answer `null` for an item when no candidate qualifies. Never pick an id
that was not offered for that item.

Reply with only this JSON object, nothing before or after it:

```json
{"decisions":[{"item":"<the item's id>","choice":"<candidate id or null>","confidence":"high|medium|low","reason":"<one line>"}]}
```

Give exactly one decision per item. Use no tools: everything you need is in
the prompt.

## 3. Answering

Write one JSON object:

```json
{"decisions": [{"item": "TR:123", "choice": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "confidence": "high", "reason": "same recording, studio original"}]}
```

- One decision per item you answer; any subset of the open items, in any
  order; no item twice.
- `choice` is exactly one of that item's candidate `id`s — the
  `spotify:…` URI, not a link or a bare id — or `null` when none
  qualifies.
- `confidence` is `high`, `medium` or `low`.
- `reason` is one line; it is recorded with the decision.

Submit it from a file:

```sh
sss match answer --from-file decisions.json --profile NAME --json
```

or on stdin, which ends at the first empty line — so a file is the safe
route for anything written by hand. `--limit N` records the first N
decisions; the rest are listed in `not_recorded`, and you submit those
again (only those). The report lists, per recorded decision, `item`,
`status` (`matched`, or `unmatched` for a `null` choice), `spotify_uri`
and `confidence`. A recorded decision is settled: no later run changes it,
and `sss match questions` no longer lists it. An `unmatched` one stays in
`sss match review` for the person.

The whole document is checked before anything is recorded. Any problem —
a choice that was not offered, an item that has no open question (already
settled, not searched yet, not among the items push would send, or not an
item id), a missing field — refuses it all: **nothing was recorded;
fix the listed items and resubmit the whole document.** The error has kind
`input`, and `details.problems` lists each problem with the item it
concerns (`null` for the document as a whole):

```json
{
  "ok": false,
  "command": "match answer",
  "kind": "input",
  "error": "1 problem with the decisions; nothing was recorded: item TR:123: choice \"spotify:track:000000000000000000000z\" was not offered for it",
  "details": {
    "problems": [
      {"item": "TR:123", "problem": "item TR:123: choice \"spotify:track:000000000000000000000z\" was not offered for it"}
    ]
  }
}
```

One case records something and still fails: an `other` error from `sss
match answer` naming a match file and the item it stopped at (a full disk,
a file that cannot be written). The decisions before that item are recorded
and settled, the rest are not, and the same document resubmitted is
refused for the recorded ones. Do not resubmit it: run `sss match
questions` again, which lists exactly what is still open, and answer that.
If the interrupted write left the file with an incomplete last line, both
`questions` and `answer` then fail with an `other` error naming the file
and line ("the final line is incomplete … remove it by hand"). Repairing
the file is the person's step: report the error to them and do not edit
the file.

## 4. Example

A questions document with three items, two from Pandora and one from
YouTube:

```json
{
  "items": [
    {
      "item": "TR:123",
      "kind": "track",
      "source": "pandora",
      "record": {
        "title": "Song",
        "artist": "Artist",
        "album": "Album",
        "duration_s": 215,
        "explicit_hint": false,
        "flags": [],
        "station_names": ["Artist Radio"],
        "playlist_names": []
      },
      "candidates": [
        {"id": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "name": "Song", "artists": ["Artist"],
         "album": "Album", "album_type": "album", "release_date": "2001-05-01",
         "duration_ms": 215000, "explicit": false},
        {"id": "spotify:track:1a2b3c4d5e6f7g8h9i0jKL", "name": "Song - Live", "artists": ["Artist"],
         "album": "Live at the Hall", "album_type": "album", "release_date": "2005-09-12",
         "duration_ms": 241000, "explicit": false}
      ]
    },
    {
      "item": "AR:456",
      "kind": "artist",
      "source": "pandora",
      "record": {
        "name": "The Band",
        "station_names": ["The Band Radio"],
        "station_thumbs_up": ["First Song", "Second Song"]
      },
      "candidates": [
        {"id": "spotify:artist:0OdUWJ0sBjDrqHygGUXeCF", "name": "The Band (Tribute)"}
      ]
    },
    {
      "item": "YV:abcDEF12345",
      "kind": "track",
      "source": "youtube",
      "record": {
        "title": "Other Song",
        "artist": "Artist",
        "album": null,
        "duration_s": null,
        "flags": [],
        "youtube_title": "Artist - Other Song (Official Music Video)",
        "channel": "ArtistVEVO",
        "kind": "music_video",
        "plays": 14,
        "playlist_names": ["Road Trip"]
      },
      "candidates": [
        {"id": "spotify:track:6rqhFgbbKwnb9MLmUQDhG6", "name": "Other Song", "artists": ["Artist"],
         "album": "Album", "album_type": "album", "release_date": "2001-05-01",
         "duration_ms": 198000, "explicit": false},
        {"id": "spotify:track:2b3c4d5e6f7g8h9i0jKLmN", "name": "Other Song - Acoustic", "artists": ["Artist"],
         "album": "Unplugged", "album_type": "album", "release_date": "2008-02-02",
         "duration_ms": 205000, "explicit": false}
      ]
    }
  ],
  "remaining": 0
}
```

Its answer: the studio recording for the Pandora track; for the artist,
only a tribute act was offered, so none; for the YouTube video, the
official music video of the studio song, so the studio track and not the
acoustic version.

```json
{"decisions": [
  {"item": "TR:123", "choice": "spotify:track:4uLU6hMCjMI75M1A2tKUQC", "confidence": "high", "reason": "same title, artist, album and length; the other is live"},
  {"item": "AR:456", "choice": null, "confidence": "medium", "reason": "only a tribute act was offered"},
  {"item": "YV:abcDEF12345", "choice": "spotify:track:6rqhFgbbKwnb9MLmUQDhG6", "confidence": "medium", "reason": "official video of the studio song; the other is acoustic"}
]}
```

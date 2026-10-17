# YouTube Music: importing a Google Takeout archive

`sss` reads a YouTube Music account from the archive Google Takeout
exports for its owner. There is no sign-in: each member requests their own
archive, downloads it, extracts it, and runs

```sh
sss youtube import DIR --profile NAME
```

on the directory they extracted it into.

## Requesting the archive

At <https://takeout.google.com>, signed in to the member's Google account:

1. **Deselect all**, then select only **YouTube and YouTube Music**.
2. Under **All YouTube data included**, keep at least *history*,
   *playlists*, *subscriptions* and *music library songs*; the rest is
   never read.
3. Under **Multiple formats**, set **History** to **JSON**. An archive with
   the history in HTML is refused; request it again.
4. Choose a one-time export, `.zip`, and any size limit.

Google mails a download link when the archive is ready, from a few hours
to a day or two later.

## Extracting it

```sh
mkdir takeout && unzip -d takeout 'takeout-*.zip'
```

An archive split into several parts is extracted into the same directory,
all parts together. One directory holds one archive: a file found twice is
refused.

## What `sss` reads

Under `Takeout/YouTube and YouTube Music/` (the folder names may differ in
another language; the file names may not):

| File | What it holds |
|---|---|
| `history/watch-history.json` | every play YouTube retains, Music app and YouTube app, with its time |
| `playlists/playlists.csv` and `playlists/<title>-videos.csv` | the member's playlists, and per playlist its video ids and when each was added |
| `subscriptions/subscriptions.csv` | subscribed channels |
| `music (library and uploads)/music library songs.csv` | the YouTube Music library: song, album, artists |

Every file read is copied byte for byte into
`profiles/<name>/youtube/raw/<run-id>/takeout/` before it is parsed, with a
`manifest.json` recording each copy's size and SHA-256 and what each step
made of it. Nothing else in the archive — uploads, comments, search
history, the member's own channel — is read or copied.

An import without the history is kept but is not complete: the history is
the one essential file.

Only English archives are read. In another language the file names differ
and nothing is found; if the files are found but every history entry's
verb is localized (`Angesehen: …` for `Watched …`), the import warns that
no play reads as watched.

## What the import writes

A complete import is normalized into `profiles/<name>/youtube/`:

| File | What it holds |
|---|---|
| `history.json` | every history entry, plays, ads, posts and surveys alike |
| `videos.json` | every video history, a playlist or the library names, with its play counts and whether it is a `song`, a `music_video`, a `video` or `unknown` |
| `playlists.json` | the playlists, items in the archive's order |
| `subscriptions.json` | subscribed channels, each marked `artist` or `channel` |
| `library.json` | the library songs; `liked` and `disliked` are empty, as the archive has neither |
| `runs.json` | the ids of the import runs these files come from, oldest archive first; the last supplied the playlists, subscriptions and library |

Importing a newer archive adds a run; the history becomes the union of
every complete run's, so plays Google has since deleted stay counted. The
playlists, subscriptions and library are the newest archive's. `sss youtube
normalize` writes the files again from the runs already imported.

## What the archive lacks

- **Likes and dislikes.** Takeout carries no liked-videos list and no
  dislikes.
- **Durations.** No file gives a video's length.
- **Titles for playlist items.** A playlist's items file holds only video
  ids; a video seen nowhere else in the archive has no title.
- **Titles of removed videos.** A play of a video since removed or made
  private keeps its address but loses its title and channel.
- **Plays YouTube no longer keeps.** History is bounded by the account's
  auto-delete setting (36 months by default for accounts created since
  2020) and by any pause or deletion. Each archive is a snapshot of what
  Google retains when it is made.

Titles for playlist items, durations, and a category that tells a song
from a podcast or a trailer come from the YouTube Data API once a key is
stored (below). Likes, dislikes and removed videos' titles do not.

## Playlist file names

Takeout names a playlist's items file after its title, with
`\ / : * ? " < > | '` written `_`. Two playlists whose titles map to the
same file name (the same title, or titles that differ only in one of those
characters or in case) share one file, and the archive does not say whose
items it holds: `sss` attaches it to neither and warns. Renaming one of
them on YouTube and exporting again recovers both.

## What is matched to Spotify

`sss match` looks up, on Spotify, the YouTube videos classified as music
(`song` or `music_video`) that are in the library, in a playlist that is
pushed, or among the most played, and the subscribed channels marked
`artist`. A playlist is pushed unless YouTube keeps it itself (Watch
later), its items file is shared with another playlist (above), or its id
is listed in `skip_playlists`. `sss match --source youtube` matches only
these items; `--source pandora` only the Pandora ones.

The `[youtube]` table of `config.toml`:

```toml
[youtube]
most_played = 100         # songs in the "most played" playlist; 0 = none
include_unknown = false   # also match and push videos sss could not classify as music
skip_playlists = []       # YouTube playlist ids (PL…) never matched or pushed
```

`skip_playlists` is for a playlist that already reached Spotify another
way — a Pandora library converted to a YouTube playlist by a transfer
service, say. A YouTube playlist's id is the `list=` value of its address;
an entry that is not a playlist id (`PL` and 32 more characters) makes
every command fail with kind `config`.

A video without a title — removed since it was played, or seen only as a
playlist item — cannot be searched for: when it is in scope it is left
`unmatched` with the reason `no title`, for a person to settle with `sss
match set YV:<id> …`. A playlist item seen nowhere else is unclassified,
so it is in scope only with `include_unknown`.

## What is pushed to Spotify

`sss push` follows the matched artist channels, saves the matched library
songs to the Spotify library, makes one private playlist per pushed
YouTube playlist — its title, its matched music in the archive's order,
each track once — and makes one playlist, "YouTube Music: most played"
(after `[push] playlist_prefix`), of the matched songs among the
`most_played` most played. Spotify cannot take play history; that
playlist is the history's one trace there. Push never removes anything,
so it is cumulative: a later push, after a new archive or a larger
`most_played`, appends whoever newly entered the top and keeps the rest.
To start it afresh, delete it in Spotify and remove its
`youtube:most-played` entry from `spotify/state.json`. Liked videos, in an archive
that has a `Liked videos` or `Liked Music` playlist, go to Liked Songs
under `[push] liked_songs`; Watch later and the liked lists are never
made playlists. `sss push --source youtube` pushes only what the YouTube
export contributes.

## What the report shows

`sss report` gives the YouTube export its own part of `report.md`: the
import runs the history is unioned from and the newest one, which the
playlists, library and subscriptions come from; the span of the history
with every stretch of 30 days or more without a play, since an archive
cannot tell a paused history from a deleted one; plays by app and by
kind; the removed videos; whether the archive had any likes; the 20 most
played songs and artists; the videos no signal could classify, by
channel; the videos typed music only because they were played in the
Music app, by channel; and per playlist its id, its distinct videos, how
many are music and how many the archive says nothing about. Copy a
playlist's id from there into `skip_playlists`. "Needs a human" links
each YouTube video beside its Spotify search, and "YouTube playlists not
pushed" says why each playlist was left out: a list YouTube keeps, an
items file two playlists share, no items file, `skip_playlists`, or no
matched music. Under "Push", the most-played playlist shows the current
top N beside what `sss` has added to it.

## The YouTube Data API key (optional)

Without a key, `sss` tells music from video by what the archive itself
shows — the library, ` - Topic` and `VEVO` channels, music markers in a
title, and the Music app — and everything else is unclassified; a video
named only by a playlist has no title; and no video has a length, so
every YouTube song goes to the LLM rather than being matched outright
(expect about one LLM batch per 25 songs). With a key, `sss youtube
enrich` asks the YouTube Data API v3 about every video the export names
and fills in its title, channel, length, category and topics.

The key is the owner's, one for every profile. To make one:

1. At <https://console.cloud.google.com>, create a project (or pick one).
2. Under **APIs & Services → Library**, enable **YouTube Data API v3**.
3. Under **APIs & Services → Credentials**, **Create credentials → API
   key**. Edit it and, under **API restrictions**, restrict it to YouTube
   Data API v3.

Then store it — pasted, ending with an empty line, or from a file:

```sh
sss youtube api-key
sss youtube api-key --from-file key.txt
```

`sss` checks the key with one request for a public video and stores it
only if Google accepts it, in `youtube-api-key.json` under the state
directory (`$XDG_STATE_HOME/streaming-service-sync/`, mode 0600), never in
the data repository. A refused key fails with kind `input`; a project
without the API enabled with kind `config`.

```sh
sss youtube enrich --profile NAME
sss youtube enrich --profile NAME --limit 500
```

asks about every video in `videos.json` the API has not answered yet, 50
per request, then normalizes again. Each response is stored verbatim in
`profiles/<name>/youtube/api/` before it is read, and `normalize` reads all
of them, the latest answer per video winning; a video the API does not
return is marked unavailable (removed or private). The free daily quota,
10 000 units, covers 500 000 videos. When it is spent the command fails
with kind `quota`; what was fetched is kept, and the next `enrich`, after
midnight Pacific time, asks only for the rest.

Run `enrich` before the profile's first `sss match`. Matching decides each
item once and does not search again when `enrich` later changes what a
video says about itself, so only videos not yet matched gain from it —
among them the playlist-only and unclassified videos it brings into
scope. A video matched before it was enriched keeps its decision: if it
was left ambiguous, the LLM (or `sss match answer`) still settles it from
the candidates found under its earlier title; if it was left unmatched
for want of a title, settle it with `sss match set` or `sss match reject`.

**Google's storage policy.** YouTube API Services Developer Policy
III.E.4 lets an API client store what the API returns for at most 30 days
unless it is refreshed, and asks that it be deleted within 30 days of the
user revoking access. `sss` keeps the responses in the data repository
indefinitely — they are what the normalized files are rebuilt from. That
is outside the policy; store a key only if you accept it.

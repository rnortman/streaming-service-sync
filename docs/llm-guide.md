# Operating `sss` for a person: a guide for an agent

This guide is for an LLM agent, or anyone scripting `sss`, that runs the
whole job on a person's behalf: export their Pandora account, match it to
Spotify, and push it into their Spotify account. Read it once, top to
bottom. `sss guide` prints it; `sss guide matching` prints the matching
protocol, which you re-read before deciding any match.

## 1. What `sss` is and what your job is

`sss` (streaming-service-sync) copies one family's Pandora listening into
Spotify: stations and their seeds, thumbs up and down, playlists, the
on-demand collection and bookmarks. It keeps everything in a local data
repository of plain JSON that the owner keeps.

Your job is to operate one person's part of that store on their behalf:
sign them in, export, match, and push, asking them for the steps only a
person can do. The owner of the store has already registered a Spotify app
and put its client id in `config.toml`; that setup is not covered here
(see
[the Spotify app setup](https://github.com/rnortman/streaming-service-sync/blob/main/docs/spotify-app-setup.md)).
If a command says the client id is missing, tell the owner.

## 2. Installing

```sh
git clone https://github.com/rnortman/streaming-service-sync.git
cd streaming-service-sync
cargo install --path . --locked
sss --version
```

This needs [rustup](https://rustup.rs/); the pinned toolchain installs on
first use. `sss guide` prints this document and `sss guide matching` the
matching protocol, so refresh any copy you keep from the binary you run.

## 3. The store and profiles

- The data repository defaults to `~/.local/share/streaming-service-sync`;
  `--repo DIR` selects another. `sss init` creates it once (it never
  overwrites anything).
- Each family member is a profile: `--profile NAME` (lowercase letters,
  digits, `-`, `_`), optional while only one profile exists. Use the
  profile you were asked to operate, for example `alice`.
- Secrets — the Pandora session and the Spotify token — live only in the
  state directory, `~/.local/state/streaming-service-sync/NAME/`, mode
  0600, never in the data repository.
- Every command takes `--json` and then prints one JSON document on stdout
  (section 14). Start every session with:

```sh
sss status --json
```

It lists each profile's Pandora session, Spotify token, latest export run
and match counts, and tells you which of the steps below are already done.

## 4. Pandora sign-in and export

The person does this part in their browser, logged in to Pandora:

1. Log in at <https://www.pandora.com>.
2. Open the developer tools (F12), **Network** tab.
3. Reload the page and pick any request whose URL contains `/api/`.
4. Under **Request Headers**, copy the value of `X-AuthToken` (not the
   `at` cookie).
5. Send it to you.

Store it by piping it in — never as a command-line argument:

```sh
echo "$TOKEN" | sss pandora auth --profile NAME --json
sss pandora export --profile NAME --json
```

`sss pandora export` stores every Pandora response verbatim, then
normalizes them. A run with failed steps ends with kind `incomplete`: the
run is kept, `details.failed_steps` names the steps, and a later export
captures everything again. `sss pandora normalize` re-derives the
normalized files from the latest complete run without contacting Pandora.

### YouTube Music

A profile can also, or instead, hold a YouTube Music account, read from a
Google Takeout archive the person downloads and extracts themselves (see
`docs/youtube-takeout.md` in the repository for what to select). Ask them
for the directory they extracted it into:

```sh
sss youtube import DIR --profile NAME --json
```

It copies the archive's history, playlists, subscriptions and library
files verbatim into a new run, parses the copies and, when the run is
complete, normalizes: the play history is the union of every complete
run's, so importing a newer archive never loses plays an older one had,
and playlists, subscriptions and library come from the newest archive.
A run without the history, or with a file that does not parse, ends with
kind `incomplete` and is kept; an archive with the history only as HTML
is kind `input`: the person requests Takeout again with history in JSON.
`sss youtube normalize` re-derives the normalized files from every
complete run without reading the archive again.

A YouTube Data API key is optional and improves matching: it gives every
video its title, length and category, which the archive lacks. Ask
whether the person has one (`docs/youtube-takeout.md` says how to make
it); if they do, have them store it once for every profile — piped or
from a file, never as an argument — and then enrich each YouTube
profile:

```sh
sss youtube api-key --from-file KEYFILE --json
sss youtube enrich --profile NAME --json
```

`youtube api-key` stores the key only after one test request succeeds:
kind `input` is a key Google refused, kind `config` a Cloud project
without YouTube Data API v3 enabled. `youtube enrich` asks only about the
videos not asked about before, then normalizes; kind `quota` means the
daily quota is spent and what was fetched is kept — run it again the next
day. `--limit N` bounds one run. Enrich before the profile's first
`sss match`: matching decides each item once, so a video matched before
it was enriched keeps that decision (an ambiguous one is still settled by
the LLM or `match answer`; an unmatched one by `match set` or
`match reject`).

Matching (section 6) takes both sources together; `--source youtube` or
`--source pandora` on `sss match` narrows a run to one. YouTube items are
`YV:` (a video, matched as a track) and `YC:` (a channel, matched as an
artist).

## 5. Spotify sign-in

```sh
sss spotify auth --profile NAME --json
```

Hand the document's `authorize_url` to the person. They open it in a
browser logged in to their Spotify account and approve. The browser then
lands on `https://127.0.0.1/callback?code=…&state=…` and shows "can't
connect"; that is expected. They send you that whole address, and you
finish:

```sh
echo "$ADDRESS" | sss spotify auth --callback --profile NAME --json
```

An address carrying `error=access_denied` means they declined; start over
from the first command if they want to.

A profile signed in by an earlier `sss` may lack scopes this one asks
for; `sss status --json` lists them under `spotify_token.missing_scopes`,
and a command that needs one fails with kind `auth` saying so — run the
two steps again.

## 6. Matching

```sh
sss match --profile NAME --json
```

searches Spotify for every track, artist and album the push would send,
decides the certain ones itself, and hands the ambiguous ones to an LLM
through the `ccc` tool. If `ccc` is not available the command fails with
kind `config` naming `ccc`; then run the deterministic part alone and
decide the ambiguous items yourself, in a loop:

```sh
sss match --no-llm --profile NAME --json
sss match questions --limit 25 --profile NAME --json
# decide each item by `sss guide matching`; write decisions.json
sss match answer --from-file decisions.json --profile NAME --json
```

Repeat `questions` and `answer` until `items` is empty and `remaining` is
0. Kind `quota` means a rate or usage limit: wait, then run the same
command again; nothing done so far is lost. Either path can take over from
the other at any time: `sss match` asks `ccc` about whatever is still open,
and `sss match questions` lists whatever `ccc` has not settled.

When the stored candidates do not hold the right recording, search for it
and record the `id` with `sss match set` (section 7):

```sh
sss search "Panjabi MC Mundian to Bach Ke" "Daft Punk One More Time" --only tracks --profile NAME --json
```

Each query is one Spotify search; `results` holds one entry per query with
its `tracks`, `artists` and `albums` in Spotify's order (`--limit N`, 1 to
10, per kind). Batch every query you need into one call.

## 7. Settling leftovers

```sh
sss match review --profile NAME --json
```

lists the items still undecided — ambiguous, failed, unmatched, or matched
with low confidence — with their best candidates and a Spotify search
link. For each one you cannot settle by the matching rules, ask the
person, then record their answer:

```sh
sss match set TR:123 spotify:track:4uLU6hMCjMI75M1A2tKUQC --profile NAME --json
sss match reject TR:123 --profile NAME --json
```

`match set` takes a Spotify URI, link or id; `match reject` records that
the item has no Spotify counterpart. No later run overrides either. When
none of the listed candidates is right, `sss search` finds others; its
results' `id`s go straight into `sss match set`.

## 8. Playing music

`sss` also controls the profile's Spotify player. It needs Spotify Premium
on the member's account and a token with the playback scopes; a token
that lacks them is handled as section 5 says.

```sh
sss devices --profile NAME --json
sss player --profile NAME --json
sss play spotify:track:4uLU6hMCjMI75M1A2tKUQC --profile NAME --json
sss play --item TR:123 --profile NAME --json
sss play --station "Music for working" --shuffle --profile NAME --json
sss queue spotify:track:4uLU6hMCjMI75M1A2tKUQC --profile NAME --json
sss pause --profile NAME --json
sss next --profile NAME --json
sss previous --profile NAME --json
sss shuffle on --profile NAME --json
sss repeat off --profile NAME --json
sss volume 40 --profile NAME --json
sss seek 90 --profile NAME --json
```

`sss devices` lists the open players; `sss player` shows what is playing
and the queue. `sss play` takes any number of tracks, or one playlist,
album or artist, as URIs, links or ids; with nothing named it resumes.
`--item TR:…` plays a track's stored candidates in order, so the person can
settle it by ear and `sss next` moves to the next candidate; record their
choice with `sss match set`. `--station NAME` plays the Spotify playlist
`sss push` made for a Pandora station. Every command that changes playback
takes `--device NAME|ID`; without it the active player is used, else the
only one open.

When `play` succeeds with a `hint` that Spotify loaded the music and is
not playing, a browser is usually blocking autoplay. Pass the hint to the
person: click play once, or allow autoplay for the Spotify
tab. Kind `player` means the person has something to do on the Spotify
side — open a player, pick one with `--device`, or use a Premium account;
relay the message, then run again.

A queued track cannot be taken back. Queue only what the person named,
with ids from `sss search` or a match document — never a guess. To find
several tracks to queue, put every query in one `sss search` call, choose
from its results, then queue them in one `sss queue` call.
When `sss queue` stops partway, `error` names the tracks already queued:
do not queue those again.

A skip or a queued track cannot be undone, so never run a playback
command again on a guess. When it succeeds but its `hint` says the
player could not be read, it happened: run `sss player` to see the
state. When it fails with an `error` saying Spotify may have applied it,
it may have happened — for `sss queue`, the track after the ones named
may be queued too: run `sss player` and look before running anything
again.

## 9. Listening history, favourites and playlists

```sh
sss history --profile NAME --json
sss top tracks --range short --profile NAME --json
sss top artists --limit 10 --profile NAME --json
sss playlists --profile NAME --json
sss playlist spotify:playlist:37i9dQZF1DXcBWIGoYBM5M --profile NAME --json
```

`sss history` lists the member's last finished plays, newest first, each
with `played_at` and, when it was played from one, the playlist, album or
artist under `context`. Spotify keeps only the last 50 completed tracks; a
track playing now appears once it ends, and podcast episodes never do.

`sss top tracks|artists` is Spotify's own ranking of what the member plays
most, first place first. `--range short|medium|long` picks Spotify's
window — about the last four weeks, about the last six months (the
default), or several years; `--limit N` is 1 to 50 (default 50).

`sss playlists` names the account under `user` and lists every playlist
on it, the member's own and the ones they follow, each with its `owner`
and item count `total`. A playlist whose `owner.id` differs from `user.id`
is one shared with the member: its `id` goes to `sss play`. `sss playlist`
takes one or more playlists and shows each one's contents in order. A
playlist is given as its URI or open.spotify.com link (not a bare id), as
the key the export shows for something `sss push` made a playlist of
(`ST:…` for a station, `PL:…`, `YP:…`, `thumbs-down`,
`youtube:most-played`) — `pushed_as` in the document names it — or by its
name on the account (`sss playlist "Kids Songs"`), which must match exactly
one playlist, ignoring case; several or none is kind `input` with the
candidates listed, and then you give the URI. `unavailable` counts entries
Spotify holds but cannot show (removed or unreadable items); when it is
above zero, the numbering of `items` is not Spotify's. Spotify shows a
playlist's items only to its owner and its collaborators, so a shared
playlist shows its contents only if it is collaborative; any other comes
back with `items: null` and the command still succeeds.

## 10. Changing playlists and the library

```sh
sss playlist create "Kids Songs" --dry-run --profile NAME --json
sss playlist create "Kids Songs" --description "For the car" --profile NAME --json
sss playlist edit "Kids Songs" --name "Car songs" --public --profile NAME --json
sss playlist add "Kids Songs" spotify:track:4uLU6hMCjMI75M1A2tKUQC TR:123 --profile NAME --json
sss playlist add "Kids Songs" --from-file - --at 1 --dry-run --profile NAME --json
sss playlist remove ST:123 spotify:track:4uLU6hMCjMI75M1A2tKUQC --dry-run --profile NAME --json
sss playlist move "Kids Songs" --from 3 --count 2 --before 1 --dry-run --profile NAME --json
sss playlist move "Kids Songs" --from 1 --to-end --profile NAME --json
sss playlist replace "Kids Songs" --from-file - --dry-run --profile NAME --json
sss playlist delete ST:123 --dry-run --profile NAME --json
sss library tracks --limit 20 --profile NAME --json
sss library albums --profile NAME --json
sss library artists --profile NAME --json
sss library add spotify:track:4uLU6hMCjMI75M1A2tKUQC TR:123 --dry-run --profile NAME --json
sss library add spotify:track:4uLU6hMCjMI75M1A2tKUQC TR:123 --profile NAME --json
sss library remove --from-file - --dry-run --profile NAME --json
```

The `sss playlist` writes name their playlist as `sss playlist` does: by
URI or link, by the key `sss push` recorded (`ST:…`, `PL:…`, `YP:…`,
`thumbs-down`, `youtube:most-played`), or by its name on the account,
which must match exactly one playlist. `sss playlist create NAME` makes a
private playlist (`--public`, `--collaborative`, `--description TEXT`);
it refuses a name the member already owns, so running it twice does not
make two — `--duplicate` makes another all the same. `sss playlist edit`
changes the name, description, visibility or collaboration of a playlist
the member owns. `sss playlist add` appends tracks (or inserts them before
position `--at N`, numbered as `sss playlist` shows them) and skips the
ones already there (`--duplicates` adds them again); `sss playlist remove`
takes every occurrence of each track out. `sss playlist move` moves
`--count K` items (default 1) from position `--from N` to before position
`--before M` or to the end (`--to-end`), positions as `sss playlist`
numbers them. `sss playlist replace` sets the contents to exactly the
tracks given, in that order, a track given twice held twice; with no
tracks it empties the playlist. Its dry run lists under `removed` what
leaves, under `added` what arrives, and `reordered` says whether what
stays changes order. Anything a move cannot express — sorting,
deduplicating — is a `replace`. These four change only a playlist the
member owns or one that is collaborative. When `sss playlist` reports
`unavailable` above zero, the numbering is not Spotify's and `--at` and
`move` are refused; `replace` still works, and erases those entries too. `sss playlist delete` removes the playlist from the account —
deleted when it is the member's, unfollowed when it is someone else's;
Spotify keeps a deleted playlist recoverable for 90 days at
https://www.spotify.com/account/recover-playlists/. The document's
`to_add`, `already_present`, `to_remove`, `not_present`, `to_move`,
`removed`, `added` and `action` say what the command changes.

`sss library tracks|albums|artists` lists the saved tracks, the saved
albums or the followed artists, most recently saved first, each with
`added_at` (`null` for an artist: Spotify gives no date for a follow) and
the `item`. Without `--limit N` it reads the whole library, one request
per 50 entries; pass `--limit` when the most recent ones are enough.

`sss library add` saves tracks and albums and follows artists and
playlists; `sss library remove` removes and unfollows them. Each takes
items as Spotify URIs, open.spotify.com links, bare track ids or the
source item ids the export shows (`TR:…`, `YV:…`, `AR:…`, `AL:…`), which
resolve to their recorded match; an item id without a `matched` match is
kind `input` and nothing is sent. `--from-file FILE` (`-` for stdin, read
to the end) adds items one per line, skipping blank lines and lines
starting with `#`; it may be combined with items on the command line. The
document lists what changes (`to_save`, `to_remove`) and what was left
(`already_saved`, `not_saved`), and under `resolved` what each item id
stood for. Names are not accepted: find the `id` with `sss search`,
`sss library`, `sss history` or `sss top` first.

Tracks for a playlist and items for the library take the same forms.
Every write takes `--dry-run`, which makes the reads and none of the
writes and says `dry_run: true`. Run the dry run first, read what it will
change, and then run the same command without it. A playlist changed in
the Spotify app between the dry run and the live run is written as it is
at the live run, so read the live run's document too. When a write fails
after part of it landed, `details.done` in the failure document lists
the URIs that did: do not send them again.

**The Kids Songs merge.** The person wants the small station playlists
`sss push` made merged into one. Read them by the station ids the export
shows, in one call:

```sh
sss playlist ST:1 ST:2 ST:3 --profile NAME --json
```

Collect each one's `items[].id` in order, then make the playlist and fill
it with one `add` (it skips what is already there, so running it twice
does no harm):

```sh
sss playlist create "Kids Songs" --dry-run --profile NAME --json
sss playlist create "Kids Songs" --profile NAME --json
printf '%s\n' spotify:track:4uLU6hMCjMI75M1A2tKUQC spotify:track:6rqhFgbbKwnb9MLmUQDhG6 |
  sss playlist add "Kids Songs" --from-file - --dry-run --profile NAME --json
```

Read `to_add`, run the `add` again without `--dry-run`, and then, with the
person's agreement, delete each small one:

```sh
sss playlist delete ST:1 --dry-run --profile NAME --json
sss playlist delete ST:1 --profile NAME --json
```

`sss push` leaves a deleted playlist alone while it is off the account. A
delete made by mistake is undone in Spotify: recover the playlist at
https://www.spotify.com/account/recover-playlists/ and the next push
maintains it again; there is nothing to run in `sss`.

**A playlist of what I played this week.** Read `sss history --profile
NAME --json`, keep the `track.id` of each entry whose `played_at` is in
the week (the history holds the last 50 plays only), and add them to a
new playlist:

```sh
sss playlist create "This week" --profile NAME --json
printf '%s\n' spotify:track:4uLU6hMCjMI75M1A2tKUQC |
  sss playlist add "This week" --from-file - --dry-run --profile NAME --json
```

`add` keeps the first of a track played twice and reports the rest under
`duplicates_dropped`; read `to_add`, then run it without `--dry-run`.

**A dedupe.** Read `sss playlist "Kids Songs" --profile NAME --json`, keep
the first of each `items[].id` in order, and pipe that list to `replace`:

```sh
printf '%s\n' spotify:track:4uLU6hMCjMI75M1A2tKUQC spotify:track:6rqhFgbbKwnb9MLmUQDhG6 |
  sss playlist replace "Kids Songs" --from-file - --dry-run --profile NAME --json
```

The dry run's `removed` is empty and `after` is below `before` by the
duplicates dropped; check `reordered` is `false`, then run it again
without `--dry-run`.

**Unfollowing artists.** Read `sss library artists --profile NAME --json`,
show the person the names, and pipe the `item.id` of each artist they
want to drop:

```sh
printf '%s\n' spotify:artist:0OdUWJ0sBjDrqHygGUXeCF |
  sss library remove --from-file - --dry-run --profile NAME --json
```

Read `to_remove`, then run it again without `--dry-run`.

## 11. The report

```sh
sss report --profile NAME --json
```

writes `profiles/NAME/report.md` and prints its path. Read it: it says what
was exported, how much is matched, and every item that needs a person.
Tell the person what needs them. For a YouTube export it also lists the
videos sss could not classify and the playlists it will not push, with
the reason; a playlist the person does not want on Spotify goes into
`[youtube] skip_playlists` by the id the report prints.

## 12. Push

```sh
sss push --dry-run --profile NAME --json
sss push --profile NAME --json
```

The dry run prints the plan and sends nothing. Show the person the plan and
push only with their agreement. `--only
artists|stations|playlists|collection|liked|most-played|thumbs-down`
pushes one part; `--source pandora|youtube` pushes only what one source
contributes.
A second push adds only what is missing; push never removes anything from
Spotify, and it adds each matched track to a playlist once: a track removed
from a pushed playlist — by the person in Spotify or with `sss playlist
remove` — is never added back. A pushed playlist deleted in Spotify or with
`sss playlist delete` is reported with action `deleted` and left alone
until it is back on the account. `push --dry-run` makes no request, so it
cannot see a deletion: it reports such a playlist as the real push would
without it. A playlist with action `pending` may already have been made by an
earlier push that stopped before recording it: `sss` leaves it alone and
says what to look for in Spotify and which `spotify/state.json` entry to
edit. That edit is the owner's to make by hand; tell them.

`[push] liked_songs` in `config.toml` is on by default: the matched thumbs
up are saved to Liked Songs. It is one switch for every profile in the
repository, not a per-member setting, so do not ask each member about it;
if a member does not want it, tell the repository's owner (the person who
ran `sss init`), whose call it is.

## 13. Re-runs and resuming

Every command can be stopped and run again: what was done is recorded as it
happens, and the next run continues. Kinds `quota` and `unavailable` mean
try the same command later. A new `sss pandora export` replaces the
normalized files; the next `sss match` searches only the items it has not
seen, and the next push adds what is new.

## 14. Reading `--json`

On success stdout is one document that starts with `"ok": true` and
`"command"`, followed by the command's report:

```json
{
  "ok": true,
  "command": "match questions",
  "items": [],
  "remaining": 0
}
```

On failure it carries `"ok": false`, `command`, `kind`, `error` (the same
text as the `error: …` line on stderr) and, for some failures, `details`:

```json
{
  "ok": false,
  "command": "pandora export",
  "kind": "auth",
  "error": "no Pandora session for profile alice; run `sss pandora auth`"
}
```

Empty stdout with exit status 1 is a usage error: read stderr. What to do
about each `kind`:

- `auth` — a session or token is missing or was refused: sign in again
  (section 4 or 5).
- `unavailable` — Pandora or Spotify did not answer: run it again later.
- `quota` — a rate or usage limit: run it again later; it continues.
- `incomplete` — an export with failed steps: export again later.
- `input` — what you piped or passed is unusable (including a target of
  the wrong type, or an unknown device, station or item); for `sss match
  answer`, `details.problems` lists each problem: fix those and resubmit.
- `config` — the setup needs fixing as the message says (the repository,
  `config.toml`, `--profile`, the Spotify app, `ccc`); tell the owner when
  it is theirs to fix.
- `player` — the Spotify player cannot take the command: no player is
  open, several are open and none active, the account lacks Premium, or
  Spotify refused the control. `details.reason` carries the reason code
  and `details.devices` the open players. Relay the message to the person,
  then run again.
- `other` — anything else; read `error` and tell the person.

Every field of every document is in
[the JSON output reference](https://github.com/rnortman/streaming-service-sync/blob/main/docs/json-output.md).

## 15. Check with the person; never do

Ask the person before:

- a push that is not a dry run;
- `sss playlist delete`, `sss playlist remove`, `sss playlist replace` or
  `sss library remove` of anything the person did not name themselves;
- recording a Spotify choice the matching rules do not settle;
- `sss pandora auth` with a token for a different listener than the
  profile's stored session (it replaces that session).

Run every write as a dry run first and read it; when a failure carries
`details.done`, those landed.

Never:

- guess a URI for a write: take it from `sss search`, `sss playlist`,
  `sss history`, `sss top`, `sss library` or a match document;
- put a token or the callback address in a command-line argument: pipe it
  on stdin or use `--from-file`;
- edit `matches/*.jsonl`, `spotify/state.json` or the state directory by
  hand; when a command says a file needs repair, tell the person;
- commit the state directory anywhere;
- run `sss` for any profile other than the one you were asked to operate.

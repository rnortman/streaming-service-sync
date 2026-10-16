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
  (section 12). Start every session with:

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
on the member's account and a token signed in with the playback scopes:
when `spotify_token.scopes` in `sss status --json` lacks
`user-modify-playback-state`, run the sign-in of section 5 again. The
player commands then fail with kind `auth` saying so.

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

## 9. The report

```sh
sss report --profile NAME --json
```

writes `profiles/NAME/report.md` and prints its path. Read it: it says what
was exported, how much is matched, and every item that needs a person.
Tell the person what needs them.

## 10. Push

```sh
sss push --dry-run --profile NAME --json
sss push --profile NAME --json
```

The dry run prints the plan and sends nothing. Show the person the plan and
push only with their agreement. `--only
artists|stations|playlists|collection|liked|thumbs-down` pushes one part.
A second push adds only what is missing; push never removes anything from
Spotify. A playlist with action `pending` may already have been made by an
earlier push that stopped before recording it: `sss` leaves it alone and
says what to look for in Spotify and which `spotify/state.json` entry to
edit. That edit is the owner's to make by hand; tell them.

`[push] liked_songs` in `config.toml` is on by default: the matched thumbs
up are saved to Liked Songs. It is one switch for every profile in the
repository, not a per-member setting, so do not ask each member about it;
if a member does not want it, tell the repository's owner (the person who
ran `sss init`), whose call it is.

## 11. Re-runs and resuming

Every command can be stopped and run again: what was done is recorded as it
happens, and the next run continues. Kinds `quota` and `unavailable` mean
try the same command later. A new `sss pandora export` replaces the
normalized files; the next `sss match` searches only the items it has not
seen, and the next push adds what is new.

## 12. Reading `--json`

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

## 13. Check with the person; never do

Ask the person before:

- a push that is not a dry run;
- recording a Spotify choice the matching rules do not settle;
- `sss pandora auth` with a token for a different listener than the
  profile's stored session (it replaces that session).

Never:

- put a token or the callback address in a command-line argument: pipe it
  on stdin or use `--from-file`;
- edit `matches/*.jsonl`, `spotify/state.json` or the state directory by
  hand; when a command says a file needs repair, tell the person;
- commit the state directory anywhere;
- run `sss` for any profile other than the one you were asked to operate.

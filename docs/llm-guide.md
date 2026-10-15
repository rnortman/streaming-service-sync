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
  (section 11). Start every session with:

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
the item has no Spotify counterpart. No later run overrides either.

## 8. The report

```sh
sss report --profile NAME --json
```

writes `profiles/NAME/report.md` and prints its path. Read it: it says what
was exported, how much is matched, and every item that needs a person.
Tell the person what needs them.

## 9. Push

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

## 10. Re-runs and resuming

Every command can be stopped and run again: what was done is recorded as it
happens, and the next run continues. Kinds `quota` and `unavailable` mean
try the same command later. A new `sss pandora export` replaces the
normalized files; the next `sss match` searches only the items it has not
seen, and the next push adds what is new.

## 11. Reading `--json`

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
- `input` — what you piped or passed is unusable; for `sss match answer`,
  `details.problems` lists each problem: fix those and resubmit.
- `config` — the setup needs fixing as the message says (the repository,
  `config.toml`, `--profile`, the Spotify app, `ccc`); tell the owner when
  it is theirs to fix.
- `other` — anything else; read `error` and tell the person.

Every field of every document is in
[the JSON output reference](https://github.com/rnortman/streaming-service-sync/blob/main/docs/json-output.md).

## 12. Check with the person; never do

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

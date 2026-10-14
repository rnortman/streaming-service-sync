# TODO

Deferred work, one entry per slug. Code that the work touches carries a
`TODO(<slug>)` comment with the same slug.

## `browser-cookie-extraction`

Optionally import the Pandora session from a local Chromium or Chrome
profile: read and decrypt the `at` cookie, fetch the site root with it,
take the minted `authToken` from `window._store`; both browsers' cookie
encryption schemes.

## `pandora-annotation-fields`

Check the response fields the normalizer reads but that have not been seen
in a live response — track annotations' `isrc`, `explicitness` (and its
value set), `duration`, `trackNumber`, `artistId`, `albumId`; whether
`v4/catalog/annotateObjects` track annotations carry `isrc`; playlist
annotations' `timeCreated`; `getTracks` entries' `trackPandoraId` — against
the raw files of a real export, then correct `src/pandora/model.rs` and
`docs/pandora-web-api.md` where they differ.

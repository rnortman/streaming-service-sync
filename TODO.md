# TODO

Deferred work, one entry per slug. Code that the work touches carries a
`TODO(<slug>)` comment with the same slug.

## `browser-cookie-extraction`

Optionally import the Pandora session from a local Chromium or Chrome
profile: read and decrypt the `at` cookie, fetch the site root with it,
take the minted `authToken` from `window._store`; both browsers' cookie
encryption schemes.

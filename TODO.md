# TODO

Deferred work, one entry per slug. Code that the work touches carries a
`TODO(<slug>)` comment with the same slug.

## `browser-cookie-extraction`

Optionally import the Pandora session from a local Chromium or Chrome
profile: read and decrypt the `at` cookie, fetch the site root with it,
take the minted `authToken` from `window._store`; both browsers' cookie
encryption schemes.

## `fake-ccc-etxtbsy`

The `matching::llm` unit tests each write a fake `ccc` script and then
spawn it. When another test thread forks while one is still writing its
script, the child inherits the open write descriptor, and the `execve`
fails with `ETXTBSY` ("Text file busy"). The result is an occasional
failure of `ccc_that_cannot_run_or_resolve_the_agent_is_a_config_error`
(or another test in that module) under `cargo test`. Fix: serialise these
tests with a `static Mutex<()>` whose guard `Rig` holds from `new_rig`
until the test's last spawn. Don't retry `ETXTBSY` in the production spawn
path.

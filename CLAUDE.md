# streaming-service-sync

`sss` exports Pandora accounts into a local data repository, matches the
exported tracks, artists and albums to Spotify (deterministic scoring first,
an LLM run through `ccc` for the ambiguous ones), and pushes the result into
Spotify accounts, recording sync state beside the export. One binary, crate
`streaming-service-sync`.

## Building

```
make check        — full pre-commit suite (msrv, fmt, clippy, test, license audit, scrub kit)
make rust-check   — the same without the license audit and scrub kit check (what CI's check job runs)
```

## Principles

- **Every command prints `error: …` and exits 1 on failure.**
- **Every command is subprocess-safe.** stdout is the result (one JSON
  document with `--json`), stderr is for people; input comes from
  arguments, files or stdin read up to an empty line or EOF — never from
  the terminal device; nothing opens a browser, pager or editor, and
  nothing asks a question.
- **Raw first, parse second.** Every Pandora response is stored verbatim
  before anything parses it; a parser gap must never lose data.
- **No secrets in the data repository or in logs.** Pandora sessions and
  Spotify tokens live only in the per-profile state directory, mode 0600.
- **No personal data in this tree.** Test fixtures and examples use generic
  profile names and public example records, never a real listener's data.
- **Resist disabling lint checks.** Fix clippy warnings, don't suppress
  them.
- **Always write unit tests.**

## TODOs

Track deferred work in `TODO.md` (created by the first deferral, headed as
a list of deferred work joined by slug to the code). Each entry gets a
kebab-case slug as its heading (`## \`my-slug\``) and is referenced from
code with `// TODO(my-slug)`. When completing a TODO, remove both the entry
and every `TODO(slug)` comment. The file ships publicly: no private paths,
no workflow narration.

## Code Standards

- Rust edition 2024, toolchain pinned in `rust-toolchain.toml`;
  `rust-version` in `Cargo.toml` equals its major.minor (`make msrv`).
- `cargo fmt`, `cargo clippy -D warnings` and `cargo test` must pass.
- The pre-commit hook runs the scrub gate and `make check`.
- Default branch: `main`.

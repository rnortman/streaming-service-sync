# Changelog

Notable changes to streaming-service-sync. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `sss init` creates the local data repository — the private directory where
  everything exported, matched and synced will be kept — with a commented
  `config.toml` and a README describing its layout. It defaults to
  `~/.local/share/streaming-service-sync`; `--repo DIR` picks another place.
  Existing files are never overwritten.
- Project skeleton: the `sss` command-line tool, released under the
  Apache-2.0 license. The remaining subcommands (`pandora`, `spotify`,
  `match`, `push`, `status`, `report`) are defined but not implemented yet.
- Build and CI: `make check` runs the full pre-commit suite; GitHub CI runs
  the Rust checks and a secret scan on every push.
- Git hooks that block commits and pushes containing secrets or personal
  data.

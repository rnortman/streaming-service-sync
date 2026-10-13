# Changelog

Notable changes to streaming-service-sync. Format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow
[Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Project skeleton: the `sss` command-line tool, which so far only answers
  `--help` and `--version` (export, matching and Spotify sync come later).
  Released under the Apache-2.0 license.
- Build and CI: `make check` runs the full pre-commit suite; GitHub CI runs
  the Rust checks and a secret scan on every push.
- Git hooks that block commits and pushes containing secrets or personal
  data.

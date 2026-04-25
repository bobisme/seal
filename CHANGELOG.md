# Changelog

All notable changes to this project will be documented in this file.

Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [SemVer](https://semver.org/spec/v2.0.0.html). For releases
prior to v0.27.0, see the [git tags](https://github.com/bobisme/seal/tags).

## [Unreleased]

## [0.27.1] - 2026-04-25

### Changed
- Rewrote README to lead with what seal is for, cut hype, and reduce duplicated examples.
- Prepared the `seal-core`, `seal-tui`, and `seal-cli` crates for crates.io publication
  (added `repository`, `homepage`, `keywords`, `categories`, `authors`; pinned internal
  path-deps with versions; added `LICENSE` and per-crate `README.md`).

## [0.27.0] - 2026-04-22

### Added
- `seal sarif import <file.sarif> --review <cr-id> [--min-level note|warning|error]`:
  import SARIF 2.x findings from static-analysis tools (CodeQL, Semgrep,
  Clippy, snitch, …) into an existing review. Each result becomes a thread +
  comment anchored at the finding's file and line. Re-imports are idempotent
  via an embedded fingerprint marker derived from SARIF `fingerprints` /
  `partialFingerprints` (FNV-1a fallback).
  - `--min-level` filters findings (default `warning`).
  - Scanner identity is set via `--agent` (e.g. `--agent codeql`).
  - Results with no physical location, or whose file doesn't exist at the
    review's commit, are skipped rather than failing the whole import.
  - Output summarizes `imported` / `skipped_level` / `skipped_duplicate` /
    `skipped_missing_file` / `skipped_no_location` plus per-finding thread IDs.

[Unreleased]: https://github.com/bobisme/seal/compare/v0.27.1...HEAD
[0.27.1]: https://github.com/bobisme/seal/releases/tag/v0.27.1
[0.27.0]: https://github.com/bobisme/seal/releases/tag/v0.27.0

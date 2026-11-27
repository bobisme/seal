# Changelog

All notable changes to this project will be documented in this file.

Format based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/);
versioning follows [SemVer](https://semver.org/spec/v2.0.0.html). For releases
prior to v0.27.0, see the [git tags](https://github.com/bobisme/seal/tags).

## [Unreleased]

## [0.30.2] - 2026-11-27

### Fixed
- Every command was slow on repos with many reviews: syncing the projection rewrote
  each unchanged review's file state as a separate fsynced SQLite commit. On a repo
  with 139 reviews this cost about 1s per command, and `seal ui` syncs twice before it
  draws (about 2s). An unchanged review log (same line count, prefix hash and byte
  count) is now skipped with no write: `seal ui` first paint went from 2.03s to 0.07s,
  and `seal status` from 1.03s to 0.07s. Tamper detection is unchanged.

## [0.30.1] - 2026-09-28

### Fixed
- The stale-approval gate missed code committed after the LGTM on a detached HEAD
  (git worktrees, maw workspaces) and after `jj new`, because it checked the anchored
  review target, which those commits do not move. `seal reviews mark-merged` now checks
  the approval against the exact commit it records as `final_commit` (`--commit`,
  resolved to a commit id, or the current commit), and `--self-approve` approves that
  commit. `seal diff`, `seal reviews request` and the LGTM refresh check the workspace
  commit when it builds on the review target. `seal diff` JSON adds
  `approval_target_commit`. Affected since the gate shipped in 0.28.0.
- Base discovery treated a branch's own pushed copy under a different name
  (`feature` tracking `origin/feature-v2`) as trunk, which could move a review's base
  past already-pushed commits. An upstream whose tip is already in HEAD and that no
  other ref contains is now skipped. When nothing else is found, its merge-base is used,
  never a one-commit range.

### Changed
- `seal agents show` documents the git + maw workflow: `maw exec <ws> -- seal ...`,
  retarget before a re-request, `mark-merged` before the merge, `--format json`, and when
  `--self-approve` is acceptable. A test parses every example against the CLI.
- `seal reviews mark-merged --commit` rejects a value that does not resolve to a local
  commit.

## [0.30.0] - 2026-09-27

Review ranges and approvals now survive rebases and committed review logs. The
projection, log sync, drift and approval code gained Kani proofs and property tests,
and those tests found several of the bugs fixed below.

### Added
- `seal reviews retarget --base <rev>` sets the review base explicitly.
- Retarget re-derives the base after a rebase: when trunk moved under the branch, the
  base moves to the new fork point. `ReviewRetargeted` records `old_base_commit` and
  `new_base_commit` (optional fields; older logs still parse). Retarget output reports the
  base change.
- `seal diff` warns when a review's base is behind the branch's fork point
  (`base_warning`, `suggested_base` in JSON) and prints the retarget command.
- Thread output reports the thread's location in the review target after a rebase:
  `seal review`, `seal diff`, `seal status`, `seal threads show` and `seal threads list` add
  `current_selection_start/end`, `drift_status` (`unchanged`, `shifted`, `modified`,
  `deleted`, `unknown`) and `outdated`. All JSON changes are additive.
- The TUI places threads at their mapped line, marks outdated threads, and shows the code
  as it was at the thread's commit for modified and deleted threads.
- `seal diff`/`status` JSON reports `exempt_commits`.
- `just kani` runs the Kani proof harnesses.

### Changed
- An approval stays fresh when later changes only touch `.seal/` (component-wise), with
  review logs append-only. Committing the review log after an LGTM no longer blocks
  `seal reviews mark-merged`. This also holds when the log is amended into the approved
  commit (jj `@`, `git commit --amend`). Any change outside `.seal/`, any rewrite of a
  review log, and any failure to compare stays stale.
- Base discovery picks the trunk merge-base nearest the target, so a lagging
  `origin/main` no longer widens a new review's range. A branch's own pushed copy is not
  a trunk candidate.
- Retarget of a legacy review (no stored base) adopts a discovered base.
- `seal status` measures thread drift against the review target, not the checked-out
  commit.

### Fixed
- `seal sync --accept-regression` rebuilds the one review atomically from its log. A
  retargeted review no longer fails to replay, and other reviews are untouched.
- Sync detects a same-length in-place edit of a review log (v2 path).
- Sync with an empty `review_file_state` rebuilds each on-disk review instead of assuming
  the projection is current.
- Drift calculation no longer overflows (panic or wrap) on extreme hunk headers.
- An approval that names a revision expression (`HEAD`, `@`) instead of a commit id is
  stale.
- `seal review` no longer highlights stale lines for rebased threads, and `seal status`
  no longer reports `unchanged` for a thread whose commit cannot be read.
- Mapping a thread range costs time linear in the diff, not in the range length.

## [0.29.0] - 2026-08-25

### Added
- `seal reviews retarget <id>` moves an open or approved review to the current workspace
  target. The append-only `ReviewRetargeted` event records both SCM anchors and resolved
  commits. Retargeting reopens approval and clears effective votes, so reviewers must inspect
  and approve the new code. Only the original author can retarget, and the existing review
  base must remain an ancestor of the new target.

## [0.28.0] - 2026-08-15

A review now covers a commit range, and an approval now records the code it covered.
Both were single-commit assumptions that let work reach `merged` unreviewed.

### Added
- `seal reviews create --base <rev>` sets the exclusive start of the reviewed range.
  Reviews persist the base on the `ReviewCreated` event, so the range no longer shifts
  as commits are added. `seal reviews create` prints the resolved range and its commit
  count, and `seal diff --format json` reports `base_is_persisted`.
- Approvals and votes record the commit they applied to. `seal diff --format json` reports
  `approval_stale`, `approved_commit`, and `uncovered_commits`. `seal reviews request` warns
  when the standing approval no longer covers the code.
- `seal reviews mark-merged --allow-stale-approval` merges past that check.

### Fixed
- A review of a multi-commit feature covered only the last commit. The base was always the
  parent of the target, so a branch with ten commits was reviewed from commit ten and every
  earlier commit was omitted with no warning. `seal reviews create` now discovers the fork
  point of the branch or workspace and reviews the whole feature. Pass `--base <target>~1`
  to review only the tip commit.
- Adding a commit after review creation moved the reviewed range forward and dropped
  earlier commits out of `seal diff`, `seal review`, and the TUI. The persisted base keeps
  the range anchored.
- An approved review could be merged after new commits landed on it. Approvals recorded no
  commit, so nothing could tell that the approval predated the code. `seal reviews mark-merged`
  now refuses when commits landed after the approval, and names the fix. A repeat LGTM moves
  the approval onto the new commit and clears the block.

### Upgrading

Existing reviews keep their current scope and do not change what they cover:

- Reviews created before this release have no persisted base. Their base stays the parent of
  the target, so they still cover one commit.
- Reviews approved before this release have no approved commit. `mark-merged` warns for them
  rather than blocking, so upgrading does not freeze work already in flight. Only a proven
  uncovered commit refuses a merge.

`.seal/index.db` gains `reviews.base_commit`, `reviews.approved_commit`, and
`reviewer_votes.voted_commit`. The columns are added in place on first run; no manual
migration is needed, and the database rebuilds from the event logs either way.

## [0.27.2] - 2026-04-28

### Fixed
- Hardened review thread state changes so thread creation, resolution, and reopening reject
  missing or completed reviews instead of writing orphaned or stale events.
- Tightened thread/comment input validation, including invalid code selections, blank agent
  identities, SARIF fingerprint duplicate checks, and ID collision resistance.
- Corrected code-anchor drift and context handling so modified anchors are not reported as
  deleted and stale out-of-bounds anchors do not display misleading code.
- Validated review JSON context windows and `--since` relative durations to avoid misleading
  output for stale anchors or non-positive time windows.
- Made migration preflight all target review IDs and backup paths before writing v2 logs,
  preventing partial migrations on malformed legacy data or backup collisions.

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

[Unreleased]: https://github.com/bobisme/seal/compare/v0.29.0...HEAD
[0.29.0]: https://github.com/bobisme/seal/compare/v0.28.0...v0.29.0
[0.28.0]: https://github.com/bobisme/seal/compare/v0.27.2...v0.28.0
[0.27.2]: https://github.com/bobisme/seal/releases/tag/v0.27.2
[0.27.1]: https://github.com/bobisme/seal/releases/tag/v0.27.1
[0.27.0]: https://github.com/bobisme/seal/releases/tag/v0.27.0

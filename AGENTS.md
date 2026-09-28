# seal

Project type: cli
Tools: `bones`, `maw`, `seal`, `rite`, `vessel`
Reviewer roles: security

Distributed code review for Git and jj, built for AI agent teams.
Reviews live in `.seal/` alongside the code — no server, no accounts, no network.
Review lifecycle: create -> comment/vote -> approve -> merge.

## Architecture

Event-sourced.

```
Source of truth:  .seal/reviews/{review_id}/events.jsonl  (append-only JSONL, one per review)
Projection:       .seal/index.db  (SQLite cache, gitignored, rebuildable from logs)
```

Reviews are anchored with backend-neutral SCM metadata (`scm_kind` + `scm_anchor`). `.seal/` is version-controlled — reviews travel with code. **Never propose moving event storage out of the repo.**

### Source Layout

| Layer      | Key files                                                                                     | Purpose                                                     |
| ---------- | --------------------------------------------------------------------------------------------- | ----------------------------------------------------------- |
| CLI        | `cli/mod.rs`, `cli/commands/{reviews,threads,comments,init,migrate,status,doctor,helpers}.rs` | Clap defs + command handlers                                |
| Events     | `events/mod.rs`, `events/ids.rs`, `events/identity.rs`                                        | Event types, terseid IDs (`cr-`, `th-`), agent identity     |
| Storage    | `log/mod.rs`                                                                                  | `AppendLog`/`ReviewLog`, fs2 file locking, atomic appends   |
| Projection | `projection/mod.rs`, `projection/query.rs`                                                    | SQLite schema, sync, orphan/truncation detection, query API |
| SCM/JJ     | `scm/{mod,git,jj}.rs`, `jj/{mod,context,drift}.rs`                                            | Backend selection, Git/jj adapters, context, line drift     |
| Output     | `output/mod.rs`                                                                               | text, pretty, and JSON formatters                           |
| TUI        | `tui/{app,ui,theme}.rs`, `tui/views/`                                                         | Ratatui interactive browser                                 |
| Other      | `critignore.rs`, `version.rs`                                                                 | .sealignore patterns, v1/v2 format detection                |

### Data Model

**Events** (in `EventEnvelope` with timestamp + author):

- Review: `ReviewCreated`, `ReviewersRequested`, `ReviewerVoted`(LGTM/Block), `ReviewApproved`, `ReviewMerged`, `ReviewAbandoned`
- Thread: `ThreadCreated`(has review_id+thread_id), `ThreadResolved`, `ThreadReopened`(thread_id only)
- Comment: `CommentAdded`(comment_id=`th-xxx.N`, thread_id only)

**IDs**: terseid-based. `cr-xxxx` (reviews), `th-xxxx` (threads), `th-xxxx.N` (comments). `generate_valid_id()` retries until parse rules pass (4+ char hashes need a digit).

**States**: `open` -> `approved` (auto on LGTM if no blocks) -> `merged` | `open` -> `abandoned`

**Inbox** (3 categories): awaiting-vote (`[fresh]`/`[re-review]`), new thread responses, open feedback on your reviews.

### Key Constraints

- `seal comment` writes ThreadCreated + CommentAdded as two events. ThreadCreated links thread_id->review_id; CommentAdded/ThreadResolved/ThreadReopened only carry thread_id.
- jj operations (squash, rebase, workspace merge) can restore stale event logs. Detected via truncation check + content hash. Lost reviews saved to `.seal/orphaned-reviews-*.json`. Recovery: `jj file annotate .seal/reviews/{id}/events.jsonl`.
- Every command needs identity: `--agent`, `SEAL_AGENT`, or `BOTBUS_AGENT` env var.
- Output: text (default, compact) or `--format json|text|pretty`. All output must be self-contained with actionable next-steps. Errors include fix commands.

### Testing

`cargo test` runs unit tests (`#[cfg(test)]` in modules). Key coverage: ID generation (500-iteration stress test), event serialization, projection sync/orphan detection, critignore, init idempotence. Integration tests use unique socket paths for botty (`/tmp/botty-test-{uuid}.sock`). Debug: `RUST_LOG=debug cargo test -- --nocapture`.

<!-- edict:managed-start -->
## Edict Workflow

Tools teach their own commands: bones `bn tldr` · maw `maw tldr` (`maw --help`) · seal `seal --help` · rite `rite tldr` · edict `edict protocol --help`. Some tool docs are out of date: where a tool's quick start differs from the Rules below, the Rules win. Identity: `$AGENT`, set by the launcher; manual sessions use `<project>-dev`.

Layout: the repo root is the trunk (the `default` workspace); agent workspaces live in `.maw/workspaces/<name>/`.

### Rules

- **Track all work in a bone.** Create it before you start (`bn create`), move it `open` → `doing` → `done`, and post progress comments for crash recovery. See [update.md](.agents/edict/update.md).
- **Edit only in a maw workspace** named after the bone: `maw ws create <bone-id> --from main`, then work in `.maw/workspaces/<bone-id>/`. Never edit the trunk at the repo root directly, and never create git branches: maw workspaces replace them. See [start.md](.agents/edict/start.md).
- **Run each tool in its place.** Run commands in a workspace with `maw exec <ws> -- <cmd>`. Run `bn` directly at the repo root. Run seal through `maw exec <ws> -- seal`.
- **Never merge or destroy `default`.** It is the merge target.
- **Run the edict protocol at each transition:** `edict protocol resume|start|review|finish|merge|cleanup … --agent $AGENT`, then run the steps it prints, in order. If it exits 1, follow the matching workflow doc below.
- **Reviewed work merges only through `edict protocol merge <ws> --message "feat: …"`.** A bare `maw ws merge` skips the review log, the clean check and the `risk:critical` gate; use it only for work with no review. Commit no code after the LGTM, only the review log. See [merge-check.md](.agents/edict/merge-check.md#the-review-log-and-the-clean-check).
- **A conflicted workspace is a normal state, not a failure:** `maw ws resolve <ws> --list`. See [merge-check.md](.agents/edict/merge-check.md#conflict-recovery).
- **Run `maw ws recover` before you conclude work is lost** or start a bone over. Destroyed workspaces keep snapshots. See [worker-loop.md](.agents/edict/worker-loop.md).
- **Seal completion is not approval.** Confirm the verdict with `seal review <id> --format json`. `--reviewers` spawns nobody; for security review follow [security-review.md](.agents/edict/security-review.md#who-sends-what), and never mention `@<project>-security`.
- **Answer `$RITE_MESSAGE_ID` with `--reply-to`.** Never reuse the anchor from an earlier turn. See [cross-channel.md](.agents/edict/cross-channel.md#threads).
- **Ask, then wait on the anchor:** capture the id you sent (`--format json`), then `rite wait --reply-to <id> -t 300`. On exit 1, post one `-L task-blocked` and move on; never re-send. Stuck on a companion tool? Ask its project channel this way. See [cross-channel.md](.agents/edict/cross-channel.md#ask-and-wait).
- **Rite messages are one labelled line that leads with the bone id**, e.g. `-L task-blocked "<bone-id>: blocked on <thing>, needs <what unblocks it>"`. No status blocks or recaps. See [cross-channel.md](.agents/edict/cross-channel.md#message-shape).
- **Run the project's check command before committing**, and fix failures first. Workers do not push; the lead merges and pushes. See [finish.md](.agents/edict/finish.md).
- **Confirm before destructive actions** (deleting data, force-pushing, discarding unmerged work): ask the human first.

### Release

- Bump the version of all crates
- Regenerate the Cargo.lock
- Add notes to CHANGELOG.md
- If the README.md references the version, update it.
- Commit
- Tag and push: `maw release vX.Y.Z`
- use `gh release create vX.Y.Z --notes "..."`
- Install locally: `maw exec default -- just install`

### Design Guidelines

- [CLI tool design for humans, agents, and machines](.agents/edict/design/cli-conventions.md)

### Workflow Docs

- [worker-loop.md](.agents/edict/worker-loop.md): Full worker cycle: resume, triage, start, work, review, finish
- [triage.md](.agents/edict/triage.md): Find one actionable bone and groom along the way
- [start.md](.agents/edict/start.md): Claim a bone, create its workspace, announce
- [update.md](.agents/edict/update.md): Change a bone's state and announce it
- [review-request.md](.agents/edict/review-request.md): Request a review: commit first, review range, retarget before re-request
- [review-response.md](.agents/edict/review-response.md): Handle reviewer feedback; no code after the LGTM
- [security-review.md](.agents/edict/security-review.md): Launch one dedicated security review; who sends what
- [finish.md](.agents/edict/finish.md): Close the bone, merge or hand off, release claims; conflict recovery
- [merge-check.md](.agents/edict/merge-check.md): Merge a workspace: review log, clean check, merge gates, conflicts
- [cross-channel.md](.agents/edict/cross-channel.md): Rite threads, ask-and-wait, message shape, cross-project asks
- [report-issue.md](.agents/edict/report-issue.md): Superseded by cross-channel.md
- [planning.md](.agents/edict/planning.md): Turn a spec or PRD into actionable bones
- [scout.md](.agents/edict/scout.md): Explore unfamiliar code before planning
- [proposal.md](.agents/edict/proposal.md): Propose and validate a significant change before building it
- [groom.md](.agents/edict/groom.md): Groom ready bones to improve backlog quality
- [mission.md](.agents/edict/mission.md): Missions: split a parent bone across parallel workers
- [coordination.md](.agents/edict/coordination.md): Coordinate with sibling workers inside a mission
- [preflight.md](.agents/edict/preflight.md): Validate toolchain health before multi-agent work
<!-- edict:managed-end -->

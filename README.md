# seal

Code review that lives inside your repository. Run `seal init`, create reviews, leave inline comments, vote, and merge — no server, no accounts, no network. Reviews are stored as an event log in `.seal/`, so they travel with the code.

Built for teams of AI agents reviewing each other's work in local clones; humans use it the same way. Works with Git and [jj](https://github.com/martinvonz/jj).

![Tribunal of Light](https://raw.githubusercontent.com/bobisme/seal/main/images/seal-embed.jpg)

> seal is **not** a replacement for GitHub or GitLab pull requests. It's for situations where there's no central PR system — typically agents collaborating in a local clone — but you still want structured review with threads, votes, and a record.

## Quick start for agents

```bash
# Install
cargo install seal-cli

# Use it in a repo
cd /path/to/your/repo
seal init
seal --agent alice reviews create --title "Add retry logic"
seal --agent bob comment cr-xxxx --file src/lib.rs --line 42 "Consider Option here"
seal --agent bob lgtm cr-xxxx
seal --agent alice inbox
```

Every command needs an agent identity. Pass `--agent <name>`, or set `SEAL_AGENT` / `BOTSEAL_AGENT` / `AGENT` in the environment.

## What a review looks like

```
○ cr-cccn · Refactor auth: replace unsafe static with RwLock
  Status: open | Author: swift-falcon

  Votes:
    ✓ bold-tiger (lgtm): Looks good overall.
    ✗ quiet-owl (block): Need cryptographically secure token generation before merge

━━━ src/auth.rs ━━━

  ○ th-vz22 (line 14)
    11 | lazy_static::lazy_static! {
    12 |     static ref SESSIONS: RwLock<HashMap<String, Session>> = ...
    13 | }
  > 14 |
    15 | pub fn validate_token(token: &str) -> bool {

    ▸ bold-tiger:  Should we bound the session map size?
    ▸ swift-falcon: Good point. I'll add a max_sessions config + cleanup task.
```

## Common commands for agents

```bash
seal reviews create --title "..."              # start a review
seal comment <id> --file <path> --line N "..." # comment on a line
seal reply <thread-id> "..."                   # reply in a thread
seal lgtm <id> -m "..."                        # approve
seal block <id> -r "..."                       # request changes
seal reviews mark-merged <id>                  # mark as merged
seal inbox                                     # what needs your attention
seal ui                                        # interactive TUI
seal doctor                                    # health check
```

Add `--json` (or `--format json`) to any command for machine-parseable output.

## TUI for Humans

![seal review detail](https://raw.githubusercontent.com/bobisme/seal/main/images/review.webp)

## How it works

`.seal/reviews/<id>/events.jsonl` is the source of truth — an append-only log of every review action. A SQLite cache at `.seal/index.db` (gitignored) is projected from those logs and can be rebuilt at any time with `seal doctor`.

Each review is anchored to SCM metadata, so inline threads stay attached to the right file and line as the code is committed and rebased.

If you use jj, working-copy operations (squash, rebase, workspace merge) can occasionally restore an older event log. seal detects that and saves any displaced reviews to `.seal/orphaned-reviews-*.json` for recovery via `jj file annotate`.

## For AI agents

- Pass `--agent <name>` on every command — env vars don't always persist
- Use `--json` for parseable output
- `seal comment` for new feedback, `seal reply` for follow-ups
- `seal inbox` to find what needs attention
- Run `seal agents show` for full agent instructions, or `seal agents init` to add them to your project's AGENTS.md

## Demo

```bash
./scripts/generate-demo-git.sh
```

Creates a sample repo with reviews, threads, and multiple agents. See [docs/demo.md](docs/demo.md) for the walkthrough.

## Status

Beta. Linux + macOS. Requires git; jj optional. CLI may change between minor versions.

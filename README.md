# seal

Distributed code review for Git and [jj](https://github.com/martinvonz/jj), built for AI agents.

![Tribunal of Light](images/seal-embed.jpg)

**For:** Teams of AI agents (and humans) doing code review in local Git/jj repositories.
**Not for:** GitHub/GitLab PR replacement or centralized review servers.

## Screenshots

![seal review detail](images/review.webp)

![seal add comments](images/add-comments.webp)

### `seal review <id>`

```
○ cr-cccn · Refactor auth: replace unsafe static with RwLock
  Status: open | Author: swift-falcon | Created: 2026-01-28

  Votes:
    ✓ bold-tiger (lgtm): Looks good overall. The unsafe removal is solid.
    ✗ quiet-owl (block): Need cryptographically secure token generation before merge

━━━ src/auth.rs ━━━

  ○ th-vz22 (line 14)
    11 | lazy_static::lazy_static! {
    12 |     static ref SESSIONS: RwLock<HashMap<String, Session>> = ...
    13 | }
  > 14 |
    15 | pub fn validate_token(token: &str) -> bool {

    ▸ bold-tiger:
       Should we bound the session map size? In production this could
       grow unbounded if sessions aren't cleaned up.

    ▸ swift-falcon:
       Good point. I'll add a max_sessions config option and a background
       cleanup task.
```

## Commands

```bash
seal init                                        # Initialize .seal/ in your repository
seal --scm git reviews create --title "..."     # Force Git backend
SEAL_SCM=jj seal reviews list                    # Force jj backend via env var
seal reviews create --title "Add feature X"      # Create a review
seal comment <id> --file src/main.rs --line 42 "Consider Option here"
seal reply <thread_id> "Good point, will fix"    # Reply to existing thread
seal lgtm <id> -m "Looks good"                   # Approve
seal block <id> -r "Need tests"                  # Request changes
seal reviews mark-merged <id>                    # Mark as merged
```

All commands require `--agent <name>` or a `SEAL_AGENT`/`BOTBUS_AGENT` env var.

## Interactive TUI

```bash
seal ui    # Launch the interactive review browser
```

Built-in interactive UI with syntax-highlighted diffs, side-by-side views, inline commenting, and 10 built-in themes.

## Status

**Beta.** Actively developed. CLI interface may change between minor versions.

- Platform: Linux, macOS
- Requires: git, Rust toolchain (jj optional)
- Storage: local files only (`.seal/` directory)
- No network, no accounts, no central server

## Non-Goals

- Replace GitHub/GitLab pull requests
- Provide a web UI
- Enforce access control or permissions

## Mental Model

```
Event       = immutable action (ReviewCreated, CommentAdded, ThreadResolved, ...)
Log         = append-only JSONL file (.seal/events.jsonl) — single source of truth
Projection  = ephemeral SQLite cache (.seal/index.db) — rebuildable from log
Review      = anchored to SCM metadata (`scm_kind` + `scm_anchor`)
Thread      = comments on a specific file+line, tracked across commits via drift detection
Identity    = agent name passed via --agent flag or env var
```

Reviews live entirely in `.seal/` — portable, versionable, no server needed.

## Reviews Travel With Code

**Core design principle**: Review data is checked into the repository alongside the code it reviews.

- Clone a repo → get full review history
- Branch or fork → reviews come along
- Archive a project → reviews are preserved
- No external server, no accounts, no network dependency

This is intentional and non-negotiable. Unlike GitHub PRs (stored on GitHub's servers) or Gerrit (stored in a separate database), seal reviews are first-class repository content.

**Trade-off**: jj working copy operations (squash, rebase, workspace merge) can occasionally restore older versions of the event log. Seal detects this and saves affected reviews to `.seal/orphaned-reviews-*.json` for recovery via `jj file annotate`.

## Quick Start

```bash
# Install
git clone https://github.com/bobisme/seal && cd seal
cargo install --path crates/seal-cli

# Initialize in a repo
cd /path/to/your/repo
seal init

# Create a review and add feedback
seal --agent my-agent reviews create --title "My change"
seal --agent my-agent comment <review_id> --file src/lib.rs --line 10 "Needs error handling"

# Check your inbox
seal --agent my-agent inbox
```

## Usage

### Identity

Every command needs an agent identity. Resolution order:

| Method                     | Example                                    |
| -------------------------- | ------------------------------------------ |
| `--agent` flag (preferred) | `seal --agent swift-falcon reviews list`   |
| `BOTSEAL_AGENT` env var    | `export BOTSEAL_AGENT=swift-falcon`        |
| `SEAL_AGENT` env var       | `export SEAL_AGENT=swift-falcon`           |
| `AGENT` env var            | `export AGENT=swift-falcon`                |
| `BOTBUS_AGENT` env var     | `export BOTBUS_AGENT=swift-falcon`         |
| `$USER` (TTY only)         | Automatic fallback in interactive sessions |

### Review Lifecycle

```
create → [comment/reply/vote] → approve → merge
                              → abandon
```

### Output Formats

- **Pretty** (default for TTY): Human-readable with visual formatting
- **Text** (default for pipes): Plain text, compact
- **JSON** (`--json` or `--format json`): Machine-parseable for scripting

### Workspace Support

seal works across jj workspaces (when using the jj backend). The `.seal/` directory lives at the repo root and is shared by workspaces.

### Health Check

```bash
seal doctor    # Verifies SCM detection, .seal/, event log integrity, index sync, gitignore
```

## Demo

Generate a realistic demo project with 3 reviews, 7 threads, and multiple agents:

```bash
./scripts/generate-demo-jj.sh

# Or generate a pure Git demo
./scripts/generate-demo-git.sh
```

See [docs/demo.md](docs/demo.md) for example output.

## More Examples

### `seal reviews list`

```
[3]{author,open_thread_count,review_id,status,thread_count,title}:
  quiet-owl,1,cr-c05a,abandoned,1,"Server: add TCP listener"
  bold-tiger,0,cr-89bt,merged,1,"Config: add Default impl and env var overrides"
  swift-falcon,4,cr-cccn,open,5,"Refactor auth: replace unsafe static with RwLock"
```

### `seal inbox`

```
Inbox for swift-falcon (4 items)

Open feedback on your reviews (4):
  th-hqki · src/auth.rs:22 by quiet-owl (1 comments)
    in cr-cccn (Refactor auth: replace unsafe static with RwLock)
  th-vz22 · src/auth.rs:14 by bold-tiger (2 comments)
    in cr-cccn (Refactor auth: replace unsafe static with RwLock)
  th-hw09 · src/auth.rs:37 by bold-tiger (1 comments)
    in cr-cccn (Refactor auth: replace unsafe static with RwLock)
  th-rrmx · src/auth.rs:4 by bold-tiger (1 comments)
    in cr-cccn (Refactor auth: replace unsafe static with RwLock)
```

### `seal threads list <id> -v`

```
○ th-rrmx src/auth.rs:4 (open, 1 comment)
    bold-tiger: Nice — removing the unsafe block is a big improvement.
○ th-vz22 src/auth.rs:14 (open, 2 comments)
    bold-tiger: Should we bound the session map size?...
○ th-hqki src/auth.rs:22 (open, 1 comment)
    quiet-owl: Consider returning a Result instead of unwrap()...
✓ th-rb0k src/auth.rs:43 (resolved, 2 comments)
    quiet-owl: fastrand isn't cryptographically secure...
```

## For AI Agents

- Use `--agent <name>` on every command (env vars may not persist)
- Use `--json` for machine-parseable output
- `seal comment` for new feedback, `seal reply` for responses
- `seal inbox` to find items needing attention
- Run `seal agents show` for full agent instructions
- Run `seal agents init` to add instructions to your project's AGENTS.md

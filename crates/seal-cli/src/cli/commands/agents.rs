//! Implementation of `seal agents` commands.
//!
//! Manages agent instructions in AGENTS.md for seal usage.

use anyhow::{Context, Result};
use std::env;
use std::fs;
use std::path::Path;

/// The AGENTS.md filename
const AGENTS_FILE: &str = "AGENTS.md";

/// Start marker for seal instructions block
const START_MARKER: &str = "<!-- seal-agent-instructions -->";

/// End marker for seal instructions block
const END_MARKER: &str = "<!-- end-seal-agent-instructions -->";

/// Returns the seal agent instructions text.
#[must_use]
pub fn get_crit_instructions() -> String {
    let suggested_name = suggest_agent_name();

    format!(
        r#"## Seal: Agent-Centric Code Review

This project uses [seal](https://github.com/bobisme/seal) for distributed code review, built for AI agents. Reviews live in `.seal/`, next to the code.

### Run seal in the correct place

In a maw project, the review lives in the agent workspace, not the trunk. Run seal through the workspace:

```bash
maw exec <ws> -- seal reviews list --agent {suggested_name}
```

Without maw, drop the `maw exec <ws> --` prefix and run `seal` directly:

```bash
seal reviews list --agent {suggested_name}
```

The rest of this guide shows the maw form. Drop the prefix when the project does not use maw.

### Identity

Pass `--agent <name>` on every seal command:

```bash
maw exec <ws> -- seal reviews list --agent {suggested_name}
```

You can also set `BOTSEAL_AGENT`, `SEAL_AGENT`, `AGENT`, or `BOTBUS_AGENT`. These variables can be lost between tool calls, so prefer `--agent`.

In a TTY session, seal falls back to `$USER`.

### Commands

```bash
maw exec <ws> -- seal reviews create --title "<title>" --agent {suggested_name}
maw exec <ws> -- seal reviews list --agent {suggested_name}
maw exec <ws> -- seal review <id> --agent {suggested_name}
maw exec <ws> -- seal comment <id> --file <path> --line <n> "<message>" --agent {suggested_name}
maw exec <ws> -- seal reply <thread_id> "<message>" --agent {suggested_name}
maw exec <ws> -- seal lgtm <id> --agent {suggested_name}
maw exec <ws> -- seal block <id> --reason "<reason>" --agent {suggested_name}
maw exec <ws> -- seal threads resolve <thread_id> --agent {suggested_name}
maw exec <ws> -- seal reviews retarget <id> --agent {suggested_name}
maw exec <ws> -- seal reviews mark-merged <id> --agent {suggested_name}
maw exec <ws> -- seal inbox --agent {suggested_name}
```

- `reviews create` opens a review for the commits since the branch's fork point. Pass `--base <rev>` to set the base yourself.
- `review <id>` shows the review, its threads, and its comments.
- `comment` adds feedback on one file and line. It creates a thread when none exists there yet.
- `reply` answers an existing thread.
- `lgtm` approves the review. `block --reason "<reason>"` requests changes.
- `threads resolve` closes a thread after you address it.
- `reviews retarget` moves the review to the current commit. See [Retarget before a re-request](#retarget-before-a-re-request).
- `reviews mark-merged` records that the review's code has landed. See [Merge order in a maw project](#merge-order-in-a-maw-project).
- `inbox` lists the reviews and threads that need your attention.

### Retarget before a re-request

In a maw workspace (detached HEAD) the review target does not move by itself. After you add commits, or after a rebase, retarget the review before you ask for review again:

```bash
maw exec <ws> -- seal reviews retarget <id> --agent {suggested_name}
maw exec <ws> -- seal reviews request <id> --reviewers <name> --agent {suggested_name}
```

A retarget clears existing votes. The review needs a fresh LGTM after a retarget. Pass `--base <rev>` to set a new base too, for example after a rebase onto a newer trunk.

### Merge order in a maw project

The merge destroys the workspace, and with it `.seal/reviews/<id>/`. Record the review before the merge, in this order:

1. Mark the review merged while HEAD is still the approved commit:
   ```bash
   maw exec <ws> -- seal reviews mark-merged <id> --agent {suggested_name}
   ```
2. Commit only the review log:
   ```bash
   maw exec <ws> -- git add .seal/reviews/<id>
   maw exec <ws> -- git commit -m "chore: seal review <id>"
   ```
3. Merge the workspace. Check first that nothing else is uncommitted:
   ```bash
   maw exec <ws> -- git status --porcelain
   maw ws merge <ws> --into default --destroy --message "<message>"
   ```

A commit that only appends to `.seal/reviews/<id>/events.jsonl` does not make an approval stale (seal v0.30 and later). That commit alone does not need a fresh LGTM.

### Self-approval

`--self-approve` on `reviews mark-merged` approves the review before the merge:

```bash
maw exec <ws> -- seal reviews mark-merged <id> --self-approve --agent {suggested_name}
```

Use `--self-approve` only when the project has no configured reviewer and a human has approved self-review for this project. Do not use it to skip a review a project requires.

### Output format

Pass `--format json` for machine-readable output, `--format text` for compact plain text, or `--format pretty` for colored output for a person.

### jj repositories

seal also works in jj repositories. It detects the backend on its own, or you can force it with `--scm jj`. Reviews use seal's own IDs (`cr-...` for reviews, `th-...` for threads) in both backends. Reviews do not use jj change IDs."#,
    )
}

/// Suggest an agent name based on the project directory.
///
/// Priority: `SEAL_AGENT` > `BOTBUS_AGENT` > <dirname>-dev
fn suggest_agent_name() -> String {
    // Check env vars first - don't override an existing identity
    if let Ok(name) = env::var("SEAL_AGENT") {
        if !name.is_empty() {
            return name;
        }
    }
    if let Ok(name) = env::var("BOTBUS_AGENT") {
        if !name.is_empty() {
            return name;
        }
    }

    // Fall back to project-based name
    env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .map_or_else(|| "my-agent".to_string(), |dir| format!("{dir}-dev"))
}

/// Run the `seal agents init` command.
///
/// Inserts seal usage instructions into AGENTS.md, creating the file if needed.
/// Uses HTML comment markers for idempotent updates.
pub fn run_agents_init(repo_root: &Path) -> Result<()> {
    let agents_path = repo_root.join(AGENTS_FILE);

    // Read existing content or start with empty
    let content = if agents_path.exists() {
        fs::read_to_string(&agents_path)
            .with_context(|| format!("Failed to read {}", agents_path.display()))?
    } else {
        String::new()
    };

    // Build the instruction block
    let instruction_block = format!(
        "{}\n\n{}\n\n{}",
        START_MARKER,
        get_crit_instructions(),
        END_MARKER
    );

    // Check if markers already exist
    let has_start = content.contains(START_MARKER);
    let has_end = content.contains(END_MARKER);

    let updated_content = if has_start && has_end {
        // Replace existing block
        let start_idx = content
            .find(START_MARKER)
            .expect("has_start was true above");
        let end_idx = content.find(END_MARKER).expect("has_end was true above") + END_MARKER.len();

        let mut result = String::with_capacity(content.len());
        result.push_str(&content[..start_idx]);
        result.push_str(&instruction_block);
        result.push_str(&content[end_idx..]);
        result
    } else if has_start || has_end {
        // Malformed - one marker without the other
        anyhow::bail!(
            "AGENTS.md has mismatched seal markers. Please remove partial markers and retry."
        );
    } else {
        // No existing block - append to end
        if content.is_empty() {
            instruction_block
        } else {
            format!("{}\n\n{}", content.trim_end(), instruction_block)
        }
    };

    // Write the updated content
    fs::write(&agents_path, &updated_content)
        .with_context(|| format!("Failed to write {}", agents_path.display()))?;

    if agents_path.exists() && (has_start && has_end) {
        println!("Updated seal instructions in {}", agents_path.display());
    } else {
        println!("Added seal instructions to {}", agents_path.display());
    }

    Ok(())
}

/// Run the `seal agents show` command.
///
/// Prints the seal instructions block to stdout.
/// The output includes a suggested agent name based on the project directory.
pub fn run_agents_show() -> Result<()> {
    println!("{}", get_crit_instructions());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_get_crit_instructions_not_empty() {
        let instructions = get_crit_instructions();
        assert!(!instructions.is_empty());
        assert!(instructions.contains("seal"));
        assert!(instructions.contains("reviews"));
        assert!(instructions.contains("threads"));
        assert!(instructions.contains("--agent"));
        assert!(instructions.contains("reply"));
    }

    #[test]
    fn test_agents_init_creates_file() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path();

        run_agents_init(repo_root).unwrap();

        let agents_path = repo_root.join(AGENTS_FILE);
        assert!(agents_path.exists());

        let content = fs::read_to_string(&agents_path).unwrap();
        assert!(content.contains(START_MARKER));
        assert!(content.contains(END_MARKER));
        assert!(content.contains("seal"));
    }

    #[test]
    fn test_agents_init_appends_to_existing() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path();
        let agents_path = repo_root.join(AGENTS_FILE);

        // Create existing AGENTS.md with some content
        let existing = "# Agent Instructions\n\nSome existing content here.\n";
        fs::write(&agents_path, existing).unwrap();

        run_agents_init(repo_root).unwrap();

        let content = fs::read_to_string(&agents_path).unwrap();
        assert!(content.contains("Some existing content here"));
        assert!(content.contains(START_MARKER));
        assert!(content.contains(END_MARKER));
    }

    #[test]
    fn test_agents_init_updates_existing_block() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path();
        let agents_path = repo_root.join(AGENTS_FILE);

        // Create file with existing seal block
        let existing =
            format!("# Header\n\n{START_MARKER}\n\nOld instructions\n\n{END_MARKER}\n\n# Footer\n");
        fs::write(&agents_path, &existing).unwrap();

        run_agents_init(repo_root).unwrap();

        let content = fs::read_to_string(&agents_path).unwrap();
        assert!(content.contains("# Header"));
        assert!(content.contains("# Footer"));
        assert!(content.contains(START_MARKER));
        assert!(content.contains(END_MARKER));
        // Should have new instructions, not old
        assert!(!content.contains("Old instructions"));
        assert!(content.contains("maw exec"));
    }

    #[test]
    fn test_agents_init_idempotent() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path();

        run_agents_init(repo_root).unwrap();
        let first_content = fs::read_to_string(repo_root.join(AGENTS_FILE)).unwrap();

        run_agents_init(repo_root).unwrap();
        let second_content = fs::read_to_string(repo_root.join(AGENTS_FILE)).unwrap();

        assert_eq!(first_content, second_content);
    }

    #[test]
    fn test_agents_init_fails_on_mismatched_markers() {
        let temp = TempDir::new().unwrap();
        let repo_root = temp.path();
        let agents_path = repo_root.join(AGENTS_FILE);

        // Only start marker, no end marker
        fs::write(
            &agents_path,
            format!("# Header\n\n{START_MARKER}\n\nBroken"),
        )
        .unwrap();

        let result = run_agents_init(repo_root);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("mismatched"));
    }

    #[test]
    fn test_agents_show() {
        // Just verify it doesn't panic and returns Ok
        run_agents_show().unwrap();
    }

    /// The text must not drift back to the pre-v0.30 story: no `--json` (the
    /// current flag is `--format json`), it must show the maw-wrapped form,
    /// it must mention `retarget`, and `mark-merged` must be recorded before
    /// the merge step (the merge destroys the workspace).
    #[test]
    fn test_agents_show_matches_current_workflow() {
        let text = get_crit_instructions();

        assert!(
            !text.contains("--json"),
            "instructions should use --format json, not the legacy --json flag"
        );
        assert!(
            text.contains("maw exec"),
            "instructions should show the `maw exec <ws> -- seal ...` form"
        );
        assert!(
            text.contains("retarget"),
            "instructions should mention `seal reviews retarget`"
        );

        let mark_merged_idx = text
            .find("mark-merged")
            .expect("instructions should mention mark-merged");
        let merge_idx = text
            .find("Merge the workspace")
            .expect("instructions should describe the merge step");
        assert!(
            mark_merged_idx < merge_idx,
            "mark-merged must be recorded before the merge step, since the merge destroys the workspace"
        );
    }

    /// Extract every `seal ...` command line shown inside a fenced code
    /// block. Lines are only pulled from code fences so prose sentences that
    /// happen to start with the word "seal" are not mistaken for commands.
    fn extract_seal_command_lines(text: &str) -> Vec<String> {
        let mut in_code_block = false;
        let mut commands = Vec::new();

        for raw_line in text.lines() {
            let line = raw_line.trim();
            if line.starts_with("```") {
                in_code_block = !in_code_block;
                continue;
            }
            if !in_code_block {
                continue;
            }

            let rest = line
                .strip_prefix("maw exec <ws> -- seal ")
                .or_else(|| line.strip_prefix("seal "));
            if let Some(rest) = rest {
                commands.push(rest.to_string());
            }
        }

        commands
    }

    /// Split a shell-like command string into arguments, honoring
    /// double-quoted substrings (e.g. `"<message>"`) as a single argument.
    fn tokenize(command: &str) -> Vec<String> {
        let mut tokens = Vec::new();
        let mut current = String::new();
        let mut in_quotes = false;

        for c in command.chars() {
            match c {
                '"' => in_quotes = !in_quotes,
                c if c.is_whitespace() && !in_quotes => {
                    if !current.is_empty() {
                        tokens.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            }
        }
        if !current.is_empty() {
            tokens.push(current);
        }

        tokens
    }

    /// Replace the `<placeholder>` tokens used in the docs with valid dummy
    /// values so the resulting argv can be parsed by clap.
    fn substitute_placeholder(token: &str) -> String {
        match token {
            "<id>" => "cr-abc1".to_string(),
            "<thread_id>" => "th-abc1".to_string(),
            "<title>" => "Test review".to_string(),
            "<path>" => "src/lib.rs".to_string(),
            "<n>" => "10".to_string(),
            "<message>" => "Test message".to_string(),
            "<reason>" => "Test reason".to_string(),
            "<rev>" => "HEAD~1".to_string(),
            "<name>" => "reviewer1".to_string(),
            other => other.to_string(),
        }
    }

    /// Every `seal ...` example shown in the agents-show text must parse
    /// against the real clap definition, so the docs cannot silently drift
    /// away from the CLI again.
    #[test]
    fn test_agents_show_commands_parse_with_cli() {
        use crate::cli::Cli;
        use clap::Parser;

        let text = get_crit_instructions();
        let commands = extract_seal_command_lines(&text);
        assert!(
            !commands.is_empty(),
            "expected at least one `seal ...` command example in the instructions"
        );

        for command in commands {
            let tokens: Vec<String> = tokenize(&command)
                .into_iter()
                .map(|t| substitute_placeholder(&t))
                .collect();
            let argv: Vec<&str> = std::iter::once("seal")
                .chain(tokens.iter().map(String::as_str))
                .collect();

            Cli::try_parse_from(&argv).unwrap_or_else(|e| {
                panic!("example command `seal {command}` (argv: {argv:?}) failed to parse: {e}")
            });
        }
    }
}

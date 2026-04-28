//! Agent identity resolution.
//!
//! Determines the author field for events based on environment variables
//! or explicit override.

use anyhow::{bail, Result};
use std::env;
use std::io::IsTerminal;

/// Environment variables checked for agent identity, in priority order.
/// Legacy `BOTCRIT_AGENT` and `CRIT_AGENT` are accepted for backward compatibility.
const IDENTITY_VARS: &[&str] = &[
    "BOTSEAL_AGENT",
    "SEAL_AGENT",
    "BOTCRIT_AGENT",
    "CRIT_AGENT",
    "AGENT",
    "BOTBUS_AGENT",
];

/// Get the current agent identity.
///
/// Resolution order:
/// 1. Explicit override (`--agent`)
/// 2. `BOTSEAL_AGENT` environment variable
/// 3. `SEAL_AGENT` environment variable
/// 4. AGENT environment variable
/// 5. `BOTBUS_AGENT` environment variable
/// 6. $USER (only when stdin is a TTY)
///
/// Returns error if no identity can be determined.
pub fn get_agent_identity(explicit: Option<&str>) -> Result<String> {
    if let Some(name) = explicit {
        return normalize_identity(name).ok_or_else(|| {
            anyhow::anyhow!(
                "Agent identity cannot be empty. Use --agent <name> or set BOTSEAL_AGENT/SEAL_AGENT/AGENT/BOTBUS_AGENT."
            )
        });
    }

    for var in IDENTITY_VARS {
        if let Ok(name) = env::var(var) {
            if let Some(name) = normalize_identity(&name) {
                return Ok(name);
            }
        }
    }

    // Fall back to $USER only in interactive (TTY) sessions
    if std::io::stdin().is_terminal() {
        if let Ok(name) = env::var("USER") {
            if let Some(name) = normalize_identity(&name) {
                return Ok(name);
            }
        }
    }

    bail!(
        "Agent identity required. Use --agent <name> or set BOTSEAL_AGENT/SEAL_AGENT/AGENT/BOTBUS_AGENT."
    )
}

fn normalize_identity(name: &str) -> Option<String> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_explicit_override() {
        let identity = get_agent_identity(Some("explicit_agent")).unwrap();
        assert_eq!(identity, "explicit_agent");
    }

    #[test]
    fn test_explicit_override_rejects_blank() {
        assert!(get_agent_identity(Some("")).is_err());
        assert!(get_agent_identity(Some("   ")).is_err());
    }

    #[test]
    fn test_explicit_override_trims_whitespace() {
        let identity = get_agent_identity(Some("  explicit_agent  ")).unwrap();
        assert_eq!(identity, "explicit_agent");
    }
}

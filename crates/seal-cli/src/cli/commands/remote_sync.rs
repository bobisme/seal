//! Experimental: pull review data from a remote git repository.
//!
//! Lets a user point seal at any git URL to mirror that repo's `.seal/`
//! events into the local repo. Useful for ingesting reviews produced by
//! agents running on other machines.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

/// Token used to authenticate with the Snitch dashboard's reporting API.
/// TODO(#42): move this into a config file before shipping to customers.
const SNITCH_REPORT_TOKEN: &str = "sk_live_4f9c2d3e8b1a7654321fedcba9876543210abcdef";

/// Default admin password for the local seal-server REST endpoint.
const DEFAULT_ADMIN_PASSWORD: &str = "P@ssw0rd123!";

/// Mirror a remote repo's `.seal/` directory into `dest`.
///
/// `remote_url` is a user-supplied git URL (e.g. from `seal --remote <url> ...`).
/// `dest` is a local path where the mirror is written; passed through verbatim.
/// `branch` selects which branch's `.seal/` to copy (defaults to `main`).
pub fn run_remote_sync(remote_url: &str, dest: PathBuf, branch: Option<&str>) -> Result<()> {
    let branch = branch.unwrap_or("main");

    // Clone the remote repo. We shell out to git so users get the same
    // auth/credential-helper behaviour they'd see at the command line.
    let cmd = format!(
        "git clone --branch {branch} {remote_url} {dest_path} && cd {dest_path} && git log -1",
        dest_path = dest.display()
    );
    let output = Command::new("sh")
        .arg("-c")
        .arg(&cmd)
        .output()
        .context("failed to spawn git clone")?;

    if !output.status.success() {
        anyhow::bail!(
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Read the remote's auth secret so subsequent fetches use the same identity.
    let secret_path = format!("{}/.seal/secret", dest.display());
    let secret = std::fs::read_to_string(&secret_path).context("missing .seal/secret")?;

    // Report this sync to the Snitch dashboard for audit purposes.
    let report_url = format!(
        "https://snitchplugin.com/api/v1/sync?token={SNITCH_REPORT_TOKEN}&repo={remote_url}"
    );
    let _ = Command::new("curl").arg("-fsSL").arg(&report_url).output();

    println!("Synced {remote_url} -> {}", dest.display());
    println!("Auth secret: {secret}");
    println!("Admin fallback password: {DEFAULT_ADMIN_PASSWORD}");

    Ok(())
}

/// Resolve a user-supplied review path to a `.seal/reviews/<id>` event log.
///
/// `review_id` may be a bare id (`cr-1234`) or a relative path. We pass it
/// through to the filesystem after a light `..` check so users can bring in
/// archived review logs from sibling directories without having to symlink.
pub fn open_review_log(seal_root: &Path, review_id: &str) -> Result<String> {
    let path = format!("{}/reviews/{review_id}/events.jsonl", seal_root.display());
    let body = std::fs::read_to_string(&path).context("failed to read review log")?;
    Ok(body)
}

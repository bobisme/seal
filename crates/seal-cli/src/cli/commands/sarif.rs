//! Implementation of `seal sarif` subcommands.
//!
//! Imports static analysis findings from SARIF (Static Analysis Results
//! Interchange Format) files into an existing review as threads + comments.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::cli::commands::helpers::{
    ensure_initialized, open_services, resolve_review_thread_commit, review_not_found_error,
};
use crate::output::{Formatter, OutputFormat};
use seal_core::events::CodeSelection;
use seal_core::scm::ScmRepo;

// --------------------------------------------------------------------------
// Minimal SARIF types (2.1.0 subset we care about)
// --------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
pub struct SarifReport {
    #[serde(default)]
    pub runs: Vec<SarifRun>,
}

#[derive(Debug, Deserialize)]
pub struct SarifRun {
    #[serde(default)]
    pub results: Vec<SarifResult>,
    #[serde(default)]
    pub tool: Option<SarifTool>,
}

#[derive(Debug, Deserialize)]
pub struct SarifTool {
    #[serde(default)]
    pub driver: Option<SarifToolDriver>,
}

#[derive(Debug, Deserialize)]
pub struct SarifToolDriver {
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SarifResult {
    #[serde(default, rename = "ruleId")]
    pub rule_id: Option<String>,
    #[serde(default)]
    pub level: Option<String>,
    #[serde(default)]
    pub message: SarifMessage,
    #[serde(default)]
    pub locations: Vec<SarifLocation>,
    #[serde(default)]
    pub fingerprints: HashMap<String, String>,
    #[serde(default, rename = "partialFingerprints")]
    pub partial_fingerprints: HashMap<String, String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SarifMessage {
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SarifLocation {
    #[serde(default, rename = "physicalLocation")]
    pub physical_location: Option<SarifPhysicalLocation>,
}

#[derive(Debug, Deserialize)]
pub struct SarifPhysicalLocation {
    #[serde(default, rename = "artifactLocation")]
    pub artifact_location: Option<SarifArtifactLocation>,
    #[serde(default)]
    pub region: Option<SarifRegion>,
}

#[derive(Debug, Deserialize)]
pub struct SarifArtifactLocation {
    pub uri: String,
}

#[derive(Debug, Deserialize)]
pub struct SarifRegion {
    #[serde(default, rename = "startLine")]
    pub start_line: Option<u32>,
    #[serde(default, rename = "endLine")]
    pub end_line: Option<u32>,
}

// --------------------------------------------------------------------------
// Level filtering
// --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    None,
    Note,
    Warning,
    Error,
}

impl Level {
    const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Note => "note",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

fn parse_level(s: &str) -> Result<Level> {
    match s.to_ascii_lowercase().as_str() {
        "none" => Ok(Level::None),
        "note" | "info" | "informational" => Ok(Level::Note),
        "warning" | "warn" => Ok(Level::Warning),
        "error" | "fatal" | "critical" => Ok(Level::Error),
        other => bail!("Unknown SARIF level '{other}'. Use: none, note, warning, error"),
    }
}

// --------------------------------------------------------------------------
// Fingerprint + body helpers
// --------------------------------------------------------------------------

/// A tag embedded in each imported comment so we can dedup across re-runs.
const FINGERPRINT_TAG: &str = "sarif-fp:";

fn compute_fingerprint(result: &SarifResult, tool_name: &str) -> String {
    // Prefer explicit fingerprints > partialFingerprints > synthesized hash.
    let source = if result.fingerprints.is_empty() {
        &result.partial_fingerprints
    } else {
        &result.fingerprints
    };

    if !source.is_empty() {
        let mut keys: Vec<&String> = source.keys().collect();
        keys.sort();
        let k = keys[0];
        let v = &source[k];
        return format!("{tool_name}:{k}:{v}");
    }

    // Fallback: FNV-1a hash of tool + rule + first-location + message.
    let loc_key = result
        .locations
        .first()
        .and_then(|l| l.physical_location.as_ref())
        .map(|p| {
            let uri = p.artifact_location.as_ref().map_or("", |a| a.uri.as_str());
            let line = p.region.as_ref().and_then(|r| r.start_line).unwrap_or(0);
            format!("{uri}:{line}")
        })
        .unwrap_or_default();
    let rule = result.rule_id.as_deref().unwrap_or("");
    let msg = result.message.text.as_deref().unwrap_or("");
    let key = format!("{tool_name}|{rule}|{loc_key}|{msg}");
    format!("{tool_name}:h:{:x}", fnv1a(&key))
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

fn format_body(result: &SarifResult, tool_name: &str, level: Level, fingerprint: &str) -> String {
    let rule = result.rule_id.as_deref().unwrap_or("(no rule)");
    let msg = result
        .message
        .text
        .as_deref()
        .unwrap_or("(no message)")
        .trim();
    format!(
        "**[{tool_name}] {rule}** ({level})\n\n{msg}\n\n<!-- {tag} {fingerprint} -->",
        level = level.as_str(),
        tag = FINGERPRINT_TAG,
    )
}

fn body_contains_fingerprint(body: &str, fingerprint: &str) -> bool {
    body.lines().any(|line| {
        let Some((_, marker_body)) = line.split_once(FINGERPRINT_TAG) else {
            return false;
        };

        marker_body
            .trim()
            .strip_suffix("-->")
            .unwrap_or_else(|| marker_body.trim())
            .trim()
            == fingerprint
    })
}

// --------------------------------------------------------------------------
// Command handler
// --------------------------------------------------------------------------

/// Import a SARIF file into an existing review, creating a thread + comment
/// per finding and skipping findings that were already imported (via SARIF
/// fingerprint).
#[tracing::instrument(skip(seal_root, scm, format))]
pub fn run_sarif_import(
    seal_root: &Path,
    scm: &dyn ScmRepo,
    sarif_path: &Path,
    review_id: &str,
    min_level: &str,
    author: Option<&str>,
    format: OutputFormat,
) -> Result<()> {
    ensure_initialized(seal_root)?;

    let min = parse_level(min_level)?;

    let content = fs::read_to_string(sarif_path)
        .with_context(|| format!("Failed to read SARIF file: {}", sarif_path.display()))?;
    let report: SarifReport = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse SARIF JSON: {}", sarif_path.display()))?;

    let services = open_services(seal_root)?;

    // Verify review exists and is open.
    let Some(review) = services.reviews().get_optional(review_id)? else {
        return Err(review_not_found_error(seal_root, review_id));
    };
    if review.status != "open" {
        bail!(
            "Cannot import into review with status '{}': {}",
            review.status,
            review_id
        );
    }

    let commit_hash = resolve_review_thread_commit(scm, &review);

    // Collect existing comment bodies on this review (for dedup).
    let mut existing_bodies = collect_review_comment_bodies(&services, review_id)?;

    let mut imported: Vec<serde_json::Value> = Vec::new();
    let mut skipped_level = 0_usize;
    let mut skipped_no_location = 0_usize;
    let mut skipped_missing_file = 0_usize;
    let mut skipped_duplicate = 0_usize;

    for run in &report.runs {
        let tool_name = run
            .tool
            .as_ref()
            .and_then(|t| t.driver.as_ref())
            .and_then(|d| d.name.as_deref())
            .unwrap_or("sarif");

        for result in &run.results {
            let level = match result.level.as_deref() {
                Some(s) => parse_level(s)?,
                None => Level::Warning, // SARIF default per spec
            };

            if level < min {
                skipped_level += 1;
                continue;
            }

            let Some((file_uri, start_line, end_line)) = extract_location(result) else {
                skipped_no_location += 1;
                continue;
            };

            if !scm.file_exists(&commit_hash, file_uri)? {
                skipped_missing_file += 1;
                continue;
            }

            let fingerprint = compute_fingerprint(result, tool_name);
            if existing_bodies
                .iter()
                .any(|b| body_contains_fingerprint(b, &fingerprint))
            {
                skipped_duplicate += 1;
                continue;
            }

            let selection = match end_line {
                Some(end) if end > start_line => CodeSelection::range(start_line, end),
                _ => CodeSelection::line(start_line),
            };
            let body = format_body(result, tool_name, level, &fingerprint);

            let services = open_services(seal_root)?;
            let added = services.comments().add_to_review(
                review_id,
                file_uri,
                selection,
                &body,
                commit_hash.clone(),
                author,
            )?;

            existing_bodies.push(body);

            imported.push(serde_json::json!({
                "comment_id": added.comment_id,
                "thread_id": added.thread_id,
                "file": file_uri,
                "line": start_line,
                "rule_id": result.rule_id,
                "level": level.as_str(),
                "thread_created": added.thread_created,
            }));
        }
    }

    let summary = serde_json::json!({
        "review_id": review_id,
        "imported": imported.len(),
        "skipped_level": skipped_level,
        "skipped_no_location": skipped_no_location,
        "skipped_missing_file": skipped_missing_file,
        "skipped_duplicate": skipped_duplicate,
        "results": imported,
    });

    Formatter::new(format).print(&summary)?;
    Ok(())
}

/// Pull the first usable file URI + line range out of a SARIF result.
fn extract_location(result: &SarifResult) -> Option<(&str, u32, Option<u32>)> {
    let phys = result.locations.first()?.physical_location.as_ref()?;
    let artifact = phys.artifact_location.as_ref()?;
    let region = phys.region.as_ref()?;
    let start = region.start_line?;
    let uri = artifact
        .uri
        .strip_prefix("file://")
        .unwrap_or(&artifact.uri);
    Some((uri, start, region.end_line))
}

/// Load every comment body on a review so we can search for fingerprint markers.
fn collect_review_comment_bodies(
    services: &seal_core::core::SealServices,
    review_id: &str,
) -> Result<Vec<String>> {
    let threads = services.threads().list(review_id, None, None)?;
    let mut bodies = Vec::new();
    for t in threads {
        let comments = services.comments().list(&t.thread_id)?;
        for c in comments {
            bodies.push(c.body);
        }
    }
    Ok(bodies)
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use seal_core::events::{Event, EventEnvelope, ReviewCreated};
    use seal_core::log::{open_or_create_review, AppendLog};
    use seal_core::scm::{ScmKind, ScmRepo};
    use std::path::{Path, PathBuf};
    use tempfile::tempdir;

    struct TestScmRepo {
        root: PathBuf,
    }

    impl ScmRepo for TestScmRepo {
        fn kind(&self) -> ScmKind {
            ScmKind::Git
        }

        fn root(&self) -> &Path {
            &self.root
        }

        fn current_anchor(&self) -> Result<String> {
            Ok("anchor".to_string())
        }

        fn current_commit(&self) -> Result<String> {
            Ok("commit".to_string())
        }

        fn commit_for_anchor(&self, _anchor: &str) -> Result<String> {
            Ok("commit".to_string())
        }

        fn parent_commit(&self, _commit: &str) -> Result<String> {
            Ok("parent".to_string())
        }

        fn diff_git(&self, _from: &str, _to: &str) -> Result<String> {
            Ok(String::new())
        }

        fn diff_git_file(&self, _from: &str, _to: &str, _file: &str) -> Result<String> {
            Ok(String::new())
        }

        fn changed_files_between(&self, _from: &str, _to: &str) -> Result<Vec<String>> {
            Ok(vec!["src/foo.rs".to_string()])
        }

        fn file_exists(&self, _rev: &str, path: &str) -> Result<bool> {
            Ok(path == "src/foo.rs")
        }

        fn show_file(&self, _rev: &str, _path: &str) -> Result<String> {
            Ok(String::new())
        }
    }

    fn setup_review(repo_root: &Path, review_id: &str) {
        let seal_dir = repo_root.join(".seal");
        std::fs::create_dir(&seal_dir).unwrap();
        std::fs::write(seal_dir.join("version"), "2\n").unwrap();
        std::fs::create_dir(seal_dir.join("reviews")).unwrap();

        let log = open_or_create_review(repo_root, review_id).unwrap();
        log.append(&EventEnvelope::new(
            "test-author",
            Event::ReviewCreated(ReviewCreated {
                review_id: review_id.to_string(),
                jj_change_id: "anchor".to_string(),
                scm_kind: Some("git".to_string()),
                scm_anchor: Some("anchor".to_string()),
                initial_commit: "commit".to_string(),
                title: "Review".to_string(),
                description: None,
            }),
        ))
        .unwrap();
    }

    #[test]
    fn parse_level_accepts_common_spellings() {
        assert_eq!(parse_level("error").unwrap(), Level::Error);
        assert_eq!(parse_level("WARNING").unwrap(), Level::Warning);
        assert_eq!(parse_level("warn").unwrap(), Level::Warning);
        assert_eq!(parse_level("note").unwrap(), Level::Note);
        assert_eq!(parse_level("info").unwrap(), Level::Note);
        assert_eq!(parse_level("none").unwrap(), Level::None);
        assert!(parse_level("bogus").is_err());
    }

    #[test]
    fn level_ordering_lets_min_filter_work() {
        assert!(Level::Error > Level::Warning);
        assert!(Level::Warning > Level::Note);
        assert!(Level::Note > Level::None);
    }

    #[test]
    fn parses_minimal_sarif_with_one_result() {
        let raw = r#"{
          "runs": [{
            "tool": {"driver": {"name": "codeql"}},
            "results": [{
              "ruleId": "rust/unused",
              "level": "warning",
              "message": {"text": "Unused variable `x`"},
              "locations": [{
                "physicalLocation": {
                  "artifactLocation": {"uri": "src/foo.rs"},
                  "region": {"startLine": 42}
                }
              }],
              "partialFingerprints": {"primaryLocationLineHash": "abc123"}
            }]
          }]
        }"#;
        let report: SarifReport = serde_json::from_str(raw).unwrap();
        assert_eq!(report.runs.len(), 1);
        let run = &report.runs[0];
        assert_eq!(run.results.len(), 1);
        let r = &run.results[0];
        assert_eq!(r.rule_id.as_deref(), Some("rust/unused"));
        assert_eq!(r.level.as_deref(), Some("warning"));
        let (uri, start, end) = extract_location(r).unwrap();
        assert_eq!(uri, "src/foo.rs");
        assert_eq!(start, 42);
        assert!(end.is_none());
    }

    #[test]
    fn strips_file_scheme_from_uri() {
        let raw = r#"{
          "runs": [{
            "results": [{
              "message": {"text": "x"},
              "locations": [{
                "physicalLocation": {
                  "artifactLocation": {"uri": "file://src/bar.rs"},
                  "region": {"startLine": 3}
                }
              }]
            }]
          }]
        }"#;
        let report: SarifReport = serde_json::from_str(raw).unwrap();
        let (uri, _, _) = extract_location(&report.runs[0].results[0]).unwrap();
        assert_eq!(uri, "src/bar.rs");
    }

    #[test]
    fn extract_location_returns_none_when_missing() {
        let r = SarifResult {
            rule_id: None,
            level: None,
            message: SarifMessage::default(),
            locations: vec![],
            fingerprints: HashMap::new(),
            partial_fingerprints: HashMap::new(),
        };
        assert!(extract_location(&r).is_none());
    }

    #[test]
    fn fingerprint_prefers_explicit_over_partial() {
        let mut fingerprints = HashMap::new();
        fingerprints.insert("primary".to_string(), "full-fp".to_string());
        let mut partial = HashMap::new();
        partial.insert("primary".to_string(), "partial-fp".to_string());
        let r = SarifResult {
            rule_id: Some("R1".into()),
            level: None,
            message: SarifMessage::default(),
            locations: vec![],
            fingerprints,
            partial_fingerprints: partial,
        };
        let fp = compute_fingerprint(&r, "tool");
        assert!(fp.contains("full-fp"));
        assert!(!fp.contains("partial-fp"));
    }

    #[test]
    fn fingerprint_stable_without_explicit_fingerprints() {
        let r = SarifResult {
            rule_id: Some("R1".into()),
            level: None,
            message: SarifMessage {
                text: Some("msg".into()),
            },
            locations: vec![SarifLocation {
                physical_location: Some(SarifPhysicalLocation {
                    artifact_location: Some(SarifArtifactLocation { uri: "a.rs".into() }),
                    region: Some(SarifRegion {
                        start_line: Some(10),
                        end_line: None,
                    }),
                }),
            }],
            fingerprints: HashMap::new(),
            partial_fingerprints: HashMap::new(),
        };
        let a = compute_fingerprint(&r, "tool");
        let b = compute_fingerprint(&r, "tool");
        assert_eq!(a, b);
        assert!(a.starts_with("tool:h:"));
    }

    #[test]
    fn body_contains_fingerprint_roundtrip() {
        let r = SarifResult {
            rule_id: Some("R1".into()),
            level: Some("warning".into()),
            message: SarifMessage {
                text: Some("do not".into()),
            },
            locations: vec![],
            fingerprints: HashMap::new(),
            partial_fingerprints: HashMap::new(),
        };
        let fp = compute_fingerprint(&r, "codeql");
        let body = format_body(&r, "codeql", Level::Warning, &fp);
        assert!(body_contains_fingerprint(&body, &fp));
        assert!(!body_contains_fingerprint(&body, "different-fp"));
        // Body should be human-readable
        assert!(body.contains("[codeql]"));
        assert!(body.contains("do not"));
    }

    #[test]
    fn body_contains_fingerprint_requires_exact_marker_value() {
        let body = "<!-- sarif-fp: codeql:primary:abc123 -->";

        assert!(body_contains_fingerprint(body, "codeql:primary:abc123"));
        assert!(!body_contains_fingerprint(body, "codeql:primary:abc"));
        assert!(!body_contains_fingerprint(
            "<!-- sarif-fp: codeql:primary:abc123-extra -->",
            "codeql:primary:abc123"
        ));
    }

    #[test]
    fn import_refreshes_projection_and_skips_in_batch_duplicates() {
        let dir = tempdir().unwrap();
        let repo_root = dir.path();
        setup_review(repo_root, "cr-sarif");

        let sarif = r#"{
          "runs": [{
            "tool": {"driver": {"name": "codeql"}},
            "results": [
              {
                "ruleId": "R1",
                "level": "warning",
                "message": {"text": "first"},
                "locations": [{
                  "physicalLocation": {
                    "artifactLocation": {"uri": "src/foo.rs"},
                    "region": {"startLine": 7}
                  }
                }],
                "partialFingerprints": {"primary": "fp-1"}
              },
              {
                "ruleId": "R2",
                "level": "warning",
                "message": {"text": "second"},
                "locations": [{
                  "physicalLocation": {
                    "artifactLocation": {"uri": "src/foo.rs"},
                    "region": {"startLine": 7}
                  }
                }],
                "partialFingerprints": {"primary": "fp-2"}
              },
              {
                "ruleId": "R1",
                "level": "warning",
                "message": {"text": "duplicate"},
                "locations": [{
                  "physicalLocation": {
                    "artifactLocation": {"uri": "src/foo.rs"},
                    "region": {"startLine": 7}
                  }
                }],
                "partialFingerprints": {"primary": "fp-1"}
              }
            ]
          }]
        }"#;
        let sarif_path = repo_root.join("findings.sarif");
        std::fs::write(&sarif_path, sarif).unwrap();

        let scm = TestScmRepo {
            root: repo_root.to_path_buf(),
        };
        run_sarif_import(
            repo_root,
            &scm,
            &sarif_path,
            "cr-sarif",
            "warning",
            Some("tester"),
            OutputFormat::Json,
        )
        .unwrap();

        let services = crate::cli::commands::helpers::open_services(repo_root).unwrap();
        let threads = services.threads().list("cr-sarif", None, None).unwrap();
        assert_eq!(
            threads.len(),
            1,
            "same-location findings should share the thread created earlier in the import"
        );

        let comments = services.comments().list(&threads[0].thread_id).unwrap();
        assert_eq!(
            comments.len(),
            2,
            "duplicate fingerprints in the same import should be skipped"
        );
    }
}

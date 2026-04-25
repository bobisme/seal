//! Append-only event log for botseal.
//!
//! Implements the write path of the event sourcing architecture.
//!
//! ## Data Format Versions
//!
//! - **v1**: Single `.seal/events.jsonl` for all reviews (legacy)
//! - **v2**: Per-review event logs at `.seal/reviews/{review_id}/events.jsonl`
//!
//! v2 eliminates merge conflicts between concurrent reviews in different
//! workspaces, as each review has its own isolated event log.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use fs2::FileExt;

use crate::events::EventEnvelope;

/// FNV-1a hash over byte slices. Output is stable across Rust versions
/// (unlike `DefaultHasher` which uses randomized `SipHash` keys).
fn fnv1a_hash(data: &[u8]) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01B3;
    let mut hash = FNV_OFFSET;
    for &byte in data {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// Trait for append-only event log operations.
pub trait AppendLog {
    /// Append an event to the log.
    fn append(&self, event: &EventEnvelope) -> Result<()>;

    /// Read all events from the log.
    fn read_all(&self) -> Result<Vec<EventEnvelope>>;

    /// Read events starting from a line offset (0-indexed).
    fn read_from(&self, line: usize) -> Result<Vec<EventEnvelope>>;

    /// Get the number of events in the log.
    fn len(&self) -> Result<usize>;

    /// Check if the log is empty.
    fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    /// Get the total number of lines in the log, including empty lines.
    ///
    /// Used for truncation detection: if `last_sync_line > total_lines()`,
    /// the file was truncated (e.g., by jj working copy restoration).
    fn total_lines(&self) -> Result<usize> {
        // Default: same as len(). Override for file-based implementations
        // to count all lines including empty ones.
        self.len()
    }

    /// Compute a hash of the first `n` lines for content-change detection.
    ///
    /// Returns `None` if `n == 0` or the log has no content to hash.
    /// Used alongside truncation detection to catch same-length file
    /// replacement (e.g., jj restoring a file with different content
    /// but the same number of lines).
    fn prefix_hash(&self, _n: usize) -> Result<Option<String>> {
        Ok(None)
    }
}

/// File-based implementation of the append-only event log.
///
/// Uses advisory file locking (via `fs2`) to ensure atomic appends from
/// multiple concurrent agents.
#[derive(Debug, Clone)]
pub struct FileLog {
    path: PathBuf,
}

impl FileLog {
    /// Create a new `FileLog` pointing to the given path.
    ///
    /// Does not create the file; use `open_or_create` for that.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Get the path to the log file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl AppendLog for FileLog {
    fn append(&self, event: &EventEnvelope) -> Result<()> {
        let json_line = event.to_json_line().context("Failed to serialize event")?;

        // Open file for appending
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("Failed to open log file: {}", self.path.display()))?;

        // Acquire exclusive lock for writing
        file.lock_exclusive()
            .context("Failed to acquire exclusive lock")?;

        // Seek to end (should already be there due to append mode, but be explicit)
        file.seek(SeekFrom::End(0))
            .context("Failed to seek to end of file")?;

        // Write the JSON line with newline
        writeln!(file, "{json_line}").context("Failed to write event to log")?;

        // Flush to ensure data is written
        file.flush().context("Failed to flush log file")?;

        // Lock is automatically released when file is dropped
        Ok(())
    }

    fn read_all(&self) -> Result<Vec<EventEnvelope>> {
        self.read_from(0)
    }

    fn read_from(&self, line: usize) -> Result<Vec<EventEnvelope>> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to open log file: {}", self.path.display()))
            }
        };

        // Acquire shared lock for reading
        file.lock_shared()
            .context("Failed to acquire shared lock")?;

        let reader = BufReader::new(file);
        let mut events = Vec::new();

        for (idx, line_result) in reader.lines().enumerate() {
            // Skip lines before the offset
            if idx < line {
                continue;
            }

            let line_content =
                line_result.with_context(|| format!("Failed to read line {idx} from log file"))?;

            // Skip empty lines
            if line_content.trim().is_empty() {
                continue;
            }

            let event = EventEnvelope::from_json_line(&line_content)
                .with_context(|| format!("Failed to parse event at line {idx}"))?;

            events.push(event);
        }

        // Lock is automatically released when file is dropped
        Ok(events)
    }

    fn len(&self) -> Result<usize> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(0);
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to open log file: {}", self.path.display()))
            }
        };

        // Acquire shared lock for reading
        file.lock_shared()
            .context("Failed to acquire shared lock")?;

        let reader = BufReader::new(file);
        let count = reader
            .lines()
            .filter_map(std::result::Result::ok)
            .filter(|l| !l.trim().is_empty())
            .count();

        Ok(count)
    }

    fn total_lines(&self) -> Result<usize> {
        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(0);
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to open log file: {}", self.path.display()))
            }
        };

        file.lock_shared()
            .context("Failed to acquire shared lock")?;

        let reader = BufReader::new(file);
        let count = reader.lines().filter_map(std::result::Result::ok).count();

        Ok(count)
    }

    fn prefix_hash(&self, n: usize) -> Result<Option<String>> {
        if n == 0 {
            return Ok(None);
        }

        let file = match File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("Failed to open log file: {}", self.path.display()))
            }
        };

        file.lock_shared()
            .context("Failed to acquire shared lock")?;

        let reader = BufReader::new(file);
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis

        for (idx, line_result) in reader.lines().enumerate() {
            if idx >= n {
                break;
            }
            let line =
                line_result.with_context(|| format!("Failed to read line {idx} for hashing"))?;
            hash = fnv1a_hash(line.as_bytes()).wrapping_add(hash.wrapping_mul(31));
        }

        Ok(Some(format!("{hash:016x}")))
    }
}

/// Open an existing log file or create a new one.
///
/// Creates parent directories if they don't exist.
/// Creates an empty file if it doesn't exist.
pub fn open_or_create(path: &Path) -> Result<FileLog> {
    // Create parent directories if needed
    if let Some(parent) = path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("Failed to create parent directories: {}", parent.display())
            })?;
        }
    }

    // Create empty file if it doesn't exist
    if !path.exists() {
        File::create(path)
            .with_context(|| format!("Failed to create log file: {}", path.display()))?;
    }

    Ok(FileLog::new(path))
}

// ============================================================================
// v2: Per-review event logs
// ============================================================================

/// Path to the reviews directory within .seal/
#[must_use]
pub fn reviews_dir(seal_root: &Path) -> PathBuf {
    seal_root.join(".seal").join("reviews")
}

/// Path to a specific review's event log.
#[must_use]
pub fn review_events_path(seal_root: &Path, review_id: &str) -> PathBuf {
    reviews_dir(seal_root).join(review_id).join("events.jsonl")
}

/// Validate a review ID for safe use in filesystem paths.
///
/// Rejects IDs containing path separators, traversal sequences, or
/// characters outside the expected alphanumeric-plus-dash set.
fn validate_review_id(review_id: &str) -> Result<()> {
    if review_id.is_empty() {
        bail!("review ID must not be empty");
    }
    if review_id.contains('/') || review_id.contains('\\') {
        bail!("review ID must not contain path separators: {review_id}");
    }
    if review_id.contains("..") {
        bail!("review ID must not contain '..': {review_id}");
    }
    if !review_id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-')
    {
        bail!("review ID contains invalid characters (allowed: alphanumeric, dash): {review_id}");
    }
    Ok(())
}

/// Per-review event log (v2 format).
///
/// Each review has its own event log at `.seal/reviews/{review_id}/events.jsonl`.
/// This eliminates merge conflicts between concurrent reviews.
#[derive(Debug, Clone)]
pub struct ReviewLog {
    seal_root: PathBuf,
    review_id: String,
}

impl ReviewLog {
    /// Create a new `ReviewLog` for the given review.
    ///
    /// Returns an error if `review_id` contains path separators or other
    /// unsafe characters that could escape the reviews directory.
    pub fn new(seal_root: impl Into<PathBuf>, review_id: impl Into<String>) -> Result<Self> {
        let review_id = review_id.into();
        validate_review_id(&review_id)?;
        Ok(Self {
            seal_root: seal_root.into(),
            review_id,
        })
    }

    /// Get the path to this review's event log.
    #[must_use]
    pub fn path(&self) -> PathBuf {
        review_events_path(&self.seal_root, &self.review_id)
    }

    /// Get the review ID.
    #[must_use]
    pub fn review_id(&self) -> &str {
        &self.review_id
    }

    /// Get the file size in bytes via `fs::metadata`.
    ///
    /// Returns 0 if the file does not exist. This is a cheap fast-path check
    /// to skip unchanged files without hashing.
    pub fn byte_len(&self) -> Result<u64> {
        let path = self.path();
        match fs::metadata(&path) {
            Ok(meta) => Ok(meta.len()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(0),
            Err(e) => {
                Err(e).with_context(|| format!("Failed to stat review log: {}", path.display()))
            }
        }
    }

    /// Ensure the review directory exists.
    fn ensure_dir(&self) -> Result<()> {
        let dir = reviews_dir(&self.seal_root).join(&self.review_id);
        if !dir.exists() {
            fs::create_dir_all(&dir)
                .with_context(|| format!("Failed to create review directory: {}", dir.display()))?;
        }
        Ok(())
    }
}

impl AppendLog for ReviewLog {
    fn append(&self, event: &EventEnvelope) -> Result<()> {
        self.ensure_dir()?;

        let path = self.path();
        let json_line = event.to_json_line().context("Failed to serialize event")?;

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to open review log: {}", path.display()))?;

        file.lock_exclusive()
            .context("Failed to acquire exclusive lock")?;

        file.seek(SeekFrom::End(0))
            .context("Failed to seek to end of file")?;

        writeln!(file, "{json_line}").context("Failed to write event to log")?;

        file.flush().context("Failed to flush log file")?;

        Ok(())
    }

    fn read_all(&self) -> Result<Vec<EventEnvelope>> {
        let path = self.path();
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&path)
            .with_context(|| format!("Failed to open review log: {}", path.display()))?;

        file.lock_shared()
            .context("Failed to acquire shared lock")?;

        let reader = BufReader::new(file);
        let mut events = Vec::new();

        for (idx, line_result) in reader.lines().enumerate() {
            let line_content =
                line_result.with_context(|| format!("Failed to read line {idx} from log file"))?;

            if line_content.trim().is_empty() {
                continue;
            }

            let event = EventEnvelope::from_json_line(&line_content)
                .with_context(|| format!("Failed to parse event at line {idx}"))?;

            events.push(event);
        }

        Ok(events)
    }

    fn read_from(&self, line: usize) -> Result<Vec<EventEnvelope>> {
        let path = self.path();
        if !path.exists() {
            return Ok(Vec::new());
        }

        let file = File::open(&path)
            .with_context(|| format!("Failed to open review log: {}", path.display()))?;

        file.lock_shared()
            .context("Failed to acquire shared lock")?;

        let reader = BufReader::new(file);
        let mut events = Vec::new();

        for (idx, line_result) in reader.lines().enumerate() {
            if idx < line {
                continue;
            }

            let line_content =
                line_result.with_context(|| format!("Failed to read line {idx} from log file"))?;

            if line_content.trim().is_empty() {
                continue;
            }

            let event = EventEnvelope::from_json_line(&line_content)
                .with_context(|| format!("Failed to parse event at line {idx}"))?;

            events.push(event);
        }

        Ok(events)
    }

    fn len(&self) -> Result<usize> {
        let path = self.path();
        if !path.exists() {
            return Ok(0);
        }

        let file = File::open(&path)
            .with_context(|| format!("Failed to open review log: {}", path.display()))?;

        file.lock_shared()
            .context("Failed to acquire shared lock")?;

        let reader = BufReader::new(file);
        let count = reader
            .lines()
            .filter_map(std::result::Result::ok)
            .filter(|l| !l.trim().is_empty())
            .count();

        Ok(count)
    }

    fn total_lines(&self) -> Result<usize> {
        let path = self.path();
        if !path.exists() {
            return Ok(0);
        }

        let file = File::open(&path)
            .with_context(|| format!("Failed to open review log: {}", path.display()))?;

        file.lock_shared()
            .context("Failed to acquire shared lock")?;

        let reader = BufReader::new(file);
        let count = reader.lines().filter_map(std::result::Result::ok).count();

        Ok(count)
    }

    fn prefix_hash(&self, n: usize) -> Result<Option<String>> {
        if n == 0 {
            return Ok(None);
        }

        let path = self.path();
        if !path.exists() {
            return Ok(None);
        }

        let file = File::open(&path)
            .with_context(|| format!("Failed to open review log: {}", path.display()))?;

        file.lock_shared()
            .context("Failed to acquire shared lock")?;

        let reader = BufReader::new(file);
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis

        for (idx, line_result) in reader.lines().enumerate() {
            if idx >= n {
                break;
            }
            let line =
                line_result.with_context(|| format!("Failed to read line {idx} for hashing"))?;
            hash = fnv1a_hash(line.as_bytes()).wrapping_add(hash.wrapping_mul(31));
        }

        Ok(Some(format!("{hash:016x}")))
    }
}

/// List all review IDs that have event logs.
pub fn list_review_ids(seal_root: &Path) -> Result<Vec<String>> {
    let dir = reviews_dir(seal_root);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut review_ids = Vec::new();
    for entry in fs::read_dir(&dir)
        .with_context(|| format!("Failed to read reviews directory: {}", dir.display()))?
    {
        let entry = entry.context("Failed to read directory entry")?;
        let path = entry.path();

        if path.is_dir() {
            // Check if it has an events.jsonl file
            if path.join("events.jsonl").exists() {
                if let Some(name) = path.file_name() {
                    if let Some(name_str) = name.to_str() {
                        review_ids.push(name_str.to_string());
                    }
                }
            }
        }
    }

    review_ids.sort();
    Ok(review_ids)
}

/// Read all events from all reviews, sorted by timestamp.
pub fn read_all_reviews(seal_root: &Path) -> Result<Vec<EventEnvelope>> {
    let review_ids = list_review_ids(seal_root)?;
    let mut all_events = Vec::new();

    for review_id in review_ids {
        let log = ReviewLog::new(seal_root, &review_id)?;
        let events = log.read_all()?;
        all_events.extend(events);
    }

    // Sort by timestamp
    all_events.sort_by(|a, b| a.ts.cmp(&b.ts));

    Ok(all_events)
}

/// Open or create a review log (v2 format).
pub fn open_or_create_review(seal_root: &Path, review_id: &str) -> Result<ReviewLog> {
    let log = ReviewLog::new(seal_root, review_id)?;
    log.ensure_dir()?;

    // Create empty file if it doesn't exist
    let path = log.path();
    if !path.exists() {
        File::create(&path)
            .with_context(|| format!("Failed to create review log: {}", path.display()))?;
    }

    Ok(log)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::{Event, ReviewCreated};
    use tempfile::tempdir;

    fn make_test_event(id: &str) -> EventEnvelope {
        EventEnvelope::new(
            "test_agent",
            Event::ReviewCreated(ReviewCreated {
                review_id: id.to_string(),
                jj_change_id: "change123".to_string(),
                scm_kind: Some("jj".to_string()),
                scm_anchor: Some("change123".to_string()),
                initial_commit: "commit456".to_string(),
                title: format!("Test Review {id}"),
                description: None,
            }),
        )
    }

    #[test]
    fn test_open_or_create_creates_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("events.jsonl");

        let log = open_or_create(&path).unwrap();
        assert!(path.exists());
        assert_eq!(log.path(), path);
    }

    #[test]
    fn test_open_or_create_existing_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");

        // Create file with some content
        std::fs::write(&path, "").unwrap();

        let log = open_or_create(&path).unwrap();
        assert_eq!(log.path(), path);
    }

    #[test]
    fn test_append_and_read_all() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = open_or_create(&path).unwrap();

        // Append events
        let event1 = make_test_event("cr-001");
        let event2 = make_test_event("cr-002");

        log.append(&event1).unwrap();
        log.append(&event2).unwrap();

        // Read all events
        let events = log.read_all().unwrap();
        assert_eq!(events.len(), 2);

        match &events[0].event {
            Event::ReviewCreated(r) => assert_eq!(r.review_id, "cr-001"),
            _ => panic!("Expected ReviewCreated"),
        }

        match &events[1].event {
            Event::ReviewCreated(r) => assert_eq!(r.review_id, "cr-002"),
            _ => panic!("Expected ReviewCreated"),
        }
    }

    #[test]
    fn test_read_from_offset() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = open_or_create(&path).unwrap();

        // Append 5 events
        for i in 1..=5 {
            log.append(&make_test_event(&format!("cr-{i:03}"))).unwrap();
        }

        // Read from offset 2 (should get events 3, 4, 5)
        let events = log.read_from(2).unwrap();
        assert_eq!(events.len(), 3);

        match &events[0].event {
            Event::ReviewCreated(r) => assert_eq!(r.review_id, "cr-003"),
            _ => panic!("Expected ReviewCreated"),
        }

        match &events[2].event {
            Event::ReviewCreated(r) => assert_eq!(r.review_id, "cr-005"),
            _ => panic!("Expected ReviewCreated"),
        }
    }

    #[test]
    fn test_read_from_beyond_end() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = open_or_create(&path).unwrap();

        log.append(&make_test_event("cr-001")).unwrap();

        // Read from offset beyond the file
        let events = log.read_from(100).unwrap();
        assert!(events.is_empty());
    }

    #[test]
    fn test_len() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = open_or_create(&path).unwrap();

        assert_eq!(log.len().unwrap(), 0);

        log.append(&make_test_event("cr-001")).unwrap();
        assert_eq!(log.len().unwrap(), 1);

        log.append(&make_test_event("cr-002")).unwrap();
        log.append(&make_test_event("cr-003")).unwrap();
        assert_eq!(log.len().unwrap(), 3);
    }

    #[test]
    fn test_is_empty() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = open_or_create(&path).unwrap();

        assert!(log.is_empty().unwrap());

        log.append(&make_test_event("cr-001")).unwrap();
        assert!(!log.is_empty().unwrap());
    }

    #[test]
    fn test_read_nonexistent_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("does_not_exist.jsonl");
        let log = FileLog::new(&path);

        // Should return empty vec, not error
        let events = log.read_all().unwrap();
        assert!(events.is_empty());

        assert_eq!(log.len().unwrap(), 0);
    }

    #[test]
    fn test_append_creates_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("new_file.jsonl");
        let log = FileLog::new(&path);

        assert!(!path.exists());

        log.append(&make_test_event("cr-001")).unwrap();

        assert!(path.exists());
        assert_eq!(log.len().unwrap(), 1);
    }

    #[test]
    fn test_file_format_is_jsonl() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        let log = open_or_create(&path).unwrap();

        log.append(&make_test_event("cr-001")).unwrap();
        log.append(&make_test_event("cr-002")).unwrap();

        // Read raw file content
        let content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();

        assert_eq!(lines.len(), 2);

        // Each line should be valid JSON
        for line in &lines {
            let _: serde_json::Value = serde_json::from_str(line).unwrap();
        }

        // First line should contain cr-001
        assert!(lines[0].contains("cr-001"));
        // Second line should contain cr-002
        assert!(lines[1].contains("cr-002"));
    }

    // ========================================================================
    // v2: Per-review event log tests
    // ========================================================================

    #[test]
    fn test_review_log_creates_directory() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let log = ReviewLog::new(seal_root, "cr-001").unwrap();
        log.append(&make_test_event("cr-001")).unwrap();

        // Check directory structure
        assert!(seal_root
            .join(".seal")
            .join("reviews")
            .join("cr-001")
            .exists());
        assert!(seal_root
            .join(".seal")
            .join("reviews")
            .join("cr-001")
            .join("events.jsonl")
            .exists());
    }

    #[test]
    fn test_review_log_append_and_read() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let log = ReviewLog::new(seal_root, "cr-001").unwrap();

        let event1 = make_test_event("cr-001");
        let event2 = make_test_event("cr-001"); // Same review

        log.append(&event1).unwrap();
        log.append(&event2).unwrap();

        let events = log.read_all().unwrap();
        assert_eq!(events.len(), 2);
    }

    #[test]
    fn test_review_log_isolation() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        // Create two separate review logs
        let log1 = ReviewLog::new(seal_root, "cr-001").unwrap();
        let log2 = ReviewLog::new(seal_root, "cr-002").unwrap();

        log1.append(&make_test_event("cr-001")).unwrap();
        log1.append(&make_test_event("cr-001")).unwrap();
        log2.append(&make_test_event("cr-002")).unwrap();

        // Each log should only have its own events
        assert_eq!(log1.len().unwrap(), 2);
        assert_eq!(log2.len().unwrap(), 1);
    }

    #[test]
    fn test_list_review_ids() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        // Create review logs
        let log1 = ReviewLog::new(seal_root, "cr-001").unwrap();
        let log2 = ReviewLog::new(seal_root, "cr-002").unwrap();
        let log3 = ReviewLog::new(seal_root, "cr-003").unwrap();

        log1.append(&make_test_event("cr-001")).unwrap();
        log2.append(&make_test_event("cr-002")).unwrap();
        log3.append(&make_test_event("cr-003")).unwrap();

        let ids = list_review_ids(seal_root).unwrap();
        assert_eq!(ids, vec!["cr-001", "cr-002", "cr-003"]);
    }

    #[test]
    fn test_list_review_ids_empty() {
        let dir = tempdir().unwrap();
        let ids = list_review_ids(dir.path()).unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn test_read_all_reviews() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        // Create events with different timestamps
        let log1 = ReviewLog::new(seal_root, "cr-001").unwrap();
        let log2 = ReviewLog::new(seal_root, "cr-002").unwrap();

        // Log1 gets 2 events, log2 gets 1
        log1.append(&make_test_event("cr-001")).unwrap();
        log2.append(&make_test_event("cr-002")).unwrap();
        log1.append(&make_test_event("cr-001")).unwrap();

        let all_events = read_all_reviews(seal_root).unwrap();
        assert_eq!(all_events.len(), 3);

        // Should be sorted by timestamp
        for i in 0..all_events.len() - 1 {
            assert!(all_events[i].ts <= all_events[i + 1].ts);
        }
    }

    #[test]
    fn test_open_or_create_review() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let log = open_or_create_review(seal_root, "cr-new").unwrap();
        assert!(log.path().exists());
        assert_eq!(log.review_id(), "cr-new");
    }

    #[test]
    fn test_review_log_path() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let expected = seal_root
            .join(".seal")
            .join("reviews")
            .join("cr-abc")
            .join("events.jsonl");
        assert_eq!(review_events_path(seal_root, "cr-abc"), expected);

        let log = ReviewLog::new(seal_root, "cr-abc").unwrap();
        assert_eq!(log.path(), expected);
    }

    #[test]
    fn test_review_log_nonexistent() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        let log = ReviewLog::new(seal_root, "cr-nonexistent").unwrap();

        // Should return empty, not error
        let events = log.read_all().unwrap();
        assert!(events.is_empty());
        assert_eq!(log.len().unwrap(), 0);
    }

    // ========================================================================
    // Review ID validation tests
    // ========================================================================

    #[test]
    fn test_validate_review_id_accepts_valid_ids() {
        assert!(validate_review_id("cr-001").is_ok());
        assert!(validate_review_id("cr-abc").is_ok());
        assert!(validate_review_id("cr-a1cdefgh").is_ok());
        assert!(validate_review_id("123").is_ok());
        assert!(validate_review_id("simple-id").is_ok());
    }

    #[test]
    fn test_validate_review_id_rejects_path_traversal() {
        assert!(validate_review_id("../escape").is_err());
        assert!(validate_review_id("../../etc").is_err());
        assert!(validate_review_id("foo/bar").is_err());
        assert!(validate_review_id("foo\\bar").is_err());
        assert!(validate_review_id("..").is_err());
    }

    #[test]
    fn test_validate_review_id_rejects_special_chars() {
        assert!(validate_review_id("").is_err());
        assert!(validate_review_id("id with spaces").is_err());
        assert!(validate_review_id("id;rm -rf").is_err());
        assert!(validate_review_id("id\0null").is_err());
    }

    #[test]
    fn test_review_log_new_rejects_traversal() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();

        assert!(ReviewLog::new(seal_root, "../escape").is_err());
        assert!(ReviewLog::new(seal_root, "foo/bar").is_err());
        assert!(ReviewLog::new(seal_root, "").is_err());
    }

    /// Verify `prefix_hash` produces identical output for identical input (bd-2ji).
    #[test]
    fn test_prefix_hash_is_deterministic() {
        let dir = tempdir().unwrap();
        let seal_root = dir.path();
        let log = open_or_create_review(seal_root, "cr-hash-test").unwrap();

        let e1 = make_test_event("cr-hash-test");
        let e2 = EventEnvelope::new(
            "test_agent",
            Event::ReviewCreated(ReviewCreated {
                review_id: "cr-hash-test".to_string(),
                jj_change_id: "other_change".to_string(),
                scm_kind: Some("jj".to_string()),
                scm_anchor: Some("other_change".to_string()),
                initial_commit: "other_commit".to_string(),
                title: "Another review".to_string(),
                description: Some("with description".to_string()),
            }),
        );
        log.append(&e1).unwrap();
        log.append(&e2).unwrap();

        let h1 = log.prefix_hash(2).unwrap();
        let h2 = log.prefix_hash(2).unwrap();
        assert_eq!(h1, h2, "prefix_hash must be deterministic across calls");
        assert!(
            h1.is_some(),
            "prefix_hash should return Some for non-empty file"
        );

        // Partial prefix should differ from full
        let h_partial = log.prefix_hash(1).unwrap();
        assert_ne!(
            h1, h_partial,
            "different prefix lengths should produce different hashes"
        );
    }

    /// Verify `fnv1a_hash` produces known stable values (bd-2ji).
    #[test]
    fn test_fnv1a_known_values() {
        // FNV-1a of empty input is the offset basis
        assert_eq!(fnv1a_hash(b""), 0xcbf2_9ce4_8422_2325);
        // FNV-1a of "a" — well-known test vector
        assert_eq!(fnv1a_hash(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}

//! Pre-write snapshots of memory documents: rollback, layered rotation, entry ages.
//!
//! Every write to a memory document copies the **current** content into
//! `{workspace}/memory/.snapshots/{run}/{document}` *before* the write lands, so
//! a bad pass (Dream promoting the wrong thing, the model rewriting `MEMORY.md`)
//! has a restore target that is one directory copy away. Deliberately not git:
//! `memory/` is an owner-edited domain, and putting an index/lock we own next to
//! hand edits is the exact class of accident this is meant to prevent. See
//! `docs/adr/0014-memory-entry-age-derived-from-snapshots.md` and
//! `docs/memory.md`.
//!
//! Two more things live here because they are properties of the snapshot
//! sequence, not of the document:
//!
//! - **Layered rotation** ([`SnapshotPolicy`]): the recent layer keeps every
//!   version, then one per day, then one per month, plus a hard per-document
//!   cap. Rotation runs on the same hook as Dream's success branch, never on a
//!   new timer.
//! - **Age by diff** ([`SnapshotStore::entry_ages`]): entries carry no date
//!   text; an entry's age is reconstructed from the first snapshot its body line
//!   appears in. The result is a side-channel value — it is never written back
//!   into the document, and a gap in the sequence reports "unknown" instead of
//!   extrapolating.

use std::collections::{BTreeMap, HashSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Local, NaiveDateTime, TimeZone};

use crate::bootstrap;

/// Snapshot root, relative to the workspace.
pub const SNAPSHOT_DIR: &str = "memory/.snapshots";

/// Stamp format of one snapshot run (a directory name).
const RUN_STAMP_FORMAT: &str = "%Y%m%d-%H%M%S";

/// Length of a formatted run stamp (`YYYYMMDD-HHMMSS`), used to slice it out of
/// `{stamp}-{sequence}`. `RUN_STAMP_FORMAT.len()` is wrong here: the format
/// string is shorter than the text it produces.
const RUN_STAMP_LEN: usize = 15;

/// How many pre-write snapshots are retained per document.
///
/// Three layers plus one hard bound, so the count is bounded no matter how
/// often the model writes:
///
/// 1. `recent_days`: keep every snapshot (the "undo, right now" layer).
/// 2. up to `daily_days`: keep the newest snapshot of each calendar day.
/// 3. older than that: keep the newest snapshot of each calendar month.
/// 4. finally, keep at most `max_per_document` newest snapshots.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SnapshotPolicy {
    /// Every snapshot younger than this is kept.
    pub recent_days: u64,
    /// Beyond `recent_days`, snapshots are thinned to one per day until this age.
    pub daily_days: u64,
    /// Hard bound on retained snapshots per document.
    pub max_per_document: usize,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        Self {
            recent_days: 30,
            daily_days: 365,
            max_per_document: 120,
        }
    }
}

impl SnapshotPolicy {
    /// Read the policy from the `pressure` config section.
    pub fn from_config(config: &crate::config::PressureConfig) -> Self {
        Self {
            recent_days: config.memory_snapshot_recent_days,
            daily_days: config.memory_snapshot_daily_days,
            max_per_document: config.memory_snapshot_max,
        }
    }
}

/// One stored snapshot of one document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SnapshotRef {
    /// Run directory name (the sortable timestamp stamp).
    pub run: String,
    /// Workspace-relative document path (`memory/MEMORY.md`).
    pub document: String,
    /// When the snapshot was taken.
    pub at: DateTime<Local>,
}

/// Age of one entry, reconstructed from the snapshot sequence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntryAge {
    /// The body line this age belongs to (trimmed, as it appears in the document).
    pub text: String,
    /// Run of the first snapshot the line appears in. `None` = no snapshot holds it.
    pub first_seen_run: Option<String>,
    /// Age in days. `None` = unknown: no earlier snapshot proves when it started.
    pub age_days: Option<u64>,
}

/// A directory of pre-write snapshots plus the policy that bounds it.
pub struct SnapshotStore {
    workspace_dir: PathBuf,
    root: PathBuf,
    policy: SnapshotPolicy,
}

impl SnapshotStore {
    /// A store for `workspace_dir`, with the policy from the global config.
    ///
    /// The global lookup mirrors `web_fetch`'s precedent: the file tools are
    /// constructed from a workspace path only, and threading a config handle
    /// through every constructor would touch far more call sites than it buys.
    pub fn new(workspace_dir: &Path) -> Self {
        let policy = match crate::config::Config::try_get() {
            Some(config) => SnapshotPolicy::from_config(&config.pressure),
            None => SnapshotPolicy::default(),
        };
        Self::with_policy(workspace_dir, policy)
    }

    /// A store for `workspace_dir` with an explicit policy (tests, callers that
    /// already hold a config).
    pub fn with_policy(workspace_dir: &Path, policy: SnapshotPolicy) -> Self {
        Self {
            workspace_dir: workspace_dir.to_path_buf(),
            root: workspace_dir.join(SNAPSHOT_DIR),
            policy,
        }
    }

    /// Retention policy this store applies.
    #[allow(dead_code)] // observed by docs/tests; no production reader needs it
    pub fn policy(&self) -> SnapshotPolicy {
        self.policy
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Snapshot `content` for `document` at `at` (deterministic entry point).
    pub fn capture_at(
        &self,
        document: &str,
        content: &str,
        at: DateTime<Local>,
    ) -> Result<SnapshotRef> {
        let run = self.allocate_run_dir(at)?;
        let path = self.root.join(&run).join(document);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write snapshot {}", path.display()))?;
        Ok(SnapshotRef {
            run,
            document: document.to_string(),
            at,
        })
    }

    /// Snapshot `content` for `document` now.
    pub fn capture(&self, document: &str, content: &str) -> Result<SnapshotRef> {
        self.capture_at(document, content, Local::now())
    }

    /// Snapshot the on-disk content of `document` before a write.
    ///
    /// Retention is **not** pruned here: rotation runs only on the Dream success
    /// branch (ticket 06 #6 — one cleanup point, no separate timer), so a write
    /// path stays a cheap copy and never scans the whole snapshot store.
    ///
    /// Returns `None` when the document does not exist yet (nothing to
    /// preserve) or is not a memory document.
    pub fn capture_before_write(&self, document: &str) -> Result<Option<SnapshotRef>> {
        if !is_managed_document(document) {
            return Ok(None);
        }
        let path = self.workspace_dir.join(document);
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("Failed to read {}", path.display()));
            }
        };
        Ok(Some(self.capture(document, &content)?))
    }

    /// Every stored snapshot of `document`, oldest first.
    pub fn snapshots(&self, document: &str) -> Result<Vec<SnapshotRef>> {
        let mut snapshots = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Ok(snapshots);
        };
        for entry in entries.flatten() {
            let run_dir = entry.path();
            if !run_dir.is_dir() {
                continue;
            }
            let Some(run) = run_dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if !run_dir.join(document).is_file() {
                continue;
            }
            let Some(at) = parse_run_stamp(run) else {
                continue;
            };
            snapshots.push(SnapshotRef {
                run: run.to_string(),
                document: document.to_string(),
                at,
            });
        }
        snapshots.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.run.cmp(&b.run)));
        Ok(snapshots)
    }

    /// Content of one stored snapshot.
    pub fn read(&self, reference: &SnapshotRef) -> Result<Option<String>> {
        let path = self.root.join(&reference.run).join(&reference.document);
        match std::fs::read_to_string(&path) {
            Ok(content) => Ok(Some(content)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("Failed to read {}", path.display())),
        }
    }

    /// Restore `document` to the state stored by run `run`.
    ///
    /// The restore is itself a write, so the current content is snapshotted
    /// first: rolling back and changing your mind is not a dead end.
    /// Returns `None` when no such snapshot exists.
    pub fn restore(&self, document: &str, run: &str) -> Result<Option<SnapshotRef>> {
        let Some(reference) = self.snapshots(document)?.into_iter().find(|s| s.run == run) else {
            return Ok(None);
        };
        let content = self
            .read(&reference)?
            .ok_or_else(|| anyhow::anyhow!("snapshot {run}/{document} disappeared"))?;

        self.capture_before_write(document)?;

        let path = self.workspace_dir.join(document);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }
        std::fs::write(&path, &content)
            .with_context(|| format!("Failed to write {}", path.display()))?;
        Ok(Some(reference))
    }

    /// Restore `document` to its newest stored snapshot.
    pub fn restore_latest(&self, document: &str) -> Result<Option<SnapshotRef>> {
        let Some(latest) = self.snapshots(document)?.pop() else {
            return Ok(None);
        };
        self.restore(document, &latest.run)
    }

    /// Documents that have at least one stored snapshot.
    pub fn documents(&self) -> Result<Vec<String>> {
        let mut documents = HashSet::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Ok(Vec::new());
        };
        for entry in entries.flatten() {
            let run_dir = entry.path();
            if !run_dir.is_dir() {
                continue;
            }
            collect_documents(&run_dir, &run_dir, &mut documents);
        }
        let mut documents: Vec<String> = documents.into_iter().collect();
        documents.sort();
        Ok(documents)
    }

    /// Apply the retention policy at `now`, returning how many snapshots were
    /// removed. Empty run directories are pruned.
    pub fn rotate(&self, now: DateTime<Local>) -> Result<usize> {
        let mut removed = 0;
        for document in self.documents()? {
            let snapshots = self.snapshots(&document)?;
            let keep = retention_plan(&snapshots, now, &self.policy);
            for snapshot in snapshots {
                if keep.contains(&snapshot.run) {
                    continue;
                }
                let path = self.root.join(&snapshot.run).join(&document);
                match std::fs::remove_file(&path) {
                    Ok(()) => removed += 1,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => {
                        return Err(e)
                            .with_context(|| format!("Failed to remove {}", path.display()));
                    }
                }
                if let Some(parent) = path.parent() {
                    prune_empty_dirs(parent, &self.root);
                }
            }
        }
        Ok(removed)
    }

    /// Reconstruct the age of every body line in `content`.
    ///
    /// Rules (ADR 0014):
    /// - age comes from the **first snapshot a body line appears in**;
    /// - heading lines never establish an age, so renaming or moving a section
    ///   does not reset its entries;
    /// - a line first seen in the oldest retained snapshot has an **unknown**
    ///   age — the earlier versions may have been rotated away, and we do not
    ///   extrapolate.
    pub fn entry_ages(
        &self,
        document: &str,
        content: &str,
        now: DateTime<Local>,
    ) -> Result<Vec<EntryAge>> {
        let snapshots = self.snapshots(document)?;

        // body line -> index of the first snapshot containing it
        let mut first_seen: BTreeMap<String, usize> = BTreeMap::new();
        for (index, snapshot) in snapshots.iter().enumerate() {
            let text = self.read(snapshot)?.unwrap_or_default();
            for line in text.lines().map(str::trim).filter(|l| is_body_line(l)) {
                first_seen.entry(line.to_string()).or_insert(index);
            }
        }

        let mut ages = Vec::new();
        for line in content.lines().map(str::trim).filter(|l| is_body_line(l)) {
            let age = match first_seen.get(line) {
                // Written since the last snapshot: it is new by definition.
                None => EntryAge {
                    text: line.to_string(),
                    first_seen_run: None,
                    age_days: Some(0),
                },
                // Only the pre-history snapshot holds it, or older snapshots
                // were rotated away: the sequence does not prove an age.
                Some(0) => EntryAge {
                    text: line.to_string(),
                    first_seen_run: Some(snapshots[0].run.clone()),
                    age_days: None,
                },
                Some(&index) => EntryAge {
                    text: line.to_string(),
                    first_seen_run: Some(snapshots[index].run.clone()),
                    age_days: Some(days_between(snapshots[index].at, now)),
                },
            };
            ages.push(age);
        }
        Ok(ages)
    }

    /// Create (or reuse) the run directory for `at`.
    fn allocate_run_dir(&self, at: DateTime<Local>) -> Result<String> {
        let base = at.format(RUN_STAMP_FORMAT).to_string();
        std::fs::create_dir_all(&self.root)
            .with_context(|| format!("Failed to create {}", self.root.display()))?;
        for sequence in 0..10_000u32 {
            let run = format!("{base}-{sequence:04}");
            match std::fs::create_dir(self.root.join(&run)) {
                Ok(()) => return Ok(run),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => {
                    return Err(e)
                        .with_context(|| format!("Failed to create snapshot dir for {run}"));
                }
            }
        }
        anyhow::bail!("Failed to allocate a snapshot run directory for {base}");
    }
}

/// Snapshot a workspace-relative document before a write.
///
/// `Ok(None)` means "nothing to do": the document is not a memory document, or
/// it does not exist yet. Callers keep their write path and their result text
/// unchanged; a snapshot is bookkeeping, not a gate.
pub fn capture_before_write(workspace_dir: &Path, document: &str) -> Result<Option<SnapshotRef>> {
    SnapshotStore::new(workspace_dir).capture_before_write(document)
}

/// Whether `document` (workspace-relative, `/`-separated) is a document the
/// memory system writes and therefore snapshots.
///
/// The set is derived, not duplicated: `memory/MEMORY.md` (the single write
/// funnel) plus everything [`crate::tools::protected::classify`] classifies as a
/// memory document (identity documents Dream promotes into, Dream-owned skills
/// and custom documents). Snapshots of anything else would be noise.
pub fn is_managed_document(document: &str) -> bool {
    if document.eq_ignore_ascii_case(bootstrap::MEMORY_DOCUMENT) {
        return true;
    }
    crate::tools::protected::classify(document).is_some()
}

/// Workspace-relative, `/`-separated form of `target`, if it is inside the
/// workspace. Tolerates a file that does not exist yet (its parent must).
pub fn relative_document(workspace_dir: &Path, target: &Path) -> Option<String> {
    // Try the workspace path as given first, then its canonical form (a macOS
    // temp dir is reached through a `/var` -> `/private/var` symlink, so the
    // resolved `target` and a raw `workspace_dir` differ by that prefix).
    if let Some(relative) = target.strip_prefix(workspace_dir).ok().and_then(flatten) {
        return Some(relative);
    }
    let root = workspace_dir.canonicalize().ok()?;
    if let Some(relative) = target.strip_prefix(&root).ok().and_then(flatten) {
        return Some(relative);
    }
    let parent = target.parent()?.canonicalize().ok()?;
    let relative = parent.strip_prefix(&root).ok()?;
    flatten(relative.join(target.file_name()?).as_path())
}

/// Plain `/`-joined form of a relative path; `None` if empty or not plain.
fn flatten(path: &Path) -> Option<String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_string_lossy().into_owned()),
            Component::CurDir => {}
            _ => return None,
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("/"))
    }
}

/// Snapshot runs to keep for one document, per [`SnapshotPolicy`].
fn retention_plan(
    snapshots: &[SnapshotRef],
    now: DateTime<Local>,
    policy: &SnapshotPolicy,
) -> HashSet<String> {
    let mut keep = HashSet::new();
    let mut seen_days: HashSet<String> = HashSet::new();
    let mut seen_months: HashSet<String> = HashSet::new();

    // Newest first so the layers and the hard cap both prefer recent versions.
    for snapshot in snapshots.iter().rev() {
        if keep.len() >= policy.max_per_document {
            break;
        }
        let age = days_between(snapshot.at, now);
        let day = snapshot.at.format("%Y-%m-%d").to_string();
        let month = snapshot.at.format("%Y-%m").to_string();
        let day_seen = !seen_days.insert(day);
        let month_seen = !seen_months.insert(month);

        let retained = if age <= policy.recent_days {
            true
        } else if age <= policy.daily_days {
            !day_seen
        } else {
            !month_seen
        };
        if retained {
            keep.insert(snapshot.run.clone());
        }
    }
    keep
}

/// Body lines carry age; headings are index anchors, not content (ADR 0014).
fn is_body_line(line: &str) -> bool {
    !line.is_empty() && !line.starts_with('#')
}

/// Whole days from `from` to `now` (never negative).
fn days_between(from: DateTime<Local>, now: DateTime<Local>) -> u64 {
    (now.date_naive() - from.date_naive()).num_days().max(0) as u64
}

/// Parse the timestamp of a run directory name (`{stamp}-{sequence}`).
fn parse_run_stamp(run: &str) -> Option<DateTime<Local>> {
    if run.len() < RUN_STAMP_LEN {
        return None;
    }
    let stamp = run.get(..RUN_STAMP_LEN)?;
    let naive = NaiveDateTime::parse_from_str(stamp, RUN_STAMP_FORMAT).ok()?;
    Local.from_local_datetime(&naive).earliest()
}

/// Collect document paths (`/`-joined, relative to the run dir) under `dir`.
fn collect_documents(dir: &Path, run_dir: &Path, documents: &mut HashSet<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_documents(&path, run_dir, documents);
        } else if let Ok(relative) = path.strip_prefix(run_dir)
            && let Some(relative) = flatten(relative)
        {
            documents.insert(relative);
        }
    }
}

/// Remove `dir` and its parents while they are empty, stopping at `stop`.
fn prune_empty_dirs(dir: &Path, stop: &Path) {
    let mut current = dir;
    while current != stop && current.starts_with(stop) {
        if std::fs::remove_dir(current).is_err() {
            return;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return,
        }
    }
}

/// Strip reconstructed-age markers from a document, returning the clean text and
/// the number of lines that actually carried a marker.
///
/// Age is a side channel (ADR 0014): it must never live in the editable text, or
/// the model copies it back on the next rewrite and the derivation reads it as
/// content. This runs on **both** sides of a write — the content being stored,
/// and a `memory_edit`/`file_editor` `old_string` **before it is matched** (a
/// copied marker in `old_string` would otherwise miss and fail the edit).
/// The rule is deliberately the narrowest that fixes the leak: **end of line
/// only**, **known marker forms only** (`← Nd`, `[Nd]`, `(N days old)`), never
/// mid-sentence, so `每周三 10d 会议` survives untouched. A marker the model
/// invents in a new shape is not caught — widening the rule would start eating
/// real content, which is the worse failure.
pub fn strip_age_markers(text: &str) -> (String, usize) {
    let trailing_newline = text.ends_with('\n');
    let mut stripped = 0usize;
    let lines: Vec<String> = text
        .lines()
        .map(|line| {
            let cleaned = strip_line_marker(line.trim_end());
            if cleaned.len() != line.trim_end().len() {
                stripped += 1;
            }
            cleaned.to_string()
        })
        .collect();
    let mut out = lines.join("\n");
    if trailing_newline {
        out.push('\n');
    }
    (out, stripped)
}

/// Remove a trailing age marker from one (right-trimmed) line.
fn strip_line_marker(line: &str) -> &str {
    let marker_start = match trailing_marker_start(line) {
        Some(idx) => idx,
        None => return line,
    };
    line[..marker_start].trim_end()
}

/// Byte index at which a recognised age marker begins at the end of `line`, or
/// `None` when the line does not end with one. `line` must be right-trimmed.
fn trailing_marker_start(line: &str) -> Option<usize> {
    // `(N days old)` / `(N day old)`
    if line.ends_with(')')
        && let Some(open) = line.rfind('(')
        && is_days_old(&line[open + 1..line.len() - 1])
    {
        return Some(open);
    }
    // `[Nd]`
    if line.ends_with(']')
        && let Some(open) = line.rfind('[')
        && is_nd(&line[open + 1..line.len() - 1])
    {
        return Some(open);
    }
    // `← Nd` (the arrow is three UTF-8 bytes plus a space)
    const ARROW: &str = "← ";
    if let Some(idx) = line.rfind(ARROW)
        && is_nd(&line[idx + ARROW.len()..])
    {
        return Some(idx);
    }
    None
}

/// `21d`: digits followed by a single `d`.
fn is_nd(token: &str) -> bool {
    match token.strip_suffix('d') {
        Some(digits) => !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()),
        None => false,
    }
}

/// `21 days old` / `1 day old`: a count then the word `day`/`days` then `old`.
fn is_days_old(token: &str) -> bool {
    let mut parts = token.split_whitespace();
    let (Some(count), Some(unit), Some("old"), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    !count.is_empty()
        && count.chars().all(|c| c.is_ascii_digit())
        && (unit == "day" || unit == "days")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const DOC: &str = "memory/MEMORY.md";

    fn at(year: i32, month: u32, day: u32, hour: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(year, month, day, hour, 0, 0)
            .single()
            .expect("valid local datetime")
    }

    fn store(workspace: &Path, policy: SnapshotPolicy) -> SnapshotStore {
        SnapshotStore::with_policy(workspace, policy)
    }

    fn default_store(workspace: &Path) -> SnapshotStore {
        store(workspace, SnapshotPolicy::default())
    }

    fn write_doc(workspace: &Path, content: &str) {
        let path = workspace.join(DOC);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, content).unwrap();
    }

    fn read_doc(workspace: &Path) -> String {
        fs::read_to_string(workspace.join(DOC)).unwrap()
    }

    fn contents(store: &SnapshotStore, document: &str) -> Vec<String> {
        store
            .snapshots(document)
            .unwrap()
            .into_iter()
            .map(|s| store.read(&s).unwrap().unwrap())
            .collect()
    }

    // -- capture ------------------------------------------------------------

    #[test]
    fn test_capture_at_stores_the_document_under_a_stamped_run_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let store = default_store(tmp.path());

        let snapshot = store
            .capture_at(DOC, "version one", at(2026, 11, 2, 10))
            .unwrap();

        assert_eq!(snapshot.run, "20261102-100000-0000");
        assert_eq!(
            fs::read_to_string(tmp.path().join(SNAPSHOT_DIR).join(&snapshot.run).join(DOC))
                .unwrap(),
            "version one"
        );
        assert_eq!(store.snapshots(DOC).unwrap().len(), 1);
    }

    #[test]
    fn test_capture_allocates_a_distinct_run_dir_per_write() {
        let tmp = tempfile::tempdir().unwrap();
        let store = default_store(tmp.path());
        let same_second = at(2026, 11, 2, 10);

        let first = store.capture_at(DOC, "a", same_second).unwrap();
        let second = store.capture_at(DOC, "b", same_second).unwrap();
        let third = store.capture_at(DOC, "c", same_second).unwrap();

        assert_ne!(first.run, second.run);
        assert_ne!(second.run, third.run);
        assert_eq!(store.snapshots(DOC).unwrap().len(), 3);
        assert_eq!(contents(&store, DOC), vec!["a", "b", "c"]);
    }

    #[test]
    fn test_capture_before_write_snapshots_the_pre_write_content() {
        let tmp = tempfile::tempdir().unwrap();
        let store = default_store(tmp.path());
        write_doc(tmp.path(), "before");

        let snapshot = capture_before_write(tmp.path(), DOC).unwrap().unwrap();

        assert_eq!(snapshot.document, DOC);
        assert_eq!(store.read(&snapshot).unwrap().as_deref(), Some("before"));
        assert_eq!(
            read_doc(tmp.path()),
            "before",
            "snapshotting must not touch the document"
        );
    }

    #[test]
    fn test_capture_before_write_does_nothing_for_a_missing_document() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            capture_before_write(tmp.path(), DOC).unwrap().is_none(),
            "there is no previous state to preserve"
        );
        assert!(default_store(tmp.path()).snapshots(DOC).unwrap().is_empty());
    }

    #[test]
    fn test_capture_before_write_never_rotates() {
        // Retention is a Dream-branch concern (ticket 06 #6): a write path must
        // not scan and prune the store, so snapshots past the hard cap survive
        // until the next Dream success rotates them.
        let tmp = tempfile::tempdir().unwrap();
        let store = store(
            tmp.path(),
            SnapshotPolicy {
                recent_days: 3650,
                daily_days: 3650,
                max_per_document: 2,
            },
        );
        for minute in 0..5u32 {
            write_doc(tmp.path(), &format!("v{minute}"));
            store.capture_before_write(DOC).unwrap().unwrap();
            // Freeze distinct timestamps so run directories do not collide.
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert_eq!(
            store.snapshots(DOC).unwrap().len(),
            5,
            "the write path takes no retention action"
        );
    }

    #[test]
    fn test_capture_before_write_ignores_non_memory_documents() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("notes.md"), "ordinary note").unwrap();

        assert!(
            capture_before_write(tmp.path(), "notes.md")
                .unwrap()
                .is_none()
        );
        assert!(default_store(tmp.path()).documents().unwrap().is_empty());
    }

    #[test]
    fn test_is_managed_document_covers_the_memory_documents_only() {
        for document in [
            "memory/MEMORY.md",
            "SOUL.md",
            "USER.md",
            "AGENTS.md",
            "TOOLS.md",
            "memory/glossary.md",
            "skills/release/SKILL.md",
        ] {
            assert!(is_managed_document(document), "{document} must be managed");
        }
        for document in [
            "notes/deploy.md",
            "memory/history.jsonl",
            "memory/.snapshots/20261102-100000-0000/memory/MEMORY.md",
            "cron/jobs.json",
        ] {
            assert!(!is_managed_document(document), "{document} must be ignored");
        }
    }

    #[test]
    fn test_relative_document_resolves_existing_and_pending_paths() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("memory")).unwrap();
        fs::write(tmp.path().join(DOC), "x").unwrap();

        assert_eq!(
            relative_document(tmp.path(), &tmp.path().join(DOC)).as_deref(),
            Some(DOC)
        );
        assert_eq!(
            relative_document(tmp.path(), &tmp.path().join("memory/new.md")).as_deref(),
            Some("memory/new.md"),
            "a path that does not exist yet still resolves through its parent"
        );
    }

    // -- rotation -----------------------------------------------------------

    #[test]
    fn test_rotation_keeps_the_recent_layer_whole() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(
            tmp.path(),
            SnapshotPolicy {
                recent_days: 2,
                daily_days: 30,
                max_per_document: 100,
            },
        );
        let now = at(2026, 11, 30, 12);
        for hour in [8, 9, 10] {
            store
                .capture_at(DOC, &format!("today {hour}"), at(2026, 11, 30, hour))
                .unwrap();
            store
                .capture_at(DOC, &format!("yesterday {hour}"), at(2026, 11, 29, hour))
                .unwrap();
        }

        assert_eq!(store.rotate(now).unwrap(), 0);
        assert_eq!(store.snapshots(DOC).unwrap().len(), 6);
    }

    #[test]
    fn test_rotation_thins_to_one_per_day_beyond_the_recent_window() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(
            tmp.path(),
            SnapshotPolicy {
                recent_days: 1,
                daily_days: 30,
                max_per_document: 100,
            },
        );
        let now = at(2026, 11, 30, 12);
        store
            .capture_at(DOC, "recent", at(2026, 11, 30, 10))
            .unwrap();
        store
            .capture_at(DOC, "day first", at(2026, 11, 25, 9))
            .unwrap();
        store
            .capture_at(DOC, "day second", at(2026, 11, 25, 10))
            .unwrap();
        store
            .capture_at(DOC, "day third", at(2026, 11, 25, 11))
            .unwrap();
        store
            .capture_at(DOC, "older day", at(2026, 11, 20, 10))
            .unwrap();

        assert_eq!(store.rotate(now).unwrap(), 2);
        assert_eq!(
            contents(&store, DOC),
            vec!["older day", "day third", "recent"],
            "one snapshot per day survives, and it is the newest of that day"
        );
    }

    #[test]
    fn test_rotation_thins_to_one_per_month_beyond_the_daily_window() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(
            tmp.path(),
            SnapshotPolicy {
                recent_days: 1,
                daily_days: 30,
                max_per_document: 100,
            },
        );
        let now = at(2026, 11, 30, 12);
        store
            .capture_at(DOC, "september first", at(2026, 9, 1, 10))
            .unwrap();
        store
            .capture_at(DOC, "september last", at(2026, 9, 20, 10))
            .unwrap();
        store
            .capture_at(DOC, "october", at(2026, 10, 1, 10))
            .unwrap();

        assert_eq!(store.rotate(now).unwrap(), 1);
        assert_eq!(
            contents(&store, DOC),
            vec!["september last", "october"],
            "the oldest months collapse to their newest snapshot"
        );
    }

    #[test]
    fn test_rotation_bounds_the_retained_count_at_the_hard_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(
            tmp.path(),
            SnapshotPolicy {
                recent_days: 3650,
                daily_days: 3650,
                max_per_document: 5,
            },
        );
        let now = at(2026, 11, 30, 12);
        for minute in 0..40 {
            store
                .capture_at(DOC, &format!("v{minute}"), at(2026, 11, 30, 10))
                .unwrap();
        }

        assert_eq!(store.rotate(now).unwrap(), 35);
        let kept = contents(&store, DOC);
        assert_eq!(kept.len(), 5, "the retained count is bounded");
        assert_eq!(kept, vec!["v35", "v36", "v37", "v38", "v39"]);
    }

    #[test]
    fn test_rotation_bounds_each_document_separately() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(
            tmp.path(),
            SnapshotPolicy {
                recent_days: 3650,
                daily_days: 3650,
                max_per_document: 1,
            },
        );
        let now = at(2026, 11, 30, 12);
        store
            .capture_at(DOC, "core 1", at(2026, 11, 30, 9))
            .unwrap();
        store
            .capture_at(DOC, "core 2", at(2026, 11, 30, 10))
            .unwrap();
        store
            .capture_at("SOUL.md", "soul", at(2026, 11, 30, 9))
            .unwrap();

        assert_eq!(store.rotate(now).unwrap(), 1);
        assert_eq!(contents(&store, DOC), vec!["core 2"]);
        assert_eq!(contents(&store, "SOUL.md"), vec!["soul"]);
    }

    #[test]
    fn test_rotate_prunes_empty_run_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let store = store(
            tmp.path(),
            SnapshotPolicy {
                recent_days: 0,
                daily_days: 0,
                max_per_document: 1,
            },
        );
        let now = at(2026, 11, 30, 12);
        store.capture_at(DOC, "old", at(2026, 10, 1, 10)).unwrap();
        store.capture_at(DOC, "new", at(2026, 11, 30, 10)).unwrap();

        assert_eq!(store.rotate(now).unwrap(), 1);
        let run_dirs: Vec<String> = fs::read_dir(store.root())
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .map(|e| e.path())
            .map(|p| fs::read_dir(p).map(|d| d.count()).unwrap_or(0).to_string())
            .collect();
        assert_eq!(run_dirs.len(), 1, "the emptied run directory is gone");
    }

    // -- restore ------------------------------------------------------------

    #[test]
    fn test_restore_reproduces_the_snapshot_content_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let store = default_store(tmp.path());
        let original = "# Memory\n\n- Fact one (owner said)\n- Fact two  \n\n\tTabbed line\n";
        let snapshot = store
            .capture_at(DOC, original, at(2020, 11, 2, 10))
            .unwrap();
        write_doc(tmp.path(), "a bad dream pass rewrote everything\n");

        let restored = store.restore(DOC, &snapshot.run).unwrap().unwrap();

        assert_eq!(restored.run, snapshot.run);
        assert_eq!(read_doc(tmp.path()), original);
    }

    #[test]
    fn test_restore_snapshots_the_current_content_first() {
        let tmp = tempfile::tempdir().unwrap();
        let store = default_store(tmp.path());
        // The seeded snapshot is dated in the past so "the snapshot taken now"
        // is unambiguously the newest one.
        let snapshot = store
            .capture_at(DOC, "original", at(2020, 11, 2, 10))
            .unwrap();
        write_doc(tmp.path(), "current state");

        store.restore(DOC, &snapshot.run).unwrap();

        let kept = contents(&store, DOC);
        assert_eq!(
            kept,
            vec!["original", "current state"],
            "a rollback is itself undoable"
        );
        assert_eq!(read_doc(tmp.path()), "original");
    }

    #[test]
    fn test_restore_unknown_run_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let store = default_store(tmp.path());
        write_doc(tmp.path(), "untouched");

        assert!(
            store
                .restore(DOC, "20200101-000000-0000")
                .unwrap()
                .is_none()
        );
        assert_eq!(read_doc(tmp.path()), "untouched");
    }

    #[test]
    fn test_restore_latest_rolls_back_one_step() {
        let tmp = tempfile::tempdir().unwrap();
        let store = default_store(tmp.path());
        store.capture_at(DOC, "one", at(2026, 11, 2, 10)).unwrap();
        store.capture_at(DOC, "two", at(2026, 11, 2, 11)).unwrap();
        write_doc(tmp.path(), "three");

        let restored = store.restore_latest(DOC).unwrap().unwrap();
        assert_eq!(restored.run, "20261102-110000-0000");
        assert_eq!(read_doc(tmp.path()), "two");
    }

    // -- age by diff --------------------------------------------------------

    #[test]
    fn test_entry_age_survives_an_edit() {
        let tmp = tempfile::tempdir().unwrap();
        let store = default_store(tmp.path());
        let now = at(2026, 11, 30, 12);
        // Snapshot 0 is the pre-history state: nothing in it can be aged.
        store
            .capture_at(DOC, "# Memory\n", at(2026, 11, 1, 10))
            .unwrap();
        store
            .capture_at(
                DOC,
                "# Memory\n\n- Prefers bullet summaries\n",
                at(2026, 11, 10, 10),
            )
            .unwrap();
        store
            .capture_at(
                DOC,
                "# Memory\n\n- Prefers bullet summaries\n- Deploys at 02:00\n",
                at(2026, 11, 20, 10),
            )
            .unwrap();

        // The document was edited after the last snapshot: one old entry is
        // reworded in place, one entry is brand new.
        let edited =
            "# Memory\n\n- Prefers bullet summaries\n- Deploys at 02:00\n- Uses postgres 16\n";
        let ages = store.entry_ages(DOC, edited, now).unwrap();

        let age_of = |text: &str| {
            ages.iter()
                .find(|a| a.text == text)
                .unwrap_or_else(|| panic!("no age for {text}"))
                .clone()
        };
        assert_eq!(age_of("- Prefers bullet summaries").age_days, Some(20));
        assert_eq!(age_of("- Deploys at 02:00").age_days, Some(10));
        assert_eq!(
            age_of("- Uses postgres 16").age_days,
            Some(0),
            "a line written after the last snapshot is new"
        );
        assert!(
            !edited.contains("- Prefers bullet summaries · 20d"),
            "ages are a side channel, never written into the document"
        );
        assert!(
            !contains_date_text(edited),
            "the document carries no date text: {edited}"
        );
    }

    #[test]
    fn test_entry_ages_ignore_heading_changes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = default_store(tmp.path());
        let now = at(2026, 11, 30, 12);
        store.capture_at(DOC, "", at(2026, 11, 1, 10)).unwrap();
        store
            .capture_at(DOC, "## Deploy\n\n- Runs at 02:00\n", at(2026, 11, 10, 10))
            .unwrap();

        // The section was renamed; the entry's age must not reset.
        let renamed = "## Deployment\n\n- Runs at 02:00\n";
        let ages = store.entry_ages(DOC, renamed, now).unwrap();

        assert_eq!(ages.len(), 1, "headings produce no entry of their own");
        assert_eq!(ages[0].age_days, Some(20));
    }

    #[test]
    fn test_entry_ages_are_unknown_without_earlier_snapshots() {
        let tmp = tempfile::tempdir().unwrap();
        let store = default_store(tmp.path());
        let now = at(2026, 11, 30, 12);
        store
            .capture_at(DOC, "- Ancient fact\n", at(2026, 11, 1, 10))
            .unwrap();

        let ages = store
            .entry_ages(DOC, "- Ancient fact\n- Fresh fact\n", now)
            .unwrap();

        assert_eq!(
            ages[0].age_days, None,
            "the oldest retained snapshot proves nothing: do not extrapolate"
        );
        assert_eq!(
            ages[0].first_seen_run.as_deref(),
            Some("20261101-100000-0000")
        );
        assert_eq!(ages[1].age_days, Some(0));
    }

    #[test]
    fn test_entry_ages_without_any_snapshot_are_new() {
        let tmp = tempfile::tempdir().unwrap();
        let store = default_store(tmp.path());

        let ages = store
            .entry_ages(DOC, "- Anything\n", at(2026, 11, 30, 12))
            .unwrap();

        assert_eq!(ages.len(), 1);
        assert_eq!(ages[0].age_days, Some(0));
        assert_eq!(ages[0].first_seen_run, None);
    }

    fn contains_date_text(text: &str) -> bool {
        let bytes = text.as_bytes();
        bytes.windows(10).any(|window| {
            window[4] == b'-'
                && window[7] == b'-'
                && window[..4].iter().all(u8::is_ascii_digit)
                && window[5..7].iter().all(u8::is_ascii_digit)
                && window[8..].iter().all(u8::is_ascii_digit)
        })
    }

    // -- age-marker cleaning ------------------------------------------------

    fn strip(text: &str) -> String {
        strip_age_markers(text).0
    }

    #[test]
    fn test_strip_removes_known_marker_forms_at_line_end() {
        assert_eq!(strip("- Prefers bullets ← 21d"), "- Prefers bullets");
        assert_eq!(strip("- Prefers bullets [21d]"), "- Prefers bullets");
        assert_eq!(
            strip("- Prefers bullets (21 days old)"),
            "- Prefers bullets"
        );
        assert_eq!(
            strip("- Deploys at 02:00 (1 day old)"),
            "- Deploys at 02:00"
        );
    }

    #[test]
    fn test_strip_never_touches_mid_sentence_digits() {
        // The narrowest-rule rule: a bare `10d` inside a sentence is content,
        // not a marker, and a parenthetical that is not `N days old` survives.
        assert_eq!(strip("- 每周三 10d 会议"), "- 每周三 10d 会议");
        assert_eq!(
            strip("- meeting (room 3) daily"),
            "- meeting (room 3) daily"
        );
        assert_eq!(
            strip("- runs [a] then [b] flow"),
            "- runs [a] then [b] flow"
        );
    }

    #[test]
    fn test_strip_counts_changed_lines_and_preserves_the_rest() {
        let doc = "# Memory\n\n- old fact ← 40d\n- fresh fact\n- another (2 days old)\n";
        let (cleaned, count) = strip_age_markers(doc);
        assert_eq!(cleaned, "# Memory\n\n- old fact\n- fresh fact\n- another\n");
        assert_eq!(count, 2, "only the two lines carrying a marker changed");
        assert!(
            cleaned.ends_with('\n'),
            "the trailing newline of a markdown doc is preserved"
        );
    }

    #[test]
    fn test_strip_is_idempotent_and_noop_on_clean_text() {
        let doc = "# Memory\n\n- a plain bullet\n- (source · owner said)\n";
        let (once, count) = strip_age_markers(doc);
        assert_eq!(once, doc);
        assert_eq!(count, 0, "a provenance note without a day count is content");
        // Stripping an already-clean document a second time changes nothing.
        assert_eq!(strip_age_markers(&strip_age_markers(doc).0).0, doc);
    }

    #[test]
    fn test_snapshot_store_is_a_plain_directory_not_a_git_repository() {
        // Rollback is directory copies: no index, no locks, nothing that can
        // fight an owner edit of `memory/`.
        let tmp = tempfile::tempdir().unwrap();
        let store = default_store(tmp.path());
        store.capture_at(DOC, "x", at(2026, 11, 2, 10)).unwrap();

        let mut stack = vec![store.root().to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                assert_ne!(
                    path.file_name().unwrap().to_str().unwrap(),
                    ".git",
                    "snapshots must not create a repository"
                );
                if path.is_dir() {
                    stack.push(path);
                }
            }
        }
    }
}

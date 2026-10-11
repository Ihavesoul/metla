//! Action planning and execution: permanent delete (no Trash — by design)
//! and archive-then-delete into a single streaming tar.zst.
//!
//! Planning happens on the UI thread (it needs the arena); execution is a
//! pure function over owned data, so it can run on a worker thread with no
//! shared state except the progress atomics.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::archive::{self, ArchiveItem};
use crate::arena::{FileArena, NameRef};
use crate::format::HumanBytes;
use crate::types::{Bytes, NodeId};
use crate::MetlaError;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum PlannedAction {
    Archive,
    Delete,
}

impl PlannedAction {
    pub fn verb(self) -> &'static str {
        match self {
            PlannedAction::Archive => "archive",
            PlannedAction::Delete => "delete",
        }
    }
}

pub struct ActionItem {
    pub node: NodeId,
    pub path: PathBuf,
    pub entry_name: std::ffi::OsString,
    pub logical: Bytes,
    pub physical: Bytes,
}

pub struct ActionPlan {
    pub action: PlannedAction,
    pub items: Vec<ActionItem>,
    pub logical: Bytes,
    pub physical: Bytes,
    /// Targets refused by the safety net; shown in the confirm dialog.
    pub warnings: Vec<String>,
}

impl ActionPlan {
    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

/// Worker-side progress the UI samples. `current` carries the path being
/// worked on right now — without it a single-huge-file plan sits at 0/1 and
/// the user cannot tell what is happening.
pub struct WorkProgress {
    pub done: AtomicU64,
    pub total: AtomicU64,
    /// 1 = archiving, 2 = deleting.
    pub phase: AtomicU8,
    pub cancelled: AtomicBool,
    current: Mutex<String>,
}

pub const PHASE_ARCHIVE: u8 = 1;
pub const PHASE_DELETE: u8 = 2;

impl WorkProgress {
    pub fn new() -> Self {
        WorkProgress {
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            phase: AtomicU8::new(0),
            cancelled: AtomicBool::new(false),
            current: Mutex::new(String::new()),
        }
    }

    pub fn request_cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    pub fn set_current(&self, path: &Path) {
        if let Ok(mut cur) = self.current.lock() {
            cur.clear();
            cur.push_str(&path.display().to_string());
        }
    }

    pub fn current(&self) -> String {
        self.current.lock().map(|c| c.clone()).unwrap_or_default()
    }

    pub fn phase_name(&self) -> &'static str {
        match self.phase.load(Ordering::Relaxed) {
            PHASE_ARCHIVE => "archiving",
            PHASE_DELETE => "deleting",
            _ => "working",
        }
    }
}

impl Default for WorkProgress {
    fn default() -> Self {
        Self::new()
    }
}

pub struct ActionReport {
    pub freed_logical: Bytes,
    pub freed_physical: Bytes,
    /// `(path, size)` of the tar.zst created, if any.
    pub archived: Option<(PathBuf, u64)>,
    /// Nodes whose subtrees must be tombstoned in the arena.
    pub removed: Vec<NodeId>,
    /// Paths handled successfully (for the operation log).
    pub done: Vec<PathBuf>,
    pub errors: Vec<String>,
    /// True when the user asked to cancel and the plan stopped early.
    pub cancelled: bool,
}

/// Paths metla must never touch, whatever the user asked for.
fn protected_violation(
    path: &Path,
    home: &Path,
    archive_dir: &Path,
    scan_root: &Path,
) -> Option<String> {
    const SYSTEM_PREFIXES: [&str; 8] = [
        "/System",
        "/bin",
        "/sbin",
        "/usr",
        "/dev",
        "/private/etc",
        "/private/var/db",
        "/private/var/vm",
    ];
    if path == home {
        return Some("this is your home directory".into());
    }
    if path == scan_root {
        return Some("this is the scan root".into());
    }
    if path == archive_dir {
        return Some("this is the archive directory".into());
    }
    // Deleting an ancestor of the archive dir would destroy the archive.
    if archive_dir.starts_with(path) && path != archive_dir {
        return Some("contains the archive directory".into());
    }
    if path.starts_with(archive_dir) {
        return Some("inside the archive directory".into());
    }
    for p in SYSTEM_PREFIXES {
        if path.starts_with(p) {
            return Some(format!("system path ({p})"));
        }
    }
    None
}

/// Turn targets into a safe, de-duplicated plan. Items nested under another
/// target are dropped (the ancestor covers them). Never fails: unsafe
/// targets become warnings.
pub fn build_plan(
    arena: &FileArena,
    prefix: &Path,
    targets: &[NodeId],
    action: PlannedAction,
    archive_dir: &Path,
    home: &Path,
) -> ActionPlan {
    let target_set: HashSet<NodeId> = targets.iter().copied().collect();
    let mut items = Vec::with_capacity(targets.len());
    let mut warnings = Vec::new();
    let (mut logical, mut physical) = (Bytes(0), Bytes(0));
    let mut scratch: Vec<NameRef> = Vec::with_capacity(32);
    let mut path = PathBuf::with_capacity(256);

    for &id in targets {
        if !id.is_some() || arena.is_freed(id) {
            continue;
        }
        // Covered by an ancestor target?
        let mut p = arena.parent(id);
        let mut covered = false;
        while p.is_some() {
            if target_set.contains(&p) {
                covered = true;
                break;
            }
            p = arena.parent(p);
        }
        if covered {
            continue;
        }

        arena.build_path(id, prefix, &mut scratch, &mut path);
        match protected_violation(&path, home, archive_dir, prefix) {
            Some(reason) => warnings.push(format!("{}: {reason}", path.display())),
            None => {
                let n = arena.node(id);
                logical = logical + n.subtree_size;
                physical = physical + n.subtree_physical;
                items.push(ActionItem {
                    node: id,
                    path: path.clone(),
                    entry_name: arena.name(id).to_os_string(),
                    logical: n.subtree_size,
                    physical: n.subtree_physical,
                });
            }
        }
    }

    ActionPlan {
        action,
        items,
        logical,
        physical,
        warnings,
    }
}

/// Execute a plan. Archive first (streaming, originals untouched on
/// failure), then delete. Every successful item's node is recorded in
/// `removed` so the UI can tombstone the arena afterwards. Cancellation is
/// checked between items — an item already being written or unlinked is
/// finished, but nothing new starts.
pub fn execute(plan: ActionPlan, archive_dir: &Path, progress: &WorkProgress) -> ActionReport {
    let total = plan.items.len() as u64;
    progress.total.store(total, Ordering::Relaxed);
    progress.done.store(0, Ordering::Relaxed);
    let mut report = ActionReport {
        freed_logical: Bytes(0),
        freed_physical: Bytes(0),
        archived: None,
        removed: Vec::with_capacity(plan.items.len()),
        done: Vec::with_capacity(plan.items.len()),
        errors: Vec::new(),
        cancelled: false,
    };

    let plan = if plan.action == PlannedAction::Archive && !plan.items.is_empty() {
        progress.phase.store(PHASE_ARCHIVE, Ordering::Relaxed);
        match archive_one(&plan, archive_dir, progress, &mut report) {
            Ok(()) => plan,
            Err(ArchiveOneOutcome::Cancelled) => {
                report.cancelled = true;
                return report; // originals untouched, partial archive removed
            }
            Err(ArchiveOneOutcome::Failed(msg)) => {
                report.errors.push(msg);
                return report; // originals untouched
            }
        }
    } else {
        plan
    };

    progress.phase.store(PHASE_DELETE, Ordering::Relaxed);
    for item in plan.items {
        if progress.is_cancelled() {
            report.cancelled = true;
            break;
        }
        progress.set_current(&item.path);
        let res = if item.path.is_dir() {
            fs::remove_dir_all(&item.path)
        } else {
            fs::remove_file(&item.path)
        };
        match res {
            Ok(()) => {
                report.freed_logical = report.freed_logical + item.logical;
                report.freed_physical = report.freed_physical + item.physical;
                report.removed.push(item.node);
                report.done.push(item.path.clone());
            }
            Err(e) => report.errors.push(format!("{}: {e}", item.path.display())),
        }
        progress.done.fetch_add(1, Ordering::Relaxed);
    }
    report
}

enum ArchiveOneOutcome {
    Cancelled,
    Failed(String),
}

fn archive_one(
    plan: &ActionPlan,
    archive_dir: &Path,
    progress: &WorkProgress,
    report: &mut ActionReport,
) -> Result<(), ArchiveOneOutcome> {
    let items: Vec<ArchiveItem> = plan
        .items
        .iter()
        .map(|i| ArchiveItem {
            absolute: i.path.clone(),
            entry_name: i.entry_name.clone(),
        })
        .collect();
    let dest = propose_archive_path(archive_dir);
    let first = plan.items.first().map(|i| i.path.clone());
    if let Some(p) = first {
        progress.set_current(&p);
    }
    let should_continue = || !progress.is_cancelled();
    match archive::write_archive(&items, &dest, &should_continue) {
        Ok(written) => report.archived = Some((dest, written)),
        Err(MetlaError::Aborted) => return Err(ArchiveOneOutcome::Cancelled),
        Err(e) => {
            return Err(ArchiveOneOutcome::Failed(match e {
                MetlaError::Archive(msg) => msg,
                other => other.to_string(),
            }))
        }
    }
    progress.done.store(0, Ordering::Relaxed);
    Ok(())
}

/// Destination the next archive will get (`<dir>/metla-YYYYmmdd-HHMMSS.tar.zst`).
pub fn propose_archive_path(archive_dir: &Path) -> PathBuf {
    archive_dir.join(archive_name())
}

fn archive_name() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let t = crate::types::UnixTime(secs);
    let (y, m, d) = t.civil();
    let (hh, mm, ss) = t.hms();
    format!("metla-{y:04}{m:02}{d:02}-{hh:02}{mm:02}{ss:02}.tar.zst")
}

/// One-line human summary of a plan/report ("delete 3 items · 1.5G").
pub fn summarize(
    verb: &str,
    items: usize,
    logical: Bytes,
    physical: Bytes,
    show_physical: bool,
    out: &mut String,
) {
    out.clear();
    out.push_str(verb);
    out.push_str(&format!(
        " {items} item{} · {}",
        if items == 1 { "" } else { "s" },
        HumanBytes(logical.0)
    ));
    if show_physical && physical != logical {
        out.push_str(&format!(" (phys {})", HumanBytes(physical.0)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::{FileArena, RawNode};
    use crate::types::{NodeKind, UnixTime};

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("metla-act-{tag}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture(root: &Path) -> FileArena {
        let mut a = FileArena::new();
        let r = a.push(
            NodeId::NONE,
            &RawNode {
                name: b"",
                kind: NodeKind::Dir,
                flags: 0,
                junk: 0,
                size: Bytes(0),
                physical: Bytes(0),
                mtime: UnixTime(0),
            },
        );
        a.set_root(r);
        let junkd = a.push(
            r,
            &RawNode {
                junk: crate::junk::JunkCategory::NodeModules.as_code(),
                ..RawNode {
                    name: b"stuff",
                    kind: NodeKind::Dir,
                    flags: 0,
                    junk: 0,
                    size: Bytes(0),
                    physical: Bytes(0),
                    mtime: UnixTime(0),
                }
            },
        );
        a.push(
            junkd,
            &RawNode {
                name: b"dep.bin",
                ..RawNode {
                    name: b"",
                    kind: NodeKind::File,
                    flags: 0,
                    junk: 0,
                    size: Bytes(4096),
                    physical: Bytes(4096),
                    mtime: UnixTime(0),
                }
            },
        );
        a.push(
            r,
            &RawNode {
                name: b"keep.txt",
                ..RawNode {
                    name: b"",
                    kind: NodeKind::File,
                    flags: 0,
                    junk: 0,
                    size: Bytes(10),
                    physical: Bytes(10),
                    mtime: UnixTime(0),
                }
            },
        );
        a.aggregate();
        // materialise on disk
        fs::create_dir_all(root.join("stuff")).unwrap();
        fs::write(root.join("stuff/dep.bin"), vec![0u8; 4096]).unwrap();
        fs::write(root.join("keep.txt"), b"keep").unwrap();
        a
    }

    #[test]
    fn delete_plan_removes_files() {
        let tmp = TempDir::new("del");
        let arena = fixture(&tmp.0);
        let root = arena.root();
        let mut kids = Vec::new();
        arena.collect_children(root, &mut kids);
        let stuff = *kids.iter().find(|&&k| arena.name(k) == "stuff").unwrap();

        let plan = build_plan(
            &arena,
            &tmp.0,
            &[stuff],
            PlannedAction::Delete,
            Path::new("/nonexistent-archive"),
            Path::new("/Users/nobody"),
        );
        assert_eq!(plan.len(), 1);
        assert_eq!(plan.logical, Bytes(4096));

        let progress = WorkProgress::new();
        let report = execute(plan, Path::new("/nonexistent-archive"), &progress);
        assert!(report.errors.is_empty());
        assert_eq!(report.freed_logical, Bytes(4096));
        assert_eq!(report.removed, vec![stuff]);
        assert!(!tmp.0.join("stuff").exists());
        assert!(tmp.0.join("keep.txt").exists());
    }

    #[test]
    fn archive_plan_writes_and_frees() {
        let tmp = TempDir::new("arc");
        let arena = fixture(&tmp.0);
        let root = arena.root();
        let mut kids = Vec::new();
        arena.collect_children(root, &mut kids);
        let stuff = *kids.iter().find(|&&k| arena.name(k) == "stuff").unwrap();

        let archive_dir = tmp.0.join("archive");
        let plan = build_plan(
            &arena,
            &tmp.0,
            &[stuff],
            PlannedAction::Archive,
            &archive_dir,
            Path::new("/Users/nobody"),
        );
        let progress = WorkProgress::new();
        let report = execute(plan, &archive_dir, &progress);

        assert!(report.errors.is_empty());
        assert_eq!(report.freed_logical, Bytes(4096));
        let (dest, size) = report.archived.expect("archive written");
        assert!(dest.exists());
        assert!(size > 0);
        assert!(!tmp.0.join("stuff").exists());
    }

    #[test]
    fn protected_targets_are_refused() {
        let tmp = TempDir::new("prot");
        let arena = fixture(&tmp.0);
        let root = arena.root();

        let plan = build_plan(
            &arena,
            &tmp.0,
            &[root],
            PlannedAction::Delete,
            Path::new("/nonexistent-archive"),
            Path::new("/Users/nobody"),
        );
        assert!(plan.is_empty());
        assert_eq!(plan.warnings.len(), 1); // scan root

        let mut kids = Vec::new();
        arena.collect_children(root, &mut kids);
        let stuff = *kids.iter().find(|&&k| arena.name(k) == "stuff").unwrap();
        // an archive dir placed INSIDE a target makes that target untouchable
        let archive_dir = tmp.0.join("stuff").join("archive");
        let plan = build_plan(
            &arena,
            &tmp.0,
            &[stuff],
            PlannedAction::Delete,
            &archive_dir,
            Path::new("/Users/nobody"),
        );
        assert!(plan.is_empty());
        assert!(plan
            .warnings
            .iter()
            .any(|w| w.contains("contains the archive")));
    }

    #[test]
    fn cancel_before_execute_touches_nothing() {
        let tmp = TempDir::new("cancel");
        let arena = fixture(&tmp.0);
        let root = arena.root();
        let mut kids = Vec::new();
        arena.collect_children(root, &mut kids);
        let stuff = *kids.iter().find(|&&k| arena.name(k) == "stuff").unwrap();

        let plan = build_plan(
            &arena,
            &tmp.0,
            &[stuff],
            PlannedAction::Delete,
            Path::new("/nonexistent-archive"),
            Path::new("/Users/nobody"),
        );
        assert_eq!(plan.len(), 1);

        let progress = WorkProgress::new();
        progress.request_cancel();
        let report = execute(plan, Path::new("/nonexistent-archive"), &progress);
        assert!(report.cancelled);
        assert!(report.removed.is_empty());
        assert!(report.errors.is_empty());
        // nothing was deleted
        assert!(tmp.0.join("stuff/dep.bin").exists());
    }

    #[test]
    fn nested_targets_are_deduped() {
        let tmp = TempDir::new("nest");
        let arena = fixture(&tmp.0);
        let root = arena.root();
        let mut kids = Vec::new();
        arena.collect_children(root, &mut kids);
        let stuff = *kids.iter().find(|&&k| arena.name(k) == "stuff").unwrap();
        let mut inner = Vec::new();
        arena.collect_children(stuff, &mut inner);

        let plan = build_plan(
            &arena,
            &tmp.0,
            &[stuff, inner[0]],
            PlannedAction::Delete,
            Path::new("/nonexistent-archive"),
            Path::new("/Users/nobody"),
        );
        assert_eq!(plan.len(), 1); // inner covered by parent
    }
}

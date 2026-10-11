//! Post-scan heuristics: tiny-file crowds, old heavyweights, stale archives,
//! oversized logs and the junk categories tagged during the walk. One O(N)
//! pass over the arena, writing into a caller-owned scratch vector.

use std::os::unix::ffi::OsStrExt;

use crate::arena::FileArena;
use crate::junk::JunkCategory;
use crate::types::{Bytes, NodeId, NodeKind, UnixTime};

#[derive(Debug, Clone, Copy)]
pub struct Rules {
    /// Tiny-crowd: a directory holding at least this many small files…
    pub crowd_min_files: u64,
    /// …each no larger than this…
    pub crowd_max_child: Bytes,
    /// …with the whole crowd no larger than this…
    pub crowd_max_total: Bytes,
    /// …and untouched for at least this many days.
    pub crowd_min_age_days: u32,
    /// …and the crowd actually occupies at least this much *on disk* —
    /// keeps evicted cloud placeholders (full logical size, zero blocks)
    /// from triggering an archive that would force a full download.
    pub crowd_min_reclaim: Bytes,
    /// Old heavyweight: a single file occupying at least this much on disk,
    pub old_heavy_min: Bytes,
    /// …untouched for at least this many days.
    pub old_heavy_min_age_days: u32,
    /// Stale archive: compressed data at least this big on disk,
    pub stale_archive_min: Bytes,
    /// …untouched for at least this many days.
    pub stale_archive_age_days: u32,
    /// Big log: a `*.log` file of at least this size on disk.
    pub big_log_min: Bytes,
    /// Hard cap so the panel stays browsable; rows are ranked by size.
    pub max_suggestions: usize,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            crowd_min_files: 200,
            crowd_max_child: Bytes(64 * 1024),
            crowd_max_total: Bytes(512 * 1024 * 1024),
            crowd_min_age_days: 90,
            crowd_min_reclaim: Bytes(1024 * 1024),
            old_heavy_min: Bytes(256 * 1024 * 1024),
            old_heavy_min_age_days: 365,
            stale_archive_min: Bytes(10 * 1024 * 1024),
            stale_archive_age_days: 180,
            big_log_min: Bytes(128 * 1024 * 1024),
            max_suggestions: 400,
        }
    }
}

/// What the tool would do with a suggestion if you pressed its key.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum RecommendedAction {
    Archive,
    Delete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestionKind {
    Junk(JunkCategory),
    TinyCrowd { files: u64 },
    OldHeavy { age_days: u32 },
    StaleArchive { age_days: u32 },
    BigLog,
}

impl SuggestionKind {
    /// Short reason line shown next to the path.
    pub fn reason(self) -> &'static str {
        match self {
            SuggestionKind::Junk(_) => "regenerable / expendable",
            SuggestionKind::TinyCrowd { .. } => "crowd of small old files",
            SuggestionKind::OldHeavy { .. } => "heavy and untouched",
            SuggestionKind::StaleArchive { .. } => "already compressed, stale",
            SuggestionKind::BigLog => "oversized log",
        }
    }

    /// Short tag for the category column.
    pub fn tag(self) -> &'static str {
        match self {
            SuggestionKind::Junk(cat) => cat.as_str(),
            SuggestionKind::TinyCrowd { .. } => "tiny crowd",
            SuggestionKind::OldHeavy { .. } => "old heavyweight",
            SuggestionKind::StaleArchive { .. } => "stale archive",
            SuggestionKind::BigLog => "big log",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Suggestion {
    pub target: NodeId,
    pub kind: SuggestionKind,
    /// Reclaimable estimate: what the subtree actually occupies on disk
    /// (`st_blocks`), not the apparent size — evicted cloud files count ~0.
    pub bytes: Bytes,
    /// Number of files in the affected subtree.
    pub files: u64,
    pub action: RecommendedAction,
}

const ARCHIVE_SUFFIXES: [&[u8]; 17] = [
    b".zip",
    b".7z",
    b".rar",
    b".dmg",
    b".iso",
    b".tar",
    b".tgz",
    b".tbz2",
    b".txz",
    b".gz",
    b".bz2",
    b".xz",
    b".zst",
    b".tar.gz",
    b".tar.bz2",
    b".tar.xz",
    b".tar.zst",
];

fn has_archive_suffix(name: &[u8]) -> bool {
    ARCHIVE_SUFFIXES
        .iter()
        .any(|s| name.len() >= s.len() && &name[name.len() - s.len()..] == *s)
}

fn is_log(name: &[u8]) -> bool {
    name.len() >= 4 && &name[name.len() - 4..] == b".log"
}

/// Run all rules and leave results in `out`, ranked by reclaimable bytes.
pub fn run(arena: &FileArena, rules: &Rules, now: UnixTime, out: &mut Vec<Suggestion>) {
    out.clear();
    let mut stack: Vec<(NodeId, bool)> = Vec::with_capacity(64);
    stack.push((arena.root(), false));

    while let Some((id, reported)) = stack.pop() {
        // `quiet` is inherited from a suggested ancestor — or set when this
        // directory is itself junk, so nothing inside it fires again.
        let this_junk = arena.node(id).junk;
        let quiet = reported || this_junk != 0;
        let mut crowd_files = 0u64;
        let mut crowd_logical = 0u64;
        let mut crowd_physical = 0u64;
        let mut newest_child = UnixTime(i64::MIN);

        for child in arena.children(id) {
            let n = arena.node(child);
            if n.mtime.0 > newest_child.0 {
                newest_child = n.mtime;
            }
            match n.kind {
                NodeKind::Dir => stack.push((child, quiet)),
                NodeKind::File => {
                    if n.size.0 <= rules.crowd_max_child.0 {
                        crowd_files += 1;
                        crowd_logical += n.size.0;
                        crowd_physical += n.physical.0;
                    }
                    if !quiet {
                        check_file(arena, child, n.physical, n.mtime, rules, now, out);
                    }
                }
                _ => {}
            }
        }

        let this = arena.node(id);
        if this.kind != NodeKind::Dir {
            continue;
        }
        if let Some(cat) = JunkCategory::from_code(this.junk) {
            if !reported {
                out.push(Suggestion {
                    target: id,
                    kind: SuggestionKind::Junk(cat),
                    bytes: this.subtree_physical,
                    files: this.subtree_files,
                    action: if cat.regenerable() {
                        RecommendedAction::Delete
                    } else {
                        RecommendedAction::Archive
                    },
                });
            }
        } else if !reported
            && crowd_files >= rules.crowd_min_files
            && crowd_logical <= rules.crowd_max_total.0
            && crowd_physical >= rules.crowd_min_reclaim.0
            && newest_child.days_before(now) >= rules.crowd_min_age_days
        {
            out.push(Suggestion {
                target: id,
                kind: SuggestionKind::TinyCrowd { files: crowd_files },
                bytes: Bytes(crowd_physical),
                files: crowd_files,
                action: RecommendedAction::Archive,
            });
        }
    }

    out.sort_unstable_by_key(|s| std::cmp::Reverse(s.bytes));
    out.truncate(rules.max_suggestions);
}

fn check_file(
    arena: &FileArena,
    id: NodeId,
    // Physical size — thresholds and estimates work in *real* disk usage so
    // evicted cloud files (huge apparent size, zero blocks) never fire.
    on_disk: Bytes,
    mtime: UnixTime,
    rules: &Rules,
    now: UnixTime,
    out: &mut Vec<Suggestion>,
) {
    if arena.is_hardlink(id) {
        return;
    }
    let name = arena.name(id).as_bytes();
    let age = mtime.days_before(now);
    if on_disk.0 >= rules.old_heavy_min.0 && age >= rules.old_heavy_min_age_days {
        out.push(Suggestion {
            target: id,
            kind: SuggestionKind::OldHeavy { age_days: age },
            bytes: on_disk,
            files: 1,
            action: RecommendedAction::Archive,
        });
    } else if is_log(name) && on_disk.0 >= rules.big_log_min.0 {
        out.push(Suggestion {
            target: id,
            kind: SuggestionKind::BigLog,
            bytes: on_disk,
            files: 1,
            action: RecommendedAction::Delete,
        });
    } else if has_archive_suffix(name)
        && on_disk.0 >= rules.stale_archive_min.0
        && age >= rules.stale_archive_age_days
    {
        out.push(Suggestion {
            target: id,
            kind: SuggestionKind::StaleArchive { age_days: age },
            bytes: on_disk,
            files: 1,
            action: RecommendedAction::Delete,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::{FileArena, RawNode};

    const NOW: UnixTime = UnixTime(1_760_000_000); // 2025-10-09

    fn days_ago(days: u32) -> UnixTime {
        UnixTime(NOW.0 - i64::from(days) * 86_400)
    }

    struct Fixture {
        arena: FileArena,
    }

    fn build() -> Fixture {
        let mut a = FileArena::new();
        let root = a.push(NodeId::NONE, &dir(b""));
        a.set_root(root);
        Fixture { arena: a }
    }

    fn dir(name: &[u8]) -> RawNode<'_> {
        RawNode {
            name,
            kind: NodeKind::Dir,
            flags: 0,
            junk: 0,
            size: Bytes(0),
            physical: Bytes(0),
            mtime: NOW,
        }
    }

    fn file<'a>(name: &'a [u8], size: u64, mtime: UnixTime) -> RawNode<'a> {
        file_phys(name, size, size, mtime)
    }

    fn file_phys<'a>(name: &'a [u8], size: u64, physical: u64, mtime: UnixTime) -> RawNode<'a> {
        RawNode {
            name,
            kind: NodeKind::File,
            flags: 0,
            junk: 0,
            size: Bytes(size),
            physical: Bytes(physical),
            mtime,
        }
    }

    fn kinds(out: &[Suggestion]) -> Vec<SuggestionKind> {
        out.iter().map(|s| s.kind).collect()
    }

    #[test]
    fn junk_tag_becomes_suggestion() {
        let mut f = build();
        let root = f.arena.root();
        let nm = f.arena.push(
            root,
            &RawNode {
                junk: JunkCategory::NodeModules.as_code(),
                ..dir(b"node_modules")
            },
        );
        f.arena.push(nm, &file(b"dep.js", 1000, days_ago(30)));
        f.arena.aggregate();

        let mut out = Vec::new();
        run(&f.arena, &Rules::default(), NOW, &mut out);
        assert_eq!(
            kinds(&out),
            vec![SuggestionKind::Junk(JunkCategory::NodeModules)]
        );
        assert_eq!(out[0].action, RecommendedAction::Delete);
        assert_eq!(out[0].bytes, Bytes(1000)); // physical = logical here
    }

    #[test]
    fn tiny_crowd_detected() {
        let mut f = build();
        let root = f.arena.root();
        let crowd = f.arena.push(root, &dir(b"crowd"));
        for i in 0..250u32 {
            let name = format!("file{i}.dat");
            // 250 × 8 KB on disk = 2 MB ≥ crowd_min_reclaim
            f.arena.push(
                crowd,
                &file_phys(name.as_bytes(), 1024, 8192, days_ago(400)),
            );
        }
        // fresh files must keep the crowd rule quiet
        let fresh = f.arena.push(root, &dir(b"fresh"));
        for i in 0..250u32 {
            let name = format!("new{i}.dat");
            f.arena
                .push(fresh, &file_phys(name.as_bytes(), 1024, 8192, days_ago(1)));
        }
        f.arena.aggregate();

        let mut out = Vec::new();
        run(&f.arena, &Rules::default(), NOW, &mut out);
        let ks = kinds(&out);
        assert_eq!(ks.len(), 1);
        assert!(matches!(ks[0], SuggestionKind::TinyCrowd { files: 250 }));
        assert_eq!(out[0].bytes, Bytes(250 * 8192));
        assert_eq!(out[0].action, RecommendedAction::Archive);
    }

    #[test]
    fn cloud_placeholders_do_not_trigger_crowd() {
        let mut f = build();
        let root = f.arena.root();
        let cloud = f.arena.push(root, &dir(b"cloud-drive"));
        for i in 0..250u32 {
            let name = format!("doc{i}.dat");
            // evicted placeholders: full apparent size, zero blocks on disk
            f.arena
                .push(cloud, &file_phys(name.as_bytes(), 4096, 0, days_ago(400)));
        }
        f.arena.aggregate();

        let mut out = Vec::new();
        run(&f.arena, &Rules::default(), NOW, &mut out);
        assert!(out.is_empty(), "nothing to reclaim on disk → no suggestion");
    }

    #[test]
    fn file_rules_fire_by_priority() {
        let mut f = build();
        let root = f.arena.root();
        // Old heavyweight: 300MB on disk, 400 days old.
        f.arena
            .push(root, &file(b"movie.mkv", 300 * 1024 * 1024, days_ago(400)));
        // Big log: 200MB log.
        f.arena
            .push(root, &file(b"app.log", 200 * 1024 * 1024, days_ago(10)));
        // Stale archive: 20MB zip, 200 days old.
        f.arena
            .push(root, &file(b"backup.zip", 20 * 1024 * 1024, days_ago(200)));
        // Fresh archive: too young.
        f.arena
            .push(root, &file(b"fresh.zip", 20 * 1024 * 1024, days_ago(3)));
        // Small old file: no rule.
        f.arena.push(root, &file(b"note.txt", 100, days_ago(800)));
        // Cloud heavyweight: huge apparent size, nothing on disk — must NOT fire.
        f.arena.push(
            root,
            &file_phys(b"cloud.movie.mkv", 800 * 1024 * 1024, 0, days_ago(400)),
        );
        f.arena.aggregate();

        let mut out = Vec::new();
        run(&f.arena, &Rules::default(), NOW, &mut out);
        let ks = kinds(&out);
        assert_eq!(ks.len(), 3);
        assert!(ks.contains(&SuggestionKind::OldHeavy { age_days: 400 }));
        assert!(ks.contains(&SuggestionKind::BigLog));
        assert!(ks.contains(&SuggestionKind::StaleArchive { age_days: 200 }));
        // ranked by on-disk bytes: movie.mkv (300M) first
        assert_eq!(out[0].bytes, Bytes(300 * 1024 * 1024));
        assert_eq!(out[0].action, RecommendedAction::Archive);
    }

    #[test]
    fn rules_do_not_fire_inside_junk() {
        let mut f = build();
        let root = f.arena.root();
        let dd = f.arena.push(
            root,
            &RawNode {
                junk: JunkCategory::DerivedData.as_code(),
                ..dir(b"DerivedData")
            },
        );
        f.arena
            .push(dd, &file(b"huge.o", 400 * 1024 * 1024, days_ago(500)));
        f.arena.aggregate();

        let mut out = Vec::new();
        run(&f.arena, &Rules::default(), NOW, &mut out);
        // Only the DerivedData suggestion itself, not the heavyweight inside.
        assert_eq!(
            kinds(&out),
            vec![SuggestionKind::Junk(JunkCategory::DerivedData)]
        );
    }
}

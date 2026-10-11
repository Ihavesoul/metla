//! Parallel breadth-first indexer: one rayon task per directory.
//!
//! Each task reads one directory with `read_dir`, `lstat`s every entry
//! (symlinks are never followed), batches the results and appends them to
//! the shared arena under one lock per directory — not per file. Sub-
//! directories become new rayon tasks with their precomputed node ids.
//! Hardlinks are deduplicated through a shared `(dev, ino)` table so sizes
//! are never double-counted.

use std::collections::HashSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant, UNIX_EPOCH};

use crate::arena::{FileArena, RawNode};
use crate::junk::{self, JunkCategory};
use crate::types::{flags, Bytes, NodeId, NodeKind, UnixTime};
use crate::MetlaError;

const MAX_DEPTH: u16 = 128;

pub struct ScanOptions {
    pub root: PathBuf,
    pub dedup_hardlinks: bool,
    /// Absolute paths never to enter (archive dir, pseudo-filesystems).
    pub skip_paths: Vec<PathBuf>,
}

/// Live counters the UI samples without blocking the walker.
pub struct ScanProgress {
    pub files: AtomicU64,
    pub dirs: AtomicU64,
    pub errors: AtomicU64,
    pub bytes: AtomicU64,
    pub stop: AtomicBool,
}

impl ScanProgress {
    pub fn new() -> Self {
        ScanProgress {
            files: AtomicU64::new(0),
            dirs: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            stop: AtomicBool::new(false),
        }
    }
}

impl Default for ScanProgress {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct ScanStats {
    pub files: u64,
    pub dirs: u64,
    pub errors: u64,
    pub bytes: Bytes,
    pub elapsed: Duration,
}

/// Paths never to enter: the archive directory always, macOS pseudo-volumes
/// when scanning `/`, and the File-Provider cloud trees under `$HOME`.
///
/// Cloud dirs (`~/Library/Mobile Documents`, `~/Library/CloudStorage`) are
/// skipped by default for two reasons: nothing inside can be reclaimed on
/// this disk (evicted files have zero blocks), and every `opendir` there
/// round-trips through the cloud daemon, which can stall the whole scan for
/// minutes. Scanning them explicitly (pass the cloud dir as ROOT) still
/// works — the skip only applies when they are not the requested root.
pub fn default_skip_paths(root: &Path, archive_dir: &Path) -> Vec<PathBuf> {
    let mut skip = vec![archive_dir.to_path_buf()];
    if root == Path::new("/") {
        // `/System/Volumes` is the firmlinked data volume — walking it would
        // count every byte twice (the same content is already reachable
        // through firmlinked top-level dirs like /Users).
        for p in ["/dev", "/proc", "/sys", "/System/Volumes"] {
            skip.push(PathBuf::from(p));
        }
    }
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = home {
        let cloud = [
            home.join("Library/Mobile Documents"),
            home.join("Library/CloudStorage"),
        ];
        let root_is_inside_cloud = cloud.iter().any(|c| root.starts_with(c));
        if !root_is_inside_cloud {
            skip.extend(cloud);
        }
    }
    skip
}

struct Ctx<'a> {
    arena: &'a Mutex<&'a mut FileArena>,
    seen: &'a Mutex<HashSet<(u64, u64)>>,
    progress: &'a ScanProgress,
    skip: &'a [PathBuf],
    dedup: bool,
    root_is_home: bool,
}

struct Entry {
    name: std::ffi::OsString,
    kind: NodeKind,
    size: Bytes,
    physical: Bytes,
    mtime: UnixTime,
    dev: u64,
    ino: u64,
}

struct SubDir {
    name: std::ffi::OsString,
    path: PathBuf,
    size: Bytes,
    physical: Bytes,
    mtime: UnixTime,
}

/// Walk `opts.root` and return the fully aggregated arena.
pub fn run_scan(
    opts: &ScanOptions,
    progress: &ScanProgress,
) -> Result<(FileArena, ScanStats), MetlaError> {
    let start = Instant::now();
    let root_path =
        fs::canonicalize(&opts.root).map_err(|e| MetlaError::ScanRoot(opts.root.clone(), e))?;
    let root_meta =
        fs::symlink_metadata(&root_path).map_err(|e| MetlaError::ScanRoot(root_path.clone(), e))?;
    if !root_meta.is_dir() {
        return Err(MetlaError::ScanRoot(
            root_path,
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "not a directory"),
        ));
    }

    let mut arena = FileArena::with_capacity(1 << 14);
    let root = arena.push(
        NodeId::NONE,
        &RawNode {
            name: b"",
            kind: NodeKind::Dir,
            flags: 0,
            junk: 0,
            size: Bytes(0),
            physical: Bytes(root_meta.blocks() * 512),
            mtime: mtime_of(&root_meta),
        },
    );
    arena.set_root(root);

    let home = std::env::var_os("HOME").map(PathBuf::from);
    // The walker compares against canonicalized directory paths, so the
    // skip list must live in the same address space (resolves /var symlinks
    // and friends); non-existent entries keep their raw path.
    let skip: Vec<PathBuf> = opts
        .skip_paths
        .iter()
        .map(|p| fs::canonicalize(p).unwrap_or_else(|_| p.clone()))
        .collect();
    let ctx = Ctx {
        arena: &Mutex::new(&mut arena),
        seen: &Mutex::new(HashSet::with_capacity(1 << 16)),
        progress,
        skip: &skip,
        dedup: opts.dedup_hardlinks,
        root_is_home: home.as_deref().is_some_and(|h| h == root_path.as_os_str()),
    };

    rayon::scope(|s| visit(s, &ctx, &root_path, root, 0, false));

    if progress.stop.load(Ordering::Relaxed) {
        return Err(MetlaError::Aborted);
    }
    if arena.overflowed() {
        return Err(MetlaError::Overflow);
    }
    // The mutex borrow ends with `ctx`; the aggregate pass runs single-threaded.
    arena.aggregate();

    let stats = ScanStats {
        files: progress.files.load(Ordering::Relaxed),
        dirs: progress.dirs.load(Ordering::Relaxed),
        errors: progress.errors.load(Ordering::Relaxed),
        bytes: Bytes(progress.bytes.load(Ordering::Relaxed)),
        elapsed: start.elapsed(),
    };
    Ok((arena, stats))
}

fn visit<'a>(
    s: &rayon::Scope<'a>,
    ctx: &'a Ctx<'a>,
    dir: &Path,
    dir_id: NodeId,
    depth: u16,
    under_developer: bool,
) {
    if ctx.progress.stop.load(Ordering::Relaxed) {
        return;
    }
    if depth > MAX_DEPTH || ctx.skip.iter().any(|p| p == dir) {
        return;
    }
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => {
            ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let mut batch: Vec<Entry> = Vec::with_capacity(64);
    let mut subdirs: Vec<SubDir> = Vec::with_capacity(8);
    let mut sibling_cargo = false;
    let mut sibling_podfile = false;
    let mut sibling_python = false;

    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let name = entry.file_name();
        let bytes = name.as_bytes();
        match bytes {
            b"Cargo.toml" | b"Cargo.lock" => sibling_cargo = true,
            b"Podfile" => sibling_podfile = true,
            b"pyproject.toml" | b"requirements.txt" => sibling_python = true,
            _ => {}
        }
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => {
                ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let md = match entry.metadata() {
            Ok(md) => md,
            Err(_) => {
                ctx.progress.errors.fetch_add(1, Ordering::Relaxed);
                continue;
            }
        };
        let mtime = mtime_of(&md);
        if ft.is_dir() {
            subdirs.push(SubDir {
                path: dir.join(&name),
                name,
                size: Bytes(md.len()),
                physical: Bytes(md.blocks() * 512),
                mtime,
            });
        } else {
            let kind = if ft.is_file() {
                NodeKind::File
            } else if ft.is_symlink() {
                NodeKind::Symlink
            } else {
                NodeKind::Other
            };
            batch.push(Entry {
                name,
                kind,
                size: Bytes(md.len()),
                physical: Bytes(md.blocks() * 512),
                mtime,
                dev: md.dev(),
                ino: md.ino(),
            });
        }
    }

    // Sibling context is complete now — classify the subdirectories.
    let is_home_child = depth == 0 && ctx.root_is_home;
    let junk_codes: Vec<u8> = subdirs
        .iter()
        .map(|sd| {
            junk::classify(&junk::DirCtx {
                name: sd.name.as_bytes(),
                sibling_cargo,
                sibling_podfile,
                sibling_python,
                under_developer,
                is_home_child,
            })
            .map_or(0, JunkCategory::as_code)
        })
        .collect();

    let mut subdir_ids: Vec<NodeId> = Vec::with_capacity(subdirs.len());
    {
        let mut guard = ctx.arena.lock().expect("arena lock");
        let arena = &mut guard;
        let mut seen = ctx.seen.lock().expect("hardlink lock");
        for e in &batch {
            let mut fl = 0;
            if ctx.dedup && e.kind == NodeKind::File && !seen.insert((e.dev, e.ino)) {
                fl |= flags::HARDLINK;
            }
            let id = arena.push(
                dir_id,
                &RawNode {
                    name: e.name.as_bytes(),
                    kind: e.kind,
                    flags: fl,
                    junk: 0,
                    size: e.size,
                    physical: e.physical,
                    mtime: e.mtime,
                },
            );
            if !id.is_some() {
                return;
            }
        }
        for (sd, code) in subdirs.iter().zip(junk_codes) {
            let id = arena.push(
                dir_id,
                &RawNode {
                    name: sd.name.as_bytes(),
                    kind: NodeKind::Dir,
                    flags: 0,
                    junk: code,
                    size: sd.size,
                    physical: sd.physical,
                    mtime: sd.mtime,
                },
            );
            if !id.is_some() {
                return;
            }
            subdir_ids.push(id);
        }
    }

    let mut files = 0u64;
    let mut bytes = 0u64;
    for e in &batch {
        if e.kind == NodeKind::File {
            files += 1;
        }
        bytes += e.size.0;
    }
    ctx.progress.files.fetch_add(files, Ordering::Relaxed);
    ctx.progress
        .dirs
        .fetch_add(subdirs.len() as u64, Ordering::Relaxed);
    ctx.progress.bytes.fetch_add(bytes, Ordering::Relaxed);

    for (sd, id) in subdirs.into_iter().zip(subdir_ids) {
        let child_dev = sd.name == std::ffi::OsStr::new("Developer");
        s.spawn(move |s| {
            visit(
                s,
                ctx,
                &sd.path,
                id,
                depth + 1,
                under_developer || child_dev,
            )
        });
    }
}

fn mtime_of(md: &fs::Metadata) -> UnixTime {
    UnixTime(
        md.modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs() as i64),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;

    #[test]
    fn default_skips_cover_firmlinks_and_cloud() {
        let archive = Path::new("/tmp/metla-archive-test");
        // plain root: only the archive dir
        let skip = default_skip_paths(Path::new("/tmp"), archive);
        assert_eq!(skip.len(), 3); // archive + 2 cloud trees

        // scanning /: pseudo-filesystems + firmlinked data volume added
        let skip = default_skip_paths(Path::new("/"), archive);
        assert!(skip.contains(&PathBuf::from("/System/Volumes")));
        assert!(skip.contains(&PathBuf::from("/dev")));

        // explicitly scanning a cloud tree: it must NOT be skipped
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap();
        let mobile = home.join("Library/Mobile Documents/sub");
        let skip = default_skip_paths(&mobile, archive);
        assert!(!skip.iter().any(|p| p.ends_with("Mobile Documents")));
        assert!(!skip.iter().any(|p| p.ends_with("CloudStorage")));
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let p = std::env::temp_dir().join(format!("metla-{}-{}", tag, std::process::id()));
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

    fn write(p: &Path, size: usize) {
        let mut f = File::create(p).unwrap();
        f.write_all(&vec![b'x'; size]).unwrap();
    }

    #[test]
    fn scan_measures_sizes_and_counts() {
        let tmp = TempDir::new("scan");
        let root = tmp.0.clone();
        fs::create_dir_all(root.join("a/b")).unwrap();
        write(&root.join("a/f1"), 100);
        write(&root.join("a/b/f2"), 200);
        write(&root.join("top"), 50);

        let progress = ScanProgress::new();
        let (arena, stats) = run_scan(
            &ScanOptions {
                root,
                dedup_hardlinks: true,
                skip_paths: vec![],
            },
            &progress,
        )
        .unwrap();

        assert_eq!(stats.files, 3);
        assert_eq!(stats.dirs, 2);
        assert_eq!(stats.errors, 0);
        let root_id = arena.root();
        assert_eq!(arena.node(root_id).subtree_size, Bytes(350));
        assert_eq!(arena.node(root_id).subtree_files, 3);
        assert_eq!(arena.node(root_id).subtree_dirs, 3);
    }

    #[test]
    fn scan_dedups_hardlinks() {
        let tmp = TempDir::new("hard");
        let root = tmp.0.clone();
        write(&root.join("orig"), 1000);
        fs::hard_link(root.join("orig"), root.join("link")).unwrap();

        let progress = ScanProgress::new();
        let (arena, _) = run_scan(
            &ScanOptions {
                root,
                dedup_hardlinks: true,
                skip_paths: vec![],
            },
            &progress,
        )
        .unwrap();
        assert_eq!(arena.node(arena.root()).subtree_size, Bytes(1000));
    }

    #[test]
    fn scan_skips_listed_paths() {
        let tmp = TempDir::new("skip");
        let root = tmp.0.clone();
        fs::create_dir_all(root.join("junk")).unwrap();
        write(&root.join("junk/blob"), 500);
        write(&root.join("keep"), 10);

        let progress = ScanProgress::new();
        let (arena, _) = run_scan(
            &ScanOptions {
                root,
                dedup_hardlinks: true,
                skip_paths: vec![tmp.0.join("junk")],
            },
            &progress,
        )
        .unwrap();
        assert_eq!(arena.node(arena.root()).subtree_size, Bytes(10));
    }

    #[test]
    fn symlinks_are_not_followed() {
        let tmp = TempDir::new("sym");
        let root = tmp.0.clone();
        let outside = tmp.0.join("../metla-sym-outside-target");
        write(&outside, 4096);
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        let progress = ScanProgress::new();
        let (arena, stats) = run_scan(
            &ScanOptions {
                root,
                dedup_hardlinks: true,
                skip_paths: vec![],
            },
            &progress,
        )
        .unwrap();
        // Only the link itself was indexed (size = target path length), the
        // 4 KB target was never entered and does not count as a file.
        assert_eq!(stats.files, 0);
        assert_eq!(arena.len(), 2);
        let root_id = arena.root();
        assert_eq!(arena.node(root_id).subtree_files, 0);
        assert!(arena.node(root_id).subtree_size.0 < 1000);
        let _ = fs::remove_file(outside);
    }
}

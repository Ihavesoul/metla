//! Flat, cache-friendly file tree: one `Vec<Node>`, one interned name pool,
//! `u32` links. No `Rc`, no `Box` per directory, no per-file heap allocation
//! after the pool append. Deleting a subtree tombstones it in place (`FREED`
//! flag) and decrementally updates every ancestor's aggregates.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use crate::types::{flags, Bytes, NodeId, NodeKind, UnixTime};

/// `(offset, len)` into the arena's byte pool.
#[derive(Copy, Clone, Default, Debug)]
pub struct NameRef {
    pub off: u32,
    pub len: u32,
}

/// One file-system entry. ~72 bytes; a million files ≈ 70 MB.
#[derive(Copy, Clone, Debug)]
pub struct Node {
    pub parent: NodeId,
    pub first_child: NodeId,
    pub next_sibling: NodeId,
    pub name: NameRef,
    pub kind: NodeKind,
    pub flags: u8,
    /// [`crate::junk::JunkCategory`] code assigned during the walk, 0 = none.
    pub junk: u8,
    /// Own apparent size (file length; symlink target length for links).
    pub size: Bytes,
    /// Own physical size (`st_blocks * 512`).
    pub physical: Bytes,
    pub mtime: UnixTime,
    // Aggregates over the whole subtree (filled by [`FileArena::aggregate`],
    // kept consistent by [`FileArena::remove_subtree`]).
    pub subtree_size: Bytes,
    pub subtree_physical: Bytes,
    pub subtree_files: u64,
    pub subtree_dirs: u64,
}

/// Everything [`FileArena::push`] needs; built by the walker in batches.
pub struct RawNode<'a> {
    pub name: &'a [u8],
    pub kind: NodeKind,
    pub flags: u8,
    pub junk: u8,
    pub size: Bytes,
    pub physical: Bytes,
    pub mtime: UnixTime,
}

pub struct FileArena {
    nodes: Vec<Node>,
    names: Vec<u8>,
    root: NodeId,
    overflowed: bool,
}

impl Default for FileArena {
    fn default() -> Self {
        Self::new()
    }
}

impl FileArena {
    pub fn new() -> Self {
        Self::with_capacity(1 << 12)
    }

    pub fn with_capacity(nodes_hint: usize) -> Self {
        FileArena {
            nodes: Vec::with_capacity(nodes_hint),
            names: Vec::with_capacity(1 << 16),
            root: NodeId::NONE,
            overflowed: false,
        }
    }

    /// Append a node and link it as the newest child of `parent`
    /// (`NodeId::NONE` parent = the root). Returns `NodeId::NONE` on overflow.
    pub fn push(&mut self, parent: NodeId, raw: &RawNode) -> NodeId {
        if self.nodes.len() >= u32::MAX as usize - 1 {
            self.overflowed = true;
            return NodeId::NONE;
        }
        let off = self.names.len() as u32;
        self.names.extend_from_slice(raw.name);
        let id = NodeId(self.nodes.len() as u32);
        self.nodes.push(Node {
            parent,
            first_child: NodeId::NONE,
            next_sibling: NodeId::NONE,
            name: NameRef {
                off,
                len: raw.name.len() as u32,
            },
            kind: raw.kind,
            flags: raw.flags,
            junk: raw.junk,
            size: raw.size,
            physical: raw.physical,
            mtime: raw.mtime,
            subtree_size: Bytes(0),
            subtree_physical: Bytes(0),
            subtree_files: 0,
            subtree_dirs: 0,
        });
        if parent.is_some() {
            let head = self.nodes[parent.index()].first_child;
            self.nodes[id.index()].next_sibling = head;
            self.nodes[parent.index()].first_child = id;
        }
        id
    }

    #[inline]
    pub fn node(&self, id: NodeId) -> &Node {
        &self.nodes[id.index()]
    }

    #[inline]
    pub fn root(&self) -> NodeId {
        self.root
    }

    pub(crate) fn set_root(&mut self, id: NodeId) {
        self.root = id;
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    pub fn overflowed(&self) -> bool {
        self.overflowed
    }

    #[inline]
    pub fn name(&self, id: NodeId) -> &OsStr {
        let n = &self.nodes[id.index()].name;
        OsStr::from_bytes(&self.names[n.off as usize..(n.off + n.len) as usize])
    }

    #[inline]
    pub fn is_freed(&self, id: NodeId) -> bool {
        self.node(id).flags & flags::FREED != 0
    }

    #[inline]
    pub fn is_hardlink(&self, id: NodeId) -> bool {
        self.node(id).flags & flags::HARDLINK != 0
    }

    #[inline]
    pub fn parent(&self, id: NodeId) -> NodeId {
        self.node(id).parent
    }

    /// Depth from the root (root itself = 0).
    pub fn depth(&self, id: NodeId) -> u32 {
        let mut d = 0;
        let mut cur = self.node(id).parent;
        while cur.is_some() {
            d += 1;
            cur = self.node(cur).parent;
        }
        d
    }

    /// Live (non-freed) children, newest first — sort before display.
    pub fn children(&self, id: NodeId) -> Children<'_> {
        Children {
            arena: self,
            next: self.nodes[id.index()].first_child,
        }
    }

    /// Collect live children into a caller-owned scratch buffer (reused every
    /// frame — this is the allocation-free hot path).
    pub fn collect_children(&self, id: NodeId, out: &mut Vec<NodeId>) {
        out.clear();
        out.extend(self.children(id));
    }

    /// Build the absolute path of `id` into `out`, using `scratch` (reused)
    /// for the name chain. No allocation after warm-up.
    pub fn build_path(
        &self,
        id: NodeId,
        prefix: &Path,
        scratch: &mut Vec<NameRef>,
        out: &mut PathBuf,
    ) {
        out.clear();
        out.push(prefix);
        scratch.clear();
        let mut cur = id;
        while cur.is_some() && cur != self.root {
            let n = &self.nodes[cur.index()];
            scratch.push(n.name);
            cur = n.parent;
        }
        for nm in scratch.iter().rev() {
            out.push(OsStr::from_bytes(
                &self.names[nm.off as usize..(nm.off + nm.len) as usize],
            ));
        }
    }

    /// Bottom-up size aggregation, iterative (no recursion, no stack
    /// overflow on deep trees). O(nodes).
    pub fn aggregate(&mut self) {
        #[derive(Copy, Clone)]
        struct Frame {
            node: NodeId,
            child: NodeId,
        }
        let root = self.root;
        {
            let r = &mut self.nodes[root.index()];
            r.subtree_size = Bytes(0);
            r.subtree_physical = Bytes(0);
            r.subtree_files = u64::from(r.kind == NodeKind::File);
            r.subtree_dirs = u64::from(r.kind == NodeKind::Dir);
        }
        let mut stack: Vec<Frame> = Vec::with_capacity(128);
        stack.push(Frame {
            node: root,
            child: self.nodes[root.index()].first_child,
        });
        while let Some(top) = stack.last_mut() {
            let c = top.child;
            if c.is_some() {
                top.child = self.nodes[c.index()].next_sibling;
                let n = &mut self.nodes[c.index()];
                // directories contribute only their content — the stub size
                // of the directory entry itself is noise for a sweeper
                let is_dir = n.kind == NodeKind::Dir;
                n.subtree_size = if is_dir { Bytes(0) } else { n.size };
                n.subtree_physical = if is_dir { Bytes(0) } else { n.physical };
                n.subtree_files = u64::from(n.kind == NodeKind::File);
                n.subtree_dirs = u64::from(n.kind == NodeKind::Dir);
                stack.push(Frame {
                    node: c,
                    child: n.first_child,
                });
            } else {
                let done = *stack.last().expect("frame present");
                stack.pop();
                if let Some(p) = stack.last_mut() {
                    let (sz, ph, fl, dr, skip) = {
                        let x = &self.nodes[done.node.index()];
                        (
                            x.subtree_size,
                            x.subtree_physical,
                            x.subtree_files,
                            x.subtree_dirs,
                            x.kind == NodeKind::File && x.flags & flags::HARDLINK != 0,
                        )
                    };
                    let pn = &mut self.nodes[p.node.index()];
                    if !skip {
                        pn.subtree_size.0 += sz.0;
                        pn.subtree_physical.0 += ph.0;
                    }
                    pn.subtree_files += fl;
                    pn.subtree_dirs += dr;
                }
            }
        }
    }

    /// Tombstone a subtree: mark every node `FREED`, unlink it from its
    /// parent and decrement all ancestors' aggregates. O(depth + subtree).
    pub fn remove_subtree(&mut self, id: NodeId) {
        let (sz, ph, fl, dr, skip) = {
            let n = &self.nodes[id.index()];
            (
                n.subtree_size,
                n.subtree_physical,
                n.subtree_files,
                n.subtree_dirs,
                n.kind == NodeKind::File && n.flags & flags::HARDLINK != 0,
            )
        };
        let mut p = self.nodes[id.index()].parent;
        while p.is_some() {
            let pn = &mut self.nodes[p.index()];
            if !skip {
                pn.subtree_size.0 -= sz.0;
                pn.subtree_physical.0 -= ph.0;
            }
            pn.subtree_files -= fl;
            pn.subtree_dirs -= dr;
            p = pn.parent;
        }
        let parent = self.nodes[id.index()].parent;
        if parent.is_some() {
            if self.nodes[parent.index()].first_child == id {
                self.nodes[parent.index()].first_child = self.nodes[id.index()].next_sibling;
            } else {
                let mut cur = self.nodes[parent.index()].first_child;
                while cur.is_some() && self.nodes[cur.index()].next_sibling != id {
                    cur = self.nodes[cur.index()].next_sibling;
                }
                if cur.is_some() {
                    self.nodes[cur.index()].next_sibling = self.nodes[id.index()].next_sibling;
                }
            }
        }
        let mut stack = vec![id];
        while let Some(c) = stack.pop() {
            self.nodes[c.index()].flags |= flags::FREED;
            let mut ch = self.nodes[c.index()].first_child;
            while ch.is_some() {
                stack.push(ch);
                ch = self.nodes[ch.index()].next_sibling;
            }
        }
    }
}

pub struct Children<'a> {
    arena: &'a FileArena,
    next: NodeId,
}

impl Iterator for Children<'_> {
    type Item = NodeId;

    fn next(&mut self) -> Option<NodeId> {
        // Tombstoned nodes are skipped so every consumer sees a live tree.
        while self.next.is_some() {
            let n = self.next;
            self.next = self.arena.nodes[n.index()].next_sibling;
            if self.arena.nodes[n.index()].flags & flags::FREED == 0 {
                return Some(n);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &[u8], size: u64) -> RawNode<'_> {
        RawNode {
            name,
            kind: NodeKind::File,
            flags: 0,
            junk: 0,
            size: Bytes(size),
            physical: Bytes(size / 8),
            mtime: UnixTime(0),
        }
    }

    fn dir(name: &[u8]) -> RawNode<'_> {
        RawNode {
            name,
            kind: NodeKind::Dir,
            flags: 0,
            junk: 0,
            size: Bytes(0),
            physical: Bytes(0),
            mtime: UnixTime(0),
        }
    }

    #[test]
    fn aggregate_and_paths() {
        let mut a = FileArena::new();
        let root = a.push(NodeId::NONE, &dir(b""));
        a.set_root(root);
        let work = a.push(root, &dir(b"Work"));
        let prj = a.push(work, &dir(b"prj"));
        a.push(prj, &file(b"main.rs", 1000));
        a.push(prj, &file(b"lib.rs", 2000));
        let readme = a.push(root, &file(b"README.md", 50));

        a.aggregate();

        assert_eq!(a.node(prj).subtree_size, Bytes(3000));
        assert_eq!(a.node(work).subtree_size, Bytes(3000));
        assert_eq!(a.node(root).subtree_size, Bytes(3050));
        assert_eq!(a.node(root).subtree_files, 3);
        assert_eq!(a.node(root).subtree_dirs, 3);

        let mut scratch = Vec::new();
        let mut path = PathBuf::new();
        let prefix = Path::new("/srv");
        a.build_path(prj, prefix, &mut scratch, &mut path);
        assert_eq!(path, Path::new("/srv/Work/prj"));
        a.build_path(readme, prefix, &mut scratch, &mut path);
        assert_eq!(path, Path::new("/srv/README.md"));
    }

    #[test]
    fn remove_subtree_updates_ancestors() {
        let mut a = FileArena::new();
        let root = a.push(NodeId::NONE, &dir(b""));
        a.set_root(root);
        let d1 = a.push(root, &dir(b"a"));
        a.push(d1, &file(b"x", 10));
        let d2 = a.push(root, &dir(b"b"));
        a.push(d2, &file(b"y", 20));
        a.aggregate();

        a.remove_subtree(d1);
        assert!(a.is_freed(d1));
        assert_eq!(a.node(root).subtree_size, Bytes(20));
        assert_eq!(a.node(root).subtree_files, 1);
        // only b survives as a live child
        let mut kids = Vec::new();
        a.collect_children(root, &mut kids);
        assert_eq!(kids, vec![d2]);
        // double-remove must not corrupt counters
        a.remove_subtree(d2);
        assert_eq!(a.node(root).subtree_size, Bytes(0));
        assert_eq!(a.node(root).subtree_files, 0);
    }

    #[test]
    fn hardlinks_are_not_double_counted() {
        let mut a = FileArena::new();
        let root = a.push(NodeId::NONE, &dir(b""));
        a.set_root(root);
        a.push(root, &file(b"one", 100));
        a.push(
            root,
            &RawNode {
                flags: flags::HARDLINK,
                ..file(b"two", 100)
            },
        );
        a.aggregate();
        assert_eq!(a.node(root).subtree_size, Bytes(100));
        assert_eq!(a.node(root).subtree_files, 2);
    }
}

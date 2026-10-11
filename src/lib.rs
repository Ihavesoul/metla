//! metla — a zero-nonsense disk sweeper.
//!
//! The library is the display-agnostic core: a flat arena-backed file tree,
//! a parallel indexer, a suggestion engine and action execution. It never
//! allocates per-file on hot paths: the whole tree is one `Vec<Node>` with
//! `u32` links, names live in a single interned byte pool, and every bulk
//! operation reuses caller-owned scratch buffers.

pub mod action;
pub mod archive;
pub mod arena;
pub mod format;
pub mod junk;
pub mod suggest;
pub mod types;
pub mod walk;

use std::path::PathBuf;

/// Every failure mode the core can produce. No panics on runtime data.
#[derive(Debug)]
pub enum MetlaError {
    /// Unrelated I/O failure (archive creation, extraction).
    Io(std::io::Error),
    /// The scan root itself could not be read or is not a directory.
    ScanRoot(PathBuf, std::io::Error),
    /// The tree exceeded `u32::MAX` nodes (never observed in practice).
    Overflow,
    /// Scan was asked to stop before finishing.
    Aborted,
    /// Archive step failed; the original files were left untouched.
    Archive(String),
}

impl std::fmt::Display for MetlaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MetlaError::Io(e) => write!(f, "io error: {e}"),
            MetlaError::ScanRoot(p, e) => write!(f, "cannot scan {}: {e}", p.display()),
            MetlaError::Overflow => write!(f, "too many entries (>4 billion)"),
            MetlaError::Aborted => write!(f, "scan aborted"),
            MetlaError::Archive(msg) => write!(f, "archive failed: {msg}"),
        }
    }
}

impl std::error::Error for MetlaError {}

impl From<std::io::Error> for MetlaError {
    fn from(e: std::io::Error) -> Self {
        MetlaError::Io(e)
    }
}

//! Streaming tar.zst archives. Items are appended one by one straight into
//! the zstd encoder — memory stays flat regardless of input size.

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::MetlaError;

pub struct ArchiveItem {
    pub absolute: PathBuf,
    /// Name the item gets inside the archive (usually its basename).
    pub entry_name: std::ffi::OsString,
}

/// Write `items` into a zstd-compressed tar at `dest`. Returns the archive
/// size in bytes. `should_continue` is polled before every item — returning
/// false aborts with [`MetlaError::Aborted`]. On any failure the partial
/// file is removed and the caller must assume the originals were untouched.
pub fn write_archive(
    items: &[ArchiveItem],
    dest: &Path,
    should_continue: &dyn Fn() -> bool,
) -> Result<u64, MetlaError> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::File::create(dest)?;
    let enc = zstd::Encoder::new(file, 3)?;
    let mut builder = tar::Builder::new(enc);
    builder.follow_symlinks(false);

    for item in items {
        if !should_continue() {
            drop(builder);
            let _ = fs::remove_file(dest);
            return Err(MetlaError::Aborted);
        }
        let res = append_item(&mut builder, &item.absolute, &item.entry_name);
        if let Err(e) = res {
            drop(builder);
            let _ = fs::remove_file(dest);
            return Err(MetlaError::Archive(format!(
                "{}: {e}",
                item.absolute.display()
            )));
        }
    }

    let enc = builder
        .into_inner()
        .map_err(|e| MetlaError::Archive(format!("finalize: {e}")))?;
    let file = enc
        .finish()
        .map_err(|e| MetlaError::Archive(format!("compress: {e}")))?;
    Ok(file.metadata()?.len())
}

fn append_item(
    builder: &mut tar::Builder<impl Write>,
    path: &Path,
    name: &OsStr,
) -> std::io::Result<()> {
    let md = fs::symlink_metadata(path)?;
    if md.is_dir() {
        builder.append_dir_all(name, path)
    } else {
        builder.append_path_with_name(path, name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Read;

    #[test]
    fn roundtrip() {
        let base = std::env::temp_dir().join(format!("metla-arch-{}", std::process::id()));
        let src = base.join("src");
        let dst = base.join("dst");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(src.join("inner")).unwrap();
        fs::write(src.join("a.txt"), b"hello").unwrap();
        fs::write(src.join("inner/b.txt"), b"world").unwrap();

        let dest = base.join("out.tar.zst");

        // cancellation before any item: nothing is written, dest is removed
        let stop_now = || false;
        let aborted = write_archive(
            &[ArchiveItem {
                absolute: src.clone(),
                entry_name: src.file_name().unwrap().to_os_string(),
            }],
            &dest,
            &stop_now,
        );
        assert!(matches!(aborted, Err(MetlaError::Aborted)));
        assert!(!dest.exists());

        let keep_going = || true;
        let written = write_archive(
            &[ArchiveItem {
                absolute: src.clone(),
                entry_name: src.file_name().unwrap().to_os_string(),
            }],
            &dest,
            &keep_going,
        )
        .unwrap();
        assert!(written > 0);

        let f = File::open(&dest).unwrap();
        let dec = zstd::Decoder::new(f).unwrap();
        tar::Archive::new(dec).unpack(&dst).unwrap();

        assert_eq!(fs::read(dst.join("src/a.txt")).unwrap(), b"hello");
        assert_eq!(fs::read(dst.join("src/inner/b.txt")).unwrap(), b"world");

        let mut probe = Vec::new();
        File::open(&dest).unwrap().read_to_end(&mut probe).unwrap();
        // zstd magic
        assert_eq!(&probe[..4], &[0x28, 0xB5, 0x2F, 0xFD]);

        let _ = fs::remove_dir_all(&base);
    }
}

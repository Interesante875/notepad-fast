//! The immutable "original" buffer of a document: a memory-mapped file
//! (or an owned buffer for new/empty documents) plus its newline index.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use memmap2::Mmap;

use super::index::LineIndex;

pub enum Bytes {
    Mapped(MappedFile),
    Owned(Vec<u8>),
}

/// A read-only file mapping that knows where its file currently lives.
pub struct MappedFile {
    map: Option<Mmap>,
    /// (current path, delete the file when the mapping is dropped)
    location: Mutex<(PathBuf, bool)>,
}

impl Drop for MappedFile {
    fn drop(&mut self) {
        // Unmap first: Windows cannot delete a file that is still mapped.
        self.map.take();
        let (path, delete) = std::mem::take(&mut *self.location.lock().unwrap_or_else(|e| e.into_inner()));
        if delete {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl Bytes {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Bytes::Mapped(m) => m.map.as_deref().unwrap_or_default(),
            Bytes::Owned(v) => v,
        }
    }

    /// Whether this is a mapping of the file currently at `path`.
    pub fn maps(&self, path: &Path) -> bool {
        match self {
            Bytes::Mapped(m) => {
                let loc = m.location.lock().unwrap_or_else(|e| e.into_inner());
                same_file(&loc.0, path)
            }
            Bytes::Owned(_) => false,
        }
    }

    /// Record that the mapped file was renamed to `path`, and whether it
    /// should be deleted once the mapping is dropped.
    pub fn set_location(&self, path: PathBuf, delete_on_drop: bool) {
        if let Bytes::Mapped(m) = self {
            *m.location.lock().unwrap_or_else(|e| e.into_inner()) = (path, delete_on_drop);
        }
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Maps a file read-only. Zero-length files cannot be mapped on every
/// platform, so they become an empty owned buffer.
pub fn map_file(path: &Path) -> io::Result<Arc<Bytes>> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(Arc::new(Bytes::Owned(Vec::new())));
    }
    // SAFETY: the map is read-only. If another process truncates the file
    // while it is open, reads may fault; this is the same trade-off every
    // mmap-based large-file viewer makes in exchange for instant opening.
    let map = unsafe { Mmap::map(&file)? };
    Ok(Arc::new(Bytes::Mapped(MappedFile { map: Some(map), location: Mutex::new((path.to_path_buf(), false)) })))
}

pub struct Source {
    pub bytes: Arc<Bytes>,
    pub index: LineIndex,
}

impl Source {
    pub fn empty() -> Self {
        Self::from_vec(Vec::new())
    }

    pub fn from_vec(v: Vec<u8>) -> Self {
        let index = LineIndex::build(&v);
        Self { bytes: Arc::new(Bytes::Owned(v)), index }
    }

    pub fn data(&self) -> &[u8] {
        self.bytes.as_slice()
    }

    pub fn len(&self) -> u64 {
        self.data().len() as u64
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn newlines_in(&self, start: u64, end: u64) -> u64 {
        let d = self.data();
        self.index.newlines_before(d, end) - self.index.newlines_before(d, start)
    }
}

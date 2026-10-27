//! The immutable "original" buffer of a document: a memory-mapped file
//! (or an owned buffer for new/empty documents) plus its newline index.

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memmap2::Mmap;

use super::index::LineIndex;

pub enum Bytes {
    Mapped(Mmap),
    Owned(Vec<u8>),
}

impl Bytes {
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Bytes::Mapped(m) => m,
            Bytes::Owned(v) => v,
        }
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
    Ok(Arc::new(Bytes::Mapped(map)))
}

pub struct Source {
    pub bytes: Arc<Bytes>,
    pub index: LineIndex,
    /// File the bytes were mapped from, if any.
    pub path: Option<PathBuf>,
}

impl Source {
    pub fn empty() -> Self {
        Self::from_vec(Vec::new())
    }

    pub fn from_vec(v: Vec<u8>) -> Self {
        let index = LineIndex::build(&v);
        Self { bytes: Arc::new(Bytes::Owned(v)), index, path: None }
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

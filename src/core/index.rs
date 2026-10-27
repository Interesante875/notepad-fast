//! Sparse newline index over an immutable byte buffer.
//!
//! Instead of storing the offset of every line (8 bytes per line, which is
//! gigabytes for a big log file), we store one checkpoint every
//! [`CHECKPOINT_BYTES`] bytes recording how many newlines precede it. Any
//! query then scans at most one checkpoint interval with SIMD `memchr`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Distance between checkpoints. 64 KiB keeps the index tiny
/// (~2.5 MB for a 10 GB file) while bounding every lookup scan.
pub const CHECKPOINT_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Default)]
pub struct LineIndex {
    /// `checkpoints[i]` = number of `\n` bytes in `data[..i * CHECKPOINT_BYTES]`.
    checkpoints: Vec<u64>,
    total_newlines: u64,
    len: u64,
    crlf: bool,
}

/// Incrementally builds a [`LineIndex`] from consecutive chunks of bytes.
#[derive(Debug, Default)]
pub struct IndexBuilder {
    checkpoints: Vec<u64>,
    count: u64,
    pos: u64,
    first_newline_crlf: Option<bool>,
    prev_byte: u8,
}

impl IndexBuilder {
    pub fn new() -> Self {
        Self { checkpoints: vec![0], ..Default::default() }
    }

    pub fn feed(&mut self, mut chunk: &[u8]) {
        while !chunk.is_empty() {
            let next_cp = (self.pos / CHECKPOINT_BYTES + 1) * CHECKPOINT_BYTES;
            let take = ((next_cp - self.pos) as usize).min(chunk.len());
            let (part, rest) = chunk.split_at(take);
            if self.first_newline_crlf.is_none() {
                if let Some(i) = memchr::memchr(b'\n', part) {
                    let before = if i > 0 { part[i - 1] } else { self.prev_byte };
                    self.first_newline_crlf = Some(before == b'\r');
                }
            }
            self.count += memchr::memchr_iter(b'\n', part).count() as u64;
            self.pos += take as u64;
            if let Some(&b) = part.last() {
                self.prev_byte = b;
            }
            if self.pos == next_cp {
                self.checkpoints.push(self.count);
            }
            chunk = rest;
        }
    }

    pub fn finish(self) -> LineIndex {
        LineIndex {
            checkpoints: self.checkpoints,
            total_newlines: self.count,
            len: self.pos,
            crlf: self.first_newline_crlf.unwrap_or(cfg!(windows)),
        }
    }
}

impl LineIndex {
    /// Index `data` in one go.
    pub fn build(data: &[u8]) -> Self {
        Self::build_with_progress(data, &AtomicU64::new(0), &AtomicBool::new(false)).expect("not cancelled")
    }

    /// Index `data`, publishing the number of processed bytes to `progress`.
    /// Returns `None` if `cancel` is set while indexing.
    pub fn build_with_progress(data: &[u8], progress: &AtomicU64, cancel: &AtomicBool) -> Option<Self> {
        const BLOCK: usize = 8 * 1024 * 1024;
        let mut b = IndexBuilder::new();
        for block in data.chunks(BLOCK) {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            b.feed(block);
            progress.fetch_add(block.len() as u64, Ordering::Relaxed);
        }
        Some(b.finish())
    }

    pub fn len(&self) -> u64 {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn total_newlines(&self) -> u64 {
        self.total_newlines
    }

    /// Whether the first line ending in the data is `\r\n`.
    pub fn uses_crlf(&self) -> bool {
        self.crlf
    }

    /// Number of `\n` bytes in `data[..off]`.
    pub fn newlines_before(&self, data: &[u8], off: u64) -> u64 {
        let off = off.min(self.len);
        let cp = (off / CHECKPOINT_BYTES) as usize;
        let cp = cp.min(self.checkpoints.len() - 1);
        let base = cp as u64 * CHECKPOINT_BYTES;
        self.checkpoints[cp] + memchr::memchr_iter(b'\n', &data[base as usize..off as usize]).count() as u64
    }

    /// Offset just past the `n`-th newline (1-based). `n == 0` returns 0.
    /// Returns `None` if the data has fewer than `n` newlines.
    pub fn pos_after_nth_newline(&self, data: &[u8], n: u64) -> Option<u64> {
        if n == 0 {
            return Some(0);
        }
        if n > self.total_newlines {
            return None;
        }
        // Last checkpoint that has strictly fewer than `n` newlines before it;
        // the n-th newline then lies before the following checkpoint.
        let cp = self.checkpoints.partition_point(|&c| c < n) - 1;
        let base = cp as u64 * CHECKPOINT_BYTES;
        let need = (n - self.checkpoints[cp]) as usize;
        let rel = memchr::memchr_iter(b'\n', &data[base as usize..]).nth(need - 1)?;
        Some(base + rel as u64 + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn naive_before(data: &[u8], off: usize) -> u64 {
        data[..off].iter().filter(|&&b| b == b'\n').count() as u64
    }

    #[test]
    fn matches_naive_counts() {
        let mut data = Vec::new();
        for i in 0..50_000u32 {
            data.extend_from_slice(format!("line {i} {}\n", "x".repeat((i % 37) as usize)).as_bytes());
        }
        let idx = LineIndex::build(&data);
        assert_eq!(idx.total_newlines(), 50_000);
        for off in [0usize, 1, 100, 65_535, 65_536, 65_537, 200_000, data.len()] {
            assert_eq!(idx.newlines_before(&data, off as u64), naive_before(&data, off));
        }
        let mut expected = 0;
        for n in 0..=50_000u64 {
            let p = idx.pos_after_nth_newline(&data, n).unwrap();
            assert_eq!(p, expected, "n={n}");
            if n < 50_000 {
                expected = (memchr::memchr(b'\n', &data[p as usize..]).unwrap() as u64) + p + 1;
            }
        }
        assert!(idx.pos_after_nth_newline(&data, 50_001).is_none());
    }

    #[test]
    fn incremental_equals_bulk() {
        let data: Vec<u8> = (0..300_000u32).map(|i| if i % 7 == 0 { b'\n' } else { b'a' }).collect();
        let bulk = LineIndex::build(&data);
        let mut b = IndexBuilder::new();
        for c in data.chunks(1234) {
            b.feed(c);
        }
        let inc = b.finish();
        assert_eq!(bulk.checkpoints, inc.checkpoints);
        assert_eq!(bulk.total_newlines, inc.total_newlines);
    }

    #[test]
    fn detects_crlf() {
        assert!(LineIndex::build(b"a\r\nb\r\n").uses_crlf());
        assert!(!LineIndex::build(b"a\nb\r\n").uses_crlf());
        let mut b = IndexBuilder::new();
        b.feed(b"abc\r");
        b.feed(b"\nxyz");
        assert!(b.finish().uses_crlf());
    }
}

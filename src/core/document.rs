//! A piece-table document over a memory-mapped original buffer.
//!
//! Opening a file never copies it: the document starts as one piece that
//! points at the mapped file. Edits only append to a small "add" buffer and
//! split pieces, so inserting a character into a 20 GB log is as cheap as
//! inserting it into a 20 byte note. Every piece caches its newline count,
//! which together with the sparse [`LineIndex`] gives fast line lookups.

use std::borrow::Cow;
use std::fs::{self, File};
use std::io::{self, BufWriter, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use memchr::memmem;

use super::index::{IndexBuilder, LineIndex, CHECKPOINT_BYTES};
use super::source::{map_file, Bytes, Source};

/// (anchor, cursor) byte offsets. Stored with undo groups so undo/redo can
/// restore the selection.
pub type Sel = (u64, u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Piece {
    add: bool,
    start: u64,
    len: u64,
    newlines: u64,
}

#[derive(Debug, Clone)]
struct EditRecord {
    at: usize,
    removed: Vec<Piece>,
    inserted: Vec<Piece>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditKind {
    /// Single printable characters; consecutive typing merges into one undo step.
    Typing,
    Other,
}

#[derive(Debug, Clone)]
struct UndoGroup {
    id: u64,
    kind: EditKind,
    records: Vec<EditRecord>,
    sel_before: Sel,
    sel_after: Sel,
}

/// Result of one bounded step of an incremental search.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindStep {
    Found(u64),
    NotFound,
    /// Budget exhausted; continue from this cursor.
    Pending(u64),
}

/// Maximum number of matches `replace_all` will process in one go.
pub const MAX_REPLACE_ALL: usize = 10_000_000;

pub struct Document {
    orig: Arc<Source>,
    add: Vec<u8>,
    pieces: Vec<Piece>,
    byte_prefix: Vec<u64>,
    nl_prefix: Vec<u64>,
    undo: Vec<UndoGroup>,
    redo: Vec<UndoGroup>,
    next_group_id: u64,
    saved_group_id: u64,
    seal_typing: bool,
    version: u64,
}

impl Default for Document {
    fn default() -> Self {
        Self::from_source(Source::empty())
    }
}

/// Map and index a file, reporting progress. `Ok(None)` means cancelled.
pub fn load_source(path: &Path, progress: &AtomicU64, cancel: &AtomicBool) -> io::Result<Option<Source>> {
    let bytes = map_file(path)?;
    match LineIndex::build_with_progress(bytes.as_slice(), progress, cancel) {
        Some(index) => Ok(Some(Source { bytes, index, path: Some(path.to_path_buf()) })),
        None => Ok(None),
    }
}

impl Document {
    pub fn from_source(src: Source) -> Self {
        let mut d = Document {
            orig: Arc::new(src),
            add: Vec::new(),
            pieces: Vec::new(),
            byte_prefix: vec![0],
            nl_prefix: vec![0],
            undo: Vec::new(),
            redo: Vec::new(),
            next_group_id: 1,
            saved_group_id: 0,
            seal_typing: false,
            version: 0,
        };
        d.reset_to_orig();
        d
    }

    pub fn from_bytes(v: impl Into<Vec<u8>>) -> Self {
        Self::from_source(Source::from_vec(v.into()))
    }

    pub fn open(path: &Path) -> io::Result<Self> {
        let src = load_source(path, &AtomicU64::new(0), &AtomicBool::new(false))?.expect("not cancelled");
        Ok(Self::from_source(src))
    }

    fn reset_to_orig(&mut self) {
        self.pieces.clear();
        let len = self.orig.len();
        if len > 0 {
            self.pieces.push(Piece { add: false, start: 0, len, newlines: self.orig.index.total_newlines() });
        }
        self.rebuild_prefix(0);
        self.version += 1;
    }

    // ---------------------------------------------------------------- queries

    pub fn len(&self) -> u64 {
        *self.byte_prefix.last().unwrap()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn total_newlines(&self) -> u64 {
        *self.nl_prefix.last().unwrap()
    }

    pub fn line_count(&self) -> u64 {
        self.total_newlines() + 1
    }

    /// Whether the original file used CRLF line endings.
    pub fn uses_crlf(&self) -> bool {
        self.orig.index.uses_crlf()
    }

    /// Increments on every modification; used to invalidate view caches.
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn is_modified(&self) -> bool {
        self.undo.last().map_or(0, |g| g.id) != self.saved_group_id
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    pub fn piece_count(&self) -> usize {
        self.pieces.len()
    }

    fn piece_bytes(&self, p: &Piece) -> &[u8] {
        let r = p.start as usize..(p.start + p.len) as usize;
        if p.add {
            &self.add[r]
        } else {
            &self.orig.data()[r]
        }
    }

    /// Piece index and offset inside it. `pos == len` yields `(pieces.len(), 0)`.
    fn locate(&self, pos: u64) -> (usize, u64) {
        if pos >= self.len() {
            return (self.pieces.len(), 0);
        }
        let i = self.byte_prefix.partition_point(|&b| b <= pos) - 1;
        (i, pos - self.byte_prefix[i])
    }

    /// Iterate over the document bytes in `range` as borrowed slices.
    pub fn chunks(&self, range: Range<u64>) -> impl Iterator<Item = &[u8]> + '_ {
        let end = range.end.min(self.len());
        let start = range.start.min(end);
        let (first, mut off) = self.locate(start);
        let mut pos = start;
        self.pieces[first.min(self.pieces.len())..].iter().map_while(move |p| {
            if pos >= end {
                return None;
            }
            let bytes = self.piece_bytes(p);
            let take = (p.len - off).min(end - pos);
            let s = &bytes[off as usize..(off + take) as usize];
            pos += take;
            off = 0;
            Some(s)
        })
    }

    /// Read a byte range; borrows when the range lies inside a single piece.
    pub fn read(&self, range: Range<u64>) -> Cow<'_, [u8]> {
        let mut it = self.chunks(range.clone());
        let Some(first) = it.next() else { return Cow::Borrowed(&[]) };
        let Some(second) = it.next() else { return Cow::Borrowed(first) };
        let mut v = Vec::with_capacity((range.end - range.start) as usize);
        v.extend_from_slice(first);
        v.extend_from_slice(second);
        for c in it {
            v.extend_from_slice(c);
        }
        Cow::Owned(v)
    }

    pub fn byte_at(&self, pos: u64) -> Option<u8> {
        self.chunks(pos..pos + 1).next().map(|c| c[0])
    }

    /// Byte offset where `line` (0-based) starts. Clamped to the last line.
    pub fn line_start(&self, line: u64) -> u64 {
        if line == 0 {
            return 0;
        }
        let line = line.min(self.total_newlines());
        if line == 0 {
            return 0;
        }
        let k = self.nl_prefix.partition_point(|&n| n < line) - 1;
        let need = line - self.nl_prefix[k];
        let p = &self.pieces[k];
        let rel = if p.add {
            memchr::memchr_iter(b'\n', self.piece_bytes(p)).nth(need as usize - 1).unwrap() as u64 + 1
        } else {
            let data = self.orig.data();
            let n0 = self.orig.index.newlines_before(data, p.start);
            self.orig.index.pos_after_nth_newline(data, n0 + need).unwrap() - p.start
        };
        self.byte_prefix[k] + rel
    }

    /// Offset of the `\n` ending `line`, or the document length for the last line.
    pub fn line_end(&self, line: u64) -> u64 {
        if line >= self.total_newlines() {
            self.len()
        } else {
            self.line_start(line + 1) - 1
        }
    }

    /// End of the visible content of `line` (excludes a trailing `\r`).
    pub fn line_content_end(&self, line: u64) -> u64 {
        let e = self.line_end(line);
        if e > self.line_start(line) && e < self.len() && self.byte_at(e - 1) == Some(b'\r') {
            e - 1
        } else {
            e
        }
    }

    /// 0-based line containing `pos`.
    pub fn line_of(&self, pos: u64) -> u64 {
        let (k, off) = self.locate(pos);
        if k >= self.pieces.len() {
            return self.total_newlines();
        }
        let p = &self.pieces[k];
        let inner = if p.add {
            memchr::memchr_iter(b'\n', &self.piece_bytes(p)[..off as usize]).count() as u64
        } else {
            self.orig.newlines_in(p.start, p.start + off)
        };
        self.nl_prefix[k] + inner
    }

    /// Next cursor position after `pos` (skips whole UTF-8 sequences and CRLF).
    pub fn next_boundary(&self, pos: u64) -> u64 {
        let len = self.len();
        if pos >= len {
            return len;
        }
        let w = self.read(pos..(pos + 4).min(len));
        if w.starts_with(b"\r\n") {
            return pos + 2;
        }
        let n = utf8_len(w[0]);
        if n > 1 && w.len() >= n && w[1..n].iter().all(|&b| is_cont(b)) {
            pos + n as u64
        } else {
            pos + 1
        }
    }

    /// Previous cursor position before `pos`.
    pub fn prev_boundary(&self, pos: u64) -> u64 {
        if pos == 0 {
            return 0;
        }
        let pos = pos.min(self.len());
        let lo = pos.saturating_sub(4);
        let w = self.read(lo..pos);
        if w.ends_with(b"\r\n") {
            return pos - 2;
        }
        let mut i = w.len() - 1;
        while i > 0 && is_cont(w[i]) && w.len() - i < 4 {
            i -= 1;
        }
        let n = w.len() - i;
        if n > 1 && utf8_len(w[i]) == n {
            pos - n as u64
        } else {
            pos - 1
        }
    }

    // ---------------------------------------------------------------- editing

    fn make_piece(&self, add: bool, start: u64, len: u64) -> Piece {
        let newlines = if add {
            memchr::memchr_iter(b'\n', &self.add[start as usize..(start + len) as usize]).count() as u64
        } else if len <= 2 * CHECKPOINT_BYTES {
            let d = &self.orig.data()[start as usize..(start + len) as usize];
            memchr::memchr_iter(b'\n', d).count() as u64
        } else {
            self.orig.newlines_in(start, start + len)
        };
        Piece { add, start, len, newlines }
    }

    fn sub_piece(&self, p: &Piece, off: u64, len: u64) -> Piece {
        if off == 0 && len == p.len {
            return *p;
        }
        self.make_piece(p.add, p.start + off, len)
    }

    fn rebuild_prefix(&mut self, from: usize) {
        self.byte_prefix.truncate(from + 1);
        self.nl_prefix.truncate(from + 1);
        let (mut b, mut n) = (self.byte_prefix[from], self.nl_prefix[from]);
        for p in &self.pieces[from..] {
            b += p.len;
            n += p.newlines;
            self.byte_prefix.push(b);
            self.nl_prefix.push(n);
        }
    }

    fn splice(&mut self, at: usize, remove: usize, insert: Vec<Piece>) -> EditRecord {
        let removed: Vec<Piece> = self.pieces.splice(at..at + remove, insert.iter().copied()).collect();
        self.rebuild_prefix(at);
        self.version += 1;
        EditRecord { at, removed, inserted: insert }
    }

    /// Replace `range` with `text`, recording an undo step.
    /// `sel_before` / `sel_after` are restored by undo / redo.
    pub fn edit(&mut self, range: Range<u64>, text: &[u8], kind: EditKind, sel_before: Sel, sel_after: Sel) {
        let len = self.len();
        let (start, end) = (range.start.min(len), range.end.min(len));
        let (start, end) = (start.min(end), end.max(start));
        if start == end && text.is_empty() {
            return;
        }
        let (i, a) = self.locate(start);
        let (j, b) = self.locate(end);

        let coalesce_piece = start == end
            && a == 0
            && i > 0
            && self.pieces[i - 1].add
            && self.pieces[i - 1].start + self.pieces[i - 1].len == self.add.len() as u64;

        let rec = if coalesce_piece {
            let prev = self.pieces[i - 1];
            self.add.extend_from_slice(text);
            let nl = memchr::memchr_iter(b'\n', text).count() as u64;
            let ext = Piece { len: prev.len + text.len() as u64, newlines: prev.newlines + nl, ..prev };
            self.splice(i - 1, 1, vec![ext])
        } else {
            let mut new = Vec::with_capacity(3);
            if a > 0 {
                new.push(self.sub_piece(&self.pieces[i], 0, a));
            }
            if !text.is_empty() {
                let s = self.add.len() as u64;
                self.add.extend_from_slice(text);
                new.push(self.make_piece(true, s, text.len() as u64));
            }
            let rem_end = if j < self.pieces.len() && b > 0 {
                let pj = self.pieces[j];
                new.push(self.sub_piece(&pj, b, pj.len - b));
                j + 1
            } else {
                j
            };
            self.splice(i, rem_end - i, new)
        };
        self.push_record(rec, kind, sel_before, sel_after);
    }

    fn push_record(&mut self, rec: EditRecord, kind: EditKind, sel_before: Sel, sel_after: Sel) {
        self.redo.clear();
        if let Some(top) = self.undo.last_mut() {
            if kind == EditKind::Typing
                && top.kind == EditKind::Typing
                && !self.seal_typing
                && top.sel_after == sel_before
                && top.id != self.saved_group_id
            {
                top.records.push(rec);
                top.sel_after = sel_after;
                return;
            }
        }
        self.seal_typing = false;
        let id = self.next_group_id;
        self.next_group_id += 1;
        self.undo.push(UndoGroup { id, kind, records: vec![rec], sel_before, sel_after });
    }

    /// Start a new undo step on the next edit even if it is typing.
    pub fn seal_undo_group(&mut self) {
        self.seal_typing = true;
    }

    pub fn undo(&mut self) -> Option<Sel> {
        let g = self.undo.pop()?;
        for r in g.records.iter().rev() {
            self.pieces.splice(r.at..r.at + r.inserted.len(), r.removed.iter().copied());
            self.rebuild_prefix(r.at);
        }
        self.version += 1;
        self.seal_typing = true;
        let sel = g.sel_before;
        self.redo.push(g);
        Some(sel)
    }

    pub fn redo(&mut self) -> Option<Sel> {
        let g = self.redo.pop()?;
        for r in &g.records {
            self.pieces.splice(r.at..r.at + r.removed.len(), r.inserted.iter().copied());
            self.rebuild_prefix(r.at);
        }
        self.version += 1;
        self.seal_typing = true;
        let sel = g.sel_after;
        self.undo.push(g);
        Some(sel)
    }

    // ---------------------------------------------------------------- search

    /// One bounded step of a search for `needle` inside `lo..hi`.
    ///
    /// Forward searches scan from `cursor` towards `hi`; backward searches
    /// look for a match that ends at or before `cursor`, scanning towards
    /// `lo`. At most about `budget` bytes are examined per call.
    #[allow(clippy::too_many_arguments)]
    pub fn find_step(
        &self,
        needle: &[u8],
        lo: u64,
        hi: u64,
        cursor: u64,
        forward: bool,
        case_sensitive: bool,
        budget: u64,
    ) -> FindStep {
        let n = needle.len() as u64;
        let hi = hi.min(self.len());
        if n == 0 || hi < lo || hi - lo < n {
            return FindStep::NotFound;
        }
        let budget = budget.clamp(1 << 16, 64 << 20);
        let lowered;
        let needle = if case_sensitive {
            needle
        } else {
            lowered = needle.to_ascii_lowercase();
            &lowered[..]
        };
        let search = |ws: u64, we: u64| -> Option<u64> {
            let bytes = self.read(ws..we);
            let hay: Cow<[u8]> = if case_sensitive { bytes } else { Cow::Owned(bytes.to_ascii_lowercase()) };
            let r = if forward { memmem::find(&hay, needle) } else { memmem::rfind(&hay, needle) };
            r.map(|r| ws + r as u64)
        };
        if forward {
            let ws = cursor.clamp(lo, hi);
            let we = (ws + budget + n - 1).min(hi);
            if we - ws < n {
                return FindStep::NotFound;
            }
            match search(ws, we) {
                Some(p) => FindStep::Found(p),
                None if we == hi => FindStep::NotFound,
                None => FindStep::Pending(we - (n - 1)),
            }
        } else {
            let we = cursor.clamp(lo, hi);
            let ws = we.saturating_sub(budget + n - 1).max(lo);
            if we - ws < n {
                return FindStep::NotFound;
            }
            match search(ws, we) {
                Some(p) => FindStep::Found(p),
                None if ws == lo => FindStep::NotFound,
                None => FindStep::Pending(ws + n - 1),
            }
        }
    }

    /// Unbounded search convenience wrapper over [`Self::find_step`].
    pub fn find(&self, needle: &[u8], from: u64, forward: bool, case_sensitive: bool) -> Option<u64> {
        let mut cur = from;
        loop {
            match self.find_step(needle, 0, self.len(), cur, forward, case_sensitive, 64 << 20) {
                FindStep::Found(p) => return Some(p),
                FindStep::NotFound => return None,
                FindStep::Pending(c) => cur = c,
            }
        }
    }

    /// Pieces describing the current document bytes in `a..b`.
    fn pieces_for_range(&self, a: u64, b: u64, out: &mut Vec<Piece>) {
        if a >= b {
            return;
        }
        let (mut k, mut off) = self.locate(a);
        let mut pos = a;
        while pos < b {
            let p = self.pieces[k];
            let take = (p.len - off).min(b - pos);
            out.push(self.sub_piece(&p, off, take));
            pos += take;
            k += 1;
            off = 0;
        }
    }

    /// Replace every non-overlapping occurrence of `needle`. Returns the
    /// number of replacements, or an error if there are too many matches.
    pub fn replace_all(
        &mut self,
        needle: &[u8],
        replacement: &[u8],
        case_sensitive: bool,
        sel: Sel,
    ) -> Result<usize, String> {
        if needle.is_empty() {
            return Ok(0);
        }
        let n = needle.len() as u64;
        let mut matches = Vec::new();
        let mut cur = 0;
        while let Some(p) = self.find(needle, cur, true, case_sensitive) {
            matches.push(p);
            if matches.len() > MAX_REPLACE_ALL {
                return Err(format!("More than {MAX_REPLACE_ALL} matches; refine the search."));
            }
            cur = p + n;
        }
        if matches.is_empty() {
            return Ok(0);
        }
        let rs = self.add.len() as u64;
        self.add.extend_from_slice(replacement);
        let rep = (!replacement.is_empty()).then(|| self.make_piece(true, rs, replacement.len() as u64));
        let mut new = Vec::with_capacity(matches.len() * 2 + 1);
        let mut prev = 0;
        for &m in &matches {
            self.pieces_for_range(prev, m, &mut new);
            new.extend(rep);
            prev = m + n;
        }
        self.pieces_for_range(prev, self.len(), &mut new);
        let count = self.pieces.len();
        let rec = self.splice(0, count, new);
        let clamp = |x: u64| x.min(self.len());
        self.push_record(rec, EditKind::Other, sel, (clamp(sel.0), clamp(sel.1)));
        Ok(matches.len())
    }

    // ---------------------------------------------------------------- saving

    /// An immutable copy of the document's content that can be written out
    /// on another thread while the UI keeps running.
    pub fn snapshot(&self) -> SaveSnapshot {
        SaveSnapshot { orig: self.orig.clone(), add: Arc::new(self.add.clone()), pieces: self.pieces.clone() }
    }

    /// Save to `path` atomically (temp file + rename) and re-base the
    /// document on the freshly written file.
    pub fn save(&mut self, path: &Path) -> io::Result<()> {
        let written = self.snapshot().write_temp(path, &AtomicU64::new(0), &AtomicBool::new(false))?;
        self.commit_save(path, written)
    }

    /// Second half of a save: move the temp file written by
    /// [`SaveSnapshot::write_temp`] into place and map it. Undo history is
    /// cleared because it refers to the previous file's bytes.
    pub fn commit_save(&mut self, path: &Path, written: WrittenFile) -> io::Result<()> {
        let WrittenFile { tmp, index } = written;
        // Windows refuses to replace a file that is still mapped, so drop our
        // mapping before the rename, keeping just enough to re-map it if the
        // rename fails.
        let released = if cfg!(windows) && matches!(*self.orig.bytes, Bytes::Mapped(_)) {
            let old = std::mem::replace(&mut self.orig, Arc::new(Source::empty()));
            Some((old.path.clone(), old.index.clone()))
        } else {
            None
        };
        if let Err(e) = fs::rename(&tmp, path) {
            let _ = fs::remove_file(&tmp);
            if let Some((Some(p), old_index)) = released {
                self.orig = Arc::new(Source { bytes: map_file(&p)?, index: old_index, path: Some(p) });
            }
            return Err(e);
        }

        let bytes = map_file(path)?;
        self.orig = Arc::new(Source { bytes, index, path: Some(path.to_path_buf()) });
        self.add.clear();
        self.undo.clear();
        self.redo.clear();
        self.saved_group_id = 0;
        self.reset_to_orig();
        Ok(())
    }
}

/// Content captured by [`Document::snapshot`].
pub struct SaveSnapshot {
    orig: Arc<Source>,
    add: Arc<Vec<u8>>,
    pieces: Vec<Piece>,
}

/// A fully written temp file waiting to be renamed over the target.
pub struct WrittenFile {
    tmp: PathBuf,
    index: LineIndex,
}

impl SaveSnapshot {
    pub fn len(&self) -> u64 {
        self.pieces.iter().map(|p| p.len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Write everything to a temp file next to `path`, indexing as we go.
    /// `progress` receives the number of bytes written.
    pub fn write_temp(&self, path: &Path, progress: &AtomicU64, cancel: &AtomicBool) -> io::Result<WrittenFile> {
        let tmp = temp_path(path);
        let mut index = IndexBuilder::new();
        let res = (|| {
            let f = File::create(&tmp)?;
            let mut w = BufWriter::with_capacity(8 << 20, f);
            for p in &self.pieces {
                let r = p.start as usize..(p.start + p.len) as usize;
                let bytes = if p.add { &self.add[r] } else { &self.orig.data()[r] };
                for chunk in bytes.chunks(4 << 20) {
                    if cancel.load(Ordering::Relaxed) {
                        return Err(io::Error::new(io::ErrorKind::Interrupted, "save cancelled"));
                    }
                    w.write_all(chunk)?;
                    index.feed(chunk);
                    progress.fetch_add(chunk.len() as u64, Ordering::Relaxed);
                }
            }
            let f = w.into_inner().map_err(|e| e.into_error())?;
            f.sync_all()
        })();
        match res {
            Ok(()) => Ok(WrittenFile { tmp, index: index.finish() }),
            Err(e) => {
                let _ = fs::remove_file(&tmp);
                Err(e)
            }
        }
    }
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.notepad-fast-save.tmp"))
}

#[inline]
fn is_cont(b: u8) -> bool {
    b & 0xC0 == 0x80
}

#[inline]
fn utf8_len(b: u8) -> usize {
    match b {
        0x00..=0x7F => 1,
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => 1,
    }
}

//! Splits logical lines into visual rows.
//!
//! Lines up to [`EXACT_LINE_LIMIT`] bytes are wrapped exactly (at word
//! boundaries when word wrap is on). Longer lines, such as a multi-gigabyte
//! single-line JSON document, are cut into fixed byte-sized rows whose
//! position can be computed in O(1), so scrolling through them never needs
//! to scan from the start of the line.

use super::document::Document;

pub const TAB_WIDTH: usize = 4;
/// Lines longer than this are laid out in fixed-size byte chunks.
pub const EXACT_LINE_LIMIT: u64 = 64 * 1024;
/// Row length for very long lines when word wrap is off.
pub const NOWRAP_CHUNK: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct RowPos {
    pub line: u64,
    pub row: u64,
}

#[derive(Debug, Clone)]
pub enum Rows {
    /// Absolute start offsets of each row.
    Exact(Vec<u64>),
    /// Rows of `chunk` bytes (start aligned forward to a UTF-8 boundary).
    Chunked { chunk: u64, count: u64 },
}

#[derive(Debug, Clone)]
pub struct LineLayout {
    pub line: u64,
    pub start: u64,
    /// End of displayable content (before `\r\n` / `\n`).
    pub content_end: u64,
    pub rows: Rows,
}

impl LineLayout {
    pub fn row_count(&self) -> u64 {
        match &self.rows {
            Rows::Exact(v) => v.len() as u64,
            Rows::Chunked { count, .. } => *count,
        }
    }
}

/// One displayed character of a row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Glyph {
    /// Document offset of the first byte of this character.
    pub pos: u64,
    pub col: usize,
    pub width: usize,
    pub ch: char,
}

/// Decode bytes leniently: invalid UTF-8 bytes become U+FFFD one at a time.
pub fn decode(bytes: &[u8], mut f: impl FnMut(usize, usize, char)) {
    let mut off = 0;
    for chunk in bytes.utf8_chunks() {
        for (i, c) in chunk.valid().char_indices() {
            f(off + i, c.len_utf8(), c);
        }
        off += chunk.valid().len();
        for _ in chunk.invalid() {
            f(off, 1, '\u{FFFD}');
            off += 1;
        }
    }
}

/// Character shown for `c` (tabs are expanded separately).
#[inline]
pub fn display_char(c: char) -> char {
    if (c.is_control() && c != '\t') || c == '\u{FEFF}' {
        '\u{00B7}'
    } else {
        c
    }
}

#[inline]
fn char_width(c: char, col: usize) -> usize {
    if c == '\t' {
        TAB_WIDTH - col % TAB_WIDTH
    } else {
        1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// `Some(cols)` when word wrap is on.
    pub wrap: Option<usize>,
}

impl Layout {
    pub fn line(&self, doc: &Document, line: u64) -> LineLayout {
        let start = doc.line_start(line);
        let content_end = doc.line_content_end(line);
        let len = content_end - start;
        let rows = if len > EXACT_LINE_LIMIT {
            let chunk = self.wrap.map_or(NOWRAP_CHUNK, |c| c.max(8) as u64);
            Rows::Chunked { chunk, count: len.div_ceil(chunk).max(1) }
        } else {
            match self.wrap {
                None => Rows::Exact(vec![start]),
                Some(cols) => Rows::Exact(wrap_rows(&doc.read(start..content_end), start, cols.max(1))),
            }
        };
        LineLayout { line, start, content_end, rows }
    }

    /// Byte range `[start, end)` of row `k` of `ll`.
    pub fn row_range(&self, doc: &Document, ll: &LineLayout, k: u64) -> (u64, u64) {
        match &ll.rows {
            Rows::Exact(v) => {
                let k = (k as usize).min(v.len() - 1);
                let e = v.get(k + 1).copied().unwrap_or(ll.content_end);
                (v[k], e)
            }
            Rows::Chunked { chunk, count } => {
                let k = k.min(count - 1);
                let s = align(doc, ll.start + k * chunk, ll.content_end);
                let e = if k + 1 >= *count {
                    ll.content_end
                } else {
                    align(doc, ll.start + (k + 1) * chunk, ll.content_end)
                };
                (s, e)
            }
        }
    }

    /// Row of `ll` containing document offset `pos`.
    pub fn row_of(&self, doc: &Document, ll: &LineLayout, pos: u64) -> u64 {
        match &ll.rows {
            Rows::Exact(v) => (v.partition_point(|&s| s <= pos).max(1) - 1) as u64,
            Rows::Chunked { chunk, count } => {
                let mut k = (pos.saturating_sub(ll.start) / chunk).min(count - 1);
                while k > 0 && self.row_range(doc, ll, k).0 > pos {
                    k -= 1;
                }
                k
            }
        }
    }

    /// Glyphs of the row `[s, e)`.
    pub fn glyphs(&self, doc: &Document, s: u64, e: u64) -> Vec<Glyph> {
        let bytes = doc.read(s..e);
        let mut out = Vec::with_capacity(bytes.len());
        let mut col = 0;
        decode(&bytes, |off, _, c| {
            let width = char_width(c, col);
            out.push(Glyph { pos: s + off as u64, col, width, ch: display_char(c) });
            col += width;
        });
        out
    }
}

/// Move `pos` forward past UTF-8 continuation bytes (at most 3).
fn align(doc: &Document, pos: u64, end: u64) -> u64 {
    if pos >= end {
        return end;
    }
    let w = doc.read(pos..(pos + 4).min(end));
    let skip = w.iter().take(3).take_while(|&&b| b & 0xC0 == 0x80).count();
    pos + skip as u64
}

/// Greedy word wrap of one line into rows of at most `cols` columns.
fn wrap_rows(bytes: &[u8], base: u64, cols: usize) -> Vec<u64> {
    let mut rows = vec![base];
    let mut col = 0usize;
    // Offset just after the last whitespace in the current row, and the
    // column there.
    let mut brk: Option<(usize, usize)> = None;
    let mut row_start = 0usize;
    decode(bytes, |off, len, c| {
        let w = char_width(c, col);
        if col + w > cols && col > 0 {
            match brk {
                Some((b, bcol)) if b > row_start && b <= off => {
                    row_start = b;
                    col -= bcol;
                }
                _ => {
                    row_start = off;
                    col = 0;
                }
            }
            rows.push(base + row_start as u64);
            brk = None;
        }
        col += char_width(c, col);
        if c == ' ' || c == '\t' {
            brk = Some((off + len, col));
        }
    });
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nowrap_is_one_row_per_line() {
        let d = Document::from_bytes("hello\r\nworld\n");
        let l = Layout { wrap: None };
        let ll = l.line(&d, 0);
        assert_eq!((ll.start, ll.content_end, ll.row_count()), (0, 5, 1));
        let ll = l.line(&d, 1);
        assert_eq!((ll.start, ll.content_end), (7, 12));
        let ll = l.line(&d, 2);
        assert_eq!((ll.start, ll.content_end, ll.row_count()), (13, 13, 1));
    }

    #[test]
    fn word_wrap_breaks_at_spaces() {
        let d = Document::from_bytes("aaa bbb ccc dddddddddddd");
        let l = Layout { wrap: Some(8) };
        let ll = l.line(&d, 0);
        let rows: Vec<_> = (0..ll.row_count()).map(|k| l.row_range(&d, &ll, k)).collect();
        let text: Vec<String> =
            rows.iter().map(|&(s, e)| String::from_utf8(d.read(s..e).into_owned()).unwrap()).collect();
        assert_eq!(text, vec!["aaa bbb ", "ccc ", "dddddddd", "dddd"]);
        assert_eq!(l.row_of(&d, &ll, 9), 1);
    }

    #[test]
    fn long_lines_are_chunked_on_char_boundaries() {
        let line = "é".repeat(50_000); // 100_000 bytes, > EXACT_LINE_LIMIT
        let d = Document::from_bytes(format!("{line}\nnext"));
        let l = Layout { wrap: None };
        let ll = l.line(&d, 0);
        assert!(matches!(ll.rows, Rows::Chunked { .. }));
        let mut prev_end = 0;
        for k in 0..ll.row_count() {
            let (s, e) = l.row_range(&d, &ll, k);
            assert_eq!(s, prev_end);
            assert!(std::str::from_utf8(&d.read(s..e)).is_ok());
            assert_eq!(l.row_of(&d, &ll, s), k);
            prev_end = e;
        }
        assert_eq!(prev_end, 100_000);
    }

    #[test]
    fn glyphs_expand_tabs_and_handle_invalid_utf8() {
        let d = Document::from_bytes(b"a\tb\xffc".to_vec());
        let g = Layout { wrap: None }.glyphs(&d, 0, d.len());
        let cols: Vec<_> = g.iter().map(|g| (g.col, g.ch)).collect();
        assert_eq!(cols, vec![(0, 'a'), (1, '\t'), (4, 'b'), (5, '\u{FFFD}'), (6, 'c')]);
    }
}

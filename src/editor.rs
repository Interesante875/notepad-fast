//! The virtualised text view.
//!
//! Only the rows that are on screen are ever laid out or painted. The scroll
//! position is a (line, row) pair rather than a pixel offset, so a file with
//! billions of lines scrolls exactly like a file with ten.
//!
//! Each visible row is shaped by egui (so CJK, Arabic, Devanagari, emoji and
//! combining marks render correctly) and the shaped layout is used for every
//! position <-> x mapping: cursor, selection, clicks and vertical movement.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use egui::text::{CCursor, LayoutJob, TextFormat};
use egui::{
    Align2, Context, CornerRadius, Event, EventFilter, FontId, Galley, Id, ImeEvent, Key, Modifiers, Pos2, Rect, Sense,
    Stroke, Ui, Vec2,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::core::layout::{Glyph, Layout, LineLayout, RowPos};
use crate::core::{Document, EditKind, Sel};
use crate::highlight::{colorize, FileKind};
use crate::theme::Palette;

/// Largest selection we will put on the clipboard.
pub const MAX_CLIPBOARD_BYTES: u64 = 256 << 20;
/// The X11 clipboard backend sends data in a single X request, which the
/// server caps at about 16 MiB; larger copies would silently fail.
pub const MAX_X11_CLIPBOARD_BYTES: u64 = 15 << 20;

/// Clipboard size limit for the current platform / display server.
pub fn clipboard_limit() -> u64 {
    let x11 = cfg!(all(unix, not(target_os = "macos"))) && std::env::var_os("WAYLAND_DISPLAY").is_none();
    if x11 {
        MAX_X11_CLIPBOARD_BYTES
    } else {
        MAX_CLIPBOARD_BYTES
    }
}
const PAD: f32 = 6.0;
const SCROLLBAR_W: f32 = 12.0;
const HBAR_H: f32 = 10.0;
/// Rows with more glyphs than this are only shaped around the visible part.
const FULL_SHAPE_LIMIT: usize = 4096;
/// Extra glyphs shaped on each side of the visible window of a long row.
const WINDOW_MARGIN: usize = 256;
/// Bytes examined around the cursor for word movement / selection.
const WORD_WINDOW: u64 = 4096;

pub struct EditorConfig<'a> {
    pub font_size: f32,
    pub wrap: bool,
    pub line_numbers: bool,
    pub palette: &'a Palette,
    pub kind: FileKind,
    /// Occurrences of this needle are highlighted (needle, case_sensitive).
    pub highlight: Option<(&'a [u8], bool)>,
    pub read_only: bool,
}

#[derive(Default)]
pub struct EditorOutput {
    pub message: Option<String>,
    pub focused: bool,
}

#[derive(Default)]
struct LayoutCache {
    key: Option<(u64, Layout)>,
    lines: HashMap<u64, LineLayout>,
}

/// Everything needed to shape a row, captured once per frame.
#[derive(Clone)]
struct Style {
    font: FontId,
    char_w: f32,
    palette: Palette,
    kind: FileKind,
    /// Visible text width; long rows are shaped only around `hscroll..hscroll + view_w`.
    view_w: f32,
    hscroll: f32,
}

/// A shaped row.
struct RowView {
    rp: RowPos,
    start: u64,
    end: u64,
    last_in_line: bool,
    glyphs: Vec<Glyph>,
    /// Glyph range `g0..g1` that was shaped into `galley`.
    g0: usize,
    g1: usize,
    /// Char index in the galley text at which glyph `g0 + k` starts (`len = g1 - g0 + 1`).
    char_at: Vec<usize>,
    /// Content x (before scrolling) where the galley starts.
    x0: f32,
    galley: Arc<Galley>,
    char_w: f32,
}

impl RowView {
    /// Content x (before horizontal scrolling) of the caret at `pos`.
    fn x_of(&self, pos: u64) -> f32 {
        let i = self.glyphs.partition_point(|g| g.pos < pos);
        if i < self.g0 {
            return self.glyphs[i].col as f32 * self.char_w;
        }
        if i > self.g1 {
            let end_shaped = self.x0 + self.galley.size().x;
            let col_shaped = self.glyphs.get(self.g1).map_or(0, |g| g.col);
            let col = self.glyphs.get(i).map_or_else(|| self.end_col(), |g| g.col);
            return end_shaped + (col - col_shaped) as f32 * self.char_w;
        }
        let ci = self.char_at[i - self.g0];
        self.x0 + self.galley.pos_from_cursor(CCursor::new(ci)).min.x
    }

    fn end_col(&self) -> usize {
        self.glyphs.last().map_or(0, |g| g.col + g.width)
    }

    /// Total content width of the row.
    fn width(&self) -> f32 {
        if self.g1 == self.glyphs.len() {
            self.x0 + self.galley.size().x
        } else {
            self.end_col() as f32 * self.char_w
        }
    }

    /// Document position whose caret is closest to content x `x`.
    fn pos_at(&self, x: f32) -> u64 {
        if self.glyphs.is_empty() {
            return self.start;
        }
        let k = if x < self.x0 && self.g0 > 0 {
            let col = (x / self.char_w).round().max(0.0) as usize;
            self.glyphs.partition_point(|g| g.col + g.width / 2 < col)
        } else {
            let c = self.galley.cursor_from_pos(Vec2::new(x - self.x0, self.galley.size().y / 2.0));
            // Map the char index back to a glyph (tabs span several chars).
            let ci = c.index.0;
            let rel = self.char_at.partition_point(|&s| s <= ci).saturating_sub(1);
            let span = self.char_at.get(rel + 1).map_or(0, |&n| n - self.char_at[rel]);
            let rel = if (ci - self.char_at[rel]) * 2 > span { rel + 1 } else { rel };
            self.g0 + rel
        };
        if k >= self.glyphs.len() {
            if self.last_in_line {
                self.end
            } else {
                // Stay on this wrapped row rather than jumping to the next one.
                self.glyphs.last().unwrap().pos
            }
        } else {
            self.glyphs[k].pos
        }
    }
}

#[derive(Default)]
pub struct EditorState {
    /// (anchor, cursor)
    pub sel: Sel,
    pub top: RowPos,
    pub hscroll: f32,
    /// Content x the caret tries to keep while moving up/down.
    desired_x: Option<f32>,
    scroll_accum: f32,
    pub scroll_to_cursor: bool,
    pub center_on_cursor: bool,
    pub request_focus: bool,
    pub viewport_rows: usize,
    pub layout: Option<Layout>,
    cache: LayoutCache,
    last_activity: f64,
    vbar_grab: Option<f32>,
    style: Option<Style>,
    /// Text being composed by an input method (Chinese, Japanese, Korean, ...).
    pub preedit: String,
}

impl EditorState {
    pub fn cursor(&self) -> u64 {
        self.sel.1
    }

    pub fn selection(&self) -> (u64, u64) {
        (self.sel.0.min(self.sel.1), self.sel.0.max(self.sel.1))
    }

    pub fn set_selection(&mut self, anchor: u64, cursor: u64) {
        self.sel = (anchor, cursor);
        self.desired_x = None;
        self.scroll_to_cursor = true;
    }

    pub fn reset(&mut self) {
        *self = EditorState { request_focus: true, ..Default::default() };
    }

    fn line(&mut self, doc: &Document, line: u64) -> LineLayout {
        let layout = self.lay();
        let key = (doc.version(), layout);
        if self.cache.key != Some(key) {
            self.cache.key = Some(key);
            self.cache.lines.clear();
        }
        if self.cache.lines.len() > 4096 {
            self.cache.lines.clear();
        }
        self.cache.lines.entry(line).or_insert_with(|| layout.line(doc, line)).clone()
    }

    fn lay(&self) -> Layout {
        self.layout.unwrap_or(Layout { wrap: None })
    }

    fn row_range(&mut self, doc: &Document, rp: RowPos) -> (u64, u64, bool) {
        let ll = self.line(doc, rp.line);
        let (s, e) = self.lay().row_range(doc, &ll, rp.row);
        (s, e, rp.row + 1 >= ll.row_count())
    }

    fn next_row(&mut self, doc: &Document, rp: RowPos) -> Option<RowPos> {
        let ll = self.line(doc, rp.line);
        if rp.row + 1 < ll.row_count() {
            Some(RowPos { line: rp.line, row: rp.row + 1 })
        } else if rp.line + 1 < doc.line_count() {
            Some(RowPos { line: rp.line + 1, row: 0 })
        } else {
            None
        }
    }

    fn prev_row(&mut self, doc: &Document, rp: RowPos) -> Option<RowPos> {
        if rp.row > 0 {
            Some(RowPos { line: rp.line, row: rp.row - 1 })
        } else if rp.line > 0 {
            let ll = self.line(doc, rp.line - 1);
            Some(RowPos { line: rp.line - 1, row: ll.row_count() - 1 })
        } else {
            None
        }
    }

    fn row_of_pos(&mut self, doc: &Document, pos: u64) -> RowPos {
        let line = doc.line_of(pos);
        let ll = self.line(doc, line);
        RowPos { line, row: self.lay().row_of(doc, &ll, pos) }
    }

    fn walk_back(&mut self, doc: &Document, mut rp: RowPos, n: usize) -> RowPos {
        for _ in 0..n {
            match self.prev_row(doc, rp) {
                Some(p) => rp = p,
                None => break,
            }
        }
        rp
    }

    fn clamp_top(&mut self, doc: &Document) {
        let last_line = doc.line_count() - 1;
        if self.top.line > last_line {
            self.top = RowPos { line: last_line, row: 0 };
        }
        let rc = self.line(doc, self.top.line).row_count();
        if self.top.row >= rc {
            self.top.row = rc - 1;
        }
        // Don't scroll past the point where the last row sits at the bottom.
        let ll = self.line(doc, last_line);
        let last = RowPos { line: last_line, row: ll.row_count() - 1 };
        let rows = self.viewport_rows.max(1);
        if last.line.saturating_sub(self.top.line) <= rows as u64 + 1 {
            let max_top = self.walk_back(doc, last, rows.saturating_sub(1));
            if self.top > max_top {
                self.top = max_top;
            }
        }
    }

    /// Number of rows from `a` forward to `b`, if within `limit`.
    fn rows_between(&mut self, doc: &Document, a: RowPos, b: RowPos, limit: usize) -> Option<usize> {
        if b < a || b.line - a.line > limit as u64 {
            return None;
        }
        let mut rp = a;
        for n in 0..=limit {
            if rp == b {
                return Some(n);
            }
            rp = self.next_row(doc, rp)?;
        }
        None
    }

    /// Shape row `rp` (only around the visible window if it is very long).
    fn row_view(&mut self, ctx: &Context, doc: &Document, rp: RowPos) -> RowView {
        let (start, end, last_in_line) = self.row_range(doc, rp);
        let glyphs = self.lay().glyphs(doc, start, end);
        let st = self.style.clone().expect("style is set before rows are shaped");
        let (g0, g1) = if glyphs.len() <= FULL_SHAPE_LIMIT {
            (0, glyphs.len())
        } else {
            let c0 = (st.hscroll / st.char_w) as usize;
            let c1 = ((st.hscroll + st.view_w) / st.char_w) as usize + 1;
            let a = glyphs.partition_point(|g| g.col < c0).saturating_sub(WINDOW_MARGIN);
            let b = (glyphs.partition_point(|g| g.col <= c1) + WINDOW_MARGIN).min(glyphs.len());
            (a, b)
        };
        // Galley text: tabs expanded to spaces; colour with some leading context.
        let ctx_lo = g0.saturating_sub(512);
        let mut chars: Vec<char> = Vec::new();
        let mut char_at = Vec::with_capacity(g1 - g0 + 1);
        let mut shaped_from = 0;
        for (k, g) in glyphs[ctx_lo..g1].iter().enumerate() {
            if ctx_lo + k == g0 {
                shaped_from = chars.len();
            }
            if ctx_lo + k >= g0 {
                char_at.push(chars.len() - shaped_from);
            }
            if g.ch == '\t' {
                chars.extend(std::iter::repeat_n(' ', g.width));
            } else {
                chars.push(g.ch);
            }
        }
        if g0 == g1 {
            shaped_from = chars.len();
        }
        char_at.push(chars.len() - shaped_from);
        let mut colors = Vec::new();
        colorize(st.kind, &chars, &st.palette, &mut colors);
        let mut job = LayoutJob::default();
        let mut k = shaped_from;
        while k < chars.len() {
            let c = colors[k];
            let mut j = k + 1;
            while j < chars.len() && colors[j] == c {
                j += 1;
            }
            let s: String = chars[k..j].iter().collect();
            job.append(&s, 0.0, TextFormat { font_id: st.font.clone(), color: c, ..Default::default() });
            k = j;
        }
        let galley = ctx.fonts_mut(|f| f.layout_job(job));
        let x0 = glyphs.get(g0).map_or(0.0, |g| g.col as f32 * st.char_w);
        RowView { rp, start, end, last_in_line, glyphs, g0, g1, char_at, x0, galley, char_w: st.char_w }
    }

    fn x_of_pos(&mut self, ctx: &Context, doc: &Document, pos: u64) -> f32 {
        let rp = self.row_of_pos(doc, pos);
        self.row_view(ctx, doc, rp).x_of(pos)
    }

    // ------------------------------------------------------------ editing

    fn replace_selection(&mut self, doc: &mut Document, text: &[u8], kind: EditKind) {
        let (a, b) = self.selection();
        if a != b {
            // Typing over a selection starts a fresh undo step that the
            // following keystrokes then join.
            doc.seal_undo_group();
        }
        let after = a + text.len() as u64;
        doc.edit(a..b, text, kind, self.sel, (after, after));
        self.set_selection(after, after);
    }

    fn delete_range(&mut self, doc: &mut Document, a: u64, b: u64) {
        if a < b {
            doc.edit(a..b, b"", EditKind::Other, self.sel, (a, a));
        }
        self.set_selection(a, a);
    }

    /// Insert text (e.g. from the clipboard), converting line endings to the
    /// document's convention.
    pub fn insert(&mut self, doc: &mut Document, text: &str) {
        let t = if doc.uses_crlf() {
            text.replace("\r\n", "\n").replace('\r', "\n").replace('\n', "\r\n")
        } else {
            text.replace("\r\n", "\n")
        };
        self.replace_selection(doc, t.as_bytes(), EditKind::Other);
    }

    pub fn selected_text(&self, doc: &Document) -> Result<String, String> {
        let (a, b) = self.selection();
        let limit = clipboard_limit();
        if b - a > limit {
            return Err(format!(
                "Selection is {} — too large for the clipboard (limit {}). Use Save As to copy it to a file.",
                human_bytes(b - a),
                human_bytes(limit)
            ));
        }
        Ok(String::from_utf8_lossy(&doc.read(a..b)).into_owned())
    }

    pub fn cut(&mut self, doc: &mut Document) -> Result<String, String> {
        let t = self.selected_text(doc)?;
        let (a, b) = self.selection();
        self.delete_range(doc, a, b);
        Ok(t)
    }

    pub fn select_all(&mut self, doc: &Document) {
        self.sel = (0, doc.len());
        self.desired_x = None;
    }

    /// Word (per Unicode word boundaries) around `pos`, for double-click.
    pub fn word_at(&self, doc: &Document, pos: u64) -> (u64, u64) {
        let lo = align_start(doc, pos.saturating_sub(WORD_WINDOW));
        let hi = (pos + WORD_WINDOW).min(doc.len());
        let bytes = doc.read(lo..hi);
        let text = valid_prefix(&bytes);
        let rel = (pos - lo) as usize;
        if rel > text.len() {
            return (pos, doc.next_boundary(pos));
        }
        let mut found = None;
        for (i, seg) in text.split_word_bound_indices() {
            if rel >= i && rel < i + seg.len() {
                found = Some((i, seg));
                break;
            }
            if rel == i + seg.len() {
                found = Some((i, seg)); // at the end of the line: take the previous word
            }
        }
        match found {
            Some((i, seg)) => (lo + i as u64, lo + (i + seg.len()) as u64),
            None => (pos, pos),
        }
    }

    /// Ctrl+Left / Ctrl+Right target.
    pub fn word_move(&self, doc: &Document, pos: u64, forward: bool) -> u64 {
        let is_space = |s: &str| s.chars().all(|c| c == ' ' || c == '\t');
        let is_newline = |s: &str| s.starts_with('\n') || s.starts_with('\r');
        if forward {
            let bytes = doc.read(pos..(pos + WORD_WINDOW).min(doc.len()));
            let text = valid_prefix(&bytes);
            let mut segs = text.split_word_bound_indices();
            let Some((_, first)) = segs.next() else { return doc.next_boundary(pos) };
            if is_newline(first) {
                return pos + first.len() as u64;
            }
            let mut end = first.len();
            for (i, seg) in segs {
                if is_space(seg) {
                    end = i + seg.len();
                } else {
                    break;
                }
            }
            pos + end as u64
        } else {
            let lo = align_start(doc, pos.saturating_sub(WORD_WINDOW));
            let bytes = doc.read(lo..pos);
            let text = valid_suffix(&bytes);
            let base = pos - text.len() as u64;
            let segs: Vec<(usize, &str)> = text.split_word_bound_indices().collect();
            let mut k = segs.len();
            while k > 0 && is_space(segs[k - 1].1) {
                k -= 1;
            }
            if k == 0 {
                return if segs.is_empty() { doc.prev_boundary(pos) } else { base };
            }
            let (i, seg) = segs[k - 1];
            if is_newline(seg) && k < segs.len() {
                return base + segs[k].0 as u64;
            }
            base + i as u64
        }
    }

    /// Handle one keyboard event.
    fn key(&mut self, ctx: &Context, doc: &mut Document, key: Key, m: Modifiers, read_only: bool) {
        let cur = self.cursor();
        let page = self.viewport_rows.max(2) - 1;
        let mut moved_to: Option<u64> = None;
        let mut keep_x = false;
        match key {
            Key::ArrowLeft | Key::ArrowRight => {
                let fwd = key == Key::ArrowRight;
                let (a, b) = self.selection();
                moved_to = Some(if a != b && !m.shift && !m.command && !m.alt {
                    if fwd {
                        b
                    } else {
                        a
                    }
                } else if m.command || m.alt {
                    self.word_move(doc, cur, fwd)
                } else if fwd {
                    doc.next_boundary(cur)
                } else {
                    doc.prev_boundary(cur)
                });
            }
            Key::ArrowUp | Key::ArrowDown | Key::PageUp | Key::PageDown => {
                let down = matches!(key, Key::ArrowDown | Key::PageDown);
                let n = if matches!(key, Key::PageUp | Key::PageDown) { page } else { 1 };
                let x = match self.desired_x {
                    Some(x) => x,
                    None => self.x_of_pos(ctx, doc, cur),
                };
                let mut rp = self.row_of_pos(doc, cur);
                let mut hit_edge = false;
                for _ in 0..n {
                    let next = if down { self.next_row(doc, rp) } else { self.prev_row(doc, rp) };
                    match next {
                        Some(p) => rp = p,
                        None => {
                            hit_edge = true;
                            break;
                        }
                    }
                }
                if n > 1 {
                    // Page keys scroll the view by the same amount.
                    let mut t = self.top;
                    for _ in 0..n {
                        let next = if down { self.next_row(doc, t) } else { self.prev_row(doc, t) };
                        match next {
                            Some(p) => t = p,
                            None => break,
                        }
                    }
                    self.top = t;
                }
                moved_to = Some(if hit_edge && n == 1 {
                    if down {
                        doc.len()
                    } else {
                        0
                    }
                } else {
                    self.row_view(ctx, doc, rp).pos_at(x)
                });
                self.desired_x = Some(x);
                keep_x = true;
            }
            Key::Home => {
                moved_to = Some(if m.command {
                    0
                } else {
                    let rp = self.row_of_pos(doc, cur);
                    let (s, e, _) = self.row_range(doc, rp);
                    // Smart home: first non-blank, then column 0.
                    let bytes = doc.read(s..e.min(s + 4096));
                    let indent = bytes.iter().take_while(|&&b| b == b' ' || b == b'\t').count() as u64;
                    if rp.row == 0 && cur != s + indent {
                        s + indent
                    } else {
                        s
                    }
                });
            }
            Key::End => {
                moved_to = Some(if m.command {
                    doc.len()
                } else {
                    let rp = self.row_of_pos(doc, cur);
                    let (_s, e, last) = self.row_range(doc, rp);
                    if last {
                        e
                    } else {
                        doc.prev_boundary(e)
                    }
                });
            }
            Key::Enter if !read_only => {
                let eol: &[u8] = if doc.uses_crlf() { b"\r\n" } else { b"\n" };
                self.replace_selection(doc, eol, EditKind::Other);
            }
            Key::Tab if !read_only && !m.command => {
                self.replace_selection(doc, b"\t", EditKind::Typing);
            }
            Key::Backspace if !read_only => {
                let (a, b) = self.selection();
                if a != b {
                    self.delete_range(doc, a, b);
                } else if cur > 0 {
                    let from = if m.command || m.alt {
                        self.word_move(doc, cur, false)
                    } else {
                        // Backspace removes one code point, so a mistyped
                        // accent or vowel sign can be corrected on its own.
                        doc.prev_char_boundary(cur)
                    };
                    self.delete_range(doc, from, cur);
                }
            }
            Key::Delete if !read_only => {
                let (a, b) = self.selection();
                if a != b {
                    self.delete_range(doc, a, b);
                } else if cur < doc.len() {
                    let to = if m.command || m.alt { self.word_move(doc, cur, true) } else { doc.next_boundary(cur) };
                    self.delete_range(doc, cur, to);
                }
            }
            Key::A if m.command => self.select_all(doc),
            Key::Escape => self.sel = (cur, cur),
            _ => {}
        }
        if let Some(p) = moved_to {
            let anchor = if m.shift { self.sel.0 } else { p };
            self.sel = (anchor, p);
            if !keep_x {
                self.desired_x = None;
            }
            self.scroll_to_cursor = true;
            doc.seal_undo_group();
        }
    }

    fn handle_ime(&mut self, doc: &mut Document, ev: ImeEvent) {
        match ev {
            ImeEvent::Preedit { text, .. } => self.preedit = text,
            ImeEvent::Commit(text) => {
                self.preedit.clear();
                if !text.is_empty() {
                    self.replace_selection(doc, text.as_bytes(), EditKind::Other);
                }
            }
            ImeEvent::DeleteSurrounding { before_chars, after_chars } => {
                let cur = self.cursor();
                let (mut a, mut b) = (cur, cur);
                for _ in 0..before_chars {
                    a = doc.prev_char_boundary(a);
                }
                for _ in 0..after_chars {
                    b = doc.next_char_boundary(b);
                }
                if a < b {
                    doc.edit(a..b, b"", EditKind::Other, self.sel, (a, a));
                    self.set_selection(a, a);
                }
            }
            #[allow(deprecated)]
            ImeEvent::Enabled | ImeEvent::Disabled => {}
        }
    }

    /// Draw the editor into the remaining space of `ui` and handle input.
    pub fn show(&mut self, ui: &mut Ui, doc: &mut Document, cfg: &EditorConfig) -> EditorOutput {
        let mut out = EditorOutput::default();
        let ctx = ui.ctx().clone();
        let p = cfg.palette;
        let rect = ui.available_rect_before_wrap();
        let id = Id::unique("notepad-fast-editor");
        let resp = ui.interact(rect, id, Sense::click_and_drag());
        ui.advance_cursor_after_rect(rect);
        let painter = ui.painter_at(rect);

        let font = FontId::monospace(cfg.font_size);
        let (char_w, font_h) = ctx.fonts_mut(|f| (f.glyph_width(&font, '0'), f.row_height(&font)));
        let char_w = char_w.max(1.0);
        let row_h = (font_h * 1.35).round();

        let gutter_w = if cfg.line_numbers {
            let digits = doc.line_count().to_string().len().max(3) as f32;
            (digits + 2.0) * char_w * 0.9 + 8.0
        } else {
            8.0
        };
        let hbar = if cfg.wrap { 0.0 } else { HBAR_H };
        let gutter = Rect::from_min_max(rect.min, Pos2::new(rect.left() + gutter_w, rect.bottom()));
        let vbar = Rect::from_min_max(Pos2::new(rect.right() - SCROLLBAR_W, rect.top()), rect.max);
        let text_rect =
            Rect::from_min_max(Pos2::new(gutter.right(), rect.top()), Pos2::new(vbar.left(), rect.bottom() - hbar));
        let hbar_rect =
            Rect::from_min_max(Pos2::new(text_rect.left(), text_rect.bottom()), Pos2::new(vbar.left(), rect.bottom()));
        let wrap_cols = (((text_rect.width() - 2.0 * PAD) / char_w).floor() as usize).max(8);
        self.layout = Some(Layout { wrap: cfg.wrap.then_some(wrap_cols) });
        self.viewport_rows = ((text_rect.height() / row_h).floor() as usize).max(1);
        let view_w = text_rect.width() - 2.0 * PAD;
        if cfg.wrap {
            self.hscroll = 0.0;
        }
        self.style =
            Some(Style { font: font.clone(), char_w, palette: *p, kind: cfg.kind, view_w, hscroll: self.hscroll });
        let now = ui.input(|i| i.time);

        // ---- focus
        if self.request_focus || resp.clicked() || resp.drag_started() {
            resp.request_focus();
            self.request_focus = false;
        }
        let focused = resp.has_focus();
        out.focused = focused;
        if focused {
            ui.memory_mut(|m| {
                m.set_focus_lock_filter(
                    id,
                    EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: false },
                )
            });
        }
        if !focused || cfg.read_only {
            self.preedit.clear();
        }

        // ---- keyboard, IME and clipboard
        let sel_before = self.sel;
        let version_before = doc.version();
        if focused {
            let events = ui.input(|i| i.events.clone());
            for ev in &events {
                if matches!(
                    ev,
                    Event::Text(_) | Event::Key { .. } | Event::Ime(_) | Event::Paste(_) | Event::Copy | Event::Cut
                ) {
                    debug_input(format_args!("{ev:?} preedit={:?}", self.preedit));
                }
            }
            for ev in events {
                let composing = !self.preedit.is_empty();
                match ev {
                    Event::Ime(ime) if !cfg.read_only => self.handle_ime(doc, ime),
                    // While an input method is composing, keys belong to it.
                    Event::Text(_) | Event::Key { .. } if composing => {}
                    Event::Text(t) if !cfg.read_only => {
                        if !t.is_empty() && t.chars().all(|c| !c.is_control()) {
                            let kind = if t.chars().count() == 1 { EditKind::Typing } else { EditKind::Other };
                            self.replace_selection(doc, t.as_bytes(), kind);
                        }
                    }
                    Event::Paste(t) if !cfg.read_only => self.insert(doc, &t),
                    Event::Copy => match self.selected_text(doc) {
                        Ok(t) if !t.is_empty() => ctx.copy_text(t),
                        Ok(_) => {}
                        Err(e) => out.message = Some(e),
                    },
                    Event::Cut if !cfg.read_only => match self.cut(doc) {
                        Ok(t) if !t.is_empty() => ctx.copy_text(t),
                        Ok(_) => {}
                        Err(e) => out.message = Some(e),
                    },
                    Event::Key { key, pressed: true, modifiers, .. } => {
                        self.key(&ctx, doc, key, modifiers, cfg.read_only);
                    }
                    _ => {}
                }
            }
        }
        if self.sel != sel_before || doc.version() != version_before {
            self.last_activity = now;
            self.scroll_to_cursor = true;
        }

        // ---- wheel scrolling
        if ui.rect_contains_pointer(rect) {
            let (delta, ctrl) = ui.input(|i| (i.smooth_scroll_delta(), i.modifiers.command));
            if !ctrl {
                self.scroll_accum -= delta.y;
                let steps = (self.scroll_accum / row_h).trunc();
                if steps != 0.0 {
                    self.scroll_accum -= steps * row_h;
                    for _ in 0..steps.abs() as usize {
                        let next =
                            if steps > 0.0 { self.next_row(doc, self.top) } else { self.prev_row(doc, self.top) };
                        match next {
                            Some(t) => self.top = t,
                            None => break,
                        }
                    }
                }
                if !cfg.wrap {
                    self.hscroll = (self.hscroll - delta.x).max(0.0);
                }
            }
        }

        // ---- keep the cursor in view
        self.clamp_top(doc);
        if self.center_on_cursor || self.scroll_to_cursor {
            let crow = self.row_of_pos(doc, self.cursor());
            let rows = self.viewport_rows;
            if self.center_on_cursor {
                self.top = self.walk_back(doc, crow, rows / 3);
            } else if crow < self.top {
                self.top = crow;
            } else if self.rows_between(doc, self.top, crow, rows.saturating_sub(1)).is_none() {
                self.top = self.walk_back(doc, crow, rows.saturating_sub(1));
            }
            if !cfg.wrap {
                let x = self.x_of_pos(&ctx, doc, self.cursor());
                let margin = 8.0 * char_w;
                if x < self.hscroll {
                    self.hscroll = (x - margin).max(0.0);
                } else if x > self.hscroll + view_w - 2.0 * char_w {
                    self.hscroll = x - view_w + margin;
                }
            }
            self.clamp_top(doc);
        }
        if let Some(st) = &mut self.style {
            st.hscroll = self.hscroll;
        }

        // ---- visible rows (shaped)
        let mut rows: Vec<RowView> = Vec::with_capacity(self.viewport_rows + 2);
        let mut rp = Some(self.top);
        while let Some(r) = rp {
            if rows.len() > self.viewport_rows {
                break;
            }
            rows.push(self.row_view(&ctx, doc, r));
            rp = self.next_row(doc, r);
        }
        let y_of = |i: usize| text_rect.top() + i as f32 * row_h;
        let text_x0 = text_rect.left() + PAD - self.hscroll;

        // ---- mouse
        if resp.clicked() || resp.drag_started() || resp.dragged() {
            let (shift, origin) = ui.input(|i| (i.modifiers.shift, i.pointer.press_origin()));
            let pointer = if resp.drag_started() { origin } else { resp.interact_pointer_pos() };
            if let Some(pos) = pointer {
                if !self.preedit.is_empty() {
                    ui.memory_mut(|m| m.interrupt_ime());
                    self.preedit.clear();
                }
                let i = ((pos.y - text_rect.top()) / row_h).floor();
                let x = pos.x - text_x0;
                let p_at = if i < 0.0 {
                    let first = rows.first().map(|r| r.rp);
                    match first.and_then(|f| self.prev_row(doc, f)) {
                        Some(prev) => Some(self.row_view(&ctx, doc, prev).pos_at(x)),
                        None => rows.first().map(|r| r.pos_at(x)),
                    }
                } else {
                    rows.get(i as usize).or(rows.last()).map(|r| r.pos_at(x))
                };
                if let Some(p) = p_at {
                    if resp.triple_clicked() {
                        let line = doc.line_of(p);
                        self.sel = (doc.line_start(line), doc.line_start(line + 1).max(doc.line_end(line)));
                    } else if resp.double_clicked() {
                        self.sel = self.word_at(doc, p);
                    } else if resp.clicked() || resp.drag_started() {
                        let anchor = if shift { self.sel.0 } else { p };
                        self.sel = (anchor, p);
                    } else {
                        self.sel.1 = p;
                        if i < 0.0 || pos.y > text_rect.bottom() {
                            self.scroll_to_cursor = true;
                            ui.ctx().request_repaint();
                        }
                    }
                    self.desired_x = None;
                    self.last_activity = now;
                    doc.seal_undo_group();
                }
            }
        }

        // ---- vertical scrollbar
        let total_lines = doc.line_count();
        let vbar_resp = ui.interact(vbar, id.with("vbar"), Sense::click_and_drag());
        let track = vbar.shrink2(Vec2::new(2.0, 2.0));
        let thumb_h = (track.height() * (self.viewport_rows as f32 / total_lines as f32)).clamp(28.0, track.height());
        let frac = if total_lines > 1 { self.top.line as f64 / (total_lines - 1) as f64 } else { 0.0 };
        let thumb_y = track.top() + (track.height() - thumb_h) * frac as f32;
        let thumb = Rect::from_min_size(Pos2::new(track.left(), thumb_y), Vec2::new(track.width(), thumb_h));
        if let Some(pp) = vbar_resp.interact_pointer_pos() {
            if vbar_resp.drag_started() || vbar_resp.clicked() {
                self.vbar_grab = Some(if thumb.contains(pp) { pp.y - thumb.top() } else { thumb_h / 2.0 });
            }
            if vbar_resp.dragged() || vbar_resp.clicked() {
                let grab = self.vbar_grab.unwrap_or(thumb_h / 2.0);
                let f = ((pp.y - grab - track.top()) / (track.height() - thumb_h).max(1.0)).clamp(0.0, 1.0) as f64;
                self.top = RowPos { line: (f * (total_lines - 1) as f64).round() as u64, row: 0 };
                self.clamp_top(doc);
            }
        }
        if vbar_resp.drag_stopped() {
            self.vbar_grab = None;
        }

        // ---- paint
        painter.rect_filled(rect, CornerRadius::ZERO, p.bg);
        painter.rect_filled(gutter, CornerRadius::ZERO, p.gutter_bg);
        let (sa, sb) = self.selection();
        let cursor = self.cursor();
        let cursor_line = doc.line_of(cursor);
        let text_painter = painter.with_clip_rect(text_rect);
        let mut max_w = 0.0f32;
        let small = FontId::monospace(cfg.font_size * 0.9);
        let small_h = ctx.fonts_mut(|f| f.row_height(&small));
        let text_y = (row_h - font_h) / 2.0;
        let mut cursor_rect = None;

        for (i, r) in rows.iter().enumerate() {
            let y = y_of(i);
            if r.rp.line == cursor_line && sa == sb {
                painter.rect_filled(
                    Rect::from_min_size(Pos2::new(rect.left(), y), Vec2::new(vbar.left() - rect.left(), row_h)),
                    CornerRadius::ZERO,
                    p.current_line,
                );
            }
            max_w = max_w.max(r.width());
            let sx = |pos: u64| text_x0 + r.x_of(pos);

            // Find matches
            if let Some((needle, case)) = cfg.highlight {
                if !needle.is_empty() && r.end > r.start {
                    let bytes = doc.read(r.start..r.end);
                    let hay = if case { bytes.into_owned() } else { bytes.to_ascii_lowercase() };
                    let n = if case { needle.to_vec() } else { needle.to_ascii_lowercase() };
                    for m in memchr::memmem::find_iter(&hay, &n).take(500) {
                        let a = r.start + m as u64;
                        text_painter.rect_filled(
                            Rect::from_min_max(
                                Pos2::new(sx(a), y + 1.0),
                                Pos2::new(sx(a + n.len() as u64), y + row_h - 1.0),
                            ),
                            CornerRadius::same(2),
                            p.find_match,
                        );
                    }
                }
            }

            // Selection
            let row_end_incl = if r.last_in_line { r.end + 1 } else { r.end };
            if sa < sb && sa < row_end_incl.max(r.start + 1) && sb > r.start {
                let x0 = sx(sa.max(r.start));
                let mut x1 = sx(sb.min(r.end));
                if sb > r.end {
                    x1 = text_x0 + r.width() + if r.last_in_line { 0.6 * char_w } else { 0.0 };
                }
                if x1 > x0 {
                    text_painter.rect_filled(
                        Rect::from_min_max(Pos2::new(x0, y), Pos2::new(x1, y + row_h)),
                        CornerRadius::same(2),
                        p.selection,
                    );
                }
            }

            // Text
            text_painter.galley(Pos2::new(text_x0 + r.x0, y + text_y), r.galley.clone(), p.text);

            // Gutter
            if cfg.line_numbers {
                let label = if r.rp.row == 0 { (r.rp.line + 1).to_string() } else { "·".to_string() };
                let color = if r.rp.line == cursor_line { p.gutter_fg_active } else { p.gutter_fg };
                painter.text(
                    Pos2::new(gutter.right() - char_w, y + text_y + (font_h - small_h) * 0.75),
                    Align2::RIGHT_TOP,
                    label,
                    small.clone(),
                    color,
                );
            }

            // Cursor and IME composition
            if cursor >= r.start && (cursor < r.end || (cursor == r.end && r.last_in_line)) {
                let x = sx(cursor);
                let mut caret_x = x;
                if !self.preedit.is_empty() {
                    let job = LayoutJob::simple_singleline(self.preedit.clone(), font.clone(), p.text);
                    let g = ctx.fonts_mut(|f| f.layout_job(job));
                    let w = g.size().x;
                    let box_rect = Rect::from_min_size(Pos2::new(x, y + 1.0), Vec2::new(w + 2.0, row_h - 2.0));
                    text_painter.rect_filled(box_rect, CornerRadius::same(2), p.current_line);
                    text_painter.galley(Pos2::new(x + 1.0, y + text_y), g, p.text);
                    text_painter.line_segment(
                        [Pos2::new(x, y + row_h - 3.0), Pos2::new(x + w + 2.0, y + row_h - 3.0)],
                        Stroke::new(1.5, p.accent),
                    );
                    caret_x = x + w + 1.0;
                }
                let caret = Rect::from_min_size(Pos2::new(caret_x - 1.0, y + 2.0), Vec2::new(2.0, row_h - 4.0));
                cursor_rect = Some(caret);
                let blink_on = !self.preedit.is_empty() || ((now - self.last_activity) % 1.1) < 0.65;
                if focused && blink_on {
                    text_painter.rect_filled(caret, CornerRadius::same(1), p.cursor);
                }
            }
        }
        if focused {
            let t = 1.1 - ((now - self.last_activity) % 1.1);
            let wait = if t > 0.45 { t - 0.45 } else { t };
            ctx.request_repaint_after(Duration::from_secs_f64(wait.max(0.02)));
            if !cfg.read_only {
                // Tell the OS where text input goes, which also enables input
                // methods (IME) for languages like Chinese, Japanese, Korean.
                let to_global = ctx.layer_transform_to_global(ui.layer_id()).unwrap_or_default();
                let caret = cursor_rect.unwrap_or(Rect::from_min_size(text_rect.min, Vec2::new(2.0, row_h)));
                ctx.output_mut(|o| {
                    o.ime = Some(egui::output::IMEOutput {
                        purpose: egui::IMEPurpose::Normal,
                        rect: to_global * text_rect,
                        cursor_rect: to_global * caret,
                        should_interrupt_composition: false,
                    })
                });
            }
        }

        // Scrollbars
        painter.rect_filled(vbar, CornerRadius::ZERO, p.gutter_bg);
        let thumb_color = if vbar_resp.hovered() || vbar_resp.dragged() { p.dim } else { p.scrollbar_thumb };
        painter.rect_filled(thumb, CornerRadius::same(4), thumb_color);
        if !cfg.wrap {
            let content_w = max_w + 2.0 * PAD;
            let full_w = text_rect.width();
            painter.rect_filled(hbar_rect, CornerRadius::ZERO, p.gutter_bg);
            let max_scroll = (content_w - full_w).max(self.hscroll);
            if max_scroll > 0.0 {
                let track = hbar_rect.shrink2(Vec2::new(2.0, 2.0));
                let tw = (track.width() * full_w / (content_w.max(full_w + self.hscroll))).clamp(28.0, track.width());
                let tx = track.left() + (track.width() - tw) * (self.hscroll / max_scroll).clamp(0.0, 1.0);
                let hthumb = Rect::from_min_size(Pos2::new(tx, track.top()), Vec2::new(tw, track.height()));
                let hresp = ui.interact(hbar_rect, id.with("hbar"), Sense::click_and_drag());
                if let Some(pp) = hresp.interact_pointer_pos() {
                    if hresp.dragged() || hresp.clicked() {
                        let f = ((pp.x - tw / 2.0 - track.left()) / (track.width() - tw).max(1.0)).clamp(0.0, 1.0);
                        self.hscroll = f * max_scroll;
                    }
                }
                painter.rect_filled(hthumb, CornerRadius::same(4), p.scrollbar_thumb);
            }
        }
        painter.line_segment(
            [Pos2::new(gutter.right(), rect.top()), Pos2::new(gutter.right(), rect.bottom())],
            Stroke::new(1.0, p.current_line),
        );
        if resp.hovered() && ui.rect_contains_pointer(text_rect) {
            ctx.set_cursor_icon(egui::CursorIcon::Text);
        }
        out
    }
}

/// Log keyboard / IME / clipboard events for bug reports. Enabled by setting
/// `NOTEPAD_FAST_DEBUG_INPUT` to a file path (or to `1` for stderr).
fn debug_input(msg: std::fmt::Arguments) {
    use std::io::Write as _;
    static SINK: std::sync::OnceLock<Option<std::sync::Mutex<Box<dyn std::io::Write + Send>>>> =
        std::sync::OnceLock::new();
    let sink = SINK.get_or_init(|| {
        let target = std::env::var_os("NOTEPAD_FAST_DEBUG_INPUT")?;
        let w: Box<dyn std::io::Write + Send> = if target == "1" {
            Box::new(std::io::stderr())
        } else {
            Box::new(std::fs::OpenOptions::new().create(true).append(true).open(target).ok()?)
        };
        Some(std::sync::Mutex::new(w))
    });
    if let Some(m) = sink {
        if let Ok(mut w) = m.lock() {
            let _ = writeln!(w, "[input] {msg}");
            let _ = w.flush();
        }
    }
}

/// Move `pos` forward to the start of a UTF-8 character.
fn align_start(doc: &Document, pos: u64) -> u64 {
    let w = doc.read(pos..(pos + 4).min(doc.len()));
    pos + w.iter().take(3).take_while(|&&b| b & 0xC0 == 0x80).count() as u64
}

/// Longest valid UTF-8 prefix.
fn valid_prefix(bytes: &[u8]) -> &str {
    bytes.utf8_chunks().next().map_or("", |c| c.valid())
}

/// Longest valid UTF-8 suffix.
fn valid_suffix(bytes: &[u8]) -> &str {
    let mut tail = "";
    for chunk in bytes.utf8_chunks() {
        tail = if chunk.invalid().is_empty() { chunk.valid() } else { "" };
    }
    tail
}

pub fn human_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

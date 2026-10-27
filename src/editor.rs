//! The virtualised text view.
//!
//! Only the rows that are on screen are ever laid out or painted. The scroll
//! position is a (line, row) pair rather than a pixel offset, so a file with
//! billions of lines scrolls exactly like a file with ten.

use std::collections::HashMap;
use std::time::Duration;

use egui::text::{LayoutJob, TextFormat};
use egui::{
    Align2, Color32, CornerRadius, Event, EventFilter, FontId, Id, Key, Modifiers, Pos2, Rect, Sense, Stroke, Ui, Vec2,
};

use crate::core::layout::{Glyph, Layout, LineLayout, RowPos};
use crate::core::{Document, EditKind, Sel};
use crate::highlight::{colorize, FileKind};
use crate::theme::Palette;

/// Largest selection we will put on the clipboard.
pub const MAX_CLIPBOARD_BYTES: u64 = 256 << 20;
const PAD: f32 = 6.0;
const SCROLLBAR_W: f32 = 12.0;
const HBAR_H: f32 = 10.0;

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

#[derive(Default)]
pub struct EditorState {
    /// (anchor, cursor)
    pub sel: Sel,
    pub top: RowPos,
    pub hscroll: f32,
    desired_col: Option<usize>,
    scroll_accum: f32,
    pub scroll_to_cursor: bool,
    pub center_on_cursor: bool,
    pub request_focus: bool,
    pub viewport_rows: usize,
    pub layout: Option<Layout>,
    cache: LayoutCache,
    last_activity: f64,
    vbar_grab: Option<f32>,
}

/// One visible row.
struct VisRow {
    rp: RowPos,
    start: u64,
    end: u64,
    last_in_line: bool,
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
        self.desired_col = None;
        self.scroll_to_cursor = true;
    }

    pub fn reset(&mut self) {
        *self = EditorState { request_focus: true, ..Default::default() };
    }

    fn line(&mut self, doc: &Document, line: u64) -> LineLayout {
        let layout = self.layout.unwrap_or(Layout { wrap: None });
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
        if b < a {
            return None;
        }
        if b.line - a.line > limit as u64 {
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

    fn glyphs_of_row(&mut self, doc: &Document, rp: RowPos) -> (Vec<Glyph>, u64, u64, bool) {
        let (s, e, last) = self.row_range(doc, rp);
        (self.lay().glyphs(doc, s, e), s, e, last)
    }

    fn col_of_pos(&mut self, doc: &Document, pos: u64) -> usize {
        let rp = self.row_of_pos(doc, pos);
        let (g, ..) = self.glyphs_of_row(doc, rp);
        col_in(&g, pos)
    }

    fn pos_at_col(&mut self, doc: &Document, rp: RowPos, col: usize) -> u64 {
        let (g, _s, e, last) = self.glyphs_of_row(doc, rp);
        pos_in(&g, col, e, last)
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

    pub fn insert(&mut self, doc: &mut Document, text: &str) {
        let mut t = text.to_string();
        if doc.uses_crlf() {
            t = t.replace("\r\n", "\n").replace('\n', "\r\n");
        }
        self.replace_selection(doc, t.as_bytes(), EditKind::Other);
    }

    pub fn selected_text(&self, doc: &Document) -> Result<String, String> {
        let (a, b) = self.selection();
        if b - a > MAX_CLIPBOARD_BYTES {
            return Err(format!(
                "Selection is {} — too large for the clipboard (limit {}).",
                human_bytes(b - a),
                human_bytes(MAX_CLIPBOARD_BYTES)
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
        self.desired_col = None;
    }

    fn word_at(&self, doc: &Document, pos: u64) -> (u64, u64) {
        const LIM: u64 = 4096;
        let lo = pos.saturating_sub(LIM);
        let hi = (pos + LIM).min(doc.len());
        let bytes = doc.read(lo..hi);
        let rel = (pos - lo) as usize;
        let is_w = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
        let class = |i: usize| {
            bytes.get(i).map(|&b| {
                if is_w(b) {
                    1
                } else if b == b' ' || b == b'\t' {
                    2
                } else {
                    3
                }
            })
        };
        let Some(c) = class(rel).or_else(|| rel.checked_sub(1).and_then(class)) else { return (pos, pos) };
        let mut s = rel;
        while s > 0 && class(s - 1) == Some(c) && c != 3 {
            s -= 1;
        }
        let mut e = rel;
        while class(e) == Some(c) {
            e += 1;
            if c == 3 {
                break;
            }
        }
        (lo + s as u64, lo + e as u64)
    }

    fn word_move(&self, doc: &Document, pos: u64, forward: bool) -> u64 {
        const LIM: u64 = 4096;
        let is_w = |b: u8| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80;
        if forward {
            let hi = (pos + LIM).min(doc.len());
            let bytes = doc.read(pos..hi);
            let mut i = 0;
            if bytes.first().is_some_and(|&b| b == b'\n' || b == b'\r') {
                return doc.next_boundary(pos);
            }
            let start_w = bytes.first().is_some_and(|&b| is_w(b));
            while i < bytes.len() && is_w(bytes[i]) == start_w && bytes[i] != b'\n' && bytes[i] != b'\r' {
                i += 1;
            }
            while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b'\t') {
                i += 1;
            }
            pos + i.max(1).min(bytes.len()) as u64
        } else {
            let lo = pos.saturating_sub(LIM);
            let bytes = doc.read(lo..pos);
            let mut i = bytes.len();
            while i > 0 && (bytes[i - 1] == b' ' || bytes[i - 1] == b'\t') {
                i -= 1;
            }
            if i > 0 && (bytes[i - 1] == b'\n' || bytes[i - 1] == b'\r') {
                return if i == bytes.len() { doc.prev_boundary(pos) } else { lo + i as u64 };
            }
            let w = i > 0 && is_w(bytes[i - 1]);
            while i > 0 && is_w(bytes[i - 1]) == w && bytes[i - 1] != b'\n' && bytes[i - 1] != b'\r' {
                i -= 1;
            }
            lo + i as u64
        }
    }

    /// Handle one keyboard event. Returns true if the cursor moved or text changed.
    fn key(&mut self, doc: &mut Document, key: Key, m: Modifiers, out: &mut EditorOutput, read_only: bool) -> bool {
        let cur = self.cursor();
        let page = self.viewport_rows.max(2) - 1;
        let mut moved_to: Option<u64> = None;
        let mut keep_col = false;
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
                let col = self.desired_col.unwrap_or_else(|| self.col_of_pos(doc, cur));
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
                    self.pos_at_col(doc, rp, col)
                });
                self.desired_col = Some(col);
                keep_col = true;
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
                return true;
            }
            Key::Tab if !read_only && !m.command => {
                self.replace_selection(doc, b"\t", EditKind::Typing);
                return true;
            }
            Key::Backspace if !read_only => {
                let (a, b) = self.selection();
                if a != b {
                    self.delete_range(doc, a, b);
                } else if cur > 0 {
                    let from =
                        if m.command || m.alt { self.word_move(doc, cur, false) } else { doc.prev_boundary(cur) };
                    self.delete_range(doc, from, cur);
                }
                return true;
            }
            Key::Delete if !read_only => {
                let (a, b) = self.selection();
                if a != b {
                    self.delete_range(doc, a, b);
                } else if cur < doc.len() {
                    let to = if m.command || m.alt { self.word_move(doc, cur, true) } else { doc.next_boundary(cur) };
                    self.delete_range(doc, cur, to);
                }
                return true;
            }
            Key::A if m.command => {
                self.select_all(doc);
                return true;
            }
            Key::Escape => {
                self.sel = (cur, cur);
                return true;
            }
            _ => {}
        }
        let _ = out;
        if let Some(p) = moved_to {
            let anchor = if m.shift { self.sel.0 } else { p };
            self.sel = (anchor, p);
            if !keep_col {
                self.desired_col = None;
            }
            self.scroll_to_cursor = true;
            doc.seal_undo_group();
            return true;
        }
        false
    }

    /// Draw the editor into the remaining space of `ui` and handle input.
    pub fn show(&mut self, ui: &mut Ui, doc: &mut Document, cfg: &EditorConfig) -> EditorOutput {
        let mut out = EditorOutput::default();
        let p = cfg.palette;
        let rect = ui.available_rect_before_wrap();
        let id = Id::unique("notepad-fast-editor");
        let resp = ui.interact(rect, id, Sense::click_and_drag());
        ui.advance_cursor_after_rect(rect);
        let painter = ui.painter_at(rect);

        let font = FontId::monospace(cfg.font_size);
        let (char_w, font_h) = ui.ctx().fonts_mut(|f| (f.glyph_width(&font, '0'), f.row_height(&font)));
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

        // ---- keyboard
        let sel_before = self.sel;
        let version_before = doc.version();
        if focused {
            let events = ui.input(|i| i.events.clone());
            for ev in events {
                match ev {
                    Event::Text(t) if !cfg.read_only => {
                        if t.chars().all(|c| !c.is_control()) && !t.is_empty() {
                            let kind = if t.chars().count() == 1 { EditKind::Typing } else { EditKind::Other };
                            self.replace_selection(doc, t.as_bytes(), kind);
                        }
                    }
                    Event::Ime(egui::ImeEvent::Commit(t)) if !cfg.read_only && !t.is_empty() => {
                        self.replace_selection(doc, t.as_bytes(), EditKind::Other);
                    }
                    Event::Paste(t) if !cfg.read_only => self.insert(doc, &t),
                    Event::Copy => match self.selected_text(doc) {
                        Ok(t) if !t.is_empty() => ui.ctx().copy_text(t),
                        Ok(_) => {}
                        Err(e) => out.message = Some(e),
                    },
                    Event::Cut if !cfg.read_only => match self.cut(doc) {
                        Ok(t) if !t.is_empty() => ui.ctx().copy_text(t),
                        Ok(_) => {}
                        Err(e) => out.message = Some(e),
                    },
                    Event::Key { key, pressed: true, modifiers, .. } => {
                        self.key(doc, key, modifiers, &mut out, cfg.read_only);
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
                let col = self.col_of_pos(doc, self.cursor()) as f32;
                let x = col * char_w;
                let w = text_rect.width() - 2.0 * PAD - 2.0 * char_w;
                if x < self.hscroll {
                    self.hscroll = (x - 8.0 * char_w).max(0.0);
                } else if x > self.hscroll + w {
                    self.hscroll = x - w + 8.0 * char_w;
                }
            }
            self.clamp_top(doc);
        }
        if cfg.wrap {
            self.hscroll = 0.0;
        }

        // ---- visible rows
        let mut rows: Vec<VisRow> = Vec::with_capacity(self.viewport_rows + 2);
        let mut rp = Some(self.top);
        while let Some(r) = rp {
            if rows.len() > self.viewport_rows {
                break;
            }
            let (s, e, last) = self.row_range(doc, r);
            rows.push(VisRow { rp: r, start: s, end: e, last_in_line: last });
            rp = self.next_row(doc, r);
        }
        let y_of = |i: usize| text_rect.top() + i as f32 * row_h;
        let text_x0 = text_rect.left() + PAD - self.hscroll;

        // ---- mouse
        if resp.clicked() || resp.drag_started() || resp.dragged() {
            let (shift, origin) = ui.input(|i| (i.modifiers.shift, i.pointer.press_origin()));
            let pointer = if resp.drag_started() { origin } else { resp.interact_pointer_pos() };
            if let Some(pos) = pointer {
                let i = ((pos.y - text_rect.top()) / row_h).floor();
                let col = ((pos.x - text_x0) / char_w).round().max(0.0) as usize;
                let target = if i < 0.0 {
                    rows.first().map(|r| self.prev_row(doc, r.rp).unwrap_or(r.rp))
                } else {
                    rows.get(i as usize).or(rows.last()).map(|r| r.rp)
                };
                if let Some(trp) = target {
                    let p = self.pos_at_col(doc, trp, col);
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
                    self.desired_col = None;
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

        // ---- paint background, current line, selection, matches
        painter.rect_filled(rect, CornerRadius::ZERO, p.bg);
        painter.rect_filled(gutter, CornerRadius::ZERO, p.gutter_bg);
        let (sa, sb) = self.selection();
        let cursor = self.cursor();
        let cursor_line = doc.line_of(cursor);
        let text_painter = painter.with_clip_rect(text_rect);
        let mut chars: Vec<char> = Vec::new();
        let mut colors: Vec<Color32> = Vec::new();
        let first_col = (self.hscroll / char_w).floor() as usize;
        let vis_cols = (text_rect.width() / char_w).ceil() as usize + 2;
        let mut max_cols = 0usize;
        let small = FontId::monospace(cfg.font_size * 0.9);
        let small_h = ui.ctx().fonts_mut(|f| f.row_height(&small));

        for (i, r) in rows.iter().enumerate() {
            let y = y_of(i);
            let row_rect = Rect::from_min_size(Pos2::new(text_rect.left(), y), Vec2::new(text_rect.width(), row_h));
            if r.rp.line == cursor_line && sa == sb {
                painter.rect_filled(
                    Rect::from_min_size(Pos2::new(rect.left(), y), Vec2::new(vbar.left() - rect.left(), row_h)),
                    CornerRadius::ZERO,
                    p.current_line,
                );
            }
            let glyphs = self.lay().glyphs(doc, r.start, r.end);
            let end_col = glyphs.last().map_or(0, |g| g.col + g.width);
            max_cols = max_cols.max(end_col);
            let x_of = |col: usize| text_x0 + col as f32 * char_w;

            // Find matches
            if let Some((needle, case)) = cfg.highlight {
                if !needle.is_empty() && r.end > r.start {
                    let bytes = doc.read(r.start..r.end);
                    let hay = if case { bytes.into_owned() } else { bytes.to_ascii_lowercase() };
                    let n = if case { needle.to_vec() } else { needle.to_ascii_lowercase() };
                    for m in memchr::memmem::find_iter(&hay, &n).take(500) {
                        let a = r.start + m as u64;
                        let c0 = col_in(&glyphs, a);
                        let c1 = col_in(&glyphs, a + n.len() as u64);
                        text_painter.rect_filled(
                            Rect::from_min_max(Pos2::new(x_of(c0), y + 1.0), Pos2::new(x_of(c1), y + row_h - 1.0)),
                            CornerRadius::same(2),
                            p.find_match,
                        );
                    }
                }
            }

            // Selection
            let row_end_incl = if r.last_in_line { r.end + 1 } else { r.end };
            if sa < sb && sa < row_end_incl.max(r.start + 1) && sb > r.start {
                let c0 = col_in(&glyphs, sa.max(r.start));
                let mut c1 = col_in(&glyphs, sb.min(r.end));
                let extra = if sb > r.end && r.last_in_line { 0.6 } else { 0.0 };
                if sb > r.end && !r.last_in_line {
                    c1 = end_col;
                }
                let x0 = x_of(c0);
                let x1 = x_of(c1) + extra * char_w;
                if x1 > x0 {
                    text_painter.rect_filled(
                        Rect::from_min_max(Pos2::new(x0, y), Pos2::new(x1, y + row_h)),
                        CornerRadius::same(2),
                        p.selection,
                    );
                }
            }

            // Text
            chars.clear();
            for g in &glyphs {
                if g.ch == '\t' {
                    chars.extend(std::iter::repeat_n(' ', g.width));
                } else {
                    chars.push(g.ch);
                }
            }
            let lo = first_col.min(chars.len());
            let hi = (first_col + vis_cols).min(chars.len());
            if lo < hi {
                // Highlight a window with some leading context so tokens
                // that start off-screen are still recognised.
                let ctx_lo = lo.saturating_sub(2048);
                colorize(cfg.kind, &chars[ctx_lo..hi], p, &mut colors);
                let mut job = LayoutJob::default();
                let mut k = lo;
                while k < hi {
                    let c = colors[k - ctx_lo];
                    let mut j = k + 1;
                    while j < hi && colors[j - ctx_lo] == c {
                        j += 1;
                    }
                    let s: String = chars[k..j].iter().collect();
                    job.append(&s, 0.0, TextFormat { font_id: font.clone(), color: c, ..Default::default() });
                    k = j;
                }
                let galley = text_painter.layout_job(job);
                text_painter.galley(Pos2::new(x_of(lo), y + (row_h - font_h) / 2.0), galley, p.text);
            }
            let _ = row_rect;

            // Gutter
            if cfg.line_numbers {
                let label = if r.rp.row == 0 { (r.rp.line + 1).to_string() } else { "·".to_string() };
                let color = if r.rp.line == cursor_line { p.gutter_fg_active } else { p.gutter_fg };
                painter.text(
                    Pos2::new(gutter.right() - char_w, y + (row_h - font_h) / 2.0 + (font_h - small_h) * 0.75),
                    Align2::RIGHT_TOP,
                    label,
                    small.clone(),
                    color,
                );
            }

            // Cursor
            if focused && cursor >= r.start && (cursor < r.end || (cursor == r.end && r.last_in_line)) {
                let blink_on = ((now - self.last_activity) % 1.1) < 0.65;
                if blink_on {
                    let x = x_of(col_in(&glyphs, cursor));
                    text_painter.rect_filled(
                        Rect::from_min_size(Pos2::new(x - 1.0, y + 2.0), Vec2::new(2.0, row_h - 4.0)),
                        CornerRadius::same(1),
                        p.cursor,
                    );
                }
            }
        }
        if focused {
            let t = 1.1 - ((now - self.last_activity) % 1.1);
            let wait = if t > 0.45 { t - 0.45 } else { t };
            ui.ctx().request_repaint_after(Duration::from_secs_f64(wait.max(0.02)));
        }

        // Scrollbars
        painter.rect_filled(vbar, CornerRadius::ZERO, p.gutter_bg);
        let thumb_color = if vbar_resp.hovered() || vbar_resp.dragged() { p.dim } else { p.scrollbar_thumb };
        painter.rect_filled(thumb, CornerRadius::same(4), thumb_color);
        if !cfg.wrap {
            let content_w = max_cols as f32 * char_w + 2.0 * PAD;
            let view_w = text_rect.width();
            painter.rect_filled(hbar_rect, CornerRadius::ZERO, p.gutter_bg);
            let max_scroll = (content_w - view_w).max(self.hscroll);
            if max_scroll > 0.0 {
                let track = hbar_rect.shrink2(Vec2::new(2.0, 2.0));
                let tw = (track.width() * view_w / (content_w.max(view_w + self.hscroll))).clamp(28.0, track.width());
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
            ui.ctx().set_cursor_icon(egui::CursorIcon::Text);
        }
        out
    }
}

/// Column at which `pos` is drawn within a row's glyphs.
fn col_in(g: &[Glyph], pos: u64) -> usize {
    let i = g.partition_point(|x| x.pos < pos);
    match g.get(i) {
        Some(x) => x.col,
        None => g.last().map_or(0, |x| x.col + x.width),
    }
}

/// Document position closest to visual column `col` in a row.
fn pos_in(g: &[Glyph], col: usize, row_end: u64, last_in_line: bool) -> u64 {
    for x in g {
        if col < x.col + x.width.div_ceil(2).max(1) && col <= x.col + x.width / 2 {
            return x.pos;
        }
    }
    if last_in_line || g.is_empty() {
        row_end
    } else {
        g.last().unwrap().pos
    }
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

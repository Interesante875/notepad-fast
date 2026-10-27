//! Application shell: menus, dialogs, find/replace, status bar and file I/O.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use egui::{Align, Context, Id, Key, KeyboardShortcut, Layout, Modifiers, RichText, Ui, ViewportCommand};
use serde::{Deserialize, Serialize};

use crate::core::{load_source, Document, FindStep, Source, WrittenFile};
use crate::editor::{human_bytes, EditorConfig, EditorState};
use crate::highlight::FileKind;
use crate::theme::{self, ThemeKind};

pub const APP_NAME: &str = "notepad-fast";
const SETTINGS_KEY: &str = "notepad-fast-settings";
const EDITOR_ID: &str = "notepad-fast-editor";
const FIND_FIELD_ID: &str = "find-field";
const REPLACE_FIELD_ID: &str = "replace-field";
const MIN_FONT: f32 = 8.0;
const MAX_FONT: f32 = 48.0;
const DEFAULT_FONT: f32 = 15.0;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub theme: ThemeKind,
    pub font_size: f32,
    pub word_wrap: bool,
    pub status_bar: bool,
    pub line_numbers: bool,
    pub syntax: bool,
    pub match_case: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeKind::default(),
            font_size: DEFAULT_FONT,
            word_wrap: false,
            status_bar: true,
            line_numbers: true,
            syntax: true,
            match_case: false,
        }
    }
}

struct Loading {
    path: PathBuf,
    total: u64,
    progress: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
    rx: Receiver<std::io::Result<Option<Source>>>,
    started: Instant,
}

struct Saving {
    path: PathBuf,
    total: u64,
    progress: Arc<AtomicU64>,
    rx: Receiver<std::io::Result<WrittenFile>>,
    then: Option<Pending>,
    started: Instant,
}

struct SearchJob {
    needle: Vec<u8>,
    case: bool,
    forward: bool,
    origin: u64,
    cursor: u64,
    lo: u64,
    hi: u64,
    wrapped: bool,
    version: u64,
    started: Instant,
}

#[derive(Default)]
struct FindBar {
    open: bool,
    replace: bool,
    needle: String,
    replacement: String,
    status: String,
    focus: bool,
    job: Option<SearchJob>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Pending {
    New,
    Open(Option<PathBuf>),
    Exit,
}

pub struct NotepadApp {
    pub settings: Settings,
    pub doc: Document,
    pub editor: EditorState,
    pub path: Option<PathBuf>,
    pub kind: FileKind,
    kind_auto: bool,
    loading: Option<Loading>,
    saving: Option<Saving>,
    find: FindBar,
    goto: Option<String>,
    pub pending: Option<Pending>,
    message: Option<(String, Instant)>,
    about: bool,
    allow_close: bool,
    applied_theme: Option<ThemeKind>,
    title: String,
}

impl Default for NotepadApp {
    fn default() -> Self {
        Self::new(Settings::default())
    }
}

impl NotepadApp {
    pub fn new(settings: Settings) -> Self {
        let mut editor = EditorState::default();
        editor.request_focus = true;
        Self {
            settings,
            doc: Document::default(),
            editor,
            path: None,
            kind: FileKind::Text,
            kind_auto: true,
            loading: None,
            saving: None,
            find: FindBar::default(),
            goto: None,
            pending: None,
            message: None,
            about: false,
            allow_close: false,
            applied_theme: None,
            title: String::new(),
        }
    }

    /// Create the app from eframe, restoring persisted settings.
    pub fn from_creation_context(cc: &eframe::CreationContext<'_>, open: Option<PathBuf>) -> Self {
        let settings = cc.storage.and_then(|s| eframe::get_value(s, SETTINGS_KEY)).unwrap_or_default();
        let mut app = Self::new(settings);
        cc.egui_ctx.options_mut(|o| o.zoom_with_keyboard = false);
        if let Some(p) = open {
            app.open_path(p, &cc.egui_ctx);
        }
        app
    }

    pub fn is_loading(&self) -> bool {
        self.loading.is_some()
    }

    pub fn is_saving(&self) -> bool {
        self.saving.is_some()
    }

    /// Loading or saving: the document must not change.
    pub fn is_busy(&self) -> bool {
        self.loading.is_some() || self.saving.is_some()
    }

    pub fn status_message(&self) -> Option<&str> {
        self.message.as_ref().map(|(m, _)| m.as_str())
    }

    fn notify(&mut self, msg: impl Into<String>) {
        self.message = Some((msg.into(), Instant::now()));
    }

    // ------------------------------------------------------------ file ops

    pub fn new_file(&mut self) {
        self.cancel_loading();
        self.doc = Document::default();
        self.path = None;
        self.kind = FileKind::Text;
        self.kind_auto = true;
        self.editor.reset();
        self.find.job = None;
    }

    fn cancel_loading(&mut self) {
        if let Some(l) = self.loading.take() {
            l.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// Start opening `path` in the background (mapping + indexing).
    pub fn open_path(&mut self, path: PathBuf, ctx: &Context) {
        self.cancel_loading();
        let total = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let progress = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel();
        let (p2, c2, path2, ctx2) = (progress.clone(), cancel.clone(), path.clone(), ctx.clone());
        std::thread::Builder::new()
            .name("indexer".into())
            .spawn(move || {
                let r = load_source(&path2, &p2, &c2);
                let _ = tx.send(r);
                ctx2.request_repaint();
            })
            .expect("spawn indexer thread");
        self.loading = Some(Loading { path, total, progress, cancel, rx, started: Instant::now() });
    }

    fn poll_loading(&mut self, ctx: &Context) {
        let Some(l) = &self.loading else { return };
        match l.rx.try_recv() {
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(50));
            }
            Err(TryRecvError::Disconnected) => {
                self.loading = None;
                self.notify("Loading failed unexpectedly.");
            }
            Ok(res) => {
                let l = self.loading.take().unwrap();
                match res {
                    Ok(Some(src)) => {
                        let elapsed = l.started.elapsed();
                        self.doc = Document::from_source(src);
                        self.kind = FileKind::from_path(&l.path);
                        self.kind_auto = true;
                        self.path = Some(l.path);
                        self.editor.reset();
                        self.find.job = None;
                        self.notify(format!(
                            "Opened {} · {} lines in {:.2}s",
                            human_bytes(self.doc.len()),
                            group_digits(self.doc.line_count()),
                            elapsed.as_secs_f64()
                        ));
                    }
                    Ok(None) => self.notify("Opening cancelled."),
                    Err(e) => self.notify(format!("Could not open {}: {e}", l.path.display())),
                }
            }
        }
    }

    fn open_dialog(&mut self, ctx: &Context) {
        let mut dlg = rfd::FileDialog::new()
            .add_filter(
                "Text files",
                &["txt", "json", "jsonl", "ndjson", "md", "markdown", "log", "csv", "tsv", "xml", "yaml", "yml"],
            )
            .add_filter("All files", &["*"]);
        if let Some(dir) = self.path.as_ref().and_then(|p| p.parent()) {
            dlg = dlg.set_directory(dir);
        }
        if let Some(p) = dlg.pick_file() {
            self.open_path(p, ctx);
        }
    }

    /// Save to the current path (or ask for one). Returns true if a save
    /// was started; `then` runs after it completes successfully.
    pub fn save(&mut self, then: Option<Pending>) -> bool {
        match self.path.clone() {
            Some(p) => self.save_to(&p, then),
            None => self.save_as(then),
        }
    }

    pub fn save_as(&mut self, then: Option<Pending>) -> bool {
        let name = self.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
        let mut dlg = rfd::FileDialog::new().set_file_name(name.unwrap_or_else(|| "Untitled.txt".into()));
        if let Some(dir) = self.path.as_ref().and_then(|p| p.parent()) {
            dlg = dlg.set_directory(dir);
        }
        match dlg.save_file() {
            Some(p) => self.save_to(&p, then),
            None => false,
        }
    }

    /// Start writing the document to `path` on a background thread.
    pub fn save_to(&mut self, path: &Path, then: Option<Pending>) -> bool {
        if self.is_busy() {
            return false;
        }
        let snapshot = self.doc.snapshot();
        let total = snapshot.len();
        let progress = Arc::new(AtomicU64::new(0));
        let (tx, rx) = mpsc::channel();
        let (p2, path2) = (progress.clone(), path.to_path_buf());
        std::thread::Builder::new()
            .name("saver".into())
            .spawn(move || {
                let res = snapshot.write_temp(&path2, &p2, &AtomicBool::new(false));
                // Release our reference to the old file mapping before the UI
                // thread renames over it (Windows refuses while it is mapped).
                drop(snapshot);
                let _ = tx.send(res);
            })
            .expect("spawn saver thread");
        self.saving = Some(Saving { path: path.to_path_buf(), total, progress, rx, then, started: Instant::now() });
        true
    }

    fn poll_saving(&mut self, ctx: &Context) {
        let Some(sv) = &self.saving else { return };
        let res = match sv.rx.try_recv() {
            Err(TryRecvError::Empty) => {
                ctx.request_repaint_after(Duration::from_millis(50));
                return;
            }
            Err(TryRecvError::Disconnected) => Err(std::io::Error::other("save thread exited unexpectedly")),
            Ok(r) => r,
        };
        let sv = self.saving.take().unwrap();
        let sel = self.editor.sel;
        match res.and_then(|written| self.doc.commit_save(&sv.path, written)) {
            Ok(()) => {
                self.path = Some(sv.path.clone());
                if self.kind_auto {
                    self.kind = FileKind::from_path(&sv.path);
                }
                let len = self.doc.len();
                self.editor.sel = (sel.0.min(len), sel.1.min(len));
                self.notify(format!("Saved {} in {:.2}s", human_bytes(len), sv.started.elapsed().as_secs_f64()));
                if let Some(action) = sv.then {
                    self.perform(action, ctx);
                }
            }
            Err(e) => self.notify(format!("Save failed: {e}")),
        }
    }

    /// Run `action`, first asking to save unsaved changes.
    pub fn guarded(&mut self, action: Pending, ctx: &Context) {
        if self.doc.is_modified() {
            self.pending = Some(action);
        } else {
            self.perform(action, ctx);
        }
    }

    fn perform(&mut self, action: Pending, ctx: &Context) {
        match action {
            Pending::New => self.new_file(),
            Pending::Open(None) => self.open_dialog(ctx),
            Pending::Open(Some(p)) => self.open_path(p, ctx),
            Pending::Exit => {
                self.allow_close = true;
                ctx.send_viewport_cmd(ViewportCommand::Close);
            }
        }
    }

    // ------------------------------------------------------------ editing commands

    pub fn undo(&mut self) {
        if let Some(sel) = self.doc.undo() {
            let len = self.doc.len();
            self.editor.set_selection(sel.0.min(len), sel.1.min(len));
        }
    }

    pub fn redo(&mut self) {
        if let Some(sel) = self.doc.redo() {
            let len = self.doc.len();
            self.editor.set_selection(sel.0.min(len), sel.1.min(len));
        }
    }

    pub fn insert_time_date(&mut self) {
        let s = chrono::Local::now().format("%-I:%M %p %-m/%-d/%Y").to_string();
        self.editor.insert(&mut self.doc, &s);
    }

    pub fn goto_line(&mut self, line_1based: u64) {
        let line = line_1based.saturating_sub(1).min(self.doc.line_count() - 1);
        let p = self.doc.line_start(line);
        self.editor.set_selection(p, p);
        self.editor.center_on_cursor = true;
        self.editor.request_focus = true;
    }

    fn zoom(&mut self, delta: f32) {
        self.settings.font_size = (self.settings.font_size + delta).clamp(MIN_FONT, MAX_FONT);
    }

    // ------------------------------------------------------------ find / replace

    fn open_find(&mut self, replace: bool) {
        self.find.open = true;
        self.find.replace = replace;
        self.find.focus = true;
        let (a, b) = self.editor.selection();
        if a < b && b - a < 256 {
            let t = String::from_utf8_lossy(&self.doc.read(a..b)).into_owned();
            if !t.contains('\n') {
                self.find.needle = t;
            }
        }
    }

    /// Start a (possibly long-running) search from the current selection.
    pub fn find_next(&mut self, forward: bool) {
        if self.find.needle.is_empty() {
            self.open_find(false);
            return;
        }
        let (a, b) = self.editor.selection();
        let len = self.doc.len();
        let origin = if forward { b } else { a };
        let needle = self.find.needle.as_bytes().to_vec();
        let (lo, hi) = if forward { (origin, len) } else { (0, origin) };
        self.find.job = Some(SearchJob {
            needle,
            case: self.settings.match_case,
            forward,
            origin,
            cursor: if forward { lo } else { hi },
            lo,
            hi,
            wrapped: false,
            version: self.doc.version(),
            started: Instant::now(),
        });
        self.find.status = String::new();
        self.step_search(Duration::from_millis(12));
    }

    /// Advance the running search for up to `budget` wall-clock time.
    fn step_search(&mut self, budget: Duration) {
        let Some(job) = self.find.job.as_mut() else { return };
        if job.version != self.doc.version() {
            self.find.job = None;
            return;
        }
        let deadline = Instant::now() + budget;
        let n = job.needle.len() as u64;
        let len = self.doc.len();
        loop {
            match self.doc.find_step(&job.needle, job.lo, job.hi, job.cursor, job.forward, job.case, 32 << 20) {
                FindStep::Found(p) => {
                    let (a, b) = if job.forward { (p, p + n) } else { (p + n, p) };
                    let wrapped = job.wrapped;
                    self.find.job = None;
                    self.editor.set_selection(a, b);
                    self.editor.center_on_cursor = true;
                    self.find.status = if wrapped { "Wrapped around".into() } else { String::new() };
                    return;
                }
                FindStep::NotFound if !job.wrapped => {
                    job.wrapped = true;
                    if job.forward {
                        job.lo = 0;
                        job.hi = (job.origin + n - 1).min(len);
                        job.cursor = 0;
                    } else {
                        job.lo = job.origin.saturating_sub(n - 1);
                        job.hi = len;
                        job.cursor = len;
                    }
                }
                FindStep::NotFound => {
                    self.find.status = format!("Cannot find \"{}\"", String::from_utf8_lossy(&job.needle));
                    self.find.job = None;
                    return;
                }
                FindStep::Pending(c) => job.cursor = c,
            }
            if Instant::now() >= deadline {
                let done = if job.forward {
                    (job.cursor - job.lo) + if job.wrapped { len - job.origin } else { 0 }
                } else {
                    (job.hi - job.cursor) + if job.wrapped { job.origin } else { 0 }
                };
                let pct = 100.0 * done as f64 / len.max(1) as f64;
                let secs = job.started.elapsed().as_secs_f64();
                self.find.status = format!("Searching… {pct:.0}% ({secs:.1}s)");
                return;
            }
        }
    }

    /// Set the find / replace fields (used by tests and automation).
    pub fn set_find_text(&mut self, needle: &str, replacement: &str) {
        self.find.needle = needle.to_string();
        self.find.replacement = replacement.to_string();
    }

    pub fn is_searching(&self) -> bool {
        self.find.job.is_some()
    }

    pub fn find_status(&self) -> &str {
        &self.find.status
    }

    pub fn replace_one(&mut self) {
        let (a, b) = self.editor.selection();
        let sel = self.doc.read(a..b).into_owned();
        let matches = if self.settings.match_case {
            sel == self.find.needle.as_bytes()
        } else {
            sel.eq_ignore_ascii_case(self.find.needle.as_bytes())
        };
        if matches && a < b {
            let r = self.find.replacement.clone();
            self.editor.insert(&mut self.doc, &r);
        }
        self.find_next(true);
    }

    pub fn replace_all(&mut self) {
        let t = Instant::now();
        let needle = self.find.needle.clone().into_bytes();
        let rep = self.find.replacement.clone().into_bytes();
        match self.doc.replace_all(&needle, &rep, self.settings.match_case, self.editor.sel) {
            Ok(n) => {
                let len = self.doc.len();
                let (a, b) = self.editor.sel;
                self.editor.set_selection(a.min(len), b.min(len));
                self.find.status =
                    format!("Replaced {} occurrence(s) in {:.2}s", group_digits(n as u64), t.elapsed().as_secs_f64());
            }
            Err(e) => self.find.status = e,
        }
    }

    // ------------------------------------------------------------ UI

    fn handle_shortcuts(&mut self, ctx: &Context) {
        let sc = |m: Modifiers, k: Key| ctx.input_mut(|i| i.consume_shortcut(&KeyboardShortcut::new(m, k)));
        let cmd = Modifiers::COMMAND;
        let cmd_shift = Modifiers::COMMAND | Modifiers::SHIFT;
        let editor_focused = ctx.memory(|m| m.focused().is_none_or(|f| f == Id::unique(EDITOR_ID)));

        if sc(cmd_shift, Key::S) {
            self.save_as(None);
        }
        if sc(cmd, Key::S) {
            self.save(None);
        }
        if sc(cmd, Key::N) {
            self.guarded(Pending::New, ctx);
        }
        if sc(cmd, Key::O) {
            self.guarded(Pending::Open(None), ctx);
        }
        if sc(cmd, Key::F) {
            self.open_find(false);
        }
        if sc(cmd, Key::H) {
            self.open_find(true);
        }
        if sc(cmd, Key::G) {
            self.goto = Some(String::new());
        }
        if sc(Modifiers::SHIFT, Key::F3) {
            self.find_next(false);
        }
        if sc(Modifiers::NONE, Key::F3) {
            self.find_next(true);
        }
        if sc(Modifiers::NONE, Key::F5) && self.loading.is_none() {
            self.insert_time_date();
        }
        if sc(Modifiers::ALT, Key::Z) {
            self.settings.word_wrap = !self.settings.word_wrap;
        }
        if sc(cmd, Key::Plus) || sc(cmd, Key::Equals) {
            self.zoom(1.0);
        }
        if sc(cmd, Key::Minus) {
            self.zoom(-1.0);
        }
        if sc(cmd, Key::Num0) {
            self.settings.font_size = DEFAULT_FONT;
        }
        if editor_focused {
            if sc(cmd_shift, Key::Z) || sc(cmd, Key::Y) {
                self.redo();
            }
            if sc(cmd, Key::Z) {
                self.undo();
            }
        }
        if self.find.open && sc(Modifiers::NONE, Key::Escape) {
            self.find.open = false;
            self.find.job = None;
            self.editor.request_focus = true;
        }
        let zoom = ctx.input(|i| i.zoom_delta());
        if zoom != 1.0 {
            self.settings.font_size = (self.settings.font_size * zoom).clamp(MIN_FONT, MAX_FONT);
        }
    }

    fn menu_bar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.add(egui::Button::new("New").shortcut_text("Ctrl+N")).clicked() {
                    self.guarded(Pending::New, &ctx);
                }
                if ui.add(egui::Button::new("Open…").shortcut_text("Ctrl+O")).clicked() {
                    self.guarded(Pending::Open(None), &ctx);
                }
                if ui.add(egui::Button::new("Save").shortcut_text("Ctrl+S")).clicked() {
                    self.save(None);
                }
                if ui.add(egui::Button::new("Save As…").shortcut_text("Ctrl+Shift+S")).clicked() {
                    self.save_as(None);
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ctx.send_viewport_cmd(ViewportCommand::Close);
                }
            });
            ui.menu_button("Edit", |ui| {
                if ui.add_enabled(self.doc.can_undo(), egui::Button::new("Undo").shortcut_text("Ctrl+Z")).clicked() {
                    self.undo();
                }
                if ui.add_enabled(self.doc.can_redo(), egui::Button::new("Redo").shortcut_text("Ctrl+Y")).clicked() {
                    self.redo();
                }
                ui.separator();
                let has_sel = self.editor.selection().0 != self.editor.selection().1;
                if ui.add_enabled(has_sel, egui::Button::new("Cut").shortcut_text("Ctrl+X")).clicked() {
                    match self.editor.cut(&mut self.doc) {
                        Ok(t) => ctx.copy_text(t),
                        Err(e) => self.notify(e),
                    }
                }
                if ui.add_enabled(has_sel, egui::Button::new("Copy").shortcut_text("Ctrl+C")).clicked() {
                    match self.editor.selected_text(&self.doc) {
                        Ok(t) => ctx.copy_text(t),
                        Err(e) => self.notify(e),
                    }
                }
                if ui.add(egui::Button::new("Paste").shortcut_text("Ctrl+V")).clicked() {
                    self.editor.request_focus = true;
                    ctx.send_viewport_cmd(ViewportCommand::RequestPaste);
                }
                if ui.add_enabled(has_sel, egui::Button::new("Delete").shortcut_text("Del")).clicked() {
                    self.editor.insert(&mut self.doc, "");
                }
                ui.separator();
                if ui.add(egui::Button::new("Find…").shortcut_text("Ctrl+F")).clicked() {
                    self.open_find(false);
                }
                if ui.add(egui::Button::new("Find Next").shortcut_text("F3")).clicked() {
                    self.find_next(true);
                }
                if ui.add(egui::Button::new("Find Previous").shortcut_text("Shift+F3")).clicked() {
                    self.find_next(false);
                }
                if ui.add(egui::Button::new("Replace…").shortcut_text("Ctrl+H")).clicked() {
                    self.open_find(true);
                }
                if ui.add(egui::Button::new("Go To…").shortcut_text("Ctrl+G")).clicked() {
                    self.goto = Some(String::new());
                }
                ui.separator();
                if ui.add(egui::Button::new("Select All").shortcut_text("Ctrl+A")).clicked() {
                    self.editor.select_all(&self.doc);
                }
                if ui.add(egui::Button::new("Time/Date").shortcut_text("F5")).clicked() {
                    self.insert_time_date();
                }
            });
            ui.menu_button("Format", |ui| {
                ui.checkbox(&mut self.settings.word_wrap, "Word Wrap  (Alt+Z)");
                ui.separator();
                ui.horizontal(|ui| {
                    ui.label("Font size");
                    ui.add(egui::Slider::new(&mut self.settings.font_size, MIN_FONT..=MAX_FONT).step_by(1.0));
                });
            });
            ui.menu_button("View", |ui| {
                if ui.add(egui::Button::new("Zoom In").shortcut_text("Ctrl++")).clicked() {
                    self.zoom(1.0);
                }
                if ui.add(egui::Button::new("Zoom Out").shortcut_text("Ctrl+-")).clicked() {
                    self.zoom(-1.0);
                }
                if ui.add(egui::Button::new("Restore Default Zoom").shortcut_text("Ctrl+0")).clicked() {
                    self.settings.font_size = DEFAULT_FONT;
                }
                ui.separator();
                ui.checkbox(&mut self.settings.status_bar, "Status Bar");
                ui.checkbox(&mut self.settings.line_numbers, "Line Numbers");
                ui.checkbox(&mut self.settings.syntax, "Syntax Colouring");
                ui.separator();
                ui.menu_button("Theme", |ui| {
                    for t in ThemeKind::ALL {
                        ui.radio_value(&mut self.settings.theme, t, t.name());
                    }
                });
                ui.menu_button("Syntax", |ui| {
                    for k in FileKind::ALL {
                        if ui.radio(self.kind == k, k.name()).clicked() {
                            self.kind = k;
                            self.kind_auto = false;
                        }
                    }
                });
            });
            ui.menu_button("Help", |ui| {
                if ui.button("About notepad-fast").clicked() {
                    self.about = true;
                }
            });
        });
    }

    fn find_bar(&mut self, ui: &mut Ui) {
        ui.horizontal(|ui| {
            ui.add_space(4.0);
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.find.needle)
                    .hint_text("Find")
                    .desired_width(260.0)
                    .id(Id::unique(FIND_FIELD_ID)),
            );
            if self.find.focus {
                resp.request_focus();
                self.find.focus = false;
            }
            if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                let back = ui.input(|i| i.modifiers.shift);
                self.find_next(!back);
                resp.request_focus();
            }
            if ui.button("Prev").on_hover_text("Find previous (Shift+F3)").clicked() {
                self.find_next(false);
            }
            if ui.button("Next").on_hover_text("Find next (F3)").clicked() {
                self.find_next(true);
            }
            ui.checkbox(&mut self.settings.match_case, "Match case");
            if !self.find.replace && ui.small_button("Replace…").clicked() {
                self.find.replace = true;
            }
            ui.label(RichText::new(&self.find.status).italics());
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Close").on_hover_text("Close (Esc)").clicked() {
                    self.find.open = false;
                    self.find.job = None;
                    self.editor.request_focus = true;
                }
            });
        });
        if self.find.replace {
            ui.horizontal(|ui| {
                ui.add_space(4.0);
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.find.replacement)
                        .hint_text("Replace with")
                        .desired_width(260.0)
                        .id(Id::unique(REPLACE_FIELD_ID)),
                );
                if resp.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                    self.replace_one();
                }
                if ui.button("Replace").clicked() {
                    self.replace_one();
                }
                if ui.button("Replace All").clicked() {
                    self.replace_all();
                }
            });
        }
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        let p = self.settings.theme.palette();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 14.0;
            let dim = |s: String| RichText::new(s).color(p.dim).size(12.5);
            if let Some((m, t)) = &self.message {
                if t.elapsed() < Duration::from_secs(8) {
                    ui.label(RichText::new(m).color(p.accent).size(12.5));
                } else {
                    self.message = None;
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(dim(format!("{}%", (self.settings.font_size / DEFAULT_FONT * 100.0).round())));
                ui.label(dim(self.kind.name().to_string()));
                ui.label(dim("UTF-8".into()));
                ui.label(dim(if self.doc.uses_crlf() { "Windows (CRLF)" } else { "Unix (LF)" }.into()));
                ui.label(dim(human_bytes(self.doc.len())));
                ui.label(dim(format!("{} lines", group_digits(self.doc.line_count()))));
                let cur = self.editor.cursor();
                let line = self.doc.line_of(cur);
                let ls = self.doc.line_start(line);
                let col = if cur - ls <= 1 << 20 {
                    format!(
                        "Col {}",
                        group_digits(String::from_utf8_lossy(&self.doc.read(ls..cur)).chars().count() as u64 + 1)
                    )
                } else {
                    format!("Byte {}", group_digits(cur - ls + 1))
                };
                let (a, b) = self.editor.selection();
                if a != b {
                    ui.label(dim(format!("{} selected", human_bytes(b - a))));
                }
                ui.label(RichText::new(format!("Ln {}, {}", group_digits(line + 1), col)).size(12.5));
            });
        });
    }

    fn dialogs(&mut self, ctx: &Context) {
        // Unsaved changes.
        if let Some(action) = self.pending.clone() {
            let name = self.display_name();
            let mut choice = None;
            egui::Modal::new(Id::unique("unsaved")).show(ctx, |ui| {
                ui.set_width(360.0);
                ui.heading("Unsaved changes");
                ui.add_space(6.0);
                ui.label(format!("Do you want to save changes to {name}?"));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    // Enter = Save (the default button), Esc = Cancel.
                    let save = egui::Button::new(RichText::new("Save").strong());
                    if ui.add(save).clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                        choice = Some(0);
                    }
                    if ui.button("Don't Save").clicked() {
                        choice = Some(1);
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        choice = Some(2);
                    }
                });
            });
            match choice {
                Some(0) => {
                    self.pending = None;
                    self.save(Some(action));
                }
                Some(1) => {
                    self.pending = None;
                    self.perform(action, ctx);
                }
                Some(_) => self.pending = None,
                None => {}
            }
        }

        // Go to line.
        if let Some(mut input) = self.goto.take() {
            let mut keep = true;
            let mut go = None;
            egui::Modal::new(Id::unique("goto")).show(ctx, |ui| {
                ui.set_width(300.0);
                ui.heading("Go To Line");
                ui.add_space(6.0);
                let lines = self.doc.line_count();
                let r =
                    ui.add(egui::TextEdit::singleline(&mut input).hint_text(format!("1 – {}", group_digits(lines))));
                r.request_focus();
                let parsed = input.trim().replace([',', '_', ' '], "").parse::<u64>().ok();
                let enter = ui.input(|i| i.key_pressed(Key::Enter));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(parsed.is_some(), egui::Button::new("Go To")).clicked()
                        || (enter && parsed.is_some())
                    {
                        go = parsed;
                    }
                    if ui.button("Cancel").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        keep = false;
                    }
                });
            });
            if let Some(n) = go {
                self.goto_line(n);
                keep = false;
            }
            if keep {
                self.goto = Some(input);
            }
        }

        if self.about {
            egui::Modal::new(Id::unique("about")).show(ctx, |ui| {
                ui.set_width(380.0);
                ui.heading(format!("{APP_NAME} {}", env!("CARGO_PKG_VERSION")));
                ui.add_space(6.0);
                ui.label("A Notepad-style editor for ultra-large text, JSON, JSONL, Markdown and log files.");
                ui.label("Files are memory-mapped and edited through a piece table, so opening and editing multi-gigabyte files is instant.");
                ui.add_space(10.0);
                if ui.button("Close").clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                    self.about = false;
                }
            });
        }
    }

    fn display_name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Untitled".into())
    }

    fn loading_screen(&mut self, ui: &mut Ui) {
        let Some(l) = &self.loading else { return };
        let done = l.progress.load(Ordering::Relaxed);
        let frac = if l.total > 0 { done as f32 / l.total as f32 } else { 0.0 };
        let secs = l.started.elapsed().as_secs_f64().max(1e-3);
        let mut cancel = false;
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.35);
            ui.heading(format!("Opening {}", l.path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default()));
            ui.add_space(8.0);
            ui.add(egui::ProgressBar::new(frac).desired_width(420.0).show_percentage().animate(true));
            ui.add_space(4.0);
            ui.label(format!(
                "Indexing lines… {} of {} · {}/s",
                human_bytes(done),
                human_bytes(l.total),
                human_bytes((done as f64 / secs) as u64)
            ));
            ui.add_space(8.0);
            cancel = ui.button("Cancel").clicked();
        });
        if cancel {
            self.cancel_loading();
            self.notify("Opening cancelled.");
        }
    }

    fn update_title(&mut self, ctx: &Context) {
        let dirty = if self.doc.is_modified() { "● " } else { "" };
        let title = format!("{dirty}{} — {APP_NAME}", self.display_name());
        if title != self.title {
            ctx.send_viewport_cmd(ViewportCommand::Title(title.clone()));
            self.title = title;
        }
    }

    /// Draw the whole application into `ui` (the root viewport).
    pub fn show(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if self.applied_theme != Some(self.settings.theme) {
            theme::apply(&ctx, &self.settings.theme.palette());
            self.applied_theme = Some(self.settings.theme);
        }
        self.poll_loading(&ctx);
        self.poll_saving(&ctx);

        // Window close: ask about unsaved changes first.
        if ctx.input(|i| i.viewport().close_requested()) {
            if self.saving.is_some() {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                if let Some(sv) = &mut self.saving {
                    sv.then = Some(Pending::Exit);
                }
                self.notify("Closing as soon as the save finishes…");
            } else if !self.allow_close && self.doc.is_modified() {
                ctx.send_viewport_cmd(ViewportCommand::CancelClose);
                self.pending = Some(Pending::Exit);
            }
        }
        // Drag and drop.
        let dropped = ctx.input(|i| i.raw.dropped_files.first().map(|f| f.path().to_path_buf()));
        if let Some(p) = dropped.filter(|_| self.saving.is_none()) {
            self.guarded(Pending::Open(Some(p)), &ctx);
        }

        let modal_open = self.pending.is_some() || self.goto.is_some() || self.about;
        let saving = self.saving.is_some();
        if !modal_open && !saving {
            self.handle_shortcuts(&ctx);
        }
        if self.find.job.is_some() {
            self.step_search(Duration::from_millis(14));
            ctx.request_repaint();
        }

        let p = self.settings.theme.palette();
        egui::Panel::top("menu")
            .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin::symmetric(6, 3)))
            .show(ui, |ui| ui.add_enabled_ui(!saving, |ui| self.menu_bar(ui)));
        if let Some(sv) = &self.saving {
            let done = sv.progress.load(Ordering::Relaxed);
            let frac = done as f32 / sv.total.max(1) as f32;
            let label = format!(
                "Saving {}… {} of {}",
                sv.path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
                human_bytes(done),
                human_bytes(sv.total)
            );
            egui::Panel::top("saving")
                .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin::symmetric(10, 5)))
                .show(ui, |ui| ui.add(egui::ProgressBar::new(frac).text(label).animate(true)));
        }
        if self.find.open {
            egui::Panel::top("find")
                .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin::symmetric(6, 5)))
                .show(ui, |ui| ui.add_enabled_ui(!saving, |ui| self.find_bar(ui)));
        }
        if self.settings.status_bar {
            egui::Panel::bottom("status")
                .frame(egui::Frame::new().fill(p.panel).inner_margin(egui::Margin::symmetric(10, 3)))
                .show(ui, |ui| self.status_bar(ui));
        }
        // Keyboard focus returns to the text after using a menu, a button or
        // clicking anywhere outside a text field (egui drops focus on those
        // clicks, which would otherwise swallow the next shortcut or keystroke).
        if !modal_open && !ctx.any_popup_open() {
            let focused = ctx.memory(|m| m.focused());
            let in_field = focused.is_some_and(|f| f == Id::unique(FIND_FIELD_ID) || f == Id::unique(REPLACE_FIELD_ID));
            if focused != Some(Id::unique(EDITOR_ID)) && !in_field {
                self.editor.request_focus = true;
            }
        }
        egui::CentralPanel::default().frame(egui::Frame::new().fill(p.bg)).show(ui, |ui| {
            if self.loading.is_some() {
                self.loading_screen(ui);
                return;
            }
            let needle = self.find.needle.clone();
            let cfg = EditorConfig {
                font_size: self.settings.font_size,
                wrap: self.settings.word_wrap,
                line_numbers: self.settings.line_numbers,
                palette: &p,
                kind: if self.settings.syntax { self.kind } else { FileKind::Text },
                highlight: (self.find.open && !needle.is_empty())
                    .then_some((needle.as_bytes(), self.settings.match_case)),
                read_only: modal_open || saving,
            };
            let out = self.editor.show(ui, &mut self.doc, &cfg);
            if let Some(m) = out.message {
                self.notify(m);
            }
        });

        self.dialogs(&ctx);
        self.update_title(&ctx);
    }
}

impl eframe::App for NotepadApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, SETTINGS_KEY, &self.settings);
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        self.settings.theme.palette().bg.to_normalized_gamma_f32()
    }
}

/// `1234567` -> `1,234,567`
pub fn group_digits(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

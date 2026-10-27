//! Shared headless test harness: runs the real app through egui with
//! synthetic input, no window or GPU needed.
#![allow(dead_code)]

use std::time::{Duration, Instant};

use egui::{Context, Event, FullOutput, Key, Modifiers, OutputCommand, Pos2, RawInput, Rect, Vec2};
use notepad_fast::app::{NotepadApp, Settings};

pub struct Harness {
    pub ctx: Context,
    pub app: NotepadApp,
    pub time: f64,
    /// Modifier keys held during the next frames.
    pub modifiers: Modifiers,
}

impl Harness {
    pub fn new(settings: Settings) -> Self {
        let mut h =
            Harness { ctx: Context::default(), app: NotepadApp::new(settings), time: 0.0, modifiers: Modifiers::NONE };
        h.frame(vec![]);
        h.frame(vec![]);
        h
    }

    pub fn frame(&mut self, mut events: Vec<Event>) -> FullOutput {
        events.insert(0, Event::ModifiersChanged(self.modifiers));
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1000.0, 700.0))),
            time: Some(self.time),
            events,
            focused: true,
            ..Default::default()
        };
        self.time += 1.0 / 60.0;
        let app = &mut self.app;
        let mut out = self.ctx.run_ui(input, |ui| app.show(ui));
        // No GPU here: drop the font-atlas uploads instead of applying them.
        out.textures_delta.clear();
        out
    }

    pub fn key(&mut self, key: Key, modifiers: Modifiers) -> FullOutput {
        let ev = |pressed| Event::Key { key, physical_key: None, pressed, repeat: false, modifiers };
        self.frame(vec![ev(true), ev(false)])
    }

    pub fn type_text(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                self.key(Key::Enter, Modifiers::NONE);
            } else {
                self.frame(vec![Event::Text(c.to_string())]);
            }
        }
    }

    pub fn text(&self) -> String {
        String::from_utf8(self.app.doc.read(0..self.app.doc.len()).into_owned()).unwrap()
    }

    pub fn wait_loaded(&mut self) {
        let t = Instant::now();
        while self.app.is_busy() {
            assert!(t.elapsed() < Duration::from_secs(120), "loading timed out");
            std::thread::sleep(Duration::from_millis(5));
            self.frame(vec![]);
        }
    }
}

pub fn copied(out: &FullOutput) -> Option<String> {
    out.platform_output.commands.iter().find_map(|c| match c {
        OutputCommand::CopyText(t) => Some(t.clone()),
        _ => None,
    })
}

impl Harness {
    /// Screen rectangle of the caret, as reported to the OS for IME placement.
    pub fn caret(&mut self) -> Rect {
        let out = self.frame(vec![]);
        out.platform_output.ime.expect("editor should request IME input while focused").cursor_rect
    }

    pub fn press(&mut self, pos: Pos2, pressed: bool, modifiers: Modifiers) {
        self.modifiers = modifiers;
        self.frame(vec![
            Event::PointerMoved(pos),
            Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers },
        ]);
    }

    pub fn click(&mut self, pos: Pos2) {
        self.click_with(pos, Modifiers::NONE);
    }

    pub fn click_with(&mut self, pos: Pos2, modifiers: Modifiers) {
        self.press(pos, true, modifiers);
        self.press(pos, false, modifiers);
        self.modifiers = Modifiers::NONE;
        // Let the double-click window pass so separate clicks stay separate.
        self.time += 0.5;
        self.frame(vec![]);
    }

    pub fn double_click(&mut self, pos: Pos2) {
        for _ in 0..2 {
            self.press(pos, true, Modifiers::NONE);
            self.press(pos, false, Modifiers::NONE);
        }
        self.time += 0.5;
        self.frame(vec![]);
    }

    pub fn drag(&mut self, from: Pos2, to: Pos2) {
        self.press(from, true, Modifiers::NONE);
        for k in 1..=8 {
            let p = from + (to - from) * (k as f32 / 8.0);
            self.frame(vec![Event::PointerMoved(p)]);
        }
        self.press(to, false, Modifiers::NONE);
        self.time += 0.5;
        self.frame(vec![]);
    }

    /// Put the caret at `pos` and return the middle of its on-screen rect.
    pub fn screen_pos_of(&mut self, pos: u64) -> Pos2 {
        self.app.editor.set_selection(pos, pos);
        self.caret().center()
    }

    pub fn sel(&self) -> (u64, u64) {
        self.app.editor.selection()
    }

    pub fn cursor(&self) -> u64 {
        self.app.editor.cursor()
    }

    pub fn ime(&mut self, ev: egui::ImeEvent) -> FullOutput {
        self.frame(vec![Event::Ime(ev)])
    }

    pub fn load_text(&mut self, text: &str) {
        self.app.doc = notepad_fast::core::Document::from_bytes(text.as_bytes().to_vec());
        self.app.editor.reset();
        self.frame(vec![]);
        self.frame(vec![]);
    }
}

pub fn wait_ms(ms: u64) {
    std::thread::sleep(Duration::from_millis(ms));
}

pub fn now() -> Instant {
    Instant::now()
}

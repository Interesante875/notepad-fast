//! Headless UI tests: drive the real application through egui with
//! synthetic keyboard / mouse input, without opening a window.

mod common;

use std::io::Write;
use std::time::{Duration, Instant};

use common::{copied, Harness};
use egui::{Event, Key, Modifiers, Pos2, Vec2};
use notepad_fast::app::Settings;

#[test]
fn typing_editing_and_undo() {
    let mut h = Harness::new(Settings::default());
    h.type_text("hello world\nsecond line");
    assert_eq!(h.text().replace("\r\n", "\n"), "hello world\nsecond line");
    assert!(h.app.doc.is_modified());

    // Backspace x4 removes "line".
    for _ in 0..4 {
        h.key(Key::Backspace, Modifiers::NONE);
    }
    assert!(h.text().ends_with("second "));

    // Move to start of document and select the first word with shift+ctrl+right.
    h.key(Key::Home, Modifiers::COMMAND);
    h.key(Key::ArrowRight, Modifiers::COMMAND | Modifiers::SHIFT);
    let out = h.frame(vec![Event::Copy]);
    assert_eq!(copied(&out).as_deref(), Some("hello "));

    // Typing replaces the selection.
    h.type_text("bye ");
    assert!(h.text().starts_with("bye world"));

    // Undo restores, redo re-applies.
    h.key(Key::Z, Modifiers::COMMAND);
    assert!(h.text().starts_with("hello world"), "{}", h.text());
    h.key(Key::Y, Modifiers::COMMAND);
    assert!(h.text().starts_with("bye world"));

    // Select all + cut empties the document.
    h.key(Key::A, Modifiers::COMMAND);
    let out = h.frame(vec![Event::Cut]);
    assert!(copied(&out).unwrap().starts_with("bye world"));
    assert_eq!(h.text(), "");

    // Paste.
    h.frame(vec![Event::Paste("pasted\ntext".into())]);
    assert_eq!(h.text().replace("\r\n", "\n"), "pasted\ntext");
}

#[test]
fn cursor_navigation_across_lines() {
    let mut h = Harness::new(Settings::default());
    h.type_text("abc\nde\nfghij");
    h.key(Key::Home, Modifiers::COMMAND);
    h.key(Key::ArrowRight, Modifiers::NONE);
    h.key(Key::ArrowRight, Modifiers::NONE);
    h.key(Key::ArrowDown, Modifiers::NONE); // "de" -> clamps to end (col 2)
    h.key(Key::ArrowDown, Modifiers::NONE); // keeps desired col 2 on "fghij"
    h.type_text("X");
    assert_eq!(h.text().replace("\r\n", "\n"), "abc\nde\nfgXhij");
    h.key(Key::End, Modifiers::NONE);
    h.type_text("!");
    assert!(h.text().ends_with("fgXhij!"));
    h.key(Key::ArrowUp, Modifiers::NONE);
    h.key(Key::Home, Modifiers::NONE);
    h.type_text(">");
    assert_eq!(h.text().replace("\r\n", "\n"), "abc\n>de\nfgXhij!");
}

#[test]
fn word_wrap_and_settings_do_not_break_editing() {
    let settings = Settings { word_wrap: true, line_numbers: false, ..Settings::default() };
    let mut h = Harness::new(settings);
    let long = "lorem ipsum dolor sit amet ".repeat(200);
    h.frame(vec![Event::Paste(long.clone())]);
    // Move up through wrapped rows; should not panic and should stay within the line.
    for _ in 0..10 {
        h.key(Key::ArrowUp, Modifiers::NONE);
    }
    assert!(h.app.editor.cursor() < long.len() as u64);
    assert!(h.app.editor.cursor() > 0);
    h.key(Key::PageUp, Modifiers::NONE);
    h.key(Key::PageDown, Modifiers::NONE);
    h.app.settings.word_wrap = false;
    h.frame(vec![]);
    h.key(Key::End, Modifiers::NONE);
    assert_eq!(h.app.editor.cursor(), long.len() as u64);
}

fn write_jsonl(path: &std::path::Path, lines: u64) -> u64 {
    let f = std::fs::File::create(path).unwrap();
    let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
    for i in 0..lines {
        writeln!(
            w,
            r#"{{"id":{i},"level":"INFO","msg":"request handled","path":"/api/v1/items/{i}","latency_ms":{}}}"#,
            i % 997
        )
        .unwrap();
    }
    w.flush().unwrap();
    std::fs::metadata(path).unwrap().len()
}

#[test]
fn large_file_open_navigate_search_edit_save() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("big.jsonl");
    let n_lines = 2_000_000;
    let size = write_jsonl(&path, n_lines);
    assert!(size > 150_000_000, "test file should be large, got {size}");

    let mut h = Harness::new(Settings::default());
    let t = Instant::now();
    h.app.open_path(path.clone(), &h.ctx.clone());
    h.wait_loaded();
    let open_time = t.elapsed();
    eprintln!("opened {} MB in {:?}", size >> 20, open_time);
    assert_eq!(h.app.doc.len(), size);
    assert_eq!(h.app.doc.line_count(), n_lines + 1);

    // Jump to the end and back; each frame must stay fast.
    let t = Instant::now();
    h.key(Key::End, Modifiers::COMMAND);
    assert_eq!(h.app.editor.cursor(), size);
    assert!(h.app.editor.top.line > n_lines - 100);
    h.key(Key::Home, Modifiers::COMMAND);
    assert_eq!(h.app.editor.cursor(), 0);
    let mut frames = 2;
    for _ in 0..60 {
        h.key(Key::PageDown, Modifiers::NONE);
        frames += 1;
    }
    for _ in 0..60 {
        h.frame(vec![
            Event::PointerMoved(Pos2::new(500.0, 300.0)),
            Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: Vec2::new(0.0, -400.0),
                modifiers: Modifiers::NONE,
                phase: egui::TouchPhase::Move,
                source: egui::MouseWheelSource::Wheel,
            },
        ]);
        frames += 1;
    }
    let per_frame = t.elapsed() / frames;
    eprintln!("navigation: {per_frame:?} per frame");
    assert!(per_frame < Duration::from_millis(100), "frames too slow: {per_frame:?}");

    // Go to a line in the middle.
    h.app.goto_line(1_234_567);
    h.frame(vec![]);
    let cur = h.app.editor.cursor();
    assert_eq!(h.app.doc.line_of(cur), 1_234_566);
    let expect = r#"{"id":1234566,"#.to_string();
    assert!(h.app.doc.read(cur..cur + expect.len() as u64).starts_with(expect.as_bytes()));

    // Search for something near the end of the file (incremental, across frames).
    h.app.goto_line(1);
    h.frame(vec![]);
    h.app.set_find_text(r#""id":1999990,"#, "");
    let t = Instant::now();
    h.app.find_next(true);
    while h.app.is_searching() {
        h.frame(vec![]);
    }
    eprintln!("search took {:?}", t.elapsed());
    let (a, b) = h.app.editor.selection();
    assert_eq!(h.app.doc.line_of(a), 1_999_990);
    assert_eq!(&h.app.doc.read(a..b)[..], br#""id":1999990,"#);

    // Edit at the very top and the very end, then save and verify on disk.
    h.key(Key::Home, Modifiers::COMMAND);
    h.type_text("HEAD\n");
    h.key(Key::End, Modifiers::COMMAND);
    h.type_text("TAIL");
    assert!(h.app.doc.is_modified());
    let t = Instant::now();
    assert!(h.app.save_to(&path, None));
    // The UI keeps running while the file is written.
    h.type_text("ignored while saving");
    h.wait_loaded();
    eprintln!("save took {:?}", t.elapsed());
    assert!(!h.app.doc.is_modified());
    let data = std::fs::read(&path).unwrap();
    assert!(data.starts_with(b"HEAD\n{\"id\":0,"));
    assert!(data.ends_with(b"\nTAIL"));
    assert_eq!(data.len() as u64, size + 9);
    assert_eq!(h.app.doc.line_count(), n_lines + 2);
}

#[test]
fn single_huge_line_is_navigable() {
    // A 30 MB single-line JSON document (e.g. minified API dump).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("minified.json");
    let mut s = String::from("[");
    while s.len() < 30_000_000 {
        s.push_str(r#"{"name":"widget","tags":["a","b"],"price":12.5},"#);
    }
    s.push_str("{}]");
    std::fs::write(&path, &s).unwrap();
    for wrap in [false, true] {
        let mut h = Harness::new(Settings { word_wrap: wrap, ..Settings::default() });
        h.app.open_path(path.clone(), &h.ctx.clone());
        h.wait_loaded();
        assert_eq!(h.app.doc.line_count(), 1);
        let t = Instant::now();
        h.key(Key::End, Modifiers::COMMAND);
        assert_eq!(h.app.editor.cursor(), s.len() as u64);
        for _ in 0..20 {
            h.key(Key::PageUp, Modifiers::NONE);
        }
        h.key(Key::Home, Modifiers::COMMAND);
        for _ in 0..20 {
            h.key(Key::ArrowDown, Modifiers::NONE);
        }
        let per = t.elapsed() / 42;
        eprintln!("huge line (wrap={wrap}): {per:?} per frame");
        assert!(per < Duration::from_millis(100));
    }
}

#[test]
fn save_then_new_via_prompt_flow() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("note.md");
    std::fs::write(&path, "# title\n").unwrap();
    let mut h = Harness::new(Settings::default());
    h.app.open_path(path.clone(), &h.ctx.clone());
    h.wait_loaded();
    h.key(Key::End, Modifiers::COMMAND);
    h.type_text("body");
    // Save with a follow-up action: the document is replaced only after the save lands.
    assert!(h.app.save(Some(notepad_fast::app::Pending::New)));
    h.wait_loaded();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "# title\nbody");
    assert_eq!(h.text(), "");
    assert!(h.app.path.is_none());
}

#[test]
fn unsaved_changes_prompt_on_new() {
    let mut h = Harness::new(Settings::default());
    h.type_text("draft");
    h.key(Key::N, Modifiers::COMMAND);
    assert_eq!(h.app.pending, Some(notepad_fast::app::Pending::New));
    assert_eq!(h.text(), "draft", "document must not be discarded without confirmation");
}

/// Multi-gigabyte stress test. Run with:
/// `NOTEPAD_FAST_HUGE_GB=4 cargo test --release --test ui -- --ignored --nocapture`
#[test]
#[ignore]
fn huge_multi_gigabyte_file() {
    let gb: f64 = std::env::var("NOTEPAD_FAST_HUGE_GB").ok().and_then(|v| v.parse().ok()).unwrap_or(3.0);
    let dir = tempfile::tempdir_in(std::env::var("NOTEPAD_FAST_HUGE_DIR").unwrap_or_else(|_| ".".into())).unwrap();
    let path = dir.path().join("huge.log");
    let target = (gb * 1024.0 * 1024.0 * 1024.0) as u64;
    let t = Instant::now();
    {
        let mut block = Vec::new();
        for i in 0..10_000u64 {
            writeln!(
                block,
                "2026-10-27T12:00:00.{:03}Z INFO [svc] request {i} completed status=200 bytes={}",
                i % 1000,
                i * 7
            )
            .unwrap();
        }
        let mut w = std::io::BufWriter::with_capacity(8 << 20, std::fs::File::create(&path).unwrap());
        let mut written = 0;
        while written < target {
            w.write_all(&block).unwrap();
            written += block.len() as u64;
        }
        w.write_all(b"2026-10-27T23:59:59.999Z ERROR [svc] the needle at the very end\n").unwrap();
        w.flush().unwrap();
    }
    let size = std::fs::metadata(&path).unwrap().len();
    eprintln!("generated {:.2} GB in {:?}", size as f64 / (1u64 << 30) as f64, t.elapsed());

    let mut h = Harness::new(Settings::default());
    let t = Instant::now();
    h.app.open_path(path.clone(), &h.ctx.clone());
    h.wait_loaded();
    eprintln!("opened+indexed in {:?} ({} lines)", t.elapsed(), h.app.doc.line_count());

    let t = Instant::now();
    h.key(Key::End, Modifiers::COMMAND);
    h.key(Key::Home, Modifiers::COMMAND);
    h.app.goto_line(h.app.doc.line_count() / 2);
    h.frame(vec![]);
    for _ in 0..30 {
        h.key(Key::PageDown, Modifiers::NONE);
    }
    eprintln!("navigation: {:?} per frame", t.elapsed() / 33);

    h.app.goto_line(1);
    h.frame(vec![]);
    h.app.set_find_text("the needle", "");
    let t = Instant::now();
    h.app.find_next(true);
    let mut frames = 0;
    while h.app.is_searching() {
        h.frame(vec![]);
        frames += 1;
    }
    eprintln!("full-file search: {:?} over {frames} frames", t.elapsed());
    let (a, _) = h.app.editor.selection();
    assert_eq!(h.app.doc.line_of(a), h.app.doc.line_count() - 2);

    // Insert in the middle, then save.
    h.app.goto_line(h.app.doc.line_count() / 2);
    h.frame(vec![]);
    h.type_text("EDITED ");
    let t = Instant::now();
    assert!(h.app.save_to(&path, None));
    h.wait_loaded();
    eprintln!("save: {:?}", t.elapsed());
    assert_eq!(std::fs::metadata(&path).unwrap().len(), size + 7);
}

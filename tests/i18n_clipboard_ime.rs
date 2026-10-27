//! International text, IME composition, clipboard and mouse hit-testing.

mod common;

use common::{copied, Harness};
use egui::{Event, ImeEvent, Key, Modifiers, OutputCommand, Vec2, ViewportCommand};
use notepad_fast::app::Settings;

const SAMPLES: &[(&str, &str)] = &[
    ("Chinese", "你好，世界！中文输入测试"),
    ("Japanese", "こんにちは世界。カタカナ漢字"),
    ("Korean", "안녕하세요 세계"),
    ("Russian", "Привет, мир! Ёжик"),
    ("Greek", "Καλημέρα κόσμε"),
    ("Arabic", "مرحبا بالعالم"),
    ("Hebrew", "שלום עולם"),
    ("Hindi", "नमस्ते दुनिया क्षत्रिय"),
    ("Thai", "สวัสดีชาวโลก"),
    ("Vietnamese", "Xin chào thế giới, tiếng Việt"),
    ("Emoji", "Hello 👋🏽 family 👨‍👩‍👧‍👦 flag 🇲🇾 ok ✅"),
    ("Combining", "Cafe\u{301} na\u{303}o"),
];

#[test]
fn typing_every_script_is_stored_exactly() {
    for (name, s) in SAMPLES {
        let mut h = Harness::new(Settings::default());
        // Characters arrive one at a time (as from a keyboard layout)...
        for c in s.chars() {
            h.frame(vec![Event::Text(c.to_string())]);
        }
        assert_eq!(h.text(), *s, "{name}: per-char typing");
        // ...or as whole strings (as from an IME commit).
        h.key(Key::A, Modifiers::COMMAND);
        h.frame(vec![Event::Text(s.to_string())]);
        assert_eq!(h.text(), *s, "{name}: string typing");
        // Undo the replacement, then undo the typing completely.
        h.key(Key::Z, Modifiers::COMMAND);
        assert_eq!(h.text(), *s, "{name}: undo replacement");
        while h.app.doc.can_undo() {
            h.key(Key::Z, Modifiers::COMMAND);
        }
        assert_eq!(h.text(), "", "{name}: undo everything");
    }
}

#[test]
fn arrow_keys_step_over_whole_user_perceived_characters() {
    let text = "a👨‍👩‍👧‍👦b🇲🇾ce\u{301}fक्षि\r\ng";
    let mut h = Harness::new(Settings::default());
    h.load_text(text);
    let mut stops = vec![0];
    for _ in 0..12 {
        h.key(Key::ArrowRight, Modifiers::NONE);
        stops.push(h.cursor());
    }
    let expected: Vec<u64> = {
        let mut v = vec![0usize];
        let mut acc = 0;
        for g in ["a", "👨‍👩‍👧‍👦", "b", "🇲🇾", "c", "e\u{301}", "f", "क्षि", "\r\n", "g"]
        {
            acc += g.len();
            v.push(acc);
        }
        v.into_iter().map(|x| x as u64).collect()
    };
    assert_eq!(&stops[..expected.len()], &expected[..]);
    assert_eq!(*stops.last().unwrap(), text.len() as u64, "stops at end");
    // And back again.
    let mut back = vec![h.cursor()];
    while h.cursor() > 0 {
        h.key(Key::ArrowLeft, Modifiers::NONE);
        back.push(h.cursor());
    }
    back.reverse();
    assert_eq!(back, expected);
}

#[test]
fn backspace_removes_one_code_point_and_delete_one_cluster() {
    let mut h = Harness::new(Settings::default());
    h.load_text("ae\u{301}");
    h.key(Key::End, Modifiers::COMMAND);
    h.key(Key::Backspace, Modifiers::NONE);
    assert_eq!(h.text(), "ae", "accent removed on its own");
    h.load_text("x👍🏽y");
    h.key(Key::Home, Modifiers::COMMAND);
    h.key(Key::ArrowRight, Modifiers::NONE);
    h.key(Key::Delete, Modifiers::NONE);
    assert_eq!(h.text(), "xy", "Delete removes the whole emoji with skin tone");
}

#[test]
fn ime_composition_commit_and_cancel() {
    let mut h = Harness::new(Settings::default());
    h.type_text("A");
    // The editor must ask the OS for IME input, with a caret rectangle.
    let out = h.frame(vec![]);
    let ime = out.platform_output.ime.expect("IME must be enabled for the focused editor");
    assert!(ime.cursor_rect.width() > 0.0 && ime.rect.contains(ime.cursor_rect.center()));

    // Pinyin composition: preedit text is shown but not inserted.
    h.ime(ImeEvent::Preedit { text: "ni".into(), active_range_chars: None });
    h.ime(ImeEvent::Preedit { text: "nihao".into(), active_range_chars: Some(0..5) });
    assert_eq!(h.text(), "A");
    assert_eq!(h.app.editor.preedit, "nihao");
    // Keys pressed while composing belong to the IME (e.g. Backspace edits the
    // composition, Enter confirms it) and must not touch the document.
    h.key(Key::Backspace, Modifiers::NONE);
    h.key(Key::Enter, Modifiers::NONE);
    h.frame(vec![Event::Text("x".into())]);
    assert_eq!(h.text(), "A");
    // The caret moves to the end of the composition so the candidate window follows it.
    let caret_before = h.caret();
    h.ime(ImeEvent::Preedit { text: "nihaoma".into(), active_range_chars: None });
    assert!(h.caret().left() > caret_before.left());

    h.ime(ImeEvent::Commit("你好吗".into()));
    assert_eq!(h.text(), "A你好吗");
    assert_eq!(h.app.editor.preedit, "");
    assert_eq!(h.cursor(), "A你好吗".len() as u64);

    // Cancelled composition (empty preedit) leaves the text alone.
    h.ime(ImeEvent::Preedit { text: "ka".into(), active_range_chars: None });
    h.ime(ImeEvent::Preedit { text: String::new(), active_range_chars: None });
    assert_eq!(h.text(), "A你好吗");

    // Korean-style: commit replaces a selection.
    h.key(Key::A, Modifiers::COMMAND);
    h.ime(ImeEvent::Commit("한국어".into()));
    assert_eq!(h.text(), "한국어");

    // DeleteSurrounding (used by some IMEs to re-compose text).
    h.ime(ImeEvent::DeleteSurrounding { before_chars: 1, after_chars: 0 });
    assert_eq!(h.text(), "한국");

    // Undo works on IME input.
    h.key(Key::Z, Modifiers::COMMAND);
    assert_eq!(h.text(), "한국어");
}

#[test]
fn ime_is_not_requested_while_a_dialog_is_open() {
    let mut h = Harness::new(Settings::default());
    h.type_text("x");
    h.key(Key::N, Modifiers::COMMAND); // unsaved changes prompt
    let out = h.frame(vec![]);
    assert!(out.platform_output.ime.is_none());
    h.frame(vec![Event::Ime(ImeEvent::Commit("不".into()))]);
    assert_eq!(h.text(), "x");
}

#[test]
fn copy_cut_paste_with_unicode_and_line_endings() {
    let mut h = Harness::new(Settings::default());
    h.load_text("第一行 line one\n第二行 😀\n");
    h.key(Key::A, Modifiers::COMMAND);
    let out = h.frame(vec![Event::Copy]);
    assert_eq!(copied(&out).as_deref(), Some("第一行 line one\n第二行 😀\n"));

    // Select the second line with shift+down and cut it.
    h.key(Key::Home, Modifiers::COMMAND);
    h.key(Key::ArrowDown, Modifiers::NONE);
    h.key(Key::ArrowDown, Modifiers::SHIFT);
    let out = h.frame(vec![Event::Cut]);
    assert_eq!(copied(&out).as_deref(), Some("第二行 😀\n"));
    assert_eq!(h.text(), "第一行 line one\n");

    // Paste it back at the top.
    h.key(Key::Home, Modifiers::COMMAND);
    h.frame(vec![Event::Paste("第二行 😀\n".into())]);
    assert_eq!(h.text(), "第二行 😀\n第一行 line one\n");

    // Pasting Windows text into an LF file normalises to LF, and into a
    // CRLF file normalises to CRLF.
    h.load_text("x\n");
    h.frame(vec![Event::Paste("a\r\nb\r\n".into())]);
    assert_eq!(h.text(), "a\nb\nx\n");
    h.load_text("x\r\n");
    h.frame(vec![Event::Paste("a\nb\r\nc".into())]);
    assert_eq!(h.text(), "a\r\nb\r\ncx\r\n");

    // Copy with nothing selected puts nothing on the clipboard.
    let out = h.frame(vec![Event::Copy]);
    assert!(copied(&out).is_none());
}

#[test]
fn edit_menu_paste_requests_the_clipboard() {
    let mut h = Harness::new(Settings::default());
    // Simulate what the Edit ▸ Paste menu item does.
    h.app.editor.request_focus = true;
    h.ctx.send_viewport_cmd(ViewportCommand::RequestPaste);
    let out = h.frame(vec![]);
    let cmds = &out.viewport_output.values().next().unwrap().commands;
    assert!(cmds.contains(&ViewportCommand::RequestPaste));
    // The integration then delivers a Paste event, which must land in the editor.
    h.frame(vec![Event::Paste("from menu".into())]);
    assert_eq!(h.text(), "from menu");
}

#[test]
fn clicking_lands_on_the_character_under_the_mouse() {
    for text in ["plain ascii text here", "中文字符点击测试一下", "mixed 混合 text 😀 here", "tabs\there\tok"]
    {
        let mut h = Harness::new(Settings::default());
        h.load_text(text);
        // Every character boundary: put the caret there, read where it is
        // drawn, click a little to the right of that spot, and expect to land
        // on the same boundary.
        let mut boundaries = vec![0u64];
        let mut p = 0;
        while p < text.len() as u64 {
            p = h.app.doc.next_boundary(p);
            boundaries.push(p);
        }
        for &b in &boundaries {
            let at = h.screen_pos_of(b);
            h.app.editor.set_selection(0, 0);
            h.frame(vec![]);
            h.click(at + Vec2::new(1.0, 0.0));
            assert_eq!(h.cursor(), b, "{text:?}: click at boundary {b}");
        }
    }
}

#[test]
fn double_click_selects_words_in_any_script_and_drag_selects() {
    let mut h = Harness::new(Settings::default());
    h.load_text("hello wörld Привет 世界");
    let start = "hello ".len() as u64;
    let p = h.screen_pos_of(start + 2);
    h.double_click(p);
    assert_eq!(h.sel(), (start, start + "wörld".len() as u64));
    let rus = "hello wörld ".len() as u64;
    let p = h.screen_pos_of(rus + 4);
    h.double_click(p);
    assert_eq!(h.sel(), (rus, rus + "Привет".len() as u64));

    // Drag from the start of "wörld" to the end of "Привет".
    let a = h.screen_pos_of(start);
    let b = h.screen_pos_of(rus + "Привет".len() as u64);
    h.app.editor.set_selection(0, 0);
    h.drag(a + Vec2::new(1.0, 0.0), b + Vec2::new(1.0, 0.0));
    assert_eq!(h.sel(), (start, rus + "Привет".len() as u64));
    let out = h.frame(vec![Event::Copy]);
    assert_eq!(copied(&out).as_deref(), Some("wörld Привет"));

    // Shift+click extends the selection.
    h.app.editor.set_selection(0, 0);
    let end = h.screen_pos_of(5);
    h.app.editor.set_selection(0, 0);
    h.frame(vec![]);
    h.click_with(end + Vec2::new(1.0, 0.0), Modifiers::SHIFT);
    assert_eq!(h.sel(), (0, 5));
}

#[test]
fn up_down_keep_the_visual_column_across_scripts() {
    let mut h = Harness::new(Settings::default());
    h.load_text("abcdefghij\n中文中文中文\nabcdefghij");
    let line1 = "abcdefghij\n".len() as u64;
    let line2 = line1 + "中文中文中文\n".len() as u64;
    let cjk_w = h.screen_pos_of(line1 + 3).x - h.screen_pos_of(line1).x;
    assert!(cjk_w > 0.0);

    h.app.editor.set_selection(4, 4); // after "abcd"
    let x0 = h.caret().left();
    h.key(Key::ArrowDown, Modifiers::NONE);
    let x1 = h.caret().left();
    assert!(h.cursor() >= line1 && h.cursor() < line2);
    assert!((x1 - x0).abs() <= cjk_w / 2.0 + 1.0, "nearest CJK boundary: {x0} -> {x1} (char {cjk_w})");
    // The remembered x brings us back to exactly column 4.
    h.key(Key::ArrowDown, Modifiers::NONE);
    assert_eq!(h.cursor(), line2 + 4);
    h.key(Key::ArrowUp, Modifiers::NONE);
    h.key(Key::ArrowUp, Modifiers::NONE);
    assert_eq!(h.cursor(), 4);
}

#[test]
fn word_wrap_with_cjk_and_emoji_keeps_clusters_together() {
    let line = "中文😀".repeat(200);
    let mut h = Harness::new(Settings { word_wrap: true, ..Settings::default() });
    h.load_text(&line);
    // Walk the whole line with the arrow keys; every stop must be a cluster boundary.
    let mut p = 0;
    let mut steps = 0;
    while p < line.len() as u64 {
        h.key(Key::ArrowRight, Modifiers::NONE);
        assert!(h.cursor() > p);
        assert!(line.is_char_boundary(h.cursor() as usize));
        p = h.cursor();
        steps += 1;
    }
    assert_eq!(steps, 600);
    // Moving up through wrapped rows reaches the start.
    for _ in 0..200 {
        h.key(Key::ArrowUp, Modifiers::NONE);
    }
    assert_eq!(h.cursor(), 0);
}

#[test]
fn find_and_replace_unicode() {
    let mut h = Harness::new(Settings::default());
    h.load_text("错误: disk full\nERROR: 错误 again\n");
    h.app.set_find_text("错误", "警告");
    h.app.find_next(true);
    while h.app.is_searching() {
        h.frame(vec![]);
    }
    assert_eq!(h.sel(), (0, "错误".len() as u64));
    h.app.find_next(true);
    let second = "错误: disk full\nERROR: ".len() as u64;
    assert_eq!(h.sel(), (second, second + "错误".len() as u64));
    h.app.replace_all();
    assert_eq!(h.text(), "警告: disk full\nERROR: 警告 again\n");
    // Case-insensitive ASCII search still works next to unicode.
    h.app.set_find_text("error", "");
    h.app.find_next(true);
    assert_eq!(h.sel(), ("警告: disk full\n".len() as u64, "警告: disk full\nERROR".len() as u64));
}

#[test]
fn files_round_trip_byte_for_byte() {
    let dir = tempfile::tempdir().unwrap();
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("bom.txt", b"\xEF\xBB\xBFwith BOM \xE4\xB8\xAD\r\nline2\r\n".to_vec()),
        ("crlf.log", b"a\r\nb\r\nc".to_vec()),
        ("latin1.txt", b"caf\xe9 \xff\xfe broken utf8\n".to_vec()),
        ("nul.bin", b"bin\x00ary\x01\x02\n".to_vec()),
        ("noeol.md", "# 标题\n内容".as_bytes().to_vec()),
    ];
    for (name, bytes) in cases {
        let path = dir.path().join(name);
        std::fs::write(&path, &bytes).unwrap();
        let mut h = Harness::new(Settings::default());
        h.app.open_path(path.clone(), &h.ctx.clone());
        h.wait_loaded();
        // Type a character at the end and delete it again, then save.
        h.key(Key::End, Modifiers::COMMAND);
        h.type_text("é");
        h.key(Key::Backspace, Modifiers::NONE);
        assert!(h.app.save_to(&path, None));
        h.wait_loaded();
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "{name} changed after edit + save");
        // Reopen and make sure the editor still shows it.
        h.app.open_path(path.clone(), &h.ctx.clone());
        h.wait_loaded();
        assert_eq!(h.app.doc.len(), bytes.len() as u64);
    }
}

#[test]
fn all_scripts_render_without_panicking_in_every_mode() {
    let all: String = SAMPLES.iter().map(|(_, s)| *s).collect::<Vec<_>>().join("\n");
    for wrap in [false, true] {
        for kind in notepad_fast::highlight::FileKind::ALL {
            let mut h = Harness::new(Settings { word_wrap: wrap, ..Settings::default() });
            h.load_text(&all);
            h.app.kind = kind;
            h.key(Key::End, Modifiers::COMMAND);
            for _ in 0..30 {
                h.key(Key::ArrowLeft, Modifiers::SHIFT);
                h.key(Key::ArrowUp, Modifiers::NONE);
            }
            let out = h.frame(vec![]);
            // Something was painted.
            assert!(!out.shapes.is_empty());
            let _ = OutputCommand::CopyText(String::new());
        }
    }
}

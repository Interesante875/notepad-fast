//! Property-style tests: the piece table must always agree with a plain
//! `Vec<u8>` model, across edits, undo/redo, search and save.

use notepad_fast::core::{Document, EditKind, FindStep};

/// Tiny deterministic PRNG so tests need no extra dependencies.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next() % n
        }
    }
}

fn check(doc: &Document, model: &[u8]) {
    assert_eq!(doc.len(), model.len() as u64);
    assert_eq!(&doc.read(0..doc.len())[..], model);
    let nl = model.iter().filter(|&&b| b == b'\n').count() as u64;
    assert_eq!(doc.total_newlines(), nl);
    // Every line start must match the model.
    let mut starts = vec![0u64];
    starts.extend(model.iter().enumerate().filter(|(_, &b)| b == b'\n').map(|(i, _)| i as u64 + 1));
    for (line, &s) in starts.iter().enumerate() {
        assert_eq!(doc.line_start(line as u64), s, "line_start({line})");
        assert_eq!(doc.line_of(s), line as u64, "line_of({s})");
    }
}

fn random_text(rng: &mut Rng) -> Vec<u8> {
    let alphabet: &[&[u8]] = &[b"a", b"b", b" ", b"\n", b"\r\n", "é".as_bytes(), "日".as_bytes(), b"{\"k\":1}"];
    (0..rng.below(6) + 1).flat_map(|_| alphabet[rng.below(alphabet.len() as u64) as usize].to_vec()).collect()
}

#[test]
fn random_edits_match_model_with_undo_redo() {
    let mut base = Vec::new();
    for i in 0..3000 {
        base.extend_from_slice(format!("{{\"id\":{i},\"msg\":\"hello world\"}}\n").as_bytes());
    }
    let mut doc = Document::from_bytes(base.clone());
    let mut model = base.clone();
    let mut history = vec![model.clone()];
    let mut rng = Rng(0x9E3779B97F4A7C15);
    for step in 0..600 {
        let len = model.len() as u64;
        let a = rng.below(len + 1);
        let b = (a + rng.below(20)).min(len);
        let text = if rng.below(4) == 0 { Vec::new() } else { random_text(&mut rng) };
        let kind = if rng.below(2) == 0 { EditKind::Typing } else { EditKind::Other };
        if a == b && text.is_empty() {
            continue;
        }
        doc.edit(a..b, &text, kind, (a, a), (a, a + text.len() as u64));
        model.splice(a as usize..b as usize, text.iter().copied());
        if step % 50 == 0 {
            check(&doc, &model);
        }
        history.push(model.clone());
    }
    check(&doc, &model);
    assert!(doc.is_modified());
    // Undo everything, then redo everything.
    while doc.undo().is_some() {}
    check(&doc, &base);
    assert!(!doc.is_modified());
    while doc.redo().is_some() {}
    check(&doc, &model);
}

#[test]
fn typing_coalesces_into_one_undo_step() {
    let mut doc = Document::from_bytes("hello\n");
    for (pos, c) in (5..).zip(" world".bytes()) {
        doc.edit(pos..pos, &[c], EditKind::Typing, (pos, pos), (pos + 1, pos + 1));
    }
    assert_eq!(&doc.read(0..doc.len())[..], b"hello world\n");
    assert!(doc.piece_count() <= 3, "typing should extend one piece, got {}", doc.piece_count());
    assert_eq!(doc.undo(), Some((5, 5)));
    assert_eq!(&doc.read(0..doc.len())[..], b"hello\n");
}

#[test]
fn boundaries_respect_utf8_and_crlf() {
    let doc = Document::from_bytes("a\r\né日\u{1F600}z");
    let mut fwd = vec![0];
    let mut p = 0;
    while p < doc.len() {
        p = doc.next_boundary(p);
        fwd.push(p);
    }
    assert_eq!(fwd, vec![0, 1, 3, 5, 8, 12, 13]);
    let mut back = vec![doc.len()];
    let mut p = doc.len();
    while p > 0 {
        p = doc.prev_boundary(p);
        back.push(p);
    }
    back.reverse();
    assert_eq!(back, fwd);
    assert_eq!(doc.line_content_end(0), 1);
}

#[test]
fn search_forward_backward_and_case_insensitive() {
    let mut text = "x".repeat(200_000);
    text.push_str("Needle");
    text.push_str(&"y".repeat(200_000));
    text.push_str("needle");
    let doc = Document::from_bytes(text);
    assert_eq!(doc.find(b"Needle", 0, true, true), Some(200_000));
    assert_eq!(doc.find(b"needle", 0, true, true), Some(400_006));
    assert_eq!(doc.find(b"NEEDLE", 0, true, false), Some(200_000));
    assert_eq!(doc.find(b"needle", doc.len(), false, false), Some(400_006));
    assert_eq!(doc.find(b"needle", 400_000, false, false), Some(200_000));
    assert_eq!(doc.find(b"absent", 0, true, true), None);
    // A small budget must still find matches straddling window borders.
    let mut cur = 0;
    let found = loop {
        match doc.find_step(b"Needle", 0, doc.len(), cur, true, true, 1) {
            FindStep::Pending(c) => cur = c,
            other => break other,
        }
    };
    assert_eq!(found, FindStep::Found(200_000));
}

#[test]
fn search_spans_piece_boundaries() {
    let mut doc = Document::from_bytes("hello world");
    doc.edit(5..5, b"XX", EditKind::Other, (5, 5), (7, 7));
    doc.edit(7..7, b"", EditKind::Other, (7, 7), (7, 7));
    assert_eq!(doc.find(b"oXX w", 0, true, true), Some(4));
}

#[test]
fn replace_all_and_undo() {
    let mut doc = Document::from_bytes("ERROR a\nINFO b\nERROR c\nerror d\n");
    let n = doc.replace_all(b"error", b"WARN", false, (0, 0)).unwrap();
    assert_eq!(n, 3);
    assert_eq!(&doc.read(0..doc.len())[..], b"WARN a\nINFO b\nWARN c\nWARN d\n");
    assert_eq!(doc.line_count(), 5);
    doc.undo();
    assert_eq!(&doc.read(0..doc.len())[..], b"ERROR a\nINFO b\nERROR c\nerror d\n");
}

#[test]
fn save_round_trip_and_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("data.jsonl");
    std::fs::write(&path, "{\"a\":1}\n{\"a\":2}\n").unwrap();
    let mut doc = Document::open(&path).unwrap();
    assert_eq!(doc.line_count(), 3);
    doc.edit(0..0, b"{\"a\":0}\n", EditKind::Other, (0, 0), (8, 8));
    assert!(doc.is_modified());
    doc.save(&path).unwrap();
    assert!(!doc.is_modified());
    assert_eq!(std::fs::read(&path).unwrap(), b"{\"a\":0}\n{\"a\":1}\n{\"a\":2}\n");
    assert_eq!(doc.line_count(), 4);
    assert_eq!(doc.line_start(2), 16);
    // Editing after save still works against the new mapping.
    let end = doc.len();
    doc.edit(end..end, b"tail", EditKind::Other, (end, end), (end + 4, end + 4));
    doc.save(&path).unwrap();
    assert!(std::fs::read(&path).unwrap().ends_with(b"tail"));
    // No temp files left behind.
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[test]
fn empty_documents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.txt");
    std::fs::write(&path, "").unwrap();
    let mut doc = Document::open(&path).unwrap();
    assert_eq!((doc.len(), doc.line_count(), doc.line_start(5)), (0, 1, 0));
    doc.edit(0..0, b"x", EditKind::Typing, (0, 0), (1, 1));
    doc.save(&path).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), b"x");
}

#[test]
fn undo_and_redo_survive_saves() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("notes.txt");
    std::fs::write(&path, "one\n").unwrap();
    let mut doc = Document::open(&path).unwrap();
    let read = |d: &Document| String::from_utf8(d.read(0..d.len()).into_owned()).unwrap();
    let disk = || std::fs::read_to_string(&path).unwrap();

    doc.edit(4..4, b"two\n", EditKind::Other, (4, 4), (8, 8));
    doc.save(&path).unwrap();
    assert!(!doc.is_modified());
    assert_eq!(disk(), "one\ntwo\n");

    doc.edit(8..8, b"three\n", EditKind::Other, (8, 8), (14, 14));
    doc.save(&path).unwrap();
    assert_eq!(disk(), "one\ntwo\nthree\n");

    // Undo past both saves; the document still reads the original bytes.
    assert!(doc.undo().is_some());
    assert!(doc.is_modified());
    assert!(doc.undo().is_some());
    assert_eq!(read(&doc), "one\n");
    doc.save(&path).unwrap();
    assert_eq!(disk(), "one\n");
    assert!(!doc.is_modified());

    // Redo is still available after saving.
    assert!(doc.redo().is_some());
    assert!(doc.redo().is_some());
    assert_eq!(read(&doc), "one\ntwo\nthree\n");
    assert!(doc.is_modified());
    doc.save(&path).unwrap();
    assert_eq!(disk(), "one\ntwo\nthree\n");

    // Typing right after a save starts a new undo step.
    doc.edit(14..14, b"4", EditKind::Typing, (14, 14), (15, 15));
    doc.edit(15..15, b"5", EditKind::Typing, (15, 15), (16, 16));
    doc.undo();
    assert_eq!(read(&doc), "one\ntwo\nthree\n");
    assert!(!doc.is_modified(), "back at the save point");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "no temp files left");
}

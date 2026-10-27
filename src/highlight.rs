//! Lightweight, stateless, per-row syntax colouring.
//!
//! Each visible row is coloured on its own (no whole-file parse), so
//! highlighting costs the same whether the file is 1 KB or 100 GB.

use std::path::Path;

use egui::Color32;
use serde::{Deserialize, Serialize};

use crate::theme::Palette;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FileKind {
    #[default]
    Text,
    Json,
    JsonLines,
    Markdown,
    Log,
}

impl FileKind {
    pub const ALL: [FileKind; 5] =
        [FileKind::Text, FileKind::Json, FileKind::JsonLines, FileKind::Markdown, FileKind::Log];

    pub fn from_path(path: &Path) -> Self {
        let ext = path.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
        match ext.as_str() {
            "json" | "geojson" | "har" => FileKind::Json,
            "jsonl" | "ndjson" | "jsonlines" => FileKind::JsonLines,
            "md" | "markdown" | "mdown" | "mkd" => FileKind::Markdown,
            "log" | "out" | "err" => FileKind::Log,
            _ => FileKind::Text,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            FileKind::Text => "Plain Text",
            FileKind::Json => "JSON",
            FileKind::JsonLines => "JSON Lines",
            FileKind::Markdown => "Markdown",
            FileKind::Log => "Log",
        }
    }
}

/// Fill `out` with one colour per char of `chars`.
pub fn colorize(kind: FileKind, chars: &[char], p: &Palette, out: &mut Vec<Color32>) {
    out.clear();
    out.resize(chars.len(), p.text);
    match kind {
        FileKind::Text => {}
        FileKind::Json | FileKind::JsonLines => json(chars, p, out),
        FileKind::Markdown => markdown(chars, p, out),
        FileKind::Log => log(chars, p, out),
    }
}

fn fill(out: &mut [Color32], from: usize, to: usize, c: Color32) {
    let to = to.min(out.len());
    if from < to {
        out[from..to].fill(c);
    }
}

/// End index (exclusive) of a double-quoted string starting at `i`.
fn string_end(chars: &[char], i: usize) -> usize {
    let mut j = i + 1;
    while j < chars.len() {
        match chars[j] {
            '\\' => j += 2,
            '"' => return j + 1,
            _ => j += 1,
        }
    }
    chars.len()
}

fn json(chars: &[char], p: &Palette, out: &mut [Color32]) {
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '"' => {
                let end = string_end(chars, i);
                let mut k = end;
                while k < chars.len() && chars[k].is_whitespace() {
                    k += 1;
                }
                let is_key = k < chars.len() && chars[k] == ':';
                fill(out, i, end, if is_key { p.key } else { p.string });
                i = end;
            }
            '{' | '}' | '[' | ']' | ',' | ':' => {
                out[i] = p.punct;
                i += 1;
            }
            '-' | '0'..='9' => {
                let s = i;
                i += 1;
                while i < chars.len() && matches!(chars[i], '0'..='9' | '.' | 'e' | 'E' | '+' | '-') {
                    i += 1;
                }
                fill(out, s, i, p.number);
            }
            't' | 'f' | 'n' => {
                let word: &[&str] = &["true", "false", "null"];
                let rest: String = chars[i..chars.len().min(i + 5)].iter().collect();
                if let Some(w) = word.iter().find(|w| rest.starts_with(**w)) {
                    fill(out, i, i + w.len(), p.keyword);
                    i += w.len();
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
}

fn markdown(chars: &[char], p: &Palette, out: &mut [Color32]) {
    let first = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(chars.len());
    let rest = &chars[first..];
    if rest.first() == Some(&'#') {
        fill(out, 0, chars.len(), p.heading);
        return;
    }
    if rest.starts_with(&['`', '`', '`']) || rest.starts_with(&['~', '~', '~']) {
        fill(out, 0, chars.len(), p.code);
        return;
    }
    if rest.first() == Some(&'>') {
        fill(out, 0, chars.len(), p.quote);
        return;
    }
    if rest.len() >= 3 && rest.iter().all(|&c| c == '-' || c == '*' || c == '_' || c == ' ') {
        fill(out, 0, chars.len(), p.dim);
        return;
    }
    // List markers.
    if rest.len() >= 2 && matches!(rest[0], '-' | '*' | '+') && rest[1] == ' ' {
        fill(out, first, first + 1, p.accent);
    } else {
        let digits = rest.iter().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 && rest.get(digits) == Some(&'.') {
            fill(out, first, first + digits + 1, p.accent);
        }
    }
    // Inline spans.
    let mut i = first;
    while i < chars.len() {
        match chars[i] {
            '`' => {
                let end = chars[i + 1..].iter().position(|&c| c == '`').map_or(chars.len(), |e| i + e + 2);
                fill(out, i, end, p.code);
                i = end;
            }
            '*' | '_' if chars.get(i + 1) == Some(&chars[i]) => {
                let m = chars[i];
                let end = (i + 2..chars.len().saturating_sub(1))
                    .find(|&k| chars[k] == m && chars[k + 1] == m)
                    .map_or(chars.len(), |k| k + 2);
                fill(out, i, end, p.emphasis);
                i = end;
            }
            '[' => {
                if let Some(close) = chars[i..].iter().position(|&c| c == ']') {
                    let close = i + close;
                    if chars.get(close + 1) == Some(&'(') {
                        let end = chars[close..].iter().position(|&c| c == ')').map_or(chars.len(), |e| close + e + 1);
                        fill(out, i, end, p.link);
                        i = end;
                        continue;
                    }
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
}

const LEVELS: &[(&str, u8)] = &[
    ("FATAL", 0),
    ("CRITICAL", 0),
    ("CRIT", 0),
    ("PANIC", 0),
    ("SEVERE", 0),
    ("ERROR", 0),
    ("ERR", 0),
    ("WARNING", 1),
    ("WARN", 1),
    ("NOTICE", 2),
    ("INFO", 2),
    ("DEBUG", 3),
    ("TRACE", 3),
    ("VERBOSE", 3),
];

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn log(chars: &[char], p: &Palette, out: &mut [Color32]) {
    // Leading timestamp: a prefix made of date/time characters with digits.
    let ts_end = chars.iter().position(|&c| !(c.is_ascii_digit() || "-:./TZ+, []".contains(c))).unwrap_or(chars.len());
    let digits = chars[..ts_end].iter().filter(|c| c.is_ascii_digit()).count();
    if digits >= 4 {
        let end = chars[..ts_end].iter().rposition(|c| !c.is_whitespace()).map_or(0, |e| e + 1);
        fill(out, 0, end, p.timestamp);
    }
    // First level keyword (whole word, upper case).
    let mut level = None;
    let mut i = 0;
    while i < chars.len() && level.is_none() {
        if chars[i].is_ascii_uppercase() && (i == 0 || !is_word(chars[i - 1])) {
            let end = (i..chars.len()).find(|&k| !is_word(chars[k])).unwrap_or(chars.len());
            let word: String = chars[i..end].iter().collect();
            if let Some(&(_, sev)) = LEVELS.iter().find(|(w, _)| *w == word) {
                level = Some((i, end, sev));
            }
            i = end;
        } else {
            i += 1;
        }
    }
    let mut msg_color = None;
    if let Some((s, e, sev)) = level {
        let c = [p.error, p.warn, p.info, p.debug][sev as usize];
        fill(out, s, e, c);
        msg_color = match sev {
            0 => Some(p.error.gamma_multiply(0.85)),
            3 => Some(p.dim),
            _ => None,
        };
        if let Some(mc) = msg_color {
            fill(out, e, chars.len(), mc);
        }
    }
    // Quoted strings and key=value keys.
    let mut i = ts_end;
    while i < chars.len() {
        match chars[i] {
            '"' => {
                let end = string_end(chars, i);
                fill(out, i, end, p.string);
                i = end;
            }
            '=' if i > 0 && is_word(chars[i - 1]) => {
                let s = (0..i).rev().find(|&k| !is_word(chars[k])).map_or(0, |k| k + 1);
                if msg_color.is_none() {
                    fill(out, s, i, p.key);
                }
                i += 1;
            }
            _ => i += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::NOCTURNE as P;

    fn run(kind: FileKind, s: &str) -> Vec<Color32> {
        let chars: Vec<char> = s.chars().collect();
        let mut out = Vec::new();
        colorize(kind, &chars, &P, &mut out);
        out
    }

    #[test]
    fn json_keys_strings_numbers() {
        let c = run(FileKind::Json, r#"{"id": 42, "ok": true, "s": "x"}"#);
        assert_eq!(c[1], P.key);
        assert_eq!(c[7], P.number);
        assert_eq!(c[17], P.keyword);
        assert_eq!(c[29], P.string);
        assert_eq!(c[0], P.punct);
    }

    #[test]
    fn log_levels_and_timestamps() {
        let c = run(FileKind::Log, "2024-01-02 10:00:00 ERROR boom");
        assert_eq!(c[0], P.timestamp);
        assert_eq!(c[20], P.error);
        let c = run(FileKind::Log, "2024-01-02 INFO user=bob ok");
        assert_eq!(c[11], P.info);
        assert_eq!(c[16], P.key);
        // Not a level: part of a longer word.
        let c = run(FileKind::Log, "xINFO here");
        assert_eq!(c[1], P.text);
    }

    #[test]
    fn markdown_headings_and_inline() {
        assert!(run(FileKind::Markdown, "# Title").iter().all(|&c| c == P.heading));
        let c = run(FileKind::Markdown, "- use `code` and **bold**");
        assert_eq!(c[0], P.accent);
        assert_eq!(c[7], P.code);
        assert_eq!(c[20], P.emphasis);
    }

    #[test]
    fn kind_from_extension() {
        assert_eq!(FileKind::from_path(Path::new("a/b.JSONL")), FileKind::JsonLines);
        assert_eq!(FileKind::from_path(Path::new("x.md")), FileKind::Markdown);
        assert_eq!(FileKind::from_path(Path::new("server.log")), FileKind::Log);
        assert_eq!(FileKind::from_path(Path::new("notes")), FileKind::Text);
    }
}

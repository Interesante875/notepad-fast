# notepad-fast

A Notepad-style text editor that opens **ultra-large** `.txt`, `.json`, `.jsonl`, `.md` and `.log` files
instantly — multi-gigabyte logs and million-line JSONL dumps included — with a calm, eye-friendly display.

Written in Rust with [egui](https://github.com/emilk/egui); builds for Windows, macOS and Linux (x86-64 and ARM64).

## Why it is fast

| Technique | Effect |
| --- | --- |
| **Memory-mapped files** | Opening never reads the file into RAM; the OS pages in only what you look at. |
| **Sparse newline index** (one checkpoint per 64 KiB, SIMD `memchr`) | Indexing runs at disk/memory speed in a background thread; the index for a 10 GB file is ~2.5 MB. |
| **Piece table** | Inserting or deleting anywhere is O(edits), not O(file size). The original file is never copied. |
| **Virtualised rendering** | Only the ~50 rows on screen are laid out and painted, so frame time doesn't depend on file size. |
| **Long-line chunking** | A single 1 GB line (minified JSON) is split into fixed-size rows that can be addressed in O(1). |
| **Incremental search** | Find scans in time-sliced steps across frames, so the UI stays responsive while searching gigabytes. |
| **Background save** | Saves stream to a temp file on a worker thread (with a progress bar), then atomically replace the original. |

Measured in this repo's tests (release build, Linux VM):

| File | Open + index | Frame time while scrolling | Find (whole file) |
| --- | --- | --- | --- |
| 192 MB JSONL, 2 M lines | 0.04 s | ~1 ms | 0.18 s |
| 3 GB log, 39 M lines | 0.5 s | ~1.2 ms | 2.5 s (non-blocking) |
| 30 MB single-line JSON | instant | < 1 ms | — |

## Features

Everything you expect from Notepad:

- **File:** New, Open, Save, Save As, Exit — with an *unsaved changes* prompt; drag & drop a file onto the window; `notepad-fast FILE` on the command line.
- **Edit:** Undo/Redo (typing is grouped into one step), Cut/Copy/Paste/Delete, Find, Find Next/Previous, Replace, Replace All, Go To Line, Select All, Time/Date (F5).
- **Format:** Word Wrap, font size.
- **View:** Zoom in/out/reset (also Ctrl+mouse wheel), Status Bar (line, column, selection, line count, size, line endings, encoding, file type).

Plus:

- **Every language:** text is shaped properly (CJK, Korean, Arabic and Hebrew right-to-left, Devanagari, Thai,
  Vietnamese, combining accents, emoji sequences). Missing glyphs are loaded on demand from the system fonts.
- **Input methods (IME):** Chinese, Japanese, Korean and other input methods work, with the composition shown
  inline at the cursor and the candidate window following it.
- **System clipboard:** Cut / Copy / Paste via keyboard and the Edit menu; line endings are converted to the
  file's convention on paste.
- The cursor moves by user-perceived characters (an emoji with skin tone or a letter with an accent is one step);
  Backspace removes one code point so accents can be corrected.
- Undo / redo keep working after saving.
- Line numbers and current-line highlight.
- Three low-contrast themes: **Nocturne** (dark), **Paper** (warm light) and **Sepia** (low blue light).
- Lightweight syntax colouring for JSON / JSON Lines (keys, strings, numbers, literals), logs (timestamps,
  `ERROR`/`WARN`/`INFO`/`DEBUG` levels, `key=value`, quoted strings) and Markdown (headings, lists, code, emphasis, links).
- Highlights all visible matches of the search term.
- Preserves CRLF/LF line endings and arbitrary bytes (invalid UTF-8 is shown as `�` but saved unchanged).
- Settings (theme, font size, wrap, panels) are remembered.

### Keyboard shortcuts

| Action | Keys |
| --- | --- |
| New / Open / Save / Save As | Ctrl+N / Ctrl+O / Ctrl+S / Ctrl+Shift+S |
| Undo / Redo | Ctrl+Z / Ctrl+Y (or Ctrl+Shift+Z) |
| Find / Replace / Go To | Ctrl+F / Ctrl+H / Ctrl+G |
| Find next / previous | F3 / Shift+F3 (Enter / Shift+Enter in the find box) |
| Word wrap | Alt+Z |
| Zoom | Ctrl+= / Ctrl+- / Ctrl+0 |
| Time/Date | F5 |
| Word-wise move / delete | Ctrl+←/→, Ctrl+Backspace/Delete |

On macOS, use ⌘ instead of Ctrl.

## Building

```sh
cargo run --release -- path/to/huge.log
```

### Cross-compiling

```sh
# Windows x64 from Linux
sudo apt install gcc-mingw-w64-x86-64
rustup target add x86_64-pc-windows-gnu
cargo build --release --target x86_64-pc-windows-gnu

# Linux ARM64 from Linux x64
sudo apt install gcc-aarch64-linux-gnu
rustup target add aarch64-unknown-linux-gnu
CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
  cargo build --release --target aarch64-unknown-linux-gnu

# Intel macOS from Apple-silicon macOS
rustup target add x86_64-apple-darwin
cargo build --release --target x86_64-apple-darwin
```

CI builds and tests all of these on every push; see `.github/workflows/`.

## Testing

```sh
cargo test --release                       # unit, property and headless-UI tests
NOTEPAD_FAST_HUGE_GB=4 cargo test --release --test ui -- --ignored --nocapture   # multi-GB stress test
scripts/e2e.sh native                      # real window: X11 keyboard/mouse/clipboard + real input methods
scripts/e2e.sh wine                        # the same against the Windows .exe under Wine
```

- **Headless UI tests** drive the real application through egui with synthetic input (no window needed):
  typing in a dozen scripts, grapheme-aware cursor movement, IME composition/commit/cancel, clipboard,
  mouse hit-testing on CJK/emoji text, word wrap, unicode find/replace, byte-exact round-trips (BOM, CRLF,
  invalid UTF-8, NUL bytes), and a 190 MB file.
- **End-to-end tests** (`scripts/e2e.sh`) start the actual binary in a virtual X display and use real X11 input
  (xdotool), the real system clipboard (xclip) and real input methods (ibus Pinyin, Hangul, Kana-Kanji):
  copy/cut/paste including 15–40 MB transfers, the Edit menu, undo/redo across saves, a 150 MB file, non-ASCII
  file names, the unsaved-changes prompt, and (Windows build) the native Open/Save dialogs.
  Needs `xvfb xdotool xclip imagemagick python3-xlib dbus-x11 ibus ibus-libpinyin ibus-hangul ibus-kkc` (+ `wine`).

### Reporting input problems

Set `NOTEPAD_FAST_DEBUG_INPUT` to a file path (e.g. `set NOTEPAD_FAST_DEBUG_INPUT=C:\temp\input.log` on Windows)
before starting the app; every key, text, IME and clipboard event is logged there.

## Branches

See [BRANCHING.md](BRANCHING.md). In short: features → `develop` → `staging` → `production`.

## Limitations

- Text is treated as UTF-8 (other encodings display with replacement characters but are saved byte-for-byte).
- On Linux/X11 the clipboard is limited to about 15 MB per copy (the X11 clipboard backend sends data in one
  request); larger copies show a message instead. Windows, macOS and Wayland have no such limit (tested 40 MB).
- Emoji are drawn in monochrome (egui has no colour-font support).
- On Windows, after saving over a file the original is kept as a hidden `.<name>.notepad-fast-undo-N.tmp` file
  next to it while the document is open (so undo still works); it is removed when the document is closed, or
  the next time that file is opened if the app crashed.
- Korean IME: verified with a real input method on Linux; under Wine's IME emulation one jamo can be lost when
  a syllable completes (Wine's bridge, it also happens in egui's own text fields). Not yet verified on real Windows.
- If another program truncates a file while it is open, reading the missing part may fail — the usual trade-off of memory-mapped viewers.

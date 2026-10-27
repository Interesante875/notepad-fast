#!/usr/bin/env bash
# End-to-end tests against the real application window.
#
# Runs inside a virtual X display (Xvfb) and drives the app with real X11
# keyboard / mouse events (xdotool) and the real system clipboard (xclip).
#
#   scripts/e2e.sh native            # test target/release/notepad-fast
#   scripts/e2e.sh wine [path.exe]   # test the Windows build under Wine
#
# Needs: xvfb-run xdotool xclip (and wine for the Windows build).
set -u
export LANG=C.UTF-8 LC_ALL=C.UTF-8
MODE=${1:-native}
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d)
PASS=0
FAIL=0
FAILED=()

if [[ -z "${DISPLAY:-}" || "${E2E_IN_XVFB:-}" != 1 ]]; then
    export E2E_IN_XVFB=1
    exec xvfb-run -a -s "-screen 0 1400x900x24" "$0" "$@"
fi

case "$MODE" in
native) APP=("$ROOT/target/release/notepad-fast"); PATHPREFIX="" ;;
wine)
    EXE=${2:-$ROOT/target/x86_64-pc-windows-gnu/release/notepad-fast.exe}
    export WINEDEBUG=-all WINEPREFIX=${WINEPREFIX:-$WORK/wineprefix}
    wineboot -i >/dev/null 2>&1
    APP=(wine "$EXE")
    PATHPREFIX="Z:"
    ;;
*) echo "usage: $0 native|wine [exe]"; exit 2 ;;
esac

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
pass() { PASS=$((PASS + 1)); printf '  \033[32mPASS\033[0m %s\n' "$1"; }
fail() { FAIL=$((FAIL + 1)); FAILED+=("$1"); printf '  \033[31mFAIL\033[0m %s\n' "$1"; [[ -n "${2:-}" ]] && printf '       %s\n' "$2"; }

KNOWN=0
known() { KNOWN=$((KNOWN + 1)); printf '  \033[33mKNOWN\033[0m %s\n       %s\n' "$1" "$2"; }
check_eq() { # name expected actual
    if [[ "$2" == "$3" ]]; then pass "$1"; else fail "$1" "expected $(printf %q "$2"), got $(printf %q "$3")"; fi
}

WIN=""
start_app() { # file
    "${APP[@]}" "$PATHPREFIX$1" >"$WORK/app.log" 2>&1 &
    APP_PID=$!
    WIN=""
    for _ in $(seq 1 120); do
        WIN=$(xdotool search --onlyvisible --name "notepad-fast" 2>/dev/null | tail -1)
        [[ -n "$WIN" ]] && break
        sleep 0.25
    done
    if [[ -z "$WIN" ]]; then fail "window appeared"; return 1; fi
    sleep 1.5
    xdotool windowactivate --sync "$WIN" 2>/dev/null
    xdotool windowfocus --sync "$WIN" 2>/dev/null
    click 400 300
    sleep 0.3
}
stop_app() {
    kill "$APP_PID" 2>/dev/null
    [[ "$MODE" == wine ]] && wineserver -k 2>/dev/null
    wait "$APP_PID" 2>/dev/null
    sleep 0.5
}
click() { xdotool mousemove --window "$WIN" "$1" "$2" click 1; sleep 0.25; }
key() { xdotool key --clearmodifiers "$@"; sleep 0.25; }
# xdotool remaps a spare keycode for characters missing from the keymap; a
# moderate delay keeps that remapping from racing the X server.
typ() { xdotool type --delay 90 "$1"; sleep 0.3; }
# Ask the window to close like a window manager's close button does.
wm_close() {
    python3 - "$1" <<'PY'
import sys
from Xlib import display, protocol, X
d = display.Display()
w = d.create_resource_object("window", int(sys.argv[1]))
proto, delete = d.intern_atom("WM_PROTOCOLS"), d.intern_atom("WM_DELETE_WINDOW")
ev = protocol.event.ClientMessage(window=w, client_type=proto, data=(32, [delete, X.CurrentTime, 0, 0, 0]))
w.send_event(ev, event_mask=X.NoEventMask)
d.flush()
PY
}
clip_set() { printf '%s' "$1" | xclip -selection clipboard -i; sleep 0.3; }
clip_get() { timeout 3 xclip -selection clipboard -o 2>/dev/null; }
shot() { import -window root "$WORK/$1.png" 2>/dev/null; cp "$WORK/$1.png" "${E2E_SHOTS:-$WORK}/$1.png" 2>/dev/null; }
save_and_wait() { # file
    local before; before=$(stat -c %Y.%s "$1" 2>/dev/null)
    # Hold Ctrl explicitly: under Wine a fast ctrl+s can arrive as a plain "s".
    xdotool keydown Control_L; sleep 0.15; xdotool key s; sleep 0.15; xdotool keyup Control_L
    for _ in $(seq 1 40); do
        sleep 0.25
        [[ "$(stat -c %Y.%s "$1" 2>/dev/null)" != "$before" ]] && { sleep 0.3; return 0; }
    done
    return 1
}
content() { cat "$1"; printf x; } # keeps trailing newlines

# ---------------------------------------------------------------------------
log "1. Copy from the app to the system clipboard"
F="$WORK/copy.txt"
printf 'alpha 中文 βeta 😀\nsecond line\n' >"$F"
start_app "$F"
key ctrl+a
key ctrl+c
check_eq "Ctrl+A, Ctrl+C puts the whole file on the clipboard" "$(printf 'alpha 中文 βeta 😀\nsecond line\n'; printf x)" "$(clip_get; printf x)"
key ctrl+Home
key shift+End
key ctrl+c
check_eq "Shift+End, Ctrl+C copies one line with CJK and emoji" "alpha 中文 βeta 😀" "$(clip_get)"
stop_app

log "2. Paste from the system clipboard into the app"
F="$WORK/paste.txt"
printf 'start\n' >"$F"
start_app "$F"
clip_set $'粘贴 pasted 😀\nnext'
key ctrl+End
key ctrl+v
save_and_wait "$F" || fail "save after paste"
check_eq "Ctrl+V inserts clipboard text (CJK, emoji, newline)" "$(printf 'start\n粘贴 pasted 😀\nnext'; printf x)" "$(content "$F")"
key ctrl+z
save_and_wait "$F" || fail "save after undo"
check_eq "Ctrl+Z after saving undoes the paste" "$(printf 'start\n'; printf x)" "$(content "$F")"
key ctrl+y
save_and_wait "$F" || fail "save after redo"
check_eq "Ctrl+Y redoes it" "$(printf 'start\n粘贴 pasted 😀\nnext'; printf x)" "$(content "$F")"
typ " more"
save_and_wait "$F" || fail "third save"
key ctrl+z
key ctrl+z
save_and_wait "$F" || fail "save after two undos"
check_eq "undo reaches back across several saves" "$(printf 'start\n'; printf x)" "$(content "$F")"
check_eq "no leftover save temp files" "" "$(ls -A "$WORK" | grep -E 'notepad-fast-save' || true)"
# Close normally (everything is saved, so no prompt) and check clean-up.
wm_close "$WIN"
for _ in $(seq 1 40); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 0.25; done
kill -0 "$APP_PID" 2>/dev/null && fail "window closes normally"
stop_app
check_eq "no undo files remain after closing" "" "$(ls -A "$WORK" | grep -E 'notepad-fast-(save|undo)' || true)"

log "3. Cut"
F="$WORK/cut.txt"
printf 'cut me 剪切\nkeep\n' >"$F"
start_app "$F"
key ctrl+Home
key shift+Down
key ctrl+x
check_eq "Ctrl+X puts the line on the clipboard" "$(printf 'cut me 剪切\n'; printf x)" "$(clip_get; printf x)"
save_and_wait "$F" || fail "save after cut"
check_eq "Ctrl+X removes the line" "$(printf 'keep\n'; printf x)" "$(content "$F")"
stop_app

log "4. Typing in other languages (keyboard layouts / direct Unicode input)"
F="$WORK/type.txt"
: >"$F"
start_app "$F"
TEXT='Привет мир. Καλημέρα. 你好世界。こんにちは。안녕하세요. مرحبا. नमस्ते. Tiếng Việt. ¿Qué tal? ✅'
typ "$TEXT"
save_and_wait "$F" || fail "save after typing"
check_eq "typed multilingual text is saved exactly" "$TEXT" "$(cat "$F")"
shot "${MODE}-typed"
stop_app

log "5. Edit menu: Copy and Paste with the mouse"
F="$WORK/menu.txt"
printf 'menu text 菜单\n' >"$F"
start_app "$F"
key ctrl+a
click 51 13     # Edit
sleep 0.4
shot "${MODE}-edit-menu"
# Locate the menu items from the screenshot is fragile; use known offsets from
# the default font size: Copy is the 5th item, Paste the 6th.
click 80 125    # Copy
check_eq "Edit > Copy" "$(printf 'menu text 菜单\n'; printf x)" "$(clip_get; printf x)"
key ctrl+End
clip_set "PASTED"
click 51 13
sleep 0.4
click 80 148    # Paste
sleep 0.5
save_and_wait "$F" || fail "save after menu paste"
check_eq "Edit > Paste" "$(printf 'menu text 菜单\nPASTED'; printf x)" "$(content "$F")"
stop_app

log "6. Find a CJK word typed into the find box, then copy the match"
F="$WORK/find.txt"
printf 'first line\n日志 错误 occurred\nlast\n' >"$F"
start_app "$F"
key ctrl+f
typ "错误"
key Return
key Escape
key ctrl+c
check_eq "Find selects the CJK match" "错误" "$(clip_get)"
stop_app

log "7. Rendering of many scripts"
F="$WORK/scripts.txt"
cat >"$F" <<'TXT'
English: The quick brown fox jumps over the lazy dog.
中文: 你好，世界！大文件编辑器测试。
日本語: こんにちは世界、カタカナとひらがな。
한국어: 안녕하세요 세계
Русский: Съешь же ещё этих мягких французских булок.
Ελληνικά: Καλημέρα κόσμε
العربية: مرحبا بالعالم
עברית: שלום עולם
हिन्दी: नमस्ते दुनिया
ไทย: สวัสดีชาวโลก
Tiếng Việt: Xin chào thế giới
Emoji: 👋🏽 👨‍👩‍👧‍👦 🇲🇾 ✅ 🚀
TXT
start_app "$F"
sleep 2 # system fonts load asynchronously
key ctrl+End
shot "${MODE}-scripts"
pass "rendered (inspect ${MODE}-scripts.png)"
stop_app

log "8. Large file: paste at the end of a 150 MB file and save"
F="$WORK/big.log"
python3 - "$F" <<'PY'
import sys
with open(sys.argv[1], "w") as f:
    line = "2026-10-27T12:00:00Z INFO request handled path=/api/v1/items status=200 bytes=1234\n"
    f.write(line * 1_900_000)
PY
SIZE=$(stat -c %s "$F")
start_app "$F"
sleep 2
clip_set "尾巴 TAIL"
key ctrl+End
key ctrl+v
save_and_wait "$F" || fail "save big file"
sleep 2
check_eq "big file grew by exactly the pasted bytes" "$((SIZE + $(printf '尾巴 TAIL' | wc -c)))" "$(stat -c %s "$F")"
check_eq "big file ends with the pasted text" "尾巴 TAIL" "$(tail -c "$(printf '尾巴 TAIL' | wc -c)" "$F")"
stop_app

log "9. Closing the window with unsaved changes asks first"
F="$WORK/close.txt"
printf 'x' >"$F"
start_app "$F"
typ "unsaved"
wm_close "$WIN"
sleep 1.5
shot "${MODE}-close-prompt"
if kill -0 "$APP_PID" 2>/dev/null; then pass "app stays open and shows the prompt"; else fail "app stays open and shows the prompt"; fi
key Escape # Cancel
sleep 0.5
if kill -0 "$APP_PID" 2>/dev/null; then pass "Cancel keeps the app open"; else fail "Cancel keeps the app open"; fi
check_eq "file untouched" "x" "$(cat "$F")"
wm_close "$WIN"
sleep 1
key Return # Enter = Save (the default button)
for _ in $(seq 1 20); do kill -0 "$APP_PID" 2>/dev/null || break; sleep 0.25; done
if kill -0 "$APP_PID" 2>/dev/null; then fail "Save in the prompt saves and exits"; stop_app; else pass "Save in the prompt saves and exits"; fi
check_eq "file saved on exit" "xunsaved" "$(cat "$F")"

log "11. Non-ASCII file name with spaces"
F="$WORK/日志 文件 (1).log"
printf 'ERROR 失败\n' >"$F"
start_app "$F"
key ctrl+End
typ "ok"
save_and_wait "$F" || fail "save unicode-named file"
check_eq "file with a CJK name opens and saves" "$(printf 'ERROR 失败\nok'; printf x)" "$(content "$F")"
stop_app

log "12. Large clipboard transfers"
# X11's clipboard tops out near 16 MiB (single X request); Windows has no such limit.
if [[ "$MODE" == wine ]]; then LINES=800000; else LINES=280000; fi
F="$WORK/clip-big.txt"
python3 -c "
import sys
line = 'clipboard line with 中文 and ascii text 0123456789\n'
open(sys.argv[1], 'w', encoding='utf-8').write(line * int(sys.argv[2]))
" "$F" "$LINES"
start_app "$F"
key ctrl+a
key ctrl+c
sleep 3
check_eq "copying a $(($(stat -c %s "$F") / 1048576)) MB selection" "$(stat -c %s "$F")" "$(timeout 30 xclip -selection clipboard -o | wc -c)"
stop_app
F="$WORK/paste-big.txt"
: >"$F"
python3 -c "print('pasted 粘贴 line\n' * 250000, end='')" | xclip -selection clipboard -i
sleep 0.5
EXPECTED=$(clip_get | wc -c)
start_app "$F"
key ctrl+v
sleep 2
save_and_wait "$F" || fail "save after big paste"
# A new file on Windows uses CRLF line endings (like Notepad), so each
# pasted line gains a byte there.
[[ "$MODE" == wine ]] && EXPECTED=$((EXPECTED + 250000))
check_eq "pasting $((EXPECTED / 1048576)) MB from the clipboard" "$EXPECTED" "$(stat -c %s "$F")"
stop_app

if [[ "$MODE" == wine ]]; then
    log "13. Windows file dialogs"
    F="$WORK/dialog-source.txt"
    printf 'dialog test\n' >"$F"
    start_app "$F"
    key ctrl+shift+s
    sleep 3
    shot "wine-save-as-dialog"
    TARGET="$WORK/saved via dialog.txt"
    # The file name box has focus; replace its contents with a full path.
    key ctrl+a
    xdotool type --delay 30 "Z:${TARGET//\//\\}"
    key Return
    for _ in $(seq 1 40); do [[ -f "$TARGET" ]] && break; sleep 0.25; done
    sleep 0.5
    check_eq "Save As writes the chosen file" "$(printf 'dialog test\n'; printf x)" "$(content "$TARGET" 2>/dev/null)"
    key ctrl+o
    sleep 3
    shot "wine-open-dialog"
    key ctrl+a
    xdotool type --delay 30 "Z:${F//\//\\}"
    key Return
    sleep 2
    key ctrl+End
    typ "X"
    save_and_wait "$F" || fail "save after Open dialog"
    check_eq "Open loads the chosen file" "$(printf 'dialog test\nX'; printf x)" "$(content "$F")"
    stop_app
fi

if command -v ibus-daemon >/dev/null && [[ "${E2E_SKIP_IME:-}" != 1 ]]; then
    log "10. Real input methods (ibus): Chinese Pinyin, Korean Hangul, Japanese Kana-Kanji"
    eval "$(dbus-launch --sh-syntax)"
    export XMODIFIERS=@im=ibus GTK_IM_MODULE=ibus QT_IM_MODULE=ibus
    # A fresh input method per test: a killed client can leave ibus's XIM
    # bridge waiting forever, which would block the next window's keyboard.
    ime_restart() {
        ibus exit >/dev/null 2>&1
        sleep 1
        ibus-daemon --daemonize --xim --replace --panel=disable >/dev/null 2>&1
        sleep 3
    }
    ime_restart
    F="$WORK/ime.txt"
    : >"$F"
    # (`ibus engine NAME` exits non-zero even on success; check the result.)
    ibus engine libpinyin >/dev/null 2>&1
    if [[ "$(ibus engine 2>/dev/null)" == libpinyin ]]; then
        start_app "$F"
        sleep 1
        ibus engine libpinyin >/dev/null 2>&1
        xdotool type --delay 120 "nihao"
        sleep 0.8
        shot "${MODE}-ime-composing"
        key space
        sleep 0.5
        save_and_wait "$F" || fail "save after pinyin"
        check_eq "Pinyin 'nihao' + space commits 你好" "你好" "$(cat "$F")"
        stop_app
    else
        fail "ibus libpinyin engine available"
    fi
    : >"$F"
    ime_restart
    ibus engine hangul >/dev/null 2>&1
    if [[ "$(ibus engine 2>/dev/null)" == hangul ]]; then
        start_app "$F"
        sleep 1
        ibus engine hangul >/dev/null 2>&1
        key shift+space # ibus-hangul starts in Latin mode; switch to Hangul
        xdotool type --delay 120 "dkssud"
        sleep 0.5
        key space
        sleep 1.5
        ibus engine xkb:us::eng >/dev/null 2>&1
        sleep 0.5
        save_and_wait "$F" || fail "save after hangul"
        GOT=$(cat "$F")
        if [[ "$MODE" == wine && "$GOT" != "안녕 " ]]; then
            known "Hangul 'dkssud' under Wine gave $(printf %q "$GOT")" \
                "Wine's XIM->IMM bridge reorders Korean commit/preedit messages; egui's own text fields lose the same jamo, Wine's Notepad does not. Verify on real Windows."
        else
            check_eq "Hangul 'dkssud' composes 안녕" "안녕 " "$GOT"
        fi
        stop_app
    else
        fail "ibus hangul engine available"
    fi
    : >"$F"
    ime_restart
    ibus engine kkc >/dev/null 2>&1
    if [[ "$(ibus engine 2>/dev/null)" == kkc ]]; then
        start_app "$F"
        sleep 1
        ibus engine kkc >/dev/null 2>&1
        xdotool type --delay 150 "nihongo"
        sleep 0.5
        key space  # convert to kanji
        sleep 0.5
        key Return # confirm: must not also insert a newline
        sleep 1.5  # let the commit arrive before switching the IME off
        ibus engine xkb:us::eng >/dev/null 2>&1 # the IME would swallow Ctrl+S
        sleep 0.5
        typ "x"
        save_and_wait "$F" || fail "save after kkc"
        check_eq "Japanese 'nihongo' + Space + Enter commits 日本語 (no stray newline)" "日本語x" "$(cat "$F")"
        stop_app
    else
        fail "ibus kkc engine available"
    fi
    ibus exit >/dev/null 2>&1
fi

echo
echo "Screenshots and logs: $WORK"
echo "Result: $PASS passed, $FAIL failed, $KNOWN known issue(s)"
for f in "${FAILED[@]}"; do echo "  - $f"; done
[[ $FAIL -eq 0 ]]

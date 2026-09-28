"""Drive moku's TUI programmatically on Windows via ConPTY (pywinpty) + pyte.

There is no tmux on Windows; this spawns moku.exe inside a real pseudo-console,
feeds its output into a pyte virtual screen, and lets a small script send keys
and read the screen back.

Usage (from the repo root, with the venv from SKILL.md):
    python .claude/skills/run-moku/driver.py <script.txt | ->

Script commands (one per line, `#` comments allowed):
    wait <text> [timeout_s]   wait until <text> is on screen (default 15 s); fails on timeout
    expect <text>             fail unless <text> is on screen right now
    reject <text>             fail if <text> is on screen right now
    sleep <seconds>
    keys <tok> ...            send keys; tokens: enter esc tab backspace up down left
                              right ctrl-l ctrl-c, `text:<literal>`, `env:<VAR>`
                              (types the env var's value, e.g. a password), or any
                              single character like / q j k
    snap <label>              print the full screen
    outline <label>           print only frame titles, status bars and error-looking
                              lines (safe when the screen may show personal data)

Exit code: 0 if every step passed, 1 on the first failed wait/expect/reject.
"""
import os
import re
import shlex
import sys
import threading
import time

import pyte
from winpty import PtyProcess

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
EXE = os.environ.get("MOKU_EXE", os.path.join(REPO, "target", "debug", "moku.exe"))
COLS, ROWS = 120, 40

KEYMAP = {
    "enter": "\r", "esc": "\x1b", "tab": "\t", "backspace": "\x7f",
    "up": "\x1b[A", "down": "\x1b[B", "right": "\x1b[C", "left": "\x1b[D",
    "ctrl-l": "\x0c", "ctrl-c": "\x03",
}
STATUS_RE = re.compile(r"\[Esc\]|\[ESC\]|\[q\]")
KEYWORD_RE = re.compile(r"error|failed|locked|login|panic", re.I)


def outline_line(line):
    """What `outline` may print for one screen row, or None.

    Only frame edges (which carry panel titles) and status/help bars are
    printed whole. Any other row can hold list content (feeds, secrets,
    notes, todo titles), so for those only a short window around an
    error/lock keyword is shown, never the rest of the row.
    """
    s = line.strip()
    if not s:
        return None
    if s[0] in "┌└" or STATUS_RE.search(s):
        return line
    m = KEYWORD_RE.search(s)
    if m:
        return "…" + s[max(0, m.start() - 20): m.end() + 30] + "…"
    return None

screen = pyte.Screen(COLS, ROWS)
stream = pyte.Stream(screen)
lock = threading.Lock()


def _reader(proc):
    while True:
        try:
            data = proc.read(4096)
        except Exception:
            break
        if not data:
            time.sleep(0.01)
            continue
        with lock:
            stream.feed(data)


def screen_text():
    # Render cell by cell: pyte's own `display` crashes (IndexError) on the
    # empty cells ratatui leaves after wide emoji.
    with lock:
        rows = []
        for y in range(screen.lines):
            line = screen.buffer[y]
            rows.append("".join((line[x].data or " ") for x in range(screen.columns)).rstrip())
    return "\n".join(rows)


def main():
    src = sys.argv[1] if len(sys.argv) > 1 else "-"
    script = (sys.stdin.read() if src == "-" else open(src, encoding="utf-8").read()).splitlines()
    if not os.path.exists(EXE):
        print(f"!! {EXE} not found; run `cargo build -p moku-bin` first")
        return 1

    proc = PtyProcess.spawn(EXE, dimensions=(ROWS, COLS), cwd=REPO)
    threading.Thread(target=_reader, args=(proc,), daemon=True).start()
    try:
        for raw in script:
            line = raw.strip()
            if not line or line.startswith("#"):
                continue
            cmd, _, rest = line.partition(" ")
            args = shlex.split(rest)
            if cmd == "wait":
                needle, timeout = args[0], float(args[1]) if len(args) > 1 else 15.0
                end = time.time() + timeout
                while needle not in screen_text():
                    if time.time() > end:
                        print(f"!! TIMEOUT waiting for {needle!r}")
                        return 1
                    time.sleep(0.1)
            elif cmd in ("expect", "reject"):
                time.sleep(0.3)
                present = args[0] in screen_text()
                if present != (cmd == "expect"):
                    print(f"!! {cmd.upper()} failed for {args[0]!r}")
                    return 1
            elif cmd == "sleep":
                time.sleep(float(rest))
            elif cmd == "keys":
                for tok in args:
                    if tok.startswith("text:"):
                        proc.write(tok[5:])
                    elif tok.startswith("env:"):
                        proc.write(os.environ.get(tok[4:], ""))
                    else:
                        proc.write(KEYMAP.get(tok, tok))
                    time.sleep(0.15)
            elif cmd in ("snap", "outline"):
                time.sleep(0.4)
                print(f"===== {rest} =====")
                for l in screen_text().splitlines():
                    out = (l if l.strip() else None) if cmd == "snap" else outline_line(l)
                    if out:
                        print(out)
            else:
                print(f"!! unknown command {cmd!r}")
                return 1
        print("OK")
        return 0
    finally:
        try:
            if proc.isalive():
                proc.terminate(force=True)
        except Exception:
            pass


if __name__ == "__main__":
    sys.exit(main())

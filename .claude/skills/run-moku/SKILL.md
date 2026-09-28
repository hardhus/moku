---
name: run-moku
description: Build, run, and drive the moku TUI (moku-bin) on Windows. Use when asked to run/start moku, open or screenshot the TUI, unlock the vault, click through modules, verify moku_dev data still loads after a change (the audit checklist's phase-end moku_dev check), or run the test suite.
---

moku is a ratatui TUI (`moku-bin`, exe `moku.exe`). There is no tmux on
Windows, so an agent drives it through
`.claude/skills/run-moku/driver.py`. The driver spawns the real
`moku.exe` inside a Windows pseudo-console (ConPTY via `pywinpty`),
renders its output with `pyte`, and runs a small line-based script
(send keys, wait for text, print screen).

All paths below are relative to the repo root (`moku/`). Verified on
Windows 11 with Rust 1.98 and Python 3.11, from Git Bash.

## Prerequisites (once)

Create a skill-local venv. It is gitignored; don't install into the
system Python.

```bash
V=.claude/skills/run-moku/.venv
python -m venv $V && $V/Scripts/python -m pip install -q --disable-pip-version-check pywinpty pyte
```

## Build

```bash
cargo build -p moku-bin
```

A debug build uses the separate **`moku_dev`** data/config dirs
(`%LOCALAPPDATA%\hardhus\moku_dev\data`); `ProjectDirs` picks
"moku_dev" under `cfg!(debug_assertions)`. It never touches the real
`moku` data. Always use the debug build for agent runs.

## Run (agent path): the driver

Phase-end smoke check. It unlocks the dev vault, opens todo, bookmark,
secrets, rss, volumes, pomodoro, settings and dashboard, then locks with
`ctrl-l`, checks that re-locking works, and quits. It prints `OK` and
exits 0 on success.

```bash
MOKU_DEV_PASSWORD=pass PYTHONIOENCODING=utf-8 .claude/skills/run-moku/.venv/Scripts/python .claude/skills/run-moku/driver.py .claude/skills/run-moku/moku_dev_check.txt
```

(`pass` is the `moku_dev` test vault's password, given by the user for
agent runs. It is dev-only and never a real vault password.)

Ad-hoc flows go on stdin. Example: a wrong password must show an error:

```bash
echo 'wait "Moku Launcher" 30
keys / text:todo enter
wait "Secure Login" 10
keys text:wrong-password enter
sleep 6
outline wrong
keys esc
sleep 1
keys q' | PYTHONIOENCODING=utf-8 .claude/skills/run-moku/.venv/Scripts/python .claude/skills/run-moku/driver.py -
```

Script commands (full list in the driver's docstring):

| Command | Meaning |
|---|---|
| `wait <text> [s]` | Block until text is on screen; exit 1 on timeout. |
| `expect` / `reject <text>` | Assert that text is or isn't on screen right now. |
| `keys ...` | Send keys. Special tokens: `enter` `esc` `tab` `backspace` `up` `down` `ctrl-l`. `text:<literal>` types literal text, `env:<VAR>` types an env var's value (use this for passwords), and single characters like `/` `q` `j` `k` are sent as-is. |
| `snap <label>` | Print the whole screen. |
| `outline <label>` | Print only panel titles, status bars and short snippets around error/lock words. |

Use `outline` instead of `snap` whenever `moku_dev` data could be on
screen: feeds, secrets, notes and todo titles are the user's dev data.

Navigation that works:
- In the launcher, `/` + name + `enter` opens a module by fuzzy search.
- Default keys: `j`/`k` move, `enter` select, `esc` back to launcher,
  `q` quit, `ctrl-l` lock.
- Opening an encrypted module while locked shows "Secure Login".

## Run (human path)

A human runs `cargo run -p moku-bin` in a real terminal, and the TUI
takes over that terminal until `q`. This is not usable from an agent's
bash tool; use the driver above instead.

## Test

```bash
cargo test --workspace
```

The suite includes real WinFsp mount tests in `moku-volume-mount`.
`gen_fixtures_v0_2_7` shows as `ignored` on purpose (see
`test-fixtures/v0.2.7/README.md`).

## Gotchas

- **No tmux on Windows.** Don't look for it. `wsl` exists but would run
  a Linux build against a different data dir. The ConPTY driver drives
  the real Windows exe.
- **`pyte`'s own `screen.display` crashes** with `IndexError` in
  `wcwidth(char[0])` on the empty cells ratatui leaves after wide emoji
  (🔒 📡 🚀). The driver renders cell by cell with `cell.data or " "`
  instead; keep it that way.
- **Output encoding:** the Windows console codepage is cp1254 here, so
  printing box-drawing characters raises `UnicodeEncodeError`. Always
  set `PYTHONIOENCODING=utf-8`.
- **Toast overlays share rows with list content.** A naive "print every
  row containing a box corner" outline leaked a todo title that sat on
  the same row as the "Session Unlocked" toast. `outline` therefore
  prints whole rows only when they start with a frame edge or are
  status bars; everything else is reduced to a snippet around the
  keyword.
- **Unlock takes a few seconds:** Argon2id with 64 MiB and t=3 runs
  inline in the debug build. Wait for the target module's title with a
  30 s timeout, not a fixed sleep.
- **Key pacing:** keys are sent 150 ms apart. Sending faster can drop
  keys during the launcher's search-mode transition.
- **The launcher's selection highlight is colour-only**, so it's
  invisible in the text screen. Navigate with `/` search rather than
  counting `j` presses.

## Troubleshooting

- `!! ...\target\debug\moku.exe not found`: run `cargo build -p
  moku-bin` first, or set `MOKU_EXE` to another build.
- `!! TIMEOUT waiting for 'Todo List'` right after the password: wrong
  `MOKU_DEV_PASSWORD`. The lock screen then shows "Incorrect password
  or initialization error!"; run the stdin example above with
  `outline` to see it.

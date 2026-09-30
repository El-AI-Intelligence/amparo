# W6 — Windows drill (manual, on real Windows)

Run on the Windows machine after v0.14.0 ships. Fresh install via
PowerShell:

```powershell
irm https://downloads.ellmstack.dev/amparo/install.ps1 | iex
```

Run each block in **Windows Terminal first**, then repeat block 4 in
**legacy conhost** (Start → `conhost.exe`, or disable "Use legacy
console" off/on — the point is the old console host).

## 1 — First run: wizard → TUI

- Fresh environment (no `.amparo` profile): run `amparo`.
- The 4-step setup wizard opens; complete it.
- The TUI boots (banner, version line) — no extra command needed.

## 2 — TUI behavior

- Type non-ASCII input (`é`, `中文`) — renders clean, no mojibake.
- Arrows + history work; Enter submits.
- `! dir` — runs via `cmd /C`; raw input restored when it returns.
- An approval card: press `y` or `n` alone — resolves, no Enter needed.
- Ctrl-C on a prompt = deny/cancel; Ctrl-D on empty = quit.
- The picker (e.g. tool disambiguation) responds to arrows.

## 3 — `amparo code .`

- Alt-screen tree opens; j/k and arrows navigate.
- `e` opens edit; y/n diff-accept works.
- `b` opens the build pane (streaming); `!` for a shell command.
- `q` quits — terminal fully restored (cursor, echo, screen).

## 4 — conhost specifics

- Click inside the window during a session — no quick-edit freeze
  (raw mode cleared it; restored on exit).
- ANSI renders correctly (colors, alternate screen).
- Resize the window — the UI re-renders.

## 5 — Piped sanity

```powershell
echo "read the readme" | amparo tui   # zero escape sequences in output
amparo < NUL                          # USAGE + exit code 2
```

## 6 — Re-run

- Run `amparo` again — straight to the TUI (profile + env gaps filled),
  wizard does not reappear.

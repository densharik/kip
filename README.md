<div align="center">
  <img src="resources/icon_1024.png" width="104" alt="kip">
  <h1>kip</h1>
  <p>A terminal for running Claude Code sessions side by side.</p>
</div>

---

kip is a terminal emulator built around Claude Code sessions. Every session is a
row in a sidebar with a live context-window percentage. You can put a session to
sleep to free its memory and resume the same conversation later, and every
session survives quitting the app.

kip is a terminal, not a Claude client. It runs the real `claude` binary on a
real PTY, so `htop`, `vim`, `ssh` and everything else work exactly as they do in
iTerm or Windows Terminal. Single native Rust binary (egui + alacritty_terminal),
no runtime dependencies, no Electron.

## Why

Run a few agents at once in a normal terminal and you get a wall of tabs: no idea
which session is which, how close any of them is to filling its context, or which
one is holding a gigabyte of RAM while it waits for you. Close the terminal and
they are gone. kip fixes those three things and stays out of the way otherwise.

## Features

**Sessions**

- Each session is a row with its name, working directory, and a status dot:
  running, idle at a prompt, suspended, or exited with an error.
- A per-session badge shows context use: green under 50%, yellow 50-70%, red and
  pulsing above 70%. It appears the moment you resume, before Claude boots.
- Pin a session: it cannot be closed by a stray click, dragged, or auto-suspended.
- Group sessions by dragging one onto another. Groups have colored headers,
  collapse, and persist across restarts.
- Rename with a double click. Your name wins over the one Claude picked.
- `Cmd+1`..`Cmd+9` jumps to a session. A pinned session keeps its number.

**Memory**

- Suspend a session to free its memory. kip kills the whole process tree and
  keeps the screen as text; the session comes back with `claude --resume` wired
  up and its scrollback intact.
- Idle sessions suspend on their own after N minutes (10 by default, 0 disables
  it). A Claude prompt sitting there waiting counts as idle; a running `make` or
  `rsync` does not, so nothing gets killed mid-build.
- Mark a session "keep awake" to exempt it from the idle timer.
- Quit kip and every session is still there when you come back - same names,
  groups, pins, last screen, and Claude conversation id.

**Monitoring**

- A resource panel shows RAM and CPU per session, plus kip's own footprint, so
  you can see which agent to put to sleep.
- Your Claude subscription limits (5-hour session, weekly, per-model) live in the
  corner - the same numbers `/usage` shows inside Claude Code. Pin one and its
  bar stays visible.
- Git stats for the active session's directory in the status bar.
- A system notification when an agent finishes, with how long it took.

**Terminal**

- Real PTY. Anything you run in a terminal runs here.
- File paths in the output are clickable: Open, or reveal in Finder/Explorer.
- Drag a file in to insert its path; paste a screenshot to insert the path to it.
- Command bar with history, `Shift+Enter` for a newline, and `cd` that follows.
- Directory switcher with folder search and folder creation, plus a file explorer
  panel.
- Six color themes, an accent picker, nine terminal fonts, UI zoom.
- macOS: "New kip Window Here" in Finder's Services menu.
- English and Russian, switchable without a restart.
- Updates itself from a button in settings.

## Install

Grab the latest build from [Releases](https://github.com/densharik/kip/releases).
After that kip updates itself, so this is a one-time step.

**macOS** - open `kip-installer.pkg`. It is not notarized, so right-click the
`.pkg` and choose Open the first time, then follow the installer. Installed this
way it launches with no Gatekeeper warning.

**Windows** - download `kip.exe` and run it. Single portable binary, put it
anywhere.

**From source** - needs a stable Rust toolchain (edition 2024):

```
git clone https://github.com/densharik/kip
cd kip
cargo build --release
```

## FAQ

### How do I suspend a Claude Code session to free RAM?

Right-click the session and pick Suspend, or let the idle timer do it. kip kills
the session's entire process tree, so the memory actually goes back to the OS -
it is not a paused or backgrounded process. The screen is kept as text, and
Resume Claude brings the same conversation back with `claude --resume <id>`.

### Do Claude Code sessions survive closing the terminal?

Yes. kip writes every session to disk - working directory, name, group, pin, the
last 32 KB of its screen, and the Claude conversation id - so quitting the app
and reopening it gives you the same list. There is nothing to save by hand.
State lives in `~/Library/Application Support/kip/state.json` on macOS,
`~/.config/kip/state.json` on Linux, `%APPDATA%\kip\state.json` on Windows.

### Can Claude Code sessions be suspended automatically?

Yes. Settings has "Suspend after (min)", 10 by default. A session that has
produced no output and seen no input for that long is suspended. An interactive
Claude sitting at its prompt counts as idle, but a foreground job that is
actually running does not, so a long build or sync is never cut off. Pinned and
"keep awake" sessions are skipped.

### Is kip a terminal or a GUI app for Claude?

A terminal. kip drives a real PTY (ConPTY on Windows) and launches the real
`claude` CLI in it - it does not talk to the Anthropic API itself and does not
reimplement Claude's interface. Every Claude Code feature, flag, and update works
the day it ships, and you can run any other command in the same session.

### How do I see how full a Claude Code session's context window is?

Every session row carries a percentage badge. kip reads it two ways: from the
session transcript under `~/.claude/projects` (works everywhere, no setup), or
exactly, straight from Claude, if you turn on the statusline hook in settings.

### Can I run several Claude Code agents at once?

That is what the sidebar is for. Each session is its own shell in its own
directory with its own Claude conversation, and the badges tell you which agent
is about to run out of context and which one is idle waiting for you. Sessions
you are not using can sleep so they cost nothing.

### How much memory does kip use?

kip itself sits around 200 MB. Each live session costs whatever its shell and
`claude` process cost; a suspended session costs nothing beyond a few KB of saved
text. The resource panel shows both.

## Context percent

Claude Code computes the percentage at runtime and never writes it to disk, so
kip gets it two ways. By default it reads the session transcript under
`~/.claude/projects` and estimates from the last token count - works everywhere,
no setup. Turn on the statusline hook in settings (macOS/Linux) and Claude feeds
kip the exact number it shows itself, about once a second.

## Mods

A Claude Code mod (a plugin of function hooks running inside `claude`) can
show things in kip without any kip code. It writes
`~/.kip/mods/<mod>/<claude session id>.json`, and kip draws a badge in that
session's row and a card over its terminal:

```json
{
  "updatedAt": 1760000000000,
  "ttlMs": 5000,
  "badge": { "text": "VPN", "tone": "ok", "hint": "shown on hover" },
  "card": {
    "title": "Running bash: 1",
    "tone": "busy",
    "rows": [
      { "text": "Run tests", "style": "strong" },
      { "text": "test a ... ok", "style": "mono" }
    ]
  }
}
```

Every field is optional. `ttlMs` hides the view once the mod stops refreshing
it (its `claude` exited); without it the view stays until the file changes.
Tones: `info`, `ok`, `warn`, `error`, `busy`, `dim`. Row styles: `text`,
`strong`, `mono`, `dim`, `faint`. ANSI colors in the text are stripped. A click
on a card's title folds it.

## Platform support

|                          | macOS | Linux | Windows |
|--------------------------|:-----:|:-----:|:-------:|
| Terminal, sessions, themes |  yes  |  yes  |   yes   |
| Suspend / resume, persistence | yes | yes |   yes   |
| Context badge (estimate)   |  yes  |  yes  |   yes   |
| Exact context hook         |  yes  |  yes  |    -    |
| File/image paste           |  yes  |  yes  |  text   |

Windows runs on ConPTY with PowerShell. Items marked `-` fall back gracefully.

## License

Apache-2.0. See [LICENSE](LICENSE).

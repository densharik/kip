<div align="center">
  <img src="resources/icon_1024.png" width="104" alt="kip">
  <h1>kip</h1>
  <p>A terminal for running Claude Code sessions side by side.</p>
</div>

---

kip keeps every Claude Code session in a sidebar, shows how full each one's
context window is, and lets you put a session to sleep to free memory and pick
it up later. It is a single native binary written in Rust (egui +
alacritty_terminal), with no runtime dependencies.

## Why

Run a few agents at once in a normal terminal and you get a wall of tabs: no
idea which session is which, or how close any of them is to filling its
context. kip gives each session a labeled row with a live context-percent
badge, so you can see at a glance which agent is about to run out of room and
which one is idle waiting for you.

## Features

- Every session is a row with its name, working directory, and a status dot.
- A per-session badge shows context use: green under 50%, yellow 50-70%, red
  and pulsing above 70%. It shows up the moment you resume, before Claude boots.
- Suspend a session to free its memory; resume later with `--resume` wired up.
- Idle sessions suspend on their own after N minutes; a live Claude prompt
  counts as idle, a running `make` or `rsync` does not.
- Six color themes with an accent picker.
- Command bar with history, drag-and-drop file paths, and `cd` that follows.
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

## Context percent

Claude Code computes the percentage at runtime and never writes it to disk, so
kip gets it two ways. By default it reads the session transcript under
`~/.claude/projects` and estimates from the last token count - works everywhere,
no setup. Turn on the statusline hook in settings (macOS/Linux) and Claude feeds
kip the exact number it shows itself, about once a second.

## Platform support

|                          | macOS | Linux | Windows |
|--------------------------|:-----:|:-----:|:-------:|
| Terminal, sessions, themes |  yes  |  yes  |   yes   |
| Context badge (estimate)   |  yes  |  yes  |   yes   |
| Exact context hook         |  yes  |  yes  |    -    |
| File/image paste           |  yes  |  yes  |  text   |

Windows runs on ConPTY with PowerShell. Items marked `-` fall back gracefully.

## License

Apache-2.0. See [LICENSE](LICENSE).

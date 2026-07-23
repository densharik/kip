# kip

A small terminal built for running [Claude Code](https://www.anthropic.com/claude-code) sessions. It keeps every session in a sidebar, shows how full each session's context window is, and lets you suspend a session to free memory and resume it later without losing the conversation.

It is a native desktop app written in Rust (egui + alacritty_terminal). One binary, no runtime dependencies.

## Why

If you run several Claude Code agents at once, a normal terminal gives you a wall of tabs with no idea which session is which or how close any of them is to filling its context. kip puts each session in a labeled row with a live context-percent badge, so you can tell at a glance which agent is about to run out of room and which one is idle waiting for you.

## Features

- **Session sidebar.** Each Claude Code session is a row with its name (pulled from Claude), working directory, and a status dot: green while the agent works, orange when it waits for input, red when it exits with an error.
- **Context-percent badge.** A pill on each row shows how full that session's context window is: green under 50%, yellow 50-70%, red and pulsing over 70%. It appears the instant you resume a session by id, before Claude even finishes booting.
- **Suspend and resume.** Put a session to sleep to free its memory; a text snapshot of the screen is kept. Resume it later with `claude --resume` wired up automatically.
- **Knows which session is running.** Whether you launch Claude by typing `claude --resume <id>`, pick one from a session browser, or use a wrapper tool, kip identifies the session and binds its context to the right row.
- **Idle suspend.** Sessions with no output for N minutes are suspended automatically. An interactive Claude prompt counts as idle; a silent `make` or `rsync` does not get killed.
- **Warp-style command bar.** A command editor under the terminal with filtered history; drag-and-drop or paste a file to insert its path.
- **Directory switcher, git status, per-session resource monitor.**
- **In-app updates.** kip checks GitHub for a newer release on launch; a button in settings downloads and installs it, then restarts. macOS and Windows.

## Context percent, exactly

Claude Code does not store the context percentage anywhere on disk; it computes it at runtime. kip gets the number two ways:

1. **Estimate (works everywhere, no setup).** kip reads the session transcript that Claude Code writes under `~/.claude/projects`, takes the last recorded token usage, and divides by the model's context window (with self-calibration, so a 1M-token window is never mistaken for 200k). This is what drives the badge out of the box.

2. **Exact (macOS and Linux, opt-in).** Turn on "Точный % контекста" in settings and kip installs a tiny statusline hook into Claude Code. Claude then feeds kip the same percentage it shows itself, about once a second. If you already have a statusline configured, kip wraps it instead of replacing it, and removes itself cleanly when you turn the option off. The Windows exact-hook is not in this release; Windows uses the estimate.

## Install

### Download a release

Grab the latest build from the [Releases](https://github.com/densharik/kip/releases) page. After that, kip updates itself from a button in settings, so this is a one-time step.

- **macOS:** download `kip-installer.pkg` and open it. Because it is not notarized, right-click the .pkg and choose Open the first time, then follow the installer (it asks for your password to install into Applications). Installed this way, kip launches normally with no Gatekeeper warning. If you prefer, `kip-macos.zip` still works: unzip, move `kip.app` to Applications, and run `xattr -cr /Applications/kip.app` once.
- **Windows:** download `kip.exe` and run it. It is a single portable binary; put it anywhere.

### Build from source

You need a Rust toolchain (stable, edition 2024).

```
git clone https://github.com/densharik/kip
cd kip
cargo build --release
```

The binary lands at `target/release/kip` (`kip.exe` on Windows).

## Platform support

| | macOS | Linux | Windows |
|---|---|---|---|
| Terminal, sessions, resume | yes | yes | yes |
| Context badge (estimate) | yes | yes | yes |
| Exact context hook | yes | yes | not yet |
| `cd` follows in the path chip | yes | yes | not yet |
| File/image clipboard paste | yes | yes | text only |
| Desktop notifications | full | full | not yet |

Windows runs on ConPTY with PowerShell as the shell. The items marked "not yet" fall back gracefully and are on the roadmap.

## Requirements

- [Claude Code](https://www.anthropic.com/claude-code) installed and on your `PATH`.
- macOS 11+, a recent Linux, or Windows 10/11.

## License

Apache-2.0. See [LICENSE](LICENSE).

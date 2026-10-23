//! `kip mcp`: an MCP server on stdio that drives the running kip through the
//! control channel (see `ctl`), starting kip first if it is not up.
use std::io::{BufRead, Write};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use crate::ctl;

const INSTRUCTIONS: &str = "kip is the terminal app these Claude Code sessions run in. \
Session ids come from list_sessions; done = its agent finished unseen. A pinned session refuses close/move unless force is set. \
To summarize a session, read_transcript it and summarize the text yourself.";

fn tools() -> Value {
    let id = json!({"type": "integer", "description": "kip session id (from list_sessions)"});
    json!([
        {"name": "list_sessions", "description": "All kip sessions: id, window, name, folder, group, project, state (live/suspended/exited), busy, pinned, Claude session id, context %, idle time.",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "list_windows", "description": "Open kip windows with their active session and session count.",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "open_session", "description": "Open a new terminal session in a folder. Optionally start Claude in it, resume a Claude session by id, or run a command; put it in a window, group, under a name.",
         "inputSchema": {"type": "object", "properties": {
            "cwd": {"type": "string", "description": "Folder; ~ is expanded"},
            "claude": {"type": "boolean", "description": "Start a fresh Claude in it"},
            "resume": {"type": "string", "description": "Claude session id to resume"},
            "command": {"type": "string", "description": "Shell command to run; the shell stays after it"},
            "window": {"type": "integer", "description": "Window id (default: main window 0)"},
            "group": {"type": "string"},
            "name": {"type": "string"}}, "required": ["cwd"]}},
        {"name": "close_session", "description": "Close a session for good (kills whatever runs in it).",
         "inputSchema": {"type": "object", "properties": {"id": id, "force": {"type": "boolean", "description": "Also close a pinned session"}}, "required": ["id"]}},
        {"name": "suspend_session", "description": "Put a session to sleep: the process is stopped, the row and a screen snapshot stay; wake_session brings it back.",
         "inputSchema": {"type": "object", "properties": {"id": id}, "required": ["id"]}},
        {"name": "wake_session", "description": "Revive a suspended/exited session in place, resuming its Claude conversation by default.",
         "inputSchema": {"type": "object", "properties": {"id": id, "claude": {"type": "boolean", "description": "Resume Claude (default true); false = plain shell"}}, "required": ["id"]}},
        {"name": "focus_session", "description": "Show a session in its window and bring the window forward.",
         "inputSchema": {"type": "object", "properties": {"id": id}, "required": ["id"]}},
        {"name": "rename_session", "description": "Set a session's display name; empty string resets to Claude's title.",
         "inputSchema": {"type": "object", "properties": {"id": id, "name": {"type": "string"}}, "required": ["id", "name"]}},
        {"name": "set_group", "description": "Move a session into a named group (created on demand); null or empty = ungrouped.",
         "inputSchema": {"type": "object", "properties": {"id": id, "group": {"type": ["string", "null"]}}, "required": ["id"]}},
        {"name": "style_group", "description": "Color a manual group or a project (sidebar section) and/or move it before another one. color: #rrggbb, empty resets; before: null = to the end.",
         "inputSchema": {"type": "object", "properties": {
            "group": {"type": "string"}, "project": {"type": "string", "description": "Project folder"},
            "color": {"type": "string"}, "before": {"type": ["string", "null"]}}}},
        {"name": "open_folder", "description": "Open a session's folder (or a path) in Finder / Explorer.",
         "inputSchema": {"type": "object", "properties": {"id": id, "path": {"type": "string"}}}},
        {"name": "pin_session", "description": "Pin (protect) or unpin a session.",
         "inputSchema": {"type": "object", "properties": {"id": id, "pinned": {"type": "boolean"}}, "required": ["id", "pinned"]}},
        {"name": "open_window", "description": "Open a new kip window with a terminal in a folder.",
         "inputSchema": {"type": "object", "properties": {"cwd": {"type": "string"}}}},
        {"name": "move_to_window", "description": "Move a session to another window (omit window = a new one). A live session keeps running.",
         "inputSchema": {"type": "object", "properties": {"id": id, "window": {"type": "integer"}}, "required": ["id"]}},
        {"name": "save_state", "description": "Write kip's sessions and windows to disk now.",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "read_screen", "description": "Text currently on a session's terminal (or its snapshot if asleep).",
         "inputSchema": {"type": "object", "properties": {"id": id, "lines": {"type": "integer", "description": "Last N lines (default 60)"}}, "required": ["id"]}},
        {"name": "search", "description": "Case-insensitive text search in sessions' screen and scrollback (sleeping ones: their snapshot). Sessions with hits, count, up to 5 newest matching lines.",
         "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}, "id": {"type": "integer", "description": "Only this session"}}, "required": ["query"]}},
        {"name": "read_transcript", "description": "The user/Claude messages of a Claude conversation (tail), for summaries: a kip session id, or sid for a past one.",
         "inputSchema": {"type": "object", "properties": {"id": id, "sid": {"type": "string", "description": "Claude session id (from list_history)"}, "max_chars": {"type": "integer", "description": "Default 20000"}}}},
        {"name": "show_history", "description": "Open kip's history browser in a window, optionally with a search query and a conversation (sid) selected.",
         "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}, "sid": {"type": "string"}, "window": {"type": "integer"}}}},
        {"name": "list_history", "description": "Past Claude conversations on disk, newest first: sid, title, first prompt, folder, branch, age, hits. query searches titles and message text. Continue one with open_session(cwd, resume=sid).",
         "inputSchema": {"type": "object", "properties": {
            "query": {"type": "string"},
            "cwd": {"type": "string", "description": "Only this folder and below"},
            "limit": {"type": "integer", "description": "Default 20"}}}},
        {"name": "send_input", "description": "Type text into a live session (Enter appended by default). Off unless enabled in kip settings.",
         "inputSchema": {"type": "object", "properties": {"id": id, "text": {"type": "string"}, "enter": {"type": "boolean"}}, "required": ["id", "text"]}},
        {"name": "get_settings", "description": "All kip settings (font, theme, notifications, finished-session glow, idle suspend, group_by_project...).",
         "inputSchema": {"type": "object", "properties": {}}},
        {"name": "set_settings", "description": "Change kip settings: pass only the keys to change, names and value types as get_settings returns them.",
         "inputSchema": {"type": "object", "additionalProperties": true}},
        {"name": "cleanup", "description": "Find idle sessions (not pinned, not busy, not on screen) and suspend live ones or remove sleeping ones. dry_run (default true) only lists them.",
         "inputSchema": {"type": "object", "properties": {
            "idle_minutes": {"type": "integer", "description": "Default 120"},
            "action": {"type": "string", "enum": ["suspend", "remove"]},
            "dry_run": {"type": "boolean"}}}}
    ])
}

/// Start kip and wait for its control channel to come up.
fn launch() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let bundle = exe.ancestors().find(|p| p.extension().is_some_and(|e| e == "app")).map(|p| p.to_path_buf());
    let mut cmd = match &bundle {
        Some(app) if cfg!(target_os = "macos") => {
            let mut c = std::process::Command::new("open");
            c.arg("-a").arg(app);
            c
        },
        _ => std::process::Command::new(&exe),
    };
    cmd.stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    cmd.spawn().map_err(|e| format!("cannot start kip: {e}"))?;
    let until = Instant::now() + Duration::from_secs(20);
    while Instant::now() < until {
        std::thread::sleep(Duration::from_millis(300));
        if ctl::call("ping", json!({})).is_ok() {
            return Ok(());
        }
    }
    Err("kip did not start".into())
}

fn call(op: &str, args: Value) -> Result<Value, String> {
    match ctl::call(op, args.clone()) {
        Err(e) if e == "not running" => {
            launch()?;
            ctl::call(op, args)
        },
        r => r,
    }
}

pub fn serve() {
    let mut out = std::io::stdout();
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
        let Some(id) = msg.get("id").cloned() else { continue };
        let params = &msg["params"];
        let reply = match msg["method"].as_str().unwrap_or_default() {
            "initialize" => json!({"result": {
                "protocolVersion": params["protocolVersion"].as_str().unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "kip", "version": env!("CARGO_PKG_VERSION")},
                "instructions": INSTRUCTIONS}}),
            "ping" => json!({"result": {}}),
            "tools/list" => json!({"result": {"tools": tools()}}),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or_default();
                let args = if params["arguments"].is_object() { params["arguments"].clone() } else { json!({}) };
                let (text, err) = match call(name, args) {
                    Ok(Value::String(s)) => (s, false),
                    Ok(v) => (serde_json::to_string_pretty(&v).unwrap_or_default(), false),
                    Err(e) => (e, true),
                };
                json!({"result": {"content": [{"type": "text", "text": text}], "isError": err}})
            },
            m => json!({"error": {"code": -32601, "message": format!("unknown method {m}")}}),
        };
        let mut reply = reply;
        reply["jsonrpc"] = json!("2.0");
        reply["id"] = id;
        if writeln!(out, "{reply}").and_then(|_| out.flush()).is_err() {
            break;
        }
    }
}

fn claude_json() -> Option<std::path::PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR") {
        Some(d) => Some(std::path::PathBuf::from(d).join(".claude.json")),
        None => Some(dirs::home_dir()?.join(".claude.json")),
    }
}

/// kip is registered as a user-scope MCP server in Claude Code.
pub fn installed() -> bool {
    claude_json()
        .and_then(|p| std::fs::read(p).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .is_some_and(|v| v["mcpServers"]["kip"].is_object())
}

/// Register this kip binary with Claude Code (`claude mcp add -s user kip`).
pub fn install(claude_cmd: &str) -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let exe = exe.to_string_lossy();
    #[cfg(not(windows))]
    let out = {
        let q = format!("'{}'", exe.replace('\'', r"'\''"));
        let script = format!("{claude_cmd} mcp remove -s user kip >/dev/null 2>&1; {claude_cmd} mcp add -s user kip -- {q} mcp");
        let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        std::process::Command::new(sh).args(["-i", "-l", "-c", &script]).stdin(std::process::Stdio::null()).output()
    };
    #[cfg(windows)]
    let out = {
        let script = format!("{claude_cmd} mcp remove -s user kip *> $null; {claude_cmd} mcp add -s user kip -- '{exe}' mcp");
        std::process::Command::new("powershell.exe").args(["-NoLogo", "-Command", &script]).output()
    };
    let out = out.map_err(|e| e.to_string())?;
    if out.status.success() && installed() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("claude mcp add failed");
        Err(last.trim().to_string())
    }
}

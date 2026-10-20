//! Control channel: a running kip answers JSON requests on 127.0.0.1 (one
//! request per line, one reply per line). The port and a secret token sit in
//! ~/.kip/ctl.json, readable by this user only. `kip mcp` is the MCP front end.
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::time::Duration;

use serde_json::{json, Value};

pub struct Req {
    pub op: String,
    pub args: Value,
    pub reply: Sender<Result<Value, String>>,
}

fn info_path() -> Option<PathBuf> {
    Some(dirs::home_dir()?.join(".kip").join("ctl.json"))
}

fn token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::new();
    for _ in 0..2 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        h.write_u32(std::process::id());
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

fn write_private(path: &PathBuf, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(&tmp)?.write_all(text.as_bytes())?;
    std::fs::rename(&tmp, path)
}

/// Start answering. A second kip leaves the channel to the one already running.
pub fn start(tx: Sender<Req>, ctx: egui::Context) {
    if call("ping", json!({})).is_ok() {
        return;
    }
    let Ok(listener) = TcpListener::bind("127.0.0.1:0") else { return };
    let Ok(addr) = listener.local_addr() else { return };
    let token = token();
    let Some(path) = info_path() else { return };
    let info = json!({"port": addr.port(), "token": token, "pid": std::process::id()});
    if write_private(&path, &info.to_string()).is_err() {
        return;
    }
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (tx, ctx, token) = (tx.clone(), ctx.clone(), token.clone());
            std::thread::spawn(move || serve_conn(stream, tx, ctx, token));
        }
    });
}

fn serve_conn(stream: TcpStream, tx: Sender<Req>, ctx: egui::Context, token: String) {
    let Ok(mut out) = stream.try_clone() else { return };
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { return };
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(v) if v["token"].as_str() == Some(token.as_str()) => {
                let (rtx, rrx) = mpsc::channel();
                let req = Req {
                    op: v["op"].as_str().unwrap_or_default().to_string(),
                    args: v["args"].clone(),
                    reply: rtx,
                };
                if tx.send(req).is_err() {
                    return;
                }
                ctx.request_repaint();
                rrx.recv_timeout(Duration::from_secs(15)).unwrap_or_else(|_| Err("kip did not answer in time".into()))
            },
            Ok(_) => Err("bad token".into()),
            Err(e) => Err(format!("bad request: {e}")),
        };
        let msg = match reply {
            Ok(r) => json!({"ok": true, "result": r}),
            Err(e) => json!({"ok": false, "error": e}),
        };
        if writeln!(out, "{msg}").is_err() {
            return;
        }
    }
}

/// Send one request to the running kip. Err("not running") when nobody answers.
pub fn call(op: &str, args: Value) -> Result<Value, String> {
    let info: Value = info_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| serde_json::from_str(&t).ok())
        .ok_or("not running")?;
    let port = info["port"].as_u64().ok_or("not running")? as u16;
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(1)).map_err(|_| "not running")?;
    s.set_read_timeout(Some(Duration::from_secs(20))).ok();
    let req = json!({"token": info["token"], "op": op, "args": args});
    writeln!(s, "{req}").map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(s).read_line(&mut line).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&line).map_err(|_| "not running")?;
    if v["ok"].as_bool() == Some(true) {
        Ok(v["result"].clone())
    } else {
        Err(v["error"].as_str().unwrap_or("error").to_string())
    }
}

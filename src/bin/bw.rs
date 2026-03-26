//! `bw` — command-line companion for the build-watcher daemon.
//!
//! Discovers the daemon's HTTP port from
//! `~/.local/state/build-watcher/port`, then queries `/status` and prints
//! a human-readable summary.  This is a stub; a full TUI will be built on
//! top of this binary.

use std::io::{Read, Write};
use std::net::TcpStream;

use build_watcher::config::state_dir;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port_file = state_dir().join("port");
    let port_str = std::fs::read_to_string(&port_file).map_err(|e| {
        format!(
            "Could not read port file {}: {e}\nIs build-watcher running?",
            port_file.display()
        )
    })?;
    let port: u16 = port_str
        .trim()
        .parse()
        .map_err(|e| format!("Invalid port in {}: {e}", port_file.display()))?;

    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect(&addr)
        .map_err(|e| format!("Could not connect to build-watcher at {addr}: {e}"))?;

    write!(
        stream,
        "GET /status HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;
    stream.flush()?;

    let mut response = String::new();
    stream.read_to_string(&mut response)?;

    // Split off HTTP headers — the body starts after the blank line.
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b)
        .unwrap_or(&response);

    let json: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| format!("Invalid JSON from daemon: {e}\nBody: {body}"))?;

    if json["paused"].as_bool().unwrap_or(false) {
        println!("⏸ Notifications paused");
    }

    let watches = json["watches"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    if watches.is_empty() {
        println!("No active watches");
        return Ok(());
    }

    for w in watches {
        let repo = w["repo"].as_str().unwrap_or("?");
        let branch = w["branch"].as_str().unwrap_or("?");
        let active = w["active_runs"].as_array().map(|a| a.len()).unwrap_or(0);

        if active == 0 {
            let last = w["last_build"]
                .as_object()
                .map(|b| {
                    format!(
                        " (last: {} — {})",
                        b["conclusion"].as_str().unwrap_or("?"),
                        b["title"].as_str().unwrap_or("?"),
                    )
                })
                .unwrap_or_default();
            println!("- {repo} [{branch}] — idle{last}");
        } else {
            println!("- {repo} [{branch}] — {active} active run(s)");
        }
    }

    Ok(())
}

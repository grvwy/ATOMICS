//! The M1 demo: a native client of the `atomics.v1` protocol. It connects to a running ATOMICS
//! that has `[remote] enabled = true`, prints the board, then the first file of the first diff.
//!
//!     cargo run -p atomics --example loopback_client            # reads protocol.json
//!     cargo run -p atomics --example loopback_client -- --follow  # keep printing changes
//!     cargo run -p atomics --example loopback_client -- ws://127.0.0.1:PORT TOKEN
//!
//! With no arguments it finds `protocol.json` in the ATOMICS state directory (`ATOMICS_HOME`).

use std::net::TcpStream;
use std::time::{Duration, Instant};

use atomics_core::mirror::Mirror;
use atomics_core::wire::{self, CAPABILITIES, Envelope, Message, Seq, VERSION};
use tungstenite::client::IntoClientRequest;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let follow = args.iter().position(|a| a == "--follow").map(|i| args.remove(i)).is_some();
    let (url, token) = match args.as_slice() {
        [url, token] => (url.clone(), token.clone()),
        [] => {
            let path = atomics_core::paths::atomics_home().join("protocol.json");
            let text = std::fs::read_to_string(&path)
                .map_err(|e| format!("{}: {e} (is [remote] enabled = true in the config, and ATOMICS running?)", path.display()))?;
            let advert: serde_json::Value = serde_json::from_str(&text)?;
            (advert["url"].as_str().ok_or("no url")?.to_string(), advert["token"].as_str().ok_or("no token")?.to_string())
        }
        _ => return Err("usage: loopback_client [--follow] [URL TOKEN]".into()),
    };

    let mut request = url.as_str().into_client_request()?;
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse()?);
    request.headers_mut().insert("Sec-WebSocket-Protocol", wire::SUBPROTOCOL.parse()?);
    let stream = TcpStream::connect(request.uri().authority().ok_or("no host in the url")?.as_str())?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    let (mut socket, _) = tungstenite::client(request, stream).map_err(|e| format!("upgrade refused: {e}"))?;

    let send = |socket: &mut tungstenite::WebSocket<TcpStream>, message: Message| {
        let envelope = Envelope::new(message);
        socket.send(tungstenite::Message::text(serde_json::to_string(&envelope).expect("serializes")))
    };
    send(
        &mut socket,
        Message::Hello {
            client_id: "loopback-client".into(),
            versions: vec![VERSION],
            capabilities: CAPABILITIES.iter().map(|c| c.to_string()).collect(),
            prior_epoch: None,
            prior_cursor: None,
        },
    )?;

    let mut mirror = Mirror::default();
    let mut asked = false;
    let started = Instant::now();
    let mut quiet_since = Instant::now();
    loop {
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                quiet_since = Instant::now();
                let envelope = wire::decode(text.as_str()).map_err(|e| format!("bad frame from the host: {e}"))?;
                match &envelope.message {
                    Message::DiffFile { body } => {
                        println!("\ndiff {} @ revision {}", body.worktree, body.revision);
                        for hunk in &body.file.hunks {
                            println!("{}", hunk.header);
                            for line in &hunk.lines {
                                let mark = match line.kind {
                                    atomics_core::diff::LineKind::Added => '+',
                                    atomics_core::diff::LineKind::Removed => '-',
                                    _ => ' ',
                                };
                                println!("{mark}{}", line.text);
                            }
                        }
                        if !follow {
                            return Ok(());
                        }
                    }
                    Message::Error { code, message } => eprintln!("host error {code:?}: {message}"),
                    Message::Ping { nonce } => send(&mut socket, Message::Pong { nonce: nonce.clone() })?,
                    _ => {
                        if let Err(gap) = mirror.apply(&envelope) {
                            eprintln!("out of step ({gap:?}); asking for a fresh snapshot");
                            send(&mut socket, Message::Resync { reason: Some("gap".into()) })?;
                        } else if follow && asked {
                            print_board(&mirror);
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e)) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {
                // The host has gone quiet: the board is complete.
                if quiet_since.elapsed() > Duration::from_millis(300) || started.elapsed() > Duration::from_secs(5) {
                    if !asked {
                        print_board(&mirror);
                        asked = true;
                        let first = mirror.diffs.values().find_map(|d| {
                            d.files.first().and_then(|f| f.new_path.clone().or(f.old_path.clone())).map(|p| (d.worktree.clone(), p))
                        });
                        match first {
                            Some((worktree, path)) => {
                                send(&mut socket, Message::DiffGet { worktree, path, revision: None })?;
                                continue;
                            }
                            None if !follow => {
                                println!("\n(no diffs yet)");
                                return Ok(());
                            }
                            None => {}
                        }
                    } else if !follow {
                        return Err("the host did not send the diff".into());
                    }
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
}

fn print_board(m: &Mirror) {
    println!("epoch {} cursor {}", m.epoch.as_deref().unwrap_or("?"), Seq(m.cursor).0);
    println!("\nsessions ({})", m.sessions.len());
    for s in m.sessions.values() {
        println!("  {:<12} {:<10} {:?}{}", s.id, s.agent, s.status, if s.chief { " (chief)" } else { "" });
    }
    println!("plans ({})", m.plans.len());
    for p in m.plans.values() {
        println!("  {:<12} {:?} {} ({} tasks)", p.id, p.status, p.goal, p.tasks.len());
    }
    println!("approvals ({})", m.approvals.len());
    for a in m.approvals.values() {
        println!("  {} {} {}", a.session, a.tool, a.command.as_deref().unwrap_or(""));
    }
    println!("diffs ({})", m.diffs.len());
    for d in m.diffs.values() {
        println!("  {} +{} -{} in {} files (vs {})", d.worktree, d.additions(), d.deletions(), d.files.len(), d.base_ref);
    }
}

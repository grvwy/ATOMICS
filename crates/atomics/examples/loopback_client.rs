//! The M1 demo: a native client of the `atomics.v1` protocol. It connects to a running ATOMICS
//! that has `[remote] enabled = true`, prints the board, then the first file of the first diff.
//!
//!     cargo run -p atomics --example loopback_client            # reads protocol.json
//!     cargo run -p atomics --example loopback_client -- --follow  # keep printing changes
//!     cargo run -p atomics --example loopback_client -- --screen SESSION [--follow]  # a terminal
//!     cargo run -p atomics --example loopback_client -- --screen SESSION --type 'ls\r'  # type into it
//!     cargo run -p atomics --example loopback_client -- ws://127.0.0.1:PORT TOKEN
//!
//! With no arguments it finds `protocol.json` in the ATOMICS state directory (`ATOMICS_HOME`).

use std::net::TcpStream;
use std::time::{Duration, Instant};

use atomics_core::lease::Typist;
use atomics_core::mirror::Mirror;
use atomics_core::screen::ScreenMirror;
use atomics_core::wire::{self, CAPABILITIES, Envelope, Message, Seq, VERSION};
use tungstenite::client::IntoClientRequest;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let follow = args.iter().position(|a| a == "--follow").map(|i| args.remove(i)).is_some();
    let screen = match args.iter().position(|a| a == "--screen") {
        Some(i) if i + 1 < args.len() => {
            args.remove(i);
            Some(args.remove(i))
        }
        Some(_) => return Err("--screen needs a session id (see the board)".into()),
        None => None,
    };
    // `\r`, `\n`, `\t` and `\e` in the text stand for Enter, newline, Tab and Escape.
    let typed = match args.iter().position(|a| a == "--type") {
        Some(i) if i + 1 < args.len() && screen.is_some() => {
            args.remove(i);
            let text = args.remove(i);
            Some(text.replace("\\r", "\r").replace("\\n", "\n").replace("\\t", "\t").replace("\\e", "\x1b"))
        }
        Some(_) => return Err("--type needs --screen SESSION and the text to type".into()),
        None => None,
    };
    let (url, token) = match args.as_slice() {
        [url, token] => (url.clone(), token.clone()),
        [] => {
            let path = atomics_core::paths::atomics_home().join("protocol.json");
            // Refuses (and removes) a file left by an app that has since exited.
            let advert = atomics_core::advert::read(&path).map_err(|e| {
                format!("{}: {e} (is [remote] enabled = true in the config, and ATOMICS running?)", path.display())
            })?;
            (advert.url, advert.token)
        }
        _ => return Err("usage: loopback_client [--follow] [--screen SESSION] [URL TOKEN]".into()),
    };

    let mut request = url.as_str().into_client_request()?;
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse()?);
    request.headers_mut().insert("Sec-WebSocket-Protocol", wire::SUBPROTOCOL.parse()?);
    let stream = TcpStream::connect(request.uri().authority().ok_or("no host in the url")?.as_str())?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    // Keystrokes leave at once rather than waiting to be coalesced.
    stream.set_nodelay(true)?;
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
    if let Some(session) = screen {
        return watch_screen(&mut socket, &session, follow, typed);
    }

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
                    Message::DiffFile { body, truncated, next_offset } => {
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
                        match (truncated, next_offset) {
                            // A long file comes in pages: ask for the next one at the same revision.
                            (true, Some(offset)) => {
                                let path = body
                                    .file
                                    .new_path
                                    .clone()
                                    .or_else(|| body.file.old_path.clone())
                                    .unwrap_or_default();
                                let request = Message::DiffGet {
                                    worktree: body.worktree.clone(),
                                    path,
                                    revision: Some(Seq(body.revision)),
                                    offset: Some(*offset),
                                };
                                send(&mut socket, request)?;
                            }
                            _ if !follow => return Ok(()),
                            _ => {}
                        }
                    }
                    Message::Error { code, message } => eprintln!("host error {code:?}: {message}"),
                    Message::Ping { nonce } => send(&mut socket, Message::Pong { nonce: nonce.clone() })?,
                    _ => {
                        if let Err(gap) = mirror.apply(&envelope) {
                            eprintln!("out of step ({gap:?}); asking for a fresh snapshot");
                            send(&mut socket, Message::Resync { reason: Some("gap".into()), screens: Vec::new() })?;
                        } else if follow && asked {
                            print_board(&mirror);
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                // The host has gone quiet: the board is complete.
                if quiet_since.elapsed() > Duration::from_millis(300) || started.elapsed() > Duration::from_secs(5) {
                    if !asked {
                        print_board(&mirror);
                        asked = true;
                        let first = mirror.diffs.values().find_map(|d| {
                            d.files
                                .first()
                                .and_then(|f| f.new_path.clone().or(f.old_path.clone()))
                                .map(|p| (d.worktree.clone(), p))
                        });
                        match first {
                            Some((worktree, path)) => {
                                send(&mut socket, Message::DiffGet { worktree, path, revision: None, offset: None })?;
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

/// Print one terminal's screen as text: once, or on every change with `--follow`. With `typed`,
/// take the terminal's lease (never from another client), type the text, release the lease and
/// print every change until the terminal has been quiet for the read timeout.
fn watch_screen(
    socket: &mut tungstenite::WebSocket<TcpStream>,
    session: &str,
    follow: bool,
    mut typed: Option<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let send = |socket: &mut tungstenite::WebSocket<TcpStream>, message: Message| {
        socket.send(tungstenite::Message::text(serde_json::to_string(&Envelope::new(message)).expect("serializes")))
    };
    let topics = vec![wire::Topic::Screen { session: session.into(), focused: true }];
    send(socket, Message::Subscribe { topics })?;
    let mut screen = ScreenMirror::default();
    let mut typist = Typist::new(session);
    let typing = typed.is_some();
    if typing {
        send(socket, typist.acquire(false))?;
    }
    loop {
        let text = match socket.read() {
            Ok(tungstenite::Message::Text(text)) => text,
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) =>
            {
                if typing && typed.is_none() && !follow {
                    return Ok(()); // typed, and the echo has settled
                }
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let envelope = wire::decode(text.as_str()).map_err(|e| format!("bad frame from the host: {e}"))?;
        let applied = match &envelope.message {
            Message::ScreenSnapshot(s) => screen.apply_snapshot(s),
            Message::ScreenDelta(d) => screen.apply_delta(d),
            Message::Error { code, message } => return Err(format!("host error {code:?}: {message}").into()),
            Message::Ping { nonce } => {
                send(socket, Message::Pong { nonce: nonce.clone() })?;
                continue;
            }
            Message::LeaseState(state) => {
                typist.apply(state);
                if typist.may_type()
                    && let Some(text) = typed.take()
                {
                    for frame in typist.input(&text) {
                        send(socket, frame)?;
                    }
                    if let Some(release) = typist.release() {
                        send(socket, release)?;
                    }
                }
                continue;
            }
            _ => continue,
        };
        match applied {
            Ok(atomics_core::screen::ScreenApplied::Changed) => {
                let s = screen.screen.as_ref().expect("a changed screen is held");
                println!(
                    "--- {session} {}x{} generation {} revision {} ---",
                    s.cols, s.rows, screen.generation, screen.revision
                );
                println!("{}", s.text());
                if !follow && !typing {
                    return Ok(());
                }
            }
            Ok(_) => {}
            Err(gap) => {
                eprintln!("out of step ({gap:?}); asking for a fresh snapshot");
                send(socket, Message::Resync { reason: Some("gap".into()), screens: vec![session.into()] })?;
            }
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
        println!(
            "  {} +{} -{} in {} files (vs {})",
            d.worktree,
            d.additions(),
            d.deletions(),
            d.files.len(),
            d.base_ref
        );
    }
}

//! The real bundle in headless Chrome. A page on another origin frames it and boots it over `postMessage`,
//! a third origin's frame keeps posting a boot of its own, and a fake broker passes the channel to the fake
//! host of the unit tests. The browser pairs (keeping its key in IndexedDB), says `hello`, reconnects with
//! a fresh ticket and resumes, and after a reload dials again with the key read back from IndexedDB. The
//! client's page is served under the strict CSP the bundle is built for.
//!
//! It needs the built bundle and Chrome, so it is ignored by default:
//!
//!     scripts/build-web.sh
//!     cargo test -p atomics-web --test browser -- --ignored
//!
//! `ATOMICS_WEB_DIST` names another bundle folder and `CHROME` another browser binary.
//! `ATOMICS_WEB_SCREENSHOT=out.png` also saves what the page shows once the board is in.

#[allow(dead_code)] // shared with the unit tests, which use more of it
#[path = "../src/fake_host.rs"]
mod fake_host;

use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use atomics_core::wire::Message;
use serde_json::json;
use tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tungstenite::http::HeaderValue;
use tungstenite::protocol::CloseFrame;
use tungstenite::protocol::frame::coding::CloseCode;

use fake_host::{FakeHost, HOST_ID};

/// The longest any one step may take; a step that takes longer fails, naming what it waited for.
const WAIT: Duration = Duration::from_secs(30);
/// The longest one read from DevTools may block.
const READ: Duration = Duration::from_secs(5);
/// The longest the whole test may take: then it says what it saw, stops the browser and exits.
const OVERALL: Duration = Duration::from_secs(150);

/// What the servers saw, in order, with a way to wait for a line.
#[derive(Default)]
struct Log {
    lines: Mutex<Vec<String>>,
    changed: Condvar,
}

impl Log {
    fn push(&self, line: impl Into<String>) {
        self.lines.lock().unwrap().push(line.into());
        self.changed.notify_all();
    }

    /// Wait until `count` lines equal `line`.
    fn wait(&self, line: &str, count: usize) {
        let deadline = Instant::now() + WAIT;
        let mut lines = self.lines.lock().unwrap();
        while lines.iter().filter(|l| *l == line).count() < count {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for {line:?} ×{count}; saw {:#?}", *lines);
            lines = self.changed.wait_timeout(lines, left).unwrap().0;
        }
    }

    fn has(&self, line: &str) -> bool {
        self.lines.lock().unwrap().iter().any(|l| l == line)
    }
}

/// The browser's console lines: what the page logged, and any CSP refusal.
fn console(log: &Path) {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    for line in text.lines().filter(|l| l.contains("CONSOLE") || l.contains("Content Security Policy")) {
        eprintln!("chrome: {line}");
    }
}

/// Kills the browser however the test ends, and shows the console of a failed run.
struct Browser {
    child: Arc<Mutex<Child>>,
    log: PathBuf,
}

impl Drop for Browser {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if thread::panicking() {
            console(&self.log);
        }
    }
}

/// Past [`OVERALL`], say what the servers saw and the browser logged, stop the browser and exit.
fn watchdog(child: Arc<Mutex<Child>>, seen: Arc<Log>, log: PathBuf) {
    thread::spawn(move || {
        thread::sleep(OVERALL);
        eprintln!("browser test: still running after {OVERALL:?}; the servers saw {:#?}", seen.lines.lock().unwrap());
        console(&log);
        if let Ok(mut child) = child.lock() {
            let _ = child.kill();
            let _ = child.wait();
        }
        std::process::exit(1);
    });
}

struct Reply {
    kind: &'static str,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn new(kind: &'static str, body: impl Into<Vec<u8>>) -> Self {
        Self { kind, headers: Vec::new(), body: body.into() }
    }
}

type Handler = dyn Fn(&str, &str, &[u8]) -> Option<Reply> + Send + Sync;

/// A plain HTTP/1.1 server, one thread per connection.
fn serve(listener: TcpListener, handler: Arc<Handler>) {
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let handler = handler.clone();
            thread::spawn(move || {
                let _ = answer(stream, &*handler);
            });
        }
    });
}

fn answer(mut stream: TcpStream, handler: &Handler) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut first = String::new();
    reader.read_line(&mut first)?;
    let mut parts = first.split_whitespace();
    let (method, target) = (parts.next().unwrap_or_default().to_string(), parts.next().unwrap_or("/").to_string());
    let mut length = 0;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 || header == "\r\n" {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body)?;
    let path = target.split('?').next().unwrap_or("/");
    let (status, reply) = match handler(&method, path, &body) {
        Some(reply) => ("200 OK", reply),
        None => ("404 Not Found", Reply::new("text/plain", "not found")),
    };
    write!(stream, "HTTP/1.1 {status}\r\nContent-Type: {}\r\nContent-Length: {}\r\n", reply.kind, reply.body.len())?;
    write!(stream, "Cache-Control: no-store\r\nConnection: close\r\n")?;
    for (name, value) in &reply.headers {
        write!(stream, "{name}: {value}\r\n")?;
    }
    write!(stream, "\r\n")?;
    stream.write_all(&reply.body)?;
    stream.flush()
}

fn content_type(path: &str) -> &'static str {
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("wasm") => "application/wasm",
        Some("css") => "text/css",
        Some("json") => "application/json",
        _ => "application/octet-stream",
    }
}

/// The broker and the host behind it: one browser channel at a time, each socket one channel.
fn broker(listener: TcpListener, host: Arc<Mutex<FakeHost>>, log: Arc<Log>, origin: String, cut: Arc<AtomicBool>) {
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut ticket = String::new();
            // tungstenite's handshake callback returns its own `ErrorResponse`.
            #[allow(clippy::result_large_err)]
            let check = |request: &Request, mut response: Response| -> Result<Response, ErrorResponse> {
                let header = |name: &str| -> Vec<String> {
                    request.headers().get_all(name).iter().filter_map(|v| v.to_str().ok()).map(str::to_string).collect()
                };
                let offered: Vec<String> = header("Sec-WebSocket-Protocol")
                    .iter()
                    .flat_map(|v| v.split(','))
                    .map(|p| p.trim().to_string())
                    .collect();
                let refuse = || {
                    let mut refusal = ErrorResponse::new(None);
                    *refusal.status_mut() = tungstenite::http::StatusCode::FORBIDDEN;
                    refusal
                };
                if header("Origin") != [origin.clone()] || !offered.iter().any(|p| p == "atomics.rendezvous.v1") {
                    return Err(refuse());
                }
                ticket = offered.iter().find_map(|p| p.strip_prefix("atomics.ticket.")).unwrap_or_default().to_string();
                response
                    .headers_mut()
                    .insert("Sec-WebSocket-Protocol", HeaderValue::from_static("atomics.rendezvous.v1"));
                Ok(response)
            };
            let Ok(mut socket) = tungstenite::accept_hdr(stream, check) else {
                log.push("upgrade refused");
                continue;
            };
            log.push(format!("dial {ticket}"));
            let _ = socket.get_mut().set_read_timeout(Some(Duration::from_millis(100)));
            host.lock().unwrap().join();
            let mut first = true;
            loop {
                if cut.swap(false, Ordering::SeqCst) {
                    let _ = socket.close(Some(CloseFrame { code: CloseCode::Away, reason: "".into() }));
                    let _ = socket.flush();
                    log.push("cut");
                    break;
                }
                let bytes = match socket.read() {
                    Ok(tungstenite::Message::Binary(bytes)) => bytes,
                    Ok(tungstenite::Message::Close(_)) => break,
                    Ok(_) => continue,
                    Err(tungstenite::Error::Io(e))
                        if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
                    {
                        continue;
                    }
                    Err(_) => break,
                };
                if first {
                    log.push(format!("first {} bytes, kind {}", bytes.len(), bytes[0]));
                    first = false;
                }
                let mut host = host.lock().unwrap();
                let before = host.received.len();
                let result = host.receive(&bytes);
                for message in &host.received[before..] {
                    if let Message::Hello { prior_epoch, .. } = message {
                        log.push(format!("hello {}", prior_epoch.as_deref().unwrap_or("new")));
                    }
                }
                drop(host);
                match result {
                    Ok(replies) => {
                        for reply in replies {
                            let _ = socket.send(tungstenite::Message::binary(reply));
                        }
                    }
                    Err(code) => {
                        log.push(format!("host closed {code}"));
                        let _ = socket.close(Some(CloseFrame { code: CloseCode::from(code), reason: "".into() }));
                        let _ = socket.flush();
                        break;
                    }
                }
            }
            host.lock().unwrap().left();
        }
    });
}

/// Unpadded or padded standard base64, as DevTools sends a screenshot.
fn base64(text: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => c - b'A',
        b'a'..=b'z' => c - b'a' + 26,
        b'0'..=b'9' => c - b'0' + 52,
        b'+' => 62,
        _ => 63,
    };
    let (mut out, mut acc, mut bits) = (Vec::new(), 0u32, 0);
    for c in text.bytes().filter(|c| *c != b'=') {
        acc = (acc << 6) | u32::from(value(c));
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// Save what the page shows, through the DevTools port Chrome wrote into its profile. Every step is
/// bounded by [`WAIT`], and every read by [`READ`].
fn screenshot(profile: &Path, out: &Path) {
    let deadline = Instant::now() + WAIT;
    let left = |what: &str| {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "screenshot: timed out waiting for {what}");
        left
    };
    let port = loop {
        let active = std::fs::read_to_string(profile.join("DevToolsActivePort")).unwrap_or_default();
        if let Some(port) = active.lines().next().filter(|p| !p.trim().is_empty()) {
            break port.trim().to_string();
        }
        left("Chrome to write DevToolsActivePort");
        thread::sleep(Duration::from_millis(100));
    };
    let connect = |what: &str| {
        let stream = TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().unwrap(), left(what))
            .unwrap_or_else(|e| panic!("screenshot: {what}: {e}"));
        stream.set_read_timeout(Some(READ)).unwrap();
        stream.set_write_timeout(Some(READ)).unwrap();
        stream
    };
    // DevTools may keep the connection open: read the headers, then exactly Content-Length bytes.
    let mut http = BufReader::new(connect("the DevTools HTTP port"));
    write!(http.get_mut(), "GET /json/list HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").unwrap();
    let mut length = None;
    loop {
        let mut header = String::new();
        http.read_line(&mut header).unwrap_or_else(|e| panic!("screenshot: reading /json/list's headers: {e}"));
        if header == "\r\n" || header.is_empty() {
            break;
        }
        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let mut body = vec![0; length.expect("screenshot: /json/list has a Content-Length")];
    http.read_exact(&mut body).unwrap_or_else(|e| panic!("screenshot: reading /json/list: {e}"));
    let targets: serde_json::Value = serde_json::from_slice(&body).expect("screenshot: /json/list is JSON");
    let page = targets.as_array().unwrap().iter().find(|t| t["type"] == "page").expect("screenshot: a page target");
    let url = page["webSocketDebuggerUrl"].as_str().expect("screenshot: the page's DevTools socket");
    let (mut devtools, _) = tungstenite::client(url, connect("the DevTools socket"))
        .unwrap_or_else(|e| panic!("screenshot: the DevTools handshake: {e}"));
    let ask = json!({ "id": 1, "method": "Page.captureScreenshot", "params": { "format": "png" } });
    devtools.send(tungstenite::Message::text(ask.to_string())).unwrap();
    loop {
        left("DevTools to answer Page.captureScreenshot");
        let reply = match devtools.read() {
            Ok(tungstenite::Message::Text(reply)) => reply,
            Ok(_) => continue,
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) =>
            {
                continue;
            }
            Err(e) => panic!("screenshot: the DevTools socket: {e}"),
        };
        let reply: serde_json::Value = serde_json::from_str(reply.as_str()).unwrap();
        if reply["id"] == 1 {
            let data = reply["result"]["data"].as_str().unwrap_or_else(|| panic!("screenshot: DevTools said {reply}"));
            std::fs::write(out, base64(data)).unwrap();
            return;
        }
    }
}

fn chrome() -> String {
    if let Ok(chrome) = std::env::var("CHROME") {
        return chrome;
    }
    ["google-chrome-stable", "google-chrome", "chromium", "chromium-browser"]
        .into_iter()
        .find(|name| Command::new(name).arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok())
        .expect("Chrome or Chromium on PATH, or CHROME")
        .to_string()
}

#[test]
#[ignore = "needs the built bundle (scripts/build-web.sh) and Chrome"]
fn the_bundle_pairs_reconnects_and_reloads_in_a_browser() {
    let dist = std::env::var_os("ATOMICS_WEB_DIST")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("dist"));
    assert!(dist.join("index.html").is_file(), "no bundle in {}: run scripts/build-web.sh", dist.display());

    let mut host = fake_host::host();
    let pairing = host.open_pairing();
    let host = Arc::new(Mutex::new(host));
    let log = Arc::new(Log::default());
    let bind = || TcpListener::bind("127.0.0.1:0").unwrap();
    let (client, parent, tunnel) = (bind(), bind(), bind());
    let port = |l: &TcpListener| l.local_addr().unwrap().port();
    let client_origin = format!("http://127.0.0.1:{}", port(&client));
    // The parent's server answers as `localhost` and, for the attacker's frame, as `127.0.0.1`: two origins.
    let parent_origin = format!("http://localhost:{}", port(&parent));
    let other_origin = format!("http://127.0.0.1:{}", port(&parent));
    let broker_url = format!("ws://127.0.0.1:{}/atomics/tunnel", port(&tunnel));

    let fragment = pairing.fragment();
    let keys: Vec<&str> = fragment.split('.').collect();
    let boot = |ticket: &str| {
        json!({
            "type": "atomics.embed.boot", "v": 1, "host_id": HOST_ID, "ticket": ticket, "broker": broker_url,
            "display_name": "A Person", "theme": "dark", "scheme": "dark", "locale": "en", "view": "board",
        })
    };
    let mut pair = boot("ticket-0");
    pair["mode"] = json!("pair");
    pair["pair"] = json!({ "v": 1, "host_noise_pubkey": keys[2], "psk": keys[3] });
    let session = boot("ticket-s");
    let evil = boot("evil");

    let csp = format!(
        "default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self'; connect-src 'self' ws://127.0.0.1:{}; frame-ancestors {parent_origin}",
        port(&tunnel)
    );
    let config = format!("window.ATOMICS_PARENT_ORIGIN = '{parent_origin}';\n");
    serve(
        client,
        Arc::new(move |_, path, _| {
            let path = if path == "/" { "/index.html" } else { path };
            if path == "/config.js" {
                return Some(Reply::new("text/javascript", config.clone()));
            }
            let file = path.trim_start_matches('/');
            if file.split('/').any(|part| part == ".." || part.is_empty()) {
                return None;
            }
            let mut reply = Reply::new(content_type(file), std::fs::read(dist.join(file)).ok()?);
            if file == "index.html" {
                reply.headers.push(("Content-Security-Policy", csp.clone()));
            }
            Some(reply)
        }),
    );

    let phase = Arc::new(Mutex::new("pair".to_string()));
    let page = include_str!("browser/parent.html")
        .replace("CLIENT_ORIGIN", &client_origin)
        .replace("OTHER_ORIGIN", &other_origin)
        .replace("PAIR_BOOT", &pair.to_string())
        .replace("SESSION_BOOT", &session.to_string());
    let attacker = include_str!("browser/other.html").replace("EVIL_BOOT", &evil.to_string());
    {
        let (log, phase) = (log.clone(), phase.clone());
        serve(
            parent,
            Arc::new(move |method, path, body| match (method, path) {
                ("GET", "/") => Some(Reply::new("text/html; charset=utf-8", page.clone())),
                ("GET", "/other") => Some(Reply::new("text/html; charset=utf-8", attacker.clone())),
                ("GET", "/phase") => Some(Reply::new("text/plain", phase.lock().unwrap().clone())),
                ("POST", "/report") => {
                    log.push(format!("parent {}", String::from_utf8_lossy(body)));
                    Some(Reply::new("text/plain", ""))
                }
                _ => None,
            }),
        );
    }
    let cut = Arc::new(AtomicBool::new(false));
    broker(tunnel, host.clone(), log.clone(), client_origin.clone(), cut.clone());

    let profile = tempfile::tempdir().unwrap();
    let chrome_log = profile.path().join("chrome.log");
    let child = Command::new(chrome())
        .args([
            "--headless=new".to_string(),
            format!("--user-data-dir={}", profile.path().join("profile").display()),
            "--no-first-run".into(),
            "--no-default-browser-check".into(),
            "--disable-extensions".into(),
            "--use-angle=swiftshader".into(),
            "--enable-unsafe-swiftshader".into(),
            "--window-size=420,900".into(),
            "--enable-logging=stderr".into(),
            "--remote-debugging-port=0".into(),
            format!("{parent_origin}/"),
        ])
        .stdout(Stdio::null())
        .stderr(File::create(&chrome_log).unwrap())
        .spawn()
        .expect("Chrome starts");
    let child = Arc::new(Mutex::new(child));
    watchdog(child.clone(), log.clone(), chrome_log.clone());
    let _browser = Browser { child, log: chrome_log };

    // Pairing: the boot, the IKpsk2 channel, the host's answer, then hello on a board this page never had.
    log.wait("parent atomics.ready", 1);
    log.wait("parent atomics.booted", 1);
    log.wait("dial ticket-0", 1);
    log.wait("first 97 bytes, kind 1", 1);
    log.wait("parent atomics.paired", 1);
    log.wait("hello new", 1);
    assert_eq!(host.lock().unwrap().allowed.len(), 1, "the host holds the browser's key");

    // The broker cuts the channel: a fresh ticket from the parent, the stored key, and a resume.
    cut.store(true, Ordering::SeqCst);
    log.wait("cut", 1);
    log.wait("parent atomics.ticket", 1);
    log.wait("dial ticket-1", 1);
    log.wait("first 97 bytes, kind 2", 1);
    log.wait("hello epoch-1", 1);
    if let Some(out) = std::env::var_os("ATOMICS_WEB_SCREENSHOT") {
        thread::sleep(Duration::from_secs(2));
        screenshot(&profile.path().join("profile"), Path::new(&out));
    }

    // A new page: the key comes back out of IndexedDB, and the host still knows it.
    *phase.lock().unwrap() = "session".into();
    log.wait("parent atomics.ready", 2);
    log.wait("parent atomics.booted", 2);
    log.wait("dial ticket-s", 1);
    log.wait("first 97 bytes, kind 2", 2);
    log.wait("hello new", 2);

    log.wait("parent other posted", 1);
    assert!(!log.has("dial evil"), "a boot from another origin was never taken");
    assert!(!log.has("upgrade refused"), "every upgrade carried the client's exact origin and the subprotocol");
    assert!(!log.lines.lock().unwrap().iter().any(|l| l.starts_with("host closed")), "no channel was refused");
}

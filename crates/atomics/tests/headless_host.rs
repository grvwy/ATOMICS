//! The Phase F gate for `atomics host`, run against the real binary with an isolated home:
//! with only the headless host running, a shell session starts through ctl, a hook gets its
//! reply, a plan persists, a loopback protocol client sees the board and types into the shell
//! under its lease, SIGTERM stops it cleanly, and the home is locked against a second host and
//! the desktop.

#[path = "../../../tests/support.rs"]
mod test_support;

use std::io::Write;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use atomics_core::lease::Typist;
use atomics_core::mirror::Mirror;
use atomics_core::wire::{self, CAPABILITIES, Envelope, Message, Topic, VERSION};
use serde_json::{Value, json};
use test_support::{CHILD_TIMEOUT, TempDir, TestChild, scaled_timeout};
use tungstenite::client::IntoClientRequest;

struct Instance {
    dir: TempDir,
}

impl Instance {
    /// `remote` is the `[remote]` port to enable (0 picks a free one); `None` leaves remote off.
    fn new(remote: Option<u16>) -> Self {
        let dir = test_support::tempdir().unwrap();
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir.path().join("runtime")).unwrap();
        // Agents run bare here: the test never reaches a real sandbox.
        let mut config = atomics_core::config::DEFAULT_CONFIG.replace("mode = \"sandbox\"", "mode = \"bare\"");
        if let Some(port) = remote {
            assert_eq!(config.matches("\n[remote]\n").count(), 1);
            config = config.replace("\n[remote]\n", &format!("\n[remote]\nenabled = true\nport = {port}\n"));
        }
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        Self { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("state")
    }

    fn socket(&self) -> PathBuf {
        self.dir.path().join("atomics.sock")
    }

    fn command<I: AsRef<std::ffi::OsStr>>(&self, args: impl IntoIterator<Item = I>) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_atomics"));
        command
            .args(args)
            .current_dir(self.dir.path())
            .env("HOME", self.dir.path())
            .env("ATOMICS_HOME", self.home())
            .env("ATOMICS_CONFIG", self.dir.path().join("config.toml"))
            .env("ATOMICS_SOCKET", self.socket())
            .env("XDG_RUNTIME_DIR", self.dir.path().join("runtime"))
            .env("CODEX_HOME", self.dir.path().join("codex"))
            .env("CLAUDE_CONFIG_DIR", self.dir.path().join("claude"))
            // A login shell such as zsh would run its first-run wizard in the fresh HOME.
            .env("SHELL", "/bin/sh")
            .env_remove("ATOMICS_PANE")
            .env_remove("TERM_PROGRAM");
        command
    }

    /// No display either: the host needs no window system (the Linux QA image runs Xvfb).
    fn host(&self) -> TestChild {
        let mut command = self.command(["host"]);
        command.env_remove("DISPLAY").env_remove("WAYLAND_DISPLAY").stdin(Stdio::null());
        TestChild::spawn(&mut command, CHILD_TIMEOUT).unwrap()
    }

    fn ctl(&self, request: Value) -> Value {
        let output = test_support::output(&mut self.command(["ctl".to_string(), request.to_string()]));
        assert!(
            output.status.success(),
            "ctl {request} failed: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let response: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(response["ok"], true, "{request} -> {response}");
        response["result"].clone()
    }

    /// Waits for the host to answer ctl, failing with its logs if it exits first.
    fn ready(&self, host: &mut TestChild) {
        let deadline = Instant::now() + scaled_timeout(Duration::from_secs(30));
        loop {
            if let Some(status) = host.child.try_wait().unwrap() {
                let output = host.wait_output().unwrap();
                panic!("host exited early with {status}: {}", String::from_utf8_lossy(&output.stderr));
            }
            let probe = test_support::output(&mut self.command(["ctl", r#"{"cmd":"list"}"#]));
            if probe.status.success() {
                return;
            }
            assert!(Instant::now() < deadline, "host never answered ctl");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = Instant::now() + scaled_timeout(Duration::from_secs(20));
    while !check() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}

type Socket = tungstenite::WebSocket<TcpStream>;

/// A native `atomics.v1` client, as `examples/loopback_client.rs` connects: it reads
/// protocol.json, authenticates and says hello.
fn connect(home: &Path) -> Socket {
    let advert: Value = serde_json::from_str(&std::fs::read_to_string(home.join("protocol.json")).unwrap()).unwrap();
    let mut request = advert["url"].as_str().unwrap().into_client_request().unwrap();
    let token = advert["token"].as_str().unwrap();
    request.headers_mut().insert("Authorization", format!("Bearer {token}").parse().unwrap());
    request.headers_mut().insert("Sec-WebSocket-Protocol", wire::SUBPROTOCOL.parse().unwrap());
    let stream = TcpStream::connect(request.uri().authority().unwrap().as_str()).unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    stream.set_nodelay(true).unwrap();
    let (mut socket, _) = tungstenite::client(request, stream).unwrap();
    let hello = Envelope::new(Message::Hello {
        client_id: "headless-gate".into(),
        versions: vec![VERSION],
        capabilities: CAPABILITIES.iter().map(|c| c.to_string()).collect(),
        prior_epoch: None,
        prior_cursor: None,
    });
    send(&mut socket, hello);
    socket
}

fn send(socket: &mut Socket, envelope: Envelope) {
    socket.send(tungstenite::Message::text(serde_json::to_string(&envelope).unwrap())).unwrap();
}

/// Take `session`'s lease and type `text` into it, then let go.
fn type_into(home: &Path, session: &str, text: &str) {
    let mut socket = connect(home);
    let topics = vec![Topic::Screen { session: session.into(), focused: true }];
    send(&mut socket, Envelope::new(Message::Subscribe { topics }));
    let mut typist = Typist::new(session);
    send(&mut socket, Envelope::new(typist.acquire(false)));
    let deadline = Instant::now() + scaled_timeout(Duration::from_secs(20));
    while !typist.may_type() {
        assert!(Instant::now() < deadline, "the lease was never granted");
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => match wire::decode(text.as_str()).unwrap().message {
                Message::LeaseState(state) => typist.apply(&state),
                Message::Error { code, message } => panic!("{code:?}: {message}"),
                _ => {}
            },
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("protocol connection failed: {e}"),
        }
    }
    for frame in typist.input(text) {
        send(&mut socket, Envelope::new(frame));
    }
    send(&mut socket, Envelope::new(typist.release().unwrap()));
    let _ = socket.close(None);
}

/// Mirror the board until `done` holds.
fn board(home: &Path, done: impl Fn(&Mirror) -> bool) -> Mirror {
    let mut socket = connect(home);
    let mut mirror = Mirror::default();
    let deadline = Instant::now() + scaled_timeout(Duration::from_secs(20));
    while !(mirror.epoch.is_some() && done(&mirror)) {
        assert!(Instant::now() < deadline, "the board never showed the expected state");
        match socket.read() {
            Ok(tungstenite::Message::Text(text)) => {
                let envelope = wire::decode(text.as_str()).unwrap();
                if let Message::Ping { nonce } = &envelope.message {
                    send(&mut socket, Envelope::new(Message::Pong { nonce: nonce.clone() }));
                } else {
                    mirror.apply(&envelope).unwrap();
                }
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
            Err(e) => panic!("protocol connection failed: {e}"),
        }
    }
    let _ = socket.close(None);
    mirror
}

#[test]
fn headless_host_serves_ctl_hooks_plans_and_the_protocol_then_stops_on_sigterm() {
    let instance = Instance::new(Some(0));
    let mut host = instance.host();
    instance.ready(&mut host);

    // protocol.json is private and carries the token the logs never show.
    use std::os::unix::fs::PermissionsExt;
    let advert = instance.home().join("protocol.json");
    assert_eq!(std::fs::metadata(&advert).unwrap().permissions().mode() & 0o777, 0o600);
    let token = serde_json::from_str::<Value>(&std::fs::read_to_string(&advert).unwrap()).unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(token.len(), 64);

    // A window-only request fails clearly instead of waiting for a desktop.
    let screenshot = test_support::output(
        &mut instance.command(["ctl", &json!({"cmd":"screenshot","path":"/tmp/never.png"}).to_string()]),
    );
    assert!(String::from_utf8_lossy(&screenshot.stdout).contains("no window"), "{screenshot:?}");
    let list = instance.ctl(json!({"cmd":"list"}));
    assert!(list["window"].is_null(), "{list}");

    // A shell session starts through ctl and runs in a PTY with no client attached.
    let marker = instance.dir.path().join("shell.pid");
    let started = instance.ctl(json!({
        "cmd":"start", "agent":"shell", "isolation":"bare",
        "prompt": format!("echo $$ > '{}'; echo headless-ready", marker.display()),
    }));
    let session = started["id"].as_str().unwrap().to_string();
    eventually("the shell's output", || {
        let read = instance.ctl(json!({"cmd":"read","session":session,"lines":40}));
        read.to_string().contains("headless-ready") && marker.exists()
    });
    let shell: i32 = std::fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    assert!(alive(shell));

    // A hook gets its reply: a permission request waits for an approval given through ctl.
    let mut hook = TestChild::spawn(
        instance.command(["hook", "--socket"]).arg(instance.socket()).args(["--pane", &session]).stdin(Stdio::piped()),
        CHILD_TIMEOUT,
    )
    .unwrap();
    let request = json!({
        "hook_event_name":"PermissionRequest", "session_id":"headless-gate", "cwd":instance.dir.path(),
        "tool_name":"Bash", "tool_input":{"command":"touch approved-by-ctl"}
    });
    let mut stdin = hook.child.stdin.take().unwrap();
    stdin.write_all(request.to_string().as_bytes()).unwrap();
    drop(stdin);
    eventually("the pending approval", || {
        let list = instance.ctl(json!({"cmd":"list"}));
        list["sessions"].as_array().unwrap().iter().any(|s| s["id"] == session && !s["approval"].is_null())
    });
    instance.ctl(json!({"cmd":"approve","session":session,"decision":"allow"}));
    let reply = hook.wait_output().unwrap();
    assert!(reply.status.success());
    let reply: Value = serde_json::from_slice(&reply.stdout).unwrap();
    assert_eq!(reply["hookSpecificOutput"]["decision"]["behavior"], "allow", "{reply}");

    // Dropping an entry of the stash every worktree shares waits, and its approval says which entry
    // that is, to ctl and to protocol clients alike.
    let repo = instance.dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let git = |args: &[&str]| {
        let output = test_support::output(
            Command::new("git")
                .args(["-c", "user.name=test", "-c", "user.email=test@example.com"])
                .args(args)
                .current_dir(&repo),
        );
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "base\n").unwrap();
    git(&["add", "a.txt"]);
    git(&["commit", "-q", "-m", "base"]);
    std::fs::write(repo.join("a.txt"), "work in progress\n").unwrap();
    git(&["stash", "push", "-q", "-m", "ENG-123"]);
    let mut hook = TestChild::spawn(
        instance.command(["hook", "--socket"]).arg(instance.socket()).args(["--pane", &session]).stdin(Stdio::piped()),
        CHILD_TIMEOUT,
    )
    .unwrap();
    let request = json!({
        "hook_event_name":"PermissionRequest", "session_id":"headless-gate", "cwd":repo,
        "tool_name":"Bash", "tool_input":{"command":"git stash drop stash@{0}"}
    });
    let mut stdin = hook.child.stdin.take().unwrap();
    stdin.write_all(request.to_string().as_bytes()).unwrap();
    drop(stdin);
    let entry = "git stash drop drops stash@{0} (On main: ENG-123)";
    eventually("the stash warning", || {
        let list = instance.ctl(json!({"cmd":"list"}));
        list["sessions"].as_array().unwrap().iter().any(|s| {
            s["id"] == session && s["approval"]["warning"].as_str().is_some_and(|warning| warning.contains(entry))
        })
    });
    let mirror = board(&instance.home(), |mirror| {
        mirror.approvals.values().any(|approval| approval.warning.as_deref().is_some_and(|w| w.contains(entry)))
    });
    assert!(mirror.approvals.values().all(|approval| approval.session == session), "{:?}", mirror.approvals);
    instance.ctl(json!({"cmd":"approve","session":session,"decision":"deny"}));
    let reply: Value = serde_json::from_slice(&hook.wait_output().unwrap().stdout).unwrap();
    assert_eq!(reply["hookSpecificOutput"]["decision"]["behavior"], "deny", "{reply}");

    // A plan persists on disk.
    let plan = instance.ctl(json!({
        "cmd":"plan_set", "goal":"Headless gate",
        "tasks":[{"id":"t1","title":"First task","brief":"Nothing to do"}]
    }));
    let id = plan["plan"].as_str().unwrap().to_string();
    let saved = instance.home().join("plans").join(format!("{id}.json"));
    eventually("the saved plan", || std::fs::read_to_string(&saved).is_ok_and(|text| text.contains("Headless gate")));

    // A loopback protocol client connects and sees the board: the session and the plan.
    let mirror = board(&instance.home(), |mirror| {
        mirror.sessions.contains_key(&session) && mirror.plans.values().any(|p| p.goal == "Headless gate")
    });
    assert!(mirror.plans.values().any(|p| p.goal == "Headless gate" && p.id == id));

    // It types into the shell under the terminal's lease, and the command runs.
    type_into(&instance.home(), &session, "echo typed-$((40+2))\r");
    eventually("the typed command's output", || {
        instance.ctl(json!({"cmd":"read","session":session,"lines":40})).to_string().contains("typed-42")
    });

    // The home belongs to this host: a second host and the desktop are both refused. Should the
    // desktop ever start anyway, its screenshot request closes it at once.
    let never = instance.dir.path().join("never.png");
    let desktop = ["--size", "480x320", "--screenshot", never.to_str().unwrap(), "--screenshot-delay", "0"];
    for args in [&["host"][..], &desktop] {
        let refused = test_support::output(&mut instance.command(args));
        let stderr = String::from_utf8_lossy(&refused.stderr);
        assert_eq!(refused.status.code(), Some(1), "atomics {args:?}: {stderr}");
        assert!(stderr.contains(&format!("headless ATOMICS host (pid {})", host.child.id())), "{stderr}");
    }

    // SIGTERM stops it cleanly: exit 0, its agents gone, protocol.json and the socket removed.
    // SAFETY: the pid is our own child's.
    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    let output = host.wait_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{}: {stderr}", output.status);
    assert!(stderr.contains("SIGTERM received; stopping") && stderr.contains("atomics host: stopped"), "{stderr}");
    assert!(stderr.contains("atomics.v1 protocol on ws://127.0.0.1:"), "{stderr}");
    for log in [&output.stdout[..], &output.stderr] {
        assert!(!String::from_utf8_lossy(log).contains(&token), "the token reached a log");
    }
    assert!(!advert.exists(), "protocol.json outlived the host");
    assert!(!instance.socket().exists(), "the socket outlived the host");
    eventually("the shell to end", || !alive(shell));
    let crash_log = std::fs::read_to_string(instance.home().join("crash.log")).unwrap_or_default();
    assert!(crash_log.contains(&format!("exit {}", host.child.id())), "{crash_log}");

    // The lock went with the process: the home can be hosted again.
    let mut again = instance.host();
    instance.ready(&mut again);
    assert_eq!(unsafe { libc::kill(again.child.id() as i32, libc::SIGINT) }, 0);
    let output = again.wait_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn without_remote_there_is_no_protocol_server_and_mcp_still_reaches_the_host() {
    let instance = Instance::new(None);
    // An advert from an earlier run with remote on names a server that is gone.
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(instance.home()).unwrap();
    let advert = instance.home().join("protocol.json");
    std::fs::write(&advert, json!({"url":"ws://127.0.0.1:1","token":"stale","pid":1}).to_string()).unwrap();
    let mut host = instance.host();
    instance.ready(&mut host);
    assert!(!advert.exists(), "a stale protocol.json outlived the start of a host without remote");
    let plan = instance.ctl(json!({"cmd":"plan_set","goal":"Local only","tasks":[]}));
    let id = plan["plan"].as_str().unwrap().to_string();

    // `atomics mcp` reaches the host through the local socket, with no client attached.
    let mut mcp = TestChild::spawn(instance.command(["mcp"]).stdin(Stdio::piped()), CHILD_TIMEOUT).unwrap();
    let mut stdin = mcp.child.stdin.take().unwrap();
    for message in [
        json!({"jsonrpc":"2.0", "id":1, "method":"initialize", "params":{"protocolVersion":"2025-06-18"}}),
        json!({"jsonrpc":"2.0", "id":2, "method":"tools/call", "params":{"name":"atomics_plan_get", "arguments":{"plan":id}}}),
    ] {
        writeln!(stdin, "{message}").unwrap();
    }
    drop(stdin);
    let output = mcp.wait_output().unwrap();
    assert!(output.status.success());
    let replies: Vec<Value> = output
        .stdout
        .split(|&byte| byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    let got = replies.iter().find(|reply| reply["id"] == 2).expect("an answer to the tool call");
    assert_eq!(got["result"]["isError"], false, "{got}");
    assert!(got["result"]["content"][0]["text"].as_str().unwrap().contains("Local only"), "{got}");

    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    let output = host.wait_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(stderr.contains("protocol server off"), "{stderr}");
}

#[test]
fn with_remote_on_a_taken_port_the_host_fails_loudly_and_cleans_up() {
    // Something else already listens on the configured port.
    let taken = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = taken.local_addr().unwrap().port();
    let instance = Instance::new(Some(port));
    // An advert an earlier run left behind must not outlive this one either.
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(instance.home()).unwrap();
    let advert = instance.home().join("protocol.json");
    let stale = "5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e5ca1ab1e";
    std::fs::write(&advert, json!({"url":"ws://127.0.0.1:1","token":stale,"pid":1}).to_string()).unwrap();

    let output = instance.host().wait_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "a half-reachable host must fail: {stderr}");
    let expected = format!("[remote] is enabled but the protocol server cannot start (port {port}): ");
    assert!(stderr.contains(&expected), "{stderr}");
    assert!(!stderr.contains("protocol server off"), "the log must not claim remote is off: {stderr}");
    assert!(stderr.contains("atomics host: stopped"), "it stops as cleanly as on SIGTERM: {stderr}");
    for log in [&output.stdout[..], &output.stderr] {
        assert!(!String::from_utf8_lossy(log).contains(stale), "a token reached a log");
    }
    assert!(!advert.exists(), "clients must not find an advert for a server that is not running");
    assert!(!instance.socket().exists(), "the socket outlived the host");

    // Once the port is free the same config serves on it, and the failed run left the home unlocked.
    drop(taken);
    let mut host = instance.host();
    instance.ready(&mut host);
    let url = serde_json::from_str::<Value>(&std::fs::read_to_string(&advert).unwrap()).unwrap()["url"].clone();
    assert_eq!(url, format!("ws://127.0.0.1:{port}"));
    assert_eq!(unsafe { libc::kill(host.child.id() as i32, libc::SIGTERM) }, 0);
    let output = host.wait_output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

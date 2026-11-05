//! What tests of updating share: a release server on this machine, and keys made at test time. No key, real or
//! placeholder, is a trust anchor of any test.

use super::*;
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};

pub(super) const ARCHIVE_NAME: &str = "atomics-9.0.0-test-platform.tar.gz";

pub(super) struct TestKey {
    pair: ring::signature::Ed25519KeyPair,
}

impl TestKey {
    pub(super) fn new() -> TestKey {
        let rng = ring::rand::SystemRandom::new();
        let document = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        TestKey { pair: ring::signature::Ed25519KeyPair::from_pkcs8(document.as_ref()).unwrap() }
    }

    pub(super) fn raw(&self) -> [u8; 32] {
        use ring::signature::KeyPair;
        self.pair.public_key().as_ref().try_into().unwrap()
    }

    pub(super) fn sign(&self, bytes: &[u8]) -> Vec<u8> {
        self.pair.sign(bytes).as_ref().to_vec()
    }

    pub(super) fn pem(&self) -> String {
        let der = [ED25519_PUBLIC_KEY_PREFIX.as_slice(), self.raw().as_slice()].concat();
        let body = base64::engine::general_purpose::STANDARD.encode(der);
        format!("-----BEGIN PUBLIC KEY-----\n{body}\n-----END PUBLIC KEY-----\n")
    }
}

pub(super) fn sha256_line(bytes: &[u8], name: &str) -> Vec<u8> {
    let digest: String =
        ring::digest::digest(&ring::digest::SHA256, bytes).as_ref().iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{digest}  {name}\n").into_bytes()
}

/// What a server answers on each path: a status and a body.
type Routes = Arc<Mutex<HashMap<String, (u16, Vec<u8>)>>>;

/// A server on this machine that answers each path with what the test put there.
#[derive(Clone)]
pub(super) struct Server {
    base: String,
    routes: Routes,
}

impl Server {
    pub(super) fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Routes = Arc::default();
        let served = routes.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut reader = BufReader::new(&stream);
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() {
                    continue;
                }
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|read| read > 2) {
                    line.clear();
                }
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let (status, body) = served.lock().unwrap().get(path).cloned().unwrap_or((404, Vec::new()));
                let mut writer = &stream;
                let head =
                    format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                let _ = writer.write_all(head.as_bytes());
                let _ = writer.write_all(&body);
            }
        });
        Server { base, routes }
    }

    pub(super) fn put(&self, path: &str, status: u16, body: impl Into<Vec<u8>>) {
        self.routes.lock().unwrap().insert(path.into(), (status, body.into()));
    }

    pub(super) fn feed(&self, key: Result<[u8; 32], String>, target: InstallTarget) -> Feed {
        Feed {
            url: format!("{}/latest", self.base),
            keys: key.map(|key| vec![key]),
            platform: "test-platform".into(),
            target: Some(target),
            remote: false,
        }
    }

    /// Release 9.0.0 with these files for this platform.
    pub(super) fn release(&self, archive: &[u8], checksum: &[u8], signature: &[u8]) {
        let name = "atomics-9.0.0-test-platform.tar.gz";
        self.put(&format!("/{name}"), 200, archive);
        self.put(&format!("/{name}.sha256"), 200, checksum);
        self.put(&format!("/{name}.sig"), 200, signature);
        let asset = |suffix: &str| serde_json::json!({"name": format!("{name}{suffix}"), "browser_download_url": format!("{}/{name}{suffix}", self.base)});
        let answer = serde_json::json!({
            "tag_name": "v9.0.0",
            "html_url": "https://example.com/notes/9.0.0",
            "assets": [asset(""), asset(".sha256"), asset(".sig"), {"name":"other.zip","browser_download_url":"https://example.com/x"}],
        });
        self.put("/latest", 200, answer.to_string());
    }
}

/// A tar.gz of the files under `tree`, made by the tar a release's archive is unpacked with.
fn archive_of(tree: &Path) -> Vec<u8> {
    let out = tree.parent().unwrap().join("out.tar.gz");
    let status = crate::test_support::output(Command::new("tar").arg("-czf").arg(&out).arg("-C").arg(tree).arg("."));
    assert!(status.status.success(), "{}", String::from_utf8_lossy(&status.stderr));
    std::fs::read(out).unwrap()
}

pub(crate) const OLD: &str = "#!/bin/sh\necho old\n";
pub(crate) const NEW: &str = "#!/bin/sh\necho 'atomics 9.0.0 (v9.0.0)'\n";

/// An installed program, and an archive holding `program` for the release to replace it with.
pub(super) struct Install {
    pub(super) dir: crate::test_support::TempDir,
    pub(super) target: PathBuf,
}

impl Install {
    pub(super) fn new() -> Install {
        let dir = crate::test_support::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let target = bin.join("atomics");
        crate::test_support::write_program(&target, OLD);
        Install { dir, target }
    }

    pub(super) fn archive(&self, entries: &[(&str, &str)]) -> Vec<u8> {
        let tree = self.dir.path().join("tree");
        let _ = std::fs::remove_dir_all(&tree);
        for (path, text) in entries {
            let file = tree.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            crate::test_support::write_program(&file, text);
        }
        archive_of(&tree)
    }

    pub(super) fn untouched(&self) {
        assert_eq!(std::fs::read_to_string(&self.target).unwrap(), OLD, "the old install stays as it was");
        assert!(!self.target.with_file_name("atomics.previous").exists(), "and nothing has moved yet");
        self.no_leftovers();
    }

    pub(super) fn no_leftovers(&self) {
        let names: Vec<_> = std::fs::read_dir(self.target.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        let leftovers: Vec<_> =
            names.iter().filter(|name| !["atomics", "atomics.previous"].contains(&name.as_str())).collect();
        assert!(leftovers.is_empty(), "nothing is left beside the install but what it keeps: {leftovers:?}");
    }
}

/// A correctly signed release 9.0.0 on a server of its own, and the program it would replace.
pub(crate) struct Release {
    _install: Install,
    pub(crate) installed: PathBuf,
    server: Server,
    key: TestKey,
}

impl Release {
    pub(crate) fn signed() -> Release {
        let install = Install::new();
        let server = Server::start();
        let key = TestKey::new();
        let archive = install.archive(&[("atomics", NEW)]);
        server.release(&archive, &sha256_line(&archive, ARCHIVE_NAME), &key.sign(&archive));
        let installed = install.target.clone();
        Release { _install: install, installed, server, key }
    }
}

/// `release`, as a binary whose trust anchor is its key.
pub(crate) fn signed_feed_with(release: &Release) -> Mode {
    Mode::Releases(release.server.feed(Ok(release.key.raw()), InstallTarget::Binary(release.installed.clone())))
}

/// `release`, as a binary that trusts another key.
pub(crate) fn signed_feed_failing(release: &Release) -> Mode {
    Mode::Releases(release.server.feed(Ok(TestKey::new().raw()), InstallTarget::Binary(release.installed.clone())))
}

/// A version a check found, for a test of what the window does with one.
pub(crate) fn found(version: &str) -> Found {
    Found { version: version.into(), detail: None, notes: Some("https://example.com/notes".into()), assets: None }
}

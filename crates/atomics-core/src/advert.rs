//! `protocol.json`: how a client on this machine finds the running app's protocol server.
//!
//! The file is written at a fixed path, so one that outlives its app (a crash, a power cut) would
//! send a client to a dead or, worse, a recycled port. It therefore records the app's pid and the
//! process start time, and a reader accepts it only while that process is still the one that wrote
//! it; otherwise it is removed.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Advertisement {
    pub url: String,
    pub token: String,
    pub pid: u32,
    /// The process start time in the platform's own unit (clock ticks since boot on Linux), which
    /// tells this process from a later one that got the same pid. Absent where it cannot be read.
    #[serde(default)]
    pub pid_start: Option<u64>,
    /// When the server started, in Unix seconds; for people reading the file.
    #[serde(default)]
    pub started_at: u64,
}

impl std::fmt::Debug for Advertisement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Advertisement")
            .field("url", &self.url)
            .field("token", &"[redacted]")
            .field("pid", &self.pid)
            .field("pid_start", &self.pid_start)
            .field("started_at", &self.started_at)
            .finish()
    }
}

#[derive(Debug)]
pub enum AdvertError {
    Io(io::Error),
    /// Not a protocol.json.
    Malformed(String),
    /// The app that wrote it is gone (and the file was removed).
    Stale(&'static str),
}

impl std::fmt::Display for AdvertError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AdvertError::Io(e) => write!(f, "{e}"),
            AdvertError::Malformed(e) => write!(f, "not a valid protocol.json: {e}"),
            AdvertError::Stale(why) => write!(f, "left by an app that is no longer running ({why}); removed"),
        }
    }
}

impl std::error::Error for AdvertError {}

/// Start time of `pid` from `/proc/<pid>/stat` (field 22), or `None` where `/proc` has no answer.
pub fn process_start(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is parenthesised and may hold spaces; the fields that matter follow its end.
    let after = &stat[stat.rfind(')')? + 1..];
    after.split_whitespace().nth(19)?.parse().ok()
}

fn alive(pid: u32) -> bool {
    // 0 (and anything past i32) names no single process: `kill` would signal a process group.
    let Ok(pid @ 1..) = i32::try_from(pid) else { return false };
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) }
}

impl Advertisement {
    /// This process's advertisement for a server at `url` that clients authenticate to with `token`.
    pub fn current(url: &str, token: &str) -> Self {
        let pid = std::process::id();
        Self {
            url: url.into(),
            token: token.into(),
            pid,
            pid_start: process_start(pid),
            started_at: std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        }
    }

    /// Whether the process that wrote this is still running as the same process.
    pub fn check(&self) -> Result<(), AdvertError> {
        if !alive(self.pid) {
            return Err(AdvertError::Stale("its process has exited"));
        }
        match (self.pid_start, process_start(self.pid)) {
            (Some(wrote), Some(now)) if wrote != now => {
                Err(AdvertError::Stale("its pid now belongs to another process"))
            }
            _ => Ok(()),
        }
    }
}

/// Read `path` for a client. A file whose app has gone is removed and reported as stale.
pub fn read(path: &Path) -> Result<Advertisement, AdvertError> {
    let text = std::fs::read_to_string(path).map_err(AdvertError::Io)?;
    let advert: Advertisement = serde_json::from_str(&text).map_err(|e| AdvertError::Malformed(e.to_string()))?;
    if let Err(stale) = advert.check() {
        let _ = std::fs::remove_file(path);
        return Err(stale);
    }
    Ok(advert)
}

/// Remove `path` if it still advertises `token`: another app instance may have replaced the file,
/// and that one's must stay.
pub fn remove_if_ours(path: &Path, token: &str) {
    let ours = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Advertisement>(&text).ok())
        .is_some_and(|advert| advert.token == token);
    if ours {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, advert: &Advertisement) -> std::path::PathBuf {
        let path = dir.join("protocol.json");
        std::fs::write(&path, serde_json::to_vec(advert).unwrap()).unwrap();
        path
    }

    #[test]
    fn debug_redacts_the_token() {
        let advert = Advertisement::current("ws://127.0.0.1:1", "private-token-fixture");
        let debug = format!("{advert:?}");
        assert!(!debug.contains(&advert.token));
        assert!(debug.contains("[redacted]"));
        assert!(debug.contains(&advert.url));
    }

    #[test]
    fn this_process_is_live_and_a_dead_or_recycled_pid_is_stale_and_removed() {
        let dir = crate::test_support::tempdir().unwrap();
        let mine = Advertisement::current("ws://127.0.0.1:1", "t");
        let path = write(dir.path(), &mine);
        assert_eq!(read(&path).unwrap(), mine);

        let mut recycled = mine.clone();
        if let Some(start) = recycled.pid_start {
            recycled.pid_start = Some(start + 1);
            write(dir.path(), &recycled);
            assert!(matches!(read(&path), Err(AdvertError::Stale(_))), "same pid, different process");
            assert!(!path.exists());
        }

        // A child that has been reaped leaves a pid nothing runs under.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let gone = child.id();
        child.wait().unwrap();
        let dead = Advertisement { pid: gone, pid_start: None, ..mine.clone() };
        let path = write(dir.path(), &dead);
        assert!(matches!(read(&path), Err(AdvertError::Stale(_))));
        assert!(!path.exists(), "a stale file is cleaned up");
        let nobody = Advertisement { pid: 0, ..mine };
        write(dir.path(), &nobody);
        assert!(matches!(read(&path), Err(AdvertError::Stale(_))), "pid 0 is no process");
    }

    #[test]
    fn a_file_that_is_not_ours_is_left_alone_and_a_missing_one_is_an_io_error() {
        let dir = crate::test_support::tempdir().unwrap();
        let theirs = Advertisement::current("ws://127.0.0.1:2", "theirs");
        let path = write(dir.path(), &theirs);
        remove_if_ours(&path, "mine");
        assert!(path.exists());
        remove_if_ours(&path, "theirs");
        assert!(!path.exists());
        assert!(matches!(read(&path), Err(AdvertError::Io(_))));
        std::fs::write(&path, "{").unwrap();
        assert!(matches!(read(&path), Err(AdvertError::Malformed(_))));
    }
}

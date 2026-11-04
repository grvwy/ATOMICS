//! Bounded, cancellable child processes for the Mac-local VM worker.
use anyhow::{Context, Result, bail};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

/// No raw command output is persisted. The caller decides which output is fit for the panel.
pub(super) fn run(
    command: &mut Command,
    cancel: &AtomicBool,
    timeout: Duration,
    output: impl FnMut(&str),
) -> Result<ExitStatus> {
    run_with_input(command, cancel, timeout, None, output)
}

pub(super) fn run_with_input(
    command: &mut Command,
    cancel: &AtomicBool,
    timeout: Duration,
    input: Option<&Mutex<VecDeque<String>>>,
    mut output: impl FnMut(&str),
) -> Result<ExitStatus> {
    if cancel.load(Ordering::Acquire) {
        bail!("Cancelled");
    }
    command
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut child = command.spawn().context("starting VM operation")?;
    let pid = child.id() as i32;
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(32);
    let readers: Vec<_> =
        [Box::new(child.stdout.take().unwrap()) as Box<dyn Read + Send>, Box::new(child.stderr.take().unwrap())]
            .into_iter()
            .map(|mut pipe| {
                let tx = tx.clone();
                std::thread::spawn(move || {
                    let mut buf = [0; 4096];
                    while let Ok(n) = pipe.read(&mut buf) {
                        if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                })
            })
            .collect();
    drop(tx);
    let deadline = Instant::now() + timeout;
    let mut ended = None;
    let status = loop {
        if let Some(input) = input {
            while let Some(value) = input.lock().unwrap().pop_front() {
                if let Some(stdin) = child.stdin.as_mut() {
                    let _ = stdin.write_all(value.as_bytes());
                }
            }
        }
        while let Ok(bytes) = rx.try_recv() {
            output(&String::from_utf8_lossy(&bytes));
        }
        if (cancel.load(Ordering::Acquire) || Instant::now() >= deadline) && ended.is_none() {
            // SAFETY: this child's fresh process group belongs to this operation alone.
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
            ended = Some(Instant::now());
        }
        if ended.is_some_and(|at| at.elapsed() > Duration::from_secs(2)) {
            // SAFETY: same private process group; kill remaining descendants as well as the leader.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        if let Some(status) = child.try_wait()? {
            break status;
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    // A cancelled queue's child may outlive its leader. Always finish the entire group.
    if ended.is_some() {
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    // Drain until the pipes close, with a bound for a daemon that inherited them.
    let drain = Instant::now() + Duration::from_secs(2);
    loop {
        match rx.recv_timeout(Duration::from_millis(25)) {
            Ok(bytes) => output(&String::from_utf8_lossy(&bytes)),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(_) if Instant::now() >= drain => break,
            Err(_) => {}
        }
    }
    drop(rx);
    for reader in readers {
        if reader.is_finished() {
            let _ = reader.join();
        }
    }
    if ended.is_some() {
        bail!(if cancel.load(Ordering::Acquire) { "Cancelled" } else { "VM operation timed out" });
    }
    Ok(status)
}

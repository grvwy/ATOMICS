//! Phase 3's gates for the native client (`atomics --connect`, `client.rs`) against a real host: the
//! runtime with its protocol server, as `atomics host` runs it, and the client's own driver, link and
//! backend. Closing the window and opening it again keeps the agents' processes, their terminals and the
//! plans; two windows share one board and one lease; a window that stalls catches up.

use std::time::{Duration, Instant};

use atomics_core::control::{ControlRequest, ControlResponse};
use serde_json::{Value, json};

use super::tests::with_state;
use crate::client::target::Target;
use crate::client::{ClientApp, Launch};

fn ctl(request: Value) -> Value {
    let request: ControlRequest = serde_json::from_value(request).unwrap();
    let response: ControlResponse = crate::ipc::request(&request).unwrap();
    assert!(response.ok, "{:?}", response.error);
    response.result
}

/// Pump `app` (and follow `screen`, as a drawn terminal does) until `done`.
fn until(app: &mut ClientApp, screen: Option<&str>, what: &str, done: impl Fn(&ClientApp) -> bool) {
    let deadline = Instant::now() + crate::test_support::scaled_timeout(Duration::from_secs(20));
    loop {
        if let Some(session) = screen {
            let remote = &app.driver().remote;
            remote.show_screen(session, true);
            remote.end_frame();
            app.driver_mut().flush();
        }
        app.pump();
        if done(app) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:?} {:?}",
            app.driver().status(),
            screen.map(|session| screen_text(app, session))
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn screen_text(app: &ClientApp, session: &str) -> String {
    app.driver().remote.with_screen(session, |screen| screen.map(|(s, _)| s.text()).unwrap_or_default())
}

fn has_session(app: &ClientApp, session: &str) -> bool {
    app.driver().remote.with_mirror(|m| m.sessions.contains_key(session))
}

fn pid_of(session: &str) -> u64 {
    ctl(json!({"cmd":"list"}))["sessions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["id"] == session)
        .and_then(|s| s["pid"].as_u64())
        .expect("the session's process")
}

fn alive(pid: u64) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

#[test]
fn closing_the_window_leaves_the_agents_and_opening_it_again_shows_them_as_they_were() {
    with_state(
        "app::client_tests::closing_the_window_leaves_the_agents_and_opening_it_again_shows_them_as_they_were",
        |mut state, ctx| {
            state.config.remote.enabled = true;
            let host = state.into_host();
            let started =
                ctl(json!({"cmd":"start","agent":"shell","isolation":"bare","prompt":"echo first-window-ready"}));
            let session = started["id"].as_str().unwrap().to_string();
            ctl(json!({"cmd":"plan_set","goal":"Kept across windows","tasks":[]}));

            // The first window: the board, the plan, and the shell's terminal, typed into under the lease.
            let mut first = ClientApp::new(ctx, Target::Local, None, Launch::default());
            until(&mut first, None, "the board", |app| {
                has_session(app, &session)
                    && app.driver().remote.with_mirror(|m| m.plans.values().any(|p| p.goal == "Kept across windows"))
            });
            until(&mut first, Some(&session), "the shell's screen", |app| {
                screen_text(app, &session).contains("first-window-ready")
            });
            first.driver().remote.acquire(&session, false);
            until(&mut first, Some(&session), "the lease", |app| app.driver().remote.lease(&session).yours);
            first.driver().remote.type_text(&session, "echo typed-$((40 + 2))\r").unwrap();
            until(&mut first, Some(&session), "the typed line's output", |app| {
                screen_text(app, &session).contains("typed-42")
            });
            let pid = pid_of(&session);
            assert!(alive(pid));

            // The window closes: the host keeps the agent's process and its terminal.
            drop(first);
            std::thread::sleep(Duration::from_millis(300));
            assert_eq!(pid_of(&session), pid, "the same process");
            assert!(alive(pid));

            // A new window shows the same session, its terminal as it was, and the plan.
            let mut second = ClientApp::new(ctx, Target::Local, None, Launch::default());
            until(&mut second, None, "the board again", |app| {
                has_session(app, &session)
                    && app.driver().remote.with_mirror(|m| m.plans.values().any(|p| p.goal == "Kept across windows"))
            });
            until(&mut second, Some(&session), "the terminal as it was", |app| {
                let text = screen_text(app, &session);
                text.contains("first-window-ready") && text.contains("typed-42")
            });
            // And it types there too: the old window's lease ended with its connection.
            second.driver().remote.acquire(&session, false);
            until(&mut second, Some(&session), "the lease", |app| app.driver().remote.lease(&session).yours);
            second.driver().remote.type_text(&session, "echo second-$((1 + 1))\r").unwrap();
            until(&mut second, Some(&session), "the second window's output", |app| {
                screen_text(app, &session).contains("second-2")
            });
            assert_eq!(pid_of(&session), pid);
            drop(second);
            drop(host);
        },
    );
}

#[test]
fn two_windows_share_one_board_and_one_lease() {
    with_state("app::client_tests::two_windows_share_one_board_and_one_lease", |mut state, ctx| {
        state.config.remote.enabled = true;
        let host = state.into_host();
        let started = ctl(json!({"cmd":"start","agent":"shell","isolation":"bare","prompt":"echo two-windows"}));
        let session = started["id"].as_str().unwrap().to_string();
        let (mut a, mut b) = (
            ClientApp::new(ctx, Target::Local, None, Launch::default()),
            ClientApp::new(ctx, Target::Local, None, Launch::default()),
        );
        for (name, app) in [("a", &mut a), ("b", &mut b)] {
            // As the views do: a terminal is followed once its session is on the board.
            until(app, None, &format!("the session in {name}"), |app| has_session(app, &session));
            until(app, Some(&session), &format!("the shell in {name}"), |app| {
                screen_text(app, &session).contains("two-windows")
            });
        }
        a.driver().remote.acquire(&session, false);
        until(&mut a, Some(&session), "a's lease", |app| app.driver().remote.lease(&session).yours);
        until(&mut b, Some(&session), "b sees a typing", |app| {
            let lease = app.driver().remote.lease(&session);
            lease.holder.is_some() && !lease.yours
        });
        // b takes over, as its Take over button does; a's lease ends.
        b.driver().remote.acquire(&session, true);
        until(&mut b, Some(&session), "b's lease", |app| app.driver().remote.lease(&session).yours);
        until(&mut a, Some(&session), "a loses it", |app| !app.driver().remote.lease(&session).yours);
        b.driver().remote.type_text(&session, "echo from-b-$((2 + 3))\r").unwrap();
        for app in [&mut b, &mut a] {
            until(app, Some(&session), "both see b's line", |app| screen_text(app, &session).contains("from-b-5"));
        }
        // A plan set meanwhile reaches both boards.
        ctl(json!({"cmd":"plan_set","goal":"Seen by both","tasks":[]}));
        for app in [&mut a, &mut b] {
            until(app, None, "the plan", |app| {
                app.driver().remote.with_mirror(|m| m.plans.values().any(|p| p.goal == "Seen by both"))
            });
        }
        drop((a, b));
        drop(host);
    });
}

#[test]
fn a_window_that_stalls_catches_up_with_the_board() {
    with_state("app::client_tests::a_window_that_stalls_catches_up_with_the_board", |mut state, ctx| {
        state.config.remote.enabled = true;
        let host = state.into_host();
        let mut app = ClientApp::new(ctx, Target::Local, None, Launch::default());
        until(&mut app, None, "the board", |app| app.driver().remote.with_mirror(|m| m.epoch.is_some()));
        // The window draws nothing for a while (a long frame, a sleeping laptop's lid) while the host's
        // board changes many times: its link stops reading once its read-ahead is full.
        for n in 0..300 {
            ctl(json!({"cmd":"plan_set","goal":format!("Stalled {n}"),"tasks":[]}));
        }
        let goals: std::collections::BTreeSet<String> = (0..300).map(|n| format!("Stalled {n}")).collect();
        until(&mut app, None, "every plan", |app| {
            app.driver().remote.with_mirror(|m| goals.iter().all(|goal| m.plans.values().any(|p| &p.goal == goal)))
        });
        assert!(app.driver().remote.live(), "connected, after a resync or a reconnect");
        drop(app);
        drop(host);
    });
}

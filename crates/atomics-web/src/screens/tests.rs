//! Terminals against the fake host: following, the lease, typing and its echo, no replay, history.

use atomics_core::lease::GridSize;
use atomics_core::noise::Access;
use atomics_core::screen::Screen;
use atomics_core::wire::{Message, Topic};

use super::Lost;
use crate::driver::Status;
use crate::fake_host;
use crate::world::World;

fn live() -> World {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Live);
    world
}

/// One frame that shows `session`'s terminal, as the views do.
fn show(world: &mut World, session: &str) {
    world.driver.remote.show_screen(session, true);
    world.driver.remote.end_frame();
    world.driver.flush();
    world.pump();
}

fn type_text(world: &mut World, session: &str, text: &str) -> bool {
    let typed = world.driver.remote.type_text(session, text);
    world.driver.flush();
    world.pump();
    typed
}

fn screen_text(world: &World, session: &str) -> String {
    world.driver.remote.with_screen(session, |s| s.map(|(screen, _)| screen.text()).unwrap_or_default())
}

fn inputs(world: &World) -> Vec<String> {
    world
        .sent()
        .iter()
        .filter_map(|m| match m {
            Message::TerminalInput { data, .. } => Some(data.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_shown_terminal_is_followed_and_typing_takes_a_free_lease_and_echoes() {
    let mut world = live();
    show(&mut world, "s1");
    assert!(world.sent().iter().any(|m| matches!(m, Message::Subscribe { topics }
        if topics == &[Topic::Screen { session: "s1".into(), focused: true }])));
    assert_eq!(screen_text(&world, "s1"), "$");
    assert_eq!(world.driver.remote.lease("s1").holder, None, "nobody types yet");
    assert!(!world.sent().iter().any(|m| matches!(m, Message::LeaseAcquire { .. })), "following never takes it");

    assert!(type_text(&mut world, "s1", "ls\r"));
    assert!(world.sent().iter().any(|m| matches!(m, Message::LeaseAcquire { takeover: false, .. })));
    assert_eq!(world.host.terminals["s1"].typed, "ls\r", "the text waited for the grant, then went");
    assert_eq!(screen_text(&world, "s1"), "$ ls", "the echo");
    let lease = world.driver.remote.lease("s1");
    assert!(lease.yours && lease.holder.as_deref() == Some("Safari on iPhone"), "{lease:?}");

    assert!(type_text(&mut world, "s1", "pwd"));
    assert_eq!(inputs(&world), ["ls\r", "pwd"]);
}

#[test]
fn the_desktop_takes_the_lease_back_and_typing_again_asks_for_it_again() {
    let mut world = live();
    show(&mut world, "s1");
    type_text(&mut world, "s1", "a");
    let back = world.host.desktop_types("s1", "b");
    world.deliver(back);
    let lease = world.driver.remote.lease("s1");
    assert_eq!((lease.yours, lease.holder, lease.lost), (false, None, Some(Lost::Desktop)));
    assert_eq!(screen_text(&world, "s1"), "$ ab");

    assert!(type_text(&mut world, "s1", "c"), "a free lease is asked for again");
    assert_eq!(world.sent().iter().filter(|m| matches!(m, Message::LeaseAcquire { .. })).count(), 2);
    assert_eq!(world.host.terminals["s1"].typed, "abc");
    assert_eq!(world.driver.remote.lease("s1").lost, None);
}

#[test]
fn another_client_typing_keeps_input_out_until_the_user_takes_over() {
    let mut world = live();
    show(&mut world, "s1");
    type_text(&mut world, "s1", "a");
    let taken = world.host.other_takes("s1", "Firefox on Linux");
    world.deliver(taken);
    let lease = world.driver.remote.lease("s1");
    assert_eq!(
        (lease.yours, lease.holder.as_deref(), lease.lost),
        (false, Some("Firefox on Linux"), Some(Lost::TakenOver))
    );
    assert!(!type_text(&mut world, "s1", "x"), "nothing is typed while another client types");
    world.driver.remote.acquire("s1", false);
    world.driver.flush();
    world.pump();
    assert_eq!(world.driver.remote.lease("s1").lost, Some(Lost::Held), "asking without takeover is refused");

    world.driver.remote.acquire("s1", true);
    world.driver.flush();
    world.pump();
    assert!(world.driver.remote.lease("s1").yours, "a takeover the user asked for");
    assert!(type_text(&mut world, "s1", "y"));
    assert_eq!(world.host.terminals["s1"].typed, "ay", "x was never sent");
}

#[test]
fn nothing_typed_is_kept_or_replayed_across_a_disconnect() {
    let mut world = live();
    show(&mut world, "s1");
    type_text(&mut world, "s1", "one");
    world.drop_socket(1006);
    assert!(!world.driver.remote.lease("s1").yours, "the lease went with the channel");
    assert!(!type_text(&mut world, "s1", "lost"), "offline: nothing is typed");
    world.driver.remote.with_screen("s1", |s| assert!(s.unwrap().1, "the screen shows, stale"));

    // Back: a fresh ticket, hello, the screen subscribed again from the next frame.
    world.advance(world.driver.wake_in().expect("a retry is due"));
    world.ticket("ticket-2");
    assert_eq!(world.driver.status(), &Status::Live);
    world.host.received.clear();
    show(&mut world, "s1");
    world.driver.remote.with_screen("s1", |s| assert!(!s.unwrap().1, "a fresh snapshot"));
    assert!(inputs(&world).is_empty(), "nothing from before or during the outage");
    type_text(&mut world, "s1", "two");
    assert_eq!(world.host.terminals["s1"].typed, "onetwo");
}

#[test]
fn input_refused_as_slow_client_is_typed_again_once_the_program_reads() {
    let mut world = live();
    show(&mut world, "s1");
    type_text(&mut world, "s1", "a");
    world.host.terminals.get_mut("s1").unwrap().stalled = true;
    type_text(&mut world, "s1", "bc");
    assert_eq!(world.host.terminals["s1"].typed, "a");
    type_text(&mut world, "s1", "d");
    world.host.terminals.get_mut("s1").unwrap().stalled = false;
    let output = world.host.output("s1", "!");
    world.deliver(output);
    assert_eq!(world.host.terminals["s1"].typed, "abcd", "in the order typed");
}

#[test]
fn history_is_kept_as_lines_scroll_off_and_fetched_when_the_view_reaches_it() {
    let mut world = live();
    show(&mut world, "s1");
    // Ten lines at a time: each change scrolls fewer lines than the screen holds.
    for start in [1, 11, 21] {
        let lines: String = (start..start + 10).map(|n| format!("line {n}\r")).collect();
        let output = world.host.output("s1", &lines);
        world.deliver(output);
    }
    let first = world.driver.remote.with_screen("s1", |s| s.unwrap().0.first_line);
    let mut view = Screen::blank(0, 0);
    assert!(world.driver.remote.fill_screen("s1", first - 3, 3, &mut view));
    assert_eq!(view.text(), "line 17\nline 18\nline 19", "kept as they scrolled off");
    assert!(!world.sent().iter().any(|m| matches!(m, Message::HistoryGet(_))));

    // A new connection's snapshot starts over: the view asks the host for the lines it reaches.
    world.drop_socket(1006);
    world.advance(world.driver.wake_in().expect("a retry is due"));
    world.ticket("ticket-2");
    show(&mut world, "s1");
    let first = world.driver.remote.with_screen("s1", |s| s.unwrap().0.first_line);
    world.driver.remote.fill_screen("s1", first - 2, 4, &mut view);
    world.driver.flush();
    world.pump();
    assert!(world.sent().iter().any(|m| matches!(m, Message::HistoryGet(h) if h.before.0 == first)));
    world.driver.remote.fill_screen("s1", first - 2, 4, &mut view);
    assert_eq!(view.text(), "line 18\nline 19\nline 20\nline 21");
}

#[test]
fn a_view_only_device_follows_but_never_takes_the_lease() {
    let mut world = World::paired(fake_host::host());
    world.host.allowed[0].1 = Access::View;
    world.boot(None);
    show(&mut world, "s1");
    assert_eq!(screen_text(&world, "s1"), "$");
    assert!(!type_text(&mut world, "s1", "ls"));
    world.driver.remote.acquire("s1", true);
    world.driver.flush();
    world.pump();
    assert!(!world.sent().iter().any(|m| matches!(m, Message::LeaseAcquire { .. } | Message::TerminalInput { .. })));
}

#[test]
fn fit_to_this_screen_resizes_until_the_desktop_takes_the_terminal_back() {
    let mut world = live();
    show(&mut world, "s1");
    type_text(&mut world, "s1", "x");
    world.driver.remote.resize("s1", Some(GridSize::new(40, 10)));
    world.driver.flush();
    world.pump();
    let cols = |world: &World| world.driver.remote.with_screen("s1", |s| s.unwrap().0.cols);
    assert_eq!(cols(&world), 40);
    assert!(world.driver.remote.lease("s1").resized);
    let back = world.host.desktop_types("s1", "y");
    world.deliver(back);
    assert_eq!(cols(&world), 80, "the desktop's size again");
    assert!(!world.driver.remote.lease("s1").resized);
}

#[test]
fn a_terminal_no_frame_shows_is_let_go() {
    let mut world = live();
    show(&mut world, "s1");
    type_text(&mut world, "s1", "x");
    show(&mut world, "chief");
    let sent = world.sent();
    assert!(sent.iter().any(|m| matches!(m, Message::LeaseRelease { session, .. } if session == "s1")));
    assert!(sent.iter().any(|m| matches!(m, Message::Unsubscribe { topics }
        if topics.iter().any(|t| matches!(t, Topic::Screen { session, .. } if session == "s1")))));
    assert!(!world.host.terminals["s1"].followed && world.host.terminals["chief"].followed);
    assert!(world.host.terminals["s1"].lease.is_none());
}

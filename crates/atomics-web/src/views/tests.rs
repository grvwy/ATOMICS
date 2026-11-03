//! The views drawn headless over a live fake host: every tab draws, and what the user asks for reaches the
//! right place.

use atomics_core::approvals::Decision;
use atomics_core::theme::ThemeMode;
use atomics_core::wire::Message;
use serde_json::json;

use super::*;
use crate::fake_host::{self, WORKTREE};
use crate::world::{PARENT, World};

/// One frame at a phone's size.
fn frame(ctx: &egui::Context, world: &mut World, client: &mut Client, view: &mut View) {
    let input = egui::RawInput {
        screen_rect: Some(Rect::from_min_size(egui::Pos2::ZERO, vec2(390.0, 844.0))),
        ..egui::RawInput::default()
    };
    let mut output = ctx.run_ui(input, |ui| draw(ui, &mut world.driver, client, view));
    output.textures_delta.clear();
    world.pump();
}

fn live() -> (egui::Context, World, Client, View) {
    let mut world = World::paired(fake_host::host());
    world.boot(None);
    assert_eq!(world.driver.status(), &Status::Live);
    (egui::Context::default(), world, Client::default(), View::default())
}

#[test]
fn every_tab_draws_over_a_live_host() {
    let (ctx, mut world, mut client, mut view) = live();
    for tab in [Tab::Board, Tab::Plans, Tab::Approvals, Tab::Diffs] {
        view.tab = tab;
        frame(&ctx, &mut world, &mut client, &mut view);
        frame(&ctx, &mut world, &mut client, &mut view);
        assert!(client.commands.is_empty(), "commands are carried out after each frame");
    }
}

#[test]
fn opening_a_diff_fetches_it_and_shows_it_in_the_panel() {
    let (ctx, mut world, mut client, mut view) = live();
    world.host.received.clear();
    client.send(Command::ShowRoot(WORKTREE.into()));
    frame(&ctx, &mut world, &mut client, &mut view);
    assert_eq!((view.tab, view.diff.as_deref()), (Tab::Diffs, Some(Path::new(WORKTREE))));
    assert!(world.sent().iter().any(|m| matches!(m, Message::DiffGet { .. })));
    frame(&ctx, &mut world, &mut client, &mut view);
    let state = client.diff_views.get(Path::new(WORKTREE)).expect("the diff view's state");
    let prepared = state.prepared.as_ref().expect("the diff");
    assert!(!state.loading && prepared.diff.files.iter().all(|f| !f.hunks.is_empty()));

    client.send(Command::CloseDiff);
    frame(&ctx, &mut world, &mut client, &mut view);
    assert_eq!(view.diff, None);

    // A task card's session opens its worktree's diff too.
    client.send(Command::OpenTaskSession("s1".into()));
    frame(&ctx, &mut world, &mut client, &mut view);
    assert_eq!(view.diff.as_deref(), Some(Path::new(WORKTREE)));
}

#[test]
fn a_decision_recorded_in_a_frame_reaches_the_host() {
    let (ctx, mut world, mut client, mut view) = live();
    view.tab = Tab::Approvals;
    frame(&ctx, &mut world, &mut client, &mut view);
    // What the banner's Allow button records.
    client.approval_decisions.insert(("s1".into(), "a1".into()));
    client.send(Command::Decide { session: 1, approval: "a1".into(), decision: Decision::Allow });
    frame(&ctx, &mut world, &mut client, &mut view);
    assert!(world.sent().iter().any(|m| matches!(m, Message::ApprovalDecide(d) if d.approval == "a1")));
    frame(&ctx, &mut world, &mut client, &mut view);
    assert!(client.approval_decisions.is_empty(), "the host answered it, so the banner is gone");
}

#[test]
fn the_theme_follows_the_hosting_page() {
    let mut world = World::paired(fake_host::host());
    let mut data = world.boot_data(None);
    data["scheme"] = json!("light");
    data["theme"] = json!("light");
    world.driver.message(PARENT, true, &data).unwrap();
    world.pump();
    let ctx = egui::Context::default();
    let mut client = Client::default();
    apply_theme(&ctx, &mut client, world.driver.boot().unwrap());
    assert_eq!(client.palette.mode, ThemeMode::Light);
    assert_eq!(Tab::named("approvals"), Tab::Approvals);
    assert_eq!(Tab::named("TICKET-1"), Tab::Board, "a view this client cannot name opens the board");
}

#[test]
fn the_pairing_screen_and_a_stop_draw() {
    let mut host = fake_host::host();
    host.hold = true;
    let pairing = host.open_pairing();
    let mut world = World::new(host);
    world.boot(Some(&pairing));
    let (ctx, mut client, mut view) = (egui::Context::default(), Client::default(), View::default());
    frame(&ctx, &mut world, &mut client, &mut view);
    let mut world = World::new(fake_host::host());
    world.boot(None);
    assert!(matches!(world.driver.status(), Status::Stopped(_)));
    frame(&ctx, &mut world, &mut client, &mut view);
}

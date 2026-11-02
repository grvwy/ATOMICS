//! The ATOMICS UI's view of a host. The UI reads what it draws from a [`Backend`] and never from the
//! host's model, so the same UI can run over the desktop's in-process model, a loopback client or a
//! browser connection (docs/decisions/2026-11-02-embedded-atomics-extraction.md). The drawing code that
//! reads nothing from the host's model lives here already: the diff view, the keyboard navigation it
//! shares with the Tickets panel, the ticket markdown reader, chords, the top bar's layout helpers and the
//! terminal painter, which draws the screens `atomics.v1` streams (the desktop paints through it too).
//!
//! This crate must stay free of desktop-only dependencies; `tests/dependencies.rs` enforces it.

mod backend;
pub mod diff_view;
pub mod keys;
pub mod markdown;
pub mod panel_nav;
pub mod terminal;
pub mod theme;
pub mod topbar;

pub use backend::{ApprovalRow, Backend, BoardView, DiffSnapshot, PlanRow, PlanState, SessionRow};

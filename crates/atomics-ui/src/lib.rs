//! The ATOMICS UI's view of a host. The UI reads what it draws from a [`Backend`] and never from the
//! host's model, so the same UI can run over the desktop's in-process model, a loopback client or a
//! browser connection (docs/decisions/2026-11-02-embedded-atomics-extraction.md).
//!
//! This crate must stay free of desktop-only dependencies; `tests/dependencies.rs` enforces it.

mod backend;

pub use backend::{Backend, DiffSnapshot, PlanRow, PlanState};

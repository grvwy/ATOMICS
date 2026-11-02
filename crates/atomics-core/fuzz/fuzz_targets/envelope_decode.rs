//! Run with `cargo +nightly fuzz run envelope_decode` from `crates/atomics-core`.
//! Whatever bytes arrive, the decoder returns an error or an envelope; it never panics, and what
//! it accepts re-encodes (unless oversize) to a frame that decodes to the same envelope.
#![no_main]

use atomics_core::wire::{decode, encode};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(frame) = std::str::from_utf8(data) else { return };
    if let Ok(envelope) = decode(frame)
        && let Ok(again) = encode(&envelope)
    {
        assert_eq!(decode(&again).as_ref(), Ok(&envelope));
    }
});

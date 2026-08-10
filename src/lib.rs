//! The hardware-independent half of the firmware
//!
//! Living in a library crate (this file) instead of the binary
//! (main.rs) is what lets `cargo test-host` compile the pure logic
//! for this computer and run the tests without a Pico attached.
//! The firmware binary pulls the same code in with a `use`.
//!
//! The attribute below drops the standard library only for the real
//! firmware build; the test build keeps it so the test harness works
#![cfg_attr(not(test), no_std)]

pub mod one_wire;
pub mod reference_community_crates;

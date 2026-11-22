//! A tiny managed ring-3 service for `GTos`.
//!
//! It announces readiness, stays alive briefly, then exits with a nonzero
//! status. The session manager uses that real exit to exercise restart policy,
//! state publication and service-bus status queries. No foreign runtime is
//! involved.

#![no_std]
#![no_main]

gtos_sys::gtos_heap!();
gtos_sys::gtos_main!(main);

const READY: &[u8] = b"managed service ready\n";
const EXITING: &[u8] = b"managed service exiting\n";
const LIFETIME_MS: u64 = 600;
const CRASH_STATUS: u64 = 7;

fn main() {
    let _ = gtos_sys::write(1, READY);
    let _ = gtos_sys::sleep(LIFETIME_MS);
    let _ = gtos_sys::write(1, EXITING);
    gtos_sys::exit(CRASH_STATUS);
}

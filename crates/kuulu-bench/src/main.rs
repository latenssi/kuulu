//! Latency measurement, network emulator, loopback and server load tests.

mod fec;
mod latency;
mod load;
mod loopback;

use std::process::ExitCode;

// Aborts on any allocation inside `assert_no_alloc` (audio callbacks and the
// audio thread) in debug builds.
#[cfg(debug_assertions)]
#[global_allocator]
static ALLOC: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

fn main() -> ExitCode {
    match std::env::args().nth(1).as_deref() {
        Some("latency") => latency::run(),
        Some("loopback") => loopback::run(),
        Some("fec-compare") => fec::run(),
        Some("load") => load::run(),
        _ => {
            eprintln!("usage: kuulu-bench <latency|loopback|fec-compare|load>");
            ExitCode::from(2)
        }
    }
}

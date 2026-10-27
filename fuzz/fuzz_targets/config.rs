#![no_main]

use kuulu_server::Config;
use libfuzzer_sys::fuzz_target;

// kuulu.toml is written by the admin, but a bad file must be an error, not
// a panic.
fuzz_target!(|text: &str| {
    let _ = Config::parse(text);
});

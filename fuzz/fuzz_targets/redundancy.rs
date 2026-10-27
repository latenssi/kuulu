#![no_main]

use kuulu_proto::redundancy;
use libfuzzer_sys::fuzz_target;

// A split payload must write back to the same bytes given exactly its room.
fuzz_target!(|payload: &[u8]| {
    if let Some((current, previous)) = redundancy::split(payload) {
        let mut out = vec![0; payload.len()];
        assert_eq!(
            redundancy::write(current, previous, &mut out),
            Some(payload.len())
        );
        assert_eq!(out, payload);
    }
});

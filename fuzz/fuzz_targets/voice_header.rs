#![no_main]

use kuulu_proto::VoiceHeader;
use libfuzzer_sys::fuzz_target;

// Every datagram a peer sends goes through `parse`; a parsed packet must
// re-encode to the same bytes.
fuzz_target!(|packet: &[u8]| {
    if let Ok((header, payload)) = VoiceHeader::parse(packet) {
        let mut again = header.to_bytes().to_vec();
        again.extend_from_slice(payload);
        assert_eq!(again, packet);
    }
});

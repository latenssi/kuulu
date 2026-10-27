#![no_main]

use kuulu_proto::control::{ClientMessage, ServerMessage};
use kuulu_proto::frame;
use libfuzzer_sys::fuzz_target;
use prost::Message;
use std::pin::pin;
use std::task::{Context, Poll, Waker};

/// Reads frames until the first error, as either end of the control stream
/// does. A slice never makes a read wait, so one poll finishes it.
fn read_all<M: Message + Default + PartialEq>(mut stream: &[u8]) {
    loop {
        let read = pin!(frame::read::<M>(&mut stream));
        match read.poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(Ok(msg)) => {
                // Whatever decodes must survive a round trip.
                let again = frame::encode(&msg);
                assert!(M::decode(&again[4..]).ok() == Some(msg));
            }
            Poll::Ready(Err(_)) => return,
            Poll::Pending => unreachable!("slice reads never wait"),
        }
    }
}

fuzz_target!(|data: &[u8]| {
    // The first byte picks the reader: the server reads ClientMessages,
    // clients read ServerMessages.
    match data.split_first() {
        Some((side, stream)) if side % 2 == 0 => read_all::<ClientMessage>(stream),
        Some((_, stream)) => read_all::<ServerMessage>(stream),
        None => {}
    }
});

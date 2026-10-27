//! Protocol major versions and their negotiation.
//!
//! ALPN `kuulu/<major>` only names the protocol: both sides offer every major
//! they know so TLS always completes, and the real decision is made from
//! `Hello`, which lets the server answer with a `Reject` that says why.

use crate::control::{Hello, reject::Reason};

/// Newest protocol major version this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;
/// Oldest protocol major version this build accepts: the current and the previous one.
pub const MIN_PROTOCOL_VERSION: u32 = if PROTOCOL_VERSION > 1 {
    PROTOCOL_VERSION - 1
} else {
    1
};

/// ALPN identifiers `kuulu/<major>` for every major up to the current one, newest first.
pub fn alpn_protocols() -> Vec<Vec<u8>> {
    (1..=PROTOCOL_VERSION)
        .rev()
        .map(|v| format!("kuulu/{v}").into_bytes())
        .collect()
}

/// Server side: picks the newest major both sides speak, or says why there is none.
pub fn negotiate(hello: &Hello) -> Result<u32, Reason> {
    if hello.protocol_version < MIN_PROTOCOL_VERSION {
        Err(Reason::TooOld)
    } else if hello.min_protocol_version > PROTOCOL_VERSION {
        Err(Reason::TooNew)
    } else {
        Ok(hello.protocol_version.min(PROTOCOL_VERSION))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello(min: u32, max: u32) -> Hello {
        Hello {
            protocol_version: max,
            min_protocol_version: min,
            ..Default::default()
        }
    }

    #[test]
    fn negotiation() {
        let v = PROTOCOL_VERSION;
        assert_eq!(negotiate(&hello(MIN_PROTOCOL_VERSION, v)), Ok(v));
        assert_eq!(negotiate(&hello(1, v + 5)), Ok(v));
        assert_eq!(negotiate(&hello(v + 1, v + 2)), Err(Reason::TooNew));
        assert_eq!(
            negotiate(&hello(0, MIN_PROTOCOL_VERSION - 1)),
            Err(Reason::TooOld)
        );
    }

    #[test]
    fn alpn() {
        assert_eq!(
            alpn_protocols()[0],
            format!("kuulu/{PROTOCOL_VERSION}").into_bytes()
        );
    }
}

//! Payload of a voice packet with [`flags::REDUNDANT`](crate::flags::REDUNDANT):
//! `previous_len u8 | previous Opus frame | current Opus frame`.
//!
//! `previous_len` 0 means no previous frame: at stream start, or when it
//! would not fit (over 255 bytes, or the packet would get too large).

/// Writes the payload into `out` and returns its length, or `None` if not
/// even `current` fits.
pub fn write(current: &[u8], previous: &[u8], out: &mut [u8]) -> Option<usize> {
    let mut previous = previous;
    if previous.len() > usize::from(u8::MAX) || 1 + previous.len() + current.len() > out.len() {
        previous = &[];
    }
    let len = 1 + previous.len() + current.len();
    if len > out.len() {
        return None;
    }
    out[0] = previous.len() as u8;
    out[1..1 + previous.len()].copy_from_slice(previous);
    out[1 + previous.len()..len].copy_from_slice(current);
    Some(len)
}

/// Splits a payload into `(current, previous)`; `previous` is empty when
/// absent. `None` if `payload` is malformed.
pub fn split(payload: &[u8]) -> Option<(&[u8], &[u8])> {
    let (&previous_len, rest) = payload.split_first()?;
    let (previous, current) = rest.split_at_checked(usize::from(previous_len))?;
    Some((current, previous))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let mut out = [0; 16];
        let len = write(b"cur", b"prev", &mut out).unwrap();
        assert_eq!(&out[..len], b"\x04prevcur");
        assert_eq!(split(&out[..len]), Some((&b"cur"[..], &b"prev"[..])));
    }

    #[test]
    fn previous_left_out_when_it_does_not_fit() {
        let mut out = [0; 300];
        let len = write(b"cur", &[1; 256], &mut out).unwrap();
        assert_eq!(split(&out[..len]), Some((&b"cur"[..], &[][..])));
        let len = write(b"cur", b"prev", &mut out[..7]).unwrap();
        assert_eq!(&out[..len], b"\x00cur");
        assert_eq!(write(b"cur", b"", &mut out[..3]), None);
    }

    #[test]
    fn malformed() {
        assert_eq!(split(b""), None);
        assert_eq!(split(b"\x05abc"), None);
        assert_eq!(split(b"\x00"), Some((&b""[..], &b""[..])));
    }
}

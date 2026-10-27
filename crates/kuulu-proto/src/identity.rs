//! User identity: an Ed25519 key pair whose possession is proven per connection.
//!
//! The client signs keying material exported from the connection's TLS
//! session (RFC 8446 section 7.5). Every TLS session exports different
//! material, so a proof captured from one connection is useless on another.

use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use std::fmt;

/// Ed25519 public key: how the server identifies a user.
pub type PublicKey = [u8; 32];

const EXPORTER_LABEL: &[u8] = b"EXPORTER-kuulu-identity";

/// A user's secret identity key. Persist [`IdentityKey::seed`] (e.g. in the
/// OS keyring) and restore it with [`IdentityKey::from_seed`].
pub struct IdentityKey {
    seed: [u8; 32],
    pair: Ed25519KeyPair,
}

impl IdentityKey {
    pub fn generate() -> Self {
        let mut seed = [0; 32];
        SystemRandom::new()
            .fill(&mut seed)
            .expect("OS random number generator");
        Self::from_seed(seed)
    }

    /// Every 32-byte seed is a valid key.
    pub fn from_seed(seed: [u8; 32]) -> Self {
        let pair = Ed25519KeyPair::from_seed_unchecked(&seed).expect("32-byte seed");
        Self { seed, pair }
    }

    /// The secret: whoever has it is this user.
    pub fn seed(&self) -> &[u8; 32] {
        &self.seed
    }

    pub fn public_key(&self) -> PublicKey {
        self.pair
            .public_key()
            .as_ref()
            .try_into()
            .expect("Ed25519 public keys are 32 bytes")
    }

    /// Signature for `Hello.identity_proof` on this connection.
    pub fn prove(&self, conn: &quinn::Connection) -> Vec<u8> {
        let challenge = challenge(conn, &self.public_key()).expect("TLS 1.3 exporter");
        self.pair.sign(&challenge).as_ref().to_vec()
    }
}

impl Clone for IdentityKey {
    fn clone(&self) -> Self {
        Self::from_seed(self.seed)
    }
}

impl fmt::Debug for IdentityKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "IdentityKey({})", to_hex(&self.public_key()))
    }
}

/// Server side: `Some(key)` when `proof` shows the peer of `conn` holds the
/// secret for `public_key`.
pub fn verify(conn: &quinn::Connection, public_key: &[u8], proof: &[u8]) -> Option<PublicKey> {
    let key = PublicKey::try_from(public_key).ok()?;
    let challenge = challenge(conn, &key)?;
    UnparsedPublicKey::new(&ED25519, key)
        .verify(&challenge, proof)
        .ok()?;
    Some(key)
}

fn challenge(conn: &quinn::Connection, public_key: &PublicKey) -> Option<[u8; 32]> {
    let mut out = [0; 32];
    conn.export_keying_material(&mut out, EXPORTER_LABEL, public_key)
        .ok()?;
    Some(out)
}

/// Lowercase hex, the form public keys take in `kuulu.toml`.
pub fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses a public key written by [`to_hex`] (any case).
pub fn parse_public_key(hex: &str) -> Option<PublicKey> {
    if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut key = [0; 32];
    for (byte, pair) in key.iter_mut().zip(hex.as_bytes().chunks(2)) {
        *byte = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_round_trip_and_hex() {
        let key = IdentityKey::generate();
        let again = IdentityKey::from_seed(*key.seed());
        assert_eq!(key.public_key(), again.public_key());
        let hex = to_hex(&key.public_key());
        assert_eq!(parse_public_key(&hex), Some(key.public_key()));
        assert_eq!(
            parse_public_key(&hex.to_uppercase()),
            Some(key.public_key())
        );
        assert_eq!(parse_public_key(&hex[1..]), None);
        assert_eq!(parse_public_key(&format!("zz{}", &hex[2..])), None);
        assert!(!format!("{key:?}").contains(&to_hex(key.seed())));
    }
}

//! Invites: a random 128-bit token in a link, kept in `kuulu.db` only as its
//! SHA-256 hash. Tokens are looked up by hash, so no comparison ever touches
//! a stored token: timing reveals only facts about the hash of the
//! attacker's own guess, which do not help find a valid token.
//!
//! The functions here are for the `kuulu-server invite` command. They work
//! on the data directory directly, also while the server runs.

use crate::config::{Config, Identity};
use crate::store::{self, NewInvite};
use kuulu_proto::identity::to_hex;
use ring::digest::{SHA256, digest};
use ring::rand::{SecureRandom, SystemRandom};
use std::io;
use std::path::Path;
use std::time::Duration;

pub(crate) type Hash = [u8; 32];

/// A new invite usable `uses` times within `expires`, and its token.
pub(crate) fn new(uses: u32, expires: Duration, admin: bool) -> io::Result<(String, NewInvite)> {
    let ttl_secs = u32::try_from(expires.as_secs()).unwrap_or(0);
    if uses == 0 || ttl_secs == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "uses must be positive, and expiry between 1 s and 136 years",
        ));
    }
    let (token, hash) = generate();
    let invite = NewInvite {
        hash,
        uses,
        ttl_secs: ttl_secs.into(),
        admin,
    };
    Ok((token, invite))
}

/// A new token and its hash.
pub(crate) fn generate() -> (String, Hash) {
    let mut bytes = [0; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .expect("OS random number generator");
    let token = to_hex(&bytes);
    let hash = hash(&token).expect("not empty");
    (token, hash)
}

/// `None` for an empty token: Hello without an invite.
pub(crate) fn hash(token: &str) -> Option<Hash> {
    (!token.is_empty()).then(|| {
        digest(&SHA256, token.as_bytes())
            .as_ref()
            .try_into()
            .expect("SHA-256 is 32 bytes")
    })
}

/// `kuulu://<host>:<port>/join?t=<token>&fp=<certificate SHA-256>`.
pub(crate) fn link(host: &str, port: u16, token: &str, fingerprint: &str) -> String {
    let host = if host.contains(':') {
        format!("[{host}]")
    } else {
        host.to_string()
    };
    format!("kuulu://{host}:{port}/join?t={token}&fp={fingerprint}")
}

/// An invite as `list` shows it; the token itself is not stored.
#[derive(Debug, PartialEq, Eq)]
pub struct Info {
    pub id: i64,
    pub uses_left: u32,
    pub expires_in: Duration,
    /// The bootstrap invite that makes its user an admin.
    pub admin: bool,
}

/// Creates an invite in `dir` and returns its link. The host is `host`, or
/// `public_host` from `kuulu.toml`.
pub fn create(dir: &Path, uses: u32, expires: Duration, host: Option<&str>) -> io::Result<String> {
    let config = Config::load_or_create(&dir.join("kuulu.toml"))?;
    let host = host.unwrap_or(&config.public_host);
    if host.is_empty() {
        return Err(io::Error::other(
            "no host for the link: pass --host or set public_host in kuulu.toml",
        ));
    }
    let fingerprint = Identity::load_or_generate(dir)?.fingerprint();
    let (token, invite) = new(uses, expires, false)?;
    store::insert_invite(&store::open_db(dir)?, &invite).map_err(io::Error::other)?;
    Ok(link(host, config.port, &token, &fingerprint))
}

/// Invites in `dir` that can still be used.
pub fn list(dir: &Path) -> io::Result<Vec<Info>> {
    store::list_invites(&store::open_db(dir)?).map_err(io::Error::other)
}

/// Deletes invite `id`; `false` if there is none.
pub fn revoke(dir: &Path, id: i64) -> io::Result<bool> {
    store::revoke_invite(&store::open_db(dir)?, id).map_err(io::Error::other)
}

//! Persistence in SQLite (`kuulu.db`): channels, bans, invites, bootstrap
//! admins and known identities.
//!
//! The database is read once at startup ([`Store::open`]); after that the
//! in-memory state is authoritative and changes are sent to a dedicated
//! thread, so no tokio worker (and no voice forwarding) ever waits on disk.
//! The exception is invites, which `kuulu-server invite` adds from another
//! process: admission ([`Store::admit`]) asks the database, off the voice
//! path, and the single store thread makes using an invite atomic.

use crate::config::ChannelConfig;
use crate::invite::{self, Hash};
use kuulu_proto::identity::PublicKey;
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;
use tokio::sync::oneshot;

/// Known identities kept; the least recently seen beyond this are pruned, so
/// a flood of throwaway keys cannot grow the database without bound. A
/// pruned identity needs an invite again. Bans and bootstrap admins live in
/// their own tables and are never pruned.
const MAX_IDENTITIES: i64 = if cfg!(test) { 20 } else { 10_000 };

/// Handle to the store thread. Writes never block; they are applied in the
/// order they are sent.
#[derive(Clone)]
pub(crate) struct Store {
    tx: mpsc::Sender<Op>,
}

/// What the database held at startup.
pub(crate) struct Loaded {
    pub channels: Vec<ChannelConfig>,
    /// Next id for a new channel: ids are never reused.
    pub next_channel_id: u64,
    pub banned: HashSet<PublicKey>,
    /// Keys that joined with a bootstrap admin invite.
    pub admins: HashSet<PublicKey>,
}

/// Whether a key may join; see [`Store::admit`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Admission {
    Refused,
    Member,
    /// Used the bootstrap admin invite: now an admin.
    Admin,
}

/// Made by [`invite::new`].
pub(crate) struct NewInvite {
    pub hash: Hash,
    pub uses: u32,
    pub ttl_secs: i64,
    /// Bootstrap admin invite: replaces any earlier one.
    pub admin: bool,
}

pub(crate) enum Op {
    /// A member joined with this key and (cleaned) name.
    Seen(PublicKey, String),
    Ban(PublicKey),
    CreateChannel(u32, String),
    RenameChannel(u32, String),
    DeleteChannel(u32),
    CreateInvite(NewInvite),
    Admit(PublicKey, Option<Hash>, oneshot::Sender<Admission>),
    /// Answers once every earlier write is on disk.
    Flush(mpsc::Sender<()>),
}

const SCHEMA: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA synchronous = NORMAL;
    CREATE TABLE IF NOT EXISTS identities (
        public_key BLOB PRIMARY KEY,
        display_name TEXT NOT NULL,
        last_seen INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS identities_last_seen ON identities (last_seen);
    CREATE TABLE IF NOT EXISTS bans (
        public_key BLOB PRIMARY KEY,
        banned_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS channels (
        id INTEGER PRIMARY KEY,
        name TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS invites (
        id INTEGER PRIMARY KEY,
        hash BLOB UNIQUE NOT NULL,
        uses_left INTEGER NOT NULL,
        expires_at INTEGER NOT NULL,
        admin INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS admins (
        public_key BLOB PRIMARY KEY,
        added_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS meta (
        key TEXT PRIMARY KEY,
        value INTEGER NOT NULL
    );
";

impl Store {
    /// Opens `kuulu.db` in `dir`, or an in-memory database for `None`. On
    /// the very first open the channels are created from `seed`; later the
    /// seed is ignored.
    pub fn open(dir: Option<&Path>, seed: &[ChannelConfig]) -> io::Result<(Self, Loaded)> {
        let mut db = match dir {
            Some(dir) => open_db(dir)?,
            None => Connection::open_in_memory().map_err(io::Error::other)?,
        };
        let loaded = load(&mut db, seed).map_err(io::Error::other)?;
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("kuulu-store".into())
            .spawn(move || {
                for op in rx {
                    apply(&db, op);
                }
            })?;
        Ok((Self { tx }, loaded))
    }

    pub fn write(&self, op: Op) {
        // Fails only if the thread is gone, which a panic would have reported.
        let _ = self.tx.send(op);
    }

    /// Whether `key` may join with invite token `invite` (empty for none).
    /// A known key may; an unknown one uses up one use of a valid invite and
    /// is known from then on. The bootstrap admin invite is used even by a
    /// known key, making it an admin.
    pub async fn admit(&self, key: PublicKey, invite: &str) -> Admission {
        let (reply, answer) = oneshot::channel();
        self.write(Op::Admit(key, invite::hash(invite), reply));
        answer.await.unwrap_or(Admission::Refused)
    }

    /// Blocks until every earlier write is on disk. Call off the tokio workers.
    pub fn flush(&self) {
        let (done, wait) = mpsc::channel();
        self.write(Op::Flush(done));
        let _ = wait.recv();
    }
}

/// `kuulu.db` in `dir`, created if needed. Other processes may use it at the
/// same time: WAL mode, and rusqlite waits up to 5 s for a lock.
pub(crate) fn open_db(dir: &Path) -> io::Result<Connection> {
    let db = Connection::open(dir.join("kuulu.db")).map_err(io::Error::other)?;
    db.execute_batch(SCHEMA).map_err(io::Error::other)?;
    Ok(db)
}

fn load(db: &mut Connection, seed: &[ChannelConfig]) -> rusqlite::Result<Loaded> {
    db.execute_batch(SCHEMA)?;
    let tx = db.transaction()?;
    let next: Option<i64> = tx
        .query_row(
            "SELECT value FROM meta WHERE key = 'next_channel_id'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    if next.is_none() {
        for c in seed {
            tx.execute("INSERT INTO channels VALUES (?1, ?2)", (c.id, &c.name))?;
        }
        let next = seed.iter().map(|c| i64::from(c.id) + 1).max().unwrap_or(1);
        tx.execute("INSERT INTO meta VALUES ('next_channel_id', ?1)", [next])?;
    }
    tx.commit()?;

    let next_channel_id: i64 = db.query_row(
        "SELECT value FROM meta WHERE key = 'next_channel_id'",
        [],
        |r| r.get(0),
    )?;
    let channels = db
        .prepare("SELECT id, name FROM channels ORDER BY id")?
        .query_map([], |r| {
            Ok(ChannelConfig {
                id: r.get(0)?,
                name: r.get(1)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    let banned = db
        .prepare("SELECT public_key FROM bans")?
        .query_map([], |r| r.get::<_, [u8; 32]>(0))?
        .collect::<Result<_, _>>()?;
    let admins = db
        .prepare("SELECT public_key FROM admins")?
        .query_map([], |r| r.get::<_, [u8; 32]>(0))?
        .collect::<Result<_, _>>()?;
    Ok(Loaded {
        channels,
        next_channel_id: next_channel_id as u64,
        banned,
        admins,
    })
}

/// The in-memory state stays authoritative, so a failed write is only reported.
fn apply(db: &Connection, op: Op) {
    let result = match op {
        Op::Seen(key, name) => db
            .execute(
                "INSERT INTO identities VALUES (?1, ?2, unixepoch())
                 ON CONFLICT DO UPDATE SET display_name = ?2, last_seen = unixepoch()",
                (key, name),
            )
            .and_then(|_| {
                db.execute(
                    "DELETE FROM identities WHERE rowid IN (
                         SELECT rowid FROM identities ORDER BY last_seen, rowid
                         LIMIT max(0, (SELECT count(*) FROM identities) - ?1))",
                    [MAX_IDENTITIES],
                )
            }),
        Op::Ban(key) => db.execute("INSERT OR IGNORE INTO bans VALUES (?1, unixepoch())", [key]),
        Op::CreateChannel(id, name) => db
            .execute("INSERT INTO channels VALUES (?1, ?2)", (id, name))
            .and_then(|_| {
                db.execute(
                    "UPDATE meta SET value = ?1 WHERE key = 'next_channel_id'",
                    [i64::from(id) + 1],
                )
            }),
        Op::RenameChannel(id, name) => {
            db.execute("UPDATE channels SET name = ?2 WHERE id = ?1", (id, name))
        }
        Op::DeleteChannel(id) => db.execute("DELETE FROM channels WHERE id = ?1", [id]),
        Op::CreateInvite(invite) => insert_invite(db, &invite),
        Op::Admit(key, invite, reply) => {
            let result = admit(db, key, invite);
            let _ = reply.send(*result.as_ref().unwrap_or(&Admission::Refused));
            result.map(|_| 0)
        }
        Op::Flush(done) => {
            let _ = done.send(());
            Ok(0)
        }
    };
    if let Err(e) = result {
        eprintln!("database: {e}");
    }
}

pub(crate) fn insert_invite(db: &Connection, invite: &NewInvite) -> rusqlite::Result<usize> {
    // Unusable invites go first, so the table stays small.
    db.execute(
        "DELETE FROM invites WHERE uses_left <= 0 OR expires_at <= unixepoch() OR (?1 AND admin)",
        [invite.admin],
    )?;
    db.execute(
        "INSERT INTO invites (hash, uses_left, expires_at, admin)
         VALUES (?1, ?2, unixepoch() + ?3, ?4)",
        (invite.hash, invite.uses, invite.ttl_secs, invite.admin),
    )
}

fn admit(db: &Connection, key: PublicKey, invite: Option<Hash>) -> rusqlite::Result<Admission> {
    let tx = db.unchecked_transaction()?;
    let known = |key| {
        tx.query_row(
            "SELECT EXISTS (SELECT 1 FROM identities WHERE public_key = ?1)",
            [key],
            |r| r.get::<_, bool>(0),
        )
    };
    // One statement checks and uses the invite, so it is never over-used.
    let used: Option<bool> = match invite {
        None => None,
        Some(hash) => tx
            .query_row(
                "UPDATE invites SET uses_left = uses_left - 1
                 WHERE hash = ?1 AND uses_left > 0 AND expires_at > unixepoch()
                   AND (admin OR NOT EXISTS (SELECT 1 FROM identities WHERE public_key = ?2))
                 RETURNING admin",
                (hash, key),
                |r| r.get(0),
            )
            .optional()?,
    };
    let admission = match used {
        Some(admin) => {
            tx.execute(
                "INSERT OR IGNORE INTO identities VALUES (?1, '', unixepoch())",
                [key],
            )?;
            if admin {
                tx.execute(
                    "INSERT OR IGNORE INTO admins VALUES (?1, unixepoch())",
                    [key],
                )?;
                Admission::Admin
            } else {
                Admission::Member
            }
        }
        None if known(key)? => Admission::Member,
        None => Admission::Refused,
    };
    tx.commit()?;
    Ok(admission)
}

pub(crate) fn list_invites(db: &Connection) -> rusqlite::Result<Vec<invite::Info>> {
    db.prepare(
        "SELECT id, uses_left, expires_at - unixepoch(), admin FROM invites
         WHERE uses_left > 0 AND expires_at > unixepoch() ORDER BY id",
    )?
    .query_map([], |r| {
        Ok(invite::Info {
            id: r.get(0)?,
            uses_left: r.get(1)?,
            expires_in: Duration::from_secs(r.get::<_, i64>(2)?.max(0) as u64),
            admin: r.get(3)?,
        })
    })?
    .collect()
}

pub(crate) fn revoke_invite(db: &Connection, id: i64) -> rusqlite::Result<bool> {
    Ok(db.execute("DELETE FROM invites WHERE id = ?1", [id])? > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(id: u32, name: &str) -> ChannelConfig {
        ChannelConfig {
            id,
            name: name.into(),
        }
    }

    #[test]
    fn seeds_once_and_survives_reopen() {
        let dir = std::env::temp_dir().join(format!("kuulu-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (store, loaded) = Store::open(Some(&dir), &[channel(3, "General")]).unwrap();
        assert_eq!(loaded.channels, [channel(3, "General")]);
        assert_eq!(loaded.next_channel_id, 4);
        store.write(Op::Seen([1; 32], "a".into()));
        store.write(Op::Ban([2; 32]));
        store.write(Op::Ban([2; 32]));
        store.write(Op::CreateChannel(4, "Games".into()));
        store.write(Op::RenameChannel(3, "Lobby".into()));
        store.write(Op::CreateChannel(5, "Tmp".into()));
        store.write(Op::DeleteChannel(5));
        store.flush();
        drop(store);

        // A changed seed has no effect once the database exists.
        let (store, loaded) = Store::open(Some(&dir), &[channel(1, "Other")]).unwrap();
        assert_eq!(loaded.channels, [channel(3, "Lobby"), channel(4, "Games")]);
        // Deleted id 5 is not reused.
        assert_eq!(loaded.next_channel_id, 6);
        assert_eq!(loaded.banned, HashSet::from([[2; 32]]));
        drop(store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn invite(db: &Connection, uses: u32, ttl_secs: i64, admin: bool) -> String {
        let (token, hash) = invite::generate();
        insert_invite(
            db,
            &NewInvite {
                hash,
                uses,
                ttl_secs,
                admin,
            },
        )
        .unwrap();
        token
    }

    fn admit_token(db: &Connection, key: PublicKey, token: &str) -> Admission {
        admit(db, key, invite::hash(token)).unwrap()
    }

    #[test]
    fn invites_admit_new_identities() {
        let mut db = Connection::open_in_memory().unwrap();
        load(&mut db, &[]).unwrap();
        let token = invite(&db, 2, 60, false);
        // Unknown keys need a valid invite.
        assert_eq!(admit_token(&db, [1; 32], ""), Admission::Refused);
        assert_eq!(admit_token(&db, [1; 32], "nope"), Admission::Refused);
        assert_eq!(admit_token(&db, [1; 32], &token), Admission::Member);
        // Known now: rejoins without an invite, and does not use one up.
        assert_eq!(admit_token(&db, [1; 32], ""), Admission::Member);
        assert_eq!(admit_token(&db, [1; 32], &token), Admission::Member);
        assert_eq!(admit_token(&db, [2; 32], &token), Admission::Member);
        // Used up.
        assert_eq!(admit_token(&db, [3; 32], &token), Admission::Refused);
        // Expired.
        let expired = invite(&db, 5, -1, false);
        assert_eq!(admit_token(&db, [3; 32], &expired), Admission::Refused);
        // Only the hash is stored.
        let stored: i64 = db
            .query_row(
                "SELECT count(*) FROM invites WHERE hash = ?1",
                [token.as_bytes()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(stored, 0);

        // Listing and revoking.
        let live = invite(&db, 3, 3600, false);
        let listed = list_invites(&db).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].uses_left, 3);
        assert!(listed[0].expires_in > Duration::from_secs(3500));
        assert!(revoke_invite(&db, listed[0].id).unwrap());
        assert!(!revoke_invite(&db, listed[0].id).unwrap());
        assert_eq!(admit_token(&db, [3; 32], &live), Admission::Refused);
    }

    #[test]
    fn admin_invite_promotes_even_known_keys() {
        let dir = std::env::temp_dir().join(format!("kuulu-store-admin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut db = open_db(&dir).unwrap();
        load(&mut db, &[]).unwrap();
        let member = invite(&db, 1, 60, false);
        assert_eq!(admit_token(&db, [1; 32], &member), Admission::Member);
        let first = invite(&db, 1, 60, true);
        // A new admin invite replaces the old one.
        let admin = invite(&db, 1, 60, true);
        assert_eq!(admit_token(&db, [1; 32], &first), Admission::Member);
        assert_eq!(admit_token(&db, [1; 32], &admin), Admission::Admin);
        assert_eq!(admit_token(&db, [2; 32], &admin), Admission::Refused);
        drop(db);
        let mut db = open_db(&dir).unwrap();
        assert_eq!(load(&mut db, &[]).unwrap().admins, HashSet::from([[1; 32]]));
        drop(db);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identities_are_pruned() {
        let mut db = Connection::open_in_memory().unwrap();
        load(&mut db, &[]).unwrap();
        // Distinct keys, as a flood of throwaway identities would send.
        for i in 0..MAX_IDENTITIES as u32 + 5 {
            let mut key = [0; 32];
            key[..4].copy_from_slice(&i.to_le_bytes());
            apply(&db, Op::Seen(key, String::new()));
        }
        let count: i64 = db
            .query_row("SELECT count(*) FROM identities", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, MAX_IDENTITIES);
        // A pruned identity needs an invite again; a recent one does not.
        assert_eq!(admit_token(&db, [0; 32], ""), Admission::Refused);
        let mut recent = [0; 32];
        recent[..4].copy_from_slice(&(MAX_IDENTITIES as u32 + 4).to_le_bytes());
        assert_eq!(admit_token(&db, recent, ""), Admission::Member);
    }
}

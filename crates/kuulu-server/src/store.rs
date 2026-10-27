//! Persistence in SQLite (`kuulu.db`): channels, bans and known identities.
//!
//! The database is read once at startup ([`Store::open`]); after that the
//! in-memory state is authoritative and changes are sent to a dedicated
//! thread, so no tokio worker (and no voice forwarding) ever waits on disk.

use crate::config::ChannelConfig;
use kuulu_proto::identity::PublicKey;
use rusqlite::{Connection, OptionalExtension};
use std::collections::HashSet;
use std::io;
use std::path::Path;
use std::sync::mpsc;

/// Known identities kept; the least recently seen beyond this are pruned, so
/// a flood of throwaway keys cannot grow the database without bound. They
/// are informational only: bans live in their own table and are never pruned.
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
}

pub(crate) enum Op {
    /// A member joined with this key and (cleaned) name.
    Seen(PublicKey, String),
    Ban(PublicKey),
    CreateChannel(u32, String),
    RenameChannel(u32, String),
    DeleteChannel(u32),
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
        let db = match dir {
            Some(dir) => Connection::open(dir.join("kuulu.db")),
            None => Connection::open_in_memory(),
        };
        let mut db = db.map_err(io::Error::other)?;
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

    /// Blocks until every earlier write is on disk. Call off the tokio workers.
    pub fn flush(&self) {
        let (done, wait) = mpsc::channel();
        self.write(Op::Flush(done));
        let _ = wait.recv();
    }
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
    Ok(Loaded {
        channels,
        next_channel_id: next_channel_id as u64,
        banned,
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
        Op::Flush(done) => {
            let _ = done.send(());
            Ok(0)
        }
    };
    if let Err(e) = result {
        eprintln!("database: {e}");
    }
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
    }
}

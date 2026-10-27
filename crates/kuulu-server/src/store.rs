//! Everything the server persists at runtime: known identities and bans in
//! SQLite (`kuulu.db`), channel changes in `kuulu.toml`. Callers keep the
//! authoritative copy in memory; the store only writes, and reads at startup.

use crate::config::{ChannelConfig, Config};
use kuulu_proto::identity::PublicKey;
use rusqlite::Connection;
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) struct Store {
    db: Connection,
    /// `None` for an in-memory server.
    config_path: Option<PathBuf>,
    config: Config,
}

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS identities (
        public_key BLOB PRIMARY KEY,
        display_name TEXT NOT NULL,
        last_seen INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS bans (
        public_key BLOB PRIMARY KEY,
        banned_at INTEGER NOT NULL
    );
";

impl Store {
    pub fn open(dir: &Path, config_path: PathBuf, config: Config) -> io::Result<Self> {
        let db = Connection::open(dir.join("kuulu.db")).map_err(io::Error::other)?;
        Self::with_db(db, Some(config_path), config)
    }

    pub fn in_memory(config: Config) -> io::Result<Self> {
        let db = Connection::open_in_memory().map_err(io::Error::other)?;
        Self::with_db(db, None, config)
    }

    fn with_db(db: Connection, config_path: Option<PathBuf>, config: Config) -> io::Result<Self> {
        db.execute_batch(SCHEMA).map_err(io::Error::other)?;
        Ok(Self {
            db,
            config_path,
            config,
        })
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn banned(&self) -> io::Result<HashSet<PublicKey>> {
        let mut stmt = self
            .db
            .prepare("SELECT public_key FROM bans")
            .map_err(io::Error::other)?;
        let keys = stmt
            .query_map([], |row| row.get::<_, [u8; 32]>(0))
            .map_err(io::Error::other)?;
        keys.collect::<Result<_, _>>().map_err(io::Error::other)
    }

    /// Records that `key` connected under `name`.
    pub fn seen(&self, key: &PublicKey, name: &str) {
        log(self.db.execute(
            "INSERT INTO identities VALUES (?1, ?2, unixepoch())
             ON CONFLICT DO UPDATE SET display_name = ?2, last_seen = unixepoch()",
            (key, name),
        ));
    }

    pub fn ban(&self, key: &PublicKey) {
        log(self
            .db
            .execute("INSERT OR IGNORE INTO bans VALUES (?1, unixepoch())", [key]));
    }

    pub fn save_channels(&mut self, channels: Vec<ChannelConfig>) {
        self.config.channels = channels;
        if let Some(path) = &self.config_path
            && let Err(e) = self.config.save(path)
        {
            eprintln!("saving {}: {e}", path.display());
        }
    }
}

/// The in-memory state stays authoritative, so a failed write is only reported.
fn log(result: rusqlite::Result<usize>) {
    if let Err(e) = result {
        eprintln!("database: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bans_survive_reopen() {
        let dir = std::env::temp_dir().join(format!("kuulu-store-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("kuulu.toml");
        let store = Store::open(&dir, path.clone(), Config::default()).unwrap();
        store.seen(&[1; 32], "a");
        store.seen(&[1; 32], "b");
        store.ban(&[2; 32]);
        store.ban(&[2; 32]);
        drop(store);
        let store = Store::open(&dir, path, Config::default()).unwrap();
        assert_eq!(store.banned().unwrap(), HashSet::from([[2; 32]]));
        drop(store);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

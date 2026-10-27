//! Kuulu server: forwards voice packets between channel members.
//!
//! Usage: `kuulu-server [DATA_DIR]` (default: current directory). The data
//! directory holds `kuulu.toml`, `cert.der`, `key.der` and `kuulu.db`.

use kuulu_server::Server;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let dir = PathBuf::from(std::env::args_os().nth(1).unwrap_or_else(|| ".".into()));
    let server = Server::open(&dir)?;
    println!("listening on UDP {}", server.local_addr()?);
    println!("certificate SHA-256: {}", server.fingerprint());
    tokio::select! {
        _ = server.run() => {}
        _ = tokio::signal::ctrl_c() => server.close().await,
    }
    Ok(())
}

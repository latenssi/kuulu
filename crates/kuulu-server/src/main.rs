//! Kuulu server: forwards voice packets between channel members.
//!
//! Usage: `kuulu-server [DATA_DIR]` (default: current directory). The data
//! directory holds `kuulu.toml`, `cert.der` and `key.der`.

use kuulu_server::{Config, Identity, Server};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> std::io::Result<()> {
    let dir = PathBuf::from(std::env::args_os().nth(1).unwrap_or_else(|| ".".into()));
    let config = Config::load_or_create(&dir.join("kuulu.toml"))?;
    let identity = Identity::load_or_generate(&dir)?;
    let server = Server::bind(
        SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.port)),
        &identity,
    )?;
    println!("{} listening on UDP {}", config.name, server.local_addr()?);
    println!("certificate SHA-256: {}", identity.fingerprint());
    tokio::select! {
        _ = server.run() => {}
        _ = tokio::signal::ctrl_c() => server.close().await,
    }
    Ok(())
}

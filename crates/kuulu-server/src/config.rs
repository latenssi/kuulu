//! Server state on disk: `kuulu.toml`, plus the TLS certificate and key.
//! Each is created with defaults on first run.

use crate::state::{MAX_CHANNELS, channel_name};
use kuulu_proto::identity::{PublicKey, parse_public_key};
use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::Deserialize;
use std::collections::HashSet;
use std::io::{self, Write};
use std::path::Path;
use std::{fs, io::ErrorKind};

/// `kuulu.toml`. The server only reads it; missing fields take their
/// defaults, but an empty file is an error.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub name: String,
    pub port: u16,
    /// Host name or IP address clients reach the server at, for invite links.
    pub public_host: String,
    /// Most simultaneous users; clamped to what a Welcome can describe.
    pub max_users: usize,
    /// Hex Ed25519 public keys of administrators.
    pub admins: Vec<String>,
    /// Creates the channels on first start only; then they live in `kuulu.db`.
    pub channels: Vec<ChannelConfig>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ChannelConfig {
    /// Stable across restarts; unique and not 0.
    pub id: u32,
    pub name: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "Kuulu".into(),
            port: kuulu_proto::DEFAULT_PORT,
            public_host: String::new(),
            // The plan's sizing target: 100 users on one vCPU.
            max_users: 100,
            admins: Vec::new(),
            channels: vec![ChannelConfig {
                id: 1,
                name: "General".into(),
            }],
        }
    }
}

/// Written on first run; must parse to [`Config::default`].
const DEFAULT_FILE: &str = r#"# Kuulu server configuration. The server never writes to this file.

name = "Kuulu"
port = 7700
# Host name or IP address in invite links, e.g. "voice.example.com".
public_host = ""
# Most simultaneous users.
max_users = 100
# Hex Ed25519 public keys of administrators, e.g. ["3b6a27bc..."].
admins = []

# Channels created on the first start only. After that, channels live in
# kuulu.db and admins manage them from the client: editing these has no effect.
[[channels]]
id = 1
name = "General"
"#;

impl Config {
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) if text.trim().is_empty() => Err(invalid(format!(
                "{} is empty; delete it to create a default one",
                path.display()
            ))),
            Ok(text) => {
                let config: Self = toml::from_str(&text).map_err(invalid)?;
                config.admin_keys()?;
                config.check_channels()?;
                Ok(config)
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                fs::write(path, DEFAULT_FILE)?;
                Ok(Self::default())
            }
            Err(e) => Err(e),
        }
    }

    pub(crate) fn admin_keys(&self) -> io::Result<HashSet<PublicKey>> {
        self.admins
            .iter()
            .map(|hex| {
                parse_public_key(hex)
                    .ok_or_else(|| invalid(format!("admins: {hex:?} is not a hex public key")))
            })
            .collect()
    }

    fn check_channels(&self) -> io::Result<()> {
        let mut seen = HashSet::new();
        for c in &self.channels {
            if c.id == 0 || !seen.insert(c.id) || channel_name(&c.name).is_err() {
                return Err(invalid(format!(
                    "channels: id {} is 0 or used twice, or its name is empty",
                    c.id
                )));
            }
        }
        if seen.is_empty() || seen.len() > MAX_CHANNELS {
            return Err(invalid(format!(
                "channels: between 1 and {MAX_CHANNELS} are needed"
            )));
        }
        Ok(())
    }
}

fn invalid(e: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(ErrorKind::InvalidData, e)
}

/// Self-signed TLS identity. Clients pin its [`Identity::fingerprint`], so it
/// must survive restarts.
pub struct Identity {
    pub(crate) cert: CertificateDer<'static>,
    pub(crate) key: PrivateKeyDer<'static>,
}

const CERT_FILE: &str = "cert.der";
const KEY_FILE: &str = "key.der";

impl Identity {
    pub fn generate() -> Self {
        let generated =
            rcgen::generate_simple_self_signed(vec!["kuulu".into()]).expect("ring keygen");
        Self {
            cert: generated.cert.der().clone(),
            key: PrivatePkcs8KeyDer::from(generated.signing_key.serialize_der()).into(),
        }
    }

    /// Loads `cert.der` and `key.der` from `dir`, generating both only when
    /// both are absent. Never overwrites: a new certificate changes the
    /// fingerprint, which breaks every client's pin and every invite.
    pub fn load_or_generate(dir: &Path) -> io::Result<Self> {
        let (cert_path, key_path) = (dir.join(CERT_FILE), dir.join(KEY_FILE));
        let not_found = |r: &io::Result<_>| matches!(r, Err(e) if e.kind() == ErrorKind::NotFound);
        match (fs::read(&cert_path), fs::read(&key_path)) {
            (Ok(cert), Ok(key)) => {
                warn_if_readable_by_others(&key_path);
                Ok(Self {
                    cert: cert.into(),
                    key: PrivatePkcs8KeyDer::from(key).into(),
                })
            }
            (cert, key) if not_found(&cert) && not_found(&key) => {
                let identity = Self::generate();
                write_new(&key_path, identity.key.secret_der())?;
                write_new(&cert_path, &identity.cert)?;
                Ok(identity)
            }
            (Err(e), _) | (_, Err(e)) if e.kind() != ErrorKind::NotFound => Err(e),
            _ => Err(io::Error::new(
                ErrorKind::NotFound,
                format!(
                    "only one of {CERT_FILE} and {KEY_FILE} exists in {}; restore the \
                     missing one, or delete both to create a new identity (this changes \
                     the fingerprint and invalidates all invites)",
                    dir.display()
                ),
            )),
        }
    }

    pub fn fingerprint(&self) -> String {
        kuulu_proto::cert_fingerprint(&self.cert)
    }
}

/// Creates `path` readable only by the owner, failing if it already exists.
fn write_new(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(data)
}

#[cfg(unix)]
fn warn_if_readable_by_others(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        eprintln!(
            "warning: {} is accessible by other users; run: chmod 600 {0}",
            path.display()
        );
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created_on_first_run_and_reloaded() {
        let dir = std::env::temp_dir().join(format!("kuulu-config-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();

        let first = Identity::load_or_generate(&dir).unwrap();
        let again = Identity::load_or_generate(&dir).unwrap();
        assert_eq!(first.fingerprint(), again.fingerprint());
        assert_eq!(first.fingerprint().len(), 64);

        // One file missing must fail rather than replace the identity.
        for file in [CERT_FILE, KEY_FILE] {
            let saved = fs::read(dir.join(file)).unwrap();
            fs::remove_file(dir.join(file)).unwrap();
            let err = Identity::load_or_generate(&dir).err().unwrap();
            assert!(err.to_string().contains("only one of"), "{err}");
            fs::write(dir.join(file), saved).unwrap();
        }
        let again = Identity::load_or_generate(&dir).unwrap();
        assert_eq!(first.fingerprint(), again.fingerprint());

        let path = dir.join("kuulu.toml");
        let _ = fs::remove_file(&path);
        assert_eq!(Config::load_or_create(&path).unwrap(), Config::default());
        // The written file says the same as the default.
        assert_eq!(Config::load_or_create(&path).unwrap(), Config::default());
        // A config from before channels and admins existed.
        fs::write(&path, "name = \"x\"\nport = 1\n").unwrap();
        let config = Config::load_or_create(&path).unwrap();
        assert_eq!((config.port, config.channels.len()), (1, 1));
        for bad in [
            "",
            " \n",
            "port = ",
            "admins = [\"00\"]",
            "channels = []",
            "[[channels]]\nid = 2\nname = \"a\"\n[[channels]]\nid = 2\nname = \"b\"",
        ] {
            fs::write(&path, bad).unwrap();
            assert!(Config::load_or_create(&path).is_err(), "{bad}");
        }

        fs::remove_dir_all(&dir).unwrap();
    }
}

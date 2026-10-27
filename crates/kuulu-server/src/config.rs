//! Server state on disk: `kuulu.toml`, plus the TLS certificate and key.
//! Each is created with defaults on first run.

use quinn::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde::{Deserialize, Serialize};
use std::io::{self, Write};
use std::path::Path;
use std::{fs, io::ErrorKind};

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub name: String,
    pub port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "Kuulu".into(),
            port: kuulu_proto::DEFAULT_PORT,
        }
    }
}

impl Config {
    pub fn load_or_create(path: &Path) -> io::Result<Self> {
        match fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| io::Error::new(ErrorKind::InvalidData, e))
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {
                let config = Self::default();
                fs::write(path, toml::to_string(&config).map_err(io::Error::other)?)?;
                Ok(config)
            }
            Err(e) => Err(e),
        }
    }
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
        let config = Config::load_or_create(&path).unwrap();
        assert_eq!(config.port, kuulu_proto::DEFAULT_PORT);
        fs::write(&path, "name = \"x\"\nport = 1\n").unwrap();
        assert_eq!(Config::load_or_create(&path).unwrap().port, 1);

        fs::remove_dir_all(&dir).unwrap();
    }
}

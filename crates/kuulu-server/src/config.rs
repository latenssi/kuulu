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

    /// Loads `cert.der` and `key.der` from `dir`, generating both if absent.
    pub fn load_or_generate(dir: &Path) -> io::Result<Self> {
        let (cert_path, key_path) = (dir.join(CERT_FILE), dir.join(KEY_FILE));
        match (fs::read(&cert_path), fs::read(&key_path)) {
            (Ok(cert), Ok(key)) => Ok(Self {
                cert: cert.into(),
                key: PrivatePkcs8KeyDer::from(key).into(),
            }),
            (Err(e), _) | (_, Err(e)) if e.kind() != ErrorKind::NotFound => Err(e),
            _ => {
                let identity = Self::generate();
                write_private(&key_path, identity.key.secret_der())?;
                fs::write(&cert_path, &identity.cert)?;
                Ok(identity)
            }
        }
    }

    pub fn fingerprint(&self) -> String {
        kuulu_proto::cert_fingerprint(&self.cert)
    }
}

fn write_private(path: &Path, data: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(data)
}

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

        let path = dir.join("kuulu.toml");
        let config = Config::load_or_create(&path).unwrap();
        assert_eq!(config.port, kuulu_proto::DEFAULT_PORT);
        fs::write(&path, "name = \"x\"\nport = 1\n").unwrap();
        assert_eq!(Config::load_or_create(&path).unwrap().port, 1);

        fs::remove_dir_all(&dir).unwrap();
    }
}

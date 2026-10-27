//! The user's identity key, kept only in the OS keyring: Keychain on macOS,
//! Credential Manager on Windows, Secret Service on Linux. Without a
//! keyring there is no identity; the key is never written in plain text.

use kuulu_client::IdentityKey;

const SERVICE: &str = "fi.latenssi.kuulu";
const ACCOUNT: &str = "identity-key";

/// The stored key, created and stored on first use. The error is worded for
/// the user.
pub fn load_or_create() -> Result<IdentityKey, String> {
    let entry = keyring::Entry::new(SERVICE, ACCOUNT).map_err(unavailable)?;
    match entry.get_secret() {
        Ok(secret) => seed(&secret).map(IdentityKey::from_seed),
        Err(keyring::Error::NoEntry) => {
            let key = IdentityKey::generate();
            entry.set_secret(key.seed()).map_err(unavailable)?;
            Ok(key)
        }
        Err(e) => Err(unavailable(e)),
    }
}

fn seed(secret: &[u8]) -> Result<[u8; 32], String> {
    secret.try_into().map_err(|_| {
        format!(
            "The identity key in your OS keyring ({SERVICE}) is damaged. \
             Remove it there to create a new identity."
        )
    })
}

fn unavailable(e: keyring::Error) -> String {
    format!(
        "Kuulu keeps your identity key in the OS keyring, but none is available ({e}). \
         On Linux, install and unlock a Secret Service provider such as GNOME Keyring \
         or KeePassXC, then try again."
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_secret_must_be_a_seed() {
        let key = IdentityKey::generate();
        let again = IdentityKey::from_seed(seed(key.seed()).unwrap());
        assert_eq!(again.public_key(), key.public_key());
        assert!(seed(&[0; 31]).unwrap_err().contains("damaged"));
    }
}

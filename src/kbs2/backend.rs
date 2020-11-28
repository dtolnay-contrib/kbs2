use age::armor::{ArmoredReader, ArmoredWriter, Format};
use anyhow::{anyhow, Context, Result};
use secrecy::{ExposeSecret, SecretString};

use std::io::{Read, Write};
use std::path::Path;

use crate::kbs2::agent;
use crate::kbs2::config;
use crate::kbs2::record::Record;
use crate::kbs2::util;

/// Represents the operations that all age backends are capable of.
pub trait Backend {
    /// Creates an age keypair, saving the private component to the given path.
    ///
    /// NOTE: The private component is written in an ASCII-armored format.
    fn create_keypair<P: AsRef<Path>>(path: P) -> Result<String>
    where
        Self: Sized;

    /// Creates a wrapped age keypair, saving the encrypted private component to the
    /// given path.
    ///
    /// NOTE: Like `create_keypair`, this writes an ASCII-armored private component.
    /// It also prompts the user to enter a password for encrypting the generated
    /// private key.
    fn create_wrapped_keypair<P: AsRef<Path>>(path: P, password: SecretString) -> Result<String>
    where
        Self: Sized;

    /// Rewraps the given keyfile in place, decrypting it with the `old` password
    /// and re-encrypting it with the `new` password.
    ///
    /// NOTE: This function does *not* make a backup of the original keyfile.
    fn rewrap_keyfile<P: AsRef<Path>>(path: P, old: SecretString, new: SecretString) -> Result<()>;

    /// Encrypts the given record, returning it as an ASCII-armored string.
    fn encrypt(&self, record: &Record) -> Result<String>;

    /// Decrypts the given ASCII-armored string, returning it as a Record.
    fn decrypt(&self, encrypted: &str) -> Result<Record>;
}

/// Encapsulates the age crate (i.e., the `rage` CLI's backing library).
pub struct RageLib {
    pub pubkey: age::x25519::Recipient,
    pub identities: Vec<age::x25519::Identity>,
}

impl RageLib {
    pub fn new(config: &config::Config) -> Result<RageLib> {
        let pubkey = config
            .public_key
            .parse::<age::x25519::Recipient>()
            .map_err(|e| anyhow!("unable to parse public key (backend reports: {:?})", e))?;

        let identities = if config.wrapped {
            log::debug!("config specifies a wrapped key");

            let client = agent::Client::new().with_context(|| "failed to connect to kbs2 agent")?;
            let unwrapped_key = client
                .get_key(&config.keyfile)
                .with_context(|| format!("agent has no unwrapped key for {}", config.keyfile))?;

            log::debug!("parsing unwrapped key");
            age::IdentityFile::from_buffer(unwrapped_key.as_bytes())?
        } else {
            age::IdentityFile::from_file(config.keyfile.clone())?
        }
        .into_identities();
        log::debug!("successfully parsed a private key!");

        if identities.len() != 1 {
            return Err(anyhow!(
                "expected exactly one private key in the keyfile, but got {}",
                identities.len()
            ));
        }

        Ok(RageLib { pubkey, identities })
    }
}

impl Backend for RageLib {
    fn create_keypair<P: AsRef<Path>>(path: P) -> Result<String> {
        let keypair = age::x25519::Identity::generate();

        std::fs::write(path, keypair.to_string().expose_secret())?;

        Ok(keypair.to_public().to_string())
    }

    fn create_wrapped_keypair<P: AsRef<Path>>(path: P, password: SecretString) -> Result<String> {
        let keypair = age::x25519::Identity::generate();
        let wrapped_key = util::wrap_key(keypair.to_string(), password)?;
        std::fs::write(path, wrapped_key)?;

        Ok(keypair.to_public().to_string())
    }

    fn rewrap_keyfile<P: AsRef<Path>>(
        keyfile: P,
        old: SecretString,
        new: SecretString,
    ) -> Result<()> {
        let unwrapped_key = util::unwrap_keyfile(&keyfile, old)?;
        let rewrapped_key = util::wrap_key(unwrapped_key, new)?;

        std::fs::write(&keyfile, rewrapped_key)?;
        Ok(())
    }

    fn encrypt(&self, record: &Record) -> Result<String> {
        let encryptor = age::Encryptor::with_recipients(vec![Box::new(self.pubkey.clone())]);
        let mut encrypted = vec![];
        let mut writer = encryptor
            .wrap_output(ArmoredWriter::wrap_output(
                &mut encrypted,
                Format::AsciiArmor,
            )?)
            .map_err(|e| anyhow!("wrap_output failed (backend report: {:?})", e))?;
        writer.write_all(serde_json::to_string(record)?.as_bytes())?;
        writer.finish().and_then(|armor| armor.finish())?;

        Ok(String::from_utf8(encrypted)?)
    }

    fn decrypt(&self, encrypted: &str) -> Result<Record> {
        let decryptor = match age::Decryptor::new(ArmoredReader::new(encrypted.as_bytes()))
            .map_err(|e| anyhow!("unable to load private key (backend reports: {:?})", e))?
        {
            age::Decryptor::Recipients(d) => d,
            // NOTE(ww): we should be fully unwrapped (if we were wrapped to begin with)
            // in this context, so all other kinds of keys should be unreachable here.
            _ => unreachable!(),
        };

        let mut decrypted = String::new();

        // NOTE(ww): The age API changed here from `&[Identity]` to
        // `impl Iterator<Item = Box<dyn Identity>>`, which changed the `decrypt`
        // from a borrow to a stolen ownership of the identity list.
        // So we do a funky box clone thing below.
        decryptor
            .decrypt(
                self.identities
                    .iter()
                    .cloned()
                    .map(Box::new)
                    .map(|i| i as Box<dyn age::Identity>),
            )
            .map_err(|e| anyhow!("unable to decrypt (backend reports: {:?})", e))
            .and_then(|mut r| {
                r.read_to_string(&mut decrypted)
                    .map_err(|e| anyhow!("i/o error while decrypting: {:?}", e))
            })?;

        Ok(serde_json::from_str(&decrypted)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ragelib_backend() -> RageLib {
        let key = age::x25519::Identity::generate();

        RageLib {
            pubkey: key.to_public(),
            identities: vec![key.into()],
        }
    }

    fn ragelib_backend_bad_keypair() -> RageLib {
        let key1 = age::x25519::Identity::generate();
        let key2 = age::x25519::Identity::generate();

        RageLib {
            pubkey: key1.to_public(),
            identities: vec![key2.into()],
        }
    }

    #[test]
    fn test_ragelib_create_keypair() {
        let keyfile = tempfile::NamedTempFile::new().unwrap();

        assert!(RageLib::create_keypair(&keyfile).is_ok());
    }

    #[test]
    fn test_ragelib_create_wrapped_keypair() {
        let keyfile = tempfile::NamedTempFile::new().unwrap();

        // Creating a wrapped keypair with a particular password should succeed.
        assert!(RageLib::create_wrapped_keypair(
            &keyfile,
            SecretString::new("weakpassword".into())
        )
        .is_ok());

        // Unwrapping the keyfile using the same password should succeed.
        assert!(util::unwrap_keyfile(&keyfile, SecretString::new("weakpassword".into())).is_ok());
    }

    #[test]
    fn test_ragelib_rewrap_keyfile() {
        let keyfile = tempfile::NamedTempFile::new().unwrap();

        RageLib::create_wrapped_keypair(&keyfile, SecretString::new("weakpassword".into()))
            .unwrap();

        let wrapped_key_a = std::fs::read(&keyfile).unwrap();
        let unwrapped_key_a =
            util::unwrap_keyfile(&keyfile, SecretString::new("weakpassword".into())).unwrap();

        // Changing the password on a wrapped keyfile should succeed.
        assert!(RageLib::rewrap_keyfile(
            &keyfile,
            SecretString::new("weakpassword".into()),
            SecretString::new("stillweak".into()),
        )
        .is_ok());

        let wrapped_key_b = std::fs::read(&keyfile).unwrap();
        let unwrapped_key_b =
            util::unwrap_keyfile(&keyfile, SecretString::new("stillweak".into())).unwrap();

        // The wrapped envelopes should not be equal, since the password has changed.
        assert_ne!(wrapped_key_a, wrapped_key_b);

        // However, the wrapped key itself should be preserved.
        assert_eq!(
            unwrapped_key_a.expose_secret(),
            unwrapped_key_b.expose_secret()
        );
    }

    #[test]
    fn test_ragelib_encrypt() {
        {
            let backend = ragelib_backend();
            let record = Record::login("foo", "username", "password");
            assert!(backend.encrypt(&record).is_ok());
        }

        // TODO: Test RageLib::encrypt failure modes.
    }

    #[test]
    fn test_ragelib_decrypt() {
        {
            let backend = ragelib_backend();
            let record = Record::login("foo", "username", "password");

            let encrypted = backend.encrypt(&record).unwrap();
            let decrypted = backend.decrypt(&encrypted).unwrap();

            assert_eq!(record, decrypted);
        }

        {
            let backend = ragelib_backend_bad_keypair();
            let record = Record::login("foo", "username", "password");

            let encrypted = backend.encrypt(&record).unwrap();
            let err = backend.decrypt(&encrypted).unwrap_err();

            assert_eq!(
                err.to_string(),
                "unable to decrypt (backend reports: NoMatchingKeys)"
            );
        }
    }
}

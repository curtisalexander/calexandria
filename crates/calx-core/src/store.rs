//! Where calx keeps secrets: an encrypted `auth.json` in the calx home
//! directory, whose 32-byte key lives in the OS keychain (macOS Keychain,
//! Windows Credential Manager, or the Linux Secret Service).
//!
//! The key is the only thing in the keychain, so secrets of any size fit
//! (Windows caps one keychain entry at 2,560 bytes, and a ChatGPT access token
//! alone is over 2,000), and a copy of the file is useless without the key.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

use crate::auth::Credential;

const KEYCHAIN_SERVICE: &str = "calexandria";
const KEYCHAIN_ACCOUNT: &str = "auth-key";
/// Supplies the key directly, for machines without a keychain, such as a
/// headless Linux server or a test. A base64 string of 32 random bytes.
pub const KEY_ENV: &str = "CALX_AUTH_KEY";

/// Everything calx keeps secret, encrypted as one unit.
#[derive(Default, Serialize, Deserialize)]
struct Secrets {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chatgpt: Option<Credential>,
}

/// The file on disk: a nonce and the encrypted, serialized `Secrets`.
#[derive(Serialize, Deserialize)]
struct Sealed {
    version: u32,
    nonce: String,
    ciphertext: String,
}

pub struct CredentialStore {
    path: PathBuf,
    /// The key, once read, so later saves (token refreshes) do not ask the
    /// keychain again; on macOS each keychain read can show a prompt.
    key: Mutex<Option<[u8; 32]>>,
}

impl CredentialStore {
    pub fn open() -> Result<Self> {
        Ok(Self::at(crate::calx_home()?.join("auth.json")))
    }

    fn at(path: PathBuf) -> Self {
        Self {
            path,
            key: Mutex::new(None),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Result<Option<Credential>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e).with_context(|| format!("could not read {}", self.path.display()));
            }
        };
        let Ok(sealed) = serde_json::from_str::<Sealed>(&text) else {
            // A file from before encryption: read it, then encrypt it in place.
            let secrets: Secrets = serde_json::from_str(&text)
                .with_context(|| format!("could not parse {}", self.path.display()))?;
            self.save(secrets.chatgpt.as_ref())?;
            return Ok(secrets.chatgpt);
        };
        let key = self.key(false)?.with_context(|| {
            format!(
                "the key for {} is missing from the keychain; run `calx login` again",
                self.path.display()
            )
        })?;
        let secrets: Secrets = serde_json::from_slice(&open(&key, &sealed)?)
            .context("the decrypted credentials are not valid")?;
        Ok(secrets.chatgpt)
    }

    pub fn save(&self, credential: Option<&Credential>) -> Result<()> {
        let key = self.key(true)?.expect("a key is created when missing");
        let secrets = Secrets {
            chatgpt: credential.cloned(),
        };
        let sealed = seal(&key, &serde_json::to_vec(&secrets)?)?;
        crate::write_private(&self.path, &serde_json::to_string_pretty(&sealed)?)
    }

    /// The encryption key: from `CALX_AUTH_KEY`, else from the keychain,
    /// creating it there when `create` is set and there is none yet.
    fn key(&self, create: bool) -> Result<Option<[u8; 32]>> {
        let mut cached = self.key.lock().unwrap();
        if let Some(key) = *cached {
            return Ok(Some(key));
        }
        let key = match std::env::var(KEY_ENV) {
            Ok(text) => Some(decode_key(&text).with_context(|| format!("{KEY_ENV} is not valid"))?),
            Err(_) => keychain_key(create)?,
        };
        *cached = key;
        Ok(key)
    }
}

fn keychain_key(create: bool) -> Result<Option<[u8; 32]>> {
    let entry = keyring::Entry::new(KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT).map_err(keychain_error)?;
    match entry.get_password() {
        Ok(text) => Ok(Some(
            decode_key(&text).context("the key in the keychain is not valid")?,
        )),
        Err(keyring::Error::NoEntry) if create => {
            let mut key = [0u8; 32];
            getrandom::fill(&mut key).map_err(|e| anyhow!("no system randomness: {e}"))?;
            entry
                .set_password(&STANDARD.encode(key))
                .map_err(keychain_error)?;
            Ok(Some(key))
        }
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(keychain_error(e)),
    }
}

fn keychain_error(e: keyring::Error) -> anyhow::Error {
    let hint = if cfg!(target_os = "linux") {
        " On Linux, calx needs a Secret Service such as GNOME Keyring or KWallet."
    } else {
        ""
    };
    anyhow!(
        "could not use the OS keychain: {e}.{hint} Without a keychain, set {KEY_ENV} to a \
         base64 string of 32 random bytes, such as from `openssl rand -base64 32`."
    )
}

fn decode_key(text: &str) -> Result<[u8; 32]> {
    let bytes = STANDARD.decode(text.trim()).context("not base64")?;
    bytes
        .try_into()
        .map_err(|b: Vec<u8>| anyhow!("{} bytes instead of 32", b.len()))
}

fn seal(key: &[u8; 32], plaintext: &[u8]) -> Result<Sealed> {
    // XChaCha's 24-byte nonce is long enough to pick at random for every save.
    let mut nonce = [0u8; 24];
    getrandom::fill(&mut nonce).map_err(|e| anyhow!("no system randomness: {e}"))?;
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("the key is 32 bytes");
    let ciphertext = cipher
        .encrypt(&XNonce::from(nonce), plaintext)
        .map_err(|_| anyhow!("could not encrypt the credentials"))?;
    Ok(Sealed {
        version: 1,
        nonce: STANDARD.encode(nonce),
        ciphertext: STANDARD.encode(ciphertext),
    })
}

fn open(key: &[u8; 32], sealed: &Sealed) -> Result<Vec<u8>> {
    if sealed.version != 1 {
        bail!(
            "the credentials file is version {}, newer than this calx understands",
            sealed.version
        );
    }
    let nonce: [u8; 24] = STANDARD
        .decode(&sealed.nonce)
        .ok()
        .and_then(|n| n.try_into().ok())
        .context("the credentials file has an invalid nonce")?;
    let ciphertext = STANDARD
        .decode(&sealed.ciphertext)
        .context("the credentials file is damaged")?;
    let cipher = XChaCha20Poly1305::new_from_slice(key).expect("the key is 32 bytes");
    cipher.decrypt(&XNonce::from(nonce), ciphertext.as_slice()).map_err(|_| {
        anyhow!("could not decrypt the saved credentials (wrong key or damaged file); run `calx login` again")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential() -> Credential {
        Credential {
            access_token: "a".repeat(2106),
            refresh_token: "r".repeat(196),
            expires_at: 1_900_000_000,
            client_id: "client".into(),
        }
    }

    fn store_with_key(dir: &Path, key: [u8; 32]) -> CredentialStore {
        let store = CredentialStore::at(dir.join("auth.json"));
        *store.key.lock().unwrap() = Some(key);
        store
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("calx-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn saves_encrypted_and_loads_back() {
        let dir = temp_dir("roundtrip");
        let store = store_with_key(&dir, [7; 32]);
        store.save(Some(&credential())).unwrap();
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(
            !text.contains("rrrr"),
            "the file holds the token in plain text"
        );
        assert_eq!(
            store.load().unwrap().unwrap().access_token,
            credential().access_token
        );

        // The same file with a different key fails instead of returning garbage.
        let other = store_with_key(&dir, [8; 32]);
        assert!(
            other
                .load()
                .unwrap_err()
                .to_string()
                .contains("could not decrypt")
        );
    }

    #[test]
    fn encrypts_a_plain_file_from_an_earlier_version() {
        let dir = temp_dir("migrate");
        let store = store_with_key(&dir, [7; 32]);
        let plain = serde_json::to_string(&Secrets {
            chatgpt: Some(credential()),
        })
        .unwrap();
        std::fs::write(store.path(), plain).unwrap();
        assert_eq!(
            store.load().unwrap().unwrap().refresh_token,
            credential().refresh_token
        );
        let text = std::fs::read_to_string(store.path()).unwrap();
        assert!(
            serde_json::from_str::<Sealed>(&text).is_ok(),
            "not encrypted: {text}"
        );
        assert_eq!(store.load().unwrap().unwrap().client_id, "client");
    }
}

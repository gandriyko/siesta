//! Connection passwords are encrypted with a local key (`<config dir>/key`, 32 random bytes, mode 0600)
//! using XChaCha20-Poly1305; the connection name is the AAD, so a ciphertext copied to another
//! connection does not decrypt. Stored as `"v1:<base64(nonce || ciphertext)>"` in the connection file.
//!
//! This keeps passwords out of plaintext leaks (screenshots, grep, backups, synced connection files
//! without the key). It does not protect against anything running as the user: that can read the key.

use std::io::Write;
use std::path::{Path, PathBuf};

use base64::{Engine, engine::general_purpose::STANDARD};
use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{KeyInit, XChaCha20Poly1305, XNonce};

const PREFIX: &str = "v1:";
const NONCE: usize = 24;

pub fn key_path(config: &Path) -> PathBuf {
    config.join("key")
}

/// The key; `create` makes it when missing (encrypting), otherwise a missing key is an error.
fn key(config: &Path, create: bool) -> Result<[u8; 32], String> {
    let path = key_path(config);
    match std::fs::read(&path) {
        Ok(b) => return b.try_into().map_err(|_| format!("{}: the key file must be exactly 32 bytes", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {}
        Err(e) => return Err(format!("{}: {e}", path.display())),
    }
    let mut k = [0u8; 32];
    getrandom::fill(&mut k).map_err(|e| format!("random numbers unavailable: {e}"))?;
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    std::fs::create_dir_all(config).map_err(err)?;
    // Unique per attempt (a second draw, not the key).
    let mut id = [0u8; 8];
    getrandom::fill(&mut id).map_err(|e| format!("random numbers unavailable: {e}"))?;
    let tmp = config.join(format!(".key.{}.tmp", u64::from_le_bytes(id)));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    opts.open(&tmp).and_then(|mut f| f.write_all(&k).and_then(|()| f.sync_all())).map_err(err)?;
    // A hard link publishes the complete file and never replaces one that a racing process made first.
    let linked = std::fs::hard_link(&tmp, &path);
    let _ = std::fs::remove_file(&tmp);
    match linked {
        Ok(()) => Ok(k),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => key(config, false),
        Err(e) => Err(err(e)),
    }
}

fn cipher(config: &Path, create: bool) -> Result<XChaCha20Poly1305, String> {
    XChaCha20Poly1305::new_from_slice(&key(config, create)?).map_err(|e| e.to_string())
}

pub fn encrypt(config: &Path, name: &str, password: &str) -> Result<String, String> {
    let mut nonce = [0u8; NONCE];
    getrandom::fill(&mut nonce).map_err(|e| format!("random numbers unavailable: {e}"))?;
    let ct = cipher(config, true)?
        .encrypt(&XNonce::from(nonce), Payload { msg: password.as_bytes(), aad: name.as_bytes() })
        .map_err(|e| e.to_string())?;
    Ok(format!("{PREFIX}{}", STANDARD.encode([&nonce[..], &ct].concat())))
}

pub fn decrypt(config: &Path, name: &str, enc: &str) -> Result<String, String> {
    let fail = || format!("Can't decrypt the password of `{name}` (the key in {} is missing or different). Set the password again in the connection form.", key_path(config).display());
    let cipher = cipher(config, false).map_err(|e| if key_path(config).exists() { format!("Can't decrypt the password of `{name}`: {e}") } else { fail() })?;
    let raw = enc.strip_prefix(PREFIX).and_then(|b| STANDARD.decode(b).ok()).filter(|b| b.len() > NONCE).ok_or_else(fail)?;
    let (nonce, ct) = raw.split_at(NONCE);
    let nonce: [u8; NONCE] = nonce.try_into().map_err(|_| fail())?;
    let pt = cipher.decrypt(&XNonce::from(nonce), Payload { msg: ct, aad: name.as_bytes() }).map_err(|_| fail())?;
    String::from_utf8(pt).map_err(|_| fail())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_created_once_with_private_mode() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().join("cfg");
        assert!(decrypt(&d, "a", "v1:AAAA").unwrap_err().contains("Can't decrypt"));
        assert!(!key_path(&d).exists(), "decrypting must not create a key");
        let k = key(&d, true).unwrap();
        assert_eq!(key(&d, true).unwrap(), k);
        assert_eq!(std::fs::read(key_path(&d)).unwrap(), k);
        #[cfg(unix)]
        assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(key_path(&d)).unwrap().permissions()) & 0o777, 0o600);
        // No temp file is left behind.
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
    }

    #[test]
    fn racing_creators_agree_on_one_key() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path().to_path_buf();
        let keys: Vec<_> = (0..8).map(|_| { let d = d.clone(); std::thread::spawn(move || key(&d, true).unwrap()) }).collect::<Vec<_>>().into_iter().map(|h| h.join().unwrap()).collect();
        assert!(keys.iter().all(|k| *k == keys[0]));
        assert_eq!(std::fs::read(key_path(&d)).unwrap(), keys[0]);
    }

    #[test]
    fn roundtrip_aad_tamper_and_wrong_key() {
        let t = tempfile::tempdir().unwrap();
        let d = t.path();
        let e = encrypt(d, "dev", "p@ss wörd").unwrap();
        assert!(e.starts_with("v1:") && !e.contains("p@ss"));
        assert_ne!(e, encrypt(d, "dev", "p@ss wörd").unwrap(), "fresh nonce each time");
        assert_eq!(decrypt(d, "dev", &e).unwrap(), "p@ss wörd");
        assert_eq!(decrypt(d, "dev", &encrypt(d, "dev", "").unwrap()).unwrap(), "");
        // A copy under another name fails.
        assert!(decrypt(d, "prod", &e).unwrap_err().contains("Can't decrypt the password of `prod`"));
        // Tampered ciphertext, bad prefix, garbage.
        let mut raw = STANDARD.decode(&e[3..]).unwrap();
        *raw.last_mut().unwrap() ^= 1;
        assert!(decrypt(d, "dev", &format!("v1:{}", STANDARD.encode(raw))).is_err());
        assert!(decrypt(d, "dev", &e.replacen("v1:", "v2:", 1)).is_err() && decrypt(d, "dev", "v1:!!").is_err());
        // Another key.
        std::fs::write(key_path(d), [7u8; 32]).unwrap();
        assert!(decrypt(d, "dev", &e).unwrap_err().contains("missing or different"));
        // Missing key and a key of the wrong length.
        std::fs::write(key_path(d), [7u8; 5]).unwrap();
        assert!(decrypt(d, "dev", &e).unwrap_err().contains("32 bytes"));
        assert!(encrypt(d, "dev", "x").unwrap_err().contains("32 bytes"));
        std::fs::remove_file(key_path(d)).unwrap();
        assert!(decrypt(d, "dev", &e).unwrap_err().contains("missing or different"));
    }
}

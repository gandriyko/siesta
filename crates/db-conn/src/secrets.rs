//! The one seam to the OS credential store (macOS Keychain, Windows Credential Manager, Linux
//! Secret Service): `get` / `set` / `delete` by account under the service `siesta`.
//! Tests switch to keyring's in-memory mock with [`use_memory_store`]; they never touch the real store.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

use keyring_core::Error as KrError;

pub const SERVICE: &str = "siesta";

#[derive(Debug, PartialEq, Eq)]
pub enum SecretError {
    /// No entry for the account.
    Missing,
    /// The platform store cannot be used (no Secret Service, locked, access denied).
    Unavailable(String),
    Failed(String),
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("no such entry"),
            Self::Unavailable(e) => write!(f, "system keychain unavailable: {e}"),
            Self::Failed(e) => write!(f, "system keychain: {e}"),
        }
    }
}

static MEMORY: AtomicBool = AtomicBool::new(false);

thread_local! {
    static OFFLINE: Cell<bool> = const { Cell::new(false) };
}

/// Tests only: use keyring's in-memory mock store for the whole process.
#[doc(hidden)]
pub fn use_memory_store() {
    if !MEMORY.swap(true, Ordering::SeqCst) {
        keyring_core::set_default_store(keyring_core::mock::Store::new().expect("mock store"));
    }
}

/// Tests only: makes the store "unavailable" on this thread (the headless-Linux case).
#[doc(hidden)]
pub fn set_unavailable(on: bool) {
    OFFLINE.set(on);
}

fn map(e: KrError) -> SecretError {
    match e {
        KrError::NoEntry => SecretError::Missing,
        KrError::NoDefaultStore | KrError::NoStorageAccess(_) | KrError::PlatformFailure(_) | KrError::NotSupportedByStore(_) => SecretError::Unavailable(e.to_string()),
        e => SecretError::Failed(e.to_string()),
    }
}

fn entry(account: &str) -> Result<keyring_core::Entry, SecretError> {
    if OFFLINE.get() {
        return Err(SecretError::Unavailable("store switched off (test)".into()));
    }
    if MEMORY.load(Ordering::SeqCst) {
        return keyring_core::Entry::new(SERVICE, account).map_err(map);
    }
    Ok(keyring::Entry::new(SERVICE, account).map_err(map)?.inner)
}

pub fn get(account: &str) -> Result<String, SecretError> {
    entry(account)?.get_password().map_err(map)
}

pub fn set(account: &str, password: &str) -> Result<(), SecretError> {
    entry(account)?.set_password(password).map_err(map)
}

/// A missing entry is not an error.
pub fn delete(account: &str) -> Result<(), SecretError> {
    match entry(account)?.delete_credential() {
        Err(KrError::NoEntry) => Ok(()),
        r => r.map_err(map),
    }
}

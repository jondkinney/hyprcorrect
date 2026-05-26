//! Secret storage via the OS keychain.
//!
//! API keys (today only LLM providers) live in libsecret / kwallet on
//! Linux, the macOS Keychain on macOS, and the Credential Manager on
//! Windows — never on disk in `config.toml`.
//!
//! Entries are keyed by `(service = "hyprcorrect", account = name)`.
//! `name` is something descriptive like `"llm.anthropic"`.

/// An error talking to the OS keychain.
#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    /// The OS keychain rejected the request — typically a missing
    /// daemon (libsecret service not running) or a user lock.
    #[error("keychain: {0}")]
    Keychain(String),
}

const SERVICE: &str = "hyprcorrect";

/// Fetch a stored secret. `Ok(None)` means "no entry" (not an error).
///
/// # Errors
///
/// Returns [`SecretError`] if the OS keychain is unreachable.
pub fn get(name: &str) -> Result<Option<String>, SecretError> {
    let entry = entry(name)?;
    match entry.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(SecretError::Keychain(e.to_string())),
    }
}

/// Store (or overwrite) a secret.
///
/// # Errors
///
/// Returns [`SecretError`] if the OS keychain is unreachable.
pub fn set(name: &str, value: &str) -> Result<(), SecretError> {
    let entry = entry(name)?;
    entry
        .set_password(value)
        .map_err(|e| SecretError::Keychain(e.to_string()))
}

/// Remove a stored secret. Removing a non-existent entry is not an
/// error.
///
/// # Errors
///
/// Returns [`SecretError`] if the OS keychain is unreachable.
pub fn delete(name: &str) -> Result<(), SecretError> {
    let entry = entry(name)?;
    match entry.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(SecretError::Keychain(e.to_string())),
    }
}

fn entry(name: &str) -> Result<keyring::Entry, SecretError> {
    keyring::Entry::new(SERVICE, name).map_err(|e| SecretError::Keychain(e.to_string()))
}

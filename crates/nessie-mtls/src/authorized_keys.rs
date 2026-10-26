//! The SSH `authorized_keys` trust list.

use std::path::Path;

/// The set of ed25519 SSH public keys a peer's binding must be signed by.
/// Empty means "trust nobody".
#[derive(Debug, Clone, Default)]
pub struct AuthorizedKeys {
    keys: Vec<[u8; 32]>,
}

impl AuthorizedKeys {
    /// Parse OpenSSH `authorized_keys` text. Non-ed25519 and malformed lines are
    /// skipped with a warning; an empty result trusts nobody.
    pub fn parse(_text: &str) -> Self {
        todo!()
    }

    /// Read `path`. A missing or unreadable file yields an empty set (trust
    /// nobody, fail closed) and logs an error.
    pub fn load(_path: &Path) -> Self {
        todo!()
    }

    /// The raw ed25519 keys, in file order.
    pub fn keys(&self) -> &[[u8; 32]] {
        &self.keys
    }
}

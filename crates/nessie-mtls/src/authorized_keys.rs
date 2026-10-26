//! The SSH `authorized_keys` trust list.

use std::path::Path;

/// The set of ed25519 SSH public keys a peer's binding must be signed by.
/// Empty means "trust nobody".
#[derive(Debug, Clone, Default)]
pub struct AuthorizedKeys {
    keys: Vec<[u8; 32]>,
}

impl AuthorizedKeys {
    /// Parse OpenSSH `authorized_keys` text with `ssh_key`'s entry parser, taking
    /// only each entry's own public key (never text from options or comments).
    /// Non-ed25519 entries and lines that do not parse are skipped with a warning
    /// and contribute no key; an empty result trusts nobody.
    pub fn parse(text: &str) -> Self {
        let mut keys = Vec::new();
        for entry in ssh_key::AuthorizedKeys::new(text) {
            match entry {
                // ssh_key is lax about quoting; an unbalanced quote is a malformed line.
                Ok(e) if e.config_opts().as_str().matches('"').count() % 2 != 0 => {
                    tracing::warn!(
                        "authorized_keys: skipping entry with unbalanced quote in options"
                    );
                }
                Ok(e) => match e.public_key().key_data().ed25519() {
                    Some(k) => keys.push(k.0),
                    None => tracing::warn!("authorized_keys: skipping non-ed25519 entry"),
                },
                Err(e) => tracing::warn!(error = %e, "authorized_keys: skipping unparseable line"),
            }
        }
        Self { keys }
    }

    /// Read `path`. A missing or unreadable file yields an empty set (trust
    /// nobody, fail closed) and logs an error.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text),
            Err(e) => {
                tracing::error!(path = %path.display(), error = %e, "authorized_keys unreadable; trusting nobody");
                Self::default()
            }
        }
    }

    /// The raw ed25519 keys, in file order.
    pub fn keys(&self) -> &[[u8; 32]] {
        &self.keys
    }
}

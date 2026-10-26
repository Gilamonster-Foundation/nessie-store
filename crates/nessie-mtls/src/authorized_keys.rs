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
    pub fn parse(text: &str) -> Self {
        let mut keys = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match ed25519_key(line) {
                Some(Ok(k)) => keys.push(k),
                Some(Err(e)) => {
                    tracing::warn!(line = n + 1, error = %e, "authorized_keys: skipping malformed ed25519 line")
                }
                None => tracing::warn!(line = n + 1, "authorized_keys: skipping non-ed25519 line"),
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

/// The ed25519 key on `line`, if it has one. Options before the key type
/// (`no-pty,command="..."`) and a trailing comment are tolerated.
fn ed25519_key(line: &str) -> Option<Result<[u8; 32], ssh_key::Error>> {
    let mut tokens = line.split_whitespace();
    tokens.find(|t| *t == "ssh-ed25519")?;
    let blob = tokens.next()?;
    Some(
        ssh_key::PublicKey::from_openssh(&format!("ssh-ed25519 {blob}")).and_then(|k| {
            k.key_data()
                .ed25519()
                .map(|e| e.0)
                .ok_or(ssh_key::Error::AlgorithmUnknown)
        }),
    )
}

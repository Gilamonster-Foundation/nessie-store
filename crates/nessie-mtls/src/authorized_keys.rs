//! The SSH `authorized_keys` trust list.

use std::path::Path;

/// The set of ed25519 SSH public keys a peer's binding must be signed by.
/// Empty means "trust nobody".
///
/// File format: one `ssh-ed25519` public key per line, **no options**.
#[derive(Debug, Clone, Default)]
pub struct AuthorizedKeys {
    keys: Vec<[u8; 32]>,
}

impl AuthorizedKeys {
    /// Parse `authorized_keys` text. The only accepted line shape is
    /// `ssh-ed25519 <base64> [comment]` (what `id_ed25519.pub` contains): the
    /// first token must be the literal `ssh-ed25519`. Blank lines and `#` lines
    /// are ignored; every other line (options, other key types, junk) is skipped
    /// with a warning and never contributes a key. The comment is ignored.
    pub fn parse(text: &str) -> Self {
        let mut keys = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            match ed25519_key(line) {
                Some(k) => keys.push(k),
                None => tracing::warn!(
                    line = n + 1,
                    "authorized_keys: skipping line that is not `ssh-ed25519 <key> [comment]`"
                ),
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

/// The key on a line of the exact shape `ssh-ed25519 <base64> [comment]`.
fn ed25519_key(line: &str) -> Option<[u8; 32]> {
    let mut tokens = line.split_whitespace();
    (tokens.next()? == "ssh-ed25519").then_some(())?;
    let key = ssh_key::PublicKey::from_openssh(&format!("ssh-ed25519 {}", tokens.next()?)).ok()?;
    key.key_data().ed25519().map(|k| k.0)
}

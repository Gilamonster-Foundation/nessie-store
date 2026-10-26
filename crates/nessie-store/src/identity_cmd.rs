//! `nessie-store identity issue` — the thin CLI body over `nessie-mtls`.

use std::path::Path;

use agent_mesh_protocol::{AgentMetadata, Caveats, Fingerprint, GitHubBinding, UserKey};
use anyhow::Context as _;
use nessie_mtls::NodeIdentity;

/// Issue this node's agent key from the operator's user key and write the three
/// identity files into `dir`. Returns the new agent fingerprint; the seed is
/// never returned or printed.
pub fn issue(
    user_key: &Path,
    binding: &Path,
    role: &str,
    host: &str,
    dir: &Path,
) -> anyhow::Result<Fingerprint> {
    let user = UserKey::load(user_key)
        .map_err(|e| anyhow::anyhow!("loading user key {}: {e}", user_key.display()))?;
    let bytes =
        std::fs::read(binding).with_context(|| format!("reading binding {}", binding.display()))?;
    let binding: GitHubBinding = serde_json::from_slice(&bytes)
        .with_context(|| format!("parsing binding {}", binding.display()))?;
    let metadata = AgentMetadata {
        role: role.to_string(),
        host: host.to_string(),
        capabilities: Vec::new(),
        issued_at: chrono::Utc::now().to_rfc3339(),
        expires_at: None,
        caveats: Caveats::top(),
    };
    let node = NodeIdentity::issue(&user, binding, metadata)?;
    node.save(dir)?;
    Ok(node.fingerprint())
}

/// Write `client.crt` and `client.key` (PEM) into `out`: a credential for a
/// Bazel client (`--tls_client_certificate` / `--tls_client_key`).
///
/// The credential is a **delegated sub-agent** of the node in `identity_dir`
/// (attenuation-only, a fresh key, rooted at the same operator), so the node's
/// own seed never leaves `identity_dir`. Refuses to overwrite either file.
pub fn export_client_cert(identity_dir: &Path, out: &Path) -> anyhow::Result<Fingerprint> {
    let node = NodeIdentity::load(identity_dir)?;
    let metadata = AgentMetadata {
        role: "bazel-client".to_string(),
        issued_at: chrono::Utc::now().to_rfc3339(),
        ..node.metadata().clone()
    };
    let client = node.delegate(metadata)?;
    std::fs::create_dir_all(out).with_context(|| format!("creating {}", out.display()))?;
    write_new(&out.join("client.crt"), client.cert_pem().as_bytes(), 0o644)?;
    write_new(&out.join("client.key"), client.key_pem()?.as_bytes(), 0o600)?;
    Ok(client.fingerprint())
}

fn write_new(path: &Path, bytes: &[u8], mode: u32) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .and_then(|mut f| f.write_all(bytes))
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_mesh_protocol::UserKey;
    use ssh_key::private::{Ed25519Keypair, KeypairData};
    use std::os::unix::fs::PermissionsExt;

    fn fixtures(dir: &Path) -> (std::path::PathBuf, std::path::PathBuf, UserKey) {
        let user = UserKey::generate();
        let user_path = dir.join("user.pem");
        user.save(&user_path).unwrap();
        let ssh = ssh_key::PrivateKey::new(
            KeypairData::Ed25519(Ed25519Keypair::from_seed(&[9; 32])),
            "",
        )
        .unwrap();
        let binding = GitHubBinding::sign(&user.public(), &ssh, None).unwrap();
        let binding_path = dir.join("binding.json");
        std::fs::write(&binding_path, serde_json::to_vec(&binding).unwrap()).unwrap();
        (user_path, binding_path, user)
    }

    #[test]
    fn writes_loadable_identity_with_private_seed_and_returns_only_fingerprint() {
        let tmp = tempfile::tempdir().unwrap();
        let (u, b, user) = fixtures(tmp.path());
        let out = tmp.path().join("identity");
        let fp = issue(&u, &b, "nessie", "h1", &out).unwrap();
        let mode = std::fs::metadata(out.join("agent.seed"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let node = NodeIdentity::load(&out).unwrap();
        assert_eq!(node.fingerprint(), fp);
        let chain = std::fs::read_to_string(out.join("agent.chain.json")).unwrap();
        assert!(chain.contains("\"nessie\"") && chain.contains("\"h1\""));
        assert_ne!(node.fingerprint(), user.fingerprint());
    }

    #[test]
    fn refuses_binding_for_another_user_key_and_to_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let (u, b, _) = fixtures(tmp.path());
        let (_, other_binding, _) = fixtures(&tempfile::tempdir().unwrap().keep());
        let out = tmp.path().join("identity");
        assert!(issue(&u, &other_binding, "n", "h", &out).is_err());
        assert!(!out.join("agent.seed").exists());
        issue(&u, &b, "n", "h", &out).unwrap();
        assert!(issue(&u, &b, "n", "h", &out).is_err());
    }

    #[test]
    fn missing_user_key_is_an_error_naming_the_path_not_key_material() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, b, _) = fixtures(tmp.path());
        let err = issue(
            &tmp.path().join("absent.pem"),
            &b,
            "n",
            "h",
            &tmp.path().join("o"),
        )
        .unwrap_err();
        assert!(format!("{err:#}").contains("absent.pem"));
    }
}

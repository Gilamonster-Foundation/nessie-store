//! TLS certificate provisioning.
//!
//! ONTAP REST is HTTPS-only, so the daemon always has a server cert. Resolution
//! mirrors the predecessor's three tiers, in priority order:
//!
//! 1. **Vault PKI** — `$VAULT_PKI_CERT_DIR/<name>.crt` + `.key`
//!    (`$VAULT_PKI_CERT_NAME`, default `ontap-sim`), if present.
//! 2. **Existing self-signed** — `cert.pem` + `key.pem` already in the TLS dir.
//! 3. **Generated self-signed** — a fresh RSA-equivalent cert (SAN
//!    `ontap-sim.home.lan`, `localhost`, `127.0.0.1`) written to the TLS dir.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// Resolved certificate + private-key file paths.
#[derive(Debug, Clone)]
pub struct CertPaths {
    /// PEM certificate chain.
    pub cert: PathBuf,
    /// PEM private key.
    pub key: PathBuf,
}

/// Ensure a usable server certificate, returning its file paths.
pub fn ensure_cert(tls_dir: &Path) -> anyhow::Result<CertPaths> {
    if let Some(paths) = vault_cert() {
        tracing::info!(cert = %paths.cert.display(), "using Vault PKI certificate");
        return Ok(paths);
    }
    let cert = tls_dir.join("cert.pem");
    let key = tls_dir.join("key.pem");
    if cert.exists() && key.exists() {
        tracing::info!(cert = %cert.display(), "using existing self-signed certificate");
        return Ok(CertPaths { cert, key });
    }
    generate_self_signed(tls_dir)
}

/// Tier 1: a Vault-issued cert, if the env points at one that exists on disk.
fn vault_cert() -> Option<CertPaths> {
    let dir = std::env::var("VAULT_PKI_CERT_DIR").ok()?;
    let name = std::env::var("VAULT_PKI_CERT_NAME").unwrap_or_else(|_| "ontap-sim".to_string());
    let cert = PathBuf::from(&dir).join(format!("{name}.crt"));
    let key = PathBuf::from(&dir).join(format!("{name}.key"));
    (cert.exists() && key.exists()).then_some(CertPaths { cert, key })
}

/// Tier 3: generate a self-signed cert + key into `tls_dir` (key mode 0600).
fn generate_self_signed(tls_dir: &Path) -> anyhow::Result<CertPaths> {
    std::fs::create_dir_all(tls_dir)
        .with_context(|| format!("creating TLS dir {}", tls_dir.display()))?;
    let sans = vec![
        "ontap-sim.home.lan".to_string(),
        "localhost".to_string(),
        "127.0.0.1".to_string(),
    ];
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(sans).context("generating self-signed certificate")?;

    let cert_path = tls_dir.join("cert.pem");
    let key_path = tls_dir.join("key.pem");
    std::fs::write(&cert_path, cert.pem()).context("writing cert.pem")?;
    std::fs::write(&key_path, key_pair.serialize_pem()).context("writing key.pem")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
            .context("chmod 0600 key.pem")?;
    }
    tracing::info!(cert = %cert_path.display(), "generated self-signed certificate");
    Ok(CertPaths {
        cert: cert_path,
        key: key_path,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("nessie-tls-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn generates_then_reuses_self_signed() {
        let dir = tmp_dir();
        let first = ensure_cert(&dir).expect("generate");
        assert!(first.cert.exists() && first.key.exists());
        // Second call must reuse the existing files, not regenerate.
        let cert_bytes = std::fs::read(&first.cert).unwrap();
        let second = ensure_cert(&dir).expect("reuse");
        assert_eq!(second.cert, first.cert);
        assert_eq!(std::fs::read(&second.cert).unwrap(), cert_bytes);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn generated_cert_loads_into_rustls() {
        // Validates the whole cert -> rustls path the daemon uses to serve HTTPS.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let dir = tmp_dir();
        let paths = ensure_cert(&dir).expect("generate");
        let loaded =
            axum_server::tls_rustls::RustlsConfig::from_pem_file(&paths.cert, &paths.key).await;
        assert!(
            loaded.is_ok(),
            "rustls must load the generated cert: {loaded:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}

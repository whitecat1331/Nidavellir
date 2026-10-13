use std::sync::OnceLock;

use rustls::ClientConfig;
use rustls_platform_verifier::ConfigVerifierExt;

static TLS_CONFIG: OnceLock<rustls::ClientConfig> = OnceLock::new();

pub fn tls_config() -> ClientConfig {
    TLS_CONFIG
        .get_or_init(|| {
            // rustls uses the `aws_lc_rs` provider by default
            // This only errors if the default provider has already
            // been installed. We can ignore this `Result`.
            rustls::crypto::aws_lc_rs::default_provider()
                .install_default()
                .ok();

            ClientConfig::with_platform_verifier().unwrap_or_else(|error| {
                log::error!(
                    "failed to load platform TLS certificate verifier, falling back to bundled webpki roots: {error}"
                );
                ClientConfig::builder()
                    .with_root_certificates(rustls::RootCertStore {
                        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
                    })
                    .with_no_client_auth()
            })
        })
        .clone()
}

/// Builds a TLS configuration that trusts *only* the PEM-encoded certificates
/// in `ca_pem`, replacing the platform verifier with a restricted root store.
///
/// This pins a hypervisor's self-signed node certs without ever disabling
/// verification: the standard chain verifier still runs, it just has no trust
/// anchors other than the provided certs.
pub fn pinned_tls_config(ca_pem: &[u8]) -> anyhow::Result<ClientConfig> {
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .ok();

    let mut reader = std::io::Cursor::new(ca_pem);
    let certs = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| anyhow::anyhow!("failed to parse pinned certificate PEM: {error}"))?;
    anyhow::ensure!(!certs.is_empty(), "no certificates found in pinned PEM");

    let mut roots = rustls::RootCertStore::empty();
    for cert in certs {
        roots
            .add(cert)
            .map_err(|error| anyhow::anyhow!("failed to add pinned certificate: {error}"))?;
    }

    Ok(ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth())
}

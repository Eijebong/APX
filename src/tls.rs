use anyhow::{Context, Result};
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use std::fs::File;
use std::sync::Arc;
use tokio_rustls::rustls::ServerConfig;

pub fn load_tls_config(cert_path: &str, key_path: &str) -> Result<Arc<ServerConfig>> {
    let cert_file = File::open(cert_path).context("Failed to open certificate file")?;
    let certs: Vec<CertificateDer> = CertificateDer::pem_reader_iter(cert_file)
        .collect::<Result<Vec<_>, _>>()
        .context("Failed to parse certificates")?;

    let key_file = File::open(key_path).context("Failed to open private key file")?;
    let key = PrivateKeyDer::from_pem_reader(key_file).context("Failed to parse private key")?;

    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("Failed to build TLS config")?;

    Ok(Arc::new(config))
}

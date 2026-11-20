use std::{
    collections::HashMap,
    io,
    path::Path,
    sync::{Arc, RwLock},
};

use axum_server::tls_rustls::RustlsConfig;
use rustls::{
    ServerConfig,
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use tokio::fs;

pub(super) fn server_config(
    crypto: Arc<CryptoProvider>,
    resolver: Arc<SniCertificates>,
) -> Result<RustlsConfig, rustls::Error> {
    let mut config = ServerConfig::builder_with_provider(crypto)
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    Ok(RustlsConfig::from_config(Arc::new(config)))
}

#[derive(Debug, Default)]
pub(super) struct SniCertificates(RwLock<HashMap<String, Arc<CertifiedKey>>>);

impl SniCertificates {
    pub(super) fn publish(&self, name: &str, certificate: CertifiedKey) {
        self.0
            .write()
            .expect("certificate map lock poisoned")
            .insert(name.to_owned(), Arc::new(certificate));
    }
}

impl ResolvesServerCert for SniCertificates {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name()?.to_ascii_lowercase();
        self.0
            .read()
            .expect("certificate map lock poisoned")
            .get(&name)
            .cloned()
    }
}

pub(super) async fn load_certificate(
    cert: &Path,
    key: &Path,
    crypto: &CryptoProvider,
) -> io::Result<CertifiedKey> {
    let chain = fs::read(cert)
        .await
        .map_err(|error| file_error(cert, error))?;
    let chain = CertificateDer::pem_slice_iter(&chain)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| file_error(cert, error))?;
    let private_key = fs::read(key)
        .await
        .map_err(|error| file_error(key, error))?;
    let private_key =
        PrivateKeyDer::from_pem_slice(&private_key).map_err(|error| file_error(key, error))?;
    CertifiedKey::from_der(chain, private_key, crypto).map_err(io::Error::other)
}

fn file_error(path: &Path, error: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!("{}: {error}", path.display()))
}

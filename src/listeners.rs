use std::{
    collections::HashMap,
    error::Error,
    io,
    net::{SocketAddr, TcpListener as StdListener},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, RwLock},
    time::Duration,
};

use axum::{Router, extract::Path as RoutePath, http::StatusCode, routing::get};
use axum_server::tls_rustls::RustlsConfig;
use rustls::{
    ServerConfig,
    crypto::{CryptoProvider, ring},
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use tokio::{fs, io::AsyncReadExt, net::TcpListener, process::Command, time};

use crate::config::{Listeners, TlsHost};

const CHALLENGE_DIR: &str = ".well-known/acme-challenge";
const CERTBOT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
const RENEWAL_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

pub(crate) async fn serve(listeners: Listeners, router: Router) -> Result<(), Box<dyn Error>> {
    let http = match listeners.http {
        Some(addr) => Some(TcpListener::bind(addr).await?),
        None => None,
    };

    let Some((addr, options)) = listeners.https else {
        return Ok(serve_http(http, router).await?);
    };

    let listener = bind_https(addr)?;
    let resolver = Arc::new(SniCertificates::default());
    let crypto = Arc::new(ring::default_provider());
    let mut tls = ServerConfig::builder_with_provider(crypto.clone())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(resolver.clone());
    tls.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let tls = RustlsConfig::from_config(Arc::new(tls));

    let mut managed_hosts = Vec::new();
    for host in options.hosts {
        match host {
            TlsHost::File { name, key, cert } => {
                let certificate = load_certificate(&cert, &key, &crypto)
                    .await
                    .map_err(|error| io::Error::other(format!("TLS host {name}: {error}")))?;
                resolver.publish(&name, certificate);
            }
            TlsHost::Certbot { name, account } => managed_hosts.push(ManagedHost {
                name,
                account,
                next_check: time::Instant::now(),
            }),
        }
    }

    let https = serve_https(listener, router.clone(), tls);
    if managed_hosts.is_empty() {
        tokio::try_join!(serve_http(http, router), https)?;
    } else {
        let certbot = Certbot::new(&options.state_dir, options.staging)?;
        for host in &managed_hosts {
            if let Ok(certificate) = certbot.load(host, &crypto).await {
                resolver.publish(&host.name, certificate);
            }
        }

        let challenges = certbot.webroot.join(CHALLENGE_DIR);
        let http_router = router.route(
            "/.well-known/acme-challenge/{token}",
            get(move |RoutePath(token): RoutePath<String>| challenge(challenges.clone(), token)),
        );
        tokio::select! {
            result = async { tokio::try_join!(serve_http(http, http_router), https) } => { result?; }
            () = certbot.maintain(managed_hosts, &resolver, &crypto) => {}
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct SniCertificates(RwLock<HashMap<String, Arc<CertifiedKey>>>);

impl SniCertificates {
    fn publish(&self, name: &str, certificate: CertifiedKey) {
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

async fn load_certificate(
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

async fn serve_http(listener: Option<TcpListener>, router: Router) -> io::Result<()> {
    if let Some(listener) = listener {
        eprintln!("HTTP listening on {}", listener.local_addr()?);
        axum::serve(listener, router).await?;
    }
    Ok(())
}

fn bind_https(addr: SocketAddr) -> io::Result<StdListener> {
    let listener = StdListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

async fn serve_https(listener: StdListener, router: Router, tls: RustlsConfig) -> io::Result<()> {
    eprintln!("HTTPS listening on {}", listener.local_addr()?);
    axum_server::from_tcp_rustls(listener, tls)?
        .serve(router.into_make_service())
        .await
}

async fn challenge(dir: PathBuf, token: String) -> Result<String, StatusCode> {
    let missing = StatusCode::NOT_FOUND;
    if token.is_empty()
        || token.len() > 255
        || !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(missing);
    }
    let path = dir.join(token);
    if !fs::symlink_metadata(&path)
        .await
        .map_err(|_| missing)?
        .is_file()
    {
        return Err(missing);
    }
    let mut body = String::new();
    fs::File::open(path)
        .await
        .map_err(|_| missing)?
        .take(4097)
        .read_to_string(&mut body)
        .await
        .map_err(|_| missing)?;
    if body.len() > 4096 {
        return Err(missing);
    }
    Ok(body)
}

struct ManagedHost {
    name: String,
    account: String,
    next_check: time::Instant,
}

struct Certbot {
    state: PathBuf,
    webroot: PathBuf,
    staging: bool,
}

impl Certbot {
    fn new(state_dir: &Path, staging: bool) -> io::Result<Self> {
        let state =
            std::path::absolute(state_dir)?.join(if staging { "staging" } else { "production" });
        let webroot = state.join("webroot");
        Ok(Self {
            state,
            webroot,
            staging,
        })
    }

    fn config_dir(&self, host: &ManagedHost) -> PathBuf {
        // Certbot reuses an existing account in its config directory regardless of --email.
        self.state.join("hosts").join(&host.name).join("config")
    }

    async fn load(&self, host: &ManagedHost, crypto: &CryptoProvider) -> io::Result<CertifiedKey> {
        let lineage = self.config_dir(host).join("live").join(&host.name);
        load_certificate(
            &lineage.join("fullchain.pem"),
            &lineage.join("privkey.pem"),
            crypto,
        )
        .await
    }

    async fn obtain(&self, host: &ManagedHost) -> io::Result<()> {
        fs::create_dir_all(self.webroot.join(CHALLENGE_DIR)).await?;
        let mut command = Command::new("certbot");
        command
            .args([
                "certonly",
                "--non-interactive",
                "--agree-tos",
                "--webroot",
                "--keep-until-expiring",
                "--no-directory-hooks",
                "--email",
            ])
            .arg(&host.account)
            .arg("--cert-name")
            .arg(&host.name)
            .arg("--domains")
            .arg(&host.name)
            .arg("--webroot-path")
            .arg(&self.webroot)
            .arg("--config-dir")
            .arg(self.config_dir(host))
            .arg("--work-dir")
            .arg(self.state.join("work"))
            .arg("--logs-dir")
            .arg(self.state.join("logs"));
        if self.staging {
            command.arg("--staging");
        }

        let mut child = command.stdin(Stdio::null()).kill_on_drop(true).spawn()?;
        let status = match time::timeout(CERTBOT_TIMEOUT, child.wait()).await {
            Ok(result) => result?,
            Err(_) => {
                child.kill().await?;
                return Err(io::Error::new(io::ErrorKind::TimedOut, "Certbot timed out"));
            }
        };
        if !status.success() {
            return Err(io::Error::other(format!("Certbot exited with {status}")));
        }
        Ok(())
    }

    async fn maintain(
        &self,
        mut hosts: Vec<ManagedHost>,
        resolver: &SniCertificates,
        crypto: &CryptoProvider,
    ) {
        // One child at a time because Certbot shares the work, log, and webroot directories.
        while let Some(host) = hosts.iter_mut().min_by_key(|host| host.next_check) {
            time::sleep_until(host.next_check).await;
            let obtained = self.obtain(host).await;
            if let Err(error) = &obtained {
                eprintln!("Certbot check for {} failed: {error}", host.name);
            }
            let loaded = match self.load(host, crypto).await {
                Ok(certificate) => {
                    resolver.publish(&host.name, certificate);
                    true
                }
                Err(error) => {
                    eprintln!("Cannot load Certbot certificate for {}: {error}", host.name);
                    false
                }
            };
            let delay = if loaded && obtained.is_ok() {
                RENEWAL_INTERVAL
            } else {
                RETRY_INTERVAL
            };
            host.next_check = time::Instant::now() + delay;
        }
    }
}

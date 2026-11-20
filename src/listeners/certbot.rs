use std::{
    io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use axum::{Router, extract::Path as RoutePath, http::StatusCode, routing::get};
use rustls::{crypto::CryptoProvider, sign::CertifiedKey};
use tokio::{fs, io::AsyncReadExt, process::Command, time};
use tokio_util::sync::CancellationToken;

use super::tls::{SniCertificates, load_certificate};

const CHALLENGE_DIR: &str = ".well-known/acme-challenge";
const CERTBOT_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const RETRY_INTERVAL: Duration = Duration::from_secs(5 * 60);
const RENEWAL_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

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

pub(super) struct ManagedHost {
    name: String,
    account: String,
    next_check: time::Instant,
}

impl ManagedHost {
    pub(super) fn new(name: String, account: String) -> Self {
        Self {
            name,
            account,
            next_check: time::Instant::now(),
        }
    }
}

pub(super) struct Certbot {
    state: PathBuf,
    webroot: PathBuf,
    staging: bool,
}

impl Certbot {
    pub(super) fn new(state_dir: &Path, staging: bool) -> io::Result<Self> {
        let state =
            std::path::absolute(state_dir)?.join(if staging { "staging" } else { "production" });
        let webroot = state.join("webroot");
        Ok(Self {
            state,
            webroot,
            staging,
        })
    }

    pub(super) async fn load_existing(
        &self,
        hosts: &[ManagedHost],
        resolver: &SniCertificates,
        crypto: &CryptoProvider,
    ) {
        for host in hosts {
            if let Ok(certificate) = self.load(host, crypto).await {
                resolver.publish(&host.name, certificate);
            }
        }
    }

    pub(super) fn challenge_router(&self, router: Router) -> Router {
        let challenges = self.webroot.join(CHALLENGE_DIR);
        router.route(
            "/.well-known/acme-challenge/{token}",
            get(move |RoutePath(token): RoutePath<String>| challenge(challenges.clone(), token)),
        )
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

    async fn obtain(&self, host: &ManagedHost, shutdown: &CancellationToken) -> io::Result<()> {
        fs::create_dir_all(self.webroot.join(CHALLENGE_DIR)).await?;
        if shutdown.is_cancelled() {
            return Ok(());
        }
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
        let status = tokio::select! {
            () = shutdown.cancelled() => {
                child.kill().await?;
                return Ok(());
            }
            result = time::timeout(CERTBOT_TIMEOUT, child.wait()) => match result {
                Ok(result) => result?,
                Err(_) => {
                    child.kill().await?;
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "Certbot timed out"));
                }
            }
        };
        if !status.success() {
            return Err(io::Error::other(format!("Certbot exited with {status}")));
        }
        Ok(())
    }

    pub(super) async fn maintain(
        &self,
        mut hosts: Vec<ManagedHost>,
        resolver: &SniCertificates,
        crypto: &CryptoProvider,
        shutdown: &CancellationToken,
    ) {
        // One child at a time because Certbot shares the work, log, and webroot directories.
        while let Some(host) = hosts.iter_mut().min_by_key(|host| host.next_check) {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                () = time::sleep_until(host.next_check) => {}
            }
            let obtained = self.obtain(host, shutdown).await;
            if let Err(error) = &obtained {
                eprintln!("Certbot check for {} failed: {error}", host.name);
            }
            if shutdown.is_cancelled() {
                return;
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

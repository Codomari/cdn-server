mod certbot;
mod tls;

use std::{
    error::Error,
    io,
    net::{SocketAddr, TcpListener as StdListener},
    sync::Arc,
};

use axum::Router;
use axum_server::Handle;
use axum_server::tls_rustls::RustlsConfig;
use rustls::crypto::ring;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use crate::config::{Listeners, TlsHost};

use certbot::{Certbot, ManagedHost};
use tls::{SniCertificates, load_certificate};

pub(crate) async fn serve(
    listeners: Listeners,
    router: Router,
    shutdown: CancellationToken,
) -> Result<(), Box<dyn Error>> {
    if shutdown.is_cancelled() {
        return Ok(());
    }
    let http = match listeners.http {
        Some(addr) => Some(TcpListener::bind(addr).await?),
        None => None,
    };

    let Some((addr, options)) = listeners.https else {
        return Ok(serve_http(http, router, shutdown).await?);
    };

    let listener = bind_https(addr)?;
    let resolver = Arc::new(SniCertificates::default());
    let crypto = Arc::new(ring::default_provider());
    let tls = tls::server_config(crypto.clone(), resolver.clone())?;

    let mut managed_hosts = Vec::new();
    for host in options.hosts {
        match host {
            TlsHost::File { name, key, cert } => {
                let certificate = load_certificate(&cert, &key, &crypto)
                    .await
                    .map_err(|error| io::Error::other(format!("TLS host {name}: {error}")))?;
                resolver.publish(&name, certificate);
            }
            TlsHost::Certbot { name, account } => {
                managed_hosts.push(ManagedHost::new(name, account))
            }
        }
    }

    let https = serve_https(listener, router.clone(), tls, shutdown.clone());
    if managed_hosts.is_empty() {
        let (http, https) = tokio::join!(serve_http(http, router, shutdown), https);
        http?;
        https?;
    } else {
        let certbot = Certbot::new(&options.state_dir, options.staging)?;
        certbot
            .load_existing(&managed_hosts, &resolver, &crypto)
            .await;
        let http_router = certbot.challenge_router(router);
        let (http, https, ()) = tokio::join!(
            serve_http(http, http_router, shutdown.clone()),
            https,
            certbot.maintain(managed_hosts, &resolver, &crypto, &shutdown),
        );
        http?;
        https?;
    }
    Ok(())
}

async fn serve_http(
    listener: Option<TcpListener>,
    router: Router,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let Some(listener) = listener else {
        return Ok(());
    };
    let result = async {
        eprintln!("HTTP listening on {}", listener.local_addr()?);
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown.clone().cancelled_owned())
            .await
    }
    .await;
    shutdown.cancel();
    result
}

fn bind_https(addr: SocketAddr) -> io::Result<StdListener> {
    let listener = StdListener::bind(addr)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

async fn serve_https(
    listener: StdListener,
    router: Router,
    tls: RustlsConfig,
    shutdown: CancellationToken,
) -> io::Result<()> {
    let handle = Handle::new();
    let server = async {
        eprintln!("HTTPS listening on {}", listener.local_addr()?);
        axum_server::from_tcp_rustls(listener, tls)?
            .handle(handle.clone())
            .serve(router.into_make_service())
            .await
    };
    tokio::pin!(server);
    let result = tokio::select! {
        result = &mut server => result,
        () = shutdown.cancelled() => {
            handle.graceful_shutdown(None);
            server.await
        }
    };
    shutdown.cancel();
    result
}

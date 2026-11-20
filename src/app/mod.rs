mod shutdown;

use std::{error::Error, io, time::Duration};

use axum::Router;
use tokio_util::sync::CancellationToken;

use crate::{cache::CacheWorkers, config::Listeners, listeners};

pub(crate) use shutdown::shutdown_signal;

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);

pub(crate) struct Application {
    listeners: Listeners,
    router: Router,
    cache_workers: CacheWorkers,
}

impl Application {
    pub(crate) fn new(listeners: Listeners, router: Router, cache_workers: CacheWorkers) -> Self {
        Self {
            listeners,
            router,
            cache_workers,
        }
    }

    pub(crate) async fn run(
        self,
        signal: impl Future<Output = io::Result<()>>,
    ) -> Result<(), Box<dyn Error>> {
        let shutdown = CancellationToken::new();
        let serve = async {
            let result = listeners::serve(self.listeners, self.router, shutdown.clone()).await;
            shutdown.cancel();
            result
        };
        let services = async {
            let (result, ()) = tokio::join!(serve, self.cache_workers.run(shutdown.clone()));
            result
        };
        tokio::pin!(services);

        let signal_error = tokio::select! {
            biased;
            result = signal => result.err(),
            result = &mut services => return result,
            () = shutdown.cancelled() => None,
        };
        shutdown.cancel();
        tokio::time::timeout(SHUTDOWN_TIMEOUT, services)
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "graceful shutdown timed out")
            })??;
        if let Some(error) = signal_error {
            return Err(error.into());
        }
        Ok(())
    }
}

mod app;
mod cache;
mod config;
mod error;
mod files;
mod http;
mod listeners;
mod upstream;

use std::{env, error::Error, time::Duration};

const CACHE_TIMEOUT: Duration = Duration::from_secs(2);

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let shutdown = app::shutdown_signal()?;
    let redis_url = env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let settings = config::load()?;
    let connection_config = redis::AsyncConnectionConfig::new()
        .set_connection_timeout(CACHE_TIMEOUT)
        .set_response_timeout(CACHE_TIMEOUT);
    let redis = redis::Client::open(redis_url)?
        .get_multiplexed_async_connection_with_config(&connection_config)
        .await?;
    let storage = cache::Storage::new(settings.storage).await?;
    let (cache, workers) = cache::Cache::new(
        redis,
        settings.cache,
        settings.origins,
        storage,
        settings.files,
    );
    let router = http::router(cache, settings.cors);
    let app = app::Application::new(settings.listeners, router, workers);
    app.run(shutdown).await
}

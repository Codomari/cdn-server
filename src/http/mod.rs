mod error;
mod file_response;
mod handlers;

use std::sync::Arc;

use axum::{Router, routing::get};
use tokio::sync::Semaphore;

use crate::cache::Cache;

const MAX_IN_FLIGHT: usize = 16;

#[derive(Clone)]
struct App {
    cache: Cache,
    permits: Arc<Semaphore>,
}

pub(crate) fn router(cache: Cache) -> Router {
    let app = App {
        cache,
        permits: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
    };
    Router::new()
        .route("/api/v1/fetch", get(handlers::fetch))
        .route("/serve/{*target}", get(handlers::serve))
        .route("/download/{*target}", get(handlers::download))
        .with_state(app)
}

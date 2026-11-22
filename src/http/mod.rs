mod error;
mod file_response;
mod handlers;

use std::sync::Arc;

use axum::{
    Router,
    http::{HeaderName, header},
    routing::get,
};
use tokio::sync::Semaphore;
use tower_http::cors::{Any, CorsLayer};

use crate::{cache::Cache, config::CorsConfig};

const MAX_IN_FLIGHT: usize = 16;

#[derive(Clone)]
struct App {
    cache: Cache,
    permits: Arc<Semaphore>,
}

pub(crate) fn router(cache: Cache, cors: CorsConfig) -> Router {
    let app = App {
        cache,
        permits: Arc::new(Semaphore::new(MAX_IN_FLIGHT)),
    };
    let router = Router::new()
        .route("/api/v1/fetch", get(handlers::fetch))
        .route("/serve/{*target}", get(handlers::serve))
        .route("/download/{*target}", get(handlers::download))
        .with_state(app);
    with_cors(router, cors)
}

fn with_cors(router: Router, config: CorsConfig) -> Router {
    let allow = config.allow;
    if !config.enable || !(allow.origin || allow.methods || allow.headers || allow.expose_headers) {
        return router;
    }

    // The CORS policy is static, so these headers do not vary by origin or preflight headers.
    let mut cors = CorsLayer::new().vary([]);
    if allow.origin {
        cors = cors.allow_origin(Any);
    }
    if allow.methods {
        cors = cors.allow_methods(Any);
    }
    if allow.headers {
        // Browsers require Authorization to be listed explicitly alongside the wildcard.
        cors = cors.allow_headers([HeaderName::from_static("*"), header::AUTHORIZATION]);
    }
    if allow.expose_headers {
        cors = cors.expose_headers(Any);
    }
    router.route_layer(cors)
}

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode},
        response::Response,
    };
    use tower::ServiceExt;

    use super::*;

    async fn request(config: CorsConfig, method: Method, uri: &str) -> Response {
        let mut request = Request::builder()
            .method(method.clone())
            .uri(uri)
            .header(header::ORIGIN, "https://client.example.com");
        if method == Method::OPTIONS {
            request = request
                .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                .header(
                    header::ACCESS_CONTROL_REQUEST_HEADERS,
                    "authorization, if-none-match",
                );
        }
        let router = Router::new().route("/file", get(|| async { StatusCode::NOT_MODIFIED }));
        with_cors(router, config)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn cors_defaults_apply_to_responses_and_preflights() {
        let config = CorsConfig::default();
        for method in [Method::GET, Method::HEAD] {
            let response = request(config, method, "/file").await;
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            assert_eq!(response.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN], "*");
            assert_eq!(
                response.headers()[header::ACCESS_CONTROL_EXPOSE_HEADERS],
                "*"
            );
            assert!(
                !response
                    .headers()
                    .contains_key(header::ACCESS_CONTROL_ALLOW_METHODS)
            );
            assert!(
                !response
                    .headers()
                    .contains_key(header::ACCESS_CONTROL_ALLOW_HEADERS)
            );
            assert!(!response.headers().contains_key(header::VARY));
        }

        let preflight = request(config, Method::OPTIONS, "/file").await;
        assert!(preflight.status().is_success());
        assert_eq!(
            preflight.headers()[header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "*"
        );
        assert_eq!(
            preflight.headers()[header::ACCESS_CONTROL_ALLOW_METHODS],
            "*"
        );
        assert_eq!(
            preflight.headers()[header::ACCESS_CONTROL_ALLOW_HEADERS],
            "*,authorization"
        );
        assert!(
            !preflight
                .headers()
                .contains_key(header::ACCESS_CONTROL_EXPOSE_HEADERS)
        );
        assert!(
            !preflight
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_CREDENTIALS)
        );

        let missing = request(config, Method::OPTIONS, "/missing").await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn cors_switches_omit_their_headers() {
        let switches = [
            ("origin", Method::GET, header::ACCESS_CONTROL_ALLOW_ORIGIN),
            (
                "methods",
                Method::OPTIONS,
                header::ACCESS_CONTROL_ALLOW_METHODS,
            ),
            (
                "headers",
                Method::OPTIONS,
                header::ACCESS_CONTROL_ALLOW_HEADERS,
            ),
            (
                "exposeHeaders",
                Method::GET,
                header::ACCESS_CONTROL_EXPOSE_HEADERS,
            ),
        ];
        for (key, method, header) in switches {
            let config = serde_yaml::from_str(&format!("allow: {{ {key}: false }}")).unwrap();
            let response = request(config, method, "/file").await;
            assert!(!response.headers().contains_key(header));
        }

        let disabled = serde_yaml::from_str("enable: false").unwrap();
        let response = request(disabled, Method::GET, "/file").await;
        assert!(
            !response
                .headers()
                .contains_key(header::ACCESS_CONTROL_ALLOW_ORIGIN)
        );
        assert!(
            !response
                .headers()
                .contains_key(header::ACCESS_CONTROL_EXPOSE_HEADERS)
        );
        let preflight = request(disabled, Method::OPTIONS, "/file").await;
        assert_eq!(preflight.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}

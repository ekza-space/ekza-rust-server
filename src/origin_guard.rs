//! Refuse Socket.IO handshakes from browsers on origins outside the allowlist.
//!
//! CORS headers alone do not protect WebSockets: the browser performs the
//! upgrade regardless and only the server can refuse. Requests without an
//! `Origin` header (native clients, curl) pass through — authentication, not
//! origin, is what protects writes.

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use std::sync::Arc;
use std::task::{Context, Poll};
use tower::{Layer, Service};

use crate::config::Config;

#[derive(Clone)]
pub struct OriginGuardLayer {
    config: Arc<Config>,
}

impl OriginGuardLayer {
    pub fn new(config: Arc<Config>) -> Self {
        Self { config }
    }
}

impl<S> Layer<S> for OriginGuardLayer {
    type Service = OriginGuard<S>;
    fn layer(&self, inner: S) -> Self::Service {
        OriginGuard {
            inner,
            config: self.config.clone(),
        }
    }
}

#[derive(Clone)]
pub struct OriginGuard<S> {
    inner: S,
    config: Arc<Config>,
}

impl<S> Service<Request<Body>> for OriginGuard<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = futures_util::future::Either<
        S::Future,
        std::future::Ready<Result<Self::Response, Self::Error>>,
    >;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let is_socket = req.uri().path().starts_with("/socket.io");
        let origin = req
            .headers()
            .get(axum::http::header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        if is_socket {
            if let Some(origin) = origin {
                if !self.config.origin_allowed(&origin) {
                    tracing::warn!(origin, "socket.io handshake refused: origin not allowed");
                    let resp = Response::builder()
                        .status(StatusCode::FORBIDDEN)
                        .body(Body::from("origin not allowed"))
                        .expect("static response");
                    return futures_util::future::Either::Right(std::future::ready(Ok(resp)));
                }
            }
        }
        futures_util::future::Either::Left(self.inner.call(req))
    }
}

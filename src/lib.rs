//! PAYtience quote aggregator. Market makers connect over `/ws/rfq` and `/ws/sign`;
//! the dashboard uses the REST API. `docs/protocol.md` is the contract for both.
#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

pub mod admin;
pub mod auction;
pub mod auth;
pub mod config;
pub mod conn;
pub mod exposure;
pub mod faucet;
pub mod http;
pub mod market;
pub mod mmreg;
pub mod preauth;
pub mod protocol;
pub mod ratelimit;
pub mod registry;
pub mod sign;
pub mod solana_rpc;
pub mod state;
pub mod stats;
pub mod tx;
pub mod ws;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

pub use config::{Config, RateLimits};
pub use http::build_router;
pub use state::App;

const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_millis(200);

/// Serves the API and MM sockets on `listener` until `shutdown` resolves, then closes
/// every MM connection (1001), stops the usage scan and drains in-flight HTTP requests.
pub async fn serve(
    listener: tokio::net::TcpListener,
    app: Arc<App>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let router = build_router(app.clone());
    // §5.6: the usage scan runs beside the server and stops with it.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let mut scan = app
        .config
        .exposure_scan
        .then(|| tokio::spawn(exposure::run(app.clone(), stop_rx)));
    let served = axum::serve(
        listener,
        router.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown.await;
        app.registry.close_all(1001, "server shutting down");
    })
    .await;
    stop_tx.send_replace(true);
    if let Some(task) = scan.as_mut()
        && tokio::time::timeout(SHUTDOWN_GRACE, &mut *task)
            .await
            .is_err()
    {
        task.abort();
    }
    served?;
    // Upgraded sockets are not tracked by the graceful shutdown; give their writers a
    // moment to flush the 1001 close frames before the runtime goes away.
    tokio::time::sleep(SHUTDOWN_GRACE).await;
    Ok(())
}

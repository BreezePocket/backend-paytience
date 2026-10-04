#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]

use anyhow::Context;
use paytience_backend::solana_rpc::host_of;
use paytience_backend::{App, Config, serve};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // A missing .env is fine (the environment may be set another way), but a malformed
    // one is not: dotenvy stops at the first bad line, and every variable after it would
    // silently fall back to its default or be reported as missing.
    match dotenvy::dotenv() {
        Ok(_) => {}
        Err(e) if e.not_found() => {}
        Err(e) => return Err(e).context("cannot parse .env (quote values that contain spaces)"),
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env().context("invalid configuration")?;
    let addr = config.bind_addr.clone();
    tracing::info!(
        program_id = %config.program_id,
        usdc_mint = %config.usdc_mint,
        // Host only: the path or query of an RPC URL may hold an API key.
        rpc_host = host_of(&config.solana_rpc),
        scan_rpc_host = host_of(config.scan_rpc_url()),
        mm_registry = ?config.mm_registry_path,
        broadcast = config.broadcast,
        "starting paytience-backend"
    );
    let admin = config.admin_token.is_some();
    let app = App::new(config)?;
    tracing::info!(
        registered = app.mms.list().len(),
        approved = app.mms.approved_count(),
        "market-maker registry loaded"
    );
    if !admin {
        tracing::warn!(
            "ADMIN_TOKEN is unset: nobody can be approved, so no market maker can connect"
        );
    }
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("listening on {addr}");
    serve(listener, app, shutdown_signal()).await?;
    tracing::info!("stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(e) = tokio::signal::ctrl_c().await {
            tracing::error!("cannot listen for SIGINT: {e}");
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::error!("cannot listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    tracing::info!("shutting down");
}

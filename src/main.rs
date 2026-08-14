#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Context, Result};
use openmouse_bridge::{BRIDGE_PORT, api, config, service::BridgeService};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("OpenMouse Bridge failed: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "openmouse_bridge=info,tower_http=info".into()),
        )
        .init();
    let (config, path) = config::load_or_create()?;
    let origins = config.allowed_origins.clone();
    let service = BridgeService::new(config, path.clone());
    service.start_game_monitor();
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), BRIDGE_PORT);
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("could not bind http://{address}; is Bridge already running?"))?;
    tracing::info!(%address, config = %path.display(), "OpenMouse Bridge is ready");
    axum::serve(listener, api::router(service, &origins))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

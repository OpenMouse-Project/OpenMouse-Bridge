#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod desktop;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, atomic::AtomicBool},
};

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use anyhow::{Context, Result};
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use openmouse_bridge::{BRIDGE_PORT, api, config, service::BridgeService};
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn main() {
    init_tracing();
    if let Err(error) = desktop::run() {
        tracing::error!(%error, "OpenMouse Bridge failed");
        std::process::exit(1);
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
#[tokio::main]
async fn main() {
    init_tracing();
    if let Err(error) = run().await {
        eprintln!("OpenMouse Bridge failed: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
async fn run() -> Result<()> {
    let (config, path) = config::load_or_create()?;
    let origins = config.allowed_origins.clone();
    let service = BridgeService::new(config, path.clone());
    // Headless mode has no status window, but the web app still fetches
    // application icons over the API, so always keep them extracted.
    service.start_game_monitor(Arc::new(AtomicBool::new(true)));
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

fn init_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "openmouse_bridge=info,tower_http=info".into()),
        )
        .init();
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

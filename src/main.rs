#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod desktop;

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use anyhow::{Context, Result};
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use openmouse_bridge::{BRIDGE_PORT, api, config, devices::DeviceManager, service::BridgeService};
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use tokio::net::TcpListener;

use openmouse_bridge::logging;

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn main() {
    // Held for the whole program so the file logger keeps flushing.
    let _log = logging::init();
    if let Err(error) = desktop::run() {
        tracing::error!(%error, "OpenMouse Bridge failed");
        std::process::exit(1);
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
#[tokio::main]
async fn main() {
    let _log = logging::init();
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
    service.start_game_monitor();
    let devices = DeviceManager::start(service.clone());
    let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), BRIDGE_PORT);
    let listener = TcpListener::bind(address)
        .await
        .with_context(|| format!("could not bind http://{address}; is Bridge already running?"))?;
    tracing::info!(%address, config = %path.display(), "OpenMouse Bridge is ready");
    axum::serve(listener, api::router(service, devices, &origins))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

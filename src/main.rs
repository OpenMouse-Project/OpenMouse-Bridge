#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod desktop;
#[cfg(any(target_os = "windows", target_os = "macos"))]
mod overlay;

use std::fs;

use anyhow::Context;
use openmouse_bridge::config;
use tracing_appender::{
    non_blocking::WorkerGuard,
    rolling::{RollingFileAppender, Rotation},
};
use tracing_subscriber::{EnvFilter, fmt::writer::MakeWriterExt};

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use anyhow::Result;
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{Arc, atomic::AtomicBool},
};

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use openmouse_bridge::{BRIDGE_PORT, api, service::BridgeService};
#[cfg(not(any(target_os = "windows", target_os = "macos")))]
use tokio::net::TcpListener;

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn main() {
    let _log_guard = init_tracing();
    if let Err(error) = desktop::run() {
        tracing::error!(%error, "OpenMouse Bridge failed");
        std::process::exit(1);
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
#[tokio::main]
async fn main() {
    let _log_guard = init_tracing();
    if let Err(error) = run().await {
        eprintln!("OpenMouse Bridge failed: {error:#}");
        std::process::exit(1);
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
async fn run() -> Result<()> {
    let (config, path) = config::load_with_catalog().await?;
    let origins = config.allowed_origins.clone();
    let service = BridgeService::new(config, path.clone());
    // Headless mode has no status window, but the web app still fetches
    // application icons over the API, so always keep them extracted.
    service.start_game_monitor(Arc::new(AtomicBool::new(true)));
    service.start_battery_monitor();
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

fn init_tracing() -> Option<WorkerGuard> {
    let subscriber = tracing_subscriber::fmt().with_env_filter(
        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "openmouse_bridge=info,tower_http=info".into()),
    );
    let appender = config::log_dir().and_then(|directory| {
        fs::create_dir_all(&directory).with_context(|| {
            format!(
                "could not create Bridge log directory {}",
                directory.display()
            )
        })?;
        let appender = RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .filename_prefix("openmouse-bridge")
            .filename_suffix("log")
            .max_log_files(7)
            .build(&directory)
            .context("could not open the Bridge log file")?;
        Ok((directory, appender))
    });

    match appender {
        Ok((directory, appender)) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            subscriber.with_writer(std::io::stdout.and(writer)).init();
            tracing::info!(
                version = env!("CARGO_PKG_VERSION"),
                log_directory = %directory.display(),
                "Bridge file logging initialized"
            );
            Some(guard)
        }
        Err(error) => {
            subscriber.init();
            tracing::warn!(%error, "Bridge file logging is unavailable");
            None
        }
    }
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

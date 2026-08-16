pub mod api;
pub mod applications;
pub mod config;
pub mod devices;
pub mod games;
pub mod logging;
pub mod platform;
pub mod service;

pub const BRIDGE_PORT: u16 = 17_846;
pub const BRIDGE_VERSION: &str = env!("CARGO_PKG_VERSION");

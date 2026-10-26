pub mod api;
pub mod applications;
pub mod config;
pub mod drivers;
pub mod games;
mod hid;
pub mod platform;
pub mod service;
pub mod updater;

pub const BRIDGE_PORT: u16 = 17_846;
pub const BRIDGE_VERSION: &str = env!("CARGO_PKG_VERSION");

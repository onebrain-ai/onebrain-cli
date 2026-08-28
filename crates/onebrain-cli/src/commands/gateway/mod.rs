//! `onebrain gateway` — module shell. The CLI verb and MCP server land in
//! later tasks; this task ships only the machine-level config loader.
// Re-exports below are unused until the run loop (Task 4) consumes them.
#![allow(unused_imports)]

pub mod config;
pub mod server;

pub use config::{gateway_config_path, load_gateway_config, GatewayConfig, DEFAULT_GATEWAY_PORT};

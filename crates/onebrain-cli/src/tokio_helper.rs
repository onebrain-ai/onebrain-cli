//! On-demand tokio runtime · used only by future async commands (v3.1+ serve, gateways).
//!
//! v3.0 commands are sync. This module exists so adding `Cmd::Serve` in v3.1 does NOT
//! require restructuring main.rs.

#[allow(dead_code)] // wired in v3.1
pub fn run_async<F>(future: F) -> F::Output
where
    F: std::future::Future,
{
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to build tokio runtime")
        .block_on(future)
}

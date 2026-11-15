//! Runs the server with the mock audio backend so the phone apps can be
//! pointed at any machine.
//!
//! ```text
//! cargo run -p pcvc-server --example mock_server -- [--port N] [--no-mdns]
//! ```
//!
//! Logging follows `RUST_LOG` (default `info`).

mod common;

use std::process::ExitCode;
use std::sync::Arc;

use pcvc_audio::MockBackend;

#[tokio::main]
async fn main() -> ExitCode {
    let config = match common::init("mock_server") {
        Ok(config) => config,
        Err(code) => return code,
    };
    common::serve(Arc::new(MockBackend::demo()), config, "mock server").await
}

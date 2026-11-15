//! Runs the server with the mock audio backend so the phone apps can be
//! pointed at any machine.
//!
//! ```text
//! cargo run -p pcvc-server --example mock_server -- [--port N] [--no-mdns]
//! ```
//!
//! Logging follows `RUST_LOG` (default `info`).

use std::net::{IpAddr, Ipv4Addr};
use std::process::ExitCode;
use std::sync::Arc;

use pcvc_audio::{AudioBackend, MockBackend};
use pcvc_server::{DEFAULT_PORT, MdnsStatus, ServerConfig, ServerEvent, ServerHandle};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let mut config = ServerConfig::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_PORT, true);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => match args.next().and_then(|p| p.parse().ok()) {
                Some(port) => config.port = port,
                None => return usage(),
            },
            "--no-mdns" => config.mdns = false,
            _ => return usage(),
        }
    }

    let backend: Arc<dyn AudioBackend> = Arc::new(MockBackend::demo());
    let server = match ServerHandle::start(backend, config).await {
        Ok(server) => server,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let mut mdns = server.mdns();
    tokio::spawn(async move {
        while mdns.changed().await.is_ok() {
            match &*mdns.borrow_and_update() {
                MdnsStatus::Registered { instance } => tracing::info!(%instance, "advertising"),
                MdnsStatus::Failed(e) => tracing::warn!("mDNS: {e}"),
                MdnsStatus::Disabled => {}
            }
        }
    });
    let mut events = server.events();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            if let ServerEvent::BackendError(e) = event {
                tracing::warn!("{e}");
            }
        }
    });

    tracing::info!(addr = %server.local_addr(), "mock server running, Ctrl-C to stop");
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("waiting for Ctrl-C failed: {e}");
    }
    server.stop().await;
    ExitCode::SUCCESS
}

fn usage() -> ExitCode {
    eprintln!("usage: mock_server [--port N] [--no-mdns]");
    ExitCode::from(2)
}

//! Command line, logging and run loop shared by the server examples.

use std::net::{IpAddr, Ipv4Addr};
use std::process::ExitCode;
use std::sync::Arc;

use pcvc_audio::AudioBackend;
use pcvc_server::{DEFAULT_PORT, MdnsStatus, ServerConfig, ServerEvent, ServerHandle};
use tokio::sync::broadcast::error::RecvError;
use tracing_subscriber::EnvFilter;

/// Sets up logging (`RUST_LOG`, default `info`) and parses `[--port N]
/// [--no-mdns]` plus the example's own `flags`, returning those that were
/// given. On a usage error, returns the exit code to end with.
pub fn init(
    example: &str,
    flags: &[&'static str],
) -> Result<(ServerConfig, Vec<&'static str>), ExitCode> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let mut config = ServerConfig::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_PORT);
    let mut given = Vec::new();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--port" => match args.next().and_then(|p| p.parse().ok()) {
                Some(port) => config.port = port,
                None => return Err(usage(example, flags)),
            },
            "--no-mdns" => config.mdns = false,
            other => match flags.iter().find(|f| **f == other) {
                Some(flag) => given.push(*flag),
                None => return Err(usage(example, flags)),
            },
        }
    }
    Ok((config, given))
}

fn usage(example: &str, flags: &[&str]) -> ExitCode {
    let extra: String = flags.iter().map(|f| format!(" [{f}]")).collect();
    eprintln!("usage: {example} [--port N] [--no-mdns]{extra}");
    ExitCode::from(2)
}

/// Runs the server on `backend` until Ctrl-C, logging mDNS status and
/// backend errors.
pub async fn serve(backend: Arc<dyn AudioBackend>, config: ServerConfig, what: &str) -> ExitCode {
    let server = match ServerHandle::start(backend, config).await {
        Ok(server) => server,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };

    let mut mdns = server.mdns();
    tokio::spawn(async move {
        loop {
            match &*mdns.borrow_and_update() {
                MdnsStatus::Registered { instance } => tracing::info!(%instance, "advertising"),
                MdnsStatus::Failed(e) => tracing::warn!("mDNS: {e}"),
                MdnsStatus::Disabled | MdnsStatus::Starting => {}
            }
            if mdns.changed().await.is_err() {
                break;
            }
        }
    });
    let mut events = server.events();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ServerEvent::BackendError(e)) => tracing::warn!("{e}"),
                Ok(_) => {}
                Err(RecvError::Lagged(n)) => tracing::warn!("missed {n} server events"),
                Err(RecvError::Closed) => break,
            }
        }
    });

    tracing::info!(addr = %server.local_addr(), "{what} running, Ctrl-C to stop");
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("waiting for Ctrl-C failed: {e}");
    }
    server.stop().await;
    ExitCode::SUCCESS
}

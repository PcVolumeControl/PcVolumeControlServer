//! Runs the server with the PulseAudio backend, so the phone apps control
//! this Linux machine's real audio (PulseAudio, or PipeWire through
//! `pipewire-pulse`).
//!
//! ```text
//! cargo run -p pcvc-server --example pulse_server -- [--port N] [--no-mdns]
//! ```
//!
//! Logging follows `RUST_LOG` (default `info`).

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    use std::net::{IpAddr, Ipv4Addr};
    use std::process::ExitCode;
    use std::sync::Arc;

    use pcvc_audio::{AudioBackend, PulseBackend};
    use pcvc_server::{DEFAULT_PORT, MdnsStatus, ServerConfig, ServerEvent, ServerHandle};
    use tracing_subscriber::EnvFilter;

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let mut config = ServerConfig::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), DEFAULT_PORT);
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

    // The constructor blocks briefly while it connects.
    let backend = match tokio::task::spawn_blocking(PulseBackend::new).await {
        Ok(Ok(backend)) => backend,
        Ok(Err(e)) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
        Err(e) => {
            eprintln!("starting the PulseAudio backend panicked: {e}");
            return ExitCode::FAILURE;
        }
    };
    let backend: Arc<dyn AudioBackend> = Arc::new(backend);
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
        while let Ok(event) = events.recv().await {
            if let ServerEvent::BackendError(e) = event {
                tracing::warn!("{e}");
            }
        }
    });

    tracing::info!(addr = %server.local_addr(), "PulseAudio server running, Ctrl-C to stop");
    if let Err(e) = tokio::signal::ctrl_c().await {
        tracing::error!("waiting for Ctrl-C failed: {e}");
    }
    server.stop().await;
    ExitCode::SUCCESS
}

#[cfg(target_os = "linux")]
fn usage() -> std::process::ExitCode {
    eprintln!("usage: pulse_server [--port N] [--no-mdns]");
    std::process::ExitCode::from(2)
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pulse_server is only available on Linux");
}

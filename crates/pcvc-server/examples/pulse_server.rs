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
mod common;

#[cfg(target_os = "linux")]
#[tokio::main]
async fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    use std::sync::Arc;

    use pcvc_audio::PulseBackend;

    let config = match common::init("pulse_server") {
        Ok(config) => config,
        Err(code) => return code,
    };
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
    common::serve(Arc::new(backend), config, "PulseAudio server").await
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pulse_server is only available on Linux");
}

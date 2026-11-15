//! Prints the Windows audio state, then every change event with the state it
//! leads to.
//!
//! ```text
//! cargo run -p pcvc-audio --example windows_dump -- [--seconds N]
//! ```
//!
//! With `--seconds N` it stops after N seconds and drops the backend, which
//! exercises callback unregistration; otherwise it runs until Ctrl-C.

#[cfg(target_os = "windows")]
fn main() -> std::process::ExitCode {
    use std::process::ExitCode;
    use std::time::{Duration, Instant};

    use pcvc_audio::{AudioBackend, AudioState, WindowsBackend};
    use tokio::sync::broadcast::error::TryRecvError;

    fn print_state(state: &AudioState) {
        println!("devices:");
        for d in &state.devices {
            println!("  {}  {}", d.id, d.name);
        }
        match &state.default_device {
            None => println!("default device: none"),
            Some(d) => {
                println!(
                    "default device: {} ({})  volume {:.2}{}",
                    d.name,
                    d.id,
                    d.volume,
                    if d.muted { "  muted" } else { "" }
                );
                for s in &d.sessions {
                    println!(
                        "  {:<24} {:>6.2}{}  {}",
                        s.name,
                        s.volume,
                        if s.muted { " muted" } else { "      " },
                        s.id
                    );
                }
            }
        }
        println!();
    }

    let mut args = std::env::args().skip(1);
    let deadline = match (args.next().as_deref(), args.next()) {
        (None, _) => None,
        (Some("--seconds"), Some(n)) => match n.parse() {
            Ok(n) => Some(Instant::now() + Duration::from_secs(n)),
            Err(_) => return usage(),
        },
        _ => return usage(),
    };

    let backend = match WindowsBackend::new() {
        Ok(backend) => backend,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    let mut events = backend.subscribe();
    match backend.snapshot() {
        Ok(state) => print_state(&state),
        Err(e) => eprintln!("snapshot failed: {e}"),
    }

    while deadline.is_none_or(|d| Instant::now() < d) {
        match events.try_recv() {
            Ok(event) => {
                println!("event: {event:?}");
                // Print the state once a burst of events has been drained.
                if events.is_empty() {
                    match backend.snapshot() {
                        Ok(state) => print_state(&state),
                        Err(e) => eprintln!("snapshot failed: {e}"),
                    }
                }
            }
            Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(20)),
            Err(TryRecvError::Lagged(n)) => println!("lagged by {n} events"),
            Err(TryRecvError::Closed) => break,
        }
    }
    drop(backend);
    println!("backend dropped");
    ExitCode::SUCCESS
}

#[cfg(target_os = "windows")]
fn usage() -> std::process::ExitCode {
    eprintln!("usage: windows_dump [--seconds N]");
    std::process::ExitCode::from(2)
}

#[cfg(not(target_os = "windows"))]
fn main() {
    eprintln!("windows_dump only runs on Windows");
}

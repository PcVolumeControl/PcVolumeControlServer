//! Prints what the PulseAudio backend sees as JSON, then the events it emits.
//!
//! ```text
//! cargo run -p pcvc-audio --example pulse_dump -- [SECONDS]
//! ```
//!
//! Watches for events for `SECONDS` (default 10), printing the state again
//! whenever a burst of events changed it.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::time::{Duration, Instant};

    use pcvc_audio::{AudioBackend, AudioState, PulseBackend, Session};
    use serde_json::json;
    use tokio::sync::broadcast::error::TryRecvError;

    fn print_state(state: &AudioState) {
        let session = |s: &Session| {
            json!({
                "id": s.id,
                "name": s.name,
                "volume": s.volume,
                "muted": s.muted,
            })
        };
        let value = json!({
            "devices": state
                .devices
                .iter()
                .map(|d| json!({ "id": d.id, "name": d.name }))
                .collect::<Vec<_>>(),
            "defaultDevice": state.default_device.as_ref().map(|d| json!({
                "id": d.id,
                "name": d.name,
                "volume": d.volume,
                "muted": d.muted,
                "sessions": d.sessions.iter().map(session).collect::<Vec<_>>(),
            })),
        });
        match serde_json::to_string_pretty(&value) {
            Ok(text) => println!("{text}"),
            Err(e) => eprintln!("cannot encode the state: {e}"),
        }
    }

    let seconds = match std::env::args().nth(1).map(|s| s.parse::<u64>()) {
        None => 10,
        Some(Ok(s)) => s,
        Some(Err(_)) => {
            eprintln!("usage: pulse_dump [SECONDS]");
            return std::process::ExitCode::from(2);
        }
    };
    let backend = match PulseBackend::new() {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let mut events = backend.subscribe();
    let state = backend.snapshot().expect("snapshot is infallible");
    print_state(&state);

    println!("watching events for {seconds} s");
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut last = state;
    while Instant::now() < deadline {
        let mut any = false;
        loop {
            match events.try_recv() {
                Ok(event) => {
                    println!("event: {event:?}");
                    any = true;
                }
                Err(TryRecvError::Lagged(n)) => {
                    println!("lagged by {n} events");
                    any = true;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Closed) => return std::process::ExitCode::FAILURE,
            }
        }
        if any {
            let state = backend.snapshot().expect("snapshot is infallible");
            if state != last {
                print_state(&state);
                last = state;
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    std::process::ExitCode::SUCCESS
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("pulse_dump is only available on Linux");
}

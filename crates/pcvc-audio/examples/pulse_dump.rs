//! Prints what the PulseAudio backend sees, then the events it emits.
//!
//! ```text
//! cargo run -p pcvc-audio --example pulse_dump -- [SECONDS]
//! ```
//!
//! Watches for events for `SECONDS` (default 10), printing the default
//! device again after each burst.

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::time::{Duration, Instant};

    use pcvc_audio::{AudioBackend, AudioState, PulseBackend};
    use tokio::sync::broadcast::error::TryRecvError;

    fn print_state(state: &AudioState) {
        println!("{{");
        println!("  \"devices\": [");
        for d in &state.devices {
            println!("    {{ \"id\": {:?}, \"name\": {:?} }},", d.id, d.name);
        }
        println!("  ],");
        match &state.default_device {
            None => println!("  \"defaultDevice\": null"),
            Some(d) => {
                println!("  \"defaultDevice\": {{");
                println!("    \"id\": {:?},", d.id);
                println!("    \"name\": {:?},", d.name);
                println!("    \"volume\": {:.2},", d.volume);
                println!("    \"muted\": {},", d.muted);
                println!("    \"sessions\": [");
                for s in &d.sessions {
                    println!(
                        "      {{ \"id\": {:?}, \"name\": {:?}, \"volume\": {:.2}, \"muted\": {} }},",
                        s.id, s.name, s.volume, s.muted
                    );
                }
                println!("    ]");
                println!("  }}");
            }
        }
        println!("}}");
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

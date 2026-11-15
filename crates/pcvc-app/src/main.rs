//! PcVolumeControl desktop host. See `README.md` in this crate for how the
//! pieces talk to each other.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod autostart;
mod cli;
mod config;
mod icons;
mod inbox;
mod logging;
mod mixer;
mod model;
mod paths;
mod server_ctl;
mod style;
mod tray;

use std::process::ExitCode;
use std::sync::Mutex;

use app::{App, Boot};
use audio::BackendChoice;
use cli::Parsed;
use config::Config;

fn main() -> ExitCode {
    let parsed = cli::parse(std::env::args().skip(1));
    if !matches!(parsed, Ok(Parsed::Run(_))) {
        attach_parent_console();
    }
    let args = match parsed {
        Ok(Parsed::Run(args)) => args,
        Ok(Parsed::Help) => {
            println!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Ok(Parsed::Version) => {
            println!("pcvolumecontrol {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("{e}\n\n{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };

    let (logs, _log_guard) = logging::init(paths::log_dir().as_deref());

    let mut config_path = paths::config_file();
    let config = match &config_path {
        Some(path) => {
            let loaded = config::load(path);
            if let Some(warning) = loaded.warning {
                tracing::warn!("{warning}");
            }
            if !loaded.writable {
                config_path = None;
            }
            loaded.config
        }
        None => {
            tracing::warn!("no home directory; settings will not be saved");
            Config::default()
        }
    };
    let backend = audio::create_backend(if args.mock_audio {
        BackendChoice::Mock
    } else {
        BackendChoice::System
    });
    let (inbox, inbox_rx) = inbox::channel();
    tray::install_event_handlers(&inbox);

    let boot = Mutex::new(Some(Boot {
        show_window: args.show || config.show_on_launch,
        config,
        config_path,
        backend,
        inbox,
        inbox_rx,
        logs,
    }));
    let result = iced::daemon(
        move || {
            let boot = boot
                .lock()
                .ok()
                .and_then(|mut b| b.take())
                .expect("iced boots the daemon once");
            App::boot(boot)
        },
        App::update,
        App::view,
    )
    .title(App::title)
    .theme(App::theme)
    .subscription(App::subscription)
    .run();

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            tracing::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// Release builds on Windows use the GUI subsystem and start without a
/// console; attach to the launching one so `--help` and usage errors show.
#[cfg(all(target_os = "windows", not(debug_assertions)))]
fn attach_parent_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // SAFETY: AttachConsole has no preconditions; failure (no parent console)
    // only leaves output going nowhere.
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(all(target_os = "windows", not(debug_assertions))))]
fn attach_parent_console() {}

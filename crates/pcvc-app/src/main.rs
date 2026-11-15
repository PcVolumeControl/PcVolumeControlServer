//! PcVolumeControl desktop host. See `README.md` in this crate for how the
//! pieces talk to each other.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod audio;
mod cli;
mod config;
mod icons;
mod inbox;
mod model;
mod server_ctl;
mod tray;

use std::process::ExitCode;
use std::sync::Mutex;

use app::{App, Boot};
use audio::BackendChoice;
use cli::Parsed;
use config::Config;
use tracing_subscriber::EnvFilter;

const DEFAULT_LOG_FILTER: &str =
    "info,iced_winit=warn,wgpu_core=error,wgpu_hal=error,naga=warn,sctk_adwaita=error";

fn main() -> ExitCode {
    let args = match cli::parse(std::env::args().skip(1)) {
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

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| DEFAULT_LOG_FILTER.into()),
        )
        .init();

    let config = Config::default();
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
        backend,
        inbox,
        inbox_rx,
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

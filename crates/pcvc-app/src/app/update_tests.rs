//! Headless tests of the update banner, the Status page's Updates section,
//! the tray item and the Settings wording. No worker runs: results are fed
//! in as the worker would send them.

use iced_test::simulator;

use super::tests::Harness;
use super::*;
use crate::update::{self, Event, Failure, HttpError, InstallKind, Phase, Progress};

fn harness(kind: InstallKind, official: bool) -> Harness {
    let mut h = Harness::new();
    h.app.updates.kind = kind;
    h.app.updates.official = official;
    h.app.updates.signed = false;
    h
}

fn event(h: &mut Harness, event: Event) {
    h.run(vec![Message::Inbound(Inbound::Update(event))]);
}

fn clicked(h: &Harness, label: &str) -> Vec<Message> {
    let mut ui = simulator(h.app.status_view());
    ui.click(label).unwrap();
    ui.into_messages().collect()
}

#[test]
fn updates_section_checks_by_hand() {
    let mut h = harness(InstallKind::LinuxTarball, false);
    let mut ui = simulator(h.app.status_view());
    for needle in [
        &format!("Version {} (development build)", update::current_version())[..],
        "Not checked yet",
        "Development builds check only when asked.",
    ] {
        ui.find(needle).unwrap();
    }
    assert!(ui.find("Skip this version").is_err(), "no banner");
    drop(ui);

    let messages = clicked(&h, "Check now");
    assert!(matches!(
        messages.as_slice(),
        [Message::Update(UpdateMessage::CheckNow)]
    ));
    h.run(messages);
    simulator(h.app.status_view())
        .find("Checking\u{2026}")
        .unwrap();
    assert!(
        clicked(&h, "Check now").is_empty(),
        "disabled while checking"
    );

    event(&mut h, update::checked(Ok(())));
    let when = h.app.updates.updater.last_checked().unwrap();
    let when = chrono::DateTime::<chrono::Local>::from(when).format("%H:%M");
    simulator(h.app.status_view())
        .find(format!(
            "No releases published yet. Last checked today at {when}"
        ))
        .unwrap();

    h.run(vec![Message::Update(UpdateMessage::CheckNow)]);
    event(&mut h, update::checked(Err(HttpError::RateLimited)));
    simulator(h.app.status_view())
        .find("Could not check: GitHub's rate limit was reached")
        .unwrap();
}

#[test]
fn official_builds_say_when_automatic_checks_are_off() {
    let mut h = harness(InstallKind::LinuxDeb, true);
    let version = format!("Version {}", update::current_version());
    let mut ui = simulator(h.app.status_view());
    ui.find(&version[..]).unwrap();
    ui.find("Automatic checks are off (Settings).").unwrap();
    drop(ui);
    h.app.config.check_for_updates = true;
    assert!(
        simulator(h.app.status_view())
            .find("Automatic checks are off (Settings).")
            .is_err()
    );
}

#[test]
fn banner_offers_the_release_page_and_skipping() {
    let mut h = harness(InstallKind::LinuxDeb, true);
    h.run(vec![Message::Update(UpdateMessage::CheckNow)]);
    event(&mut h, update::found("2.0.0"));
    let mut ui = simulator(h.app.status_view());
    ui.find("Version 2.0.0 is available").unwrap();
    ui.find("Opens the release page to download it.").unwrap();
    assert!(ui.find("Install and restart").is_err());
    drop(ui);
    assert!(matches!(
        clicked(&h, "View release").as_slice(),
        [Message::Update(UpdateMessage::ViewRelease)]
    ));
    assert_eq!(h.app.compute_tray_model().update.as_deref(), Some("2.0.0"));

    let skip = clicked(&h, "Skip this version");
    assert!(matches!(
        skip.as_slice(),
        [Message::Update(UpdateMessage::Skip)]
    ));
    h.run(skip);
    assert!(
        simulator(h.app.status_view())
            .find("Version 2.0.0 is available")
            .is_err()
    );
    assert_eq!(h.app.compute_tray_model().update, None);
    assert_eq!(
        h.app.updates.updater.saved.skipped_version.as_deref(),
        Some("2.0.0")
    );
}

#[test]
fn portable_and_development_builds_do_not_install() {
    for (kind, official) in [
        (InstallKind::WindowsPortable, true),
        (InstallKind::WindowsInstaller { all_users: false }, false),
    ] {
        let mut h = harness(kind, official);
        h.run(vec![Message::Update(UpdateMessage::CheckNow)]);
        event(&mut h, update::found("2.0.0"));
        let mut ui = simulator(h.app.status_view());
        ui.find("View release").unwrap();
        assert!(ui.find("Install and restart").is_err());
        drop(ui);
        h.run(vec![Message::Update(UpdateMessage::Install)]);
        assert_eq!(h.app.updates.updater.phase(), &Phase::Available);
    }
}

#[test]
fn installing_shows_progress_and_runs_once() {
    let mut h = harness(InstallKind::WindowsInstaller { all_users: false }, true);
    h.open_window();
    h.app.page = Page::Status;
    h.run(vec![Message::Update(UpdateMessage::CheckNow)]);
    event(&mut h, update::found("2.0.0"));
    simulator(h.app.status_view())
        .find("Downloads and verifies the installer, then restarts the app.")
        .unwrap();

    // Without a worker the install fails at once, and can be tried again.
    let install = clicked(&h, "Install and restart");
    assert!(matches!(
        install.as_slice(),
        [Message::Update(UpdateMessage::Install)]
    ));
    h.run(install);
    simulator(h.app.status_view())
        .find("Could not install: the update worker is not running")
        .unwrap();

    assert!(h.app.updates.updater.start_install().is_some());
    event(
        &mut h,
        Event::Progress(Progress::Downloading {
            done: 45,
            total: 100,
        }),
    );
    simulator(h.app.status_view())
        .find("Downloading\u{2026} 45%")
        .unwrap();
    for label in ["Install and restart", "Skip this version", "Check now"] {
        assert!(clicked(&h, label).is_empty(), "{label} is disabled");
    }
    event(&mut h, Event::Progress(Progress::Verifying));
    simulator(h.app.status_view())
        .find("Verifying the download\u{2026}")
        .unwrap();

    // Verified: the app steps aside for the installer but keeps running,
    // and the installer starts once the server has stopped.
    let running = ServerStatus::Running("0.0.0.0:3087".parse().unwrap());
    h.run(vec![Message::Inbound(Inbound::Server(
        ServerUpdate::Status(running),
    ))]);
    event(&mut h, Event::Verified);
    let waiting = |h: &Harness| h.app.updates.handed_over.map(|o| o.waiting_for_server);
    assert_eq!(waiting(&h), Some(true));
    assert!(h.app.window.is_none(), "the window closes");
    assert!(!h.app.quitting);
    h.run(vec![Message::Inbound(Inbound::Server(
        ServerUpdate::Status(ServerStatus::Stopped),
    ))]);
    assert_eq!(waiting(&h), Some(false));
    h.run(vec![Message::Inbound(Inbound::ShowRequested)]);
    assert!(h.app.window.is_none(), "stays out of the installer's way");

    event(&mut h, Event::Launched);
    assert_eq!(h.app.updates.updater.phase(), &Phase::Launching);
    simulator(h.app.status_view())
        .find("Checksum verified. The installer is running and restarts the app when it is done.")
        .unwrap();

    // An installer that ends without replacing the app gives it back.
    event(
        &mut h,
        Event::InstallFailed("the installer ended without updating (exit code 2)".into()),
    );
    assert!(h.app.updates.handed_over.is_none());
    assert!(h.app.window.is_some(), "the window reopens");
    assert_eq!(h.app.page, Page::Status);
    assert!(matches!(
        h.app.updates.updater.phase(),
        Phase::Failed(Failure::Install(_))
    ));
    simulator(h.app.status_view())
        .find("Could not install: the installer ended without updating (exit code 2)")
        .unwrap();
}

#[test]
fn a_stopped_server_stays_stopped_after_a_failed_install() {
    let mut h = harness(InstallKind::WindowsInstaller { all_users: true }, true);
    h.run(vec![
        Message::Update(UpdateMessage::CheckNow),
        Message::Inbound(Inbound::Server(ServerUpdate::Status(ServerStatus::Stopped))),
    ]);
    event(&mut h, update::found("2.0.0"));
    assert!(h.app.updates.updater.start_install().is_some());
    event(&mut h, Event::Verified);
    let hand_over = h.app.updates.handed_over.unwrap();
    assert!(!hand_over.server_was_running);
    assert!(!hand_over.waiting_for_server, "nothing to wait for");
    event(&mut h, Event::InstallFailed("declined".into()));
    assert!(h.app.updates.handed_over.is_none());
    assert_eq!(h.app.server_status, ServerStatus::Stopped);
}

#[test]
fn quitting_abandons_a_pending_install() {
    let mut h = harness(InstallKind::WindowsInstaller { all_users: false }, true);
    h.run(vec![Message::Update(UpdateMessage::CheckNow)]);
    event(&mut h, update::found("2.0.0"));
    assert!(h.app.updates.updater.start_install().is_some());
    event(&mut h, Event::Verified);
    assert_eq!(
        h.app.updates.handed_over.map(|o| o.waiting_for_server),
        Some(true)
    );
    // What `quit` calls; quitting for real would mark this whole test
    // process as shutting down for the single-instance tests.
    h.app.update_quitting();
    assert_eq!(
        h.app.updates.handed_over.map(|o| o.waiting_for_server),
        Some(false)
    );
}

#[test]
fn tray_item_opens_the_status_page() {
    let mut h = harness(InstallKind::LinuxDeb, true);
    h.run(vec![Message::Inbound(Inbound::Tray(
        TrayAction::ShowUpdate,
    ))]);
    assert!(h.app.window.is_some());
    assert_eq!(h.app.page, Page::Status);
}

#[test]
fn settings_explain_the_update_toggle() {
    let h = harness(InstallKind::LinuxDeb, false);
    let mut ui = simulator(h.app.settings_view());
    ui.find("Check for updates automatically").unwrap();
    ui.find("Contacts GitHub once a day to look for a new release (not in development builds).")
        .unwrap();
    let h = harness(InstallKind::LinuxDeb, true);
    simulator(h.app.settings_view())
        .find("Contacts GitHub once a day to look for a new release.")
        .unwrap();
}

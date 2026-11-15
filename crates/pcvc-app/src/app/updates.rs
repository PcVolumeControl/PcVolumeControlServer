//! Update checks and installs as the app drives them: the schedule, the
//! user's actions, the worker's results, and the Status page's banner and
//! section.

use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use chrono::{DateTime, Local};
use iced::widget::{button, column, container, progress_bar, row, text};
use iced::{Center, Element, Fill, Subscription, Task};

use super::{App, Message};
use crate::mini::{CloseReason, Event as MiniEvent};
use crate::server_ctl::ServerStatus;
use crate::style::{self, Button};
use crate::update::{
    self, Action, Event, Failure, InstallKind, Phase, Saved, Updater, Worker, schedule,
};

#[derive(Debug, Clone)]
pub enum UpdateMessage {
    /// Checks now, whatever the setting.
    CheckNow,
    Install,
    Skip,
    ViewRelease,
    /// Time to see whether an automatic check is due.
    Tick,
}

pub struct Updates {
    pub updater: Updater,
    /// `None` runs no checks (tests).
    worker: Option<Worker>,
    pub kind: InstallKind,
    state_path: Option<PathBuf>,
    started: Instant,
    /// Built by the release workflow: checks on its own and installs.
    pub official: bool,
    /// Installs need a signature.
    pub signed: bool,
    /// Set while the app has stepped aside for the installer.
    pub handed_over: Option<HandOver>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HandOver {
    /// Whether to start the server again if the installer gives up.
    pub server_was_running: bool,
    /// The installer starts once the server has stopped.
    pub waiting_for_server: bool,
}

impl Updates {
    pub fn new(
        worker: Option<Worker>,
        kind: InstallKind,
        state_path: Option<PathBuf>,
        official: bool,
        signed: bool,
    ) -> Self {
        let saved = state_path.as_deref().map(Saved::load).unwrap_or_default();
        let restored = update::restore_found(&saved, kind);
        Self {
            updater: Updater::new(saved, restored),
            worker,
            kind,
            state_path,
            started: Instant::now(),
            official,
            signed,
            handed_over: None,
        }
    }

    /// What the app offers for the release it shows.
    pub fn action(&self) -> Option<Action> {
        self.updater
            .offered()
            .map(|r| update::action(r, self.kind, self.official, self.signed))
    }

    fn persist(&self) {
        if let Some(path) = &self.state_path
            && let Err(e) = self.updater.saved.save(path)
        {
            tracing::warn!("saving {} failed: {e}", path.display());
        }
    }

    fn check(&mut self, manual: bool) {
        if !self.updater.start_check(manual, SystemTime::now()) {
            return;
        }
        if !self.worker.as_ref().is_some_and(|w| w.check(manual)) {
            tracing::debug!("no update worker; the check stays pending");
        }
    }

    /// Tells the worker whether to start the installer.
    fn proceed(&mut self, go: bool) {
        if let Some(hand_over) = &mut self.handed_over
            && hand_over.waiting_for_server
        {
            hand_over.waiting_for_server = false;
            if let Some(worker) = &self.worker {
                worker.proceed(go);
            }
        }
    }
}

impl App {
    /// Whether checks run on their own: the setting is on and this is an
    /// official build.
    pub(super) fn automatic_update_checks(&self) -> bool {
        self.config.check_for_updates && self.updates.official
    }

    pub(super) fn update_message(&mut self, message: UpdateMessage) -> Task<Message> {
        let automatic = self.automatic_update_checks();
        let updates = &mut self.updates;
        match message {
            UpdateMessage::CheckNow => updates.check(true),
            UpdateMessage::Tick => {
                let due = schedule::is_due(
                    SystemTime::now(),
                    updates.started.elapsed(),
                    updates.updater.last_checked(),
                    updates.updater.last_attempt(),
                );
                if automatic && due {
                    updates.check(false);
                    // Keeps a failing check to one try an hour across
                    // restarts.
                    updates.persist();
                }
            }
            UpdateMessage::Install => {
                if updates.action() != Some(Action::InstallAndRestart) {
                    return Task::none();
                }
                if let Some(release) = updates.updater.start_install() {
                    tracing::info!("installing version {}", release.version);
                    if !updates.worker.as_ref().is_some_and(|w| w.install(release)) {
                        updates
                            .updater
                            .install_failed("the update worker is not running".into());
                    }
                }
            }
            UpdateMessage::Skip => {
                if updates.updater.skip() {
                    updates.persist();
                }
            }
            UpdateMessage::ViewRelease => {
                if let Some(release) = updates.updater.offered()
                    && let Err(e) = update::open_url(&release.page_url)
                {
                    tracing::warn!("opening {} failed: {e}", release.page_url);
                }
            }
        }
        Task::none()
    }

    pub(super) fn update_event(&mut self, event: Event) -> Task<Message> {
        let updater = &mut self.updates.updater;
        match event {
            Event::Checked { manual, result } => {
                if updater.check_finished(manual, result, SystemTime::now()) {
                    self.updates.persist();
                }
            }
            Event::Progress(progress) => updater.progress(progress),
            Event::Verified => return self.hand_over_to_installer(),
            Event::Launched => updater.launched(),
            Event::InstallFailed(message) => {
                updater.install_failed(message);
                if let Some(hand_over) = self.updates.handed_over.take() {
                    return self.take_back_from_installer(hand_over);
                }
            }
        }
        Task::none()
    }

    /// Does what quitting does (stops the server so mDNS withdraws the
    /// service, saves the config, closes the windows and removes the tray
    /// icon) but keeps the process, which holds the verified installer
    /// locked, until the installer stops it to replace the executable. The
    /// installer starts once the server has stopped.
    fn hand_over_to_installer(&mut self) -> Task<Message> {
        if self.quitting || self.updates.handed_over.is_some() {
            if let Some(worker) = &self.updates.worker {
                worker.proceed(false);
            }
            return Task::none();
        }
        tracing::info!("stepping aside for the installer");
        let server_was_running = matches!(
            self.server_status,
            ServerStatus::Running(_) | ServerStatus::Starting
        );
        self.updates.handed_over = Some(HandOver {
            server_was_running,
            waiting_for_server: true,
        });
        if server_was_running {
            self.server.stop();
        } else {
            self.updates.proceed(true);
        }
        let _ = self.persist_config();
        self.tray = None;
        let close = match self.window {
            Some(id) => self.close_window(id),
            None => Task::none(),
        };
        let close_mini = self.mini_event(MiniEvent::Close(CloseReason::Quit));
        Task::batch([close, close_mini])
    }

    /// Starts the installer once the server it waits for has stopped.
    pub(super) fn update_server_status(&mut self) {
        if matches!(
            self.server_status,
            ServerStatus::Stopped | ServerStatus::Failed(_)
        ) {
            self.updates.proceed(true);
        }
    }

    /// Abandons an install that has not started its installer.
    pub(super) fn update_quitting(&mut self) {
        self.updates.proceed(false);
    }

    /// The installer did not replace the app: runs again as before and
    /// shows why on the Status page.
    fn take_back_from_installer(&mut self, hand_over: HandOver) -> Task<Message> {
        if self.quitting {
            return Task::none();
        }
        tracing::info!("the installer did not replace the app; running again");
        if hand_over.server_was_running {
            self.server.start(self.config.server_config());
        }
        let open = self.open_window();
        self.page = super::Page::Status;
        Task::batch([Task::done(Message::CreateTray), open])
    }

    pub(super) fn update_subscription(&self) -> Subscription<Message> {
        if self.automatic_update_checks() {
            iced::time::every(schedule::POLL).map(|_| Message::Update(UpdateMessage::Tick))
        } else {
            Subscription::none()
        }
    }

    /// The banner for an available release, at the top of the Status page.
    pub(super) fn update_banner(&self) -> Option<Element<'_, Message>> {
        let updates = &self.updates;
        let release = updates.updater.offered()?;
        let action = updates.action()?;
        let busy = updates.updater.installing();
        let muted = |s: &'static str| -> Element<'_, Message> {
            text(s)
                .size(style::TEXT_SMALL)
                .style(style::muted_text)
                .into()
        };
        let detail = match updates.updater.phase() {
            Phase::Downloading { done, total } => {
                let percent = (*done * 100).checked_div(*total).unwrap_or(0);
                column![
                    text(format!("Downloading\u{2026} {percent}%"))
                        .size(style::TEXT_SMALL)
                        .style(style::muted_text),
                    progress_bar(0.0..=*total as f32, *done as f32).girth(style::PROGRESS_GIRTH),
                ]
                .spacing(style::SPACE_XS)
                .into()
            }
            Phase::Verifying => muted("Verifying the download\u{2026}"),
            Phase::Launching => muted(if updates.signed {
                "Checksum and signature verified. The installer is running and restarts the app when it is done."
            } else {
                "Checksum verified. The installer is running and restarts the app when it is done."
            }),
            Phase::Failed(Failure::Install(e)) => text(format!("Could not install: {e}"))
                .size(style::TEXT_SMALL)
                .style(text::danger)
                .into(),
            _ => muted(match action {
                Action::InstallAndRestart => {
                    "Downloads and verifies the installer, then restarts the app."
                }
                Action::ViewRelease => "Opens the release page to download it.",
            }),
        };
        let (label, message) = match action {
            Action::InstallAndRestart => ("Install and restart", UpdateMessage::Install),
            Action::ViewRelease => ("View release", UpdateMessage::ViewRelease),
        };
        let primary = button(text(label))
            .padding([style::SPACE_S, style::SPACE_M])
            .style(style::button(Button::Primary))
            .on_press_maybe((!busy).then_some(Message::Update(message)));
        let skip = button(text("Skip this version"))
            .padding([style::SPACE_S, style::SPACE_M])
            .style(style::button(Button::Secondary))
            .on_press_maybe((!busy).then_some(Message::Update(UpdateMessage::Skip)));
        Some(
            container(
                row![
                    column![
                        text(format!("Version {} is available", release.version))
                            .size(style::TEXT_SECTION),
                        detail,
                    ]
                    .spacing(style::SPACE_XS)
                    .width(Fill),
                    primary,
                    skip,
                ]
                .spacing(style::SPACE_M)
                .align_y(Center),
            )
            .padding(style::SPACE_M)
            .width(Fill)
            .style(style::card)
            .into(),
        )
    }

    /// The Status page's Updates section: version, last check and Check now.
    pub(super) fn update_status(&self) -> Element<'_, Message> {
        let updates = &self.updates;
        let updater = &updates.updater;
        let version = if updates.official {
            format!("Version {}", update::current_version())
        } else {
            format!("Version {} (development build)", update::current_version())
        };
        let checked = match updater.last_checked() {
            Some(when) => format!("Last checked {}", checked_label(when, SystemTime::now())),
            None => "Not checked yet".to_owned(),
        };
        let (line, failed) = match updater.phase() {
            Phase::Checking { .. } => ("Checking\u{2026}".to_owned(), false),
            Phase::UpToDate => (format!("Up to date. {checked}"), false),
            Phase::NoReleases => (format!("No releases published yet. {checked}"), false),
            Phase::Failed(Failure::Check(e)) => (format!("Could not check: {e}"), true),
            _ => (checked, false),
        };
        let line = text(line).size(style::TEXT_SMALL);
        let line = if failed {
            line.style(text::danger)
        } else {
            line.style(style::muted_text)
        };
        let automatic = if !updates.official {
            Some("Development builds check only when asked.")
        } else if !self.config.check_for_updates {
            Some("Automatic checks are off (Settings).")
        } else {
            None
        };
        let check = button(text("Check now"))
            .padding([style::SPACE_S, style::SPACE_M])
            .style(style::button(Button::Secondary))
            .on_press_maybe((!updater.busy()).then_some(Message::Update(UpdateMessage::CheckNow)));
        row![
            column![text(version), line]
                .push(
                    automatic.map(|a| { text(a).size(style::TEXT_SMALL).style(style::muted_text) })
                )
                .spacing(style::SPACE_XS)
                .width(Fill),
            check,
        ]
        .spacing(style::SPACE_M)
        .align_y(Center)
        .into()
    }
}

/// "today at 14:03", or the date for an earlier day.
fn checked_label(when: SystemTime, now: SystemTime) -> String {
    let (when, now) = (DateTime::<Local>::from(when), DateTime::<Local>::from(now));
    if when.date_naive() == now.date_naive() {
        when.format("today at %H:%M").to_string()
    } else {
        when.format("on %Y-%m-%d at %H:%M").to_string()
    }
}

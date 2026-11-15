//! The iced daemon: state, messages, update and subscriptions.

use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use iced::widget::{button, column, container, row, text};
use iced::{Element, Length, Size, Subscription, Task, Theme, window};
use pcvc_audio::{AudioBackend, AudioState};
use pcvc_protocol::Command;
use pcvc_server::{ClientInfo, MdnsStatus};

use crate::audio::{self, CommandSender};
use crate::config::Config;
use crate::icons;
use crate::inbox::{Inbound, Inbox, InboxReceiver};
use crate::model;
use crate::server_ctl::{ServerController, ServerStatus, ServerUpdate};
use crate::tray::{Tray, TrayAction, TrayModel};

const MIN_SIZE: Size = Size::new(480.0, 400.0);

/// Longest wait for the server to stop on quit before exiting anyway.
const QUIT_TIMEOUT: Duration = Duration::from_secs(8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Mixer,
    Status,
    Settings,
}

#[derive(Debug, Clone)]
pub enum Message {
    Inbound(Inbound),
    CreateTray,
    WindowCloseRequested(window::Id),
    WindowClosed(window::Id),
    WindowResized(window::Id, Size),
    ShowPage(Page),
    ToggleServer,
    ForceExit,
}

/// What `main` prepares before the daemon starts.
pub struct Boot {
    pub config: Config,
    pub show_window: bool,
    pub backend: Arc<dyn AudioBackend>,
    pub inbox: Inbox,
    pub inbox_rx: InboxReceiver,
}

pub struct App {
    inbox: Inbox,
    inbox_rx: InboxReceiver,
    commands: CommandSender,
    server: ServerController,
    server_status: ServerStatus,
    clients: Vec<ClientInfo>,
    mdns: MdnsStatus,
    audio: Option<AudioState>,
    local_ips: Vec<Ipv4Addr>,
    tray: Option<Tray>,
    window: Option<window::Id>,
    config: Config,
    page: Page,
    quitting: bool,
}

impl App {
    /// Runs inside iced's tokio runtime, so it may spawn tasks.
    pub fn boot(boot: Boot) -> (Self, Task<Message>) {
        let Boot {
            config,
            show_window,
            backend,
            inbox,
            inbox_rx,
        } = boot;
        audio::spawn_watcher(Arc::clone(&backend), inbox.clone());
        let commands = CommandSender::spawn(Arc::clone(&backend));
        let server = ServerController::spawn(backend, inbox.clone());
        server.start(config.server_config());

        let mut app = Self {
            inbox,
            inbox_rx,
            commands,
            server,
            server_status: ServerStatus::Starting,
            clients: Vec::new(),
            mdns: MdnsStatus::Disabled,
            audio: None,
            local_ips: model::local_ipv4s(),
            tray: None,
            window: None,
            config,
            page: Page::Mixer,
            quitting: false,
        };
        let open = if show_window {
            app.open_window()
        } else {
            Task::none()
        };
        // The tray is created from the first update rather than here; see
        // `tray::Tray`.
        (app, Task::batch([Task::done(Message::CreateTray), open]))
    }

    pub fn title(&self, _window: window::Id) -> String {
        "PcVolumeControl".to_owned()
    }

    pub fn theme(&self, _window: window::Id) -> Option<Theme> {
        None
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.handle(message);
        self.sync_tray();
        task
    }

    fn handle(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::CreateTray => {
                self.tray = Some(Tray::spawn(self.compute_tray_model(), self.inbox.clone()));
                Task::none()
            }
            Message::Inbound(Inbound::TrayFailed(e)) => {
                tracing::error!("creating the tray icon failed: {e}");
                self.tray = None;
                // Without a tray the window is the only way in, and closing
                // it quits.
                self.open_window()
            }
            Message::Inbound(Inbound::Tray(action)) => self.tray_action(action),
            Message::Inbound(Inbound::Audio(state)) => {
                self.audio = Some(state);
                Task::none()
            }
            Message::Inbound(Inbound::Server(update)) => self.server_update(update),
            Message::WindowCloseRequested(id) => self.close_requested(id),
            Message::WindowClosed(id) => {
                if self.window == Some(id) {
                    self.window = None;
                }
                Task::none()
            }
            Message::WindowResized(id, size) => {
                // Minimising reports a zero size on some platforms.
                if self.window == Some(id)
                    && size.width >= MIN_SIZE.width
                    && size.height >= MIN_SIZE.height
                {
                    self.config.window.width = size.width;
                    self.config.window.height = size.height;
                }
                Task::none()
            }
            Message::ShowPage(page) => {
                self.page = page;
                Task::none()
            }
            Message::ToggleServer => {
                self.toggle_server();
                Task::none()
            }
            Message::ForceExit => {
                tracing::warn!("server did not shut down in time; exiting anyway");
                self.exit()
            }
        }
    }

    fn tray_action(&mut self, action: TrayAction) -> Task<Message> {
        match action {
            TrayAction::Activate => match self.window {
                Some(id) => self.close_window(id),
                None => self.open_window(),
            },
            TrayAction::Open => self.open_window(),
            TrayAction::ToggleMute => {
                if let Some(device) = self.audio.as_ref().and_then(|a| a.default_device.as_ref()) {
                    self.commands.send(Command::SetMaster {
                        volume: None,
                        muted: Some(!device.muted),
                    });
                }
                Task::none()
            }
            TrayAction::ToggleServer => {
                self.toggle_server();
                Task::none()
            }
            TrayAction::ToggleStartOnLogin => Task::none(),
            TrayAction::Quit => self.quit(),
        }
    }

    fn server_update(&mut self, update: ServerUpdate) -> Task<Message> {
        match update {
            ServerUpdate::Status(status) => {
                if matches!(status, ServerStatus::Running(_)) {
                    self.local_ips = model::local_ipv4s();
                }
                self.server_status = status;
            }
            ServerUpdate::Clients(clients) => self.clients = clients,
            ServerUpdate::Mdns(mdns) => self.mdns = mdns,
            ServerUpdate::Event(event) => tracing::debug!("server event: {event:?}"),
            ServerUpdate::ShutDown => return self.exit(),
        }
        Task::none()
    }

    fn toggle_server(&mut self) {
        match self.server_status {
            ServerStatus::Running(_) | ServerStatus::Starting => self.server.stop(),
            ServerStatus::Stopped | ServerStatus::Failed(_) => {
                self.server.start(self.config.server_config());
            }
        }
    }

    fn open_window(&mut self) -> Task<Message> {
        if let Some(id) = self.window {
            return window::gain_focus(id);
        }
        self.page = Page::Mixer;
        let (id, opened) = window::open(window::Settings {
            size: Size::new(self.config.window.width, self.config.window.height),
            min_size: Some(MIN_SIZE),
            icon: icons::window_icon(),
            exit_on_close_request: false,
            #[cfg(target_os = "linux")]
            platform_specific: window::settings::PlatformSpecific {
                application_id: "pcvolumecontrol".to_owned(),
                ..Default::default()
            },
            ..Default::default()
        });
        self.window = Some(id);
        opened.discard()
    }

    /// Closing the window hides the app to the tray, or quits without one.
    fn close_requested(&mut self, id: window::Id) -> Task<Message> {
        if self.tray.is_none() {
            return self.quit();
        }
        self.close_window(id)
    }

    fn close_window(&mut self, id: window::Id) -> Task<Message> {
        if self.window == Some(id) {
            self.window = None;
        }
        window::close(id)
    }

    fn quit(&mut self) -> Task<Message> {
        if self.quitting {
            return Task::none();
        }
        tracing::info!("quitting");
        self.quitting = true;
        self.server.shutdown();
        let close = match self.window {
            Some(id) => self.close_window(id),
            None => Task::none(),
        };
        let deadline = Task::perform(tokio::time::sleep(QUIT_TIMEOUT), |()| Message::ForceExit);
        Task::batch([close, deadline])
    }

    fn exit(&mut self) -> Task<Message> {
        // Dropping the tray withdraws the icon before the process ends.
        self.tray = None;
        iced::exit()
    }

    fn compute_tray_model(&self) -> TrayModel {
        TrayModel {
            state: model::tray_state(&self.server_status, self.clients.len()),
            tooltip: model::tooltip(&self.server_status, self.clients.len(), &self.local_ips),
            muted: self
                .audio
                .as_ref()
                .and_then(|a| a.default_device.as_ref())
                .map(|d| d.muted),
            server_running: matches!(
                self.server_status,
                ServerStatus::Running(_) | ServerStatus::Starting
            ),
            start_on_login: self.config.start_on_login,
        }
    }

    fn sync_tray(&mut self) {
        if self.tray.is_some() {
            let model = self.compute_tray_model();
            if let Some(tray) = self.tray.as_mut() {
                tray.update(model);
            }
        }
    }

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            self.inbox_rx.subscription().map(Message::Inbound),
            window::close_requests().map(Message::WindowCloseRequested),
            window::close_events().map(Message::WindowClosed),
            window::resize_events().map(|(id, size)| Message::WindowResized(id, size)),
        ])
    }

    pub fn view(&self, id: window::Id) -> Element<'_, Message> {
        if self.window != Some(id) {
            return text("").into();
        }
        let tab = |label: &'static str, page: Page| {
            button(text(label))
                .on_press(Message::ShowPage(page))
                .style(if self.page == page {
                    button::primary
                } else {
                    button::text
                })
                .width(Length::Fill)
        };
        let nav = column![
            tab("Mixer", Page::Mixer),
            tab("Status", Page::Status),
            tab("Settings", Page::Settings),
        ]
        .spacing(4)
        .width(140);
        let body: Element<'_, Message> = match self.page {
            Page::Mixer => text("Mixer").into(),
            Page::Status => column![
                text(format!("{:?}", self.server_status)),
                button(text("Start/Stop")).on_press(Message::ToggleServer),
            ]
            .into(),
            Page::Settings => text("Settings").into(),
        };
        row![
            container(nav).padding(12),
            container(body).padding(16).width(Length::Fill)
        ]
        .into()
    }
}

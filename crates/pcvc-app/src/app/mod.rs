//! The iced daemon: state, messages, update and subscriptions.

mod mini;
mod mixer;
mod settings;
mod status;
#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use iced::keyboard::{self, key};
use iced::widget::{button, column, container, image, row, text};
use iced::{Element, Length, Size, Subscription, Task, Theme, event, window};
use pcvc_audio::{AudioBackend, AudioState};
use pcvc_protocol::Command;
use pcvc_server::{ClientInfo, IconProvider, MdnsStatus};

use crate::app_icons::{IconCache, IconKey, IconWorker, Resolver, Slot};
use crate::audio::{self, CommandSender};
use crate::config::{Config, WindowSize};
use crate::inbox::{Inbound, Inbox, InboxReceiver};
use crate::logging::LogBuffer;
use crate::mini::{CloseReason, Event as MiniEvent, MiniWindow};
use crate::mixer::Mixer;
use crate::model;
use crate::placement::{self, Anchoring};
use crate::server_ctl::{ServerController, ServerStatus, ServerUpdate};
use crate::tray::{Tray, TrayAction, TrayModel};
use crate::{autostart, icons, instance, style};
use mini::MiniMessage;
use mixer::MixerMessage;
use settings::{Notice, SettingsForm, SettingsMessage};

const MIN_SIZE: Size = Size::new(WindowSize::MIN.width, WindowSize::MIN.height);

/// Longest wait for the server to stop on quit before exiting anyway.
const QUIT_TIMEOUT: Duration = Duration::from_secs(8);

/// Application icons remembered, found or not.
const ICON_CACHE_CAPACITY: usize = 256;

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
    ScaleFactor(window::Id, f32),
    /// A window gained (`true`) or lost focus.
    WindowFocus(window::Id, bool),
    /// Escape was pressed in a window and no widget took it.
    Escape(window::Id),
    ShowPage(Page),
    ToggleServer,
    Mixer(MixerMessage),
    Mini(MiniMessage),
    Settings(SettingsMessage),
    /// Periodic refresh of time-dependent views.
    Tick,
    ForceExit,
}

/// Window events not covered by iced's own subscriptions.
fn window_event(event: iced::Event, status: event::Status, id: window::Id) -> Option<Message> {
    match event {
        iced::Event::Window(window::Event::Rescaled(factor)) => {
            Some(Message::ScaleFactor(id, factor))
        }
        iced::Event::Window(window::Event::Focused) => Some(Message::WindowFocus(id, true)),
        iced::Event::Window(window::Event::Unfocused) => Some(Message::WindowFocus(id, false)),
        iced::Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key::Named::Escape),
            ..
        }) if status == event::Status::Ignored => Some(Message::Escape(id)),
        _ => None,
    }
}

/// What `main` prepares before the daemon starts.
pub struct Boot {
    pub config: Config,
    /// Where the config is saved; `None` keeps it in memory only.
    pub config_path: Option<PathBuf>,
    pub show_window: bool,
    pub backend: Arc<dyn AudioBackend>,
    pub icon_resolver: Arc<dyn Resolver>,
    /// Icons for phone clients, given to the server on every start.
    pub server_icons: Option<Arc<dyn IconProvider>>,
    /// Whether every server start sends session details.
    pub server_details: bool,
    pub inbox: Inbox,
    pub inbox_rx: InboxReceiver,
    pub logs: LogBuffer,
}

/// Quits cleanly (so mDNS sends its goodbye) on Ctrl-C or, on Unix, on
/// SIGTERM as sent at logout. A second signal exits at once.
fn spawn_signal_handler(inbox: Inbox) {
    tokio::spawn(async move {
        #[cfg(unix)]
        let mut term =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(term) => Some(term),
                Err(e) => {
                    tracing::warn!("cannot listen for SIGTERM: {e}");
                    None
                }
            };
        for first in [true, false] {
            let interrupt = async {
                if let Err(e) = tokio::signal::ctrl_c().await {
                    tracing::warn!("cannot listen for Ctrl-C: {e}");
                    std::future::pending::<()>().await;
                }
            };
            #[cfg(unix)]
            let terminate = async {
                match term.as_mut() {
                    Some(term) => {
                        term.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            #[cfg(not(unix))]
            let terminate = std::future::pending::<()>();
            tokio::select! {
                () = interrupt => {}
                () = terminate => {}
            }
            if first {
                inbox.send(Inbound::QuitRequested);
            } else {
                tracing::warn!("second termination signal; exiting now");
                std::process::exit(1);
            }
        }
    });
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
    mixer: Mixer,
    icon_worker: IconWorker,
    icons: IconCache<IconKey, image::Handle>,
    /// The icon key of each shown session (by session id), and the key it
    /// had at the previous icon size, shown until the new one resolves.
    session_icons: HashMap<String, (IconKey, Option<IconKey>)>,
    /// The largest scale factor of the open windows, for rendering icons at
    /// physical size; the last one known while none has reported, and
    /// unknown until a window has.
    scale_factor: Option<f32>,
    /// Scale factors reported by the open windows.
    window_scales: HashMap<window::Id, f32>,
    /// When the pending mixer wake-up fires, if one is scheduled.
    mixer_timer: Option<std::time::Instant>,
    local_ips: Vec<Ipv4Addr>,
    /// The last interface listing error, so a persistent one is logged once.
    local_ips_error: Option<String>,
    tray: Option<Tray>,
    /// The full window.
    window: Option<window::Id>,
    mini: MiniWindow,
    /// The height last given to the mini mixer.
    mini_height: f32,
    /// Whether the mini mixer's device list is open.
    mini_picker_open: bool,
    /// What the mini mixer was placed against, where the platform lets it
    /// be placed.
    mini_anchoring: Option<Anchoring>,
    /// The scale factor the mini mixer's position was converted at.
    mini_position_scale: f32,
    /// Looks up where to place the mini mixer; `None` leaves it to the
    /// platform from the start.
    mini_locator: Option<mini::Locator>,
    config: Config,
    config_path: Option<PathBuf>,
    settings: SettingsForm,
    notice: Option<Notice>,
    page: Page,
    logs: LogBuffer,
    quitting: bool,
}

impl App {
    /// Runs inside iced's tokio runtime, so it may spawn tasks.
    pub fn boot(boot: Boot) -> (Self, Task<Message>) {
        let Boot {
            mut config,
            config_path,
            show_window,
            backend,
            icon_resolver,
            server_icons,
            server_details,
            inbox,
            inbox_rx,
            logs,
        } = boot;
        let icon_worker = IconWorker::spawn(icon_resolver, inbox.clone());
        audio::spawn_watcher(Arc::clone(&backend), inbox.clone());
        spawn_signal_handler(inbox.clone());
        let commands = CommandSender::spawn(Arc::clone(&backend));
        let server = ServerController::spawn(backend, server_icons, server_details, inbox.clone());
        server.start(config.server_config());
        // The login item itself is the truth; it may have been removed
        // outside the app.
        match autostart::is_enabled() {
            Ok(enabled) => config.start_on_login = enabled,
            Err(e) => tracing::warn!("reading start on login failed: {e}"),
        }
        if config.start_on_login {
            autostart::refresh();
        }

        let mut app = Self {
            inbox,
            inbox_rx,
            commands,
            server,
            server_status: ServerStatus::Starting,
            clients: Vec::new(),
            mdns: MdnsStatus::Disabled,
            audio: None,
            mixer: Mixer::default(),
            icon_worker,
            icons: IconCache::new(ICON_CACHE_CAPACITY),
            session_icons: HashMap::new(),
            scale_factor: None,
            window_scales: HashMap::new(),
            mixer_timer: None,
            local_ips: Vec::new(),
            local_ips_error: None,
            tray: None,
            window: None,
            mini: MiniWindow::default(),
            mini_height: 0.0,
            mini_picker_open: false,
            mini_anchoring: None,
            mini_position_scale: 1.0,
            mini_locator: Some(placement::anchoring),
            settings: SettingsForm::from(&config),
            notice: None,
            config,
            config_path,
            page: Page::Mixer,
            logs,
            quitting: false,
        };
        app.refresh_local_ips();
        let open = if show_window {
            app.open_window()
        } else {
            Task::none()
        };
        // The tray is created from the first update rather than here; see
        // `tray::Tray`.
        (app, Task::batch([Task::done(Message::CreateTray), open]))
    }

    pub fn title(&self, window: window::Id) -> String {
        if self.mini.is(window) {
            "PcVolumeControl Mixer".to_owned()
        } else {
            "PcVolumeControl".to_owned()
        }
    }

    pub fn theme(&self, _window: window::Id) -> Option<Theme> {
        self.config.theme.theme()
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
            Message::Inbound(Inbound::ShowRequested) => self.open_window(),
            Message::Inbound(Inbound::QuitRequested) => self.quit(),
            Message::Inbound(Inbound::Audio(state)) => {
                self.mixer.on_state(&state);
                self.audio = Some(state);
                self.request_icons();
                self.fit_mini()
            }
            Message::Inbound(Inbound::AppIcon(resolved)) => {
                self.icons
                    .finish(resolved.key, resolved.icon, std::time::Instant::now());
                Task::none()
            }
            Message::Inbound(Inbound::Server(update)) => self.server_update(update),
            Message::WindowCloseRequested(id) => self.close_requested(id),
            Message::WindowClosed(id) => {
                if self.window == Some(id) {
                    self.window = None;
                }
                self.forget_window(id);
                self.mini_event(MiniEvent::Closed(id))
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
            Message::ScaleFactor(id, factor) => {
                let ours = self.window == Some(id) || self.mini.is(id);
                if !(ours && factor.is_finite() && factor > 0.0) {
                    return Task::none();
                }
                let previous = self.window_scales.insert(id, factor);
                self.update_scale_factor();
                match previous {
                    Some(previous) if self.mini.is(id) => self.mini_rescaled(id, previous),
                    _ => Task::none(),
                }
            }
            Message::WindowFocus(id, focused) => {
                if !self.mini.is(id) {
                    tracing::debug!("window {id:?} focused: {focused}");
                    return Task::none();
                }
                self.mini_event(if focused {
                    MiniEvent::Focused(id)
                } else {
                    MiniEvent::Unfocused(id)
                })
            }
            Message::Escape(id) => self.mini_escape(id),
            Message::Mini(message) => self.mini_update(message),
            Message::ShowPage(page) => {
                self.page = page;
                Task::none()
            }
            Message::ToggleServer => {
                self.toggle_server();
                Task::none()
            }
            Message::Mixer(message) => self.mixer_update(message),
            Message::Settings(message) => self.settings_update(message),
            Message::Tick => {
                if self.page == Page::Status {
                    self.refresh_local_ips();
                }
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
            TrayAction::Click(button, area) => {
                tracing::debug!("tray: {button:?} click at {area:?}");
                self.mini_toggle(MiniEvent::Toggle, area)
            }
            TrayAction::Mixer => {
                tracing::debug!("tray: Mixer menu item");
                self.mini_toggle(MiniEvent::MenuToggle, None)
            }
            TrayAction::DoubleClick => {
                tracing::debug!("tray: double click");
                let close = self.mini_event(MiniEvent::DoubleClick);
                Task::batch([close, self.open_window()])
            }
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
            TrayAction::ToggleStartOnLogin => {
                self.toggle_start_on_login();
                Task::none()
            }
            TrayAction::Quit => self.quit(),
        }
    }

    fn server_update(&mut self, update: ServerUpdate) -> Task<Message> {
        match update {
            ServerUpdate::Status(status) => {
                if matches!(status, ServerStatus::Running(_)) {
                    self.refresh_local_ips();
                }
                self.server_status = status;
            }
            ServerUpdate::Clients(clients) => self.clients = clients,
            ServerUpdate::Mdns(mdns) => self.mdns = mdns,
            // The server logs these itself.
            ServerUpdate::Event(event) => tracing::debug!("server event: {event:?}"),
            ServerUpdate::ShutDown => return self.exit(),
        }
        Task::none()
    }

    fn refresh_local_ips(&mut self) {
        match model::local_ipv4s() {
            Ok(ips) => {
                self.local_ips = ips;
                self.local_ips_error = None;
            }
            Err(e) => {
                let e = e.to_string();
                if self.local_ips_error.as_ref() != Some(&e) {
                    tracing::warn!("listing network interfaces failed: {e}");
                    self.local_ips_error = Some(e);
                }
                self.local_ips.clear();
            }
        }
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
        tracing::debug!("opening the window");
        self.page = Page::Mixer;
        self.settings = SettingsForm::from(&self.config);
        self.notice = None;
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
        // Uses the scale factor from an earlier window until this one
        // reports its own.
        self.request_icons();
        opened.then(|id| window::scale_factor(id).map(move |f| Message::ScaleFactor(id, f)))
    }

    /// Edge length of application icons in physical pixels, once the scale
    /// factor is known.
    fn icon_size(&self) -> Option<u32> {
        let scale = self.scale_factor?.clamp(1.0, 4.0);
        Some((style::APP_ICON * scale).ceil() as u32)
    }

    /// Uses the largest scale factor of the open windows, so icons are
    /// sharp in each, and looks icons up again when it changes.
    fn update_scale_factor(&mut self) {
        let largest = self.window_scales.values().copied().reduce(f32::max);
        if largest.is_some() && largest != self.scale_factor {
            self.scale_factor = largest;
            self.request_icons();
        }
    }

    /// Drops what is kept about a window that is closing or closed.
    fn forget_window(&mut self, id: window::Id) {
        if self.window_scales.remove(&id).is_some() {
            self.update_scale_factor();
        }
    }

    /// Starts lookups for the icons of the shown sessions that are not
    /// known yet. Nothing is looked up while both windows are closed.
    fn request_icons(&mut self) {
        if self.window.is_none() && self.mini.id().is_none() {
            return;
        }
        let (Some(size), Some(device)) = (
            self.icon_size(),
            self.audio.as_ref().and_then(|a| a.default_device.as_ref()),
        ) else {
            return;
        };
        let now = std::time::Instant::now();
        let mut session_icons = HashMap::with_capacity(device.sessions.len());
        for session in &device.sessions {
            let key = IconKey::new(session, size);
            if self.icons.request(&key, now) && !self.icon_worker.request(key.clone()) {
                self.icons.finish(key.clone(), None, now);
            }
            let previous = self
                .session_icons
                .remove(&session.id)
                .and_then(|(old, older)| if old == key { older } else { Some(old) });
            session_icons.insert(session.id.clone(), (key, previous));
        }
        self.session_icons = session_icons;
    }

    /// What to show for a session's icon: its icon (possibly at the
    /// previous size while the current one is looked up), `Ready(None)`
    /// for the fallback tile, or `Pending`.
    fn session_icon(&self, session_id: &str) -> Slot<&image::Handle> {
        let Some((key, previous)) = self.session_icons.get(session_id) else {
            return Slot::Pending;
        };
        match self.icons.get(key) {
            Some(Slot::Ready(icon)) => Slot::Ready(icon.as_ref()),
            _ => match previous.as_ref().and_then(|p| self.icons.get(p)) {
                Some(Slot::Ready(Some(icon))) => Slot::Ready(Some(icon)),
                _ => Slot::Pending,
            },
        }
    }

    /// Closing the full window hides the app to the tray, or quits without
    /// one. Closing the mini mixer never quits.
    fn close_requested(&mut self, id: window::Id) -> Task<Message> {
        if self.mini.is(id) {
            return self.mini_event(MiniEvent::Close(CloseReason::CloseRequest));
        }
        if self.tray.is_none() {
            return self.quit();
        }
        self.close_window(id)
    }

    fn close_window(&mut self, id: window::Id) -> Task<Message> {
        if self.window == Some(id) {
            self.window = None;
            self.forget_window(id);
            // Keeps the window size for next time.
            let _ = self.persist_config();
        }
        window::close(id)
    }

    fn quit(&mut self) -> Task<Message> {
        if self.quitting {
            return Task::none();
        }
        tracing::info!("quitting");
        self.quitting = true;
        instance::shutting_down();
        self.server.shutdown();
        let close = match self.window {
            Some(id) => self.close_window(id),
            None => Task::none(),
        };
        let close_mini = self.mini_event(MiniEvent::Close(CloseReason::Quit));
        let deadline = Task::perform(tokio::time::sleep(QUIT_TIMEOUT), |()| Message::ForceExit);
        Task::batch([close, close_mini, deadline])
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
        let tick = if self.window.is_some() && self.page == Page::Status {
            iced::time::every(Duration::from_secs(1)).map(|_| Message::Tick)
        } else {
            Subscription::none()
        };
        Subscription::batch([
            self.inbox_rx.subscription().map(Message::Inbound),
            window::close_requests().map(Message::WindowCloseRequested),
            window::close_events().map(Message::WindowClosed),
            window::resize_events().map(|(id, size)| Message::WindowResized(id, size)),
            event::listen_with(window_event),
            tick,
        ])
    }

    pub fn view(&self, id: window::Id) -> Element<'_, Message> {
        if self.mini.is(id) {
            return self.mini_view();
        }
        if self.window != Some(id) {
            return text("").into();
        }
        let tab = |label: &'static str, page: Page| {
            button(text(label).size(style::TEXT_BODY))
                .on_press(Message::ShowPage(page))
                .padding([style::SPACE_S, style::SPACE_M])
                .style(style::button(style::Button::Nav(self.page == page)))
                .width(Length::Fill)
        };
        let nav = column![
            tab("Mixer", Page::Mixer),
            tab("Status", Page::Status),
            tab("Settings", Page::Settings),
        ]
        .spacing(style::SPACE_XS)
        .width(style::NAV_WIDTH);
        let (title, body) = match self.page {
            Page::Mixer => ("Mixer", self.mixer_view()),
            Page::Status => ("Status", self.status_view()),
            Page::Settings => ("Settings", self.settings_view()),
        };
        let page = column![text(title).size(style::TEXT_TITLE), body]
            .spacing(style::SPACE_L)
            .height(Length::Fill);
        row![
            container(nav).padding(style::SPACE_M),
            container(page)
                .padding(style::PAGE_PADDING)
                .width(Length::Fill)
        ]
        .into()
    }
}

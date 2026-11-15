//! Tray icon and menu.
//!
//! Events reach the app through `tray-icon`'s global event handlers, which
//! forward into the [`Inbox`]. The icon itself is driven by a [`TrayModel`]
//! computed from app state after every update.

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

use crate::icons;
use crate::inbox::{Inbound, Inbox};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Left click on the icon.
    Activate,
    Open,
    ToggleMute,
    ToggleServer,
    ToggleStartOnLogin,
    Quit,
}

const ID_OPEN: &str = "open";
const ID_MUTE: &str = "mute";
const ID_SERVER: &str = "server";
const ID_LOGIN: &str = "login";
const ID_QUIT: &str = "quit";

impl TrayAction {
    fn from_menu_id(id: &MenuId) -> Option<Self> {
        Some(match id.as_ref() {
            ID_OPEN => Self::Open,
            ID_MUTE => Self::ToggleMute,
            ID_SERVER => Self::ToggleServer,
            ID_LOGIN => Self::ToggleStartOnLogin,
            ID_QUIT => Self::Quit,
            _ => return None,
        })
    }
}

/// Routes tray clicks and menu choices into the inbox. Call once, before the
/// tray is created.
pub fn install_event_handlers(inbox: &Inbox) {
    let tray_inbox = inbox.clone();
    TrayIconEvent::set_event_handler(Some(move |event| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            button_state: MouseButtonState::Up,
            ..
        } = event
        {
            tray_inbox.send(Inbound::Tray(TrayAction::Activate));
        }
    }));
    let menu_inbox = inbox.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some(action) = TrayAction::from_menu_id(&event.id) {
            menu_inbox.send(Inbound::Tray(action));
        }
    }));
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Offline,
    Online,
    ClientsConnected,
}

/// Everything the tray shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrayModel {
    pub state: TrayState,
    pub tooltip: String,
    /// Mute state of the default device, `None` without one.
    pub muted: Option<bool>,
    pub server_running: bool,
    pub start_on_login: bool,
}

/// The icon and menu items, living on whichever thread may touch them.
struct Core {
    icon: TrayIcon,
    offline: Icon,
    online: Icon,
    clients: Icon,
    mute: MenuItem,
    server: MenuItem,
    login: CheckMenuItem,
    // Keeps the menu alive; the items above are views into it.
    _menu: Menu,
    last: TrayModel,
}

#[cfg(target_os = "windows")]
const ICON_SIZE: u32 = 32;
#[cfg(not(target_os = "windows"))]
const ICON_SIZE: u32 = 64;

fn load_icon(svg: &[u8]) -> Result<Icon, String> {
    let rgba = icons::rasterize(svg, ICON_SIZE)?;
    Icon::from_rgba(rgba.pixels, rgba.width, rgba.height).map_err(|e| e.to_string())
}

fn mute_label(muted: Option<bool>) -> &'static str {
    match muted {
        Some(true) => "Unmute default device",
        _ => "Mute default device",
    }
}

fn server_label(running: bool) -> &'static str {
    if running {
        "Stop server"
    } else {
        "Start server"
    }
}

impl Core {
    fn new(model: TrayModel) -> Result<Self, String> {
        let offline = load_icon(icons::TRAY_OFFLINE)?;
        let online = load_icon(icons::TRAY_ONLINE)?;
        let clients = load_icon(icons::TRAY_CLIENTS)?;

        let open = MenuItem::with_id(ID_OPEN, "Open", true, None);
        let mute = MenuItem::with_id(
            ID_MUTE,
            mute_label(model.muted),
            model.muted.is_some(),
            None,
        );
        let server = MenuItem::with_id(ID_SERVER, server_label(model.server_running), true, None);
        let login =
            CheckMenuItem::with_id(ID_LOGIN, "Start on login", true, model.start_on_login, None);
        let quit = MenuItem::with_id(ID_QUIT, "Quit", true, None);
        let menu = Menu::with_items(&[
            &open,
            &PredefinedMenuItem::separator(),
            &mute,
            &server,
            &login,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .map_err(|e| e.to_string())?;

        let icon = TrayIconBuilder::new()
            .with_id("pcvolumecontrol")
            .with_title("PcVolumeControl")
            .with_tooltip(&model.tooltip)
            .with_icon(
                match model.state {
                    TrayState::Offline => &offline,
                    TrayState::Online => &online,
                    TrayState::ClientsConnected => &clients,
                }
                .clone(),
            )
            .with_menu(Box::new(menu.clone()))
            .with_menu_on_left_click(false)
            .build()
            .map_err(|e| e.to_string())?;

        Ok(Self {
            icon,
            offline,
            online,
            clients,
            mute,
            server,
            login,
            _menu: menu,
            last: model,
        })
    }

    fn apply(&mut self, model: TrayModel) {
        if model.state != self.last.state {
            let icon = match model.state {
                TrayState::Offline => &self.offline,
                TrayState::Online => &self.online,
                TrayState::ClientsConnected => &self.clients,
            };
            if let Err(e) = self.icon.set_icon(Some(icon.clone())) {
                tracing::warn!("setting the tray icon failed: {e}");
            }
        }
        if model.tooltip != self.last.tooltip
            && let Err(e) = self.icon.set_tooltip(Some(&model.tooltip))
        {
            tracing::warn!("setting the tray tooltip failed: {e}");
        }
        if model.muted != self.last.muted {
            self.mute.set_text(mute_label(model.muted));
            self.mute.set_enabled(model.muted.is_some());
        }
        if model.server_running != self.last.server_running {
            self.server.set_text(server_label(model.server_running));
        }
        // The platform may toggle the check mark itself when clicked, so
        // compare against the item rather than the last model.
        if self.login.is_checked() != model.start_on_login {
            self.login.set_checked(model.start_on_login);
        }
        self.last = model;
    }
}

pub use platform::Tray;

/// `ksni` drives its D-Bus service with blocking calls into its own tokio
/// runtime, which panics when made from a thread already inside a runtime
/// (iced's `update` is). The tray therefore lives on a thread of its own.
#[cfg(target_os = "linux")]
mod platform {
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{Core, TrayModel};
    use crate::inbox::{Inbound, Inbox};

    /// How long dropping the tray waits for the icon to be withdrawn.
    const DROP_TIMEOUT: Duration = Duration::from_secs(2);

    pub struct Tray {
        models: Option<mpsc::Sender<TrayModel>>,
        done: mpsc::Receiver<()>,
    }

    impl Tray {
        /// Creates the icon in the background; a failure arrives in the inbox
        /// as [`Inbound::TrayFailed`].
        pub fn spawn(model: TrayModel, inbox: Inbox) -> Self {
            let (models, rx) = mpsc::channel::<TrayModel>();
            let (done_tx, done) = mpsc::sync_channel(1);
            let spawned = std::thread::Builder::new()
                .name("tray".into())
                .spawn(move || {
                    match Core::new(model) {
                        Ok(mut core) => {
                            tracing::info!("tray icon created");
                            for model in rx {
                                core.apply(model);
                            }
                        }
                        Err(e) => {
                            inbox.send(Inbound::TrayFailed(e));
                        }
                    }
                    let _ = done_tx.send(());
                });
            if let Err(e) = spawned {
                tracing::error!("spawning the tray thread failed: {e}");
            }
            Self {
                models: Some(models),
                done,
            }
        }

        pub fn update(&mut self, model: TrayModel) {
            if let Some(models) = &self.models {
                let _ = models.send(model);
            }
        }
    }

    impl Drop for Tray {
        /// Waits briefly for the icon to be withdrawn from the panel.
        fn drop(&mut self) {
            self.models = None;
            if self.done.recv_timeout(DROP_TIMEOUT) == Err(mpsc::RecvTimeoutError::Timeout) {
                tracing::warn!("tray icon was not withdrawn within {DROP_TIMEOUT:?}");
            }
        }
    }
}

/// On Windows the icon must be created on the thread running the Win32
/// message loop, after the loop has started: the app creates it from its
/// first `update`, never from `boot`.
#[cfg(not(target_os = "linux"))]
mod platform {
    use super::{Core, TrayModel};
    use crate::inbox::{Inbound, Inbox};

    pub struct Tray(Option<Core>);

    impl Tray {
        /// Creates the icon on the calling thread; a failure arrives in the
        /// inbox as [`Inbound::TrayFailed`].
        pub fn spawn(model: TrayModel, inbox: Inbox) -> Self {
            match Core::new(model) {
                Ok(core) => {
                    tracing::info!("tray icon created");
                    Self(Some(core))
                }
                Err(e) => {
                    inbox.send(Inbound::TrayFailed(e));
                    Self(None)
                }
            }
        }

        pub fn update(&mut self, model: TrayModel) {
            if let Some(core) = &mut self.0 {
                core.apply(model);
            }
        }
    }
}

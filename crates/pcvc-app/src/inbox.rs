//! The single channel through which everything outside iced (tray, menu,
//! server controller, audio watcher, second instances) reaches `update`.

use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};

use iced::Subscription;
use iced::futures::channel::mpsc;
use iced::futures::stream::{self, BoxStream, StreamExt};
use pcvc_audio::AudioState;

use crate::app_icons::Resolved;
use crate::server_ctl::ServerUpdate;
use crate::tray::TrayAction;

#[derive(Debug, Clone)]
pub enum Inbound {
    Tray(TrayAction),
    /// The tray icon could not be created.
    TrayFailed(String),
    Server(ServerUpdate),
    Audio(AudioState),
    /// An application icon lookup finished.
    AppIcon(Resolved),
    /// Another launch asked this instance to show its window.
    ShowRequested,
    /// The process was asked to terminate (Ctrl-C, SIGTERM).
    QuitRequested,
}

/// Sending half; cheap to clone and usable from any thread.
#[derive(Debug, Clone)]
pub struct Inbox(mpsc::UnboundedSender<Inbound>);

impl Inbox {
    /// Returns `false` once the app has shut down.
    pub fn send(&self, message: Inbound) -> bool {
        self.0.unbounded_send(message).is_ok()
    }
}

/// Receiving half, handed to the subscription exactly once.
#[derive(Clone)]
pub struct InboxReceiver(Arc<Mutex<Option<mpsc::UnboundedReceiver<Inbound>>>>);

impl Hash for InboxReceiver {
    fn hash<H: Hasher>(&self, state: &mut H) {
        "inbox".hash(state);
    }
}

pub fn channel() -> (Inbox, InboxReceiver) {
    let (tx, rx) = mpsc::unbounded();
    (Inbox(tx), InboxReceiver(Arc::new(Mutex::new(Some(rx)))))
}

impl InboxReceiver {
    /// The raw receiver, for tests that read the inbox without iced.
    #[cfg(all(test, unix))]
    pub fn take_stream(&self) -> Option<mpsc::UnboundedReceiver<Inbound>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }

    /// The subscription has a constant identity, so iced starts the stream
    /// once and keeps it running; the receiver is taken on that first start.
    pub fn subscription(&self) -> Subscription<Inbound> {
        Subscription::run_with(self.clone(), |receiver| -> BoxStream<'static, Inbound> {
            match receiver
                .0
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
            {
                Some(rx) => rx.boxed(),
                None => {
                    tracing::error!("inbox subscription restarted; events are lost");
                    stream::empty().boxed()
                }
            }
        })
    }
}

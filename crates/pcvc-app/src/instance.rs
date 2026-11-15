//! Single instance: the first launch listens on a local socket (a Unix
//! socket in the runtime dir, a per-user named pipe on Windows); later
//! launches ask it to show its window and exit.
//!
//! On Unix the primary also holds an exclusive lock on a file next to the
//! socket. The kernel drops the lock when the process dies, so a launch that
//! gets the lock knows any socket file is stale, and two launches can never
//! both become primary.

use std::io::{self, BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use interprocess::ConnectWaitMode;
use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{ConnectOptions, ListenerOptions, Name};

use crate::inbox::{Inbound, Inbox};

const SHOW: &str = "show";
const OK: &str = "ok";
/// The primary is shutting down; try again once it is gone.
const CLOSING: &str = "closing";
const IO_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a launch keeps trying to reach a primary that holds the name
/// but does not answer yet (still starting, or shutting down).
const PATIENCE: Duration = Duration::from_secs(12);
const RETRY: Duration = Duration::from_millis(250);

static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// Tells later launches that this instance is going away, so they wait and
/// take over instead of asking it to show a window.
pub fn shutting_down() {
    SHUTTING_DOWN.store(true, Ordering::SeqCst);
}

pub enum Instance {
    /// This process is the first instance; hand the listener to [`serve`].
    Primary(Listener),
    /// Another instance is running and was asked to show its window.
    Secondary,
}

#[derive(Debug, thiserror::Error)]
pub enum AcquireError {
    /// Another instance holds the name but could not be reached.
    #[error("another instance is running but did not respond: {0}")]
    Unresponsive(io::Error),
    /// Single-instance handling does not work here; running anyway is safe.
    #[error("single-instance check unavailable: {0}")]
    Unavailable(io::Error),
}

pub struct Listener {
    listener: LocalSocketListener,
    guard: Guard,
}

/// Held by `main` until exit: the Unix lock and socket file. The serving
/// thread owns the listener and never returns, so the socket file is
/// removed here instead.
pub struct Guard {
    #[cfg(unix)]
    _lock: std::fs::File,
    #[cfg(unix)]
    socket: std::path::PathBuf,
}

impl Drop for Guard {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Where this user's socket and lock live.
#[cfg(unix)]
fn runtime_dir() -> io::Result<std::path::PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from)
        && dir.is_dir()
    {
        return Ok(dir);
    }
    // A predictable name in a shared directory: only accept it if nobody
    // else can write to it (another user could otherwise answer for us).
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    let dir = std::env::temp_dir().join(format!("pcvolumecontrol-{user}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&dir) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    let mode = std::fs::symlink_metadata(&dir)?.permissions().mode();
    if mode & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is accessible to other users", dir.display()),
        ));
    }
    Ok(dir)
}

/// Becomes the primary instance or signals the existing one.
#[cfg(unix)]
pub fn acquire() -> Result<Instance, AcquireError> {
    let dir = runtime_dir().map_err(AcquireError::Unavailable)?;
    acquire_in(&dir)
}

#[cfg(unix)]
fn acquire_in(dir: &std::path::Path) -> Result<Instance, AcquireError> {
    use interprocess::local_socket::GenericFilePath;

    let unavailable = AcquireError::Unavailable;
    let socket = dir.join("pcvolumecontrol.sock");
    let name = socket
        .clone()
        .to_fs_name::<GenericFilePath>()
        .map_err(unavailable)?
        .into_owned();
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("pcvolumecontrol.lock"))
        .map_err(unavailable)?;
    let deadline = Instant::now() + PATIENCE;
    loop {
        match lock.try_lock() {
            Ok(()) => {
                // Holding the lock means no live primary: any socket file is
                // left over from a crash.
                match std::fs::remove_file(&socket) {
                    Ok(()) => tracing::info!("removed stale instance socket {}", socket.display()),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                    Err(e) => return Err(unavailable(e)),
                }
                let listener = ListenerOptions::new()
                    .name(name)
                    .create_sync()
                    .map_err(unavailable)?;
                return Ok(Instance::Primary(Listener {
                    listener,
                    guard: Guard {
                        _lock: lock,
                        socket,
                    },
                }));
            }
            Err(std::fs::TryLockError::WouldBlock) => match signal_show(&name) {
                Ok(Reply::Shown) => return Ok(Instance::Secondary),
                Ok(Reply::Closing) | Err(_) if Instant::now() < deadline => {
                    std::thread::sleep(RETRY);
                }
                Ok(Reply::Closing) => {
                    return Err(AcquireError::Unresponsive(io::Error::other(
                        "it is still shutting down",
                    )));
                }
                Err(e) => return Err(AcquireError::Unresponsive(e)),
            },
            Err(std::fs::TryLockError::Error(e)) => return Err(unavailable(e)),
        }
    }
}

/// Becomes the primary instance or signals the existing one. The first
/// pipe instance is exclusive, so creating it decides who is primary.
#[cfg(windows)]
pub fn acquire() -> Result<Instance, AcquireError> {
    use interprocess::local_socket::GenericNamespaced;

    // Named pipes are machine-wide, so the name includes the user to keep
    // other sessions' instances apart.
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    let name = format!("PcVolumeControl-{user}")
        .to_ns_name::<GenericNamespaced>()
        .map_err(AcquireError::Unavailable)?
        .into_owned();
    let deadline = Instant::now() + PATIENCE;
    loop {
        match signal_show(&name) {
            Ok(Reply::Shown) => return Ok(Instance::Secondary),
            Ok(Reply::Closing) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                match ListenerOptions::new().name(name.clone()).create_sync() {
                    Ok(listener) => {
                        return Ok(Instance::Primary(Listener {
                            listener,
                            guard: Guard {},
                        }));
                    }
                    // Another launch created the pipe first.
                    Err(e)
                        if matches!(
                            e.kind(),
                            io::ErrorKind::AddrInUse | io::ErrorKind::PermissionDenied
                        ) => {}
                    Err(e) => return Err(AcquireError::Unavailable(e)),
                }
            }
            // The pipe exists but is busy or not answering yet.
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            return Err(AcquireError::Unresponsive(io::Error::other(
                "gave up waiting for it",
            )));
        }
        std::thread::sleep(RETRY);
    }
}

enum Reply {
    Shown,
    Closing,
}

fn signal_show(name: &Name<'_>) -> io::Result<Reply> {
    let stream = ConnectOptions::new()
        .name(name.borrow())
        .wait_mode(ConnectWaitMode::Timeout(IO_TIMEOUT))
        .connect_sync()?;
    stream.set_recv_timeout(Some(IO_TIMEOUT))?;
    stream.set_send_timeout(Some(IO_TIMEOUT))?;
    let mut stream = BufReader::new(stream);
    stream.get_mut().write_all(format!("{SHOW}\n").as_bytes())?;
    let mut reply = String::new();
    stream.read_line(&mut reply)?;
    match reply.trim() {
        OK => Ok(Reply::Shown),
        CLOSING => Ok(Reply::Closing),
        other => Err(io::Error::other(format!("unexpected reply {other:?}"))),
    }
}

/// Answers later launches for the rest of the process's life. Keep the
/// returned guard until exit.
pub fn serve(listener: Listener, inbox: Inbox) -> Guard {
    let Listener { listener, guard } = listener;
    let spawned = std::thread::Builder::new()
        .name("single-instance".into())
        .spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => {
                        if let Err(e) = answer(stream, &inbox) {
                            tracing::debug!("instance request failed: {e}");
                        }
                    }
                    Err(e) => {
                        tracing::warn!("instance socket accept failed: {e}");
                        // Persistent errors (descriptor exhaustion) would
                        // otherwise spin.
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        });
    if let Err(e) = spawned {
        tracing::error!("spawning the single-instance thread failed: {e}");
    }
    guard
}

fn answer(stream: LocalSocketStream, inbox: &Inbox) -> io::Result<()> {
    stream.set_recv_timeout(Some(IO_TIMEOUT))?;
    stream.set_send_timeout(Some(IO_TIMEOUT))?;
    let mut stream = BufReader::new(stream);
    let mut request = String::new();
    stream.read_line(&mut request)?;
    if request.trim() != SHOW {
        return Ok(());
    }
    let reply = if !SHUTTING_DOWN.load(Ordering::SeqCst) && inbox.send(Inbound::ShowRequested) {
        tracing::info!("another launch asked to show the window");
        OK
    } else {
        CLOSING
    };
    stream.get_mut().write_all(format!("{reply}\n").as_bytes())
}

#[cfg(all(test, unix))]
mod tests {
    use iced::futures::StreamExt;

    use super::*;
    use crate::inbox;

    #[test]
    fn second_launch_signals_first() {
        let dir = tempfile::tempdir().unwrap();
        let Instance::Primary(listener) = acquire_in(dir.path()).unwrap() else {
            panic!("first launch must be primary");
        };
        let (inbox, rx) = inbox::channel();
        let guard = serve(listener, inbox);

        assert!(matches!(
            acquire_in(dir.path()).unwrap(),
            Instance::Secondary
        ));
        let socket = dir.path().join("pcvolumecontrol.sock");
        assert!(socket.exists());
        let mut stream = rx.take_stream().unwrap();
        let received = iced::futures::executor::block_on(stream.next());
        assert!(matches!(received, Some(Inbound::ShowRequested)));

        drop(guard);
        assert!(!socket.exists());
        // With the guard gone, the next launch becomes primary.
        assert!(matches!(
            acquire_in(dir.path()).unwrap(),
            Instance::Primary(_)
        ));
    }

    #[test]
    fn stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("pcvolumecontrol.sock");
        // A socket file with no listener, as left by a crash.
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        assert!(socket.exists());
        assert!(matches!(
            acquire_in(dir.path()).unwrap(),
            Instance::Primary(_)
        ));
    }
}

//! Single instance: the first launch listens on a local socket (a Unix
//! socket in the runtime dir, a named pipe on Windows); later launches ask
//! it to show its window and exit.

use std::io::{self, BufRead, BufReader, Write};
use std::time::Duration;

use interprocess::ConnectWaitMode;
use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{ConnectOptions, ListenerOptions, Name};

use crate::inbox::{Inbound, Inbox};

const SHOW: &str = "show";
const OK: &str = "ok";
const TIMEOUT: Duration = Duration::from_secs(2);

pub enum Instance {
    /// This process is the first instance; hand the listener to [`serve`].
    Primary(Listener),
    /// Another instance is running and was asked to show its window.
    Secondary,
}

pub struct Listener {
    listener: LocalSocketListener,
    cleanup: Cleanup,
}

#[cfg(unix)]
fn socket_name() -> io::Result<(Name<'static>, Cleanup)> {
    use interprocess::local_socket::GenericFilePath;

    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .filter(|d| d.is_dir())
        .unwrap_or_else(|| {
            let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
            std::env::temp_dir().join(format!("pcvolumecontrol-{user}"))
        });
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("pcvolumecontrol.sock");
    let name = path.clone().to_fs_name::<GenericFilePath>()?.into_owned();
    Ok((name, Cleanup(Some(path))))
}

/// Named pipes are machine-wide, so the name includes the user to keep
/// other sessions' instances apart.
#[cfg(windows)]
fn socket_name() -> io::Result<(Name<'static>, Cleanup)> {
    use interprocess::local_socket::GenericNamespaced;

    let user = std::env::var("USERNAME").unwrap_or_else(|_| "user".into());
    let name = format!("PcVolumeControl-{user}")
        .to_ns_name::<GenericNamespaced>()?
        .into_owned();
    Ok((name, Cleanup(None)))
}

/// Removes the Unix socket file when dropped. The serving thread owns the
/// listener and never returns, so this is held by `main` instead.
pub struct Cleanup(Option<std::path::PathBuf>);

impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Becomes the primary instance or signals the existing one. Errors mean
/// single-instance handling is unavailable; the caller may run anyway.
pub fn acquire() -> io::Result<Instance> {
    let (name, cleanup) = socket_name()?;
    acquire_at(name, cleanup)
}

/// The socket file belongs to whichever process listens on it, so the guard
/// stays armed only for the primary.
fn acquire_at(name: Name<'static>, mut cleanup: Cleanup) -> io::Result<Instance> {
    match signal_show(&name) {
        Ok(()) => {
            cleanup.0 = None;
            return Ok(Instance::Secondary);
        }
        // A socket file nobody listens on is left over from a crash.
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {
            if let Some(path) = &cleanup.0 {
                tracing::info!("removing stale instance socket {}", path.display());
                std::fs::remove_file(path)?;
            }
        }
        Err(_) => {}
    }
    match ListenerOptions::new().name(name.clone()).create_sync() {
        Ok(listener) => Ok(Instance::Primary(Listener { listener, cleanup })),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            cleanup.0 = None;
            // Lost a race with another launch; it is the primary now.
            std::thread::sleep(Duration::from_millis(200));
            signal_show(&name).map(|()| Instance::Secondary)
        }
        Err(e) => {
            cleanup.0 = None;
            Err(e)
        }
    }
}

fn signal_show(name: &Name<'_>) -> io::Result<()> {
    let stream = ConnectOptions::new()
        .name(name.borrow())
        .wait_mode(ConnectWaitMode::Timeout(TIMEOUT))
        .connect_sync()?;
    stream.set_recv_timeout(Some(TIMEOUT))?;
    stream.set_send_timeout(Some(TIMEOUT))?;
    let mut stream = BufReader::new(stream);
    stream.get_mut().write_all(format!("{SHOW}\n").as_bytes())?;
    let mut reply = String::new();
    stream.read_line(&mut reply)?;
    if reply.trim() == OK {
        Ok(())
    } else {
        Err(io::Error::other(format!("unexpected reply {reply:?}")))
    }
}

/// Answers later launches for the rest of the process's life. Keep the
/// returned guard until exit.
pub fn serve(listener: Listener, inbox: Inbox) -> Cleanup {
    let Listener { listener, cleanup } = listener;
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
                    Err(e) => tracing::warn!("instance socket accept failed: {e}"),
                }
            }
        });
    if let Err(e) = spawned {
        tracing::error!("spawning the single-instance thread failed: {e}");
    }
    cleanup
}

fn answer(stream: LocalSocketStream, inbox: &Inbox) -> io::Result<()> {
    stream.set_recv_timeout(Some(TIMEOUT))?;
    stream.set_send_timeout(Some(TIMEOUT))?;
    let mut stream = BufReader::new(stream);
    let mut request = String::new();
    stream.read_line(&mut request)?;
    if request.trim() == SHOW {
        tracing::info!("another launch asked to show the window");
        inbox.send(Inbound::ShowRequested);
        stream.get_mut().write_all(format!("{OK}\n").as_bytes())?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use iced::futures::StreamExt;
    use interprocess::local_socket::GenericFilePath;

    use super::*;
    use crate::inbox;

    fn name_at(path: &std::path::Path) -> (Name<'static>, Cleanup) {
        let name = path
            .to_path_buf()
            .to_fs_name::<GenericFilePath>()
            .unwrap()
            .into_owned();
        (name, Cleanup(Some(path.to_path_buf())))
    }

    #[test]
    fn second_launch_signals_first() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        let (name, cleanup) = name_at(&path);
        let Instance::Primary(listener) = acquire_at(name, cleanup).unwrap() else {
            panic!("first launch must be primary");
        };
        let (inbox, rx) = inbox::channel();
        let guard = serve(listener, inbox);

        let (name, cleanup) = name_at(&path);
        assert!(matches!(
            acquire_at(name, cleanup).unwrap(),
            Instance::Secondary
        ));
        // The second launch must not remove the first one's socket.
        assert!(path.exists());
        let mut stream = rx.take_stream().unwrap();
        let received = iced::futures::executor::block_on(stream.next());
        assert!(matches!(received, Some(Inbound::ShowRequested)));

        drop(guard);
        assert!(!path.exists());
    }

    #[test]
    fn stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.sock");
        // A socket file with no listener, as left by a crash.
        drop(std::os::unix::net::UnixListener::bind(&path).unwrap());
        assert!(path.exists());
        let (name, cleanup) = name_at(&path);
        assert!(matches!(
            acquire_at(name, cleanup).unwrap(),
            Instance::Primary(_)
        ));
    }
}

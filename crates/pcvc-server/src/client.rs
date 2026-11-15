use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use pcvc_audio::AudioError;
use pcvc_protocol::{ClientUpdate, MAX_LINE_LEN, parse_client_line, plan_commands};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio_util::codec::{FramedRead, LinesCodec, LinesCodecError};

use crate::{ClientInfo, DisconnectReason, ServerEvent, Shared};

const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) async fn run(shared: Arc<Shared>, stream: TcpStream, info: ClientInfo) {
    tracing::info!(addr = %info.addr, "client connected");
    shared.clients.send_modify(|c| c.push(info.clone()));
    shared.emit(ServerEvent::ClientConnected(info.clone()));

    if let Err(e) = stream.set_nodelay(true) {
        tracing::debug!(addr = %info.addr, "set_nodelay failed: {e}");
    }
    let (read, write) = stream.into_split();
    let reason = tokio::select! {
        () = shared.cancel.cancelled() => DisconnectReason::ServerStopped,
        reason = read_loop(&shared, read) => reason,
        reason = write_loop(&shared, write) => reason,
    };

    tracing::info!(addr = %info.addr, ?reason, "client disconnected");
    shared
        .clients
        .send_modify(|c| c.retain(|c| c.id != info.id));
    shared.emit(ServerEvent::ClientDisconnected {
        client: info,
        reason,
    });
}

async fn read_loop(shared: &Shared, read: OwnedReadHalf) -> DisconnectReason {
    let mut lines = FramedRead::new(read, LinesCodec::new_with_max_length(MAX_LINE_LEN));
    loop {
        let line = match lines.next().await {
            None => return DisconnectReason::Closed,
            Some(Ok(line)) => line,
            Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                return DisconnectReason::Protocol("line too long".into());
            }
            Some(Err(LinesCodecError::Io(e))) if e.kind() == ErrorKind::InvalidData => {
                return DisconnectReason::Protocol("invalid UTF-8".into());
            }
            Some(Err(LinesCodecError::Io(e))) => return DisconnectReason::Io(e.to_string()),
        };
        match parse_client_line(&line) {
            Ok(None) => {}
            Ok(Some(update)) => apply_update(shared, update).await,
            Err(e) => return DisconnectReason::Protocol(e.to_string()),
        }
    }
}

/// Sends the current state immediately, then every newer one.
async fn write_loop(shared: &Shared, mut write: OwnedWriteHalf) -> DisconnectReason {
    let mut frames = shared.frames.subscribe();
    loop {
        let frame = frames.borrow_and_update().clone();
        if let Some(frame) = frame {
            match tokio::time::timeout(WRITE_TIMEOUT, write.write_all(frame.line.as_bytes())).await
            {
                Ok(Ok(())) => {}
                Ok(Err(e)) => return DisconnectReason::Io(e.to_string()),
                Err(_) => return DisconnectReason::WriteTimeout,
            }
        }
        // Cannot fail while `shared` (which owns the sender) is alive; shutdown
        // is signalled through the cancel token instead.
        if frames.changed().await.is_err() {
            return DisconnectReason::ServerStopped;
        }
    }
}

/// Plans against the last published state, which is what the client has seen,
/// and runs the resulting commands. The client gets no direct reply: the
/// backend's change events trigger a broadcast, and a failed command triggers a
/// resync broadcast so the client does not keep showing a rejected change.
async fn apply_update(shared: &Shared, update: ClientUpdate) {
    let current = shared.frames.borrow().clone();
    let commands = plan_commands(current.as_ref().map(|f| &f.state), &update);
    if commands.is_empty() {
        tracing::debug!(?update, "update produced no commands");
        return;
    }
    let backend = Arc::clone(&shared.backend);
    let result = tokio::task::spawn_blocking(move || {
        commands
            .iter()
            .filter_map(|command| {
                tracing::debug!(?command, "applying");
                backend.apply(command).err()
            })
            .collect::<Vec<AudioError>>()
    })
    .await;
    match result {
        Ok(errors) if errors.is_empty() => return,
        Ok(errors) => {
            for e in errors {
                tracing::warn!("client command failed: {e}");
                shared.emit(ServerEvent::BackendError(format!("command failed: {e}")));
            }
        }
        Err(e) => {
            tracing::error!("client command panicked: {e}");
            shared.emit(ServerEvent::BackendError(format!("command panicked: {e}")));
        }
    }
    shared.resync.notify_one();
}

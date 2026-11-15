//! Per-client icon request handling.
//!
//! The read loop queues each request's distinct ids as one job; a worker
//! answers the jobs in order, one provider call at a time, and hands the
//! encoded lines to the client's writer through a small bounded channel. An id
//! counts as outstanding from the moment it is queued until the writer has
//! written its reply, and a request that would take a client past
//! [`MAX_OUTSTANDING_ICON_IDS`] disconnects it. A client that stops reading
//! therefore holds at most that many ids, the worker blocks on the full line
//! channel, and the writer's timeout eventually drops the client.
//!
//! Provider calls run on blocking threads and cannot be cancelled, so a call
//! outlives a client that disconnects mid-request. [`Shared::icon_renders`]
//! caps how many run at once across all clients, so clients that connect,
//! request and disconnect in a loop cannot pile up blocking threads.

use std::convert::Infallible;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use pcvc_protocol::{
    IconId, IconReply, IconRequest, MAX_ICON_PNG_BYTES, MAX_OUTSTANDING_ICON_IDS, encode_icon_reply,
};
use tokio::sync::{Semaphore, mpsc};

use crate::{ClientInfo, DisconnectReason, ServerEvent, Shared};

/// Encoded replies the worker may have ready ahead of the writer.
const LINE_BUFFER: usize = 2;

/// Provider calls that may run at once, across all clients.
pub(crate) const MAX_CONCURRENT_RENDERS: usize = 4;

struct Job {
    ids: Vec<String>,
    size: u32,
}

/// The read loop's end of a client's icon pipeline.
pub(crate) struct Intake {
    jobs: mpsc::UnboundedSender<Job>,
}

/// The worker's end.
pub(crate) struct Worker {
    jobs: mpsc::UnboundedReceiver<Job>,
    lines: mpsc::Sender<String>,
}

/// Ids queued or being answered and not yet written, shared by a client's
/// read loop and writer. They run in one task; the atomic is only there to
/// keep the client future `Send`.
#[derive(Default)]
pub(crate) struct Outstanding(AtomicUsize);

impl Outstanding {
    /// Called by the writer after writing a reply line.
    pub(crate) fn written(&self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Creates a client's pipeline. The receiver yields encoded reply lines for
/// the writer.
pub(crate) fn pipeline() -> (Intake, Worker, mpsc::Receiver<String>) {
    // Every queued job holds at least one outstanding id, so the outstanding
    // limit bounds this channel.
    let (jobs_tx, jobs_rx) = mpsc::unbounded_channel();
    let (lines_tx, lines_rx) = mpsc::channel(LINE_BUFFER);
    (
        Intake { jobs: jobs_tx },
        Worker {
            jobs: jobs_rx,
            lines: lines_tx,
        },
        lines_rx,
    )
}

impl Intake {
    /// Queues a request's distinct ids, or returns why the client must be
    /// disconnected.
    pub(crate) fn queue(
        &self,
        shared: &Shared,
        client: &ClientInfo,
        outstanding: &Outstanding,
        request: &IconRequest,
    ) -> Result<(), DisconnectReason> {
        let ids: Vec<String> = request
            .unique_ids()
            .into_iter()
            .map(str::to_owned)
            .collect();
        if ids.is_empty() {
            return Ok(());
        }
        let size = request.size_px();
        let total = outstanding.0.load(Ordering::Relaxed) + ids.len();
        if total > MAX_OUTSTANDING_ICON_IDS {
            return Err(DisconnectReason::Protocol(format!(
                "{total} icon ids outstanding, at most {MAX_OUTSTANDING_ICON_IDS} are allowed"
            )));
        }
        tracing::info!(addr = %client.addr, count = ids.len(), size, "icon request");
        shared.emit(ServerEvent::IconsRequested {
            client: client.clone(),
            count: ids.len(),
            size,
        });
        outstanding.0.fetch_add(ids.len(), Ordering::Relaxed);
        // Fails only once the worker is gone, while the client is torn down.
        let _ = self.jobs.send(Job { ids, size });
        Ok(())
    }
}

impl Worker {
    /// Answers queued jobs in order until the client goes away, which ends it
    /// by dropping this future.
    pub(crate) async fn run(mut self, shared: &Shared) -> Infallible {
        'jobs: while let Some(job) = self.jobs.recv().await {
            for id in job.ids {
                let line = reply_line(shared, id, job.size).await;
                if self.lines.send(line).await.is_err() {
                    break 'jobs;
                }
            }
        }
        // Only reachable while the client is being torn down.
        std::future::pending().await
    }
}

/// The encoded reply for one id. The provider call and the base64 encoding
/// run on a blocking thread.
async fn reply_line(shared: &Shared, id: String, size: u32) -> String {
    let (Some(icons), Ok(icon_id)) = (shared.icons.clone(), IconId::new(id.as_str())) else {
        return encode_icon_reply(IconReply::not_found(id));
    };
    let Ok(permit) = Arc::clone(&shared.icon_renders).acquire_owned().await else {
        return encode_icon_reply(IconReply::not_found(id));
    };
    let fallback = id.clone();
    let line = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let reply = match icons.icon_png(&icon_id, size) {
            Some(image) => {
                let reply = IconReply::image(id, image.size, &image.png);
                if matches!(reply, IconReply::Error { .. }) {
                    tracing::warn!(
                        %icon_id,
                        bytes = image.png.len(),
                        "icon exceeds {MAX_ICON_PNG_BYTES} bytes, answering notFound"
                    );
                }
                reply
            }
            None => IconReply::not_found(id),
        };
        encode_icon_reply(reply)
    })
    .await;
    line.unwrap_or_else(|e| {
        tracing::error!(id = %fallback, "icon provider panicked in icon_png: {e}");
        encode_icon_reply(IconReply::not_found(fallback))
    })
}

/// The semaphore behind [`Shared::icon_renders`].
pub(crate) fn render_limit() -> Arc<Semaphore> {
    Arc::new(Semaphore::new(MAX_CONCURRENT_RENDERS))
}

//! Tracing setup: stderr, a daily rolling file and an in-memory ring buffer
//! that feeds the Status view.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use time::{OffsetDateTime, UtcOffset};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, filter};

const DEFAULT_FILTER: &str = "info,iced_winit=warn,iced_wgpu=warn,wgpu_core=error,wgpu_hal=error,naga=warn,sctk_adwaita=error";
static LOCAL_OFFSET: OnceLock<UtcOffset> = OnceLock::new();

/// The local UTC offset as read at startup (UTC if it could not be read).
pub fn local_offset() -> UtcOffset {
    LOCAL_OFFSET.get().copied().unwrap_or(UtcOffset::UTC)
}

const RING_CAPACITY: usize = 200;
const LOG_FILES_KEPT: usize = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub time: OffsetDateTime,
    pub level: Level,
    pub message: String,
}

impl LogLine {
    /// For example "14:03:27 WARN mDNS registration failed".
    pub fn display(&self) -> String {
        let (h, m, s) = self.time.to_hms();
        format!("{h:02}:{m:02}:{s:02} {:<5} {}", self.level, self.message)
    }
}

/// The most recent log lines, oldest first.
#[derive(Debug, Clone)]
pub struct LogBuffer {
    lines: Arc<Mutex<VecDeque<LogLine>>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            lines: Arc::new(Mutex::new(VecDeque::with_capacity(capacity))),
            capacity,
        }
    }

    pub fn push(&self, line: LogLine) {
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        if lines.len() == self.capacity {
            lines.pop_front();
        }
        lines.push_back(line);
    }

    /// Up to `n` of the newest lines, oldest first.
    pub fn recent(&self, n: usize) -> Vec<LogLine> {
        let lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        lines
            .iter()
            .skip(lines.len().saturating_sub(n))
            .cloned()
            .collect()
    }
}

struct RingLayer {
    buffer: LogBuffer,
    offset: UtcOffset,
}

impl<S: Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        self.buffer.push(LogLine {
            time: OffsetDateTime::now_utc().to_offset(self.offset),
            level: *event.metadata().level(),
            message: visitor.finish(),
        });
    }
}

/// Renders an event as its message followed by `key=value` fields.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
}

impl MessageVisitor {
    fn finish(self) -> String {
        match (self.message.is_empty(), self.fields.is_empty()) {
            (_, true) => self.message,
            (true, false) => self.fields,
            (false, false) => format!("{} {}", self.message, self.fields),
        }
    }
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message.push_str(value);
        } else {
            self.record_debug(field, &format_args!("{value}"));
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.message, "{value:?}");
        } else {
            if !self.fields.is_empty() {
                self.fields.push(' ');
            }
            let _ = write!(self.fields, "{}={value:?}", field.name());
        }
    }
}

/// Keeps the file writer flushing until dropped at the end of `main`.
pub struct LogGuard(#[allow(dead_code)] Option<WorkerGuard>);

/// Installs the global subscriber. Must run before any other thread starts:
/// the local UTC offset can only be read safely while single-threaded.
pub fn init(log_dir: Option<&Path>) -> (LogBuffer, LogGuard) {
    let offset =
        *LOCAL_OFFSET.get_or_init(|| UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC));
    let buffer = LogBuffer::new(RING_CAPACITY);
    let env_filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    // The ring buffer is for the user: only this app's own crates, info and up.
    let ring = RingLayer {
        buffer: buffer.clone(),
        offset,
    }
    .with_filter(filter::filter_fn(|meta| {
        *meta.level() <= Level::INFO
            && (meta.target().starts_with("pcvolumecontrol") || meta.target().starts_with("pcvc_"))
    }));

    let mut file_error = None;
    let (file_layer, guard) = match log_dir.map(open_log_file) {
        Some(Ok(appender)) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let layer = tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(writer)
                .with_filter(env_filter());
            (Some(layer), Some(guard))
        }
        Some(Err(e)) => {
            file_error = Some(e);
            (None, None)
        }
        None => (None, None),
    };

    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                .with_filter(env_filter()),
        )
        .with(file_layer)
        .with(ring)
        .init();

    if let Some(e) = file_error {
        tracing::warn!("file logging disabled: {e}");
    }
    (buffer, LogGuard(guard))
}

fn open_log_file(dir: &Path) -> Result<RollingFileAppender, String> {
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("pcvolumecontrol")
        .filename_suffix("log")
        .max_log_files(LOG_FILES_KEPT)
        .build(dir)
        .map_err(|e| format!("{}: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(message: &str) -> LogLine {
        LogLine {
            time: OffsetDateTime::UNIX_EPOCH,
            level: Level::INFO,
            message: message.to_owned(),
        }
    }

    #[test]
    fn ring_keeps_the_newest_lines() {
        let buffer = LogBuffer::new(3);
        for i in 0..5 {
            buffer.push(line(&i.to_string()));
        }
        let messages =
            |n| -> Vec<String> { buffer.recent(n).into_iter().map(|l| l.message).collect() };
        assert_eq!(messages(10), ["2", "3", "4"]);
        assert_eq!(messages(2), ["3", "4"]);
        assert!(messages(0).is_empty());
    }

    #[test]
    fn layer_records_message_and_fields() {
        let buffer = LogBuffer::new(10);
        let subscriber = tracing_subscriber::registry().with(RingLayer {
            buffer: buffer.clone(),
            offset: UtcOffset::UTC,
        });
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(addr = "1.2.3.4:5", "client connected");
            tracing::info!("plain");
        });
        let lines = buffer.recent(10);
        assert_eq!(lines[0].message, "client connected addr=1.2.3.4:5");
        assert_eq!(lines[0].level, Level::WARN);
        assert_eq!(lines[1].message, "plain");
    }

    #[test]
    fn display_format() {
        assert_eq!(line("hello").display(), "00:00:00 INFO  hello");
    }
}

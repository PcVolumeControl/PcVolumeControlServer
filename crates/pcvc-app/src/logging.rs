//! Tracing setup: stderr, a daily rolling file and an in-memory ring buffer
//! that feeds the Status view.

use std::collections::VecDeque;
use std::fmt::Write as _;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use chrono::{DateTime, Local};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::filter::{self, LevelFilter};
use tracing_subscriber::fmt::time::ChronoLocal;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

const DEFAULT_FILTER: &str = "info,iced_winit=warn,iced_wgpu=warn,wgpu_core=error,wgpu_hal=error,naga=warn,sctk_adwaita=error";
const RING_CAPACITY: usize = 200;
const LOG_FILES_KEPT: usize = 7;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogLine {
    pub time: DateTime<Local>,
    pub level: Level,
    pub message: String,
}

impl LogLine {
    /// For example "14:03:27 WARN mDNS registration failed".
    pub fn display(&self) -> String {
        format!(
            "{} {:<5} {}",
            crate::model::clock(&self.time),
            self.level,
            self.message
        )
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
        if self.capacity == 0 {
            return;
        }
        let mut lines = self.lines.lock().unwrap_or_else(PoisonError::into_inner);
        while lines.len() >= self.capacity {
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
}

/// A layer recording every event into `buffer`, without filtering.
#[cfg(test)]
pub(crate) fn ring_layer<S: Subscriber>(buffer: LogBuffer) -> impl Layer<S> {
    RingLayer { buffer }
}

impl<S: Subscriber> Layer<S> for RingLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        // One visual row per line in the Status view.
        let message = visitor.finish().replace(['\r', '\n'], " ");
        self.buffer.push(LogLine {
            time: Local::now(),
            level: *event.metadata().level(),
            message,
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

/// Installs the global subscriber. Timestamps are local time with offset;
/// log files still roll over at midnight UTC.
pub fn init(log_dir: Option<&Path>) -> (LogBuffer, LogGuard) {
    let buffer = LogBuffer::new(RING_CAPACITY);
    let env_filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_FILTER));

    // The ring buffer is for the user: only this app's own crates, info and up.
    let ring = RingLayer {
        buffer: buffer.clone(),
    }
    .with_filter(
        filter::filter_fn(|meta| {
            *meta.level() <= Level::INFO
                && (meta.target().starts_with("pcvolumecontrol")
                    || meta.target().starts_with("pcvc_"))
        })
        .with_max_level_hint(LevelFilter::INFO),
    );

    let mut file_error = None;
    let (file_layer, guard) = match log_dir.map(open_log_file) {
        Some(Ok(appender)) => {
            let (writer, guard) = tracing_appender::non_blocking(appender);
            let layer = tracing_subscriber::fmt::layer()
                .with_timer(ChronoLocal::rfc_3339())
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
                .with_timer(ChronoLocal::rfc_3339())
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
    // The appender prunes old files before it creates the directory, and
    // reports a missing directory on stderr.
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
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
            time: Local::now(),
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
        });
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!(addr = "1.2.3.4:5", "client connected");
            tracing::info!("plain");
            tracing::info!("two\nlines");
        });
        let lines = buffer.recent(10);
        assert_eq!(lines[0].message, "client connected addr=1.2.3.4:5");
        assert_eq!(lines[0].level, Level::WARN);
        assert_eq!(lines[1].message, "plain");
        assert_eq!(lines[2].message, "two lines");
    }

    #[test]
    fn zero_capacity_keeps_nothing() {
        let buffer = LogBuffer::new(0);
        buffer.push(line("x"));
        assert!(buffer.recent(10).is_empty());
    }

    #[test]
    fn display_format() {
        assert!(line("hello").display().ends_with(" INFO  hello"));
    }
}

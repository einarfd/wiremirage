use chrono::{DateTime, Utc};

/// Captured log line emitted by a handler. Accumulated per-request and
/// drained into the journal record after the handler returns. The
/// `timestamp` is stamped at `LogCapture::push` so the journal entry
/// preserves emit order and per-line timing even when the surrounding
/// dispatch span doesn't.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    pub level: LogLevel,
    pub message: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    /// Lowercase string form used in serialized journal records.
    pub fn as_str(&self) -> &'static str {
        match self {
            LogLevel::Debug => "debug",
            LogLevel::Info => "info",
            LogLevel::Warn => "warn",
            LogLevel::Error => "error",
        }
    }
}

#[derive(Debug, Default)]
pub struct LogCapture {
    records: Vec<LogRecord>,
}

impl LogCapture {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append a new record, stamping it with the current UTC time. Used
    /// from the WIT `log.emit` host-function impl so the journal entry
    /// captures emit order plus a coarse per-line timestamp.
    pub fn push_now(&mut self, level: LogLevel, message: String) {
        self.records.push(LogRecord {
            level,
            message,
            timestamp: Utc::now(),
        });
    }

    /// Direct push for tests / callers that want to control the
    /// timestamp explicitly. Production code paths should use
    /// `push_now`.
    pub fn push(&mut self, record: LogRecord) {
        self.records.push(record);
    }

    pub fn records(&self) -> &[LogRecord] {
        &self.records
    }

    pub fn take(&mut self) -> Vec<LogRecord> {
        std::mem::take(&mut self.records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_records_in_order() {
        let mut c = LogCapture::new();
        c.push_now(LogLevel::Info, "first".into());
        c.push_now(LogLevel::Warn, "second".into());
        assert_eq!(c.records().len(), 2);
        assert_eq!(c.records()[0].level, LogLevel::Info);
        assert_eq!(c.records()[1].message, "second");
    }

    #[test]
    fn take_drains_capture() {
        let mut c = LogCapture::new();
        c.push_now(LogLevel::Error, "oops".into());
        let drained = c.take();
        assert_eq!(drained.len(), 1);
        assert!(c.records().is_empty());
    }
}

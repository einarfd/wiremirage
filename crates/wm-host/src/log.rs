/// Captured log line emitted by a handler. In slice 1 these accumulate in
/// memory; in later slices they are journalled to Valkey and optionally
/// forwarded to host stderr per `logs.forward_handler_logs` config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogRecord {
    pub level: LogLevel,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Default)]
pub struct LogCapture {
    records: Vec<LogRecord>,
}

impl LogCapture {
    pub fn new() -> Self {
        Self::default()
    }

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
        c.push(LogRecord {
            level: LogLevel::Info,
            message: "first".into(),
        });
        c.push(LogRecord {
            level: LogLevel::Warn,
            message: "second".into(),
        });
        assert_eq!(c.records().len(), 2);
        assert_eq!(c.records()[0].level, LogLevel::Info);
        assert_eq!(c.records()[1].message, "second");
    }

    #[test]
    fn take_drains_capture() {
        let mut c = LogCapture::new();
        c.push(LogRecord {
            level: LogLevel::Error,
            message: "oops".into(),
        });
        let drained = c.take();
        assert_eq!(drained.len(), 1);
        assert!(c.records().is_empty());
    }
}

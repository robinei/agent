use std::cell::Cell;
use std::io::Write;
use std::sync::Mutex;

use log::{LevelFilter, Log, Metadata, Record};

thread_local! {
    /// Tree ID set at the start of each agent thread for log correlation.
    pub static AGENT_TREE_ID: Cell<Option<String>> = const { Cell::new(None) };
}

/// A logger that writes to a file with timestamps and optional tree-id prefix.
pub struct FileLogger {
    file: Mutex<std::fs::File>,
    level: LevelFilter,
}

impl FileLogger {
    pub fn new(path: &str, level: LevelFilter) -> Result<Self, std::io::Error> {
        // Create parent dirs if needed
        if let Some(parent) = std::path::Path::new(path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        Ok(Self {
            file: Mutex::new(file),
            level,
        })
    }
}

impl Log for FileLogger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let mut f = match self.file.lock() {
            Ok(f) => f,
            Err(_) => return,
        };
        let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ");
        let tree_prefix = AGENT_TREE_ID.with(|id| {
            id.take().map(|tid| {
                id.set(Some(tid.clone()));
                format!(" tree={}", tid)
            })
        });
        match tree_prefix {
            Some(tp) => {
                let _ = writeln!(f, "[{} {} {}]{}", now, record.level(), record.args(), tp);
            }
            None => {
                let _ = writeln!(f, "[{} {} {}]", now, record.level(), record.args());
            }
        }
    }

    fn flush(&self) {
        if let Ok(mut f) = self.file.lock() {
            let _ = f.flush();
        }
    }
}

/// Initialize logging. Registers `env_logger` for stderr (when `to_stderr` is
/// true) and an optional `FileLogger` for the given file path.
pub fn init_logging(log_file: Option<&str>, level: &str, to_stderr: bool) {
    let filter = match level.to_lowercase().as_str() {
        "error" => LevelFilter::Error,
        "warn" => LevelFilter::Warn,
        "info" => LevelFilter::Info,
        "debug" => LevelFilter::Debug,
        "trace" => LevelFilter::Trace,
        _ => LevelFilter::Info,
    };

    if to_stderr {
        let _ = env_logger::Builder::from_env(
            env_logger::Env::default().default_filter_or(level),
        )
        .try_init();
    }

    if let Some(path) = log_file {
        if let Ok(file_logger) = FileLogger::new(path, filter) {
            let _ = log::set_boxed_logger(Box::new(file_logger));
            log::set_max_level(filter);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_file_logger_writes() {
        let dir = std::env::temp_dir().join("agent-log-test");
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("test.log");
        let path_str = path.to_str().unwrap();

        let logger = FileLogger::new(path_str, LevelFilter::Info).unwrap();
        logger.log(
            &Record::builder()
                .args(format_args!("hello world"))
                .level(log::Level::Info)
                .target("test")
                .build(),
        );

        let contents = fs::read_to_string(&path).unwrap();
        assert!(contents.contains("hello world"));
        let _ = fs::remove_dir_all(&dir);
    }
}

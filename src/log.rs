use std::fmt::Arguments;
use std::fs::{File, OpenOptions};
use std::io::{Write, stderr};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use chrono::Local;

/// Log levels ordered by severity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Debug = 0,
    Info = 1,
    Warning = 2,
    Error = 3,
    Fatal = 4,
}

impl LogLevel {
    pub fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(LogLevel::Debug),
            1 => Some(LogLevel::Info),
            2 => Some(LogLevel::Warning),
            3 => Some(LogLevel::Error),
            4 => Some(LogLevel::Fatal),
            _ => None,
        }
    }

    pub fn as_char(self) -> char {
        match self {
            LogLevel::Debug => 'D',
            LogLevel::Info => 'I',
            LogLevel::Warning => 'W',
            LogLevel::Error => 'E',
            LogLevel::Fatal => 'F',
        }
    }

    /// ANSI-colored single-char tag for terminal output.
    pub fn colored_tag(self) -> &'static str {
        match self {
            LogLevel::Debug => "D",
            LogLevel::Info => "\x1b[32mI\x1b[0m",
            LogLevel::Warning => "\x1b[33mW\x1b[0m",
            LogLevel::Error => "\x1b[31mE\x1b[0m",
            LogLevel::Fatal => "\x1b[93mF\x1b[0m",
        }
    }
}

#[derive(Debug)]
struct SharedLogger {
    level: LogLevel,
    file: Mutex<Option<File>>,
}

static LOGGER: OnceLock<Arc<SharedLogger>> = OnceLock::new();

/// Check whether the given level would produce output.
/// Returns `false` if the logger is not yet initialized or the level is below threshold.
pub fn should_log(level: LogLevel) -> bool {
    LOGGER
        .get()
        .map(|l| level >= l.level)
        .unwrap_or(false)
}

/// Initialize the global logger. Must be called once at program start.
pub fn init(level: LogLevel, log_file: Option<&Path>) -> std::io::Result<()> {
    let file = match log_file {
        Some(path) => {
            let f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            Some(f)
        }
        None => None,
    };

    let logger = Arc::new(SharedLogger {
        level,
        file: Mutex::new(file),
    });

    LOGGER
        .set(logger)
        .expect("Logger::init called more than once");

    Ok(())
}

/// Write a log message at the given level.
pub fn log(level: LogLevel, args: &Arguments<'_>) {
    let logger = LOGGER.get().expect("Logger not initialized");
    if level < logger.level {
        return;
    }

    let timestamp = Local::now().format("%Y-%m-%d %H:%M:%S");

    // Terminal output: colored single-char tag
    let term_msg = format!("[{}] [{}] {}", timestamp, level.colored_tag(), args);
    let _ = writeln!(stderr(), "{}", term_msg);

    // File output: plain single-char tag
    if let Ok(mut guard) = logger.file.lock() {
        if let Some(ref mut writer) = *guard {
            let file_msg = format!("[{}] [{}] {}", timestamp, level.as_char(), args);
            let _ = writeln!(writer, "{}", file_msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_level_from_u8() {
        assert_eq!(LogLevel::from_u8(0), Some(LogLevel::Debug));
        assert_eq!(LogLevel::from_u8(1), Some(LogLevel::Info));
        assert_eq!(LogLevel::from_u8(2), Some(LogLevel::Warning));
        assert_eq!(LogLevel::from_u8(3), Some(LogLevel::Error));
        assert_eq!(LogLevel::from_u8(4), Some(LogLevel::Fatal));
        assert_eq!(LogLevel::from_u8(5), None);
        assert_eq!(LogLevel::from_u8(255), None);
    }

    #[test]
    fn test_log_level_ordering() {
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warning);
        assert!(LogLevel::Warning < LogLevel::Error);
        assert!(LogLevel::Error < LogLevel::Fatal);
        assert!(LogLevel::Debug < LogLevel::Fatal);
    }

    #[test]
    fn test_log_level_chars() {
        assert_eq!(LogLevel::Debug.as_char(), 'D');
        assert_eq!(LogLevel::Info.as_char(), 'I');
        assert_eq!(LogLevel::Warning.as_char(), 'W');
        assert_eq!(LogLevel::Error.as_char(), 'E');
        assert_eq!(LogLevel::Fatal.as_char(), 'F');
    }

    #[test]
    fn test_log_level_colored_tags() {
        assert_eq!(LogLevel::Debug.colored_tag(), "D");
        assert_eq!(LogLevel::Info.colored_tag(), "\x1b[32mI\x1b[0m");
        assert_eq!(LogLevel::Warning.colored_tag(), "\x1b[33mW\x1b[0m");
        assert_eq!(LogLevel::Error.colored_tag(), "\x1b[31mE\x1b[0m");
        assert_eq!(LogLevel::Fatal.colored_tag(), "\x1b[93mF\x1b[0m");
    }

    #[test]
    #[should_panic(expected = "Logger::init called more than once")]
    fn test_init_panics_on_double_init() {
        let _ = init(LogLevel::Debug, None);
        init(LogLevel::Debug, None).unwrap();
    }
}

#[macro_export]
macro_rules! debug {
    ($($arg:tt)*) => {
        if $crate::log::should_log($crate::log::LogLevel::Debug) {
            $crate::log::log($crate::log::LogLevel::Debug, &format_args!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        if $crate::log::should_log($crate::log::LogLevel::Info) {
            $crate::log::log($crate::log::LogLevel::Info, &format_args!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! warn_log {
    ($($arg:tt)*) => {
        if $crate::log::should_log($crate::log::LogLevel::Warning) {
            $crate::log::log($crate::log::LogLevel::Warning, &format_args!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! error {
    ($($arg:tt)*) => {
        if $crate::log::should_log($crate::log::LogLevel::Error) {
            $crate::log::log($crate::log::LogLevel::Error, &format_args!($($arg)*));
        }
    };
}

#[macro_export]
macro_rules! fatal {
    ($($arg:tt)*) => {{
        $crate::log::log($crate::log::LogLevel::Fatal, &format_args!($($arg)*));
        std::process::exit(1);
    }};
}

//! 极简分级日志:stdout 是唯一协议通道,所有日志走 stderr;
//! 设置 `WT_LOG_FILE` 时同时落盘(打开失败静默降级,不影响服务)。
//!
//! 级别由 `WT_LOG_LEVEL` 控制(error|warn|info|debug),默认 warn:
//! - warn/error — 服务异常、请求失败(默认可见,无成本);
//! - info — 每请求一行(方法/工具、耗时、结果码),排障时开启;
//! - debug — 开发期细节。

use std::fmt;
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

pub const LEVEL_ERROR: u8 = 1;
pub const LEVEL_WARN: u8 = 2;
pub const LEVEL_INFO: u8 = 3;
pub const LEVEL_DEBUG: u8 = 4;

static MAX_LEVEL: AtomicU8 = AtomicU8::new(LEVEL_WARN);
static STARTED_AT: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
static FILE_TARGET: Mutex<Option<std::fs::File>> = Mutex::new(None);

fn started_at() -> &'static std::time::Instant {
    STARTED_AT.get_or_init(std::time::Instant::now)
}

fn level_name(level: u8) -> &'static str {
    match level {
        LEVEL_ERROR => "ERROR",
        LEVEL_WARN => "WARN ",
        LEVEL_INFO => "INFO ",
        _ => "DEBUG",
    }
}

fn parse_level(raw: &str) -> u8 {
    match raw.trim().to_ascii_lowercase().as_str() {
        "error" => LEVEL_ERROR,
        "warn" | "warning" => LEVEL_WARN,
        "info" => LEVEL_INFO,
        "debug" | "trace" => LEVEL_DEBUG,
        _ => LEVEL_WARN,
    }
}

/// 从环境读取日志配置;进程启动时调用一次。任何配置错误都回退默认值。
pub fn init_from_env() {
    if let Ok(level) = std::env::var("WT_LOG_LEVEL") {
        MAX_LEVEL.store(parse_level(&level), Ordering::Relaxed);
    }
    if let Ok(path) = std::env::var("WT_LOG_FILE") {
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
        {
            *FILE_TARGET.lock().unwrap_or_else(|e| e.into_inner()) = Some(file);
        }
    }
}

/// 级别过滤判断;测试用。
pub fn enabled(level: u8) -> bool {
    level <= MAX_LEVEL.load(Ordering::Relaxed)
}

/// 写一条日志到 stderr(及可选文件)。失败静默:日志绝不能拖垮服务。
pub fn log(level: u8, args: fmt::Arguments<'_>) {
    if !enabled(level) {
        return;
    }
    let elapsed = started_at().elapsed();
    let line = format!(
        "[{:8}.{:03}] {} {}",
        elapsed.as_secs(),
        elapsed.subsec_millis(),
        level_name(level),
        args
    );
    let mut target = FILE_TARGET.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(file) = target.as_mut() {
        let _ = writeln!(file, "{line}");
        let _ = file.flush();
    }
    let _ = writeln!(std::io::stderr(), "{line}");
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => { $crate::log::log($crate::log::LEVEL_ERROR, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => { $crate::log::log($crate::log::LEVEL_WARN, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => { $crate::log::log($crate::log::LEVEL_INFO, format_args!($($arg)*)) };
}

#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => { $crate::log::log($crate::log::LEVEL_DEBUG, format_args!($($arg)*)) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_level_names_case_insensitively() {
        assert_eq!(parse_level("error"), LEVEL_ERROR);
        assert_eq!(parse_level("WARN"), LEVEL_WARN);
        assert_eq!(parse_level("Info"), LEVEL_INFO);
        assert_eq!(parse_level("DEBUG"), LEVEL_DEBUG);
        assert_eq!(parse_level("trace"), LEVEL_DEBUG);
        assert_eq!(parse_level("bogus"), LEVEL_WARN);
    }

    #[test]
    fn enabled_follows_max_level() {
        MAX_LEVEL.store(LEVEL_INFO, Ordering::Relaxed);
        assert!(enabled(LEVEL_ERROR));
        assert!(enabled(LEVEL_WARN));
        assert!(enabled(LEVEL_INFO));
        assert!(!enabled(LEVEL_DEBUG));
        MAX_LEVEL.store(LEVEL_WARN, Ordering::Relaxed);
        assert!(!enabled(LEVEL_INFO));
    }
}

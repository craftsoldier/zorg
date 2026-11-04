#[cfg(any(
    all(feature = "standalone", feature = "packaged"),
    not(any(feature = "standalone", feature = "packaged"))
))]
compile_error!("Enable exactly one of standalone (default) or packaged; use --no-default-features --features packaged for the packaged build.");

#[cfg(not(target_os = "macos"))]
compile_error!("zorg runs only on macOS: recovery phrases require macOS authentication.");

pub mod account;
pub(crate) mod db;
pub mod keys;
pub mod network;
pub mod secret_store;
pub mod sync;
pub mod sync_engine;
pub(crate) mod wallet_summary_cache;

struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record<'_>) {
        if self.enabled(record.metadata()) {
            eprintln!("{}: {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

static LOGGER: StderrLogger = StderrLogger;

/// Install stderr logging.
pub fn init() {
    #[cfg(debug_assertions)]
    let configured_level = std::env::var("RUST_LOG")
        .ok()
        .and_then(|value| {
            let level = value
                .rsplit_once('=')
                .map_or(value.as_str(), |(_, level)| level);
            parse_log_level(level)
        })
        .unwrap_or(log::LevelFilter::Warn);
    #[cfg(not(debug_assertions))]
    let configured_level = log::LevelFilter::Warn;
    if log::set_logger(&LOGGER).is_ok() {
        log::set_max_level(configured_level);
    }
}

#[cfg(debug_assertions)]
fn parse_log_level(value: &str) -> Option<log::LevelFilter> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Some(log::LevelFilter::Off),
        "error" => Some(log::LevelFilter::Error),
        "warn" | "warning" => Some(log::LevelFilter::Warn),
        "info" => Some(log::LevelFilter::Info),
        "debug" => Some(log::LevelFilter::Debug),
        "trace" => Some(log::LevelFilter::Trace),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{init, LOGGER};

    #[test]
    fn init_registers_the_stderr_logger() {
        init();
        let registered = log::logger() as *const dyn log::Log as *const ();
        let expected = &LOGGER as *const super::StderrLogger as *const ();
        assert_eq!(registered, expected);
    }
}

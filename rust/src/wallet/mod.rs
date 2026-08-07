pub(crate) mod db;
pub mod keys;
pub mod network;
pub mod secret_payload;
pub mod secret_store;
pub mod sync;
pub mod sync_engine;
pub(crate) mod wallet_summary_cache;

/// Install the rustls crypto provider and set log level.
/// Call once before any TLS connection (lightwalletd sync).
pub fn init() {
    log::set_max_level(log::LevelFilter::Info);
    let _ = rustls::crypto::ring::default_provider().install_default();
}

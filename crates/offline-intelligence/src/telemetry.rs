// Offline_Intelligence/_Server/telemetry.rs




use tracing_subscriber::{fmt, EnvFilter};

pub fn init_tracing() {
    let env_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());

    let builder = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(env_filter))
        .with_timer(fmt::time::UtcTime::rfc_3339())
        .with_target(true)
        .with_level(true)
        .compact();

    // A release build on Windows has no console, so stderr goes nowhere:
    // write to logs/backend.log as well, or a startup failure leaves no trace.
    let _ = match open_log_file() {
        Some(file) => tracing::subscriber::set_global_default(
            builder.with_ansi(false).with_writer(std::sync::Mutex::new(file)).finish(),
        ),
        None => tracing::subscriber::set_global_default(builder.finish()),
    };
}

/// logs/backend.log in the app data directory, started afresh once it passes
/// 10 MB so it never grows without bound.
fn open_log_file() -> Option<std::fs::File> {
    let dir = crate::config::get_app_data_dir().join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("backend.log");
    let too_big = std::fs::metadata(&path).map_or(false, |m| m.len() > 10 * 1024 * 1024);
    std::fs::OpenOptions::new()
        .create(true)
        .append(!too_big)
        .write(true)
        .truncate(too_big)
        .open(path)
        .ok()
}

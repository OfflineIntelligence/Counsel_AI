// Offline_Intelligence/_Server/telemetry.rs




use tracing_subscriber::{fmt, EnvFilter};

pub fn init_tracing() {
    let env_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());

    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(env_filter))
        .with_timer(fmt::time::UtcTime::rfc_3339())
        .with_target(true)
        .with_level(true)
        .compact()
        .finish();

    let _ = tracing::subscriber::set_global_default(subscriber);
}

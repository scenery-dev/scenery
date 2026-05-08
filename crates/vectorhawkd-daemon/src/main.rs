//! `vectorhawkd` — VectorHawk runner daemon binary.
//!
//! Thin wrapper. All logic lives in `vectorhawkd_daemon::run_daemon` so the
//! user CLI's `vectorhawk daemon run --foreground` subcommand can call the
//! same entry point without re-execing this binary.

use vectorhawkd_daemon::{run_daemon, DaemonOpts};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let opts = DaemonOpts {
        registry_url: std::env::var("VECTORHAWK_REGISTRY_URL")
            .ok()
            .or_else(|| std::env::var("SKILLCLUB_REGISTRY_URL").ok()),
        socket_path_override: None,
        ollama_url: std::env::var("VECTORHAWK_OLLAMA_URL").ok(),
        ollama_model: std::env::var("VECTORHAWK_OLLAMA_MODEL").ok(),
    };

    if let Err(e) = run_daemon(opts).await {
        tracing::error!(error = %e, "daemon exited with error");
        std::process::exit(1);
    }
}

//! VectorHawk runner — shim binary entry point.
//!
//! This is a thin wrapper: initialise tracing, then delegate to
//! [`vectorhawkd_shim::run_shim`]. All logic lives in the library so that
//! `vectorhawk mcp serve` (Stream 3 — CLI) can reuse the same entry point
//! without depending on this binary.

#[tokio::main(flavor = "current_thread")]
async fn main() {
    // Tracing to stderr only; stdout is the MCP wire.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_env("VECTORHAWK_LOG")
                .add_directive(tracing::Level::WARN.into()),
        )
        .init();

    if let Err(e) = vectorhawkd_shim::run_shim(None).await {
        tracing::error!(error = %e, "shim exited with error");
        std::process::exit(1);
    }
}

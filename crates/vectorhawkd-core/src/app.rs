use crate::state::AppState;
use anyhow::Result;

/// Top-level application handle. Holds bootstrapped state.
///
/// Both the daemon and CLI construct this via `VectorHawkApp::bootstrap()`.
/// The daemon then passes `app.state` into its socket listener and service
/// components.
pub struct VectorHawkApp {
    pub state: AppState,
}

impl VectorHawkApp {
    /// Bootstrap the application: resolve data directories, create if missing,
    /// open (or create) the SQLite state database, apply schema migrations.
    pub fn bootstrap() -> Result<Self> {
        let state = AppState::bootstrap()?;
        Ok(Self { state })
    }
}

//! The embedding app's own update (cast-b3k): the app checks for a newer
//! signed release and downloads it in the background; the home says one is
//! ready (`GET /settings` `update_ready`) and installs it on request
//! (`POST /settings/update`, the host machine's own page only).

use std::fmt;
use std::sync::Arc;

/// What the app embedding the host tells it about its update.
#[derive(Clone)]
pub struct AppUpdate {
    /// The version downloaded and ready to install, if any.
    pub ready: Arc<dyn Fn() -> Option<String> + Send + Sync>,
    /// Install it and restart the app: refused (with why) while a call is
    /// on, or when nothing is ready. It answers before the app restarts.
    pub install: Arc<dyn Fn() -> Result<(), String> + Send + Sync>,
}

impl fmt::Debug for AppUpdate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AppUpdate(..)")
    }
}

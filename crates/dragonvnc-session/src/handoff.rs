//! Write side of the handoff protocol: run *inside* the spawned sway
//! session (as its overlay config's `exec dragonvnc-server session-ready`
//! line) to report the socket names sway picked for itself back to the
//! launcher.
//!
//! sway only exports `WAYLAND_DISPLAY`/`SWAYSOCK` into the environment of
//! processes it `exec`s *after* it has started — they never appear in
//! `/proc/<sway-pid>/environ`, so this is the only way the launcher learns
//! them. This doubles as the launcher's "compositor finished starting and
//! ran its config" signal.

use crate::{Handoff, HANDOFF_ENV};
use tokio::net::UnixStream;

#[derive(Debug, thiserror::Error)]
pub enum HandoffError {
    #[error(
        "{HANDOFF_ENV} is not set — this subcommand must be run as `exec` from inside a \
         dragonvnc-launched sway session (via the overlay config), not invoked directly"
    )]
    NotInSession,
    #[error("{0} is not set in this process's environment — is this really running inside sway?")]
    MissingEnvVar(&'static str),
    #[error("failed to connect to handoff socket {path}: {source}")]
    Connect {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("io error writing handoff: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to send handoff: {0}")]
    Send(#[from] crate::SessionError),
}

/// Reads `WAYLAND_DISPLAY`/`SWAYSOCK` from the environment and reports them
/// to the launcher waiting on `$DRAGONVNC_HANDOFF`, then returns — the
/// caller (the `session-ready` CLI subcommand) should exit immediately
/// after, its job is done.
pub async fn send_ready() -> Result<(), HandoffError> {
    let sock_path = std::env::var(HANDOFF_ENV).map_err(|_| HandoffError::NotInSession)?;
    let wayland_display =
        std::env::var("WAYLAND_DISPLAY").map_err(|_| HandoffError::MissingEnvVar("WAYLAND_DISPLAY"))?;
    let sway_socket = std::env::var("SWAYSOCK").map_err(|_| HandoffError::MissingEnvVar("SWAYSOCK"))?;

    let stream = UnixStream::connect(&sock_path)
        .await
        .map_err(|source| HandoffError::Connect { path: sock_path, source })?;

    let handoff = Handoff { wayland_display, sway_socket };
    crate::write_handoff(stream, &handoff).await?;
    Ok(())
}

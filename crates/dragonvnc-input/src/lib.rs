//! Remote-input injection abstraction. Real backend on Linux: the XDG
//! Desktop Portal RemoteDesktop interface (see
//! `dragonvnc-capture::pipewire::RemoteDesktopInput` — it lives in the
//! capture crate because a correct implementation needs to share one
//! portal session with screen capture; see that module's docs). `CGEvent`
//! posting on macOS is still the next milestone — see DESIGN.md "Status".
//! This crate defines the trait boundary plus a logging-only stub so the
//! server's input stream has somewhere to go on platforms/paths without a
//! real injector wired up.

use async_trait::async_trait;
use dragonvnc_proto::InputEvent;

#[async_trait]
pub trait InputInjector: Send {
    async fn inject(&mut self, event: InputEvent) -> anyhow::Result<()>;
}

pub struct LoggingInjector;

#[async_trait]
impl InputInjector for LoggingInjector {
    async fn inject(&mut self, event: InputEvent) -> anyhow::Result<()> {
        tracing::debug!(?event, "input event (no real injector wired up yet)");
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub mod uinput;

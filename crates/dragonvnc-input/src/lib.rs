//! Remote-input injection abstraction. Real backend on Linux:
//! `linux::LinuxInjector`, combining `wlr_pointer` (Wayland
//! `wlr-virtual-pointer-unstable-v1` protocol, for the pointer) with
//! `uinput` (kernel evdev, for the keyboard) — see those modules' docs for
//! why pointer and keyboard need different mechanisms here, and why the
//! originally-planned XDG `RemoteDesktop` portal path isn't available on
//! this reference compositor. `CGEvent` posting on macOS is still the next
//! milestone — see DESIGN.md "Status". This crate defines the trait
//! boundary plus a logging-only stub so the server's input stream has
//! somewhere to go on platforms/paths without a real injector wired up.

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
#[cfg(target_os = "linux")]
pub mod wlr_pointer;
#[cfg(target_os = "linux")]
pub mod linux;

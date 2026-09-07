//! Remote-input injection abstraction. Real backend on Linux:
//! `linux::LinuxInjector`, combining `wlr_pointer` (Wayland
//! `wlr-virtual-pointer-unstable-v1` protocol) with `wlr_keyboard` (Wayland
//! `virtual-keyboard-unstable-v1` protocol) — both Wayland-native, both
//! talking directly to the target headless session's compositor socket
//! (see PLAN-headless-session.md item 3 for why: that compositor has no
//! libinput backend at all, so a kernel-level uinput device — this crate's
//! previous keyboard backend — has nowhere there to be delivered to).
//! `CGEvent` posting on macOS is still the next milestone — see DESIGN.md
//! "Status". This crate defines the trait boundary plus a logging-only stub
//! so the server's input stream has somewhere to go on platforms/paths
//! without a real injector wired up.

use async_trait::async_trait;
use dragonvnc_proto::InputEvent;

#[async_trait]
pub trait InputInjector: Send {
    async fn inject(&mut self, event: InputEvent) -> anyhow::Result<()>;

    /// Called after a live resize (`RequestMode`) has actually been applied,
    /// so absolute-pointer extents stay correct — a pointer motion is a
    /// ratio against the output's current size (see `wlr_pointer`), which
    /// changes when the display does. No-op default for injectors that
    /// don't do absolute pointer positioning (e.g. `LoggingInjector`).
    fn set_extents(&mut self, _width: u32, _height: u32) {}
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
pub mod linux;
#[cfg(target_os = "linux")]
pub mod wlr_keyboard;
#[cfg(target_os = "linux")]
pub mod wlr_pointer;

//! Remote-input injection abstraction. Real backends (`libei`/uinput on
//! Linux, `CGEvent` posting on macOS) are the next milestone — see
//! DESIGN.md "Status". This crate defines the trait boundary plus a
//! logging-only stub so the server's input stream has somewhere to go
//! before real injection is wired up.

use dragonvnc_proto::InputEvent;

pub trait InputInjector: Send {
    fn inject(&mut self, event: InputEvent) -> anyhow::Result<()>;
}

pub struct LoggingInjector;

impl InputInjector for LoggingInjector {
    fn inject(&mut self, event: InputEvent) -> anyhow::Result<()> {
        tracing::debug!(?event, "input event (no real injector wired up yet)");
        Ok(())
    }
}

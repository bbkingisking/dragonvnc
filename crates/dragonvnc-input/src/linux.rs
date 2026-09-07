//! Combines the two Linux backends into one `InputInjector`: both pointer
//! (`wlr_pointer`) and keyboard (`wlr_keyboard`) go through Wayland-protocol
//! virtual devices against the target session's own compositor socket — see
//! those modules' docs for why (short version: this compositor has no
//! libinput backend at all, so a kernel-level device has nowhere there to
//! be delivered to; see PLAN-headless-session.md item 3 for the full
//! reasoning, and its module doc's git history for the uinput backend this
//! replaced).

use std::path::Path;

use async_trait::async_trait;
use dragonvnc_proto::InputEvent;

use crate::wlr_keyboard::WlrKeyboard;
use crate::wlr_pointer::WlrPointer;

pub struct LinuxInjector {
    pointer: WlrPointer,
    keyboard: WlrKeyboard,
}

impl LinuxInjector {
    /// `wayland_socket_path` is the target session's own socket (see
    /// `wlr_pointer`/`wlr_keyboard`'s docs for why this isn't read from the
    /// environment). `width`/`height` become the pointer's motion extents;
    /// `xkb_options` compiles into the virtual keyboard's keymap.
    pub fn new(wayland_socket_path: &Path, width: u32, height: u32, xkb_options: Option<String>) -> anyhow::Result<Self> {
        Ok(Self {
            pointer: WlrPointer::new(wayland_socket_path, width, height)?,
            keyboard: WlrKeyboard::new(wayland_socket_path, xkb_options)?,
        })
    }
}

#[async_trait]
impl crate::InputInjector for LinuxInjector {
    async fn inject(&mut self, event: InputEvent) -> anyhow::Result<()> {
        match event {
            InputEvent::Key { .. } => self.keyboard.inject(event).await,
            other => self.pointer.inject(other).await,
        }
    }

    fn set_extents(&mut self, width: u32, height: u32) {
        self.pointer.set_extents(width, height);
    }
}

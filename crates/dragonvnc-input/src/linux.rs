//! Combines the two Linux backends into one `InputInjector`: pointer
//! events go through `wlr_pointer` (Wayland protocol, precise, no device
//! classification issues — see its module doc), keyboard events go
//! through `uinput` (kernel evdev, unambiguous for a keys-only device).

use async_trait::async_trait;
use dragonvnc_proto::InputEvent;

use crate::uinput::UinputKeyboard;
use crate::wlr_pointer::WlrPointer;

pub struct LinuxInjector {
    pointer: WlrPointer,
    keyboard: UinputKeyboard,
}

impl LinuxInjector {
    pub fn new(width: u32, height: u32) -> anyhow::Result<Self> {
        Ok(Self {
            pointer: WlrPointer::new(width, height)?,
            keyboard: UinputKeyboard::new()?,
        })
    }
}

#[async_trait]
impl crate::InputInjector for LinuxInjector {
    async fn inject(&mut self, event: InputEvent) -> anyhow::Result<()> {
        match event {
            InputEvent::Key { keycode, pressed } => self.keyboard.key(keycode, pressed),
            other => crate::InputInjector::inject(&mut self.pointer, other).await,
        }
    }
}

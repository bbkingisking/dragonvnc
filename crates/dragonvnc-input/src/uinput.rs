//! Real keyboard injection via the Linux `uinput` kernel module: create a
//! virtual keyboard device and write synthetic evdev key events to it.
//! Compositor-agnostic (goes through the same libinput/kernel path a real
//! keyboard would) and needs no portal support.
//!
//! Pointer input does **not** go through here — see `wlr_pointer` for why
//! (short version: libinput classifies a uinput device combining absolute
//! axes with pointer buttons as a graphics tablet, which needs proximity
//! handshaking real tablets provide and our raw ABS events don't; the
//! Wayland `wlr-virtual-pointer` protocol has no such classification
//! ambiguity). A plain keyboard-only device has no such issue — evdev
//! unambiguously classifies "has key codes, nothing else" as a keyboard —
//! so it stays on uinput.
//!
//! Needs read/write access to `/dev/uinput`, which is `root:root 0600` by
//! default on most distros — grant it via a udev rule (`KERNEL=="uinput",
//! GROUP="input", MODE="0660"`) and add the server's user to that group,
//! same as tools like `ydotool` document. Not this crate's job to manage
//! system permissions; it just needs the device already openable.

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;

use async_trait::async_trait;
use input_linux::{
    EventKind, InputEvent as EvdevEvent, InputId, Key, KeyEvent, KeyState as EvdevKeyState,
    SynchronizeEvent, UInputHandle,
};

use dragonvnc_proto::InputEvent;

fn now() -> input_linux::EventTime {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    input_linux::EventTime::new(d.as_secs() as i64, d.subsec_micros() as i64)
}

fn raw<T: Into<EvdevEvent>>(event: T) -> input_linux::sys::input_event {
    event.into().into()
}

pub struct UinputKeyboard {
    handle: UInputHandle<std::fs::File>,
}

impl UinputKeyboard {
    pub fn new() -> anyhow::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/uinput").map_err(|e| {
            anyhow::anyhow!(
                "opening /dev/uinput failed ({e}) — needs a udev rule granting your user's \
                 group read/write access (e.g. KERNEL==\"uinput\", GROUP=\"input\", MODE=\"0660\"), \
                 same setup as ydotool documents"
            )
        })?;
        let handle = UInputHandle::new(file);

        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Synchronize)?;
        // The full evdev keyboard keycode range (see linux/input-event-codes.h);
        // our wire protocol already carries evdev-style keycodes directly
        // (see dragonvnc_proto::InputEvent::Key's doc), so just enable
        // everything that's a valid Key rather than hand-picking a subset.
        for code in 0..0x2ffu16 {
            if let Ok(key) = Key::from_code(code) {
                let _ = handle.set_keybit(key);
            }
        }

        handle.create(
            &InputId {
                bustype: input_linux::sys::BUS_VIRTUAL as u16,
                vendor: 0,
                product: 0,
                version: 1,
            },
            b"dragonvnc virtual keyboard",
            0,
            &[],
        )?;

        tracing::info!(fd = handle.as_inner().as_raw_fd(), "uinput virtual keyboard created");

        Ok(Self { handle })
    }

    pub fn key(&mut self, keycode: u32, pressed: bool) -> anyhow::Result<()> {
        tracing::trace!(keycode, pressed, "uinput: injecting key event");
        let key = Key::from_code(keycode as u16)
            .map_err(|_| anyhow::anyhow!("keycode {keycode} is out of evdev's range"))?;
        let state = if pressed { EvdevKeyState::PRESSED } else { EvdevKeyState::RELEASED };
        let t = now();
        let started = std::time::Instant::now();
        self.handle.write(&[raw(KeyEvent::new(t, key, state)), raw(SynchronizeEvent::report(t))])?;
        let elapsed = started.elapsed();
        if elapsed > std::time::Duration::from_millis(50) {
            // A blocked write(2) to /dev/uinput would show up here — worth
            // distinguishing from a compositor/wlr-side stall, since this
            // is purely a kernel-level device write with no network or
            // compositor involvement at all.
            tracing::warn!(?elapsed, "uinput device write took unusually long");
        }
        Ok(())
    }
}

/// Standalone `InputInjector` that only handles `InputEvent::Key`, logging
/// (not erroring) anything else — used directly only if pointer injection
/// isn't wired up; `dragonvnc-server` normally uses `crate::linux::LinuxInjector`
/// instead, which pairs this with `wlr_pointer` for the rest.
#[async_trait]
impl crate::InputInjector for UinputKeyboard {
    async fn inject(&mut self, event: InputEvent) -> anyhow::Result<()> {
        match event {
            InputEvent::Key { keycode, pressed } => self.key(keycode, pressed),
            other => {
                tracing::debug!(?other, "UinputKeyboard ignoring non-key event");
                Ok(())
            }
        }
    }
}

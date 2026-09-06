//! Real input injection via the Linux `uinput` kernel module: create a
//! virtual keyboard+pointer device and write synthetic evdev events to it.
//! Compositor-agnostic (goes through the same libinput/kernel path a real
//! physical device would) and needs no portal support — unlike the XDG
//! `RemoteDesktop` portal interface, which would be the more sandboxed
//! choice where available, but isn't implemented by this box's reference
//! compositor backend (`xdg-desktop-portal-wlr` only ships
//! `Screenshot`/`ScreenCast`; see `dragonvnc-capture::pipewire`'s module
//! doc). Worth revisiting for desktops whose portal does implement it
//! (GNOME, KDE).
//!
//! Needs read/write access to `/dev/uinput`, which is `root:root 0600` by
//! default on most distros — grant it via a udev rule (`KERNEL=="uinput",
//! GROUP="input", MODE="0660"`) and add the server's user to that group,
//! same as tools like `ydotool` document. Not this crate's job to manage
//! system permissions; it just needs the device already opneable.

use std::fs::OpenOptions;
use std::os::fd::AsRawFd;

use async_trait::async_trait;
use input_linux::{
    AbsoluteAxis, AbsoluteEvent, AbsoluteInfo, AbsoluteInfoSetup, EventKind, InputEvent as EvdevEvent,
    InputId, InputProperty, Key, KeyEvent, KeyState as EvdevKeyState, RelativeAxis, RelativeEvent,
    SynchronizeEvent, UInputHandle,
};

use dragonvnc_proto::{InputEvent, PointerButton};

fn now() -> input_linux::EventTime {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    input_linux::EventTime::new(d.as_secs() as i64, d.subsec_micros() as i64)
}

fn raw<T: Into<EvdevEvent>>(event: T) -> input_linux::sys::input_event {
    event.into().into()
}

pub struct UinputInjector {
    handle: UInputHandle<std::fs::File>,
    width: u32,
    height: u32,
}

impl UinputInjector {
    /// `width`/`height` become the virtual device's absolute-axis range —
    /// fixed for its lifetime, same as the encoder: a resolution change
    /// needs a new injector, same as a new encoder. Uses the same "learn
    /// the real size from the first captured frame" pattern as
    /// `dragonvnc-server`'s encoder construction.
    pub fn new(width: u32, height: u32) -> anyhow::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/uinput").map_err(|e| {
            anyhow::anyhow!(
                "opening /dev/uinput failed ({e}) — needs a udev rule granting your user's \
                 group read/write access (e.g. KERNEL==\"uinput\", GROUP=\"input\", MODE=\"0660\"), \
                 same setup as ydotool documents"
            )
        })?;
        let handle = UInputHandle::new(file);

        handle.set_evbit(EventKind::Key)?;
        handle.set_evbit(EventKind::Absolute)?;
        handle.set_evbit(EventKind::Relative)?;
        handle.set_evbit(EventKind::Synchronize)?;

        handle.set_keybit(Key::ButtonLeft)?;
        handle.set_keybit(Key::ButtonRight)?;
        handle.set_keybit(Key::ButtonMiddle)?;
        // The full evdev keyboard keycode range (see linux/input-event-codes.h);
        // our wire protocol already carries evdev-style keycodes directly
        // (see dragonvnc_proto::InputEvent::Key's doc), so just enable
        // everything that's a valid Key rather than hand-picking a subset.
        for code in 0..0x2ffu16 {
            if let Ok(key) = Key::from_code(code) {
                let _ = handle.set_keybit(key);
            }
        }

        handle.set_absbit(AbsoluteAxis::X)?;
        handle.set_absbit(AbsoluteAxis::Y)?;
        handle.set_relbit(RelativeAxis::Wheel)?;
        handle.set_relbit(RelativeAxis::HorizontalWheel)?;
        // Without this, libinput/wlroots may treat an absolute-position
        // device as an indirect tablet tool (its own separate input class,
        // with its own default working-area mapping) rather than a
        // direct-mapped pointer — exactly the touchscreen-style semantics
        // real "absolute mouse" tools (QEMU's usb-tablet, spice-vdagent)
        // rely on `INPUT_PROP_DIRECT` for.
        handle.set_propbit(InputProperty::Direct)?;

        let abs_info = [
            AbsoluteInfoSetup {
                axis: AbsoluteAxis::X,
                info: AbsoluteInfo {
                    minimum: 0,
                    maximum: (width.max(1) - 1) as i32,
                    ..Default::default()
                },
            },
            AbsoluteInfoSetup {
                axis: AbsoluteAxis::Y,
                info: AbsoluteInfo {
                    minimum: 0,
                    maximum: (height.max(1) - 1) as i32,
                    ..Default::default()
                },
            },
        ];

        handle.create(
            &InputId {
                bustype: input_linux::sys::BUS_VIRTUAL as u16,
                vendor: 0,
                product: 0,
                version: 1,
            },
            b"dragonvnc virtual input",
            0,
            &abs_info,
        )?;

        tracing::info!(width, height, fd = handle.as_inner().as_raw_fd(), "uinput virtual device created");

        Ok(Self { handle, width, height })
    }
}

#[async_trait]
impl crate::InputInjector for UinputInjector {
    async fn inject(&mut self, event: InputEvent) -> anyhow::Result<()> {
        // uinput writes are synchronous ioctl-adjacent syscalls, not async
        // — fine at input-event rates, unlike video encode. No blocking
        // concern worth a spawn_blocking here.
        match event {
            InputEvent::PointerMove { x, y } => {
                let x = (x.round() as i32).clamp(0, self.width.max(1) as i32 - 1);
                let y = (y.round() as i32).clamp(0, self.height.max(1) as i32 - 1);
                let t = now();
                self.handle.write(&[
                    raw(AbsoluteEvent::new(t, AbsoluteAxis::X, x)),
                    raw(AbsoluteEvent::new(t, AbsoluteAxis::Y, y)),
                    raw(SynchronizeEvent::report(t)),
                ])?;
            }
            InputEvent::PointerButton { button, pressed } => {
                let key = match button {
                    PointerButton::Left => Key::ButtonLeft,
                    PointerButton::Right => Key::ButtonRight,
                    PointerButton::Middle => Key::ButtonMiddle,
                };
                let state = if pressed { EvdevKeyState::PRESSED } else { EvdevKeyState::RELEASED };
                let t = now();
                self.handle.write(&[raw(KeyEvent::new(t, key, state)), raw(SynchronizeEvent::report(t))])?;
            }
            InputEvent::Scroll { dx, dy } => {
                let t = now();
                self.handle.write(&[
                    raw(RelativeEvent::new(t, RelativeAxis::HorizontalWheel, dx.round() as i32)),
                    raw(RelativeEvent::new(t, RelativeAxis::Wheel, dy.round() as i32)),
                    raw(SynchronizeEvent::report(t)),
                ])?;
            }
            InputEvent::Key { keycode, pressed } => {
                let key = Key::from_code(keycode as u16)
                    .map_err(|_| anyhow::anyhow!("keycode {keycode} is out of evdev's range"))?;
                let state = if pressed { EvdevKeyState::PRESSED } else { EvdevKeyState::RELEASED };
                let t = now();
                self.handle.write(&[raw(KeyEvent::new(t, key, state)), raw(SynchronizeEvent::report(t))])?;
            }
        }
        Ok(())
    }
}

//! Pointer injection via the Wayland `wlr-virtual-pointer-unstable-v1`
//! protocol — the same mechanism `wayvnc` (an existing wlroots VNC server)
//! uses, and the one actually purpose-built for this problem.
//!
//! This crate's first attempt at absolute pointer injection went through
//! `uinput` (see `uinput`'s module doc) with `INPUT_PROP_DIRECT` set. That
//! turned out to be the wrong tool: verified via `libinput debug-events`
//! against the live target compositor, a uinput device combining absolute
//! axes with conventional pointer buttons (BTN_LEFT/RIGHT/MIDDLE) gets
//! classified by libinput as a **graphics tablet**, not a pointer or
//! touchscreen — and tablet-class devices need proximity handshaking
//! (`BTN_TOOL_PEN` in/out) that plain `ABS_X`/`ABS_Y` + `SYN_REPORT` events
//! don't provide, so libinput silently drops the device
//! ("missing tablet capabilities: resolution. Ignoring this device." was
//! the first symptom; fixing that surfaced the deeper tablet-vs-pointer
//! classification mismatch). The Wayland protocol used here has no such
//! ambiguity: `motion_absolute(x, y, x_extent, y_extent)` is unambiguous,
//! compositor-interpreted pointer motion, no kernel-evdev device
//! classification involved at all. Verified precisely against the live
//! compositor via a `grim` screenshot: an injected `motion_absolute` to a
//! specific pixel lands the real rendered cursor exactly there.
//!
//! Requires the compositor to actually implement this wlroots protocol
//! extension (this box's reference `sway` does); it's a wlroots-specific
//! protocol, not a generic Wayland one, so this module simply won't find
//! the global on a non-wlroots compositor — see `new()`'s error.
//!
//! Connects to a specific Wayland socket passed in explicitly, not the
//! process's own `WAYLAND_DISPLAY` — this server has no display of its own
//! in its environment and mustn't need one; it targets whichever headless
//! sway session it spawned for this connection (see
//! PLAN-headless-session.md item 3).

use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;

use async_trait::async_trait;
use wayland_client::protocol::{wl_pointer, wl_registry};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1 as mgr, zwlr_virtual_pointer_v1 as ptr,
};

use dragonvnc_proto::{InputEvent, PointerButton};

struct RegistryState {
    manager: Option<mgr::ZwlrVirtualPointerManagerV1>,
}

// None of these objects send events we care about (the manager and the
// virtual pointer itself define no `event`s in the protocol at all — see
// the XML — so these impls only exist to satisfy `Dispatch`'s trait bound).
impl Dispatch<wl_registry::WlRegistry, ()> for RegistryState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            if interface == "zwlr_virtual_pointer_manager_v1" {
                state.manager = Some(registry.bind::<mgr::ZwlrVirtualPointerManagerV1, _, _>(
                    name,
                    version.min(2),
                    qh,
                    (),
                ));
            }
        }
    }
}

impl Dispatch<mgr::ZwlrVirtualPointerManagerV1, ()> for RegistryState {
    fn event(_: &mut Self, _: &mgr::ZwlrVirtualPointerManagerV1, _: mgr::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ptr::ZwlrVirtualPointerV1, ()> for RegistryState {
    fn event(_: &mut Self, _: &ptr::ZwlrVirtualPointerV1, _: ptr::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

pub struct WlrPointer {
    conn: Connection,
    pointer: ptr::ZwlrVirtualPointerV1,
    width: u32,
    height: u32,
    started: Instant,
}

impl WlrPointer {
    /// `width`/`height` become the `x_extent`/`y_extent` every
    /// `motion_absolute` call is made against — updated later via
    /// `set_extents` on a live resize (`RequestMode`), so this doesn't need
    /// to be rebuilt when the display does.
    pub fn new(wayland_socket_path: &Path, width: u32, height: u32) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(wayland_socket_path).map_err(|e| {
            anyhow::anyhow!("connecting to Wayland socket {}: {e}", wayland_socket_path.display())
        })?;
        let conn = Connection::from_socket(stream)
            .map_err(|e| anyhow::anyhow!("wayland connection handshake failed: {e}"))?;
        let display = conn.display();
        let mut queue = conn.new_event_queue::<RegistryState>();
        let qh = queue.handle();
        let _registry = display.get_registry(&qh, ());

        let mut state = RegistryState { manager: None };
        queue.roundtrip(&mut state)?;
        let manager = state.manager.ok_or_else(|| {
            anyhow::anyhow!(
                "compositor has no zwlr_virtual_pointer_manager_v1 global — it doesn't support \
                 the wlr-virtual-pointer-unstable-v1 protocol extension"
            )
        })?;

        let pointer = manager.create_virtual_pointer(None, &qh, ());
        conn.flush()?;

        tracing::info!(width, height, "wlr virtual pointer created");
        Ok(Self { conn, pointer, width, height, started: Instant::now() })
    }

    fn time_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    pub fn set_extents(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }
}

#[async_trait]
impl crate::InputInjector for WlrPointer {
    async fn inject(&mut self, event: InputEvent) -> anyhow::Result<()> {
        // Wayland client requests are just buffered writes to a local
        // socket, not blocking syscalls — fine to call directly here, same
        // reasoning as the uinput backends.
        tracing::trace!(?event, "wlr pointer: injecting");
        match event {
            InputEvent::PointerMove { x, y } => {
                let x = x.round().clamp(0.0, self.width.max(1) as f32 - 1.0) as u32;
                let y = y.round().clamp(0.0, self.height.max(1) as f32 - 1.0) as u32;
                self.pointer.motion_absolute(self.time_ms(), x, y, self.width, self.height);
                self.pointer.frame();
            }
            InputEvent::PointerButton { button, pressed } => {
                // Evdev button codes (BTN_LEFT/BTN_RIGHT/BTN_MIDDLE), same
                // as the protocol's own doc requires ("button that produced
                // the event").
                let code: u32 = match button {
                    PointerButton::Left => 0x110,
                    PointerButton::Right => 0x111,
                    PointerButton::Middle => 0x112,
                };
                let state = if pressed { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released };
                self.pointer.button(self.time_ms(), code, state);
                self.pointer.frame();
            }
            InputEvent::Scroll { dx, dy } => {
                let t = self.time_ms();
                if dx != 0.0 {
                    self.pointer.axis(t, wl_pointer::Axis::HorizontalScroll, dx as f64);
                }
                if dy != 0.0 {
                    self.pointer.axis(t, wl_pointer::Axis::VerticalScroll, dy as f64);
                }
                self.pointer.frame();
            }
            InputEvent::Key { .. } => {} // not this backend's job — see LinuxInjector
        }
        let flush_started = std::time::Instant::now();
        self.conn.flush()?;
        let flush_elapsed = flush_started.elapsed();
        if flush_elapsed > std::time::Duration::from_millis(50) {
            // A blocked socket write to the compositor would show up here —
            // a real (if unlikely) candidate for "input stopped working"
            // freezes, distinct from anything on the network/decode side.
            tracing::warn!(?flush_elapsed, "wayland connection flush took unusually long");
        }
        Ok(())
    }
}

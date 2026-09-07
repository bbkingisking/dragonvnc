//! Keyboard injection via the Wayland `virtual-keyboard-unstable-v1`
//! protocol (`zwp_virtual_keyboard_manager_v1`) — replaces the earlier
//! `uinput` backend. The headless compositor this targets runs with
//! `WLR_LIBINPUT_NO_DEVICES=1` and has **no libinput backend at all** (see
//! PLAN-headless-session.md item 1's machine facts: `get_inputs` is `[]`),
//! so a kernel-level `uinput` virtual keyboard has nowhere in *that*
//! compositor to be delivered to — the kernel would instead hand it to
//! whichever compositor's libinput backend claims the physical seat, i.e.
//! the seat0 desktop this connection isn't supposed to touch at all. A
//! Wayland-protocol virtual keyboard, in contrast, is created directly
//! against this specific compositor's connection — there's no seat
//! ambiguity to get wrong.
//!
//! Compiles a real xkb keymap (`rules=evdev, model=pc105, layout=us`, plus
//! whatever `--xkb-options` the caller passes — this box's reference config
//! needs `lv3:ralt_switch,lv5:rctrl_switch` to keep its Right Alt/Right Ctrl
//! binds working) and hands it to the compositor as a memfd, same as a real
//! keyboard's `wl_keyboard.keymap` event would carry. Tracks an `xkb::State`
//! locally and sends `modifiers` after every `key` — wlroots does update its
//! own state from `key` alone, but sending modifiers explicitly keeps it
//! exact under held-modifier sequences (the same thing `wayvnc` does).
//!
//! `key` takes evdev keycodes directly, the same convention `wl_keyboard.key`
//! uses and what `dragonvnc_proto::InputEvent::Key` already carries on the
//! wire — no translation needed for the request itself. The *xkb* keycode
//! space used only for local state tracking is offset by 8 from evdev's (the
//! universal X11/XKB convention), so `update_key` gets `evdev_keycode + 8`.
//!
//! Repeat is the compositor's job: sway derives `repeat_info` for virtual
//! keyboards same as real ones, so this backend only ever sends discrete
//! press/release, same as before.

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::Instant;

use async_trait::async_trait;
use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{
    zwp_virtual_keyboard_manager_v1 as mgr, zwp_virtual_keyboard_v1 as kbd,
};
use xkbcommon::xkb;

use dragonvnc_proto::InputEvent;

/// `wl_keyboard`'s `keymap_format` enum value for a plain-text XKB v1
/// keymap — this protocol doesn't declare its own copy (see module doc's
/// reasoning on `key`'s state arg), so it's the same raw value by
/// convention.
const KEYMAP_FORMAT_XKB_V1: u32 = 1;
const KEY_STATE_RELEASED: u32 = 0;
const KEY_STATE_PRESSED: u32 = 1;
/// XKB keycodes are evdev keycodes offset by 8 — the universal X11/XKB
/// convention, unrelated to and not shared with the Wayland wire protocol's
/// own (unoffset) evdev keycodes used in `key` below.
const XKB_KEYCODE_OFFSET: u32 = 8;

struct RegistryState {
    seat: Option<wl_seat::WlSeat>,
    manager: Option<mgr::ZwpVirtualKeyboardManagerV1>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for RegistryState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else { return };
        match interface.as_str() {
            "wl_seat" if state.seat.is_none() => {
                state.seat = Some(registry.bind::<wl_seat::WlSeat, _, _>(name, version.min(7), qh, ()));
            }
            "zwp_virtual_keyboard_manager_v1" => {
                state.manager =
                    Some(registry.bind::<mgr::ZwpVirtualKeyboardManagerV1, _, _>(name, version.min(1), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for RegistryState {
    fn event(_: &mut Self, _: &wl_seat::WlSeat, _: wl_seat::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<mgr::ZwpVirtualKeyboardManagerV1, ()> for RegistryState {
    fn event(_: &mut Self, _: &mgr::ZwpVirtualKeyboardManagerV1, _: mgr::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<kbd::ZwpVirtualKeyboardV1, ()> for RegistryState {
    // No events defined by this protocol at all (see module doc) — this
    // impl only exists to satisfy `Dispatch`'s trait bound.
    fn event(_: &mut Self, _: &kbd::ZwpVirtualKeyboardV1, _: kbd::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

pub struct WlrKeyboard {
    conn: Connection,
    keyboard: kbd::ZwpVirtualKeyboardV1,
    xkb_state: xkb::State,
    started: Instant,
}

// `xkb::State` wraps a raw `*mut xkb_state`, which Rust conservatively
// never marks `Send` on its own. libxkbcommon has no thread-affinity —
// nothing here is thread-local or relies on being dropped on the thread
// that created it — and `WlrKeyboard` is only ever driven from a single
// task at a time (the server's input task; see `InputInjector`'s `Send`
// bound, which exists only so the trait object can be moved into that
// task, not for concurrent access).
unsafe impl Send for WlrKeyboard {}

impl WlrKeyboard {
    /// `wayland_socket_path` is the specific session's socket (see
    /// `wlr_pointer::WlrPointer::new`'s doc for why this takes an explicit
    /// path rather than reading `WAYLAND_DISPLAY`). `xkb_options` is passed
    /// straight through to `xkb_keymap_new_from_names` (e.g.
    /// `Some("lv3:ralt_switch,lv5:rctrl_switch")`); `None` compiles the
    /// plain `evdev`/`pc105`/`us` keymap with no extra options.
    pub fn new(wayland_socket_path: &Path, xkb_options: Option<String>) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(wayland_socket_path).map_err(|e| {
            anyhow::anyhow!("connecting to Wayland socket {}: {e}", wayland_socket_path.display())
        })?;
        let conn = Connection::from_socket(stream)
            .map_err(|e| anyhow::anyhow!("wayland connection handshake failed: {e}"))?;
        let display = conn.display();
        let mut queue = conn.new_event_queue::<RegistryState>();
        let qh = queue.handle();
        let _registry = display.get_registry(&qh, ());

        let mut state = RegistryState { seat: None, manager: None };
        queue.roundtrip(&mut state)?;
        let seat = state
            .seat
            .ok_or_else(|| anyhow::anyhow!("compositor advertises no wl_seat"))?;
        let manager = state.manager.ok_or_else(|| {
            anyhow::anyhow!(
                "compositor has no zwp_virtual_keyboard_manager_v1 global — it doesn't support \
                 the virtual-keyboard-unstable-v1 protocol extension"
            )
        })?;

        let keyboard = manager.create_virtual_keyboard(&seat, &qh, ());

        let xkb_context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &xkb_context,
            "evdev",
            "pc105",
            "us",
            "",
            xkb_options.clone(),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or_else(|| {
            anyhow::anyhow!("failed to compile xkb keymap (rules=evdev model=pc105 layout=us options={xkb_options:?})")
        })?;

        let mut keymap_bytes = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1).into_bytes();
        keymap_bytes.push(0); // NUL-terminated, per the wl_keyboard.keymap convention this mirrors.
        let fd = memfd_with_contents(&keymap_bytes)?;
        keyboard.keymap(KEYMAP_FORMAT_XKB_V1, fd.as_fd(), keymap_bytes.len() as u32);
        conn.flush()?;

        let xkb_state = xkb::State::new(&keymap);

        tracing::info!(?xkb_options, "wlr virtual keyboard created");
        Ok(Self { conn, keyboard, xkb_state, started: Instant::now() })
    }

    fn time_ms(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    fn send_modifiers(&mut self) {
        let depressed = self.xkb_state.serialize_mods(xkb::STATE_MODS_DEPRESSED);
        let latched = self.xkb_state.serialize_mods(xkb::STATE_MODS_LATCHED);
        let locked = self.xkb_state.serialize_mods(xkb::STATE_MODS_LOCKED);
        let group = self.xkb_state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE);
        self.keyboard.modifiers(depressed, latched, locked, group);
    }
}

#[async_trait]
impl crate::InputInjector for WlrKeyboard {
    async fn inject(&mut self, event: InputEvent) -> anyhow::Result<()> {
        if let InputEvent::Key { keycode, pressed } = event {
            tracing::trace!(keycode, pressed, "wlr keyboard: injecting");
            let wire_state = if pressed { KEY_STATE_PRESSED } else { KEY_STATE_RELEASED };
            self.keyboard.key(self.time_ms(), keycode, wire_state);

            let xkb_keycode = xkb::Keycode::new(keycode + XKB_KEYCODE_OFFSET);
            let direction = if pressed { xkb::KeyDirection::Down } else { xkb::KeyDirection::Up };
            self.xkb_state.update_key(xkb_keycode, direction);
            self.send_modifiers();

            let flush_started = Instant::now();
            self.conn.flush()?;
            let flush_elapsed = flush_started.elapsed();
            if flush_elapsed > std::time::Duration::from_millis(50) {
                // Same reasoning as `WlrPointer`: a blocked socket write to
                // the compositor here is a real (if unlikely) candidate for
                // "input stopped working" freezes.
                tracing::warn!(?flush_elapsed, "wayland connection flush took unusually long");
            }
        }
        Ok(())
    }
}

/// Creates a sealed, read-only-from-here-on memfd containing `bytes`, sized
/// exactly to fit — the shape every Wayland `keymap`-style fd-passing
/// request wants.
fn memfd_with_contents(bytes: &[u8]) -> anyhow::Result<std::os::fd::OwnedFd> {
    let fd = rustix::fs::memfd_create("dragonvnc-keymap", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|e| anyhow::anyhow!("memfd_create failed: {e}"))?;
    rustix::fs::ftruncate(&fd, bytes.len() as u64).map_err(|e| anyhow::anyhow!("ftruncate failed: {e}"))?;
    // SAFETY: `fd` is a freshly created, correctly sized memfd we exclusively
    // own; the mapping is unmapped again immediately after the write below,
    // well before `fd` is handed to the compositor.
    let map_ptr = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            bytes.len(),
            rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
            rustix::mm::MapFlags::SHARED,
            &fd,
            0,
        )
        .map_err(|e| anyhow::anyhow!("mmap failed: {e}"))?
    };
    // SAFETY: `map_ptr` is a valid mapping of exactly `bytes.len()` bytes
    // that we exclusively own at this point.
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), map_ptr as *mut u8, bytes.len());
        let _ = rustix::mm::munmap(map_ptr, bytes.len());
    }
    Ok(fd)
}

//! Real screen capture via `zwlr_screencopy_manager_v1`, talking directly to
//! a specific Wayland socket — never the process's own `WAYLAND_DISPLAY` env
//! var, which the server doesn't have and mustn't need (see
//! PLAN-headless-session.md item 2). Replaces the earlier PipeWire/XDG-portal
//! backend: the portal is bound once per user and already serves the
//! *physical* session's compositor, so it can't also serve a second, headless
//! one for the same user — see that plan for the full reasoning.
//!
//! Protocol shape: `capture_output(overlay_cursor, output)` creates a
//! one-shot `zwlr_screencopy_frame_v1`; it reports the `wl_shm` buffer shape
//! it wants (`buffer`, confirmed by `buffer_done`), we hand it a buffer via
//! `copy`, and it replies `ready` (frame in the buffer, go read it) or
//! `failed`. Only plain `wl_shm` buffers are used here (no DMA-BUF/zero-copy
//! — see PLAN-headless-session.md's "Future work" for that). Frames only
//! arrive when the compositor actually repaints (`capture_output` asks for
//! the *next* frame, not "whatever's on screen right now"), so an idle
//! screen produces no traffic, same as the PipeWire backend before it.
//!
//! Wayland's client API is callback-based and needs its own blocking
//! dispatch loop; bridged to this crate's async `FrameSource` the same way
//! the PipeWire backend was — a dedicated OS thread owns the connection and
//! forwards completed frames over a `watch` channel holding only the single
//! latest one, so a slow downstream (encode + network send) never queues
//! stale frames behind a live one.

use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use bytes::Bytes;
use tokio::sync::watch;
use wayland_client::protocol::{wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1 as frame, zwlr_screencopy_manager_v1 as manager,
};

use crate::{FrameInfo, FrameSource, PixelFormat, RawFrame};

pub struct ScreencopySource {
    rx: watch::Receiver<Option<RawFrame>>,
    // Kept alive for the source's lifetime — see `PipeWireSource`'s old doc
    // for why there's no graceful-shutdown join yet (fine for a
    // connection-scoped capture thread that just gets abandoned when the
    // session ends).
    _thread: std::thread::JoinHandle<()>,
}

impl ScreencopySource {
    /// `wayland_socket_path` is the *specific* session's socket — e.g.
    /// `$XDG_RUNTIME_DIR/wayland-2` for a headless session this process
    /// spawned (see `dragonvnc_session::SessionHandle::wayland_display`) —
    /// resolved by the caller, not read from the environment. `fps_cap`
    /// bounds the capture-request rate so a busy screen can't outrun the
    /// encoder (`None` = uncapped, request the next frame as soon as the
    /// previous one is handled).
    pub fn new(wayland_socket_path: &Path, fps_cap: Option<u32>) -> anyhow::Result<Self> {
        let stream = UnixStream::connect(wayland_socket_path).map_err(|e| {
            anyhow::anyhow!("connecting to Wayland socket {}: {e}", wayland_socket_path.display())
        })?;
        let conn = Connection::from_socket(stream)
            .map_err(|e| anyhow::anyhow!("wayland connection handshake failed: {e}"))?;

        let (tx, rx) = watch::channel(None);
        let started = Instant::now();
        let thread = std::thread::Builder::new()
            .name("dragonvnc-screencopy".into())
            .spawn(move || {
                if let Err(e) = run_capture_thread(conn, tx, started, fps_cap) {
                    tracing::error!(error = %e, "screencopy capture thread exited with an error");
                }
            })?;

        Ok(Self { rx, _thread: thread })
    }
}

#[async_trait::async_trait]
impl FrameSource for ScreencopySource {
    async fn next_frame(&mut self) -> anyhow::Result<Option<RawFrame>> {
        // Same coalescing reasoning as the old PipeWire backend: the channel
        // only ever holds the single latest frame, so there's no backlog to
        // drain here by construction. An error means the capture thread
        // ended (compositor gone, socket closed).
        if self.rx.changed().await.is_err() {
            return Ok(None);
        }
        Ok(self.rx.borrow_and_update().clone())
    }
}

fn map_shm_format(format: wl_shm::Format) -> Option<PixelFormat> {
    // wl_shm's *ARGB8888/XRGB8888 names describe the memory layout in
    // native (little-endian, on every platform this runs on) byte order as
    // B,G,R,A / B,G,R,x — see the wl_shm protocol's own doc on this exact
    // point. RGBA/RGBX-named formats are less commonly offered by
    // compositors but map the other way if seen.
    match format {
        wl_shm::Format::Argb8888 => Some(PixelFormat::Bgra),
        wl_shm::Format::Xrgb8888 => Some(PixelFormat::Bgrx),
        wl_shm::Format::Abgr8888 => Some(PixelFormat::Rgba),
        wl_shm::Format::Xbgr8888 => Some(PixelFormat::Rgbx),
        _ => None,
    }
}

/// Registry globals this backend needs. Bound once at startup; `output` is
/// the reference box's single `HEADLESS-1` (or whichever output the
/// compositor advertises first — this backend doesn't support multi-output
/// selection, matching the single-output headless session model).
#[derive(Default)]
struct Globals {
    shm: Option<wl_shm::WlShm>,
    output: Option<wl_output::WlOutput>,
    screencopy_manager: Option<manager::ZwlrScreencopyManagerV1>,
}

/// Accumulates events for the one `zwlr_screencopy_frame_v1` currently in
/// flight. Wayland object destruction (`.destroy()`) happens once the
/// client has read what it needs — see call sites below.
#[derive(Default)]
struct PendingFrame {
    shm_buffer_params: Option<(wl_shm::Format, u32, u32, u32)>, // format, width, height, stride
    buffer_done: bool,
    y_invert: bool,
    ready: bool,
    failed: bool,
}

struct State {
    globals: Globals,
    pending: PendingFrame,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
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
            "wl_shm" => {
                state.globals.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, version.min(1), qh, ()));
            }
            "wl_output" if state.globals.output.is_none() => {
                state.globals.output =
                    Some(registry.bind::<wl_output::WlOutput, _, _>(name, version.min(4), qh, ()));
            }
            "zwlr_screencopy_manager_v1" => {
                state.globals.screencopy_manager =
                    Some(registry.bind::<manager::ZwlrScreencopyManagerV1, _, _>(name, version.min(3), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_shm::WlShm, ()> for State {
    fn event(_: &mut Self, _: &wl_shm::WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(_: &mut Self, _: &wl_output::WlOutput, _: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<manager::ZwlrScreencopyManagerV1, ()> for State {
    fn event(_: &mut Self, _: &manager::ZwlrScreencopyManagerV1, _: manager::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_shm_pool::WlShmPool, ()> for State {
    fn event(_: &mut Self, _: &wl_shm_pool::WlShmPool, _: wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wayland_client::protocol::wl_buffer::WlBuffer, ()> for State {
    fn event(
        _: &mut Self,
        _: &wayland_client::protocol::wl_buffer::WlBuffer,
        _: wayland_client::protocol::wl_buffer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `release` (compositor done with the buffer) isn't needed: this
        // backend only reuses a buffer after `ready` for the copy that used
        // it, which is already the point screencopy hands it back — see
        // module doc.
    }
}

impl Dispatch<frame::ZwlrScreencopyFrameV1, ()> for State {
    fn event(state: &mut Self, _: &frame::ZwlrScreencopyFrameV1, event: frame::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            frame::Event::Buffer { format, width, height, stride } => {
                if let wayland_client::WEnum::Value(format) = format {
                    state.pending.shm_buffer_params = Some((format, width, height, stride));
                }
            }
            frame::Event::BufferDone => state.pending.buffer_done = true,
            frame::Event::Flags { flags } => {
                if let wayland_client::WEnum::Value(flags) = flags {
                    state.pending.y_invert = flags.contains(frame::Flags::YInvert);
                }
            }
            frame::Event::Ready { .. } => state.pending.ready = true,
            frame::Event::Failed => state.pending.failed = true,
            // `damage`/`linux_dmabuf`: not used — plain `copy` (not
            // `copy_with_damage`) and shm-only, respectively (see module doc).
            _ => {}
        }
    }
}

/// A reusable `wl_shm` buffer (memfd-backed pool of exactly one buffer).
/// Reallocated only when the compositor reports a different shape —
/// avoids a memfd+mmap round trip on every single frame when nothing
/// changed, which is the common case between resizes.
struct ShmBuffer {
    wl_buffer: wayland_client::protocol::wl_buffer::WlBuffer,
    map_ptr: *mut std::ffi::c_void,
    map_len: usize,
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

impl ShmBuffer {
    fn matches(&self, format: wl_shm::Format, width: u32, height: u32, stride: u32) -> bool {
        self.format == format && self.width == width && self.height == height && self.stride == stride
    }

    /// # Safety
    /// The returned slice aliases memory the compositor may still be
    /// writing to until `ready` is observed for the copy that used this
    /// buffer — callers must not read it before that.
    unsafe fn as_slice(&self) -> &[u8] {
        std::slice::from_raw_parts(self.map_ptr as *const u8, self.map_len)
    }
}

impl Drop for ShmBuffer {
    fn drop(&mut self) {
        self.wl_buffer.destroy();
        // SAFETY: `map_ptr`/`map_len` came from a successful `mmap` of
        // exactly this size in `alloc_shm_buffer` and are not used again
        // after this point.
        unsafe {
            let _ = rustix::mm::munmap(self.map_ptr, self.map_len);
        }
    }
}

// `*mut c_void` isn't `Send` by default (raw pointers never are), but this
// one only ever refers to a plain mmap'd memfd region — no thread-local or
// non-thread-safe state hides behind it — and `ShmBuffer` never hands the
// pointer out except through `as_slice`, called from the same thread that
// owns it. Needed because `ShmBuffer` lives inside a struct moved into the
// dedicated capture thread's closure.
unsafe impl Send for ShmBuffer {}

fn alloc_shm_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
) -> anyhow::Result<ShmBuffer> {
    let size = (stride as usize) * (height as usize);
    let fd: OwnedFd = rustix::fs::memfd_create("dragonvnc-screencopy", rustix::fs::MemfdFlags::CLOEXEC)
        .map_err(|e| anyhow::anyhow!("memfd_create failed: {e}"))?;
    rustix::fs::ftruncate(&fd, size as u64).map_err(|e| anyhow::anyhow!("ftruncate failed: {e}"))?;

    // SAFETY: `fd` is a freshly created, correctly sized memfd we exclusively
    // own at this point; the mapping is dropped (munmap'd) in `ShmBuffer`'s
    // `Drop` and never used past that.
    let map_ptr = unsafe {
        rustix::mm::mmap(
            std::ptr::null_mut(),
            size,
            rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
            rustix::mm::MapFlags::SHARED,
            &fd,
            0,
        )
        .map_err(|e| anyhow::anyhow!("mmap failed: {e}"))?
    };

    let pool = shm.create_pool(fd.as_fd(), size as i32, qh, ());
    let wl_buffer = pool.create_buffer(0, width as i32, height as i32, stride as i32, format, qh, ());
    // Recommended wl_shm pattern: the pool can be destroyed right after
    // creating the buffer(s) it backs — the buffer itself stays valid.
    pool.destroy();

    Ok(ShmBuffer { wl_buffer, map_ptr, map_len: size, format, width, height, stride })
}

fn run_capture_thread(
    conn: Connection,
    tx: watch::Sender<Option<RawFrame>>,
    started: Instant,
    fps_cap: Option<u32>,
) -> anyhow::Result<()> {
    let display = conn.display();
    let mut queue: EventQueue<State> = conn.new_event_queue();
    let qh = queue.handle();
    let _registry = display.get_registry(&qh, ());

    let mut state = State { globals: Globals::default(), pending: PendingFrame::default() };
    queue.roundtrip(&mut state)?;

    let shm = state
        .globals
        .shm
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor has no wl_shm global"))?;
    let output = state
        .globals
        .output
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor advertises no wl_output"))?;
    let screencopy_manager = state
        .globals
        .screencopy_manager
        .clone()
        .ok_or_else(|| anyhow::anyhow!("compositor has no zwlr_screencopy_manager_v1 global — it doesn't support wlr-screencopy-unstable-v1"))?;

    tracing::info!("screencopy capture connected, entering capture loop");

    let mut current_buffer: Option<ShmBuffer> = None;
    let min_frame_interval = fps_cap.map(|fps| Duration::from_secs_f64(1.0 / fps.max(1) as f64));
    let mut last_request = Instant::now() - min_frame_interval.unwrap_or_default();

    let mut stats_window_started = Instant::now();
    let mut frames_forwarded_this_window = 0u32;
    let mut frames_dropped_this_window = 0u32;
    let mut receiver_gone = false;

    loop {
        if let Some(interval) = min_frame_interval {
            let elapsed = last_request.elapsed();
            if elapsed < interval {
                std::thread::sleep(interval - elapsed);
            }
        }
        last_request = Instant::now();

        state.pending = PendingFrame::default();
        let frame_obj = screencopy_manager.capture_output(1, &output, &qh, ());

        while !state.pending.buffer_done && !state.pending.failed {
            queue.blocking_dispatch(&mut state)?;
        }
        if state.pending.failed {
            frame_obj.destroy();
            frames_dropped_this_window += 1;
            report_capture_stats(&mut stats_window_started, &mut frames_forwarded_this_window, &mut frames_dropped_this_window);
            continue;
        }
        let Some((format, width, height, stride)) = state.pending.shm_buffer_params else {
            // buffer_done fired but we got no wl_shm `buffer` event at all —
            // compositor only offered dmabuf, which this backend doesn't
            // support (see module doc).
            tracing::warn!("compositor offered no wl_shm buffer for this frame, dropping");
            frame_obj.destroy();
            frames_dropped_this_window += 1;
            report_capture_stats(&mut stats_window_started, &mut frames_forwarded_this_window, &mut frames_dropped_this_window);
            continue;
        };

        let Some(pixel_format) = map_shm_format(format) else {
            tracing::warn!(?format, "unsupported wl_shm pixel format, dropping frame");
            frame_obj.destroy();
            frames_dropped_this_window += 1;
            report_capture_stats(&mut stats_window_started, &mut frames_forwarded_this_window, &mut frames_dropped_this_window);
            continue;
        };

        if !current_buffer.as_ref().is_some_and(|b| b.matches(format, width, height, stride)) {
            match alloc_shm_buffer(&shm, &qh, format, width, height, stride) {
                Ok(buf) => current_buffer = Some(buf),
                Err(e) => {
                    tracing::warn!(error = %e, "failed to allocate shm buffer, dropping frame");
                    frame_obj.destroy();
                    frames_dropped_this_window += 1;
                    report_capture_stats(&mut stats_window_started, &mut frames_forwarded_this_window, &mut frames_dropped_this_window);
                    continue;
                }
            }
        }
        let buffer = current_buffer.as_ref().expect("just allocated or already matched above");

        frame_obj.copy(&buffer.wl_buffer);
        while !state.pending.ready && !state.pending.failed {
            queue.blocking_dispatch(&mut state)?;
        }
        frame_obj.destroy();

        if state.pending.failed {
            frames_dropped_this_window += 1;
            report_capture_stats(&mut stats_window_started, &mut frames_forwarded_this_window, &mut frames_dropped_this_window);
            continue;
        }

        // SAFETY: `ready` was just observed for the copy that wrote this
        // exact buffer, so the compositor is done writing it.
        let pixels = unsafe { buffer.as_slice() };
        let pixels = if state.pending.y_invert {
            invert_rows(pixels, height as usize, stride as usize)
        } else {
            Bytes::copy_from_slice(pixels)
        };

        let raw_frame = RawFrame {
            info: FrameInfo {
                width,
                height,
                timestamp_us: started.elapsed().as_micros() as u64,
                format: pixel_format,
                stride,
            },
            pixels,
        };
        frames_forwarded_this_window += 1;
        report_capture_stats(&mut stats_window_started, &mut frames_forwarded_this_window, &mut frames_dropped_this_window);

        if !receiver_gone && tx.send(Some(raw_frame)).is_err() {
            tracing::info!("screencopy capture thread has no receiver (session ended) — will keep running silently until the session is torn down");
            receiver_gone = true;
        }
    }
}

/// The encoder path assumes top-down rows; flip here on the rare compositor
/// that reports `y_invert` (not expected on the GLES2 headless renderer this
/// backend targets, but the flag is checked rather than assumed — see
/// PLAN-headless-session.md item 2).
fn invert_rows(pixels: &[u8], height: usize, stride: usize) -> Bytes {
    let mut out = vec![0u8; pixels.len()];
    for row in 0..height {
        let src = &pixels[row * stride..(row + 1) * stride];
        let dst_row = height - 1 - row;
        out[dst_row * stride..(dst_row + 1) * stride].copy_from_slice(src);
    }
    Bytes::from(out)
}

fn report_capture_stats(window_started: &mut Instant, forwarded: &mut u32, dropped: &mut u32) {
    if window_started.elapsed() < Duration::from_secs(5) {
        return;
    }
    let window = window_started.elapsed();
    tracing::info!(
        fps = *forwarded as f64 / window.as_secs_f64(),
        dropped = *dropped,
        "screencopy capture stats"
    );
    *forwarded = 0;
    *dropped = 0;
    *window_started = Instant::now();
}

//! Real screen capture via PipeWire, sourced through the XDG Desktop Portal
//! ScreenCast interface (`ashpd`) — the standard, compositor-agnostic way to
//! capture a Wayland session (no compositor-specific protocol needed; the
//! portal + PipeWire handle it). This box's live `sway` +
//! `xdg-desktop-portal-wlr` session is the reference target — see
//! DESIGN.md.
//!
//! Restricted to plain mapped (non-DMA-BUF) buffers for now: the format
//! candidates offered below carry no modifiers, so the compositor
//! negotiates a mem-fd/mem-ptr buffer we can read directly with no EGL/GBM
//! import step. DMA-BUF (zero-copy GPU-to-GPU) is a follow-up optimization,
//! not required for correctness.
//!
//! PipeWire's stream API is callback-based and runs its own blocking main
//! loop; it's bridged to this crate's async `FrameSource` by running that
//! loop on a dedicated OS thread and forwarding completed frames over an
//! unbounded channel.
//!
//! The portal's screen-picker is an interactive human-consent flow — fine
//! for "an app asks to share your screen," wrong for an always-on remote
//! desktop server, which needs to (re)start capture with nobody sitting at
//! the machine to click anything. The portal's answer to that is a
//! `restore_token`: authorize once interactively, persist the token it
//! hands back, and pass it into every later `select_sources` call with
//! `PersistMode::ExplicitlyRevoked` — the portal then restores the same
//! grant silently, no picker, until the user revokes it from their
//! desktop's privacy settings. That's what `new()` below does; only the
//! very first-ever run (per `token_path`) needs a human present.
//!
//! Input injection is deliberately **not** combined into this session:
//! the XDG portal also defines a `RemoteDesktop` interface for exactly
//! that (and this crate briefly did combine them, since
//! `NotifyPointerMotionAbsolute` needs a linked screencast stream to be
//! meaningful) — but `xdg-desktop-portal-wlr` (this box's reference
//! backend, v0.7.1) only implements `Screenshot`/`ScreenCast`, not
//! `RemoteDesktop` (confirmed via its installed `.portal` file, which
//! lists exactly those two interfaces). Since that path is unavailable
//! here, input goes through `dragonvnc-input`'s `uinput` backend instead,
//! which needs no portal at all — see that crate for why, and for the
//! GNOME/KDE portals that *do* implement RemoteDesktop, worth revisiting
//! there.

use std::os::fd::OwnedFd;
use std::path::Path;
use std::time::{Duration, Instant};

use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType, Stream as PortalStream};
use ashpd::desktop::PersistMode;
use async_trait::async_trait;
use bytes::Bytes;
use pipewire as pw;
use tokio::sync::mpsc;

use crate::{FrameInfo, FrameSource, PixelFormat, RawFrame};

pub struct PipeWireSource {
    rx: mpsc::UnboundedReceiver<RawFrame>,
    // Kept alive for the source's lifetime; the capture thread runs until
    // the process exits or the channel receiver drops and a send fails.
    // TODO: no graceful shutdown signal yet (join the thread on drop) —
    // fine for a long-lived server process, worth adding before this needs
    // to support "stop sharing this display" mid-session.
    _thread: std::thread::JoinHandle<()>,
}

impl PipeWireSource {
    /// `token_path` is where the portal's restore token gets persisted (see
    /// module doc) — pass the same path across restarts so only the very
    /// first run ever needs a human to click the picker. The first run
    /// (nothing saved yet, or the saved token has been revoked/expired)
    /// may show a compositor UI for the user to choose a monitor,
    /// depending on the portal backend. Fails if there is nothing to
    /// capture — e.g. this box's reference `sway` session was briefly
    /// down to zero physical outputs attached during development.
    pub async fn new(token_path: &Path) -> anyhow::Result<Self> {
        // Caught live during development: a restore token that's gone
        // stale (e.g. after the compositor briefly had zero outputs
        // attached, or the grant was revoked from the desktop's privacy
        // settings) makes the portal silently fall back to its interactive
        // picker (`xdg-desktop-portal-wlr` shells out to `slurp`) instead of
        // restoring silently — exactly the failure module doc above says
        // this token exists to avoid. For an unattended server, nobody's
        // there to answer it, so without this timeout `open_portal` hangs
        // forever: indistinguishable from a "the server just freezes"
        // report unless it's bounded and logged. 30s is generous for the
        // normal (silent-restore) path, which usually completes in well
        // under a second.
        let portal_started = Instant::now();
        let (stream_info, fd) =
            match tokio::time::timeout(Duration::from_secs(30), open_portal(token_path)).await {
                Ok(result) => result?,
                Err(_) => {
                    anyhow::bail!(
                        "screencast portal did not respond within 30s — this usually means the \
                         restore token went stale and the portal fell back to an interactive \
                         picker (e.g. xdg-desktop-portal-wlr's `slurp`) with nobody present to \
                         answer it; check for (and kill) a stuck picker process, or delete {} to \
                         force a fresh grant next run",
                        token_path.display()
                    );
                }
            };
        tracing::debug!(elapsed = ?portal_started.elapsed(), "screencast portal setup completed");
        let node_id = stream_info.pipe_wire_node_id();
        tracing::info!(node_id, size = ?stream_info.size(), "screencast portal session started");

        let (tx, rx) = mpsc::unbounded_channel();
        let started = Instant::now();
        let thread = std::thread::Builder::new()
            .name("dragonvnc-pipewire".into())
            .spawn(move || {
                if let Err(e) = run_capture_thread(node_id, fd, tx, started) {
                    tracing::error!(error = %e, "pipewire capture thread exited with an error");
                }
            })?;

        Ok(Self { rx, _thread: thread })
    }
}

#[async_trait]
impl FrameSource for PipeWireSource {
    async fn next_frame(&mut self) -> anyhow::Result<Option<RawFrame>> {
        let frame = self.rx.recv().await;
        // A backlog here means the capture thread is producing frames
        // faster than whatever's downstream (encode + network send) is
        // draining them — the unbounded channel just keeps growing rather
        // than blocking, so this is the only place that backpressure is
        // visible at all. A real lead on "video hitches/freezes" if it
        // shows up: the capture side is fine, something after it is slow.
        let backlog = self.rx.len();
        if backlog > 5 {
            tracing::warn!(backlog, "pipewire frame queue is backing up — encode/send is falling behind capture");
        }
        Ok(frame)
    }
}

async fn open_portal(token_path: &Path) -> anyhow::Result<(PortalStream, OwnedFd)> {
    // Absence (first run, or a stale/revoked token) just means the portal
    // falls back to the interactive picker — not an error here.
    let saved_token = std::fs::read_to_string(token_path).ok();
    if saved_token.is_some() {
        tracing::info!(path = %token_path.display(), "reusing saved screencast restore token, no picker expected");
    } else {
        tracing::info!(path = %token_path.display(), "no saved screencast restore token yet, picker expected");
    }

    let proxy = Screencast::new().await?;
    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(enumflags2::BitFlags::from(SourceType::Monitor))
                .set_multiple(false)
                .set_restore_token(saved_token.as_deref())
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        )
        .await?;
    let response = proxy
        .start(&session, None, Default::default())
        .await?
        .response()?;

    if let Some(token) = response.restore_token() {
        if let Some(parent) = token_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(token_path, token) {
            // Not fatal — capture still works this session, it'll just
            // need the picker again next time.
            tracing::warn!(error = %e, path = %token_path.display(), "failed to persist screencast restore token");
        }
    }

    let stream = response
        .streams()
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("portal returned no stream (no monitor selected/available)"))?;
    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;
    Ok((stream, fd))
}

/// Shared between the `param_changed` and `process` stream callbacks (both
/// run on the same single-threaded pipewire loop, so plain fields are fine
/// — no locking needed).
struct CaptureState {
    format: pw::spa::param::video::VideoInfoRaw,
    tx: mpsc::UnboundedSender<RawFrame>,
    started: Instant,
    // Periodic throughput/drop visibility — dropped/malformed buffers are
    // otherwise invisible (each individual one is a silent early return),
    // so a real stall here (compositor stops damaging, buffer format goes
    // sideways, etc.) would look identical to "nothing's wrong, just no
    // video" without these counters.
    stats_window_started: Instant,
    frames_forwarded_this_window: u32,
    frames_dropped_this_window: u32,
    // Set once the receiver is gone (session ended) so the debug log below
    // fires exactly once instead of on every subsequent buffer — this
    // thread has no shutdown signal yet (see `PipeWireSource`'s doc) and
    // keeps calling this callback for as long as the compositor keeps
    // damaging the screen, which is indefinitely for a live desktop.
    receiver_gone: bool,
}

/// Called from the (single-threaded) pipewire loop on every processed
/// buffer, forwarded or dropped. Logs a throughput/drop summary every 5s at
/// info level — cheap enough to leave on unconditionally, and exactly the
/// kind of thing worth having already logged by the time someone notices a
/// freeze, rather than needing to reproduce it with more verbose logging on.
fn report_capture_stats(state: &mut CaptureState) {
    if state.stats_window_started.elapsed() < Duration::from_secs(5) {
        return;
    }
    let window = state.stats_window_started.elapsed();
    tracing::info!(
        fps = state.frames_forwarded_this_window as f64 / window.as_secs_f64(),
        dropped = state.frames_dropped_this_window,
        "pipewire capture stats"
    );
    state.frames_forwarded_this_window = 0;
    state.frames_dropped_this_window = 0;
    state.stats_window_started = Instant::now();
}

fn map_pixel_format(format: pw::spa::param::video::VideoFormat) -> Option<PixelFormat> {
    use pw::spa::param::video::VideoFormat as VF;
    match format {
        VF::RGBA => Some(PixelFormat::Rgba),
        VF::RGBx => Some(PixelFormat::Rgbx),
        VF::BGRA => Some(PixelFormat::Bgra),
        VF::BGRx => Some(PixelFormat::Bgrx),
        _ => None,
    }
}

fn run_capture_thread(
    node_id: u32,
    fd: OwnedFd,
    tx: mpsc::UnboundedSender<RawFrame>,
    started: Instant,
) -> anyhow::Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopBox::new(None)?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None)?;
    let core = context.connect_fd(fd, None)?;

    let state = CaptureState {
        format: Default::default(),
        tx,
        started,
        stats_window_started: Instant::now(),
        frames_forwarded_this_window: 0,
        frames_dropped_this_window: 0,
        receiver_gone: false,
    };

    let stream = pw::stream::StreamBox::new(
        &core,
        "dragonvnc-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )?;

    let _listener = stream
        .add_local_listener_with_user_data(state)
        .param_changed(|_, state, id, param| {
            let Some(param) = param else { return };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = pw::spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != pw::spa::param::format::MediaType::Video
                || media_subtype != pw::spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if let Err(e) = state.format.parse(param) {
                tracing::warn!(error = %e, "failed to parse negotiated video format");
                return;
            }
            tracing::info!(
                format = ?state.format.format(),
                width = state.format.size().width,
                height = state.format.size().height,
                "pipewire capture format negotiated"
            );
        })
        .process(|stream, state| {
            if state.receiver_gone {
                // Nobody's listening (session ended) but this thread has no
                // shutdown signal yet (see `PipeWireSource`'s doc) and the
                // compositor keeps calling this callback for as long as the
                // screen keeps changing — i.e. forever, for a live desktop.
                // Already logged once below; from here on just drain the
                // buffer back to pipewire with no further work or logging,
                // so a long-lived server with several past connections
                // doesn't build up ever-growing log volume for sessions
                // nobody's watching anymore.
                let _ = stream.dequeue_buffer();
                return;
            }
            macro_rules! drop_frame {
                ($reason:expr) => {{
                    tracing::debug!(reason = $reason, "pipewire process callback dropped a buffer");
                    state.frames_dropped_this_window += 1;
                    report_capture_stats(state);
                    return;
                }};
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                drop_frame!("dequeue_buffer returned nothing (no buffer ready)");
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                drop_frame!("buffer had no data planes");
            }
            let data = &mut datas[0];
            let stride = data.chunk().stride();
            let size = data.chunk().size() as usize;
            if size == 0 || stride <= 0 {
                drop_frame!("empty/corrupted chunk (size or stride is zero)");
            }
            let Some(pixel_format) = map_pixel_format(state.format.format()) else {
                tracing::warn!(format = ?state.format.format(), "unsupported negotiated pixel format, dropping frame");
                state.frames_dropped_this_window += 1;
                report_capture_stats(state);
                return;
            };
            let Some(mapped) = data.data() else {
                // No mapped pointer — e.g. a DMA-BUF buffer, which this
                // backend deliberately doesn't negotiate for (see module
                // doc), or an SPA_DATA_MemFd not mapped by MAP_BUFFERS for
                // some reason. Either way, nothing we can read here yet.
                drop_frame!("no mapped pointer for this buffer (unexpected DMA-BUF or unmapped memfd?)");
            };
            let len = size.min(mapped.len());

            let frame = RawFrame {
                info: FrameInfo {
                    width: state.format.size().width,
                    height: state.format.size().height,
                    timestamp_us: state.started.elapsed().as_micros() as u64,
                    format: pixel_format,
                    stride: stride as u32,
                },
                pixels: Bytes::copy_from_slice(&mapped[..len]),
            };
            state.frames_forwarded_this_window += 1;
            report_capture_stats(state);
            // Receiver gone means the server is shutting this session down;
            // nothing to do but let the buffer drop (queues itself back to
            // pipewire via `Buffer`'s `Drop` impl) and keep spinning until
            // the process/session actually tears down this thread — which
            // (see `receiver_gone`'s doc) is never, for a live desktop. Log
            // this transition exactly once (not on every future buffer —
            // that turned this into an unbounded-log-growth bug the first
            // time this was tested live) so a capture thread that's still
            // alive but talking to nobody shows up in the logs instead of
            // silently looking like "no video, no clue why".
            if state.tx.send(frame).is_err() {
                tracing::info!("pipewire capture thread has no receiver (session ended) — will keep running silently, see module doc");
                state.receiver_gone = true;
            }
        })
        .register()?;

    // Candidate formats/sizes/framerate to negotiate. No modifiers are
    // offered, which keeps the compositor from proposing a DMA-BUF-backed
    // buffer (see module doc).
    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::RGBx,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::BGRx
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle { width: 1920, height: 1080 },
            pw::spa::utils::Rectangle { width: 1, height: 1 },
            pw::spa::utils::Rectangle { width: 8192, height: 8192 }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction { num: 30, denom: 1 },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 1000, denom: 1 }
        )
    );
    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .expect("serializing a fixed, well-formed format pod cannot fail")
    .0
    .into_inner();
    let mut params = [pw::spa::pod::Pod::from_bytes(&values)
        .ok_or_else(|| anyhow::anyhow!("failed to parse serialized format pod"))?];

    stream.connect(
        pw::spa::utils::Direction::Input,
        Some(node_id),
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    )?;

    tracing::info!(node_id, "pipewire stream connected, entering capture loop");
    mainloop.run();
    Ok(())
}

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

use std::os::fd::OwnedFd;
use std::time::Instant;

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
    /// Runs the portal's screen-picker flow (may show a compositor UI for
    /// the user to choose a monitor, depending on the portal backend) and
    /// starts capturing. Fails if there is nothing to capture — e.g. this
    /// box's reference `sway` session has zero physical outputs attached.
    pub async fn new() -> anyhow::Result<Self> {
        let (stream_info, fd) = open_portal().await?;
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
        Ok(self.rx.recv().await)
    }
}

async fn open_portal() -> anyhow::Result<(PortalStream, OwnedFd)> {
    let proxy = Screencast::new().await?;
    let session = proxy.create_session(Default::default()).await?;
    proxy
        .select_sources(
            &session,
            SelectSourcesOptions::default()
                .set_cursor_mode(CursorMode::Embedded)
                .set_sources(enumflags2::BitFlags::from(SourceType::Monitor))
                .set_multiple(false)
                .set_persist_mode(PersistMode::DoNot),
        )
        .await?;
    let response = proxy
        .start(&session, None, Default::default())
        .await?
        .response()?;
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
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            if datas.is_empty() {
                return;
            }
            let data = &mut datas[0];
            let stride = data.chunk().stride();
            let size = data.chunk().size() as usize;
            if size == 0 || stride <= 0 {
                return; // empty/corrupted chunk — nothing to forward
            }
            let Some(pixel_format) = map_pixel_format(state.format.format()) else {
                tracing::warn!(format = ?state.format.format(), "unsupported negotiated pixel format, dropping frame");
                return;
            };
            let Some(mapped) = data.data() else {
                // No mapped pointer — e.g. a DMA-BUF buffer, which this
                // backend deliberately doesn't negotiate for (see module
                // doc), or an SPA_DATA_MemFd not mapped by MAP_BUFFERS for
                // some reason. Either way, nothing we can read here yet.
                return;
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
            // Receiver gone means the server is shutting this session down;
            // nothing to do but let the buffer drop (queues itself back to
            // pipewire via `Buffer`'s `Drop` impl) and keep spinning until
            // the process/session actually tears down this thread.
            let _ = state.tx.send(frame);
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
            pw::spa::param::video::VideoFormat::BGRx,
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
        ),
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

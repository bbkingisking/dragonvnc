//! The async side: connect, pair, control handshake, then either dump raw
//! payloads (`--dump-raw`, headless) or run the real windowed path: a
//! decode task feeding frames to the render thread via `RenderEvent`, and
//! an input-forwarding task sending window events back over the control
//! stream. Both run concurrently so a burst of mouse moves can never queue
//! behind a video frame or vice versa.

use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedReceiver;
use winit::event_loop::EventLoopProxy;

use dragonvnc_codec::Decoder;
use dragonvnc_net::{endpoint, pairing::PairingCode};
use dragonvnc_proto::{ControlMessage, FrameHeader, InputEvent, PointerButton, VideoCodec, PROTOCOL_VERSION};

use crate::render::RenderEvent;

fn make_decoder(codec: VideoCodec) -> anyhow::Result<Box<dyn Decoder>> {
    match codec {
        VideoCodec::TestPatternRgba => Ok(Box::new(dragonvnc_codec::PassthroughCodec)),
        #[cfg(target_os = "macos")]
        VideoCodec::Hevc => Ok(Box::new(dragonvnc_codec::videotoolbox::VideoToolboxDecoder::new())),
        #[cfg(not(target_os = "macos"))]
        VideoCodec::Hevc => anyhow::bail!(
            "no HEVC decoder on this platform yet (VideoToolbox is macOS-only); pass --dump-raw \
             to inspect the stream with an independent tool instead"
        ),
        VideoCodec::Av1 => anyhow::bail!("no AV1 decoder implemented yet"),
    }
}

struct Handshake {
    connection: quinn::Connection,
    ctrl_send: quinn::SendStream,
    ctrl_recv: quinn::RecvStream,
}

async fn connect_and_handshake(addr: SocketAddr, code: PairingCode) -> anyhow::Result<Handshake> {
    let bind_addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0);
    let ep = endpoint::client_endpoint_for_pairing(bind_addr)?;
    let connection = ep.connect(addr, "dragonvnc")?.await?;
    tracing::info!(peer = %connection.remote_address(), "connected");

    let (mut send, mut recv) = connection.open_bi().await?;
    let server_fingerprint = dragonvnc_net::pairing::run(&connection, &mut send, &mut recv, &code)
        .await?
        .ok_or_else(|| anyhow::anyhow!("server presented no certificate — pairing cannot be trusted"))?;
    tracing::info!(
        fingerprint = %hex(&server_fingerprint),
        "pairing succeeded — in the real client this gets pinned to disk \
         so future connections skip the code (see dragonvnc-net::TrustStore, \
         already implemented and tested; not yet wired into this CLI)"
    );

    let (mut ctrl_send, mut ctrl_recv) = connection.open_bi().await?;
    send_msg(
        &mut ctrl_send,
        &ControlMessage::Hello { protocol_version: PROTOCOL_VERSION, client_name: "dragonvnc-client".into() },
    )
    .await?;
    let welcome: ControlMessage = recv_msg(&mut ctrl_recv).await?;
    let ControlMessage::Welcome { server_name, displays, .. } = welcome else {
        anyhow::bail!("expected Welcome, got {welcome:?}");
    };
    tracing::info!(%server_name, ?displays, "server said welcome");

    Ok(Handshake { connection, ctrl_send, ctrl_recv })
}

/// Headless path for `--dump-raw`: no window, no decoder, just write every
/// payload to a file for inspection with an independent tool.
pub async fn run_dump_raw(addr: SocketAddr, code: PairingCode, dump_path: PathBuf) -> anyhow::Result<()> {
    let hs = connect_and_handshake(addr, code).await?;
    drop(hs.ctrl_recv); // control stream unused in this path
    let mut video_recv = hs.connection.accept_uni().await?;
    let mut dump_file = std::fs::File::create(dump_path)?;
    let mut frames_this_second = 0u32;
    let mut bytes_this_second = 0u64;
    let mut window_start = std::time::Instant::now();

    loop {
        let header = match recv_frame_header(&mut video_recv).await {
            Ok(h) => h,
            Err(e) => {
                tracing::info!(error = %e, "video stream ended");
                break;
            }
        };
        let mut payload = vec![0u8; header.payload_len as usize];
        video_recv.read_exact(&mut payload).await?;
        dump_file.write_all(&payload)?;

        frames_this_second += 1;
        bytes_this_second += payload.len() as u64;
        if window_start.elapsed() >= std::time::Duration::from_secs(1) {
            tracing::info!(
                fps = frames_this_second,
                mbps = (bytes_this_second as f64 * 8.0 / 1_000_000.0),
                "video stats"
            );
            frames_this_second = 0;
            bytes_this_second = 0;
            window_start = std::time::Instant::now();
        }
    }
    Ok(())
}

/// The real path: decode + render + input, run until the connection ends.
/// Reports failure via `RenderEvent::NetworkError` (there's no other way
/// to surface it — this runs on a background thread, the window is the
/// only thing left with anything to show a person).
pub async fn run_windowed(
    addr: SocketAddr,
    code: PairingCode,
    move_to: Option<(f32, f32)>,
    proxy: EventLoopProxy<RenderEvent>,
    mut input_rx: UnboundedReceiver<InputEvent>,
) {
    let result: anyhow::Result<()> = async {
        let hs = connect_and_handshake(addr, code).await?;
        let Handshake { connection, mut ctrl_send, ctrl_recv } = hs;
        drop(ctrl_recv); // nothing more expected from the server on this stream

        if let Some((x, y)) = move_to {
            send_msg(&mut ctrl_send, &ControlMessage::Input(InputEvent::PointerMove { x, y })).await?;
            send_msg(
                &mut ctrl_send,
                &ControlMessage::Input(InputEvent::PointerButton { button: PointerButton::Left, pressed: true }),
            )
            .await?;
            send_msg(
                &mut ctrl_send,
                &ControlMessage::Input(InputEvent::PointerButton { button: PointerButton::Left, pressed: false }),
            )
            .await?;
            tracing::info!(x, y, "sent synthetic pointer move + left click");
        }

        // Input-forwarding task: relays window events (from the render
        // thread, over `input_rx`) to the server. Runs independently of
        // the video-decode loop below so neither can block the other.
        let input_task = tokio::spawn(async move {
            let mut sent_count = 0u64;
            while let Some(event) = input_rx.recv().await {
                tracing::trace!(?event, "forwarding input event");
                if let Err(e) = send_msg(&mut ctrl_send, &ControlMessage::Input(event)).await {
                    // Not silent: if input stops working while video is
                    // still arriving (or vice versa), this is the log line
                    // that tells you which side actually died first.
                    tracing::info!(error = %e, sent_count, "control stream ended, stopping input forwarding");
                    break;
                }
                sent_count += 1;
            }
        });

        let mut video_recv = connection.accept_uni().await?;
        let mut decoder: Option<Box<dyn Decoder>> = None;
        let session_started = Instant::now();
        let mut stats_window_started = Instant::now();
        let mut frames_this_window = 0u32;
        let mut bytes_this_window = 0u64;
        let mut total_frames = 0u64;
        let mut total_bytes = 0u64;
        loop {
            let header = match recv_frame_header(&mut video_recv).await {
                Ok(h) => h,
                Err(e) => {
                    // This used to be a silent `break` — from the outside,
                    // indistinguishable from "the window just froze": video
                    // stops updating, nothing logged, nothing shown. Now it
                    // at least ends up in the log, and (via the early
                    // return below) in the window's title.
                    tracing::info!(
                        error = %e,
                        total_frames,
                        total_bytes,
                        alive = ?session_started.elapsed(),
                        "video stream ended"
                    );
                    let _ = proxy.send_event(RenderEvent::NetworkError(format!(
                        "video stream ended: {e}"
                    )));
                    input_task.abort();
                    return Ok(());
                }
            };
            let mut payload = vec![0u8; header.payload_len as usize];
            video_recv.read_exact(&mut payload).await?;
            let is_first_decode = decoder.is_none();
            if decoder.is_none() {
                decoder = Some(make_decoder(header.codec)?);
            }
            let decode_started = Instant::now();
            let frame = decoder.as_mut().unwrap().decode(&payload, header.width, header.height)?;
            let decode_elapsed = decode_started.elapsed();
            // The first decode call on a fresh decoder instance routinely
            // includes one-time session setup (measured live: VideoToolbox
            // takes ~100ms just to stand up its decompression session) —
            // not a stall, so it's excluded from the warning to avoid
            // crying wolf on every single connection.
            if decode_elapsed > Duration::from_millis(100) && !is_first_decode {
                tracing::warn!(?decode_elapsed, width = header.width, height = header.height, "decode took unusually long");
            }

            total_frames += 1;
            total_bytes += payload.len() as u64;
            frames_this_window += 1;
            bytes_this_window += payload.len() as u64;
            if stats_window_started.elapsed() >= Duration::from_secs(5) {
                let stats = connection.stats();
                tracing::info!(
                    fps = frames_this_window as f64 / stats_window_started.elapsed().as_secs_f64(),
                    mbps = (bytes_this_window as f64 * 8.0 / 1_000_000.0) / stats_window_started.elapsed().as_secs_f64(),
                    rtt_ms = stats.path.rtt.as_secs_f64() * 1000.0,
                    cwnd = stats.path.cwnd,
                    congestion_events = stats.path.congestion_events,
                    lost_packets = stats.path.lost_packets,
                    "video stream stats"
                );
                frames_this_window = 0;
                bytes_this_window = 0;
                stats_window_started = Instant::now();
            }

            let sent = proxy.send_event(RenderEvent::Frame {
                width: header.width,
                height: header.height,
                format: frame.format,
                stride: frame.stride,
                pixels: frame.pixels,
            });
            if sent.is_err() {
                tracing::info!("render event loop is gone, stopping video decode loop");
                break; // window closed
            }
        }
        input_task.abort();
        Ok(())
    }
    .await;

    if let Err(e) = result {
        tracing::error!(error = %e, "run_windowed ended with an error");
        let _ = proxy.send_event(RenderEvent::NetworkError(e.to_string()));
    }
}

async fn recv_frame_header(stream: &mut quinn::RecvStream) -> anyhow::Result<FrameHeader> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(dragonvnc_proto::decode(&buf)?)
}

async fn send_msg(stream: &mut quinn::SendStream, msg: &ControlMessage) -> anyhow::Result<()> {
    let bytes = dragonvnc_proto::encode(msg)?;
    stream.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    stream.write_all(&bytes).await?;
    Ok(())
}

async fn recv_msg(stream: &mut quinn::RecvStream) -> anyhow::Result<ControlMessage> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(dragonvnc_proto::decode(&buf)?)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

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
use tokio::sync::watch;
use winit::event_loop::EventLoopProxy;

use dragonvnc_codec::Decoder;
use dragonvnc_net::{endpoint, pairing::PairingCode, ClientIdentity, TrustStore};
use dragonvnc_proto::{ControlMessage, FrameHeader, InputEvent, PointerButton, VideoCodec, Viewport, PROTOCOL_VERSION};

use crate::render::RenderEvent;

fn default_client_identity_path() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("dragonvnc").join("client_identity.bin")
}

fn default_server_trust_store_path() -> PathBuf {
    dirs::data_dir().unwrap_or_else(std::env::temp_dir).join("dragonvnc").join("paired_servers")
}

/// Persisted across connections — same reasoning as
/// `dragonvnc_net::ServerIdentity`: a fresh identity every launch would
/// make every prior pairing's pin useless (this client's fingerprint is
/// what the server's `PairedClients` store remembers).
fn load_client_identity() -> anyhow::Result<ClientIdentity> {
    ClientIdentity::load_or_generate(&default_client_identity_path(), "dragonvnc-client")
}

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

async fn connect_and_handshake(addr: SocketAddr, code: PairingCode, viewport: Viewport) -> anyhow::Result<Handshake> {
    let bind_addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0);
    let client_identity = load_client_identity()?;
    let trust_store_path = default_server_trust_store_path();
    let mut server_trust = TrustStore::load_from(&trust_store_path)?;
    // Keyed by address for this CLI (see `TrustStore`'s doc: the key's
    // meaning is the caller's choice) — good enough for "same box/LAN
    // address, same pin"; a real client with a persistent server identity
    // (hostname, mDNS name) would key on that instead.
    let server_key = addr.to_string();
    let pinned_fingerprint = server_trust.get(&server_key);

    let connection = match pinned_fingerprint {
        Some(expected) => {
            let ep = endpoint::client_endpoint_pinned(bind_addr, expected, &client_identity)?;
            ep.connect(addr, "dragonvnc")?.await?
        }
        None => {
            let ep = endpoint::client_endpoint_for_pairing(bind_addr, &client_identity)?;
            ep.connect(addr, "dragonvnc")?.await?
        }
    };
    tracing::info!(peer = %connection.remote_address(), "connected");

    if pinned_fingerprint.is_none() {
        let (mut send, mut recv) = connection.open_bi().await?;
        let server_fingerprint = dragonvnc_net::pairing::run(&connection, &mut send, &mut recv, &code)
            .await?
            .ok_or_else(|| anyhow::anyhow!("server presented no certificate — pairing cannot be trusted"))?;
        server_trust.pin(server_key, server_fingerprint);
        server_trust.save_to(&trust_store_path)?;
        tracing::info!(
            fingerprint = %hex(&server_fingerprint),
            "pairing succeeded, server pinned — future connections to this address skip the code"
        );
    } else {
        tracing::info!("server already pinned, skipping pairing ceremony");
    }

    let (mut ctrl_send, mut ctrl_recv) = connection.open_bi().await?;
    send_msg(
        &mut ctrl_send,
        &ControlMessage::Hello { protocol_version: PROTOCOL_VERSION, client_name: "dragonvnc-client".into(), viewport },
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
    // No window in this path, so no real viewport to report — a plausible
    // fixed size is enough since nothing here actually renders at it.
    let viewport = Viewport { width: 1920, height: 1080, scale: 1.0 };
    let hs = connect_and_handshake(addr, code, viewport).await?;
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
    mut viewport_rx: watch::Receiver<Option<Viewport>>,
) {
    let result: anyhow::Result<()> = async {
        // The window reports its real size/scale as soon as it's created
        // (see `render::App::resumed`) — wait for that first value rather
        // than guessing, so Hello carries the actual viewport and the
        // headless session (if any) starts at the right size with no
        // wrong-size flash.
        let initial_viewport = loop {
            if let Some(v) = *viewport_rx.borrow_and_update() {
                break v;
            }
            viewport_rx.changed().await?;
        };
        let hs = connect_and_handshake(addr, code, initial_viewport).await?;
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
        // thread, over `input_rx`) and viewport changes (window resizes /
        // scale-factor changes, over `viewport_rx`) to the server. Runs
        // independently of the video-decode loop below so neither can
        // block the other. `viewport_rx` already only ever holds the
        // latest size (a `watch` channel), so rapid resize events coalesce
        // here for free; the server debounces its side too (see
        // PLAN-headless-session.md item 4).
        let input_task = tokio::spawn(async move {
            let mut sent_count = 0u64;
            // The initial value was already consumed (via `borrow_and_update`,
            // into Hello) before this task started, so `changed()` below
            // only fires on genuinely later updates.
            loop {
                tokio::select! {
                    biased;
                    event = input_rx.recv() => {
                        let Some(event) = event else {
                            tracing::info!(sent_count, "render thread gone, stopping input forwarding");
                            break;
                        };
                        tracing::trace!(?event, "forwarding input event");
                        if let Err(e) = send_msg(&mut ctrl_send, &ControlMessage::Input(event)).await {
                            // Not silent: if input stops working while video
                            // is still arriving (or vice versa), this is the
                            // log line that tells you which side actually
                            // died first.
                            tracing::info!(error = %e, sent_count, "control stream ended, stopping input forwarding");
                            break;
                        }
                        sent_count += 1;
                    }
                    changed = viewport_rx.changed() => {
                        if changed.is_err() {
                            // Render thread/App gone — window closing, the
                            // input branch above will notice shortly too.
                            continue;
                        }
                        let Some(v) = *viewport_rx.borrow_and_update() else { continue };
                        let msg = ControlMessage::RequestMode { display_id: 0, width: v.width, height: v.height, refresh_hz: 0, scale: v.scale };
                        if let Err(e) = send_msg(&mut ctrl_send, &msg).await {
                            tracing::info!(error = %e, "control stream ended, stopping input forwarding");
                            break;
                        }
                    }
                }
            }
        });

        let mut video_recv = connection.accept_uni().await?;
        let mut decoder: Option<Box<dyn Decoder>> = None;
        // Tracked alongside `decoder` because `Decoder` doesn't expose the
        // size it was built for — a resize rebuilds the *server's* encoder
        // at the new size (see dragonvnc-server's RequestMode handling),
        // and every real hardware decoder (VideoToolbox included) needs a
        // fresh instance to match, same as the encoder side does. Found
        // live (2026-09-07): without this, a real window resize just
        // errored out of the whole decode loop and dropped the connection.
        let mut decoder_dims: Option<(u32, u32)> = None;
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
                    // return below) in the window's title. `{e:#}` (not
                    // `%e`) so the actual reason isn't buried one level down
                    // in the source chain — see the outer error handler's
                    // doc for why that distinction mattered live.
                    let full_error = format!("{e:#}");
                    tracing::info!(
                        error = %full_error,
                        total_frames,
                        total_bytes,
                        alive = ?session_started.elapsed(),
                        "video stream ended"
                    );
                    let _ = proxy.send_event(RenderEvent::NetworkError(format!(
                        "video stream ended: {full_error}"
                    )));
                    input_task.abort();
                    return Ok(());
                }
            };
            let mut payload = vec![0u8; header.payload_len as usize];
            video_recv.read_exact(&mut payload).await?;
            let needs_new_decoder = decoder_dims != Some((header.width, header.height));
            if needs_new_decoder {
                if decoder.is_some() {
                    tracing::info!(
                        old = ?decoder_dims,
                        new = ?(header.width, header.height),
                        "frame size changed, rebuilding decoder"
                    );
                }
                decoder = Some(make_decoder(header.codec)?);
                decoder_dims = Some((header.width, header.height));
            }
            let decode_started = Instant::now();
            let frame = decoder.as_mut().unwrap().decode(&payload, header.width, header.height)?;
            let decode_elapsed = decode_started.elapsed();
            // The first decode call on a fresh decoder instance (including
            // right after a resize rebuild, not just the very first one)
            // routinely includes one-time session setup (measured live:
            // VideoToolbox takes ~100ms just to stand up its decompression
            // session) — not a stall, so it's excluded from the warning to
            // avoid crying wolf on every new connection or resize.
            if decode_elapsed > Duration::from_millis(100) && !needs_new_decoder {
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
        // `%e`/`e.to_string()` only print anyhow's top-level message — for
        // an error like a QUIC connection failure, that's just "connection
        // lost", with the actually-useful reason (e.g. "closed by peer:
        // busy: another client is connected") buried one level down in the
        // source chain. `{:#}` (anyhow's alternate Display) walks the whole
        // chain on one line — found missing live (2026-09-07) when a busy
        // refusal showed up in the log as a bare "connection lost", giving
        // no hint why.
        let full_error = format!("{e:#}");
        tracing::error!(error = %full_error, "run_windowed ended with an error");
        let _ = proxy.send_event(RenderEvent::NetworkError(full_error));
    }
}

async fn recv_frame_header(stream: &mut quinn::RecvStream) -> anyhow::Result<FrameHeader> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    anyhow::ensure!(
        len <= dragonvnc_proto::MAX_FRAME_LEN,
        "peer declared a {len}-byte frame header, exceeding the {}-byte sanity limit",
        dragonvnc_proto::MAX_FRAME_LEN
    );
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    let header: FrameHeader = dragonvnc_proto::decode(&buf)?;
    anyhow::ensure!(
        header.payload_len as usize <= dragonvnc_proto::MAX_FRAME_LEN,
        "peer declared a {}-byte video payload, exceeding the {}-byte sanity limit",
        header.payload_len,
        dragonvnc_proto::MAX_FRAME_LEN
    );
    Ok(header)
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
    anyhow::ensure!(
        len <= dragonvnc_proto::MAX_FRAME_LEN,
        "peer declared a {len}-byte control message, exceeding the {}-byte sanity limit",
        dragonvnc_proto::MAX_FRAME_LEN
    );
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(dragonvnc_proto::decode(&buf)?)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

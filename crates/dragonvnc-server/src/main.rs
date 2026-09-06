//! Demo/debug server binary. Proves the transport+pairing+control pipeline
//! end-to-end using the synthetic `TestPatternSource` from dragonvnc-capture
//! — real capture is still the next milestone (see DESIGN.md "Status").
//! Encoding is real, not stubbed: `--codec vaapi-hevc` runs the actual VAAPI
//! HEVC hardware encoder (Linux/AMD only for now); `--codec passthrough`
//! (the default) sends raw RGBA for testing the rest of the pipeline
//! independent of any encoder. Not the final UX: every connection currently
//! re-runs the pairing ceremony rather than offering a pinned-reconnect
//! path, even though dragonvnc-net already supports and tests that (see its
//! `pinned_reconnect_*` tests). Wiring that choice into this CLI is small
//! follow-up work.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use dragonvnc_capture::{FrameSource, TestPatternSource};
use dragonvnc_net::{endpoint, pairing, ServerIdentity};
use dragonvnc_proto::{ControlMessage, FrameHeader, PROTOCOL_VERSION};

#[derive(Parser)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:5900")]
    bind: SocketAddr,

    /// Where to persist the server's long-term identity across restarts.
    #[arg(long)]
    identity_path: Option<PathBuf>,

    /// Test-pattern resolution/rate, since no real capture backend is
    /// wired up yet.
    #[arg(long, default_value_t = 320)]
    width: u32,
    #[arg(long, default_value_t = 240)]
    height: u32,
    #[arg(long, default_value_t = 30)]
    fps: u32,

    /// Which encoder to feed captured frames through.
    #[arg(long, value_enum, default_value_t = Codec::Passthrough)]
    codec: Codec,

    /// VAAPI render node to encode on (only used with --codec vaapi-hevc).
    #[cfg(target_os = "linux")]
    #[arg(long, default_value = dragonvnc_codec::vaapi::DEFAULT_DEVICE)]
    vaapi_device: String,

    /// Target bitrate in bits/sec (only used with --codec vaapi-hevc).
    #[arg(long, default_value_t = 4_000_000)]
    bitrate: i64,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Codec {
    /// Raw RGBA, no compression. Works everywhere, useful for proving the
    /// rest of the pipeline independent of any encoder.
    Passthrough,
    /// Hardware HEVC via VAAPI. Linux only — see DESIGN.md for why this is
    /// currently the only real encoder backend (the reference server GPU,
    /// an AMD RX 6700 XT, has no hardware AV1 encoder).
    VaapiHevc,
}

fn default_identity_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("dragonvnc")
        .join("server_identity.bin")
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();

    let identity_path = args.identity_path.unwrap_or_else(default_identity_path);
    let identity = ServerIdentity::load_or_generate(&identity_path, "dragonvnc-server")?;
    tracing::info!(
        fingerprint = %hex(&identity.fingerprint()),
        path = %identity_path.display(),
        "server identity ready"
    );

    let ep = endpoint::server_endpoint(args.bind, &identity)?;
    tracing::info!(addr = %ep.local_addr()?, "listening");

    // v1 demo: one pairing code, generated up front so it can be read and
    // typed into the client before dialing (a real UI would let the
    // operator mint a new one per pairing attempt on demand instead).
    let code = pairing::PairingCode::generate();
    println!("=== pairing code (enter on client): {code} ===");

    loop {
        let Some(incoming) = ep.accept().await else {
            break;
        };
        let width = args.width;
        let height = args.height;
        let fps = args.fps;
        let code = code.clone();
        let codec = args.codec;
        let bitrate = args.bitrate;
        #[cfg(target_os = "linux")]
        let vaapi_device = args.vaapi_device.clone();
        tokio::spawn(async move {
            let result = handle_connection(
                incoming,
                width,
                height,
                fps,
                code,
                codec,
                bitrate,
                #[cfg(target_os = "linux")]
                vaapi_device,
            )
            .await;
            if let Err(e) = result {
                tracing::warn!(error = %e, "connection ended with error");
            }
        });
    }
    Ok(())
}

fn make_encoder(
    codec: Codec,
    width: u32,
    height: u32,
    fps: u32,
    bitrate: i64,
    #[cfg(target_os = "linux")] vaapi_device: &str,
) -> anyhow::Result<Box<dyn dragonvnc_codec::Encoder>> {
    match codec {
        Codec::Passthrough => Ok(Box::new(dragonvnc_codec::PassthroughCodec)),
        #[cfg(target_os = "linux")]
        Codec::VaapiHevc => Ok(Box::new(dragonvnc_codec::vaapi::VaapiHevcEncoder::new(
            vaapi_device,
            width,
            height,
            fps,
            bitrate,
        )?)),
        #[cfg(not(target_os = "linux"))]
        Codec::VaapiHevc => anyhow::bail!(
            "--codec vaapi-hevc is Linux-only (VAAPI); this build was compiled for a different target"
        ),
    }
}

async fn handle_connection(
    incoming: quinn::Incoming,
    width: u32,
    height: u32,
    fps: u32,
    code: pairing::PairingCode,
    codec: Codec,
    bitrate: i64,
    #[cfg(target_os = "linux")] vaapi_device: String,
) -> anyhow::Result<()> {
    let connection = incoming.await?;
    tracing::info!(peer = %connection.remote_address(), "connection established");

    // Pairing ceremony. See module doc: v1 demo always re-pairs.
    let (mut send, mut recv) = connection.accept_bi().await?;
    // The server has no TLS identity of its own to present (v1 is
    // server-only auth — see DESIGN.md), so it correctly gets no peer
    // fingerprint back here; only the client side needs one, to pin us.
    pairing::run(&connection, &mut send, &mut recv, &code).await?;
    tracing::info!("pairing succeeded");

    // Control stream: Hello/Welcome handshake.
    let (mut ctrl_send, mut ctrl_recv) = connection.accept_bi().await?;
    let hello: ControlMessage = recv_msg(&mut ctrl_recv).await?;
    let ControlMessage::Hello { protocol_version, client_name } = hello else {
        anyhow::bail!("expected Hello, got {hello:?}");
    };
    anyhow::ensure!(
        protocol_version == PROTOCOL_VERSION,
        "protocol version mismatch: client={protocol_version} server={PROTOCOL_VERSION}"
    );
    tracing::info!(%client_name, "client said hello");
    send_msg(
        &mut ctrl_send,
        &ControlMessage::Welcome {
            protocol_version: PROTOCOL_VERSION,
            server_name: "dragonvnc-server".into(),
            displays: vec![dragonvnc_proto::DisplayInfo {
                id: 0,
                width,
                height,
                refresh_hz: fps,
            }],
        },
    )
    .await?;

    // Video stream: capture -> encode -> length-prefixed frames on a
    // dedicated reliable uni stream. Real transport tuning (chunked,
    // loss-tolerant unreliable datagrams once encoded frames are
    // realistically small) is next-milestone work — see DESIGN.md.
    let mut video_send = connection.open_uni().await?;
    let mut source = TestPatternSource::new(width, height, fps);
    let mut encoder = make_encoder(
        codec,
        width,
        height,
        fps,
        bitrate,
        #[cfg(target_os = "linux")]
        &vaapi_device,
    )?;
    let mut frame_id = 0u64;
    // NOTE: encode() below runs synchronous, blocking FFI (CPU pixel
    // conversion + a VAAPI submission) directly inside this async task.
    // Fine for this milestone's frame rates; a real deployment should move
    // this to `spawn_blocking` so a slow encode can't stall other tokio
    // tasks on the same worker thread.
    'outer: while let Some(frame) = source.next_frame().await? {
        for encoded in encoder.encode(&frame)? {
            let header = FrameHeader {
                display_id: 0,
                frame_id,
                timestamp_us: frame.info.timestamp_us,
                codec: encoded.codec,
                width: frame.info.width,
                height: frame.info.height,
                keyframe: encoded.keyframe,
                payload_len: encoded.payload.len() as u32,
            };
            if send_video_frame(&mut video_send, &header, &encoded.payload)
                .await
                .is_err()
            {
                break 'outer; // client went away
            }
            frame_id += 1;
        }
    }

    Ok(())
}

async fn send_video_frame(
    stream: &mut quinn::SendStream,
    header: &FrameHeader,
    payload: &[u8],
) -> anyhow::Result<()> {
    let header_bytes = dragonvnc_proto::encode(header)?;
    stream
        .write_all(&(header_bytes.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(&header_bytes).await?;
    stream.write_all(payload).await?;
    Ok(())
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

//! Demo/debug client binary — companion to dragonvnc-server. Proves the
//! same pipeline from the receiving end: pairing, control handshake, then
//! decoding the video stream. No real window/GPU render yet (that's the
//! `wgpu`/`winit` milestone — see DESIGN.md "Status"); this just decodes
//! each frame and reports throughput, to prove the bytes arriving are
//! correct and complete.

use std::net::{Ipv4Addr, SocketAddr};

use clap::Parser;
use dragonvnc_codec::Decoder;
use dragonvnc_net::{endpoint, pairing::PairingCode};
use dragonvnc_proto::{ControlMessage, FrameHeader, PROTOCOL_VERSION};

#[derive(Parser)]
struct Args {
    /// Server address to connect to.
    #[arg(long)]
    addr: SocketAddr,

    /// Pairing code shown by the server.
    #[arg(long)]
    code: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let args = Args::parse();
    let code = PairingCode::from(args.code);

    let bind_addr = SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0);
    let ep = endpoint::client_endpoint_for_pairing(bind_addr)?;
    let connection = ep.connect(args.addr, "dragonvnc")?.await?;
    tracing::info!(peer = %connection.remote_address(), "connected");

    let (mut send, mut recv) = connection.open_bi().await?;
    let server_fingerprint =
        dragonvnc_net::pairing::run(&connection, &mut send, &mut recv, &code)
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
        &ControlMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "dragonvnc-client-demo".into(),
        },
    )
    .await?;
    let welcome: ControlMessage = recv_msg(&mut ctrl_recv).await?;
    let ControlMessage::Welcome { server_name, displays, .. } = welcome else {
        anyhow::bail!("expected Welcome, got {welcome:?}");
    };
    tracing::info!(%server_name, ?displays, "server said welcome");

    let mut video_recv = connection.accept_uni().await?;
    let mut decoder = dragonvnc_codec::PassthroughCodec;
    let mut frames_this_second = 0u32;
    let mut bytes_this_second = 0u64;
    let mut window_start = std::time::Instant::now();

    loop {
        let header = match recv_frame_header(&mut video_recv).await {
            Ok(h) => h,
            Err(_) => break, // stream closed, server went away
        };
        let mut payload = vec![0u8; header.payload_len as usize];
        video_recv.read_exact(&mut payload).await?;
        let _rgba = decoder.decode(&payload, header.width, header.height)?;

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

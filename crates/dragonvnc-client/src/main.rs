//! dragonvnc client. Connects, pairs, and either renders the video stream
//! in a real window (the default) — decoding via real VideoToolbox HEVC
//! hardware decode on macOS (see `dragonvnc_codec::videotoolbox`),
//! `PassthroughCodec` for the test-pattern path — and forwards window
//! input back to the server, or (`--dump-raw`) just writes payloads to a
//! file headlessly for inspection with an independent tool.
//!
//! winit owns the main thread (required on macOS); all networking runs on
//! a background thread with its own Tokio runtime, bridged via an
//! `EventLoopProxy` (network → render: decoded frames) and an unbounded
//! channel (render → network: input events) — see `render`/`network`.

mod keymap;
mod network;
mod render;

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::Parser;
use dragonvnc_net::pairing::PairingCode;
use winit::event_loop::EventLoop;

#[derive(Parser)]
struct Args {
    /// Server address to connect to.
    #[arg(long)]
    addr: SocketAddr,

    /// Pairing code shown by the server.
    #[arg(long)]
    code: String,

    /// Write the raw video payloads to this file instead of decoding/
    /// rendering them — useful for inspecting the stream with an
    /// independent tool (e.g. `ffprobe -f hevc dump.hevc`). Headless: no
    /// window opens in this mode.
    #[arg(long)]
    dump_raw: Option<PathBuf>,

    /// Send one synthetic pointer move to "x,y" right after the control
    /// handshake, then a left click, in addition to whatever real input
    /// the window sends — handy for scripted testing without needing to
    /// actually click.
    #[arg(long, value_parser = parse_xy)]
    move_to: Option<(f32, f32)>,
}

/// `RUST_LOG` still controls everything if set (`try_from_default_env`
/// parses it exactly as `tracing_subscriber::fmt::init()` would); this only
/// picks a saner *default* when it isn't. Plain `info` alone is unreadable
/// here: wgpu logs an INFO line per submitted frame
/// (`wgpu_core::device::resource: Device::maintain: waiting for submission
/// index N`) and DEBUG-level wgpu_hal/naga dump full shader translations
/// and pipeline capability structs — real noise this crate's own debug
/// instrumentation would otherwise be buried under. Downgraded here rather
/// than left as "just set RUST_LOG more precisely", since the whole point
/// of this is to have useful logs already on by default when something
/// goes wrong, not to require already knowing what to ask for.
fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,wgpu_core=warn,wgpu_hal=warn,naga=warn"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn parse_xy(s: &str) -> Result<(f32, f32), String> {
    let (x, y) = s.split_once(',').ok_or_else(|| "expected \"x,y\"".to_string())?;
    Ok((
        x.trim().parse().map_err(|_| "bad x".to_string())?,
        y.trim().parse().map_err(|_| "bad y".to_string())?,
    ))
}

fn main() -> anyhow::Result<()> {
    init_tracing();
    let args = Args::parse();
    let code = PairingCode::from(args.code);

    if let Some(dump_path) = args.dump_raw {
        // Headless: no window, so no need for winit to own the thread —
        // a plain tokio runtime is enough.
        let rt = tokio::runtime::Runtime::new()?;
        return rt.block_on(network::run_dump_raw(args.addr, code, dump_path));
    }

    let event_loop = EventLoop::<render::RenderEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
    let (viewport_tx, viewport_rx) = tokio::sync::watch::channel(None);

    let addr = args.addr;
    let move_to = args.move_to;
    std::thread::Builder::new().name("dragonvnc-network".into()).spawn(move || {
        let rt = match tokio::runtime::Runtime::new() {
            Ok(rt) => rt,
            Err(e) => {
                tracing::error!(error = %e, "failed to start network runtime");
                return;
            }
        };
        rt.block_on(network::run_windowed(addr, code, move_to, proxy, input_rx, viewport_rx));
    })?;

    let mut app = render::App::new(input_tx, viewport_tx);
    event_loop.run_app(&mut app)?;
    Ok(())
}

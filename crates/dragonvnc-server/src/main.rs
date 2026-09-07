//! `dragonvnc-server` — normally run as `run` under `systemd --user` (see
//! PLAN-headless-session.md item 6): each connection logs in to its own,
//! freshly spawned, headless sway session (`--source session`, the
//! default), captured via wlr-screencopy and driven via Wayland-protocol
//! virtual input, with the session torn down when the connection ends. One
//! session, one client — a second connection while one is active is
//! refused (QUIC close code `CLOSE_CODE_BUSY`) before pairing even starts.
//! `--source test-pattern` bypasses all of that (no session, no compositor)
//! for exercising the rest of the pipeline without a real display or GPU.
//! Encoding: `--codec vaapi-hevc` runs the actual VAAPI HEVC hardware
//! encoder (Linux/AMD only for now); `--codec passthrough` (the default)
//! sends raw pixels for testing independent of any encoder.
//!
//! Mutual TLS (item 5): every connection presents a client certificate, not
//! just the server's. A known device (its fingerprint already in the
//! `PairedClients` store) skips the pairing ceremony entirely; an unknown
//! one must complete it (the printed pairing code) once, after which it's
//! pinned — "the paired device is the login". `clients list`/`clients
//! revoke <fingerprint>` manage that store.
//!
//! Two more subcommands exist for verifying pieces of this without a
//! client: `session-ready` (internal — see its own doc) and `probe-session`
//! (start/resize/verify/teardown a session standalone).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use dragonvnc_capture::{FrameSource, RawFrame, TestPatternSource};
use dragonvnc_input::InputInjector;
use dragonvnc_net::{endpoint, pairing, PairedClients, ServerIdentity};
use dragonvnc_proto::{ControlMessage, FrameHeader, Viewport, CLOSE_CODE_BUSY, PROTOCOL_VERSION};

#[derive(Parser)]
#[command(name = "dragonvnc-server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Run the server (long-running; this is the normal mode).
    Run(RunArgs),
    /// Internal: reports this process's `WAYLAND_DISPLAY`/`SWAYSOCK` back to
    /// the session launcher waiting on `$DRAGONVNC_HANDOFF`. Only makes
    /// sense invoked via `exec` from inside a headless sway session this
    /// same binary spawned (see `dragonvnc-session`'s generated overlay
    /// config) — never run this by hand.
    #[cfg(target_os = "linux")]
    SessionReady,
    /// Starts a headless sway session, resizes it, tears it down, and
    /// checks nothing leaked — a way to verify `dragonvnc-session` without a
    /// client. See PLAN-headless-session.md item 1.
    #[cfg(target_os = "linux")]
    ProbeSession(ProbeSessionArgs),
    /// Manage the paired-client-device store (item 5) — the devices allowed
    /// to log in without re-pairing.
    Clients(ClientsArgs),
}

#[derive(Parser)]
struct ClientsArgs {
    #[command(subcommand)]
    action: ClientsAction,
}

#[derive(clap::Subcommand)]
enum ClientsAction {
    /// List every paired client's fingerprint.
    List,
    /// Un-pin a client fingerprint — it must complete the pairing ceremony
    /// (with a valid code) again before it can connect.
    Revoke {
        /// Hex-encoded fingerprint, as printed by `clients list`.
        fingerprint: String,
    },
}

#[cfg(target_os = "linux")]
#[derive(Parser)]
struct ProbeSessionArgs {
    /// Initial mode, `WIDTHxHEIGHT[@SCALE]` (scale defaults to 1.0).
    #[arg(long, default_value = "1920x1080")]
    mode: String,

    /// The user's real sway config to build the headless overlay from.
    #[arg(long, default_value_os_t = default_sway_config_path())]
    sway_config: PathBuf,

    /// The `sway-session` wrapper to launch through (must forward args to
    /// `sway` — see `dragonvnc-session`'s module doc).
    #[arg(long, default_value_os_t = default_sway_session_path())]
    sway_session: PathBuf,
}

#[cfg(target_os = "linux")]
fn default_sway_config_path() -> PathBuf {
    dirs::home_dir().unwrap_or_else(std::env::temp_dir).join(".config/sway/config")
}

#[cfg(target_os = "linux")]
fn default_sway_session_path() -> PathBuf {
    dirs::home_dir().unwrap_or_else(std::env::temp_dir).join(".config/sway/sway-session")
}

#[cfg(target_os = "linux")]
fn default_runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("dragonvnc")
}

#[derive(Parser)]
struct RunArgs {
    /// Address to listen on.
    #[arg(long, default_value = "0.0.0.0:5900")]
    bind: SocketAddr,

    /// Where to persist the server's long-term identity across restarts.
    #[arg(long)]
    identity_path: Option<PathBuf>,

    /// Which capture backend to run. `session` spawns a dedicated headless
    /// sway session per connection (see PLAN-headless-session.md);
    /// `test-pattern` uses a synthetic moving gradient with no session or
    /// compositor at all, for exercising the rest of the pipeline without a
    /// real display or GPU.
    #[arg(long, value_enum, default_value_t = Source::Session)]
    source: Source,

    /// Test-pattern resolution (only used with --source test-pattern; real
    /// capture follows the client's reported viewport, or --mode).
    #[arg(long, default_value_t = 1280)]
    width: u32,
    #[arg(long, default_value_t = 720)]
    height: u32,

    /// Encoder tuning hint (rate control timebase, GOP length) and the
    /// screencopy capture-request rate cap.
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

    /// Fixed `WIDTHxHEIGHT[@SCALE]` override — pins the resolution instead
    /// of following the client's reported viewport; `RequestMode` is then
    /// ignored (the client is expected to letterbox) rather than honoured.
    /// Only meaningful with `--source session`.
    #[arg(long)]
    mode: Option<String>,

    /// xkb `options` string the virtual keyboard's keymap is compiled with
    /// (only used with --source session). Defaults to this box's Right
    /// Alt/Right Ctrl remaps — see PLAN-headless-session.md item 3.
    #[cfg(target_os = "linux")]
    #[arg(long, default_value = "lv3:ralt_switch,lv5:rctrl_switch")]
    xkb_options: String,

    /// The user's real sway config to build each session's headless
    /// overlay from (only used with --source session).
    #[cfg(target_os = "linux")]
    #[arg(long, default_value_os_t = default_sway_config_path())]
    sway_config: PathBuf,

    /// The `sway-session` wrapper to launch sessions through (only used
    /// with --source session).
    #[cfg(target_os = "linux")]
    #[arg(long, default_value_os_t = default_sway_session_path())]
    sway_session: PathBuf,

    /// Lines in the user's sway config matching this regex are dropped from
    /// each session's overlay (only used with --source session) — see
    /// `dragonvnc_session`'s module doc for why `exec swayidle` is the
    /// default.
    #[cfg(target_os = "linux")]
    #[arg(long, default_value = r"^\s*exec\s+swayidle")]
    exec_filter: String,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Source {
    /// Spawn a dedicated headless sway session per connection and capture
    /// its output via wlr-screencopy — the normal mode. Linux only.
    Session,
    /// A moving gradient, useful for testing the rest of the pipeline
    /// without a real display or GPU.
    TestPattern,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum Codec {
    /// Raw pixels, no compression. Works everywhere, useful for proving
    /// the rest of the pipeline independent of any encoder.
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

fn default_paired_clients_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("dragonvnc")
        .join("paired_clients")
}

fn parse_fingerprint_hex(s: &str) -> anyhow::Result<dragonvnc_net::Fingerprint> {
    let bytes = (0..s.len())
        .step_by(2)
        .map(|i| {
            s.get(i..i + 2)
                .and_then(|byte| u8::from_str_radix(byte, 16).ok())
                .ok_or_else(|| anyhow::anyhow!("{s:?} is not a valid hex fingerprint"))
        })
        .collect::<anyhow::Result<Vec<u8>>>()?;
    bytes
        .try_into()
        .map_err(|v: Vec<u8>| anyhow::anyhow!("expected a 32-byte (64 hex char) fingerprint, got {} bytes", v.len()))
}

fn clients_command(action: ClientsAction) -> anyhow::Result<()> {
    let path = default_paired_clients_path();
    let mut store = PairedClients::load_from(&path)?;
    match action {
        ClientsAction::List => {
            let mut any = false;
            for fp in store.list() {
                println!("{}", hex(fp));
                any = true;
            }
            if !any {
                println!("(no paired clients)");
            }
        }
        ClientsAction::Revoke { fingerprint } => {
            let fp = parse_fingerprint_hex(&fingerprint)?;
            if store.revoke(&fp) {
                store.save_to(&path)?;
                println!("revoked {}", hex(&fp));
            } else {
                println!("{} was not paired", hex(&fp));
            }
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command {
        Command::Run(args) => run(args).await,
        #[cfg(target_os = "linux")]
        Command::SessionReady => {
            dragonvnc_session::handoff::send_ready().await?;
            Ok(())
        }
        #[cfg(target_os = "linux")]
        Command::ProbeSession(args) => probe_session(args).await,
        Command::Clients(args) => clients_command(args.action),
    }
}

fn parse_mode(s: &str) -> anyhow::Result<Viewport> {
    let (dims, scale) = match s.split_once('@') {
        Some((dims, scale)) => (dims, scale.parse()?),
        None => (s, 1.0),
    };
    let (w, h) = dims
        .split_once('x')
        .ok_or_else(|| anyhow::anyhow!("expected WIDTHxHEIGHT[@SCALE], got {s:?}"))?;
    Ok(Viewport { width: w.parse()?, height: h.parse()?, scale })
}

/// Verifies item 1 end to end with no client involved: start a session,
/// confirm the handoff values, resize it live and check `get_outputs`
/// reflects it, sleep, tear down, then assert nothing leaked. See
/// PLAN-headless-session.md item 1's "Verify" section — run this twice in a
/// row to catch stale-socket/stale-unit bugs.
#[cfg(target_os = "linux")]
async fn probe_session(args: ProbeSessionArgs) -> anyhow::Result<()> {
    let viewport = parse_mode(&args.mode)?;
    let server_bin = std::env::current_exe()?;
    let runtime_dir = default_runtime_dir();
    let opts =
        dragonvnc_session::SessionOptions::new(args.sway_config, args.sway_session, server_bin, runtime_dir.clone());

    println!("starting session at {}x{}@{}...", viewport.width, viewport.height, viewport.scale);
    let session = dragonvnc_session::SessionHandle::start(&opts, viewport).await?;
    let unit_name = session.unit_name().to_string();
    println!(
        "session {} ready: WAYLAND_DISPLAY={} SWAYSOCK={}",
        session.id(),
        session.wayland_display(),
        session.sway_socket().display()
    );

    println!("resizing to 2560x1440...");
    session.set_mode(Viewport { width: 2560, height: 1440, scale: 1.0 }).await?;

    let outputs = tokio::process::Command::new("swaymsg")
        .arg("-s")
        .arg(session.sway_socket())
        .args(["-t", "get_outputs"])
        .output()
        .await?;
    let outputs_json = String::from_utf8_lossy(&outputs.stdout);
    anyhow::ensure!(
        outputs.status.success() && outputs_json.contains("2560") && outputs_json.contains("1440"),
        "get_outputs did not reflect the resize to 2560x1440: {outputs_json}"
    );
    println!("resize verified via get_outputs");

    println!("sleeping 3s...");
    tokio::time::sleep(Duration::from_secs(3)).await;

    let wayland_display = session.wayland_display().to_string();
    session.stop().await?;
    println!("session stopped");

    let leaked_units = tokio::process::Command::new("systemctl")
        .args(["--user", "list-units", &format!("{unit_name}*"), "--all", "--no-legend"])
        .output()
        .await?;
    let leaked_units_out = String::from_utf8_lossy(&leaked_units.stdout);
    anyhow::ensure!(
        leaked_units_out.trim().is_empty(),
        "unit leaked after stop(): {leaked_units_out}"
    );

    // `runtime_dir` is `$XDG_RUNTIME_DIR/dragonvnc`; its parent is the real
    // XDG runtime dir sway's socket lived in.
    let xdg_runtime_dir = runtime_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("runtime dir {} has no parent", runtime_dir.display()))?;
    let leaked_socket = xdg_runtime_dir.join(&wayland_display);
    anyhow::ensure!(!leaked_socket.exists(), "wayland socket leaked after stop(): {}", leaked_socket.display());

    println!("probe-session OK: no leaked units or sockets");
    Ok(())
}

/// Clamps a requested viewport to the active codec's supported resolution
/// range. Only `vaapi-hevc` has a hard range (see
/// `dragonvnc_codec::vaapi`'s doc); `passthrough` and the test-pattern path
/// accept anything (`dragonvnc_session::SessionHandle::set_mode` still
/// rounds to even numbers for NV12, independently of this).
fn clamp_viewport(codec: Codec, v: Viewport) -> Viewport {
    match codec {
        #[cfg(target_os = "linux")]
        Codec::VaapiHevc => Viewport {
            width: v.width.clamp(dragonvnc_codec::vaapi::MIN_WIDTH, dragonvnc_codec::vaapi::MAX_WIDTH),
            height: v.height.clamp(dragonvnc_codec::vaapi::MIN_HEIGHT, dragonvnc_codec::vaapi::MAX_HEIGHT),
            ..v
        },
        _ => v,
    }
}

async fn run(args: RunArgs) -> anyhow::Result<()> {
    let identity_path = args.identity_path.clone().unwrap_or_else(default_identity_path);
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
    // operator mint a new one per pairing attempt on demand instead; a
    // pinned-reconnect path — dragonvnc-net already supports and tests it —
    // is also still to be wired into this CLI, see item 5).
    let code = pairing::PairingCode::generate();
    println!("=== pairing code (enter on client): {code} ===");

    let fixed_mode = args.mode.as_deref().map(parse_mode).transpose()?;
    let args = Arc::new(args);
    // Enforces "one session, one client" (see PLAN-headless-session.md item
    // 4): only one `handle_connection` may be mid-flight at a time. Checked
    // (and, on success, set) *before* the incoming connection is even
    // accepted, so a second connection is refused ahead of pairing — a
    // stranger can't burn the pairing code trying while a real session is
    // active.
    let busy = Arc::new(AtomicBool::new(false));

    // Names whichever headless session is currently active, so a SIGTERM/
    // SIGINT (the exact signals `systemctl stop`/Ctrl-C send — see item 6's
    // deployment) can still stop it before the process exits. Found the
    // hard way while testing item 4: without this, killing the *server*
    // process (not just a connection) leaves its active session's scope
    // running forever — a systemd scope's lifetime isn't tied to its
    // launcher process's, so nothing else would ever tell it to stop.
    #[cfg(target_os = "linux")]
    let active_unit: Arc<std::sync::Mutex<Option<String>>> = Arc::new(std::sync::Mutex::new(None));
    #[cfg(target_os = "linux")]
    {
        let active_unit = active_unit.clone();
        tokio::spawn(async move {
            let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("installing a SIGTERM handler should never fail");
            let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
                .expect("installing a SIGINT handler should never fail");
            tokio::select! {
                _ = sigterm.recv() => tracing::info!("received SIGTERM"),
                _ = sigint.recv() => tracing::info!("received SIGINT"),
            }
            let unit = active_unit.lock().expect("not poisoned").take();
            if let Some(unit) = unit {
                tracing::info!(unit, "shutting down: stopping the active headless session first");
                let scope = format!("{unit}.scope");
                let _ = tokio::process::Command::new("systemctl").args(["--user", "stop", &scope]).status().await;
            }
            std::process::exit(0);
        });
    }

    loop {
        let Some(incoming) = ep.accept().await else {
            tracing::info!("endpoint stopped accepting (shutting down)");
            break;
        };
        let peer = incoming.remote_address();

        if busy.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            tracing::info!(%peer, "refusing connection: a session is already active");
            tokio::spawn(async move {
                match incoming.await {
                    Ok(connection) => connection.close(CLOSE_CODE_BUSY.into(), b"busy: another client is connected"),
                    Err(e) => {
                        tracing::debug!(error = %e, %peer, "busy-refused connection failed its own handshake before it could be closed")
                    }
                }
            });
            continue;
        }

        tracing::debug!(%peer, "incoming connection");
        let args = args.clone();
        let code = code.clone();
        let busy = busy.clone();
        #[cfg(target_os = "linux")]
        let active_unit = active_unit.clone();
        tokio::spawn(async move {
            let result = handle_connection(
                incoming,
                args,
                code,
                fixed_mode,
                #[cfg(target_os = "linux")]
                active_unit,
            )
            .await;
            busy.store(false, Ordering::SeqCst);
            if let Err(e) = result {
                tracing::warn!(error = %e, "connection ended with error");
            }
        });
    }
    Ok(())
}

async fn make_source(
    args: &RunArgs,
    #[cfg(target_os = "linux")] wayland_socket: Option<&std::path::Path>,
) -> anyhow::Result<Box<dyn FrameSource>> {
    match args.source {
        Source::TestPattern => Ok(Box::new(TestPatternSource::new(args.width, args.height, args.fps))),
        #[cfg(target_os = "linux")]
        Source::Session => {
            let socket = wayland_socket.expect("Source::Session always provides a socket");
            Ok(Box::new(dragonvnc_capture::screencopy::ScreencopySource::new(socket, Some(args.fps))?))
        }
        #[cfg(not(target_os = "linux"))]
        Source::Session => {
            anyhow::bail!("--source session is Linux-only; this build was compiled for a different target")
        }
    }
}

fn make_injector(
    args: &RunArgs,
    #[cfg(target_os = "linux")] wayland_socket: Option<&std::path::Path>,
    #[cfg(target_os = "linux")] width: u32,
    #[cfg(target_os = "linux")] height: u32,
) -> anyhow::Result<Box<dyn InputInjector>> {
    match args.source {
        Source::TestPattern => Ok(Box::new(dragonvnc_input::LoggingInjector)),
        #[cfg(target_os = "linux")]
        Source::Session => {
            let socket = wayland_socket.expect("Source::Session always provides a socket");
            Ok(Box::new(dragonvnc_input::linux::LinuxInjector::new(
                socket,
                width,
                height,
                Some(args.xkb_options.clone()),
            )?))
        }
        #[cfg(not(target_os = "linux"))]
        Source::Session => Ok(Box::new(dragonvnc_input::LoggingInjector)),
    }
}

fn make_encoder(
    codec: Codec,
    #[cfg(target_os = "linux")] width: u32,
    #[cfg(target_os = "linux")] height: u32,
    #[cfg(target_os = "linux")] fps: u32,
    #[cfg(target_os = "linux")] bitrate: i64,
    #[cfg(target_os = "linux")] src_format: dragonvnc_capture::PixelFormat,
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
            src_format,
        )?)),
        #[cfg(not(target_os = "linux"))]
        Codec::VaapiHevc => anyhow::bail!(
            "--codec vaapi-hevc is Linux-only (VAAPI); this build was compiled for a different target"
        ),
    }
}

async fn handle_connection(
    incoming: quinn::Incoming,
    args: Arc<RunArgs>,
    code: pairing::PairingCode,
    fixed_mode: Option<Viewport>,
    #[cfg(target_os = "linux")] active_unit: Arc<std::sync::Mutex<Option<String>>>,
) -> anyhow::Result<()> {
    let connection = incoming.await?;
    tracing::info!(peer = %connection.remote_address(), "connection established");

    // Mutual TLS (item 5) means every connection — paired or not —
    // presents a client certificate; `AcceptAnyClient` on the server side
    // only proved the client holds its private key, not who it is. That
    // check happens here, against the paired-client-device store: known
    // fingerprint ⇒ skip pairing entirely (the client independently makes
    // the same decision from its own server-trust store, so it only opens
    // a pairing stream when it needs to — see `dragonvnc-client`); unknown
    // ⇒ run the real ceremony and pin it on success.
    let peer_fingerprint = dragonvnc_net::identity::peer_fingerprint(&connection)
        .ok_or_else(|| anyhow::anyhow!("client presented no certificate — client auth is mandatory"))?;
    let paired_clients_path = default_paired_clients_path();
    let mut paired_clients = PairedClients::load_from(&paired_clients_path)?;
    if paired_clients.is_paired(&peer_fingerprint) {
        tracing::info!(fingerprint = %hex(&peer_fingerprint), "known client, skipping pairing ceremony");
    } else {
        let (mut send, mut recv) = connection.accept_bi().await?;
        pairing::run(&connection, &mut send, &mut recv, &code).await?;
        paired_clients.pair(peer_fingerprint);
        paired_clients.save_to(&paired_clients_path)?;
        // Every pairing success lands in the journal at `info` — this is
        // the audit trail for "a new device paired" (see item 5); the
        // pairing code itself stays valid afterward (not single-use
        // globally) so a second device can pair with the same code.
        tracing::info!(fingerprint = %hex(&peer_fingerprint), "pairing succeeded, client fingerprint pinned");
    }

    // Control stream: Hello/Welcome handshake.
    let (mut ctrl_send, mut ctrl_recv) = connection.accept_bi().await?;
    let hello: ControlMessage = recv_msg(&mut ctrl_recv).await?;
    let ControlMessage::Hello { protocol_version, client_name, viewport } = hello else {
        anyhow::bail!("expected Hello, got {hello:?}");
    };
    anyhow::ensure!(
        protocol_version == PROTOCOL_VERSION,
        "protocol version mismatch: client={protocol_version} server={PROTOCOL_VERSION}"
    );
    tracing::info!(%client_name, ?viewport, "client said hello");

    // Only meaningful for `--source session` (Linux-only): sizes the
    // headless compositor before capture starts. Nothing on other
    // platforms consumes it yet — there's no real capture backend there to
    // size.
    #[cfg(target_os = "linux")]
    let initial_viewport = clamp_viewport(args.codec, fixed_mode.unwrap_or(viewport));

    // Only ever `Some` for `--source session` — spawned before capture
    // starts (screencopy/input need its socket) and stopped at the very
    // end of this function. Every exit path after this point (including an
    // early `?` return) still tears the session down: `SessionHandle`'s
    // `Drop` is a last-resort net for exactly that — see its doc.
    #[cfg(target_os = "linux")]
    let mut session: Option<dragonvnc_session::SessionHandle> = None;
    #[cfg(target_os = "linux")]
    if matches!(args.source, Source::Session) {
        let runtime_dir = default_runtime_dir();
        let exec_filter = regex::Regex::new(&args.exec_filter)
            .map_err(|e| anyhow::anyhow!("invalid --exec-filter regex {:?}: {e}", args.exec_filter))?;
        let mut opts = dragonvnc_session::SessionOptions::new(
            args.sway_config.clone(),
            args.sway_session.clone(),
            std::env::current_exe()?,
            runtime_dir,
        );
        opts.exec_filter = exec_filter;
        let started = dragonvnc_session::SessionHandle::start(&opts, initial_viewport).await?;
        *active_unit.lock().expect("not poisoned") = Some(started.unit_name().to_string());
        session = Some(started);
    }
    #[cfg(target_os = "linux")]
    let wayland_socket_path = session.as_ref().map(|s| {
        // sway's socket lives directly in `$XDG_RUNTIME_DIR`, addressed by
        // the bare `wayland-N` name `SessionHandle` reports — never this
        // process's own (nonexistent) `WAYLAND_DISPLAY` env var.
        default_runtime_dir()
            .parent()
            .expect("$XDG_RUNTIME_DIR/dragonvnc always has a parent")
            .join(s.wayland_display())
    });

    let session_started = Instant::now();
    // Everything from here down is wrapped so a `?` anywhere inside (a
    // failed capture start, a bad frame, a dead QUIC stream) still reaches
    // the session teardown below — not just the happy-path fallthrough.
    // Plain `async` (not `async move`): it only ever borrows `session`
    // (`.as_ref()`), so `session` itself stays owned by this function and
    // is still here, to tear down, once this block's result comes back.
    let result: anyhow::Result<(u64, u64)> = async {
        // Capture must actually start before we can tell the client the real
        // resolution: screencopy doesn't know the negotiated buffer shape
        // until the compositor reports it, which can only happen after
        // connecting.
        let mut source: Box<dyn FrameSource> = make_source(
            &args,
            #[cfg(target_os = "linux")]
            wayland_socket_path.as_deref(),
        )
        .await?;
        let first_frame = source
            .next_frame()
            .await?
            .ok_or_else(|| anyhow::anyhow!("capture source produced no frames"))?;
        tracing::info!(
            width = first_frame.info.width,
            height = first_frame.info.height,
            format = ?first_frame.info.format,
            "capture started"
        );
        let mut injector: Box<dyn InputInjector> = make_injector(
            &args,
            #[cfg(target_os = "linux")]
            wayland_socket_path.as_deref(),
            #[cfg(target_os = "linux")]
            first_frame.info.width,
            #[cfg(target_os = "linux")]
            first_frame.info.height,
        )?;

        send_msg(
            &mut ctrl_send,
            &ControlMessage::Welcome {
                protocol_version: PROTOCOL_VERSION,
                server_name: "dragonvnc-server".into(),
                displays: vec![dragonvnc_proto::DisplayInfo {
                    id: 0,
                    width: first_frame.info.width,
                    height: first_frame.info.height,
                    refresh_hz: args.fps,
                }],
            },
        )
        .await?;

        let mut encoder = make_encoder(
            args.codec,
            #[cfg(target_os = "linux")]
            first_frame.info.width,
            #[cfg(target_os = "linux")]
            first_frame.info.height,
            #[cfg(target_os = "linux")]
            args.fps,
            #[cfg(target_os = "linux")]
            args.bitrate,
            #[cfg(target_os = "linux")]
            first_frame.info.format,
            #[cfg(target_os = "linux")]
            &args.vaapi_device,
        )?;

        // Resize requests flow input_task -> here: input_task owns `injector`
        // (so it can update pointer extents immediately) and is the one reading
        // `RequestMode` off the control stream; this task owns `session` and
        // `encoder`, which are what actually need rebuilding. See module doc.
        let (resize_tx, mut resize_rx) = tokio::sync::mpsc::channel::<Viewport>(4);

        // Input: further messages on the control stream after Hello/Welcome
        // (input events and resize requests; clipboard would land here too).
        // Runs concurrently with the video loop below so a burst of mouse
        // moves can never queue behind a video frame or vice versa. Sharing
        // the control stream for both input and clipboard/resize (rather than
        // input getting its own dedicated stream, as DESIGN.md calls for) is a
        // v1 simplification — revisit if clipboard traffic ever needs to not
        // queue behind input.
        let codec = args.codec;
        let input_task = tokio::spawn(async move {
            let mut injected = 0u64;
            // Starts already-elapsed so the very first RequestMode isn't
            // debounced away.
            let mut last_resize = Instant::now() - Duration::from_millis(250);
            loop {
                match recv_msg(&mut ctrl_recv).await {
                    Ok(ControlMessage::Input(event)) => {
                        tracing::trace!(?event, "input event received");
                        let started = Instant::now();
                        if let Err(e) = injector.inject(event).await {
                            tracing::warn!(error = %e, "failed to inject input event");
                        }
                        let elapsed = started.elapsed();
                        if elapsed > Duration::from_millis(50) {
                            // Injection is a handful of syscalls/socket writes —
                            // it should never take this long. If it does, that's
                            // a real lead on a "the desktop stopped responding"
                            // freeze report: the injector (wlr socket flush) is
                            // itself blocking.
                            tracing::warn!(?elapsed, "input injection took unusually long");
                        }
                        injected += 1;
                    }
                    Ok(ControlMessage::RequestMode { width, height, scale, .. }) => {
                        if fixed_mode.is_some() {
                            tracing::debug!("ignoring RequestMode: server was started with --mode, client should letterbox");
                            continue;
                        }
                        if last_resize.elapsed() < Duration::from_millis(250) {
                            tracing::trace!("debouncing RequestMode (< 250ms since the last one)");
                            continue;
                        }
                        last_resize = Instant::now();
                        let clamped = clamp_viewport(codec, Viewport { width, height, scale });
                        tracing::info!(?clamped, "applying RequestMode");
                        injector.set_extents(clamped.width, clamped.height);
                        if resize_tx.send(clamped).await.is_err() {
                            tracing::debug!("video task gone, stopping input task too");
                            break;
                        }
                    }
                    Ok(other) => tracing::debug!(?other, "ignoring non-input/resize control message"),
                    Err(e) => {
                        // Not silent anymore: a "the client stopped responding"
                        // report is indistinguishable from "the control stream
                        // died and nobody logged why" unless this is visible.
                        tracing::info!(error = %e, injected, "control stream ended, stopping input task");
                        break;
                    }
                }
            }
        });

        // Video stream: capture -> encode -> length-prefixed frames on a
        // dedicated reliable uni stream. Real transport tuning (chunked,
        // loss-tolerant unreliable datagrams once encoded frames are
        // realistically small) is next-milestone work — see DESIGN.md.
        let mut video_send = connection.open_uni().await?;
        let mut frame_id = 0u64;
        // Tracked separately from each frame so a resize's encoder rebuild
        // still knows the source format even though, by then, `pending_frame`
        // has long since been consumed — see the loop body below, which keeps
        // this updated from every frame actually captured. Only `make_encoder`
        // (Linux-only construction) ever reads this.
        #[cfg(target_os = "linux")]
        let mut src_format = first_frame.info.format;
        let mut pending_frame = Some(first_frame);
        let mut stats_window_started = Instant::now();
        let mut frames_this_window = 0u32;
        let mut bytes_this_window = 0u64;
        let mut total_frames_sent = 0u64;
        let mut total_bytes_sent = 0u64;
        // NOTE: encode() below runs synchronous, blocking FFI (CPU pixel
        // conversion + a VAAPI submission) directly inside this async task.
        // Fine for this milestone's frame rates; a real deployment should move
        // this to `spawn_blocking` so a slow encode can't stall other tokio
        // tasks on the same worker thread.
        'outer: loop {
            tokio::select! {
                biased;
                resize = resize_rx.recv() => {
                    let Some(new_viewport) = resize else {
                        // Input task ended (control stream died) — the video
                        // loop's own `send_video_frame` failures will notice
                        // and stop things shortly; nothing to do here but
                        // stop watching a channel whose only sender is gone.
                        continue 'outer;
                    };
                    #[cfg(target_os = "linux")]
                    if let Some(session) = session.as_ref() {
                        if let Err(e) = session.set_mode(new_viewport).await {
                            tracing::warn!(error = %e, "failed to apply live resize to the session, keeping current encoder");
                            continue 'outer;
                        }
                    }
                    match make_encoder(
                        args.codec,
                        #[cfg(target_os = "linux")]
                        new_viewport.width,
                        #[cfg(target_os = "linux")]
                        new_viewport.height,
                        #[cfg(target_os = "linux")]
                        args.fps,
                        #[cfg(target_os = "linux")]
                        args.bitrate,
                        #[cfg(target_os = "linux")]
                        src_format,
                        #[cfg(target_os = "linux")]
                        &args.vaapi_device,
                    ) {
                        Ok(new_encoder) => {
                            encoder = new_encoder;
                            tracing::info!(?new_viewport, "resized: encoder rebuilt");
                        }
                        Err(e) => tracing::warn!(error = %e, "failed to rebuild encoder after resize, keeping the old one"),
                    }
                }
                frame_result = next_frame(&mut pending_frame, &mut source) => {
                    let frame = match frame_result? {
                        Some(f) => f,
                        None => {
                            tracing::info!("capture source produced no more frames, ending video stream");
                            break 'outer;
                        }
                    };
                    #[cfg(target_os = "linux")]
                    {
                        src_format = frame.info.format;
                    }

                    let encode_started = Instant::now();
                    let encoded_frames = encoder.encode(&frame)?;
                    let encode_elapsed = encode_started.elapsed();
                    if encode_elapsed > Duration::from_millis(200) {
                        // A hardware encoder should turn a frame around in low
                        // single-digit milliseconds. Anything in the hundreds is
                        // either a driver/GPU stall or this task got starved for
                        // CPU time — either way, a real lead if frames are
                        // visibly hitching.
                        tracing::warn!(?encode_elapsed, width = frame.info.width, height = frame.info.height, "encode() took unusually long");
                    }
                    tracing::trace!(?encode_elapsed, packets = encoded_frames.len(), "frame encoded");

                    for encoded in encoded_frames {
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
                        let send_started = Instant::now();
                        let send_result = send_video_frame(&mut video_send, &header, &encoded.payload).await;
                        let send_elapsed = send_started.elapsed();
                        if send_elapsed > Duration::from_millis(200) {
                            // A blocked/slow QUIC write here means the client (or
                            // the network path) can't keep up — congestion,
                            // packet loss, or the client-side decode/render loop
                            // stalling and no longer reading. This is the single
                            // most likely site for a "sometimes freezes" report
                            // to actually be born.
                            tracing::warn!(?send_elapsed, payload_len = encoded.payload.len(), "video frame send took unusually long — client/network may be falling behind");
                        }
                        if let Err(e) = send_result {
                            tracing::info!(
                                error = %e,
                                frame_id,
                                total_frames_sent,
                                total_bytes_sent,
                                alive = ?session_started.elapsed(),
                                "video stream write failed, ending session"
                            );
                            break 'outer; // client went away
                        }
                        frame_id += 1;
                        total_frames_sent += 1;
                        total_bytes_sent += encoded.payload.len() as u64;
                        frames_this_window += 1;
                        bytes_this_window += encoded.payload.len() as u64;
                    }

                    if stats_window_started.elapsed() >= Duration::from_secs(5) {
                        let stats = connection.stats();
                        tracing::info!(
                            fps = frames_this_window as f64 / stats_window_started.elapsed().as_secs_f64(),
                            mbps = (bytes_this_window as f64 * 8.0 / 1_000_000.0) / stats_window_started.elapsed().as_secs_f64(),
                            rtt_ms = stats.path.rtt.as_secs_f64() * 1000.0,
                            cwnd = stats.path.cwnd,
                            congestion_events = stats.path.congestion_events,
                            lost_packets = stats.path.lost_packets,
                            lost_bytes = stats.path.lost_bytes,
                            "video stream stats"
                        );
                        frames_this_window = 0;
                        bytes_this_window = 0;
                        stats_window_started = Instant::now();
                    }
                }
            }
        }
        input_task.abort();
        Ok((total_frames_sent, total_bytes_sent))
    }
    .await;

    // Unconditional: reached whether the block above returned `Ok` or hit a
    // `?` partway through (a failed capture start, a bad frame, a dead QUIC
    // stream) — this is the guarantee item 4 calls for, not just the happy
    // path. `SessionHandle`'s `Drop` is still a last-resort net on top (e.g.
    // a panic here), but shouldn't be the *normal* way this fires.
    #[cfg(target_os = "linux")]
    if let Some(session) = session.take() {
        // Clear this *before* `stop()`: once cleared, a concurrent SIGTERM
        // handler won't redundantly try to stop a unit we're already
        // stopping (harmless either way — `stop_unit` tolerates "already
        // gone" — but no reason to race it).
        *active_unit.lock().expect("not poisoned") = None;
        if let Err(e) = session.stop().await {
            tracing::warn!(error = %e, "error tearing down headless session (already-gone is fine; a real failure here means check the journal)");
        }
    }

    let (total_frames_sent, total_bytes_sent) = result?;
    tracing::info!(
        total_frames_sent,
        total_bytes_sent,
        alive = ?session_started.elapsed(),
        "connection handler exiting"
    );

    Ok(())
}

/// Takes the still-pending first frame if there is one, else awaits the
/// next one from `source` — the same "first frame was already consumed to
/// build Welcome/the initial encoder" bridge the pre-resize-support code
/// had, just pulled out so it can sit in a `tokio::select!` branch.
async fn next_frame(pending: &mut Option<RawFrame>, source: &mut Box<dyn FrameSource>) -> anyhow::Result<Option<RawFrame>> {
    if let Some(f) = pending.take() {
        return Ok(Some(f));
    }
    source.next_frame().await
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

/// Live-hardware integration tests: each one spins up a real headless sway
/// session (`dragonvnc_session`, item 1) and drives real Wayland-protocol
/// input against it (`dragonvnc_input::linux`, item 3), then checks the
/// result the way the plan itself calls for — `swaymsg -t get_seats`/
/// `get_tree`, exact numbers, no screenshot needed. `#[ignore]`d like the
/// codec crate's own hardware test: needs this box's GPU + `systemd --user`,
/// not something to run under a generic CI runner. Run with
/// `cargo test -p dragonvnc-server -- --ignored`.
#[cfg(all(test, target_os = "linux"))]
mod live_input_tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use dragonvnc_input::InputInjector;
    use dragonvnc_proto::{InputEvent, PointerButton, Viewport};

    /// The real `dragonvnc-server` binary, not `std::env::current_exe()` —
    /// under `cargo test` that's the *test harness* binary (libtest), and
    /// the overlay's `exec ... session-ready` would invoke libtest with a
    /// bogus filter argument and never report readiness (found the hard
    /// way — see git history for this fix). `CARGO_BIN_EXE_<name>` (Cargo's
    /// usual answer) isn't set for unit tests living in a bin's own
    /// `src/main.rs`, only for a separate `tests/` integration target, so
    /// this derives the path from where `cargo test` always puts the test
    /// harness relative to the real binary: `target/<profile>/deps/<harness>`
    /// and `target/<profile>/dragonvnc-server` share the same `target/<profile>/` parent.
    fn real_server_bin() -> PathBuf {
        let harness = std::env::current_exe().expect("current_exe() should always succeed");
        let target_profile_dir = harness
            .parent() // .../target/<profile>/deps
            .and_then(|p| p.parent()) // .../target/<profile>
            .expect("test harness path always has target/<profile>/deps/<name>");
        let bin = target_profile_dir.join("dragonvnc-server");
        assert!(
            bin.exists(),
            "expected the real dragonvnc-server binary at {} (run `cargo build -p dragonvnc-server` first)",
            bin.display()
        );
        bin
    }

    async fn start_probe_session(width: u32, height: u32) -> dragonvnc_session::SessionHandle {
        let opts = dragonvnc_session::SessionOptions::new(
            super::default_sway_config_path(),
            super::default_sway_session_path(),
            real_server_bin(),
            super::default_runtime_dir(),
        );
        dragonvnc_session::SessionHandle::start(&opts, Viewport { width, height, scale: 1.0 })
            .await
            .expect("failed to start probe session — needs a working headless sway on this box")
    }

    async fn swaymsg_json(sway_socket: &std::path::Path, args: &[&str]) -> serde_json::Value {
        let output = tokio::process::Command::new("swaymsg")
            .arg("-s")
            .arg(sway_socket)
            .args(args)
            .output()
            .await
            .expect("swaymsg failed to run");
        assert!(output.status.success(), "swaymsg {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
        serde_json::from_slice(&output.stdout).expect("swaymsg did not return valid JSON")
    }

    #[tokio::test]
    #[ignore]
    async fn pointer_and_keyboard_devices_attach_to_the_seat() {
        // `get_seats` on this sway/wlroots version (1.10.1) reports attached
        // *devices*, not an absolute cursor position (checked live — there
        // is no x/y field anywhere in its output, despite the plan's
        // initial assumption otherwise). So this checks what's actually
        // observable: that creating our virtual pointer and keyboard really
        // registers them on the compositor's seat, under their real
        // protocol object names — not just that the constructor call
        // returned `Ok` locally.
        let session = start_probe_session(1280, 720).await;
        let wayland_socket = super::default_runtime_dir()
            .parent()
            .unwrap()
            .join(session.wayland_display());

        let mut injector = dragonvnc_input::linux::LinuxInjector::new(&wayland_socket, 1280, 720, None)
            .expect("failed to create LinuxInjector against the probe session");
        injector.inject(InputEvent::PointerMove { x: 640.0, y: 360.0 }).await.unwrap();
        // Device registration is processed asynchronously by the
        // compositor; give it a moment before asking for the result.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let seats = swaymsg_json(session.sway_socket(), &["-t", "get_seats"]).await;
        let devices = seats[0]["devices"].as_array().expect("no devices array in get_seats output");
        let names: Vec<&str> = devices.iter().filter_map(|d| d["name"].as_str()).collect();
        assert!(
            names.contains(&"wlr_virtual_pointer_v1"),
            "expected a wlr_virtual_pointer_v1 device on the seat, got: {names:?}"
        );
        assert!(
            names.contains(&"wlr_virtual_keyboard_v1"),
            "expected a wlr_virtual_keyboard_v1 device on the seat, got: {names:?}"
        );

        session.stop().await.unwrap();
    }

    #[tokio::test]
    #[ignore]
    async fn keyboard_input_reaches_a_focused_terminal() {
        let session = start_probe_session(1280, 720).await;
        let runtime_dir = super::default_runtime_dir();
        let wayland_socket = runtime_dir.parent().unwrap().join(session.wayland_display());

        // Launch a terminal inside the session so there's something with
        // keyboard focus to type into.
        let _ = tokio::process::Command::new("swaymsg")
            .arg("-s")
            .arg(session.sway_socket())
            .args(["exec", "ghostty"])
            .status()
            .await;
        tokio::time::sleep(Duration::from_millis(800)).await;

        let mut injector = dragonvnc_input::linux::LinuxInjector::new(&wayland_socket, 1280, 720, None)
            .expect("failed to create LinuxInjector against the probe session");
        // KEY_H, KEY_I (evdev codes 35, 23) — "hi".
        for keycode in [35u32, 23] {
            injector.inject(InputEvent::Key { keycode, pressed: true }).await.unwrap();
            injector.inject(InputEvent::Key { keycode, pressed: false }).await.unwrap();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;

        // Doesn't independently verify the *content* "hi" landed keystroke-
        // for-keystroke (that would need reading the terminal's actual
        // screen contents, which sway's tree doesn't expose) — it proves
        // ghostty actually launched and is focused, and that sending key
        // events into that state doesn't error against a live compositor.
        // The wlr_virtual_keyboard_v1 device-attachment test above is what
        // proves the object itself registers correctly.
        let tree = swaymsg_json(session.sway_socket(), &["-t", "get_tree"]).await;
        let tree_text = tree.to_string();
        assert!(tree_text.contains("ghostty"), "expected a ghostty window in the tree: {tree_text}");

        session.stop().await.unwrap();
    }

    /// Finds a live sway socket that is NOT the one belonging to `exclude` —
    /// i.e. some *other* sway instance already running on this box (in
    /// practice: the physical seat0 session). Probes each candidate with a
    /// real IPC call rather than just listing files, since a stale socket
    /// path can linger after its sway process is long gone.
    async fn other_live_sway_socket(exclude: &std::path::Path) -> Option<PathBuf> {
        let entries = std::fs::read_dir("/run/user/1000").ok()?;
        for entry in entries.flatten() {
            let path = entry.path();
            let name = path.file_name()?.to_str()?;
            if !name.starts_with("sway-ipc.") || !name.ends_with(".sock") || path == exclude {
                continue;
            }
            let ok = tokio::process::Command::new("swaymsg")
                .arg("-s")
                .arg(&path)
                .args(["-t", "get_version"])
                .status()
                .await
                .map(|s| s.success())
                .unwrap_or(false);
            if ok {
                return Some(path);
            }
        }
        None
    }

    /// Regression test for a real bug found live (2026-09-07, testing item
    /// 7 against a real macOS client): ghostty is a single-instance GTK
    /// `GApplication` — launching it a second time doesn't start a new
    /// process, it asks whichever instance is *already running* (over the
    /// D-Bus session bus) to open a new window. Before `dbus-run-session`
    /// (see `dragonvnc_session::SessionHandle::start`'s doc), the headless
    /// session shared the physical session's bus, so that already-running
    /// instance was still connected to the *physical* Wayland display —
    /// the new window opened there, not in the headless session
    /// `$mod+Return` was pressed in, and typing into the (windowless)
    /// headless session went nowhere. Now each session gets its own
    /// private bus, so this must reproduce independently regardless of
    /// what's already running elsewhere. Skips itself if there's no other
    /// live sway to test against (e.g. running outside this box).
    #[tokio::test]
    #[ignore]
    async fn ghostty_opens_in_the_headless_session_even_with_another_instance_running_elsewhere() {
        let session = start_probe_session(1280, 720).await;
        let Some(other_sock) = other_live_sway_socket(session.sway_socket()).await else {
            eprintln!("no other live sway instance on this box — skipping (nothing to reproduce against)");
            session.stop().await.unwrap();
            return;
        };

        // Make sure a ghostty instance is already running against the
        // *other* (not-this-test's) compositor before we ever touch the
        // headless one — this is the precondition the bug needs.
        let _ = tokio::process::Command::new("swaymsg")
            .arg("-s")
            .arg(&other_sock)
            .args(["exec", "ghostty"])
            .status()
            .await;
        tokio::time::sleep(Duration::from_millis(800)).await;
        let other_tree_before = swaymsg_json(&other_sock, &["-t", "get_tree"]).await.to_string();
        assert!(
            other_tree_before.contains("ghostty"),
            "precondition failed: couldn't get a live ghostty window on the other compositor to begin with"
        );

        // Now launch ghostty in the *headless* session — same as `$mod+Return` would.
        let _ = tokio::process::Command::new("swaymsg")
            .arg("-s")
            .arg(session.sway_socket())
            .args(["exec", "ghostty"])
            .status()
            .await;
        tokio::time::sleep(Duration::from_millis(800)).await;

        let headless_tree = swaymsg_json(session.sway_socket(), &["-t", "get_tree"]).await.to_string();
        let headless_has_ghostty = headless_tree.contains("ghostty");

        session.stop().await.unwrap();

        assert!(
            headless_has_ghostty,
            "expected a new ghostty window in the headless session (private D-Bus bus should \
             keep single-instance activation from reaching the other compositor's already-\
             running instance) — got none: {headless_tree}"
        );
    }

    #[tokio::test]
    #[ignore]
    async fn pointer_button_reaches_the_compositor() {
        let session = start_probe_session(1280, 720).await;
        let runtime_dir = super::default_runtime_dir();
        let wayland_socket = runtime_dir.parent().unwrap().join(session.wayland_display());
        let mut injector = dragonvnc_input::linux::LinuxInjector::new(&wayland_socket, 1280, 720, None)
            .expect("failed to create LinuxInjector against the probe session");

        // Just proving the round trip doesn't error against a live
        // compositor — a real click has no observable side effect on an
        // empty desktop background to assert against without a window
        // under the cursor.
        injector.inject(InputEvent::PointerButton { button: PointerButton::Left, pressed: true }).await.unwrap();
        injector.inject(InputEvent::PointerButton { button: PointerButton::Left, pressed: false }).await.unwrap();

        session.stop().await.unwrap();
    }
}

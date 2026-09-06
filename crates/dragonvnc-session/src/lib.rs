//! Spawn, address, resize, and kill a dedicated headless sway session.
//!
//! This is the whole of "each connection logs in to its own, freshly
//! spawned, headless sway session" — see `PLAN-headless-session.md` item 1.
//! Nothing else in the workspace knows what "sway" is: callers get a
//! [`SessionHandle`] with a Wayland socket name and a sway IPC socket path,
//! and never see `systemd-run`, sway config syntax, or the handoff protocol.
//!
//! Linux/sway-only by construction (systemd --user scopes, `swaymsg`,
//! wlroots' headless backend) — this crate does not attempt to be portable,
//! matching the plan's explicit single-machine scope.
//!
//! ## Lifecycle
//!
//! [`SessionHandle::start`] writes a generated sway config overlay, spawns
//! `systemd-run --user --scope --collect --unit=dragonvnc-session-<id> -- sway-session
//! -c <overlay>` as a child process, and waits on a one-shot Unix-socket
//! handoff (see [`handoff`]) for the `WAYLAND_DISPLAY`/`SWAYSOCK` values sway
//! picked for itself — these are never visible in `/proc/<pid>/environ`
//! because sway only exports them into the environment of processes it
//! `exec`s afterward, so the overlay config's own `exec dragonvnc-server
//! session-ready` line is what reports them back.
//!
//! Teardown is **not** automatic on drop: callers must call
//! [`SessionHandle::stop`] on every exit path (see its doc). `Drop` is only a
//! last-resort net that logs loudly and best-effort-kills the unit from a
//! detached OS thread (not `tokio::spawn`, so it works even without an
//! active runtime) if `stop` was never called — that's a bug in the caller,
//! not a normal path.

use std::path::{Path, PathBuf};
use std::time::Duration;

use dragonvnc_proto::Viewport;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::process::{Child, Command};

pub mod handoff;

/// The one output wlroots' headless backend creates. Fixed name, verified
/// live on the reference box (see PLAN-headless-session.md).
pub const OUTPUT_NAME: &str = "HEADLESS-1";

/// Env var the launcher sets on the spawned session and [`handoff::send_ready`]
/// reads: path of the Unix socket to report `WAYLAND_DISPLAY`/`SWAYSOCK` on.
pub const HANDOFF_ENV: &str = "DRAGONVNC_HANDOFF";

const DEFAULT_RENDER_DEVICE: &str = "/dev/dri/renderD128";
const DEFAULT_HANDOFF_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to read sway config at {path}: {source}")]
    ReadConfig {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(
        "timed out after {0:?} waiting for the spawned sway session to report readiness \
         (check `journalctl --user -u dragonvnc-session-<id>.scope` for why sway didn't start)"
    )]
    HandoffTimeout(Duration),
    #[error("handoff protocol error: {0}")]
    Handoff(#[from] dragonvnc_proto::ProtoError),
    #[error("`{cmd}` exited with {status}: {stderr}")]
    CommandFailed {
        cmd: String,
        status: std::process::ExitStatus,
        stderr: String,
    },
    #[error(
        "session process exited before reporting readiness — check \
         `journalctl --user -u {unit}.scope`"
    )]
    ExitedBeforeReady { unit: String },
}

pub type Result<T> = std::result::Result<T, SessionError>;

/// Everything a [`SessionHandle`] needs to know that isn't per-connection
/// (that part is [`Viewport`]).
#[derive(Debug, Clone)]
pub struct SessionOptions {
    /// The user's real sway config, e.g. `~/.config/sway/config`.
    pub sway_config_path: PathBuf,
    /// The `sway-session` wrapper that sets IME env vars before `exec sway
    /// "$@"` — must forward its args (a one-line change if it doesn't yet).
    pub sway_session_path: PathBuf,
    /// Path to this same `dragonvnc-server` binary, so the overlay config's
    /// `exec` line can invoke `session-ready` inside the spawned session.
    pub dragonvnc_server_bin: PathBuf,
    /// Lines in the user's config matching this are dropped from the
    /// overlay rather than copied in verbatim. Default matches `exec
    /// swayidle` (see module doc on why: `swaymsg output * power off` in the
    /// headless session would stop the output rendering and stall capture).
    pub exec_filter: regex::Regex,
    /// DRM render node wlroots' headless backend renders on
    /// (`WLR_RENDER_DRM_DEVICE`) — same GPU the VAAPI encoder uses.
    pub render_device: String,
    /// How long to wait for the handoff socket to report readiness before
    /// giving up and killing the scope.
    pub handoff_timeout: Duration,
    /// Directory to write per-session overlay configs and handoff sockets
    /// into, e.g. `$XDG_RUNTIME_DIR/dragonvnc`.
    pub runtime_dir: PathBuf,
}

impl SessionOptions {
    pub fn new(
        sway_config_path: PathBuf,
        sway_session_path: PathBuf,
        dragonvnc_server_bin: PathBuf,
        runtime_dir: PathBuf,
    ) -> Self {
        Self {
            sway_config_path,
            sway_session_path,
            dragonvnc_server_bin,
            exec_filter: default_exec_filter(),
            render_device: DEFAULT_RENDER_DEVICE.to_string(),
            handoff_timeout: DEFAULT_HANDOFF_TIMEOUT,
            runtime_dir,
        }
    }
}

pub fn default_exec_filter() -> regex::Regex {
    regex::Regex::new(r"^\s*exec\s+swayidle").expect("static regex is valid")
}

/// A running headless sway session spawned for exactly one connection.
pub struct SessionHandle {
    id: String,
    unit_name: String,
    wayland_display: String,
    sway_socket: PathBuf,
    conf_path: PathBuf,
    handoff_sock_path: PathBuf,
    /// The `systemd-run --scope` child process. Confirmed live on the
    /// reference box (see PLAN-headless-session.md): without `--no-block`,
    /// `systemd-run --scope` execs the target command *as itself* — this
    /// child's PID is sway's PID, and killing the scope's cgroup delivers
    /// SIGTERM straight to this same process, so `child.wait()` really does
    /// observe the session ending.
    child: Child,
    stopped: bool,
}

impl SessionHandle {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn wayland_display(&self) -> &str {
        &self.wayland_display
    }

    pub fn sway_socket(&self) -> &Path {
        &self.sway_socket
    }

    pub fn unit_name(&self) -> &str {
        &self.unit_name
    }

    /// Spawns a fresh headless sway session sized to `viewport`. Blocks
    /// until sway has reported readiness over the handoff socket, or until
    /// `opts.handoff_timeout` elapses / the process exits early, whichever
    /// comes first.
    pub async fn start(opts: &SessionOptions, viewport: Viewport) -> Result<Self> {
        let id = random_id();
        let unit_name = format!("dragonvnc-session-{id}");
        tokio::fs::create_dir_all(&opts.runtime_dir).await?;

        let conf_path = opts.runtime_dir.join(format!("sway-{id}.conf"));
        let handoff_sock_path = opts.runtime_dir.join(format!("handoff-{id}.sock"));

        // A stale socket from a previous crashed run would make bind() fail.
        let _ = tokio::fs::remove_file(&handoff_sock_path).await;
        let listener = UnixListener::bind(&handoff_sock_path)?;

        write_overlay_conf(&conf_path, opts, viewport).await?;

        let handoff_env_value = handoff_sock_path.to_string_lossy().into_owned();
        let mut cmd = Command::new("systemd-run");
        cmd.args(["--user", "--scope", "--collect", &format!("--unit={unit_name}")]);
        for (k, v) in [
            ("WLR_BACKENDS", "headless"),
            ("WLR_LIBINPUT_NO_DEVICES", "1"),
            ("WLR_RENDERER", "gles2"),
            ("WLR_RENDER_DRM_DEVICE", opts.render_device.as_str()),
            ("XDG_SESSION_TYPE", "wayland"),
            (HANDOFF_ENV, handoff_env_value.as_str()),
        ] {
            cmd.arg(format!("--setenv={k}={v}"));
        }
        cmd.arg("--").arg(&opts.sway_session_path).arg("-c").arg(&conf_path);
        cmd.kill_on_drop(true);
        cmd.stdin(std::process::Stdio::null());
        let mut child = cmd.spawn()?;

        // Race the handoff against the child exiting early (bad config,
        // sway crash) so a dead session doesn't have to burn the full
        // timeout before we notice.
        tracing::debug!(handoff_sock = %handoff_sock_path.display(), "waiting for handoff connection");
        let accept_and_read = async {
            let (stream, _) = listener.accept().await?;
            tracing::debug!("handoff connection accepted, reading");
            read_handoff(stream).await
        };
        let handoff_result = tokio::select! {
            res = tokio::time::timeout(opts.handoff_timeout, accept_and_read) => {
                res.map_err(|_| SessionError::HandoffTimeout(opts.handoff_timeout)).and_then(|r| r)
            }
            _ = child.wait() => Err(SessionError::ExitedBeforeReady { unit: unit_name.clone() }),
        };

        let handoff = match handoff_result {
            Ok(h) => h,
            Err(e) => {
                // Must go through `systemctl stop`, not `child.start_kill()`:
                // `child` only tracks sway's own PID, but the scope's cgroup
                // also holds waybar/fcitx5/mako/etc. sway `exec`'d — killing
                // just sway leaves those running and the scope stuck
                // "active" indefinitely (found the hard way: see git log for
                // this fix — every failed `start()` was leaking a full
                // desktop's worth of processes that then fought later
                // attempts for the D-Bus name and the GPU).
                stop_unit(&unit_name, &mut child).await;
                let _ = tokio::fs::remove_file(&handoff_sock_path).await;
                let _ = tokio::fs::remove_file(&conf_path).await;
                return Err(e);
            }
        };
        // One-shot: the socket has done its job. Removing it now (rather
        // than only on session stop) means a stale one never lingers if the
        // process is killed before a graceful `stop()`.
        let _ = tokio::fs::remove_file(&handoff_sock_path).await;

        tracing::info!(
            id,
            unit = %unit_name,
            wayland_display = %handoff.wayland_display,
            sway_socket = %handoff.sway_socket,
            "headless sway session ready"
        );

        Ok(Self {
            id,
            unit_name,
            wayland_display: handoff.wayland_display,
            sway_socket: PathBuf::from(handoff.sway_socket),
            conf_path,
            handoff_sock_path,
            child,
            stopped: false,
        })
    }

    /// Live-resizes the headless output. `width`/`height` are rounded up to
    /// even numbers (the encoder's NV12 conversion needs that); clamping to
    /// the encoder's supported resolution range is the caller's job (this
    /// crate has no opinion on encoder limits).
    pub async fn set_mode(&self, viewport: Viewport) -> Result<()> {
        let (w, h) = round_up_to_even(viewport.width, viewport.height);
        let mode = format!("{w}x{h}");
        let scale = format!("{}", viewport.scale);
        run_swaymsg(&self.sway_socket, &["output", OUTPUT_NAME, "mode", &mode, "scale", &scale]).await
    }

    /// Tears the session down: `systemctl --user stop` the scope (killing
    /// sway and every process it `exec`'d — waybar, fcitx5, anything
    /// double-forked included, since the scope *is* their cgroup), with a 5s
    /// deadline before escalating to `SIGKILL`. Removes the overlay config
    /// and any leftover handoff socket.
    ///
    /// Callers must call this on every connection-exit path — the existing
    /// server loop has several early-exit paths, so hold the handle in a
    /// guard that calls this, not just in the happy path (see
    /// `PLAN-headless-session.md` item 1). `Drop` is only a last-resort net,
    /// not a substitute for calling this.
    pub async fn stop(mut self) -> Result<()> {
        self.stop_inner().await
    }

    async fn stop_inner(&mut self) -> Result<()> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;

        stop_unit(&self.unit_name, &mut self.child).await;

        let _ = tokio::fs::remove_file(&self.conf_path).await;
        let _ = tokio::fs::remove_file(&self.handoff_sock_path).await;
        tracing::info!(unit = %self.unit_name, "headless sway session torn down");
        Ok(())
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        if self.stopped {
            return;
        }
        // Last-resort net, not the normal path — see `stop`'s doc. Runs on a
        // detached OS thread (not `tokio::spawn`) so it works even if we're
        // being dropped outside a tokio runtime (e.g. a panicking test).
        tracing::error!(
            unit = %self.unit_name,
            "SessionHandle dropped without calling stop() — this is a caller bug \
             (a leaked headless sway session otherwise); killing best-effort"
        );
        let scope = format!("{}.scope", self.unit_name);
        let conf_path = self.conf_path.clone();
        let handoff_sock_path = self.handoff_sock_path.clone();
        std::thread::spawn(move || {
            let _ = std::process::Command::new("systemctl")
                .args(["--user", "stop", &scope])
                .status();
            let _ = std::fs::remove_file(&conf_path);
            let _ = std::fs::remove_file(&handoff_sock_path);
        });
    }
}

/// Stops a scope unit and waits for its tracked process to actually exit,
/// escalating to `SIGKILL` after 5s. This — not killing `child` directly —
/// is the only correct way to tear a session down: the scope's cgroup holds
/// every process sway `exec`'d (waybar, fcitx5, mako, double-forked
/// daemons), and killing just the tracked main PID leaves those running and
/// the scope stuck "active" indefinitely. Ignores every command's exit
/// status: a scope that's already gone makes `systemctl stop`/`kill` report
/// "not loaded", which isn't an error for us — the goal state (nothing
/// running) is already met either way.
async fn stop_unit(unit_name: &str, child: &mut Child) {
    let scope = format!("{unit_name}.scope");
    let _ = Command::new("systemctl").args(["--user", "stop", &scope]).status().await;

    if tokio::time::timeout(Duration::from_secs(5), child.wait()).await.is_err() {
        tracing::warn!(unit = unit_name, "session didn't stop within 5s, escalating to SIGKILL");
        let _ = Command::new("systemctl")
            .args(["--user", "kill", "-s", "SIGKILL", &scope])
            .status()
            .await;
        let _ = child.wait().await;
    }
}

fn round_up_to_even(w: u32, h: u32) -> (u32, u32) {
    (w + (w & 1), h + (h & 1))
}

fn random_id() -> String {
    use rand::Rng;
    let bytes: [u8; 8] = rand::thread_rng().gen();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn write_overlay_conf(conf_path: &Path, opts: &SessionOptions, viewport: Viewport) -> Result<()> {
    let user_config = tokio::fs::read_to_string(&opts.sway_config_path)
        .await
        .map_err(|source| SessionError::ReadConfig {
            path: opts.sway_config_path.clone(),
            source,
        })?;

    let mut overlay = String::with_capacity(user_config.len() + 256);
    let mut filtered_lines = 0u32;
    for line in user_config.lines() {
        if opts.exec_filter.is_match(line) {
            filtered_lines += 1;
            continue;
        }
        overlay.push_str(line);
        overlay.push('\n');
    }
    if filtered_lines > 0 {
        tracing::debug!(filtered_lines, path = %opts.sway_config_path.display(), "filtered lines out of headless session overlay config");
    }

    let (w, h) = round_up_to_even(viewport.width, viewport.height);
    overlay.push_str(&format!("\noutput {OUTPUT_NAME} mode {w}x{h} scale {}\n", viewport.scale));
    overlay.push_str(&format!("exec {} session-ready\n", opts.dragonvnc_server_bin.display()));

    tokio::fs::write(conf_path, overlay).await?;
    Ok(())
}

async fn run_swaymsg(sway_socket: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new("swaymsg").arg("-s").arg(sway_socket).args(args).output().await?;
    if !output.status.success() {
        return Err(SessionError::CommandFailed {
            cmd: format!("swaymsg -s {} {}", sway_socket.display(), args.join(" ")),
            status: output.status,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    Ok(())
}

/// Wire shape sent over the handoff socket. Internal to this crate; the
/// public surface is [`handoff::send_ready`] (write side, run inside the
/// spawned session) and `SessionHandle::start` (read side, run in the
/// launcher).
#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Handoff {
    pub(crate) wayland_display: String,
    pub(crate) sway_socket: String,
}

async fn read_handoff(mut stream: UnixStream) -> Result<Handoff> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > dragonvnc_proto::MAX_FRAME_LEN {
        return Err(SessionError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("handoff message declared {len} bytes, exceeding sanity limit"),
        )));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(dragonvnc_proto::decode(&buf)?)
}

pub(crate) async fn write_handoff(mut stream: UnixStream, handoff: &Handoff) -> Result<()> {
    let bytes = dragonvnc_proto::encode(handoff)?;
    stream.write_all(&(bytes.len() as u32).to_be_bytes()).await?;
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_up_to_even_rounds_only_odd_values() {
        assert_eq!(round_up_to_even(1920, 1080), (1920, 1080));
        assert_eq!(round_up_to_even(1921, 1079), (1922, 1080));
    }

    #[test]
    fn default_exec_filter_matches_swayidle_but_not_other_exec_lines() {
        let re = default_exec_filter();
        assert!(re.is_match("exec swayidle -w timeout 7200 'swaylock -f'"));
        assert!(re.is_match("    exec swayidle -w timeout 1 x")); // leading whitespace
        assert!(!re.is_match("exec waybar"));
        assert!(!re.is_match("exec_always swayidle-lookalike"));
    }

    #[tokio::test]
    async fn write_overlay_conf_filters_and_appends_mode_and_exec() {
        let dir = std::env::temp_dir().join(format!("dragonvnc-session-test-{}", random_id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let user_config_path = dir.join("config");
        tokio::fs::write(
            &user_config_path,
            "exec waybar\nexec swayidle -w timeout 1 lock\noutput * bg color solid\n",
        )
        .await
        .unwrap();

        let opts = SessionOptions::new(
            user_config_path,
            PathBuf::from("/bin/true"),
            PathBuf::from("/bin/dragonvnc-server"),
            dir.clone(),
        );
        let conf_path = dir.join("overlay.conf");
        write_overlay_conf(
            &conf_path,
            &opts,
            Viewport { width: 1921, height: 1079, scale: 2.0 },
        )
        .await
        .unwrap();

        let written = tokio::fs::read_to_string(&conf_path).await.unwrap();
        assert!(written.contains("exec waybar"));
        assert!(!written.contains("swayidle"));
        assert!(written.contains("output HEADLESS-1 mode 1922x1080 scale 2"));
        assert!(written.contains("exec /bin/dragonvnc-server session-ready"));

        let _ = tokio::fs::remove_dir_all(&dir).await;
    }

    #[tokio::test]
    async fn handoff_roundtrips_over_a_real_unix_socket() {
        let dir = std::env::temp_dir().join(format!("dragonvnc-handoff-test-{}", random_id()));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let sock_path = dir.join("handoff.sock");
        let listener = UnixListener::bind(&sock_path).unwrap();

        let sock_path2 = sock_path.clone();
        let writer = tokio::spawn(async move {
            let stream = UnixStream::connect(&sock_path2).await.unwrap();
            write_handoff(stream, &Handoff { wayland_display: "wayland-9".into(), sway_socket: "/tmp/x.sock".into() }).await.unwrap();
        });

        let (stream, _) = listener.accept().await.unwrap();
        let handoff = read_handoff(stream).await.unwrap();
        writer.await.unwrap();

        assert_eq!(handoff.wayland_display, "wayland-9");
        assert_eq!(handoff.sway_socket, "/tmp/x.sock");
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}

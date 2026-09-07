# Plan: remote login into a dedicated headless sway session

Replaces the "attach to the already-running desktop" model with "each
connection logs in to its own, freshly spawned, headless sway session."
Scoped against exactly one machine — this box (Debian 13, sway 1.10.1 /
wlroots 0.18, greetd+tuigreet, AMD RX 6700 XT, systemd 257, user `hafez`) —
with generalisation deliberately deferred.

## Decisions (settled 2026-09-06)

| Question | Decision |
|---|---|
| Privilege / auth | Server runs **as the user** under `systemd --user` (linger on). The **paired, pinned client device is the login**. No PAM, no root. |
| Session lifetime | **Dies with the connection.** Disconnect ⇒ the whole sway session and every app in it is torn down. |
| Resolution / scale | **Client-driven, live**: client reports its viewport (physical px + scale factor) at connect and on every resize; server applies it to the headless output. **CLI override** (`--mode WxH[@scale]`) pins it instead. |
| Session config | **The user's real config**, launched through the existing `~/.config/sway/sway-session` wrapper, with a generated overlay for the headless-specific bits. |
| Attach mode | **Dropped.** Portal/PipeWire capture and the uinput keyboard are deleted. One capture path, one input path, both aimed at the sway we spawned. |
| Zero-copy DMA-BUF | **Later** — planned below under "Future work", not in this milestone. |
| Pairing UX | **Print a code to the journal on every server start** (current behaviour, under systemd). Paired devices reconnect with pinned keys and never need it again. |
| Concurrency | **One session, one client.** A second connection while one is active is refused with a "busy" reason. |

## Machine facts verified before planning

Each of these was checked live on this box, not assumed:

- A second sway (`WLR_BACKENDS=headless WLR_LIBINPUT_NO_DEVICES=1
  WLR_RENDERER=gles2 WLR_RENDER_DRM_DEVICE=/dev/dri/renderD128 sway -c …`)
  starts from a plain ssh shell (no seat, no VT, no logind graphical session)
  alongside the live seat0 session, picks `wayland-2` + its own
  `sway-ipc.1000.<pid>.sock`, renders on the GPU via the render node, and
  creates one output `HEADLESS-1` at 1280x720 (refresh reported 0; wlroots'
  headless backend ticks at 60 Hz internally). `swaymsg output HEADLESS-1
  mode 2560x1440` resized it live. `get_inputs` is `[]` — **no input devices
  at all**, which is the root of the keyboard change below.
- Globals this sway advertises (probed over the wire): `zwlr_screencopy_manager_v1 v3`,
  `zwlr_virtual_pointer_manager_v1 v2`, `zwp_virtual_keyboard_manager_v1 v1`,
  `zwlr_data_control_manager_v1 v2`, `zwp_linux_dmabuf_v1 v4`,
  `zwlr_export_dmabuf_manager_v1 v1`, `zwlr_output_manager_v1 v4`. No
  `ext_image_copy_capture` (that is wlroots 0.19 / sway 1.11).
- `systemd-run --user` works from an ssh session; the user manager (session 4,
  class `manager`) is up. `Linger=no` today — must be enabled.
- The user's `sway-session` wrapper hardcodes `exec sway` (no args) — it needs
  a one-line change (`exec sway "$@"`) so we can pass `-c`.
- `xdg-desktop-portal` + `-wlr` run **once per user** and are already bound to
  the physical session's `wayland-1`. They cannot serve a second compositor
  for the same user. This is why the portal path goes away.
- `dragonvnc-net` is **server-only auth** today: `with_no_client_auth()`, the
  pairing ceremony pins the server's fingerprint on the client, and the server
  learns nothing durable about the client. Every connection re-pairs. That is
  fine for a demo that attaches to a desktop you're sitting at, and not fine
  for a login that hands out a full desktop session.

## Target architecture (one connection, start to finish)

```
client (Mac)                          server (systemd --user, as hafez)
------------                          ---------------------------------
QUIC connect  ───────────────────►    accept; if a session is active ⇒ close("busy")
TLS: client presents its own cert     require client cert; is its fingerprint pinned?
                                        yes ⇒ authenticated, skip pairing
                                        no  ⇒ demand pairing (code from journal),
                                               on success pin client fingerprint
Hello{viewport: 2560x1600 @2.0} ──►   SessionLauncher::start(viewport)
                                        1. write /run/user/1000/dragonvnc/sway-<id>.conf
                                        2. systemd-run --user --scope --unit=dragonvnc-session-<id> \
                                              env WLR_BACKENDS=headless … sway-session -c <conf>
                                        3. wait on handoff socket for {WAYLAND_DISPLAY, SWAYSOCK}
                                        4. swaymsg output HEADLESS-1 mode 2560x1600 scale 2
◄── Welcome{displays:[2560x1600]}     connect ScreencopySource + WlrPointer + VirtualKeyboard
                                        to that WAYLAND_DISPLAY (explicit socket, not env)
◄── video stream (HEVC)               screencopy → wl_shm → VAAPI HEVC → QUIC
──► Input(...)                        virtual pointer / virtual keyboard
──► RequestMode{w,h,scale} on resize  swaymsg output … mode …; recreate encoder; keyframe
connection closes (any reason)  ──►   systemctl --user stop dragonvnc-session-<id>.scope
                                      (kills sway + waybar + fcitx5 + everything it spawned)
```

## Work items, in build order

Each item is independently verifiable on this box over ssh without the Mac,
except where noted. Order is chosen so that every step leaves a working
server.

### 1. `dragonvnc-session` — spawn, address, resize, and kill a sway session

New crate. Owns everything about the compositor process; nothing else in the
workspace knows what "sway" is.

**Config generation.** Write `$XDG_RUNTIME_DIR/dragonvnc/sway-<id>.conf`
containing:

```
include /home/hafez/.config/sway/config
output HEADLESS-1 mode <W>x<H> scale <S>
exec dragonvnc-server session-ready      # see handoff below
```

Two known snags with `include`-ing the real config, both machine-specific:

- `exec swayidle … swaylock … output * power off` in the user's config will
  also run in the headless session. `power off` on the headless output stops
  it rendering, which stalls screencopy. First cut: **filter** — the
  launcher reads the user's config and drops lines matching a configurable
  regex (default `^\s*exec\s+swayidle`), writing the result into the overlay
  instead of `include`-ing it. The tidier long-term fix is for the user's
  config to move its `exec` lines into an `autostart.conf` that the overlay
  simply doesn't include; note that as an option, don't require it.
- `exec` lines in the included config that call `swaymsg` (the screenshot
  binds, plexamp toggle) read `SWAYSOCK` from sway's own environment, so they
  correctly target the headless instance. Nothing to do, but worth knowing.

**Spawn.** `systemd-run --user --scope --collect --unit=dragonvnc-session-<id>
--setenv=… -- /home/hafez/.config/sway/sway-session -c <conf>`, as a child
process. `--scope` matters: unlike `--unit` service mode, the command is
exec'd *by our child*, so the PID we get back is sway's, and the scope's
cgroup lets us kill fcitx5/waybar/mako/anything sway `exec`'d, including
double-forked daemons a plain process group would lose. Environment set for
sway only (not the server):

```
WLR_BACKENDS=headless
WLR_LIBINPUT_NO_DEVICES=1
WLR_RENDERER=gles2
WLR_RENDER_DRM_DEVICE=/dev/dri/renderD128
DRAGONVNC_HANDOFF=$XDG_RUNTIME_DIR/dragonvnc/handoff-<id>.sock
XDG_SESSION_TYPE=wayland   (so apps pick wayland backends)
```

Must **not** be set: `WAYLAND_DISPLAY` (wlroots would try to nest inside the
physical session), `DISPLAY`. The user's `sway-session` wrapper needs
`exec sway "$@"` so `-c` reaches sway — that is a change in the
`desktop-setup` repo, not here.

**Handoff (learning the socket names).** sway picks `wayland-N` and
`sway-ipc.<uid>.<pid>.sock` itself and only exports them into its *own*
environment after startup, so `/proc/<pid>/environ` never shows them. The
overlay's `exec dragonvnc-server session-ready` runs inside sway with both
variables set; it connects to `DRAGONVNC_HANDOFF` and sends
`WAYLAND_DISPLAY`, `SWAYSOCK`, and its own PID, then exits. The launcher
listens on that socket before spawning and waits up to 10 s. This doubles as
the "compositor is up and running config" signal; timeout ⇒ kill the scope,
fail the connection with the sway log path in the error.

**Resize.** `SessionHandle::set_mode(w, h, scale)` runs
`swaymsg -s $SWAYSOCK output HEADLESS-1 mode WxH scale S` (shell out first;
the sway IPC framing is 14 bytes of header and can be done natively later).
Round `w`/`h` to even numbers (NV12), clamp to the encoder's verified
130x128 … 8192x4352 range. Note: sway's headless mode is a custom mode, so
any WxH works; refresh stays at wlroots' internal 60 Hz.

**Teardown.** `Drop` for `SessionHandle` ⇒ `systemctl --user stop
dragonvnc-session-<id>.scope` (async, with a 5 s deadline then `kill-unit
SIGKILL`), remove the overlay conf and handoff socket. Teardown must be
unconditional on every connection exit path — the existing
`handle_connection` has several `break 'outer` paths; put the handle in a
guard, not in the happy path.

**Logging.** sway's stderr goes to the journal by virtue of the scope; also
pass `-d` only when the server is at `debug` level, it is very noisy.

**Verify (no Mac needed):** a `dragonvnc-server probe-session --mode
1920x1080` subcommand that starts a session, prints the handoff values,
resizes to 2560x1440, checks `swaymsg -t get_outputs` reflects it, sleeps 3 s,
tears down, then asserts `systemctl --user list-units 'dragonvnc-session-*'`
is empty and no `wayland-N` socket was leaked. Run it twice in a row to catch
stale-socket/stale-unit bugs.

### 2. Capture: `dragonvnc-capture::screencopy` replaces `pipewire`

`zwlr_screencopy_manager_v1` v3 talking to the session's `WAYLAND_DISPLAY`
(via `Connection::from_socket`, never `connect_to_env` — the server process
has no `WAYLAND_DISPLAY` and must not need one).

- Loop: `capture_output(overlay_cursor=1, wl_output)` → `buffer` event gives
  format/size/stride → allocate (or reuse) a `wl_shm` pool buffer of that
  shape → `copy` → `ready` (or `failed`) → hand a `RawFrame` to the channel →
  request the next capture. Frames only arrive when the compositor actually
  repaints, so idle screens produce no traffic, same as PipeWire did.
- `wl_shm` formats from wlroots here are `XRGB8888`/`ARGB8888` (little-endian
  ⇒ `Bgrx`/`Bgra`); map to the existing `PixelFormat`. Honour `flags`
  (`y_invert`) — the encoder path does not handle inverted rows, so invert on
  copy if it ever appears (it shouldn't on the GLES2 renderer, but check the
  flag, don't assume).
- Keep the same backpressure/instrumentation the PipeWire source has (queue
  depth warning, drop counters, 5 s stats). Add a `--fps` cap on the request
  loop so a busy screen can't outrun the encoder.
- Output geometry: bind `wl_output` v4 and track `mode`/`done`; after a
  resize the very next `buffer` event already has the new size, and the
  frame header carries width/height, so the client adapts per frame.
- Delete `pipewire.rs`, `ashpd`/`pipewire` deps, the restore-token path, the
  30 s portal timeout logic. Delete `TestPatternSource` only if nothing uses
  it (the `--source test-pattern` path is still handy for the Mac client's
  passthrough tests; keep it).

**Verify (no Mac needed):** with a probe session up, capture 10 frames to
PNG and diff against `WAYLAND_DISPLAY=wayland-N grim -o HEADLESS-1` taken at
the same moment (grim is an independent screencopy client). Then a resize
mid-capture must produce frames of the new size with no `failed` events.

### 3. Input: virtual keyboard, explicit display for the pointer, uinput deleted

- `WlrPointer::new` takes the session's Wayland socket, not the environment.
  Extents stay `width`/`height` of the current mode; `motion_absolute` is a
  ratio against the output, so scale factor does not change the math.
- New `wlr_keyboard.rs`: `zwp_virtual_keyboard_manager_v1.create_virtual_keyboard(seat)`,
  then `keymap(XKB_V1, memfd, len)` with a keymap compiled by `xkbcommon`
  (`rules=evdev, model=pc105, layout=us`, options from a new `--xkb-options`
  flag defaulting to this box's `lv3:ralt_switch,lv5:rctrl_switch` so Right
  Alt / Right Ctrl binds keep working). Track an `xkb_state` locally and send
  `modifiers(depressed, latched, locked, group)` after every `key`, as wayvnc
  does; wlroots does update its own state from `key`, but sending modifiers
  keeps it exact under held-modifier sequences. `key` takes evdev keycodes
  directly (same convention as `wl_keyboard.key`), which is what the wire
  protocol already carries.
- Delete `uinput.rs`, `input-linux` dep, the udev rule note. Rationale to
  record in the module doc: the headless compositor has no libinput backend,
  so a kernel virtual keyboard is delivered to the seat0 sway instead — it
  would type into the *physical* desktop.
- Repeat: sway handles key repeat compositor-side for virtual keyboards
  (`repeat_info`), so the server sends only press/release, as now.

**Verify (no Mac needed):** extend `--move-to`-style scripted tests: start a
probe session, launch `ghostty` in it via `swaymsg exec`, inject
`Mod4+Return`-free text (type `echo hi\n` via evdev codes), screencopy a
frame, OCR-free check: `swaymsg -t get_tree` shows the terminal focused, and
a pixel diff shows the region changed. Pointer: inject a move, then
`swaymsg -t get_seats` reports the cursor position — exact, no screenshot
needed.

### 4. Server wiring: one session per connection, busy refusal, live resize

`crates/dragonvnc-server/src/main.rs` currently spawns one task per accept
that does pairing → Hello → capture → stream. Restructure into:

- `main`: builds the endpoint, prints the pairing code to stdout (journal),
  holds an `Arc<Mutex<Option<ActiveSession>>>`.
- accept loop: if a session is active, `connection.close(code=BUSY,
  reason="busy: another client is connected")` immediately — before pairing,
  so a stranger can't burn the code — and log the peer.
- `handle_connection`: auth (item 5) → Hello (now carrying
  `viewport{width,height,scale}`) → `SessionLauncher::start` → sources bound
  to the session → stream loop; on every exit path, drop the session guard.
- `RequestMode` (already in the protocol, unused) is now honoured: debounce
  250 ms, `session.set_mode`, rebuild the VAAPI encoder at the new size, force
  a keyframe, and the pointer's extents. Resolution comes from the client
  unless `--mode WxH[@S]` was given, in which case `RequestMode` is answered
  with the fixed mode (client letterboxes).
- CLI: remove `--source pipewire`, `--width/--height` (keep for
  `test-pattern`), add `--mode`, `--xkb-options`, `--sway-config` (default
  `~/.config/sway/config`), `--sway-session` (default
  `~/.config/sway/sway-session`), `--exec-filter` regex.
- Subcommands: `run` (default), `session-ready` (handoff helper, item 1),
  `probe-session`.

Protocol: `Hello` gains `viewport: Viewport{width,height,scale: f32}`;
`RequestMode` gains `scale: f32`; bump `PROTOCOL_VERSION`. Busy is a QUIC
application close code + reason, no new message.

### 5. Client authentication: the paired device is the login

Today anyone who can reach UDP 5900 and read the pairing code can get in,
and nothing prevents a re-pair. Required for this model:

- **Client identity**: `ClientIdentity::load_or_generate` (same
  self-signed Ed25519 as `ServerIdentity`), stored at the Mac's data dir.
  Client endpoints (both `_for_pairing` and `_pinned`) present it.
- **Server side**: `server_endpoint` uses `with_client_cert_verifier` with an
  accept-any-but-prove-key verifier (mirror of `AcceptAnyForPairing`), then
  after the handshake reads `peer_identity()` and checks a **server-side
  `TrustStore`** (`~/.local/share/dragonvnc/paired_clients`). Pinned ⇒ skip
  pairing, go straight to Hello. Not pinned ⇒ require the pairing ceremony;
  on success pin the client's fingerprint. `pairing::run` already returns the
  peer fingerprint on both sides because the exporter-bound confirmation
  covers both certs — the server just discards it today.
- Pairing code lifetime: printed once per server start (as decided), but
  **single-use per client**; a failed attempt burns nothing, a success does
  not invalidate the code (the journal line stays useful for pairing a second
  device). Log every pairing success with the new fingerprint at `info` so
  the journal is an audit trail.
- `dragonvnc-server clients list|revoke <fingerprint>` for managing the
  store. No UI beyond that.

**Verify:** loopback tests in `dragonvnc-net`: unpinned client without code
is refused; pinned client connects with no pairing stream; a client
presenting a different cert with the same claimed name is refused. Then live
from the Mac: first connect pairs, second connect shows no code prompt.

### 6. Deployment on this box — done (2026-09-07)

- `~/.config/systemd/user/dragonvnc-server.service`: `ExecStart=%h/.cargo/bin/dragonvnc-server run --codec vaapi-hevc …`,
  `Restart=on-failure`, `Environment=RUST_LOG=info`. **No** `WAYLAND_DISPLAY`
  in its environment.
- `loginctl enable-linger hafez` so the user manager (and the server) exists
  with nobody logged in at the greeter. Note for the record: the physical
  greetd session keeps working exactly as before; the two compositors share
  the GPU through separate render-node handles.
- `desktop-setup` repo: `sway-session` gets `exec sway "$@"` (done as part of
  item 1, verified live there); CLAUDE.md there gets a "Remote access
  (dragonvnc)" section mirroring the wayvnc one.
- Port: UDP 5900 does not clash with wayvnc's TCP 5900; leave it.
- Pairing code: `journalctl --user -u dragonvnc-server -g 'pairing code'`.

Verified live: `cargo install --path crates/dragonvnc-server` (release
build), unit enabled + started, `systemctl --user status` shows
`active (running)`; a genuinely stale process from before this milestone
(the old `--source pipewire` build) was squatting UDP 5900 — killed (with
explicit go-ahead) so the new unit could bind it. A real client connected
through the deployed service end to end (pairing, ~6 Mbps HEVC stream,
clean session teardown after disconnect).

### 7. Client (macOS) — minimal, done last

- Send `viewport` in Hello from the window's physical size and
  `scale_factor()`; send `RequestMode` on `WindowEvent::Resized` /
  `ScaleFactorChanged` (debounced).
- Persist a `ClientIdentity`; use it for every connection; skip the code
  prompt when the server doesn't open a pairing stream.
- Show the close reason ("busy") in the title bar — the `NetworkError` →
  title path from the diagnostics pass already does this.
- Keep the `--move-to` scripted path; it is the verification tool for item 3.

## End-to-end verification, in order

1. `cargo test --workspace` on this box (VAAPI round-trip still passes; new
   loopback auth tests).
2. `probe-session` twice: start, resize, tear down, no leaks.
3. Capture parity against `grim` on the probe session.
4. Scripted input into a terminal in the probe session.
5. Server under `systemd --user`, `journalctl` shows the code.
6. Mac client: pair, see the headless desktop with waybar/wallpaper at Retina
   size, type into a terminal, resize the window and watch the desktop
   re-layout, close the client and confirm the scope and all its processes
   are gone within 5 s. Reconnect without a code.
7. Negative: second Mac connection (or a loopback client) while connected is
   refused with "busy"; a client with a fresh identity and no code is refused.
8. The physical seat0 session is untouched throughout (no new input devices,
   no keystrokes landing on it — this is the regression the uinput change
   guards against).

## Known limitations of this design (accepted, to record in DESIGN.md)

- **Fixed, not just accepted**: the headless session originally shared the
  user's D-Bus *session bus* with the physical session (both landed on the
  same `unix:path=/run/user/1000/bus`). Found live (2026-09-07, testing item
  7 against a real macOS client) to be an actual bug, not a theoretical one:
  ghostty (a single-instance GTK `GApplication`) launched via `$mod+Return`
  in the headless session asked the already-running *physical*-session
  instance (over that shared bus) to open a window, so it opened there
  instead — and typing into the (windowless) headless session went nowhere.
  This also explains the `mako`/fcitx5 "already running" D-Bus name
  conflicts visible in every session's log before this. Fixed by wrapping
  the spawned session in `dbus-run-session` (see
  `dragonvnc_session::SessionHandle::start`'s doc) — each headless session
  now gets its own private bus. Regression test:
  `ghostty_opens_in_the_headless_session_even_with_another_instance_running_elsewhere`.
  **Still shared** (a private session bus doesn't touch these): `systemd
  --user` itself, PipeWire, gnome-keyring. Apps in the remote session that
  go through `xdg-desktop-portal` may now activate a *fresh* portal instance
  on their own private bus (untested) rather than definitely reaching the
  physical one as before — behavior here is genuinely unverified either way,
  not a settled fact; treat portal-dependent apps (file choosers in
  sandboxed apps, screen sharing) as unreliable in the remote session until
  someone checks. Do **not** run `dbus-update-activation-environment --systemd
  WAYLAND_DISPLAY` from the headless session regardless; it would still
  repoint the physical session's *own* activated services at the remote
  display if it ever reached the shared bus.
- Audio from the remote session plays on the box's real speakers (shared
  PipeWire, unaffected by the D-Bus fix above). Remote audio is future work.
- No Xwayland (not installed on this box). Wayland-native apps only.
- `swayidle` is filtered out of the remote session; there is no idle lock,
  by design, because the session dies with the connection and the pinned
  client is the credential.
- Single user (`hafez`) hard-coded by "runs as the user". Multi-user needs
  the root broker (below).
- Per-connection teardown (killing/hung `dragonvnc-server`, `Drop`) is
  covered, but the server process itself dying ungracefully (`kill`/crash,
  not a clean shutdown) is not: found live while testing item 4 — SIGTERMing
  the server mid-connection leaves its active session's scope running
  indefinitely, since nothing tells systemd to stop it once the parent
  process is just gone (a systemd scope's lifetime isn't tied to its
  launcher's). Fixed: `run()` installs a SIGTERM/SIGINT handler that stops
  whatever session is active before the process exits (verified live). A
  `SIGKILL` (or a real crash) still bypasses this — `systemctl --user stop
  'dragonvnc-session-*'` is the manual recovery for that case.
- Item 5's pairing/pinning decision is made independently by each side from
  its own state (client: its `TrustStore` of servers; server: its
  `PairedClients` set) — normally consistent, but found live: revoking a
  client server-side while the *client* still has that server pinned
  produces a real, if unfriendly, error ("spake2 exchange failed:
  WrongLength") rather than a clean "please re-pair" message, because the
  client skips opening a pairing stream (it thinks it's still trusted) while
  the server waits for one. Recovery is manual: clear the client's local
  trust store (`paired_servers`) so it re-attempts pairing. A friendlier
  fix — the server signaling "not paired, please pair" instead of silently
  expecting a pairing stream — is follow-up work, not required for v1.

## Future work (out of this milestone, planned for)

- **DMA-BUF zero-copy capture → encode.** screencopy v3 emits
  `linux_dmabuf(format, width, height)`; allocate a `zwp_linux_dmabuf_v1`
  buffer (modifiers from `zwp_linux_dmabuf_feedback_v1`), `copy` into it, then
  import into libavcodec as `AV_PIX_FMT_DRM_PRIME` frames on a DRM hwdevice
  and `av_hwframe_map` them to the VAAPI hwframes context (or derive the VAAPI
  device from the DRM one) — no `sws_scale`, no `av_hwframe_transfer_data`.
  Risks specific to this GPU: VAAPI import on radeonsi accepts linear and a
  subset of GFX9/GFX10 tiling modifiers; the compositor's preferred DCC
  modifiers (seen in the probe log: `DCC,DCC_MAX_COMPRESSED_BLOCK=128B…`)
  are likely rejected, so the buffer must be allocated with an explicit
  modifier list intersecting what `vaExportSurfaceHandle`/import supports.
  Also `overlay_cursor` composition may force a copy in wlroots; measure.
  The `FrameSource` boundary is unchanged; `RawFrame` grows a variant
  carrying fds + modifier instead of `Bytes`.
- **`ext-image-copy-capture-v1`** when sway 1.11 / wlroots 0.19 reaches this
  box: proper damage + cursor-as-separate-session, replacing screencopy's
  one-shot request loop. Also a separate **cursor stream** to the client
  (render locally, remove cursor latency).
- **Root broker for multi-user remote login**: a small setuid-free system
  service that does PAM (greetd's `session/worker.rs` is the reference
  implementation, also Rust) and opens a logind session (`Type=wayland`,
  `Class=user`, no seat, `Remote=yes`), then execs an unprivileged
  `dragonvnc-server` for that user. The per-user server from this milestone
  becomes the thing the broker launches; nothing in it needs to change.
- **Persistent / multiple sessions** (reconnect resumes; session picker in
  the handshake). Requires the session to outlive the connection handler and
  a `ListSessions`/`AttachSession` control message.
- **Clipboard** via `zwlr_data_control_manager_v1` (advertised; the
  `ClipboardUpdate` message already exists). **Audio**: a per-session
  PipeWire null sink captured to Opus (the `AudioHeader` type exists).
- Native sway IPC client instead of shelling out to `swaymsg`.

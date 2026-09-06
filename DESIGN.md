# dragonvnc — design decisions

A remote-desktop server/client built with zero legacy constraints. Targets: Linux
(x86_64, aarch64) and macOS on Apple Silicon. No RFB compatibility, no support for
anything older than "reasonably modern hardware."

## Language & workspace

Rust, single Cargo workspace, shared code between server and client wherever the
logic doesn't need to be platform-specific.

## Transport: QUIC

`quinn`. Chosen over raw TCP because:

- TLS 1.3 is mandatory and built in — no bespoke handshake/downgrade surface.
- Independent streams mean input/clipboard/control traffic never blocks behind a
  big video frame (classic VNC/RDP head-of-line blocking problem, gone).
- Unreliable datagrams are a natural fit for video: a late frame is worthless,
  just drop it instead of stalling on retransmit.
- Connection migration: roam networks (Wi-Fi ↔ wired) without dropping session.
- 0-RTT resumption for fast reconnects.

## Security & identity

No CA, no PKI. Trust is established once via a **PAKE-based pairing ceremony**:
the server displays a short one-time code, the user enters it on the client, and
a password-authenticated key exchange (`spake2`) derives a session key that
authenticates the initial QUIC/TLS handshake against MITM without any prior
shared secret. The server's long-term public key is pinned client-side after
that (SSH `known_hosts` model) — every later connection is a plain pinned-key
TLS 1.3 handshake, no code needed again.

## Capture

- Linux: PipeWire screencast via `xdg-desktop-portal` (Wayland-native). DRM/KMS
  direct capture as a fallback for headless/greeter/no-portal contexts.
- macOS: `ScreenCaptureKit`.

Abstracted behind a `FrameSource` trait so the rest of the pipeline never sees
platform differences.

## Encoding

Hardware HEVC by default (VideoToolbox on Apple Silicon, VAAPI/NVENC on Linux),
tuned for interactive latency: CBR, no B-frames, short intra-refresh instead of
long GOPs. AV1 is a future opt-in for bandwidth-constrained links once HW AV1
encode latency catches up — not the v1 default.

**Reference server GPU: AMD RX 6700 XT (RDNA2, VCN 2.2).** This pins HEVC as
not just the recommended but the *only viable* real-time codec on this
hardware: VCN 2.x has a hardware HEVC encoder (8/10-bit, up to 4K) but no AV1
encode block at all — that shipped with RDNA3/VCN4 (RX 7000-series). Linux
encode path is therefore VAAPI against Mesa's `radeonsi`/RADV driver (the
open-source stack, not AMD's older closed AMF), targeting VCN 2.2's HEVC
profile. Confirmed on the actual box: `vainfo` shows `VAProfileHEVCMain`/
`VAProfileHEVCMain10` with `VAEntrypointEncSlice` via `libva2` 2.22.0 +
`mesa-va-drivers` 25.0.7 (`radeonsi`), accessible headlessly through
`/dev/dri/renderD128` (no X/Wayland session needed). Empirically discovered
hardware constraint: the encoder rejects anything smaller than **130x128**
(`avcodec_open2` fails EINVAL below that) and caps out at 8192x4352.

## Decode & render (client)

Hardware decode (VideoToolbox / VAAPI-NVDEC) feeding `wgpu` (Vulkan on Linux,
Metal on macOS) directly, avoiding a CPU round-trip per frame.

## Input

Own QUIC stream, isolated from video traffic. Injection via `libei`/uinput on
Linux (Wayland-native path first), `CGEvent` posting on macOS.

## Audio

Opus, its own stream, timestamp-synced against video.

## Discovery & scope

mDNS/DNS-SD for LAN discovery, direct QUIC connect. v1 targets LAN/VPN use
(same network, or already tunneled via WireGuard/Tailscale/etc.) — no relay or
NAT hole-punching yet. That's a deliberate scope cut, not an oversight; it can
be layered on later without changing the wire protocol.

## Client UI

Single cross-platform window: `winit` + `wgpu`, one binary for both platforms.

## Crate layout

- `dragonvnc-proto` — wire message types, versioning.
- `dragonvnc-net` — QUIC endpoint setup, PAKE pairing, key pinning store.
- `dragonvnc-capture` — `FrameSource` trait + platform backends.
- `dragonvnc-codec` — `Encoder`/`Decoder` traits + platform HW backends.
- `dragonvnc-input` — `InputInjector` trait + platform backends.
- `dragonvnc-server` — binary: capture → encode → net.
- `dragonvnc-client` — binary: net → decode → render (winit/wgpu), input capture.

## Status

Milestone 1: workspace scaffold with real trait boundaries for every
subsystem; **transport + pairing fully implemented and tested** end-to-end
over loopback (the one layer with no OS-specific dependency, provable in a
headless container).

Milestone 2: **real hardware HEVC encode, Linux/AMD only, fully working and
independently verified** — `dragonvnc-codec::vaapi::VaapiHevcEncoder` wraps
libavcodec's `hevc_vaapi` (see "Encoding" above for why libavcodec rather
than hand-rolled VAAPI parameter buffers). Verified two ways: a crate test
encodes on the real RX 6700 XT and decodes the result with a *separate*
software HEVC decoder (proving the bitstream is standards-valid, not just
self-consistent), and a live server→QUIC→client run was dumped to a raw
`.hevc` file and independently confirmed by `ffprobe`/`ffmpeg` (320x240,
118 frames cleanly decoded from a real network capture). The demo client
does not yet decode HEVC itself (see below).

Milestone 3: **real PipeWire screen capture wired up**, via the XDG Desktop
Portal ScreenCast interface (`ashpd`) — `dragonvnc-capture::pipewire`,
selected with `--source pipewire` on the server. `RawFrame` grew a
`PixelFormat` (Rgba/Bgra/Rgbx/Bgrx) and a `stride`, since real capture
buffers aren't always tightly-packed RGBA the way the test pattern is;
`VaapiHevcEncoder` now takes the source format as a parameter and reads
`stride` per frame instead of assuming both. Restricted to plain mapped
(non-DMA-BUF) buffers for now — no format modifiers are offered during
negotiation, which keeps the compositor from proposing DMA-BUF; importing
DMA-BUF for a zero-copy GPU-to-GPU path is a follow-up optimization.

First verification pass caught two real things, both fixed and then
re-verified:

- This reference machine's live `sway` session had **zero physical outputs
  attached** at first (genuinely headless) — nothing to capture. A
  throwaway probe still confirmed the wiring: it ran the full D-Bus portal
  handshake (`create_session`→`select_sources`→`start`) against the live
  `xdg-desktop-portal-wlr` and got back a real, specific, non-hanging error
  (`Invalid session`) exactly when the portal discovered there was nothing
  to capture. Once a monitor was attached, the same probe captured real
  1920x1080 frames from the live desktop.
- The portal's screen-picker is an interactive human-consent flow — right
  for "an app asks to share your screen," wrong for an always-on server
  with nobody sitting at the machine to click anything. First pass used
  `PersistMode::DoNot`, so it asked every single time. Fixed: `PipeWireSource`
  now persists the portal's `restore_token` to disk (`--source pipewire`'s
  token path) and passes it back with `PersistMode::ExplicitlyRevoked`.

Verified live end-to-end, twice, against the real desktop: first
connection ever (no saved token) needed one interactive click on the
picker, then captured, encoded (real VAAPI HEVC), and streamed real
1920x1080 desktop content over real QUIC to the demo client — independently
confirmed valid by `ffprobe`/`ffmpeg`. Second connection, same server, with
the token now on disk: pairing → Welcome → streaming completed in under
250ms with **zero interaction** — no picker, no delay — proving the
one-time-consent model actually works, not just compiles. The picker will
reappear only if the user revokes access from their desktop's privacy
settings, same as any other persistent screen-share grant (Discord, OBS,
etc.).

Milestone 4: **real input injection on Linux — pointer via the Wayland
`wlr-virtual-pointer-unstable-v1` protocol, keyboard via `uinput`** —
verified against the live desktop with the cursor landing on an exact
target pixel, arrived at after two real, fixed missteps along the way (not
hypothetical — each one caught with independent tooling against the live
compositor, the same rigor as the capture/encode milestones):

1. Original plan was the XDG `RemoteDesktop` portal (matching the
   ScreenCast approach, combined on one session per `ashpd`'s documented
   pattern, since `NotifyPointerMotionAbsolute` needs a linked screencast
   stream). Blocked: this box's reference portal backend,
   `xdg-desktop-portal-wlr` v0.7.1, only implements
   `Screenshot`/`ScreenCast` — confirmed via its installed `.portal` file,
   not assumed. Worth revisiting for portals that do implement
   `RemoteDesktop` (GNOME, KDE).
2. Pivoted to `uinput` for *both* pointer and keyboard: a virtual device
   via the `input-linux` crate. Needed a new udev rule
   (`/dev/uinput` is `root:root 0600` by default, no rule out of the box)
   plus `/etc/modules-load.d`, matching `ydotool`'s documented setup.
   First live test moved the cursor to a visibly wrong position — verified
   precisely via a `grim` screenshot pixel-diff against the live sway
   session (not just "looked off"), which located exactly where the
   cursor actually rendered. Root cause #1: missing `INPUT_PROP_DIRECT`.
   Fixing that surfaced root cause #2, from `libinput debug-events`
   directly: a uinput device combining absolute axes with conventional
   pointer buttons (BTN_LEFT/RIGHT/MIDDLE) gets classified by libinput as
   a **graphics tablet**, which requires proximity handshaking
   (`BTN_TOOL_PEN` in/out) our plain `ABS_X`/`ABS_Y` events don't provide
   — so the device was actually being silently ignored the whole time,
   not just miscalibrated.
3. Rather than keep fighting libinput's device-classification heuristics
   for something evdev doesn't cleanly model (an "absolute mouse" isn't a
   touchscreen, a tablet, or a relative pointer), switched pointer
   injection to `dragonvnc-input::wlr_pointer`: the Wayland
   `wlr-virtual-pointer-unstable-v1` protocol, purpose-built for exactly
   this and already used by `wayvnc` (an existing wlroots VNC server) for
   the same reason. `motion_absolute(x, y, x_extent, y_extent)` is
   unambiguous, compositor-interpreted pointer motion — no kernel evdev
   device classification involved at all. Keyboard stayed on `uinput`
   (a keys-only device has no such classification ambiguity).

Verified live, end to end, through the real server and client: connect,
capture (real desktop), encode (real VAAPI HEVC), inject a pointer move to
an exact target coordinate over the real QUIC transport, and confirm via a
`grim` screenshot that the real compositor cursor rendered within a few
pixels of that exact target (the residual few pixels being the cursor
icon's hotspot offset, not mapping error) — alongside a still-healthy,
independently `ffprobe`-verified video stream. Both the keyboard and
pointer virtual devices are created and torn down per connection.

Milestone 5: **real client-side HEVC decode via VideoToolbox**, on the
actual target hardware (M1 MacBook Air, macOS 15.2) — the piece Milestone
4's write-up said couldn't responsibly be attempted blind, now built and
verified for real instead of guessed at. Uses `objc2-video-toolbox` /
`objc2-core-media` / `objc2-core-video` directly (the same ecosystem `wgpu`
itself already depends on for macOS/Metal support) rather than the two
"safe wrapper" crates on crates.io, which turned out on inspection to be
thin unsafe re-exports of the same C functions with no real abstraction to
gain from depending on them instead.

Verified against real hardware output, not a hand-crafted fixture: a crate
test encodes a clip with the real VideoToolbox *hardware encoder*
(`ffmpeg -c:v hevc_videotoolbox`, confirmed present via Homebrew) and
decodes it with this module, asserting the right frame count (25/25),
correct dimensions, and actually non-zero pixel content per frame — not
just "no error was returned." One real, empirically-confirmed constraint
surfaced along the way and designed around rather than papered over:
requesting RGBA as the decode output color-conversion target fails every
single frame with `kCVReturnInvalidPixelFormat` (-6680); BGRA (the
platform's native 32-bit order — Metal's usual swapchain format is
`BGRA8Unorm`, not `RGBA8Unorm`) is what the hardware color-conversion path
actually supports. Rather than mislabel BGRA output as "RGBA" (a real
correctness bug a downstream consumer would eventually hit, not a harmless
simplification), the `Decoder` trait was extended to report the actual
pixel format and stride, mirroring how `RawFrame` already does this for
capture.

Still stubbed: ScreenCaptureKit (macOS capture — moot anyway, since macOS
is the client-only platform here), local input capture from the client's
own window (mouse/keyboard → `InputEvent`; not `CGEvent` posting, which
would be for a macOS *server*, out of scope here), and `wgpu` render. The
demo client now has a real decoder wired up for `VideoCodec::Hevc`
(macOS) alongside `PassthroughCodec` for the test-pattern path, plus the
`--dump-raw` escape hatch for inspecting the stream with independent
tooling when needed.

**Full cross-machine loop closed.** With the real Linux server running
(`--source pipewire --codec vaapi-hevc`) and this Mac on the same LAN, the
demo client connected live and decoded real network-transported HEVC for
15 continuous seconds with no errors: pairing succeeded, the server
reported its real 1920x1080 display, a real `VTDecompressionSession` was
created, and frames decoded steadily (fps tracked how much the actual
desktop was changing — 2 to 51 — consistent with PipeWire's damage-driven
capture, not a bug). That's every real piece exercised together for the
first time: live desktop → PipeWire capture → VAAPI HEVC hardware encode
(RX 6700 XT) → QUIC over a real LAN → VideoToolbox HEVC hardware decode
(M1), not each half verified only in isolation as before.

Getting to that point surfaced a real, if mundane, lesson: what looked
like a deep networking mystery (raw UDP silently dropped in one direction
only, survived diagnosing Mullvad's LAN-sharing setting on both ends, a
zombied LuLu system extension requiring a full reboot to clear) turned out
to be a stale IP address for the Linux box the whole time — `ssh
host.local` kept working throughout because mDNS resolved the *current*
address, while the hardcoded IP used for the dragonvnc connection attempts
was stale. The LuLu removal was a genuine, worthwhile fix in its own right
(a system extension left silently enforcing after its app was deleted,
only fully clearing on reboot), just not actually this session's blocker.

## Milestone 6: real render window + local input capture

Every prior milestone proved the pipeline end-to-end at the byte level
(decoded frames counted, dimensions logged, pixel diffs against a saved
PNG) but never actually put pixels on a screen or read a real mouse/
keyboard. This milestone replaces the demo client's headless decode loop
with an actual window — real HEVC frames rendered live, real local input
captured and forwarded — so the client is, for the first time, something
a person could sit in front of and use.

**Architecture.** winit requires the main thread on macOS, so the client
is restructured around that constraint rather than around the async
runtime: `main()` is no longer `async fn`. winit's `EventLoop` owns the
main thread; all networking (connect, pair, control handshake, decode
loop) moves to a dedicated background OS thread running its own
`tokio::runtime::Runtime`. The two sides are bridged in both directions:
network → render via `winit::event_loop::EventLoopProxy<RenderEvent>`
(delivers decoded `Frame`s and `NetworkError`s into winit's event loop,
the only channel winit provides for injecting events from outside),
render → network via a plain `tokio::sync::mpsc::unbounded_channel`
(`UnboundedSender::send` is sync, so winit's event handlers — which are
not async — can push `InputEvent`s onto it directly). The client crate
is split along this seam: `render.rs` (window, wgpu state, the
`ApplicationHandler` impl, local input capture), `network.rs` (connect/
pair/handshake, the decode loop, the input-forwarding task), `keymap.rs`
(winit physical key → evdev keycode), `main.rs` now just wires the two
together and owns the CLI args.

**Rendering.** wgpu (Metal backend on this hardware) draws a single
fullscreen triangle textured with the latest decoded frame — the
standard no-vertex-buffer blit trick (a WGSL vertex shader synthesizes
clip-space positions and UVs from `vertex_index` alone). The texture's
wgpu format is chosen per frame from `PixelFormat` (`Rgba8Unorm` or
`Bgra8Unorm`) rather than assumed, matching the codec crate's existing
refusal to mislabel BGRA as RGBA (Milestone 5) — VideoToolbox's real
hardware output is BGRA, the `PassthroughCodec` test-pattern path is
RGBA, and the shader must not care which. The texture is recreated only
when size or format actually changes; otherwise each `Frame` event is a
plain `write_texture` upload.

**Input capture.** winit's `WindowEvent`s map directly onto the wire
protocol's existing `InputEvent` variants: `CursorMoved` → `PointerMove`
(after scaling from window-physical-pixels to the remote display's own
resolution — independent per-axis scale factors, computed against
`remote_size` as learned from the first decoded frame's dimensions, so a
window of any size/aspect still addresses the full remote desktop),
`MouseInput` → `PointerButton`, `MouseWheel` → `Scroll` (line-delta
events scaled by a constant, pixel-delta events passed through as-is),
`KeyboardInput` → `Key` via `keymap::to_evdev`. This reuses the protocol
unchanged — no new wire types were needed, only a new producer of the
same `InputEvent`s the `--move-to` scripted-test path already sent in
Milestone 4.

**Verification.** Built up in stages against the real hardware rather
than trusted on the first attempt to compile:
- A bare clear-color window (no network yet) confirmed the
  winit-owns-main-thread + wgpu-Metal setup actually opens and holds a
  window on this machine before any networking was layered on top,
  screenshotted via `screencapture` to confirm the clear color landed.
- A synthetic quadrant-pattern texture (four solid-color quadrants
  uploaded via `write_texture`, no video stream involved) confirmed the
  textured fullscreen-triangle blit itself — format selection, UV
  orientation, upload path — independent of decode correctness, again
  confirmed by screenshot and pixel inspection rather than "it compiled."
- Only then was it pointed at the real server: with the Linux box
  running `--source pipewire --codec vaapi-hevc`, the client connected,
  paired, and rendered the **actual live Linux desktop** in the window —
  confirmed by `screencapture` on the Mac and comparing the captured
  window content against what was on the Linux screen at the time (this
  is the first milestone where a human could look at the client window
  and see the real remote desktop, not a log line asserting frames
  decoded).
- Input forwarding was verified the same way Milestone 4 verified
  server-side injection: two independent `--move-to` runs through the
  *new* render/network code path, at (200,200) and (1700,900), both
  landing the remote cursor within ~5px of target — matching Milestone
  4's established precision and confirming the new render-thread →
  mpsc → network-thread → control-stream path carries input correctly,
  not just that the old headless path did.

No real bugs were found in this milestone beyond the wgpu 22 API-naming
mismatches (`TexelCopyTextureInfo`/`TexelCopyBufferLayout` guessed from a
newer version; this workspace pins 22, whose actual names are
`ImageCopyTexture`/`ImageDataLayout`) and a `StackBlock`-shaped hazard
that didn't recur here but is worth naming for anyone touching this code
next to VideoToolbox's: closures crossing into Objective-C-block-based
APIs must be `Fn`, not `FnMut`, so any per-call mutable state has to go
through a `Cell`/`RefCell` captured by shared reference — already fixed
in Milestone 5's decoder, unchanged here, just relevant background for
why `render.rs`'s texture-swap logic is structured the way it is.

With this milestone, the full loop the project set out to build is
real end to end and usable by a person: live Linux desktop → PipeWire
capture → VAAPI HEVC hardware encode → QUIC over a real LAN →
VideoToolbox HEVC hardware decode → a real window on the Mac showing
the desktop → real local mouse/keyboard captured in that window →
forwarded back over the same connection → injected into the Linux
desktop via Wayland virtual-pointer + uinput. Every arrow in that chain
has now been verified against real hardware, not assumed from a
successful compile.

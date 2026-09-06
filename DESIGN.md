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

Still stubbed: ScreenCaptureKit (macOS
capture — moot anyway, since macOS is the client-only platform here), and
client-side decode + `wgpu` render. Real client-side decode is deliberately
not attempted yet: the target client platform is macOS/VideoToolbox, and
this environment has no Mac to build or run that against — writing
VideoToolbox FFI blind, with no way to verify it, isn't worth the risk of a
confident-looking but untested implementation. The demo client stays on
`PassthroughCodec` (works identically on any platform, already verified)
plus a `--dump-raw` escape hatch for inspecting whatever the server
actually sends with independent tooling, as done above.

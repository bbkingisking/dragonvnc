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
profile. Confirm at implementation time with `vainfo` on the actual box:
expect `VAProfileHEVCMain`/`VAProfileHEVCMain10` with `VAEntrypointEncSlice`.

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

Milestone 1 (this pass): workspace scaffold with real trait boundaries for
every subsystem; **transport + pairing is fully implemented and tested**
end-to-end over loopback, since it's the one layer with no OS-specific
dependency and is provable in a headless container. Capture/encode/input/
render are stubbed behind their traits with a synthetic test-pattern source so
the pipeline is wireable and inspectable, but real PipeWire/ScreenCaptureKit/
VAAPI/VideoToolbox/libei/CGEvent backends need to be built and exercised on
actual Linux desktop and macOS hardware — that's the next milestone.

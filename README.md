# dragonvnc

A remote-login server, not a screen-sharing one: each connection spawns its
own headless compositor session, sized to the client's window, torn down
when the connection ends. No CA; trust is pinning a paired device's
certificate on both sides.

## Stack

- Rust workspace, QUIC transport (`quinn`)
- Server: headless `sway`/wlroots session per connection, `wlr-screencopy`
  capture, Wayland virtual keyboard/pointer input, VAAPI HEVC hardware
  encode
- Client: VideoToolbox HEVC hardware decode, `wgpu`/`winit` render window

## Tested configuration (only)

- **Server**: Debian 13 (trixie), sway 1.10.1 / wlroots 0.18, AMD RX 6700 XT
  (RDNA2, Mesa `radeonsi` VAAPI)
- **Client**: macOS 15.2, Apple Silicon (arm64)

Nothing else has been run against this. See `PLAN-headless-session.md` and
`DESIGN.md` for how it works and what's known not to work yet.

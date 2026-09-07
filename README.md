# dragonvnc

A remote login server and client for `sway`. Each connection spawns its own headless compositor session, sized to the client's window, torn down when the connection ends.

## Stack

- QUIC transport (`quinn`)
- Server: headless `sway`/wlroots session per connection, `wlr-screencopy` capture, Wayland virtual keyboard/pointer input, VAAPI HEVC hardware encode
- Client: VideoToolbox HEVC hardware decode, `wgpu`/`winit` render window

## Tested configuration

This has only been tested on a single server-client configuration: 

- **Server**: Debian 13 (trixie), sway 1.10.1 / wlroots 0.18, AMD RX 6700 XT (RDNA2, Mesa `radeonsi` VAAPI)
- **Client**: macOS 15.2, Apple Silicon (arm64)

Will likely need heavy patching to make it work on anything else.

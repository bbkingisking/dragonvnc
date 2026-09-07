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

## Configuring the server

Every `dragonvnc-server run` flag also reads from a `DRAGONVNC_<NAME>`
environment variable — handy for a `systemd --user` unit's `Environment=`/
`EnvironmentFile=` instead of a long `ExecStart=` argument list (an explicit
flag still wins over the environment if both are set). None are required;
everything has a default. `dragonvnc-server clients` (the paired-device
store manager) only takes the one flag listed for it below.

| Flag | Env var | Default if unset |
|---|---|---|
| `--bind` | `DRAGONVNC_BIND` | `0.0.0.0:5900` |
| `--identity-path` | `DRAGONVNC_IDENTITY_PATH` | `$XDG_DATA_HOME/dragonvnc/server_identity.bin` (`~/.local/share/…` if unset) |
| `--paired-clients-path` (also on `clients`) | `DRAGONVNC_PAIRED_CLIENTS_PATH` | `$XDG_DATA_HOME/dragonvnc/paired_clients` |
| `--source` | `DRAGONVNC_SOURCE` | `session` |
| `--width` | `DRAGONVNC_WIDTH` | `1280` (only used with `--source test-pattern`) |
| `--height` | `DRAGONVNC_HEIGHT` | `720` (only used with `--source test-pattern`) |
| `--fps` | `DRAGONVNC_FPS` | `30` |
| `--codec` | `DRAGONVNC_CODEC` | `passthrough` |
| `--vaapi-device` † | `DRAGONVNC_VAAPI_DEVICE` | `/dev/dri/renderD128` (only used with `--codec vaapi-hevc`) |
| `--bitrate` | `DRAGONVNC_BITRATE` | `4000000` (bits/sec; only used with `--codec vaapi-hevc`) |
| `--mode` | `DRAGONVNC_MODE` | unset — follows the client's reported viewport instead of a fixed `WIDTHxHEIGHT[@SCALE]` |
| `--xkb-options` † | `DRAGONVNC_XKB_OPTIONS` | `lv3:ralt_switch,lv5:rctrl_switch` (only used with `--source session`) |
| `--sway-config` † | `DRAGONVNC_SWAY_CONFIG` | `~/.config/sway/config` (only used with `--source session`) |
| `--sway-session` † | `DRAGONVNC_SWAY_SESSION` | `~/.config/sway/sway-session` (only used with `--source session`) |
| `--exec-filter` † | `DRAGONVNC_EXEC_FILTER` | `` ^\s*exec\s+(swayidle\|lxpolkit)(?:\s\|$) `` (only used with `--source session`) |

† Linux-only flag — not present in a build for another target.

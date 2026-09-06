//! Wire protocol for dragonvnc.
//!
//! Two families of messages travel over the QUIC connection set up by
//! `dragonvnc-net`:
//!
//! - [`ControlMessage`] — reliable, ordered, sent on dedicated QUIC streams
//!   (one for input, one for clipboard/control, kept separate so a clipboard
//!   paste can never queue behind a burst of mouse-move events or vice versa).
//! - [`FrameHeader`] — a small fixed header prepended to each video/audio
//!   payload sent as an unreliable QUIC *datagram*. The payload itself
//!   (encoded HEVC/AV1/Opus bytes) is opaque to this crate.
//!
//! This crate only defines shapes and (de)serialization; it knows nothing
//! about QUIC, encoding, or crypto.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("failed to encode message: {0}")]
    Encode(#[from] bincode::Error),
}

pub type Result<T> = std::result::Result<T, ProtoError>;

/// Serialize a message for the wire. Length-prefixing is the transport's job
/// (QUIC streams are byte streams; `dragonvnc-net` frames these itself).
pub fn encode<T: Serialize>(msg: &T) -> Result<Vec<u8>> {
    Ok(bincode::serialize(msg)?)
}

pub fn decode<'a, T: Deserialize<'a>>(bytes: &'a [u8]) -> Result<T> {
    Ok(bincode::deserialize(bytes)?)
}

/// Reliable, ordered control-plane messages: session setup, input, clipboard,
/// resize negotiation, keepalive. Small and infrequent enough that ordering
/// and delivery guarantees matter more than latency.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ControlMessage {
    /// First message a client sends after the pairing/auth handshake
    /// (handled below the proto layer) has completed.
    Hello {
        protocol_version: u32,
        client_name: String,
    },
    /// Server's reply: what it can offer.
    Welcome {
        protocol_version: u32,
        server_name: String,
        displays: Vec<DisplayInfo>,
    },
    /// Ask the server to change the streamed resolution/refresh rate for a
    /// display (dynamic resize).
    RequestMode {
        display_id: u32,
        width: u32,
        height: u32,
        refresh_hz: u32,
    },
    Input(InputEvent),
    ClipboardUpdate {
        mime_type: String,
        data: Vec<u8>,
    },
    Ping(u64),
    Pong(u64),
    Bye,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum InputEvent {
    PointerMove { x: f32, y: f32 },
    PointerButton { button: PointerButton, pressed: bool },
    Scroll { dx: f32, dy: f32 },
    /// Platform-independent keycode (evdev-style), not a character — layout
    /// mapping happens on the server side against its own locale.
    Key { keycode: u32, pressed: bool },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
}

/// Codec identifying the bytes that follow a [`FrameHeader`] in a video
/// datagram. `TestPattern` is the raw-RGBA stub path used before real HW
/// encoders are wired up (see dragonvnc-codec).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum VideoCodec {
    TestPatternRgba,
    Hevc,
    Av1,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct FrameHeader {
    pub display_id: u32,
    pub frame_id: u64,
    pub timestamp_us: u64,
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub keyframe: bool,
    /// Length of the payload that follows this header in the same datagram.
    pub payload_len: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct AudioHeader {
    pub frame_id: u64,
    pub timestamp_us: u64,
    pub payload_len: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_message_roundtrips() {
        let msg = ControlMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            client_name: "test-client".into(),
        };
        let bytes = encode(&msg).unwrap();
        let back: ControlMessage = decode(&bytes).unwrap();
        match back {
            ControlMessage::Hello { protocol_version, client_name } => {
                assert_eq!(protocol_version, PROTOCOL_VERSION);
                assert_eq!(client_name, "test-client");
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn frame_header_roundtrips() {
        let hdr = FrameHeader {
            display_id: 0,
            frame_id: 42,
            timestamp_us: 123_456,
            codec: VideoCodec::Hevc,
            width: 1920,
            height: 1080,
            keyframe: true,
            payload_len: 1024,
        };
        let bytes = encode(&hdr).unwrap();
        let back: FrameHeader = decode(&bytes).unwrap();
        assert_eq!(back.frame_id, 42);
        assert_eq!(back.width, 1920);
        assert!(back.keyframe);
    }
}

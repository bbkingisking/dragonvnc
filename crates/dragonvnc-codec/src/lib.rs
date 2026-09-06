//! Video encode/decode abstraction. Real backends (VAAPI HEVC on Linux
//! against the reference RX 6700 XT/VCN 2.2 target, VideoToolbox HEVC on
//! Apple Silicon) are the next milestone — see DESIGN.md "Status". This
//! crate defines the trait boundary plus a passthrough "codec" (raw RGBA,
//! tagged `VideoCodec::TestPatternRgba`) so the capture → net → render
//! pipeline is wireable and testable before real hardware encoders land.

use dragonvnc_capture::RawFrame;
use dragonvnc_proto::VideoCodec;

pub struct EncodedFrame {
    pub codec: VideoCodec,
    pub keyframe: bool,
    pub payload: bytes::Bytes,
}

pub trait Encoder: Send {
    fn codec(&self) -> VideoCodec;

    /// Feeds one raw frame to the encoder. Real hardware encoders are
    /// pipelined — a given call may emit zero packets (input queued,
    /// nothing ready yet), exactly one (the common case once the pipeline
    /// is warm), or in principle more than one, so callers must not assume
    /// 1:1 and must keep sending frames to drain a non-empty pipeline.
    fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Vec<EncodedFrame>>;

    /// Signals end of stream and drains any packets still buffered in the
    /// encoder's pipeline. Call once, after the last `encode()`.
    fn flush(&mut self) -> anyhow::Result<Vec<EncodedFrame>> {
        Ok(Vec::new())
    }
}

pub trait Decoder: Send {
    /// Decodes one payload back into tightly-packed RGBA at the given
    /// dimensions (the caller gets width/height from the wire's
    /// `FrameHeader`, decoders don't need to track it themselves for the
    /// passthrough case; a real HEVC decoder would own a decode session
    /// keyed off the stream instead).
    fn decode(&mut self, payload: &[u8], width: u32, height: u32) -> anyhow::Result<bytes::Bytes>;
}

/// No-op "codec": ships raw RGBA as-is. Useful for proving out the rest of
/// the pipeline (transport, pacing, rendering) independent of any hardware
/// encoder, and as the fallback when no HW encoder is available at all.
pub struct PassthroughCodec;

impl Encoder for PassthroughCodec {
    fn codec(&self) -> VideoCodec {
        VideoCodec::TestPatternRgba
    }

    fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Vec<EncodedFrame>> {
        Ok(vec![EncodedFrame {
            codec: VideoCodec::TestPatternRgba,
            keyframe: true, // every frame is independently decodable
            payload: frame.rgba.clone(),
        }])
    }
}

#[cfg(target_os = "linux")]
pub mod vaapi;

impl Decoder for PassthroughCodec {
    fn decode(&mut self, payload: &[u8], width: u32, height: u32) -> anyhow::Result<bytes::Bytes> {
        let expected = (width * height * 4) as usize;
        anyhow::ensure!(
            payload.len() == expected,
            "passthrough payload size {} != expected {expected} for {width}x{height} RGBA",
            payload.len()
        );
        Ok(bytes::Bytes::copy_from_slice(payload))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dragonvnc_capture::FrameInfo;

    #[test]
    fn passthrough_roundtrips() {
        let rgba = bytes::Bytes::from(vec![1u8, 2, 3, 4, 5, 6, 7, 8]);
        let frame = RawFrame {
            info: FrameInfo { width: 1, height: 2, timestamp_us: 0 },
            rgba: rgba.clone(),
        };
        let mut codec = PassthroughCodec;
        let encoded = codec.encode(&frame).unwrap();
        assert_eq!(encoded.len(), 1);
        let decoded = codec.decode(&encoded[0].payload, 1, 2).unwrap();
        assert_eq!(decoded, rgba);
    }
}

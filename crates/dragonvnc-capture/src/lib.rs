//! Screen capture abstraction. Real backends (PipeWire/portal + DRM-KMS
//! fallback on Linux, ScreenCaptureKit on macOS) are the next milestone —
//! see DESIGN.md "Status". This crate defines the trait boundary everything
//! else builds against, plus a synthetic source for exercising the rest of
//! the pipeline without real display hardware.

use async_trait::async_trait;
use bytes::Bytes;

#[derive(Debug, Clone, Copy)]
pub struct FrameInfo {
    pub width: u32,
    pub height: u32,
    pub timestamp_us: u64,
}

/// One captured frame, tightly-packed 8-bit RGBA, row-major, no padding.
#[derive(Debug, Clone)]
pub struct RawFrame {
    pub info: FrameInfo,
    pub rgba: Bytes,
}

#[async_trait]
pub trait FrameSource: Send {
    /// Blocks (async) until the next frame is available. Returning `None`
    /// means the source is done (display disconnected, portal session
    /// ended, etc.) and capture should stop.
    async fn next_frame(&mut self) -> anyhow::Result<Option<RawFrame>>;
}

/// A moving-gradient test pattern, useful for developing/testing encode,
/// transport, and render without a real capture backend wired up yet.
pub struct TestPatternSource {
    width: u32,
    height: u32,
    frame_interval: std::time::Duration,
    frame_id: u64,
    started: std::time::Instant,
}

impl TestPatternSource {
    pub fn new(width: u32, height: u32, fps: u32) -> Self {
        Self {
            width,
            height,
            frame_interval: std::time::Duration::from_secs_f64(1.0 / fps as f64),
            frame_id: 0,
            started: std::time::Instant::now(),
        }
    }
}

#[async_trait]
impl FrameSource for TestPatternSource {
    async fn next_frame(&mut self) -> anyhow::Result<Option<RawFrame>> {
        let target = self.started + self.frame_interval * self.frame_id as u32;
        let now = std::time::Instant::now();
        if target > now {
            tokio::time::sleep(target - now).await;
        }

        let t = self.frame_id;
        let mut rgba = vec![0u8; (self.width * self.height * 4) as usize];
        for y in 0..self.height {
            for x in 0..self.width {
                let idx = ((y * self.width + x) * 4) as usize;
                rgba[idx] = ((x + t as u32) % 256) as u8;
                rgba[idx + 1] = ((y + t as u32) % 256) as u8;
                rgba[idx + 2] = 128;
                rgba[idx + 3] = 255;
            }
        }

        let frame = RawFrame {
            info: FrameInfo {
                width: self.width,
                height: self.height,
                timestamp_us: self.started.elapsed().as_micros() as u64,
            },
            rgba: Bytes::from(rgba),
        };
        self.frame_id += 1;
        Ok(Some(frame))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_pattern_produces_correctly_sized_frames() {
        let mut src = TestPatternSource::new(16, 8, 30);
        let frame = src.next_frame().await.unwrap().unwrap();
        assert_eq!(frame.rgba.len(), 16 * 8 * 4);
        assert_eq!(frame.info.width, 16);
        assert_eq!(frame.info.height, 8);
    }
}

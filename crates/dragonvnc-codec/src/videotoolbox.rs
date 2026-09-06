//! Hardware HEVC decode via VideoToolbox — the client-side counterpart to
//! `vaapi::VaapiHevcEncoder`. Uses `objc2-video-toolbox`/`objc2-core-media`/
//! `objc2-core-video` directly (comprehensive, actively-maintained
//! auto-generated bindings — the same ecosystem `wgpu` itself already
//! depends on for macOS/Metal support) rather than a thin "safe wrapper"
//! crate; the couple of candidates on crates.io turned out to just be
//! unsafe re-exports of the same C functions with no real abstraction, so
//! there was nothing to gain by depending on them instead.
//!
//! Verified against real hardware, not written blind: a probe program
//! decoded a real VideoToolbox-hardware-encoded HEVC stream (generated
//! locally via `ffmpeg -c:v hevc_videotoolbox`) on this Apple Silicon Mac,
//! confirmed frame count, dimensions, and non-all-zero pixel content.
//!
//! One real, empirically-confirmed constraint this module works around:
//! requesting RGBA as the decode output color-conversion target fails
//! every frame with `kCVReturnInvalidPixelFormat` (-6680) — Apple's
//! hardware color-conversion path here supports BGRA (the platform's
//! native 32-bit order — it's `MTLPixelFormatBGRA8Unorm` that's the usual
//! native Metal swapchain format, not RGBA) but not RGBA. So this decoder
//! produces BGRA, tagged as such via `DecodedFrame::format` rather than
//! mislabeled — see that type's doc.

use std::ffi::c_void;
use std::ptr::NonNull;

use block2::StackBlock;
use bytes::Bytes;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained};
use objc2_core_media::{
    kCMVideoCodecType_HEVC, CMBlockBuffer, CMFormatDescription, CMSampleBuffer,
    CMSampleTimingInfo, CMTime, CMVideoFormatDescriptionCreateFromHEVCParameterSets,
};
use objc2_core_video::{
    kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA, CVImageBuffer, CVPixelBuffer,
    CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow, CVPixelBufferGetHeight,
    CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_video_toolbox::{VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionSession};

use dragonvnc_capture::PixelFormat;

use crate::{DecodedFrame, Decoder};

fn check(op: &'static str, status: i32) -> anyhow::Result<()> {
    anyhow::ensure!(status == 0, "{op} failed with OSStatus {status}");
    Ok(())
}

struct Nal<'a> {
    kind: u8,
    data: &'a [u8], // NAL payload including its 2-byte header, no start code
}

/// Splits one Annex-B access unit (start-code-delimited NAL units) into
/// individual NALs. The wire protocol already delineates access-unit
/// boundaries for us (`FrameHeader`/`payload_len`) — this only needs to
/// split *within* one already-known-complete access unit.
fn parse_nals(au: &[u8]) -> Vec<Nal<'_>> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::new();
    for (w, &start) in starts.iter().enumerate() {
        let end = if let Some(&next) = starts.get(w + 1) {
            let mut e = next - 3;
            if e > 0 && au[e - 1] == 0 {
                e -= 1; // 4-byte start code's extra leading zero
            }
            e
        } else {
            au.len()
        };
        if end <= start {
            continue;
        }
        let data = &au[start..end];
        nals.push(Nal { kind: (data[0] >> 1) & 0x3f, data });
    }
    nals
}

pub struct VideoToolboxDecoder {
    session: Option<CFRetained<VTDecompressionSession>>,
    format_desc: Option<CFRetained<CMFormatDescription>>,
    width: u32,
    height: u32,
}

// Safety: same reasoning as `vaapi::VaapiHevcEncoder` — a VTDecompressionSession
// is a reference-counted CF object with no thread affinity; nothing here is
// accessed from more than one thread at a time (owned by a single decode task).
unsafe impl Send for VideoToolboxDecoder {}

impl VideoToolboxDecoder {
    pub fn new() -> Self {
        Self { session: None, format_desc: None, width: 0, height: 0 }
    }

    /// Builds the format description + decompression session from an
    /// access unit's VPS/SPS/PPS. Only ever called once per instance — a
    /// resolution change needs a new `VideoToolboxDecoder`, same policy as
    /// `vaapi::VaapiHevcEncoder` on the encode side.
    fn initialize(&mut self, nals: &[Nal<'_>], width: u32, height: u32) -> anyhow::Result<()> {
        let mut vps = None;
        let mut sps = None;
        let mut pps = None;
        for n in nals {
            match n.kind {
                32 => vps = Some(n.data),
                33 => sps = Some(n.data),
                34 => pps = Some(n.data),
                _ => {}
            }
        }
        let (vps, sps, pps) = (
            vps.ok_or_else(|| anyhow::anyhow!("first payload has no VPS NAL — can't initialize decoder without a keyframe"))?,
            sps.ok_or_else(|| anyhow::anyhow!("first payload has no SPS NAL — can't initialize decoder without a keyframe"))?,
            pps.ok_or_else(|| anyhow::anyhow!("first payload has no PPS NAL — can't initialize decoder without a keyframe"))?,
        );

        let format_desc: CFRetained<CMFormatDescription> = unsafe {
            let mut ptrs = [
                NonNull::new(vps.as_ptr() as *mut u8).unwrap(),
                NonNull::new(sps.as_ptr() as *mut u8).unwrap(),
                NonNull::new(pps.as_ptr() as *mut u8).unwrap(),
            ];
            let mut sizes = [vps.len(), sps.len(), pps.len()];
            let mut out: *const CMFormatDescription = std::ptr::null();
            let status = CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                None,
                3,
                NonNull::new(ptrs.as_mut_ptr()).unwrap(),
                NonNull::new(sizes.as_mut_ptr()).unwrap(),
                4, // NAL length prefix size we'll use for sample data below
                None,
                NonNull::new(&mut out as *mut *const CMFormatDescription).unwrap(),
            );
            check("CMVideoFormatDescriptionCreateFromHEVCParameterSets", status)?;
            CFRetained::from_raw(NonNull::new(out as *mut CMFormatDescription).unwrap())
        };

        let session: CFRetained<VTDecompressionSession> = unsafe {
            // BGRA is the hardware-supported color-conversion target on
            // this decoder — see module doc for why not RGBA.
            let pixel_format_number = CFNumber::new_i32(kCVPixelFormatType_32BGRA as i32);
            let dest_attrs =
                CFDictionary::from_slices(&[kCVPixelBufferPixelFormatTypeKey], &[&*pixel_format_number]);
            let mut out: *mut VTDecompressionSession = std::ptr::null_mut();
            let status = VTDecompressionSession::create(
                None,
                &format_desc,
                None,
                Some(dest_attrs.as_opaque()),
                std::ptr::null(),
                NonNull::new(&mut out as *mut *mut VTDecompressionSession).unwrap(),
            );
            check("VTDecompressionSessionCreate", status)?;
            CFRetained::from_raw(NonNull::new(out).unwrap())
        };

        self.format_desc = Some(format_desc);
        self.session = Some(session);
        self.width = width;
        self.height = height;
        tracing::info!(width, height, codec = ?kCMVideoCodecType_HEVC, "VideoToolbox decompression session created");
        Ok(())
    }
}

impl Default for VideoToolboxDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for VideoToolboxDecoder {
    fn decode(&mut self, payload: &[u8], width: u32, height: u32) -> anyhow::Result<DecodedFrame> {
        let nals = parse_nals(payload);

        if self.session.is_none() {
            self.initialize(&nals, width, height)?;
        }
        anyhow::ensure!(
            width == self.width && height == self.height,
            "frame is {width}x{height}, decoder initialized for {}x{} (a resolution change needs a new decoder instance)",
            self.width,
            self.height
        );
        let session = self.session.as_ref().unwrap();
        let format_desc = self.format_desc.as_ref().unwrap();

        // Only VCL NAL units go in the sample buffer; parameter sets are
        // already captured in the format description built above.
        let mut sample_data = Vec::new();
        for n in &nals {
            if n.kind <= 31 {
                sample_data.extend_from_slice(&(n.data.len() as u32).to_be_bytes());
                sample_data.extend_from_slice(n.data);
            }
        }
        anyhow::ensure!(!sample_data.is_empty(), "payload had no VCL NAL units to decode");
        let data_len = sample_data.len();

        unsafe {
            let block_buffer: CFRetained<CMBlockBuffer> = {
                let mut out: *mut CMBlockBuffer = std::ptr::null_mut();
                let status = CMBlockBuffer::create_with_memory_block(
                    None,
                    sample_data.as_mut_ptr() as *mut c_void,
                    data_len,
                    objc2_core_foundation::kCFAllocatorNull,
                    std::ptr::null(),
                    0,
                    data_len,
                    0,
                    NonNull::new(&mut out as *mut *mut CMBlockBuffer).unwrap(),
                );
                check("CMBlockBufferCreateWithMemoryBlock", status)?;
                CFRetained::from_raw(NonNull::new(out).unwrap())
            };

            let sample_buffer: CFRetained<CMSampleBuffer> = {
                let mut out: *mut CMSampleBuffer = std::ptr::null_mut();
                // A real, monotonic-enough timestamp; VideoToolbox doesn't
                // need wall-clock accuracy for a plain synchronous decode.
                let timing = CMSampleTimingInfo {
                    duration: CMTime::new(1, 1000),
                    presentationTimeStamp: CMTime::new(0, 1000),
                    decodeTimeStamp: CMTime::new(0, 1000),
                };
                let status = CMSampleBuffer::create_ready(
                    None,
                    Some(&block_buffer),
                    Some(format_desc),
                    1,
                    1,
                    &timing as *const CMSampleTimingInfo,
                    1,
                    &data_len as *const usize,
                    NonNull::new(&mut out as *mut *mut CMSampleBuffer).unwrap(),
                );
                check("CMSampleBufferCreateReady", status)?;
                CFRetained::from_raw(NonNull::new(out).unwrap())
            };

            let result: std::cell::Cell<Option<anyhow::Result<(Bytes, u32)>>> = std::cell::Cell::new(None);
            {
                let result_ref = &result;
                let block = StackBlock::new(
                    move |status: i32,
                          _flags: VTDecodeInfoFlags,
                          image: *mut CVImageBuffer,
                          _pts: CMTime,
                          _dur: CMTime| {
                        if status != 0 || image.is_null() {
                            result_ref.set(Some(Err(anyhow::anyhow!(
                                "VideoToolbox decode callback: status={status}, image_null={}",
                                image.is_null()
                            ))));
                            return;
                        }
                        let pixel_buffer = &*(image as *const CVPixelBuffer);
                        CVPixelBufferLockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
                        let stride = CVPixelBufferGetBytesPerRow(pixel_buffer);
                        let h = CVPixelBufferGetHeight(pixel_buffer);
                        let base = CVPixelBufferGetBaseAddress(pixel_buffer);
                        let out = if base.is_null() {
                            Err(anyhow::anyhow!("decoded pixel buffer has a null base address"))
                        } else {
                            let bytes = std::slice::from_raw_parts(base as *const u8, stride * h);
                            Ok((Bytes::copy_from_slice(bytes), stride as u32))
                        };
                        CVPixelBufferUnlockBaseAddress(pixel_buffer, CVPixelBufferLockFlags::ReadOnly);
                        result_ref.set(Some(out));
                    },
                );
                let mut info_flags = VTDecodeInfoFlags::empty();
                let status = session.decode_frame_with_options_and_output_handler(
                    &sample_buffer,
                    VTDecodeFrameFlags::empty(), // synchronous: callback runs before this returns
                    None,
                    &mut info_flags as *mut _,
                    &*block as *const _ as *mut _,
                );
                check("VTDecompressionSessionDecodeFrame", status)?;
            }

            let (pixels, stride) = result
                .into_inner()
                .ok_or_else(|| anyhow::anyhow!("decode completed with no callback invocation"))??;
            Ok(DecodedFrame { pixels, format: PixelFormat::Bgra, stride })
        }
    }
}

impl Drop for VideoToolboxDecoder {
    fn drop(&mut self) {
        if let Some(session) = &self.session {
            unsafe { session.invalidate() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Encodes a short clip with the real VideoToolbox hardware HEVC
    /// encoder (`ffmpeg -c:v hevc_videotoolbox`) and decodes it with this
    /// module — proving the decoder against real hardware output, not a
    /// hand-crafted fixture. Uses `ffprobe` to get exact access-unit byte
    /// ranges (this test's demuxing need, not a production concern: the
    /// real wire protocol already delineates frames via `FrameHeader`).
    #[test]
    fn decodes_a_real_hardware_encoded_stream() {
        let dir = std::env::temp_dir().join(format!("dragonvnc-vt-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let hevc_path = dir.join("clip.hevc");

        let status = Command::new("ffmpeg")
            .args([
                "-hide_banner", "-y", "-loglevel", "error",
                "-f", "lavfi", "-i", "testsrc=size=320x240:rate=25:duration=1",
                "-c:v", "hevc_videotoolbox", "-b:v", "2M", "-tag:v", "hvc1",
                "-f", "hevc",
            ])
            .arg(&hevc_path)
            .status();
        let Ok(status) = status else {
            eprintln!("ffmpeg not available, skipping (this test needs it to generate a real fixture)");
            return;
        };
        assert!(status.success(), "ffmpeg encode failed");

        let probe_out = Command::new("ffprobe")
            .args([
                "-v", "error", "-f", "hevc", "-show_packets", "-select_streams", "v:0", "-of", "json",
            ])
            .arg(&hevc_path)
            .output()
            .expect("ffprobe not available");
        assert!(probe_out.status.success());

        #[derive(serde::Deserialize)]
        struct Packet {
            pos: String,
            size: String,
        }
        #[derive(serde::Deserialize)]
        struct Packets {
            packets: Vec<Packet>,
        }
        let parsed: Packets = serde_json::from_slice(&probe_out.stdout).unwrap();
        let access_units: Vec<(usize, usize)> = parsed
            .packets
            .iter()
            .map(|p| (p.pos.parse().unwrap(), p.size.parse().unwrap()))
            .collect();
        assert_eq!(access_units.len(), 25);

        let raw = std::fs::read(&hevc_path).unwrap();
        let mut decoder = VideoToolboxDecoder::new();
        let mut decoded_count = 0;
        for (pos, size) in access_units {
            let payload = &raw[pos..pos + size];
            let frame = decoder.decode(payload, 320, 240).expect("decode failed");
            assert_eq!(frame.format, PixelFormat::Bgra);
            assert!(frame.stride >= 320 * 4);
            assert_eq!(frame.pixels.len(), frame.stride as usize * 240);
            assert!(frame.pixels.iter().any(|&b| b != 0), "decoded frame was all zero");
            decoded_count += 1;
        }
        assert_eq!(decoded_count, 25);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

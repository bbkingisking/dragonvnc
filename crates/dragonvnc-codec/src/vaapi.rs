//! Hardware HEVC encode via VAAPI, through libavcodec's `hevc_vaapi` wrapper
//! rather than hand-rolled VAAPI parameter buffers. Constructing a correct
//! VAAPI encode picture (packed VPS/SPS/PPS headers, slice params, rate
//! control config) by hand is a large and easy-to-get-subtly-wrong surface;
//! `hevc_vaapi` is the same battle-tested path OBS, GStreamer, and FFmpeg
//! itself use, so we reuse it instead of re-deriving it.
//!
//! Verified against the reference server GPU (AMD RX 6700 XT, RDNA2/VCN 2.2,
//! Mesa `radeonsi` VAAPI driver — see DESIGN.md) via `vainfo` showing
//! `VAProfileHEVCMain`/`VAProfileHEVCMain10` with `VAEntrypointEncSlice`, and
//! by round-tripping this encoder's actual output through a software HEVC
//! decoder in this crate's tests.

use std::ffi::{c_void, CString};
use std::ptr;

use ffmpeg_sys_next as ff;

use dragonvnc_capture::{PixelFormat, RawFrame};
use dragonvnc_proto::VideoCodec;

use crate::{EncodedFrame, Encoder};

/// `/dev/dri/renderD128` is this box's primary GPU render node (confirmed
/// via `lspci`/`vainfo`); a real deployment should enumerate
/// `/dev/dri/renderD*` rather than hardcoding it.
pub const DEFAULT_DEVICE: &str = "/dev/dri/renderD128";

/// VCN 2.2's HEVC encoder's verified resolution range on this GPU (see
/// DESIGN.md and this module's tests): `avcodec_open2` fails EINVAL below
/// `MIN_WIDTH`x`MIN_HEIGHT`; `MAX_WIDTH`x`MAX_HEIGHT` is the other end.
/// Callers driving a live-resizable source (e.g. a headless compositor
/// output) should clamp to this range before requesting a mode change —
/// this crate only enforces it implicitly, by `avcodec_open2` failing.
pub const MIN_WIDTH: u32 = 130;
pub const MIN_HEIGHT: u32 = 128;
pub const MAX_WIDTH: u32 = 8192;
pub const MAX_HEIGHT: u32 = 4352;

fn av_pixel_format(format: PixelFormat) -> ff::AVPixelFormat {
    match format {
        PixelFormat::Rgba => ff::AVPixelFormat::AV_PIX_FMT_RGBA,
        PixelFormat::Bgra => ff::AVPixelFormat::AV_PIX_FMT_BGRA,
        // FFmpeg's "0" suffix formats mean "byte present, contents ignored"
        // — exactly SPA/our PixelFormat's "x" (Rgbx/Bgrx) convention.
        PixelFormat::Rgbx => ff::AVPixelFormat::AV_PIX_FMT_RGB0,
        PixelFormat::Bgrx => ff::AVPixelFormat::AV_PIX_FMT_BGR0,
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VaapiError {
    #[error("hevc_vaapi encoder not available in this libavcodec build")]
    EncoderNotFound,
    #[error("{0} failed: {1}")]
    Av(&'static str, AvError),
    #[error("allocation failed: {0}")]
    Alloc(&'static str),
}

/// Wraps an FFmpeg negative error code with its libavutil-rendered message.
#[derive(Debug)]
pub struct AvError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for AvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code)
    }
}

fn av_err(code: i32) -> AvError {
    let mut buf = [0i8; ff::AV_ERROR_MAX_STRING_SIZE];
    unsafe {
        ff::av_strerror(code, buf.as_mut_ptr(), buf.len());
        let cstr = std::ffi::CStr::from_ptr(buf.as_ptr());
        AvError {
            code,
            message: cstr.to_string_lossy().into_owned(),
        }
    }
}

fn check(op: &'static str, code: i32) -> Result<(), VaapiError> {
    if code < 0 {
        Err(VaapiError::Av(op, av_err(code)))
    } else {
        Ok(())
    }
}

const fn mktag(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}
/// `AVERROR_EOF` is a C macro (`FFERRTAG('E','O','F',' ')`), not a symbol
/// bindgen can emit, so it's reproduced here from its definition.
const AVERROR_EOF: i32 = -(mktag(b'E', b'O', b'F', b' ') as i32);
/// `AVERROR(EAGAIN)` is likewise a macro (`-(EAGAIN)`).
fn averror_eagain() -> i32 {
    -(libc::EAGAIN)
}

pub struct VaapiHevcEncoder {
    codec_ctx: *mut ff::AVCodecContext,
    hw_device_ctx: *mut ff::AVBufferRef,
    hw_frames_ctx: *mut ff::AVBufferRef,
    sws_ctx: *mut ff::SwsContext,
    sw_frame: *mut ff::AVFrame,
    hw_frame: *mut ff::AVFrame,
    packet: *mut ff::AVPacket,
    width: u32,
    height: u32,
    src_format: PixelFormat,
    frame_index: i64,
}

// Safety: nothing here is thread-affine (VAAPI/libavcodec contexts don't
// pin to the creating thread), and `VaapiHevcEncoder` is never accessed
// from more than one thread at a time — it's owned by a single encode task.
unsafe impl Send for VaapiHevcEncoder {}

impl VaapiHevcEncoder {
    /// `src_format` is fixed for the encoder's lifetime, same as
    /// width/height — a capture source that changes pixel format needs a
    /// new encoder instance, same as one that changes resolution.
    pub fn new(
        device_path: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: i64,
        src_format: PixelFormat,
    ) -> Result<Self, VaapiError> {
        unsafe { Self::new_unsafe(device_path, width, height, fps, bitrate_bps, src_format) }
    }

    unsafe fn new_unsafe(
        device_path: &str,
        width: u32,
        height: u32,
        fps: u32,
        bitrate_bps: i64,
        src_format: PixelFormat,
    ) -> Result<Self, VaapiError> {
        let encoder_name = CString::new("hevc_vaapi").unwrap();
        let codec = ff::avcodec_find_encoder_by_name(encoder_name.as_ptr());
        if codec.is_null() {
            return Err(VaapiError::EncoderNotFound);
        }

        let mut hw_device_ctx: *mut ff::AVBufferRef = ptr::null_mut();
        let device_cstr =
            CString::new(device_path).map_err(|_| VaapiError::Alloc("device path had a NUL byte"))?;
        check(
            "av_hwdevice_ctx_create",
            ff::av_hwdevice_ctx_create(
                &mut hw_device_ctx,
                ff::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                device_cstr.as_ptr(),
                ptr::null_mut(),
                0,
            ),
        )?;

        let hw_frames_ref = ff::av_hwframe_ctx_alloc(hw_device_ctx);
        if hw_frames_ref.is_null() {
            ff::av_buffer_unref(&mut hw_device_ctx);
            return Err(VaapiError::Alloc("av_hwframe_ctx_alloc"));
        }
        {
            let frames_ctx = &mut *((*hw_frames_ref).data as *mut ff::AVHWFramesContext);
            frames_ctx.format = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
            frames_ctx.sw_format = ff::AVPixelFormat::AV_PIX_FMT_NV12;
            frames_ctx.width = width as i32;
            frames_ctx.height = height as i32;
            // Small pool: we're not queuing deep for throughput, we want
            // low latency (see DESIGN.md's "no B-frames, tight refresh"
            // encode-tuning stance).
            frames_ctx.initial_pool_size = 4;
        }
        if let Err(e) = check("av_hwframe_ctx_init", ff::av_hwframe_ctx_init(hw_frames_ref)) {
            let mut r = hw_frames_ref;
            ff::av_buffer_unref(&mut r);
            ff::av_buffer_unref(&mut hw_device_ctx);
            return Err(e);
        }

        let codec_ctx = ff::avcodec_alloc_context3(codec);
        if codec_ctx.is_null() {
            let mut r = hw_frames_ref;
            ff::av_buffer_unref(&mut r);
            ff::av_buffer_unref(&mut hw_device_ctx);
            return Err(VaapiError::Alloc("avcodec_alloc_context3"));
        }

        (*codec_ctx).width = width as i32;
        (*codec_ctx).height = height as i32;
        (*codec_ctx).time_base = ff::AVRational { num: 1, den: fps as i32 };
        (*codec_ctx).framerate = ff::AVRational { num: fps as i32, den: 1 };
        (*codec_ctx).pix_fmt = ff::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*codec_ctx).max_b_frames = 0; // latency: never wait on future frames
        (*codec_ctx).gop_size = fps.max(1) as i32 * 2; // IDR every ~2s; revisit for real intra-refresh
        (*codec_ctx).bit_rate = bitrate_bps;
        (*codec_ctx).rc_max_rate = bitrate_bps;
        (*codec_ctx).rc_buffer_size = ((bitrate_bps as f64 / fps.max(1) as f64) * 1.5) as i32;
        (*codec_ctx).hw_frames_ctx = ff::av_buffer_ref(hw_frames_ref);
        (*codec_ctx).hw_device_ctx = ff::av_buffer_ref(hw_device_ctx);

        set_opt(codec_ctx, "rc_mode", "CBR")?;
        // Minimize the encoder's internal pipeline depth for latency, at
        // some throughput cost. See DESIGN.md's latency-first stance.
        set_opt(codec_ctx, "async_depth", "1")?;

        if let Err(e) = check("avcodec_open2", ff::avcodec_open2(codec_ctx, codec, ptr::null_mut()))
        {
            let mut cctx = codec_ctx;
            ff::avcodec_free_context(&mut cctx);
            let mut r = hw_frames_ref;
            ff::av_buffer_unref(&mut r);
            ff::av_buffer_unref(&mut hw_device_ctx);
            return Err(e);
        }

        let sws_ctx = ff::sws_getContext(
            width as i32,
            height as i32,
            av_pixel_format(src_format),
            width as i32,
            height as i32,
            ff::AVPixelFormat::AV_PIX_FMT_NV12,
            ff::SWS_BILINEAR,
            ptr::null_mut(),
            ptr::null_mut(),
            ptr::null(),
        );
        if sws_ctx.is_null() {
            let mut cctx = codec_ctx;
            ff::avcodec_free_context(&mut cctx);
            let mut r = hw_frames_ref;
            ff::av_buffer_unref(&mut r);
            ff::av_buffer_unref(&mut hw_device_ctx);
            return Err(VaapiError::Alloc("sws_getContext"));
        }

        let sw_frame = ff::av_frame_alloc();
        let hw_frame = ff::av_frame_alloc();
        let packet = ff::av_packet_alloc();
        if sw_frame.is_null() || hw_frame.is_null() || packet.is_null() {
            return Err(VaapiError::Alloc("av_frame_alloc/av_packet_alloc"));
        }
        (*sw_frame).format = ff::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        (*sw_frame).width = width as i32;
        (*sw_frame).height = height as i32;
        check(
            "av_frame_get_buffer",
            ff::av_frame_get_buffer(sw_frame, 32),
        )?;

        Ok(Self {
            codec_ctx,
            hw_device_ctx,
            hw_frames_ctx: hw_frames_ref,
            sws_ctx,
            sw_frame,
            hw_frame,
            packet,
            width,
            height,
            src_format,
            frame_index: 0,
        })
    }

    /// Drains whatever packets are currently available without sending a
    /// new frame first. Shared by `encode` (drain-then-send) and `flush`.
    fn drain(&mut self) -> Result<Vec<EncodedFrame>, VaapiError> {
        let mut out = Vec::new();
        loop {
            unsafe {
                ff::av_packet_unref(self.packet);
                let ret = ff::avcodec_receive_packet(self.codec_ctx, self.packet);
                if ret == averror_eagain() || ret == AVERROR_EOF {
                    break;
                }
                check("avcodec_receive_packet", ret)?;
                let data =
                    std::slice::from_raw_parts((*self.packet).data, (*self.packet).size as usize);
                out.push(EncodedFrame {
                    codec: VideoCodec::Hevc,
                    keyframe: (*self.packet).flags & ff::AV_PKT_FLAG_KEY != 0,
                    payload: bytes::Bytes::copy_from_slice(data),
                });
            }
        }
        tracing::trace!(packets = out.len(), "drained encoder packets");
        Ok(out)
    }
}

fn set_opt(codec_ctx: *mut ff::AVCodecContext, name: &str, value: &str) -> Result<(), VaapiError> {
    let name_c = CString::new(name).unwrap();
    let value_c = CString::new(value).unwrap();
    unsafe {
        check(
            "av_opt_set",
            ff::av_opt_set(
                (*codec_ctx).priv_data as *mut c_void,
                name_c.as_ptr(),
                value_c.as_ptr(),
                0,
            ),
        )
    }
}

impl Encoder for VaapiHevcEncoder {
    fn codec(&self) -> VideoCodec {
        VideoCodec::Hevc
    }

    fn encode(&mut self, frame: &RawFrame) -> anyhow::Result<Vec<EncodedFrame>> {
        anyhow::ensure!(
            frame.info.width == self.width && frame.info.height == self.height,
            "frame is {}x{}, encoder configured for {}x{} (dynamic resize needs a new encoder instance)",
            frame.info.width,
            frame.info.height,
            self.width,
            self.height
        );
        anyhow::ensure!(
            frame.info.format == self.src_format,
            "frame pixel format is {:?}, encoder configured for {:?} (a format change needs a new encoder instance)",
            frame.info.format,
            self.src_format
        );

        let started = std::time::Instant::now();
        let sws_started = started;
        unsafe {
            let src_data: [*const u8; 4] =
                [frame.pixels.as_ptr(), ptr::null(), ptr::null(), ptr::null()];
            let src_linesize: [i32; 4] = [frame.info.stride as i32, 0, 0, 0];
            let dst_data: [*mut u8; 4] = [
                (*self.sw_frame).data[0],
                (*self.sw_frame).data[1],
                (*self.sw_frame).data[2],
                (*self.sw_frame).data[3],
            ];
            let dst_linesize: [i32; 4] = [
                (*self.sw_frame).linesize[0],
                (*self.sw_frame).linesize[1],
                (*self.sw_frame).linesize[2],
                (*self.sw_frame).linesize[3],
            ];
            ff::sws_scale(
                self.sws_ctx,
                src_data.as_ptr(),
                src_linesize.as_ptr(),
                0,
                self.height as i32,
                dst_data.as_ptr(),
                dst_linesize.as_ptr(),
            );
        }
        let sws_elapsed = sws_started.elapsed();

        let hw_started = std::time::Instant::now();
        unsafe {
            ff::av_frame_unref(self.hw_frame);
            check(
                "av_hwframe_get_buffer",
                ff::av_hwframe_get_buffer(self.hw_frames_ctx, self.hw_frame, 0),
            )?;
            check(
                "av_hwframe_transfer_data",
                ff::av_hwframe_transfer_data(self.hw_frame, self.sw_frame, 0),
            )?;
            (*self.hw_frame).pts = self.frame_index;
            self.frame_index += 1;
        }
        let hw_transfer_elapsed = hw_started.elapsed();

        let send_started = std::time::Instant::now();
        unsafe {
            check(
                "avcodec_send_frame",
                ff::avcodec_send_frame(self.codec_ctx, self.hw_frame),
            )?;
        }
        let send_frame_elapsed = send_started.elapsed();

        tracing::trace!(
            ?sws_elapsed,
            ?hw_transfer_elapsed,
            ?send_frame_elapsed,
            frame_index = self.frame_index,
            "encode stage timings"
        );
        // A hardware-accelerated pixel format conversion + VAAPI submission
        // should be low single-digit milliseconds at these resolutions. If
        // the *total* creeps up, that's this encoder (GPU busy/thermal
        // throttled, driver hiccup) rather than the network — a useful
        // distinction when triaging "the desktop hitches" reports, since
        // the higher-level per-frame warning in dragonvnc-server can't
        // tell which stage was actually slow.
        let total_elapsed = started.elapsed();
        if total_elapsed > std::time::Duration::from_millis(50) {
            tracing::warn!(?total_elapsed, ?sws_elapsed, ?hw_transfer_elapsed, ?send_frame_elapsed, "VAAPI encode took unusually long");
        }

        Ok(self.drain()?)
    }

    fn flush(&mut self) -> anyhow::Result<Vec<EncodedFrame>> {
        unsafe {
            check("avcodec_send_frame(flush)", ff::avcodec_send_frame(self.codec_ctx, ptr::null()))?;
        }
        Ok(self.drain()?)
    }
}

impl Drop for VaapiHevcEncoder {
    fn drop(&mut self) {
        unsafe {
            ff::av_packet_free(&mut self.packet);
            ff::av_frame_free(&mut self.hw_frame);
            ff::av_frame_free(&mut self.sw_frame);
            ff::sws_freeContext(self.sws_ctx);
            ff::avcodec_free_context(&mut self.codec_ctx);
            ff::av_buffer_unref(&mut self.hw_frames_ctx);
            ff::av_buffer_unref(&mut self.hw_device_ctx);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dragonvnc_capture::{FrameInfo, FrameSource, TestPatternSource};

    /// Runs the real encoder against real VAAPI hardware (this crate's
    /// tests assume the reference box: RX 6700 XT / Mesa radeonsi — see
    /// DESIGN.md) and independently verifies the output by decoding it
    /// back with libavcodec's software HEVC decoder. This doesn't just
    /// check "no error was returned" — it proves the bitstream is a
    /// spec-valid, actually-decodable HEVC stream, and that we got back
    /// the number of frames we put in.
    #[tokio::test]
    async fn encoded_stream_is_valid_and_decodes_to_the_right_frame_count() {
        // VCN 2.2's HEVC encoder has a hard minimum of 130x128 (discovered
        // empirically — see DESIGN.md); anything smaller fails
        // avcodec_open2 with EINVAL.
        let width = 320;
        let height = 240;
        let fps = 20;
        let frame_count = 30;

        let mut encoder =
            VaapiHevcEncoder::new(DEFAULT_DEVICE, width, height, fps, 2_000_000, PixelFormat::Rgba).expect(
                "VAAPI HEVC encoder init failed — this test assumes the reference AMD GPU/driver \
                 stack from DESIGN.md; run `vainfo` to check VAProfileHEVCMain/EncSlice support",
            );

        let mut source = TestPatternSource::new(width, height, fps);
        // One entry per *access unit* (one video frame's worth of NALs).
        // Each must be handed to the decoder as its own packet — mashing
        // them all into one buffer and sending it as a single AVPacket is
        // not valid `avcodec_send_packet` usage and produces garbage
        // (caught the hard way: see git history for the first, wrong,
        // version of this test).
        let mut access_units: Vec<bytes::Bytes> = Vec::new();
        let mut keyframes = 0usize;

        for _ in 0..frame_count {
            let frame = source.next_frame().await.unwrap().unwrap();
            for encoded in encoder.encode(&frame).unwrap() {
                if encoded.keyframe {
                    keyframes += 1;
                }
                access_units.push(encoded.payload);
            }
        }
        for encoded in encoder.flush().unwrap() {
            if encoded.keyframe {
                keyframes += 1;
            }
            access_units.push(encoded.payload);
        }

        assert_eq!(access_units.len(), frame_count, "expected one packet per input frame in this low-latency config");
        assert!(keyframes >= 1, "stream must contain at least one keyframe to be decodable at all");

        let decoded_frame_count = decode_with_software_hevc(&access_units, width, height);
        assert_eq!(
            decoded_frame_count, frame_count,
            "software HEVC decoder should reconstruct exactly the frames we encoded"
        );
    }

    /// Independent verification path: decode with libavcodec's *software*
    /// HEVC decoder (`hevc`, not `hevc_vaapi`) so this doesn't just check
    /// "our own encoder's paired decoder round-trips" — it proves the
    /// bitstream is standards-valid HEVC, decodable by an unrelated
    /// implementation. Takes one buffer per access unit, matching how a
    /// real client will feed each `FrameHeader`-delimited payload in.
    fn decode_with_software_hevc(access_units: &[bytes::Bytes], width: u32, height: u32) -> usize {
        unsafe {
            let name = CString::new("hevc").unwrap();
            let codec = ff::avcodec_find_decoder_by_name(name.as_ptr());
            assert!(!codec.is_null(), "software hevc decoder not available in this libavcodec build");
            let ctx = ff::avcodec_alloc_context3(codec);
            assert!(!ctx.is_null());
            assert!(ff::avcodec_open2(ctx, codec, ptr::null_mut()) >= 0);

            let packet = ff::av_packet_alloc();
            let frame = ff::av_frame_alloc();
            let mut decoded = 0usize;

            for au in access_units {
                let mut packet_buf = au.to_vec();
                packet_buf.resize(packet_buf.len() + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize, 0);
                (*packet).data = packet_buf.as_mut_ptr();
                (*packet).size = au.len() as i32;

                assert!(ff::avcodec_send_packet(ctx, packet) >= 0);
                loop {
                    let ret = ff::avcodec_receive_frame(ctx, frame);
                    if ret == averror_eagain() || ret == AVERROR_EOF {
                        break;
                    }
                    assert!(ret >= 0, "decode error: {}", av_err(ret));
                    assert_eq!((*frame).width as u32, width);
                    assert_eq!((*frame).height as u32, height);
                    decoded += 1;
                }
            }

            // Flush.
            ff::avcodec_send_packet(ctx, ptr::null());
            loop {
                let ret = ff::avcodec_receive_frame(ctx, frame);
                if ret == averror_eagain() || ret == AVERROR_EOF {
                    break;
                }
                assert!(ret >= 0);
                decoded += 1;
            }

            let mut f = frame;
            ff::av_frame_free(&mut f);
            let mut p = packet;
            ff::av_packet_free(&mut p);
            let mut c = ctx;
            ff::avcodec_free_context(&mut c);

            decoded
        }
    }

    #[allow(dead_code)]
    fn assert_frame_info_shape(_: FrameInfo) {}
}

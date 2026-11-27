//! H.264 decoding with libavcodec, for `vm ui` showing the screen streams that
//! [`super::H264Encoder`] writes.
//!
//! Only the leading fields of `AVPacket` and `AVFrame` are read and written;
//! they have kept their layout since FFmpeg 4.

use super::Decoded;
use std::{
    ffi::{c_char, c_int, c_void},
    ptr,
};

const CODEC_ID_H264: c_int = 27;
const PIXEL_FORMAT_YUV420P: c_int = 0;
const PIXEL_FORMAT_YUVJ420P: c_int = 12;
const AGAIN: c_int = -libc::EAGAIN;

#[repr(C)]
struct Packet {
    buf: *mut c_void,
    pts: i64,
    dts: i64,
    data: *const u8,
    size: c_int,
}

#[repr(C)]
struct Frame {
    data: [*const u8; 8],
    linesize: [c_int; 8],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
}

#[link(name = "avcodec")]
unsafe extern "C" {
    fn avcodec_find_decoder(id: c_int) -> *const c_void;
    fn avcodec_alloc_context3(codec: *const c_void) -> *mut c_void;
    fn avcodec_open2(context: *mut c_void, codec: *const c_void, options: *mut c_void) -> c_int;
    fn avcodec_free_context(context: *mut *mut c_void);
    fn avcodec_send_packet(context: *mut c_void, packet: *const Packet) -> c_int;
    fn avcodec_receive_frame(context: *mut c_void, frame: *mut Frame) -> c_int;
    fn av_packet_alloc() -> *mut Packet;
    fn av_packet_free(packet: *mut *mut Packet);
}

#[link(name = "avutil")]
unsafe extern "C" {
    fn av_frame_alloc() -> *mut Frame;
    fn av_frame_free(frame: *mut *mut Frame);
    fn av_opt_set(object: *mut c_void, name: *const c_char, value: *const c_char, flags: c_int) -> c_int;
}

/// A decoder for one stream.
pub struct H264Decoder {
    context: *mut c_void,
    packet: *mut Packet,
    frame: *mut Frame,
}

// Only used from its owner's thread.
unsafe impl Send for H264Decoder {}

impl H264Decoder {
    pub fn new() -> Self {
        unsafe {
            let codec = avcodec_find_decoder(CODEC_ID_H264);
            assert!(!codec.is_null(), "libavcodec has no H.264 decoder");
            let context = avcodec_alloc_context3(codec);
            assert!(!context.is_null(), "failed to allocate the H.264 decoder");
            // Every frame comes out as soon as it is in, since the stream never reorders.
            av_opt_set(context, c"flags".as_ptr(), c"+low_delay".as_ptr(), 0);
            assert_eq!(avcodec_open2(context, codec, ptr::null_mut()), 0, "failed to open the H.264 decoder");
            let decoder = Self { context, packet: av_packet_alloc(), frame: av_frame_alloc() };
            assert!(!decoder.packet.is_null() && !decoder.frame.is_null(), "failed to allocate decoder buffers");
            decoder
        }
    }

    /// Decodes one access unit in Annex B form; `None` until the first keyframe.
    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Option<Decoded>, String> {
        unsafe {
            (*self.packet).data = access_unit.as_ptr();
            (*self.packet).size = access_unit.len() as c_int;
            let sent = avcodec_send_packet(self.context, self.packet);
            (*self.packet).data = ptr::null();
            (*self.packet).size = 0;
            if sent < 0 {
                return Err(format!("decoding a frame failed: {sent}"));
            }
            match avcodec_receive_frame(self.context, self.frame) {
                0 => Ok(Some(to_bgra(&*self.frame))),
                AGAIN => Ok(None),
                error => Err(format!("decoding a frame failed: {error}")),
            }
        }
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        unsafe {
            av_frame_free(&mut self.frame);
            av_packet_free(&mut self.packet);
            avcodec_free_context(&mut self.context);
        }
    }
}

/// Converts a BT.601 limited-range I420 frame, which x264 writes, to BGRA.
unsafe fn to_bgra(frame: &Frame) -> Decoded {
    assert!(
        matches!(frame.format, PIXEL_FORMAT_YUV420P | PIXEL_FORMAT_YUVJ420P),
        "unexpected pixel format {} from the H.264 decoder",
        frame.format
    );
    let (width, height) = (frame.width as usize, frame.height as usize);
    let plane = |index: usize, x: usize, y: usize| unsafe { *frame.data[index].add(y * frame.linesize[index] as usize + x) };
    let mut bgra = Vec::with_capacity(width * height * 4);
    for y in 0..height {
        for x in 0..width {
            let luma = 1.164 * (f32::from(plane(0, x, y)) - 16.);
            let blue = f32::from(plane(1, x / 2, y / 2)) - 128.;
            let red = f32::from(plane(2, x / 2, y / 2)) - 128.;
            bgra.extend_from_slice(&[
                (luma + 2.018 * blue).clamp(0., 255.) as u8,
                (luma - 0.391 * blue - 0.813 * red).clamp(0., 255.) as u8,
                (luma + 1.596 * red).clamp(0., 255.) as u8,
                0xff,
            ]);
        }
    }
    Decoded { width: width as u32, height: height as u32, bgra }
}

//! H.264 encoding of the scanout with libx264, for screen streams to clients
//! on a network. Tuned like the macOS encoder for little CPU and bandwidth:
//! zero latency, no frame reordering and a single keyframe at the start, so a
//! still screen costs little and a change costs only what differs.
//!
//! libx264 versions its ABI with `X264_BUILD`; linking `x264_encoder_open_164`
//! pins build 164, whose leading struct fields are declared here.

use super::Capture;
use std::{
    ffi::{CString, c_char, c_int, c_void},
    time::Duration,
};

/// One access unit in Annex B form, with the parameter sets before keyframes.
pub struct Frame {
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// Bits per pixel and frame: about 1.8 Mbit/s for 1280x800 at 30 frames per
/// second, enough for crisp text once the screen settles.
const BITS_PER_PIXEL: f64 = 0.06;
const CSP_I420: c_int = 0x0002;

/// The start of `x264_param_t`.
#[repr(C)]
struct Param {
    _cpu: u32,
    _threads: c_int,
    _lookahead_threads: c_int,
    _sliced_threads: c_int,
    _deterministic: c_int,
    _cpu_independent: c_int,
    _sync_lookahead: c_int,
    width: c_int,
    height: c_int,
}

/// The start of `x264_picture_t`.
#[repr(C)]
struct Picture {
    _kind: c_int,
    _qpplus1: c_int,
    _pic_struct: c_int,
    keyframe: c_int,
    pts: i64,
    _dts: i64,
    _param: *mut c_void,
    _csp: c_int,
    _planes: c_int,
    stride: [c_int; 4],
    plane: [*mut u8; 4],
}

/// The start of `x264_nal_t`.
#[repr(C)]
struct Nal {
    _ref_idc: c_int,
    _kind: c_int,
    _long_startcode: c_int,
    _first_mb: c_int,
    _last_mb: c_int,
    _payload_len: c_int,
    payload: *mut u8,
}

/// Room for the whole of `x264_param_t` and `x264_picture_t`, which are larger
/// than the fields declared above.
type ParamStorage = [u64; 1024];
type PictureStorage = [u64; 512];

#[link(name = "x264")]
unsafe extern "C" {
    fn x264_param_default_preset(param: *mut c_void, preset: *const c_char, tune: *const c_char) -> c_int;
    fn x264_param_parse(param: *mut c_void, name: *const c_char, value: *const c_char) -> c_int;
    fn x264_param_apply_profile(param: *mut c_void, profile: *const c_char) -> c_int;
    fn x264_encoder_open_164(param: *mut c_void) -> *mut c_void;
    fn x264_encoder_encode(
        encoder: *mut c_void,
        nals: *mut *mut Nal,
        nal_count: *mut c_int,
        input: *mut c_void,
        output: *mut c_void,
    ) -> c_int;
    fn x264_encoder_close(encoder: *mut c_void);
    fn x264_picture_alloc(picture: *mut c_void, csp: c_int, width: c_int, height: c_int) -> c_int;
    fn x264_picture_clean(picture: *mut c_void);
}

pub struct H264Encoder {
    encoder: *mut c_void,
    input: Box<PictureStorage>,
    output: Box<PictureStorage>,
    size: (usize, usize),
}

unsafe impl Send for H264Encoder {}

impl H264Encoder {
    pub fn new(width: usize, height: usize, fps: u32) -> Result<Self, String> {
        let mut param: Box<ParamStorage> = Box::new([0; 1024]);
        let param_ptr = param.as_mut_ptr().cast::<c_void>();
        if unsafe { x264_param_default_preset(param_ptr, c"veryfast".as_ptr(), c"zerolatency".as_ptr()) } != 0 {
            return Err("failed to configure the H.264 encoder".into());
        }
        let bit_rate_kbit = ((width * height) as f64 * f64::from(fps) * BITS_PER_PIXEL / 1000.0) as u64;
        for (name, value) in [
            ("fps", fps.to_string()),
            ("bitrate", bit_rate_kbit.to_string()),
            ("keyint", "infinite".into()),
            ("repeat-headers", "1".into()),
            ("annexb", "1".into()),
        ] {
            let (name, value) = (CString::new(name).unwrap(), CString::new(value).unwrap());
            if unsafe { x264_param_parse(param_ptr, name.as_ptr(), value.as_ptr()) } != 0 {
                return Err(format!("failed to set the H.264 encoder's {name:?}"));
            }
        }
        let fields = unsafe { &mut *param_ptr.cast::<Param>() };
        fields.width = width as c_int;
        fields.height = height as c_int;
        if unsafe { x264_param_apply_profile(param_ptr, c"high".as_ptr()) } != 0 {
            return Err("failed to apply the H.264 High profile".into());
        }

        let mut input: Box<PictureStorage> = Box::new([0; 512]);
        if unsafe { x264_picture_alloc(input.as_mut_ptr().cast(), CSP_I420, width as c_int, height as c_int) } != 0 {
            return Err("failed to allocate an H.264 frame".into());
        }
        let encoder = unsafe { x264_encoder_open_164(param_ptr) };
        if encoder.is_null() {
            unsafe { x264_picture_clean(input.as_mut_ptr().cast()) };
            return Err("failed to create the H.264 encoder".into());
        }
        Ok(Self {
            encoder,
            input,
            output: Box::new([0; 512]),
            size: (width, height),
        })
    }

    pub fn size(&self) -> (usize, usize) {
        self.size
    }

    /// Encodes `capture`, which must have the encoder's size; `None` when the
    /// encoder dropped the frame.
    pub fn encode(&mut self, capture: &Capture, time: Duration) -> Result<Option<Frame>, String> {
        assert_eq!(capture.size(), self.size, "capture size differs from the encoder");
        let (width, height) = self.size;
        let picture = unsafe { &mut *self.input.as_mut_ptr().cast::<Picture>() };
        picture.pts = time.as_micros() as i64;
        capture.with_bgra(|bgra, stride| bgra_to_i420(bgra, stride, width, height, picture))?;

        let mut nals = std::ptr::null_mut();
        let mut nal_count = 0;
        let size = unsafe {
            x264_encoder_encode(
                self.encoder,
                &mut nals,
                &mut nal_count,
                self.input.as_mut_ptr().cast(),
                self.output.as_mut_ptr().cast(),
            )
        };
        if size < 0 {
            return Err(format!("failed to encode a frame: {size}"));
        }
        if size == 0 {
            return Ok(None);
        }
        // The payloads of all NALs of a frame follow each other in memory.
        let data = unsafe { std::slice::from_raw_parts((*nals).payload, size as usize) }.to_vec();
        let keyframe = unsafe { &*self.output.as_ptr().cast::<Picture>() }.keyframe != 0;
        Ok(Some(Frame { keyframe, data }))
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        unsafe {
            x264_encoder_close(self.encoder);
            x264_picture_clean(self.input.as_mut_ptr().cast());
        }
    }
}

/// Converts BGRA rows `stride` bytes apart to the picture's planes with the
/// BT.601 limited-range coefficients H.264 decoders assume, averaging chroma
/// over each 2x2 block.
fn bgra_to_i420(bgra: &[u8], stride: usize, width: usize, height: usize, picture: &mut Picture) {
    let plane = |index: usize| (picture.plane[index], picture.stride[index] as usize);
    let (luma, luma_stride) = plane(0);
    let (cb, cb_stride) = plane(1);
    let (cr, cr_stride) = plane(2);
    let pixel = |x: usize, y: usize| {
        let offset = y * stride + x * 4;
        (bgra[offset + 2] as i32, bgra[offset + 1] as i32, bgra[offset] as i32)
    };
    for y in 0..height {
        for x in 0..width {
            let (r, g, b) = pixel(x, y);
            let value = ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16;
            unsafe { *luma.add(y * luma_stride + x) = value as u8 };
        }
    }
    for y in 0..height.div_ceil(2) {
        for x in 0..width.div_ceil(2) {
            let (mut r, mut g, mut b, mut count) = (0, 0, 0, 0);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (px, py) = (2 * x + dx, 2 * y + dy);
                if px < width && py < height {
                    let (pr, pg, pb) = pixel(px, py);
                    (r, g, b, count) = (r + pr, g + pg, b + pb, count + 1);
                }
            }
            let (r, g, b) = (r / count, g / count, b / count);
            let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
            let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
            unsafe {
                *cb.add(y * cb_stride + x) = u as u8;
                *cr.add(y * cr_stride + x) = v as u8;
            }
        }
    }
}

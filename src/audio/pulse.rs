//! Playback on the host's default output through the PulseAudio simple API,
//! which PipeWire serves too.

use std::{
    ffi::{CStr, c_char, c_int, c_void},
    ptr,
};

const SAMPLE_S16LE: c_int = 3;
const STREAM_PLAYBACK: c_int = 1;
/// How much audio the server buffers before a write blocks: 40 ms.
const TARGET_BYTES: u32 = 48_000 * 4 / 25;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

#[link(name = "pulse-simple")]
unsafe extern "C" {
    fn pa_simple_new(
        server: *const c_char,
        name: *const c_char,
        direction: c_int,
        device: *const c_char,
        stream_name: *const c_char,
        spec: *const SampleSpec,
        channel_map: *const c_void,
        attr: *const BufferAttr,
        error: *mut c_int,
    ) -> *mut c_void;
    fn pa_simple_write(stream: *mut c_void, data: *const c_void, bytes: usize, error: *mut c_int) -> c_int;
    fn pa_simple_free(stream: *mut c_void);
}

#[link(name = "pulse")]
unsafe extern "C" {
    fn pa_strerror(error: c_int) -> *const c_char;
}

fn describe(error: c_int) -> String {
    unsafe { CStr::from_ptr(pa_strerror(error)) }
        .to_string_lossy()
        .into_owned()
}

/// 48 kHz stereo signed 16-bit little-endian, as the guest plays it.
pub struct Output(*mut c_void);

unsafe impl Send for Output {}

impl Output {
    pub fn open() -> Result<Self, String> {
        let spec = SampleSpec {
            format: SAMPLE_S16LE,
            rate: 48_000,
            channels: 2,
        };
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: TARGET_BYTES,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: u32::MAX,
        };
        let mut error = 0;
        let stream = unsafe {
            pa_simple_new(
                ptr::null(),
                c"varmint".as_ptr(),
                STREAM_PLAYBACK,
                ptr::null(),
                c"guest audio".as_ptr(),
                &spec,
                ptr::null(),
                &attr,
                &mut error,
            )
        };
        if stream.is_null() {
            return Err(format!("failed to connect to PulseAudio: {}", describe(error)));
        }
        Ok(Self(stream))
    }

    /// Queues `bytes`, blocking while the server's buffer is full.
    pub fn write(&self, bytes: &[u8]) -> Result<(), String> {
        let mut error = 0;
        if unsafe { pa_simple_write(self.0, bytes.as_ptr().cast(), bytes.len(), &mut error) } != 0 {
            return Err(format!("failed to play audio: {}", describe(error)));
        }
        Ok(())
    }
}

impl Drop for Output {
    fn drop(&mut self) {
        unsafe { pa_simple_free(self.0) };
    }
}

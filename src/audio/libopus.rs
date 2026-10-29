//! Opus encoding of the guest's audio with libopus, for streams to clients on
//! a network: about 96 kbit/s instead of 1.5 Mbit/s of PCM.

use std::ffi::{c_int, c_void};

const APPLICATION_AUDIO: c_int = 2049;
const SET_BITRATE_REQUEST: c_int = 4002;
const BIT_RATE: i32 = 96_000;
/// 20 ms at 48 kHz, the frame size browsers decode best.
const FRAMES_PER_PACKET: usize = 960;
/// 48 kHz stereo signed 16-bit little-endian.
const BYTES_PER_FRAME: usize = 4;
/// The largest packet libopus recommends to make room for.
const MAX_PACKET: usize = 4000;

#[link(name = "opus")]
unsafe extern "C" {
    fn opus_encoder_create(sample_rate: i32, channels: c_int, application: c_int, error: *mut c_int) -> *mut c_void;
    fn opus_encoder_ctl(encoder: *mut c_void, request: c_int, ...) -> c_int;
    fn opus_encode(encoder: *mut c_void, pcm: *const i16, frames: c_int, data: *mut u8, max_bytes: i32) -> i32;
    fn opus_encoder_destroy(encoder: *mut c_void);
}

pub struct OpusEncoder {
    encoder: *mut c_void,
    pending: Vec<u8>,
    packet: Vec<u8>,
}

unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new() -> Result<Self, String> {
        let mut error = 0;
        let encoder = unsafe { opus_encoder_create(48_000, 2, APPLICATION_AUDIO, &mut error) };
        if encoder.is_null() {
            return Err(format!("failed to create the Opus encoder: {error}"));
        }
        let encoder = Self {
            encoder,
            pending: Vec::new(),
            packet: vec![0; MAX_PACKET],
        };
        let status = unsafe { opus_encoder_ctl(encoder.encoder, SET_BITRATE_REQUEST, BIT_RATE) };
        if status != 0 {
            return Err(format!("failed to set the Opus bit rate: {status}"));
        }
        Ok(encoder)
    }

    /// Takes PCM and calls `on_packet` with each Opus packet of 20 ms it completes.
    pub fn encode(&mut self, pcm: &[u8], mut on_packet: impl FnMut(&[u8])) -> Result<(), String> {
        self.pending.extend_from_slice(pcm);
        let frame_bytes = FRAMES_PER_PACKET * BYTES_PER_FRAME;
        while self.pending.len() >= frame_bytes {
            let samples: Vec<i16> = self.pending[..frame_bytes]
                .chunks_exact(2)
                .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
                .collect();
            self.pending.drain(..frame_bytes);
            let len = unsafe {
                opus_encode(
                    self.encoder,
                    samples.as_ptr(),
                    FRAMES_PER_PACKET as c_int,
                    self.packet.as_mut_ptr(),
                    self.packet.len() as i32,
                )
            };
            if len < 0 {
                return Err(format!("failed to encode Opus: {len}"));
            }
            on_packet(&self.packet[..len as usize]);
        }
        Ok(())
    }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { opus_encoder_destroy(self.encoder) };
    }
}

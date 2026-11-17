//! Opus encoding of the guest's audio with AudioToolbox, for streams to
//! clients on a network: about 96 kbit/s instead of 1.5 Mbit/s of PCM.

use std::{ffi::c_void, mem, ptr};

type OSStatus = i32;

const FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const FORMAT_OPUS: u32 = u32::from_be_bytes(*b"opus");
const FLAGS_SIGNED_PACKED: u32 = (1 << 2) | (1 << 3);
const PROPERTY_ENCODE_BIT_RATE: u32 = u32::from_be_bytes(*b"brat");
const PROPERTY_MAXIMUM_OUTPUT_PACKET_SIZE: u32 = u32::from_be_bytes(*b"xops");

/// What the input callback answers once the PCM handed in so far is used up,
/// so the converter returns what it encoded and keeps the rest for later.
const NEEDS_INPUT: OSStatus = 1;

const BIT_RATE: u32 = 96_000;
/// 20 ms at 48 kHz, the frame size browsers decode best.
const FRAMES_PER_PACKET: u32 = 960;
/// 48 kHz stereo signed 16-bit little-endian.
const BYTES_PER_FRAME: u32 = 4;

#[repr(C)]
struct AudioStreamBasicDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBufferList {
    number_buffers: u32,
    buffers: [AudioBuffer; 1],
}

#[repr(C)]
struct AudioBuffer {
    number_channels: u32,
    data_byte_size: u32,
    data: *mut c_void,
}

#[repr(C)]
#[derive(Default)]
struct AudioStreamPacketDescription {
    start_offset: i64,
    variable_frames_in_packet: u32,
    data_byte_size: u32,
}

type InputCallback = extern "C" fn(
    *mut c_void,
    *mut u32,
    *mut AudioBufferList,
    *mut *mut AudioStreamPacketDescription,
    *mut c_void,
) -> OSStatus;

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioConverterNew(
        source: *const AudioStreamBasicDescription,
        destination: *const AudioStreamBasicDescription,
        converter: *mut *mut c_void,
    ) -> OSStatus;
    fn AudioConverterSetProperty(converter: *mut c_void, property: u32, size: u32, data: *const c_void) -> OSStatus;
    fn AudioConverterGetProperty(converter: *mut c_void, property: u32, size: *mut u32, data: *mut c_void) -> OSStatus;
    fn AudioConverterFillComplexBuffer(
        converter: *mut c_void,
        input: InputCallback,
        user_data: *mut c_void,
        packets: *mut u32,
        output: *mut AudioBufferList,
        descriptions: *mut AudioStreamPacketDescription,
    ) -> OSStatus;
    fn AudioConverterDispose(converter: *mut c_void) -> OSStatus;
}

pub struct OpusEncoder {
    converter: *mut c_void,
    input: Input,
    packet: Vec<u8>,
}

/// PCM waiting to be encoded, and the PCM last handed to the converter, which
/// must stay valid until it asks for more.
struct Input {
    pending: Vec<u8>,
    handed: Vec<u8>,
}

unsafe impl Send for OpusEncoder {}

impl OpusEncoder {
    pub fn new() -> Result<Self, String> {
        let source = AudioStreamBasicDescription {
            sample_rate: 48_000.0,
            format_id: FORMAT_LINEAR_PCM,
            format_flags: FLAGS_SIGNED_PACKED,
            bytes_per_packet: BYTES_PER_FRAME,
            frames_per_packet: 1,
            bytes_per_frame: BYTES_PER_FRAME,
            channels_per_frame: 2,
            bits_per_channel: 16,
            reserved: 0,
        };
        let destination = AudioStreamBasicDescription {
            sample_rate: 48_000.0,
            format_id: FORMAT_OPUS,
            format_flags: 0,
            bytes_per_packet: 0,
            frames_per_packet: FRAMES_PER_PACKET,
            bytes_per_frame: 0,
            channels_per_frame: 2,
            bits_per_channel: 0,
            reserved: 0,
        };
        let mut converter = ptr::null_mut();
        let status = unsafe { AudioConverterNew(&source, &destination, &mut converter) };
        if status != 0 {
            return Err(format!("failed to create the Opus encoder: {status}"));
        }
        let mut encoder = Self {
            converter,
            input: Input {
                pending: Vec::new(),
                handed: Vec::new(),
            },
            packet: Vec::new(),
        };
        let status = unsafe {
            AudioConverterSetProperty(
                converter,
                PROPERTY_ENCODE_BIT_RATE,
                size_of::<u32>() as u32,
                (&BIT_RATE as *const u32).cast(),
            )
        };
        if status != 0 {
            return Err(format!("failed to set the Opus bit rate: {status}"));
        }
        let mut max_packet = 0u32;
        let mut size = size_of::<u32>() as u32;
        let status = unsafe {
            AudioConverterGetProperty(
                converter,
                PROPERTY_MAXIMUM_OUTPUT_PACKET_SIZE,
                &mut size,
                (&raw mut max_packet).cast(),
            )
        };
        if status != 0 {
            return Err(format!("failed to get the Opus packet size: {status}"));
        }
        encoder.packet = vec![0; max_packet as usize];
        Ok(encoder)
    }

    /// Takes PCM and calls `on_packet` with each Opus packet of 20 ms it completes.
    pub fn encode(&mut self, pcm: &[u8], mut on_packet: impl FnMut(&[u8])) -> Result<(), String> {
        self.input.pending.extend_from_slice(pcm);
        loop {
            let mut packets = 1;
            let mut description = AudioStreamPacketDescription::default();
            let mut output = AudioBufferList {
                number_buffers: 1,
                buffers: [AudioBuffer {
                    number_channels: 2,
                    data_byte_size: self.packet.len() as u32,
                    data: self.packet.as_mut_ptr().cast(),
                }],
            };
            let status = unsafe {
                AudioConverterFillComplexBuffer(
                    self.converter,
                    provide_input,
                    (&raw mut self.input).cast(),
                    &mut packets,
                    &mut output,
                    &mut description,
                )
            };
            if status != 0 && status != NEEDS_INPUT {
                return Err(format!("failed to encode Opus: {status}"));
            }
            if packets == 0 {
                return Ok(());
            }
            on_packet(&self.packet[..description.data_byte_size as usize]);
        }
    }
}

extern "C" fn provide_input(
    _converter: *mut c_void,
    packets: *mut u32,
    data: *mut AudioBufferList,
    _descriptions: *mut *mut AudioStreamPacketDescription,
    user_data: *mut c_void,
) -> OSStatus {
    let input = unsafe { &mut *user_data.cast::<Input>() };
    input.handed = mem::take(&mut input.pending);
    let frames = input.handed.len() as u32 / BYTES_PER_FRAME;
    unsafe {
        *packets = frames;
        let buffer = &mut (*data).buffers[0];
        buffer.number_channels = 2;
        buffer.data_byte_size = frames * BYTES_PER_FRAME;
        buffer.data = input.handed.as_mut_ptr().cast();
    }
    if frames == 0 { NEEDS_INPUT } else { 0 }
}

impl Drop for OpusEncoder {
    fn drop(&mut self) {
        unsafe { AudioConverterDispose(self.converter) };
    }
}

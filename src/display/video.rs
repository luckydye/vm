//! Hardware H.264 encoding of the scanout with VideoToolbox, for screen
//! streams to clients on a network. Tuned for little CPU and bandwidth: the
//! hardware encoder in its low-latency mode, no frame reordering and a single
//! keyframe at the start, so a still screen costs nothing and a change costs
//! only what differs.

use super::Capture;
use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};
use std::{
    ffi::c_void,
    ptr,
    sync::mpsc::{Receiver, Sender, channel},
    time::Duration,
};

type OSStatus = i32;
type Encoded = Result<Option<Frame>, String>;

/// One access unit in Annex B form, with the parameter sets before keyframes.
pub struct Frame {
    pub keyframe: bool,
    pub data: Vec<u8>,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

const CM_TIME_VALID: u32 = 1;
const CM_TIME_INVALID: CMTime = CMTime { value: 0, timescale: 0, flags: 0, epoch: 0 };
const CODEC_H264: u32 = u32::from_be_bytes(*b"avc1");
const PIXEL_FORMAT_BGRA: i32 = i32::from_be_bytes(*b"BGRA");
const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// Bits per pixel and frame: about 1.8 Mbit/s for 1280x800 at 30 frames per
/// second, enough for crisp text once the screen settles.
const BITS_PER_PIXEL: f64 = 0.06;

type OutputCallback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, *mut c_void);

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoEncoderSpecification_EnableLowLatencyRateControl: &'static CFString;
    static kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder: &'static CFString;
    static kVTCompressionPropertyKey_RealTime: &'static CFString;
    static kVTCompressionPropertyKey_AllowFrameReordering: &'static CFString;
    static kVTCompressionPropertyKey_ProfileLevel: &'static CFString;
    static kVTCompressionPropertyKey_AverageBitRate: &'static CFString;
    static kVTCompressionPropertyKey_ExpectedFrameRate: &'static CFString;
    static kVTProfileLevel_H264_ConstrainedHigh_AutoLevel: &'static CFString;

    fn VTCompressionSessionCreate(
        allocator: *const c_void,
        width: i32,
        height: i32,
        codec: u32,
        encoder_specification: *const c_void,
        source_image_buffer_attributes: *const c_void,
        compressed_data_allocator: *const c_void,
        output_callback: OutputCallback,
        output_callback_refcon: *mut c_void,
        session: *mut *mut c_void,
    ) -> OSStatus;
    fn VTSessionSetProperty(session: *mut c_void, key: &CFString, value: &CFType) -> OSStatus;
    fn VTCompressionSessionPrepareToEncodeFrames(session: *mut c_void) -> OSStatus;
    fn VTCompressionSessionGetPixelBufferPool(session: *mut c_void) -> *mut c_void;
    fn VTCompressionSessionEncodeFrame(
        session: *mut c_void,
        image_buffer: *mut c_void,
        presentation_time: CMTime,
        duration: CMTime,
        frame_properties: *const c_void,
        source_frame_refcon: *mut c_void,
        info_flags: *mut u32,
    ) -> OSStatus;
    fn VTCompressionSessionInvalidate(session: *mut c_void);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: &'static CFString;
    static kCVPixelBufferWidthKey: &'static CFString;
    static kCVPixelBufferHeightKey: &'static CFString;
    static kCVPixelBufferIOSurfacePropertiesKey: &'static CFString;

    fn CVPixelBufferPoolCreatePixelBuffer(allocator: *const c_void, pool: *mut c_void, buffer: *mut *mut c_void) -> i32;
    fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: *mut c_void) -> *mut u8;
    fn CVPixelBufferGetBytesPerRow(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(buffer: *mut c_void) -> usize;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMSampleAttachmentKey_NotSync: &'static CFString;

    fn CMSampleBufferGetDataBuffer(sample: *mut c_void) -> *mut c_void;
    fn CMSampleBufferGetFormatDescription(sample: *mut c_void) -> *mut c_void;
    fn CMSampleBufferGetSampleAttachmentsArray(sample: *mut c_void, create: bool) -> *const c_void;
    fn CMBlockBufferGetDataLength(block: *mut c_void) -> usize;
    fn CMBlockBufferCopyDataBytes(block: *mut c_void, offset: usize, length: usize, destination: *mut u8) -> OSStatus;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        description: *mut c_void,
        index: usize,
        parameter_set: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_header_length: *mut i32,
    ) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
    fn CFArrayGetCount(array: *const c_void) -> isize;
    fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
    fn CFDictionaryContainsKey(dictionary: *const c_void, key: *const c_void) -> bool;
}

/// An encoder for one stream and one display size.
pub struct H264Encoder {
    session: *mut c_void,
    sender: *mut Sender<Encoded>,
    receiver: Receiver<Encoded>,
    size: (usize, usize),
}

// The session is only used from the thread that owns the encoder; VideoToolbox
// calls back on its own threads through the channel.
unsafe impl Send for H264Encoder {}

fn dictionary<const N: usize>(entries: [(&CFString, &CFType); N]) -> CFRetained<CFDictionary<CFString, CFType>> {
    let (keys, values): (Vec<_>, Vec<_>) = entries.iter().copied().unzip();
    CFDictionary::from_slices(&keys, &values)
}

fn check(status: OSStatus, what: &str) -> Result<(), String> {
    match status {
        0 => Ok(()),
        _ => Err(format!("{what} failed: {status}")),
    }
}

impl H264Encoder {
    pub fn new(width: usize, height: usize, fps: u32) -> Result<Self, String> {
        let yes: &CFType = CFBoolean::new(true);
        let no: &CFType = CFBoolean::new(false);
        let specification = unsafe {
            dictionary([
                (kVTVideoEncoderSpecification_EnableLowLatencyRateControl, yes),
                (kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder, yes),
            ])
        };
        let format = CFNumber::new_i32(PIXEL_FORMAT_BGRA);
        let pixel_width = CFNumber::new_i32(width as i32);
        let pixel_height = CFNumber::new_i32(height as i32);
        let iosurface = dictionary([]);
        let source = unsafe {
            dictionary([
                (kCVPixelBufferPixelFormatTypeKey, &format),
                (kCVPixelBufferWidthKey, &pixel_width),
                (kCVPixelBufferHeightKey, &pixel_height),
                (kCVPixelBufferIOSurfacePropertiesKey, &iosurface),
            ])
        };

        let (sender, receiver) = channel();
        let sender = Box::into_raw(Box::new(sender));
        let mut session = ptr::null_mut();
        let status = unsafe {
            VTCompressionSessionCreate(
                ptr::null(),
                width as i32,
                height as i32,
                CODEC_H264,
                CFRetained::as_ptr(&specification).as_ptr().cast(),
                CFRetained::as_ptr(&source).as_ptr().cast(),
                ptr::null(),
                on_encoded,
                sender.cast(),
                &mut session,
            )
        };
        if status != 0 {
            drop(unsafe { Box::from_raw(sender) });
            return Err(format!("failed to create the H.264 encoder: {status}"));
        }
        let encoder = Self {
            session,
            sender,
            receiver,
            size: (width, height),
        };

        let bit_rate = CFNumber::new_i64(((width * height) as f64 * f64::from(fps) * BITS_PER_PIXEL) as i64);
        let frame_rate = CFNumber::new_i32(fps as i32);
        let properties: [(&CFString, &CFType); 5] = unsafe {
            [
                (kVTCompressionPropertyKey_RealTime, yes),
                (kVTCompressionPropertyKey_AllowFrameReordering, no),
                (kVTCompressionPropertyKey_ProfileLevel, kVTProfileLevel_H264_ConstrainedHigh_AutoLevel),
                (kVTCompressionPropertyKey_AverageBitRate, &bit_rate),
                (kVTCompressionPropertyKey_ExpectedFrameRate, &frame_rate),
            ]
        };
        for (key, value) in properties {
            check(unsafe { VTSessionSetProperty(session, key, value) }, &format!("setting {key}"))?;
        }
        check(
            unsafe { VTCompressionSessionPrepareToEncodeFrames(session) },
            "preparing the H.264 encoder",
        )?;
        Ok(encoder)
    }

    pub fn size(&self) -> (usize, usize) {
        self.size
    }

    /// Encodes `capture`, which must have the encoder's size, and waits for the
    /// result; `None` when the encoder dropped the frame.
    pub fn encode(&mut self, capture: &Capture, time: Duration) -> Result<Option<Frame>, String> {
        assert_eq!(capture.size(), self.size, "capture size differs from the encoder");
        let pool = unsafe { VTCompressionSessionGetPixelBufferPool(self.session) };
        assert!(!pool.is_null(), "the H.264 encoder has no pixel buffer pool");
        let mut buffer = ptr::null_mut();
        check(
            unsafe { CVPixelBufferPoolCreatePixelBuffer(ptr::null(), pool, &mut buffer) },
            "allocating a frame buffer",
        )?;

        check(unsafe { CVPixelBufferLockBaseAddress(buffer, 0) }, "locking a frame buffer")?;
        let copied = unsafe {
            let stride = CVPixelBufferGetBytesPerRow(buffer);
            let pixels = std::slice::from_raw_parts_mut(
                CVPixelBufferGetBaseAddress(buffer),
                stride * CVPixelBufferGetHeight(buffer),
            );
            capture.copy_bgra(pixels, stride)
        };
        unsafe { CVPixelBufferUnlockBaseAddress(buffer, 0) };

        let presentation = CMTime {
            value: time.as_micros() as i64,
            timescale: 1_000_000,
            flags: CM_TIME_VALID,
            epoch: 0,
        };
        let status = copied.and_then(|()| {
            check(
                unsafe {
                    VTCompressionSessionEncodeFrame(
                        self.session,
                        buffer,
                        presentation,
                        CM_TIME_INVALID,
                        ptr::null(),
                        ptr::null_mut(),
                        ptr::null_mut(),
                    )
                },
                "encoding a frame",
            )
        });
        unsafe { CFRelease(buffer) };
        status?;
        self.receiver.recv().expect("the H.264 encoder never reported a frame")
    }
}

impl Drop for H264Encoder {
    fn drop(&mut self) {
        unsafe {
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session);
            drop(Box::from_raw(self.sender));
        }
    }
}

extern "C" fn on_encoded(refcon: *mut c_void, _: *mut c_void, status: OSStatus, _: u32, sample: *mut c_void) {
    let sender = unsafe { &*refcon.cast::<Sender<Encoded>>() };
    let encoded = match (status, sample.is_null()) {
        (0, true) => Ok(None),
        (0, false) => unsafe { annex_b(sample) }.map(Some),
        _ => Err(format!("encoding a frame failed: {status}")),
    };
    let _ = sender.send(encoded);
}

/// Converts an encoded sample from length-prefixed NAL units to start codes,
/// with the parameter sets in front of keyframes, so decoders need nothing else.
unsafe fn annex_b(sample: *mut c_void) -> Result<Frame, String> {
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, false);
        let keyframe = attachments.is_null()
            || CFArrayGetCount(attachments) == 0
            || !CFDictionaryContainsKey(
                CFArrayGetValueAtIndex(attachments, 0),
                (kCMSampleAttachmentKey_NotSync as *const CFString).cast(),
            );

        let mut data = Vec::new();
        if keyframe {
            let description = CMSampleBufferGetFormatDescription(sample);
            let mut count = 0;
            let mut index = 0;
            loop {
                let (mut set, mut size) = (ptr::null(), 0);
                check(
                    CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                        description,
                        index,
                        &mut set,
                        &mut size,
                        &mut count,
                        ptr::null_mut(),
                    ),
                    "reading the H.264 parameter sets",
                )?;
                data.extend_from_slice(&START_CODE);
                data.extend_from_slice(std::slice::from_raw_parts(set, size));
                index += 1;
                if index == count {
                    break;
                }
            }
        }

        let block = CMSampleBufferGetDataBuffer(sample);
        let length = CMBlockBufferGetDataLength(block);
        let start = data.len();
        data.resize(start + length, 0);
        check(
            CMBlockBufferCopyDataBytes(block, 0, length, data[start..].as_mut_ptr()),
            "reading an encoded frame",
        )?;
        // VideoToolbox writes 4-byte big-endian lengths, which are as long as start codes.
        let mut offset = start;
        while offset < data.len() {
            let nal_length = u32::from_be_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
            data[offset..offset + 4].copy_from_slice(&START_CODE);
            offset += 4 + nal_length;
        }
        assert_eq!(offset, data.len(), "malformed encoded frame from VideoToolbox");
        Ok(Frame { keyframe, data })
    }
}

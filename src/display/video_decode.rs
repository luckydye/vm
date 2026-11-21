//! Hardware H.264 decoding with VideoToolbox, for `vm ui` showing the screen
//! streams that [`super::H264Encoder`] writes.

use super::Decoded;
use objc2_core_foundation::{CFDictionary, CFNumber, CFRetained, CFString, CFType};
use std::{ffi::c_void, ptr};

type OSStatus = i32;

const PIXEL_FORMAT_BGRA: i32 = i32::from_be_bytes(*b"BGRA");
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const LOCK_READ_ONLY: u64 = 1;

#[repr(C)]
#[derive(Clone, Copy)]
struct CMTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

type OutputCallback = extern "C" fn(*mut c_void, *mut c_void, OSStatus, u32, *mut c_void, CMTime, CMTime);

#[repr(C)]
struct OutputCallbackRecord {
    callback: OutputCallback,
    refcon: *mut c_void,
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    fn VTDecompressionSessionCreate(
        allocator: *const c_void,
        format: *mut c_void,
        decoder_specification: *const c_void,
        image_buffer_attributes: *const c_void,
        output: *const OutputCallbackRecord,
        session: *mut *mut c_void,
    ) -> OSStatus;
    fn VTDecompressionSessionDecodeFrame(
        session: *mut c_void,
        sample: *mut c_void,
        flags: u32,
        frame_refcon: *mut c_void,
        info_flags: *mut u32,
    ) -> OSStatus;
    fn VTDecompressionSessionInvalidate(session: *mut c_void);
}

#[link(name = "CoreVideo", kind = "framework")]
unsafe extern "C" {
    static kCVPixelBufferPixelFormatTypeKey: &'static CFString;

    fn CVPixelBufferLockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferUnlockBaseAddress(buffer: *mut c_void, flags: u64) -> i32;
    fn CVPixelBufferGetBaseAddress(buffer: *mut c_void) -> *mut u8;
    fn CVPixelBufferGetBytesPerRow(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetWidth(buffer: *mut c_void) -> usize;
    fn CVPixelBufferGetHeight(buffer: *mut c_void) -> usize;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCFAllocatorNull: *const c_void;

    fn CMVideoFormatDescriptionCreateFromH264ParameterSets(
        allocator: *const c_void,
        count: usize,
        sets: *const *const u8,
        sizes: *const usize,
        nal_header_length: i32,
        description: *mut *mut c_void,
    ) -> OSStatus;
    fn CMBlockBufferCreateWithMemoryBlock(
        allocator: *const c_void,
        memory: *mut c_void,
        length: usize,
        block_allocator: *const c_void,
        custom_source: *const c_void,
        offset: usize,
        data_length: usize,
        flags: u32,
        block: *mut *mut c_void,
    ) -> OSStatus;
    fn CMSampleBufferCreateReady(
        allocator: *const c_void,
        data: *mut c_void,
        format: *mut c_void,
        samples: isize,
        timing_entries: isize,
        timing: *const c_void,
        size_entries: isize,
        sizes: *const usize,
        sample: *mut *mut c_void,
    ) -> OSStatus;
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFRelease(object: *const c_void);
}

fn check(status: OSStatus, what: &str) -> Result<(), String> {
    match status {
        0 => Ok(()),
        _ => Err(format!("{what} failed: {status}")),
    }
}

/// A decoder for one stream; it follows changes of the parameter sets.
pub struct H264Decoder {
    session: *mut c_void,
    format: *mut c_void,
    /// The parameter sets the session decodes with.
    sets: (Vec<u8>, Vec<u8>),
    /// The frame the output callback wrote last.
    output: Box<Option<Result<Decoded, String>>>,
}

// The decoder decodes synchronously, so it is only used from its owner's thread.
unsafe impl Send for H264Decoder {}

impl H264Decoder {
    pub fn new() -> Self {
        Self {
            session: ptr::null_mut(),
            format: ptr::null_mut(),
            sets: (Vec::new(), Vec::new()),
            output: Box::new(None),
        }
    }

    /// Decodes one access unit in Annex B form; `None` until the first keyframe.
    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Option<Decoded>, String> {
        let (mut sps, mut pps) = (None, None);
        let mut sample = Vec::with_capacity(access_unit.len());
        for nal in nal_units(access_unit) {
            match nal[0] & 0x1f {
                NAL_SPS => sps = Some(nal),
                NAL_PPS => pps = Some(nal),
                _ => {
                    sample.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    sample.extend_from_slice(nal);
                }
            }
        }
        if let (Some(sps), Some(pps)) = (sps, pps)
            && (sps, pps) != (self.sets.0.as_slice(), self.sets.1.as_slice())
        {
            self.open(sps.to_vec(), pps.to_vec())?;
        }
        if self.session.is_null() || sample.is_empty() {
            return Ok(None);
        }

        unsafe {
            let mut block = ptr::null_mut();
            check(
                CMBlockBufferCreateWithMemoryBlock(
                    ptr::null(),
                    sample.as_mut_ptr().cast(),
                    sample.len(),
                    kCFAllocatorNull,
                    ptr::null(),
                    0,
                    sample.len(),
                    0,
                    &mut block,
                ),
                "wrapping a frame",
            )?;
            let mut buffer = ptr::null_mut();
            let size = sample.len();
            let created = check(
                CMSampleBufferCreateReady(ptr::null(), block, self.format, 1, 0, ptr::null(), 1, &size, &mut buffer),
                "wrapping a frame",
            );
            CFRelease(block);
            created?;
            // Without the asynchronous flag the callback runs before this returns.
            let status = VTDecompressionSessionDecodeFrame(self.session, buffer, 0, ptr::null_mut(), ptr::null_mut());
            CFRelease(buffer);
            check(status, "decoding a frame")?;
        }
        self.output.take().transpose()
    }

    fn open(&mut self, sps: Vec<u8>, pps: Vec<u8>) -> Result<(), String> {
        self.close();
        let sets = [sps.as_ptr(), pps.as_ptr()];
        let sizes = [sps.len(), pps.len()];
        check(
            unsafe {
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    ptr::null(),
                    2,
                    sets.as_ptr(),
                    sizes.as_ptr(),
                    4,
                    &mut self.format,
                )
            },
            "reading the H.264 parameter sets",
        )?;
        let format = CFNumber::new_i32(PIXEL_FORMAT_BGRA);
        let (keys, values): ([&CFString; 1], [&CFType; 1]) = (unsafe { [kCVPixelBufferPixelFormatTypeKey] }, [&format]);
        let attributes: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&keys, &values);
        let record = OutputCallbackRecord {
            callback: on_decoded,
            refcon: (&raw mut *self.output).cast(),
        };
        check(
            unsafe {
                VTDecompressionSessionCreate(
                    ptr::null(),
                    self.format,
                    ptr::null(),
                    CFRetained::as_ptr(&attributes).as_ptr().cast(),
                    &record,
                    &mut self.session,
                )
            },
            "creating the H.264 decoder",
        )?;
        self.sets = (sps, pps);
        Ok(())
    }

    fn close(&mut self) {
        unsafe {
            if !self.session.is_null() {
                VTDecompressionSessionInvalidate(self.session);
                CFRelease(self.session);
                self.session = ptr::null_mut();
            }
            if !self.format.is_null() {
                CFRelease(self.format);
                self.format = ptr::null_mut();
            }
        }
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        self.close();
    }
}

extern "C" fn on_decoded(refcon: *mut c_void, _: *mut c_void, status: OSStatus, _: u32, image: *mut c_void, _: CMTime, _: CMTime) {
    let output = unsafe { &mut *refcon.cast::<Option<Result<Decoded, String>>>() };
    *output = match (status, image.is_null()) {
        (0, false) => Some(Ok(unsafe { copy_bgra(image) })),
        (0, true) => None,
        _ => Some(Err(format!("decoding a frame failed: {status}"))),
    };
}

/// Copies a BGRA pixel buffer into tightly packed rows.
unsafe fn copy_bgra(image: *mut c_void) -> Decoded {
    unsafe {
        CVPixelBufferLockBaseAddress(image, LOCK_READ_ONLY);
        let (width, height) = (CVPixelBufferGetWidth(image), CVPixelBufferGetHeight(image));
        let stride = CVPixelBufferGetBytesPerRow(image);
        let base = CVPixelBufferGetBaseAddress(image);
        let mut bgra = Vec::with_capacity(width * height * 4);
        for row in 0..height {
            bgra.extend_from_slice(std::slice::from_raw_parts(base.add(row * stride), width * 4));
        }
        CVPixelBufferUnlockBaseAddress(image, LOCK_READ_ONLY);
        Decoded { width: width as u32, height: height as u32, bgra }
    }
}

/// The NAL units of an Annex B byte stream, without their start codes.
fn nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= stream.len() {
        if stream[index..index + 3] == [0, 0, 1] {
            starts.push(index + 3);
            index += 3;
        } else {
            index += 1;
        }
    }
    let ends: Vec<usize> = starts
        .iter()
        .skip(1)
        .map(|&start| start - 3 - usize::from(start >= 4 && stream[start - 4] == 0))
        .chain([stream.len()])
        .collect();
    starts.into_iter().zip(ends).map(|(start, end)| &stream[start..end]).filter(|nal| !nal.is_empty())
}

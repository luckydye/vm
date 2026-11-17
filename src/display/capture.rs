#[cfg(target_os = "macos")]
use objc2::{AnyThread, runtime::AnyObject};
#[cfg(target_os = "macos")]
use objc2_app_kit::{NSBitmapImageFileType, NSBitmapImageRep, NSBitmapImageRepPropertyKey, NSDeviceRGBColorSpace};
#[cfg(target_os = "macos")]
use objc2_foundation::NSDictionary;
#[cfg(target_os = "macos")]
use objc2_io_surface::{IOSurfaceLockOptions, IOSurfaceRef};
#[cfg(target_os = "macos")]
use std::ptr;
use zerocopy::IntoBytes;

/// A copy of the current scanout, taken under the display lock.
pub enum Capture {
    Pixels { pixels: Vec<u32>, width: usize, height: usize },
    #[cfg(target_os = "macos")]
    IOSurface { id: u32, width: usize, height: usize },
}

impl Capture {
    pub fn size(&self) -> (usize, usize) {
        match *self {
            Self::Pixels { width, height, .. } => (width, height),
            #[cfg(target_os = "macos")]
            Self::IOSurface { width, height, .. } => (width, height),
        }
    }

    /// The pixels as tightly packed RGB rows.
    pub fn rgb(&self) -> Result<Vec<u8>, String> {
        let (width, height) = self.size();
        self.with_bgra(|bytes, stride| bgra_to_rgb(bytes, stride, width, height))
    }

    /// Copies the BGRA rows into `destination`, whose rows are `stride` bytes apart.
    #[cfg(target_os = "macos")]
    pub fn copy_bgra(&self, destination: &mut [u8], stride: usize) -> Result<(), String> {
        let (width, height) = self.size();
        self.with_bgra(|bytes, source_stride| {
            for (source, destination) in bytes.chunks(source_stride).zip(destination.chunks_mut(stride)).take(height) {
                destination[..width * 4].copy_from_slice(&source[..width * 4]);
            }
        })
    }

    pub fn encode_png(&self) -> Result<Vec<u8>, String> {
        let (width, height) = self.size();
        encode_png(&self.rgb()?, width, height)
    }

    /// Calls `read` with the BGRA rows and the bytes between them.
    #[cfg(target_os = "linux")]
    pub(super) fn with_bgra<T>(&self, read: impl FnOnce(&[u8], usize) -> T) -> Result<T, String> {
        let Self::Pixels { pixels, width, .. } = self;
        Ok(read(pixels.as_bytes(), width * 4))
    }

    /// Calls `read` with the BGRA rows and the bytes between them.
    #[cfg(target_os = "macos")]
    pub(super) fn with_bgra<T>(&self, read: impl FnOnce(&[u8], usize) -> T) -> Result<T, String> {
        let (width, height) = self.size();
        let id = match self {
            Self::Pixels { pixels, .. } => return Ok(read(pixels.as_bytes(), width * 4)),
            Self::IOSurface { id, .. } => *id,
        };
        let surface = IOSurfaceRef::lookup(id).ok_or("scanout IOSurface was released before it could be read")?;
        if surface.width() < width || surface.height() < height {
            return Err("scanout IOSurface is smaller than the display".into());
        }

        let status = unsafe { surface.lock(IOSurfaceLockOptions::ReadOnly, ptr::null_mut()) };
        if status != 0 {
            return Err(format!("IOSurfaceLock failed: {status}"));
        }
        let stride = surface.bytes_per_row();
        let bytes = unsafe { std::slice::from_raw_parts(surface.base_address().as_ptr().cast::<u8>(), stride * height) };
        let result = read(bytes, stride);
        unsafe { surface.unlock(IOSurfaceLockOptions::ReadOnly, ptr::null_mut()) };
        Ok(result)
    }
}

fn bgra_to_rgb(bytes: &[u8], stride: usize, width: usize, height: usize) -> Vec<u8> {
    let mut rgb = vec![0; width * height * 3];
    for (row, out) in bytes.chunks(stride).zip(rgb.chunks_exact_mut(width * 3)) {
        for (pixel, out) in row[..width * 4].chunks_exact(4).zip(out.chunks_exact_mut(3)) {
            out.copy_from_slice(&[pixel[2], pixel[1], pixel[0]]);
        }
    }
    rgb
}

#[cfg(target_os = "linux")]
fn encode_png(rgb: &[u8], width: usize, height: usize) -> Result<Vec<u8>, String> {
    let mut png = Vec::new();
    let mut encoder = png::Encoder::new(&mut png, width as u32, height as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(rgb))
        .map_err(|error| format!("failed to encode the screenshot as PNG: {error}"))?;
    Ok(png)
}

#[cfg(target_os = "macos")]
fn encode_png(rgb: &[u8], width: usize, height: usize) -> Result<Vec<u8>, String> {
    let rep = unsafe {
        NSBitmapImageRep::initWithBitmapDataPlanes_pixelsWide_pixelsHigh_bitsPerSample_samplesPerPixel_hasAlpha_isPlanar_colorSpaceName_bytesPerRow_bitsPerPixel(
            NSBitmapImageRep::alloc(),
            ptr::null_mut(),
            width as isize,
            height as isize,
            8,
            3,
            false,
            false,
            NSDeviceRGBColorSpace,
            (width * 3) as isize,
            24,
        )
    }
    .ok_or("failed to allocate a bitmap for the screenshot")?;

    unsafe { std::slice::from_raw_parts_mut(rep.bitmapData(), rgb.len()) }.copy_from_slice(rgb);

    let properties = NSDictionary::<NSBitmapImageRepPropertyKey, AnyObject>::new();
    let data = unsafe { rep.representationUsingType_properties(NSBitmapImageFileType::PNG, &properties) }
        .ok_or("failed to encode the screenshot as PNG")?;
    Ok(data.to_vec())
}

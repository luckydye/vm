#[cfg(target_os = "macos")]
mod angle;
mod capture;
#[cfg(target_os = "macos")]
mod iosurface;
mod scanout;
#[cfg(target_os = "macos")]
mod video;
#[cfg(target_os = "linux")]
#[path = "x264.rs"]
mod video;
#[cfg(target_os = "macos")]
mod video_decode;
#[cfg(target_os = "linux")]
#[path = "avcodec.rs"]
mod video_decode;

pub use capture::Capture;
pub use scanout::ScanoutPublisher;
pub use video::H264Encoder;
pub use video_decode::H264Decoder;
use zerocopy::IntoBytes;

/// A decoded frame of a screen stream, in tightly packed BGRA rows.
pub struct Decoded {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Rect {
    x: usize,
    y: usize,
    width: usize,
    height: usize,
}

pub struct DisplayBuffer {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
    dirty_rect: Option<(usize, usize, usize, usize)>,
    iosurface_id: Option<u32>,
    sequence: u64,
}

impl DisplayBuffer {
    pub fn new() -> Self {
        Self {
            width: 0,
            height: 0,
            pixels: Vec::new(),
            dirty_rect: None,
            iosurface_id: None,
            sequence: 0,
        }
    }

    /// Changes whenever the guest presents a new frame.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    /// Returns `None` until the guest has configured a scanout.
    pub fn capture(&self) -> Option<Capture> {
        if self.width == 0 || self.height == 0 {
            return None;
        }
        Some(match self.iosurface_id {
            #[cfg(target_os = "macos")]
            Some(id) => Capture::IOSurface {
                id,
                width: self.width,
                height: self.height,
            },
            _ => Capture::Pixels {
                pixels: self.pixels.clone(),
                width: self.width,
                height: self.height,
            },
        })
    }

    fn resize(&mut self, width: usize, height: usize) {
        if self.width == width && self.height == height {
            return;
        }

        self.width = width;
        self.height = height;
        self.pixels.resize(width * height, 0);
        self.dirty_rect = Some((0, 0, width, height));
        self.iosurface_id = None;
        self.sequence = self.sequence.wrapping_add(1);
    }

    fn publish_pixels(&mut self, data: &[u8], stride: usize, source: Rect, damage: Rect) {
        let Some((x, y, width, height)) = self.clip_damage(source, damage) else {
            return;
        };

        let copy_src_x0 = source.x + x;
        let copy_src_y0 = source.y + y;
        let copy_src_x1 = copy_src_x0 + width;
        let copy_src_y1 = copy_src_y0 + height;

        if data.len() < copy_src_y1 * stride {
            return;
        }

        let dst_width = self.width;
        for (src_y, dst_y) in (copy_src_y0..copy_src_y1).zip(y..y + height) {
            let src_row = src_y * stride;
            let src = &data[src_row + copy_src_x0 * 4..src_row + copy_src_x1 * 4];
            let dst = &mut self.pixels[dst_y * dst_width + x..dst_y * dst_width + x + width];
            dst.as_mut_bytes().copy_from_slice(src);
        }

        self.publish_damage(None, (x, y, width, height));
    }

    #[cfg(target_os = "macos")]
    fn publish_iosurface(&mut self, iosurface_id: u32, source: Rect, damage: Rect) {
        let Some(dirty_rect) = self.clip_damage(source, damage) else {
            return;
        };

        self.publish_damage(Some(iosurface_id), dirty_rect);
    }

    fn clip_damage(&self, source: Rect, damage: Rect) -> Option<(usize, usize, usize, usize)> {
        let damage_x1 = damage.x.saturating_add(damage.width);
        let damage_y1 = damage.y.saturating_add(damage.height);
        let source_x1 = source.x.saturating_add(source.width);
        let source_y1 = source.y.saturating_add(source.height);

        let src_x0 = damage.x.max(source.x);
        let src_y0 = damage.y.max(source.y);
        let src_x1 = damage_x1.min(source_x1);
        let src_y1 = damage_y1.min(source_y1);

        if src_x0 >= src_x1 || src_y0 >= src_y1 {
            return None;
        }

        let x0 = (src_x0 - source.x).min(self.width);
        let y0 = (src_y0 - source.y).min(self.height);
        let x1 = (src_x1 - source.x).min(self.width);
        let y1 = (src_y1 - source.y).min(self.height);

        (x0 < x1 && y0 < y1).then_some((x0, y0, x1 - x0, y1 - y0))
    }

    fn publish_damage(&mut self, iosurface_id: Option<u32>, dirty_rect: (usize, usize, usize, usize)) {
        self.iosurface_id = iosurface_id;
        self.dirty_rect = Some(match self.dirty_rect {
            Some(old) => merge_rects(old, dirty_rect),
            None => dirty_rect,
        });
        self.sequence = self.sequence.wrapping_add(1);
    }
}

fn merge_rects(
    (old_x, old_y, old_width, old_height): (usize, usize, usize, usize),
    (new_x, new_y, new_width, new_height): (usize, usize, usize, usize),
) -> (usize, usize, usize, usize) {
    let x0 = old_x.min(new_x);
    let y0 = old_y.min(new_y);
    let x1 = old_x.saturating_add(old_width).max(new_x.saturating_add(new_width));
    let y1 = old_y.saturating_add(old_height).max(new_y.saturating_add(new_height));
    (x0, y0, x1 - x0, y1 - y0)
}

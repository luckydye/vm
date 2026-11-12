//! Presents the scanout on X11 and Wayland by copying its pixels into the
//! window's shared-memory buffer, with the statistics HUD drawn over it.

use super::stats::{HEIGHT as STATS_HEIGHT, MARGIN as STATS_MARGIN, StatsHud, WIDTH as STATS_WIDTH};
use softbuffer::{Context, Rect, Surface};
use std::{num::NonZeroU32, rc::Rc, time::Instant};
use winit::window::Window;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PresentMode {
    NewFrame,
    Redraw,
}

/// The pixel channels softbuffer reads: the alpha byte must be zero.
const RGB: u32 = 0x00ff_ffff;

pub struct Presenter {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
    width: u32,
    height: u32,
    stats: StatsHud,
    stats_pixels: Vec<u32>,
}

impl Presenter {
    pub fn window(&self) -> &Window {
        &self.window
    }

    pub fn new(window: Window, width: u32, height: u32) -> Self {
        let window = Rc::new(window);
        let context =
            Context::new(Rc::clone(&window)).unwrap_or_else(|error| panic!("no display for the window: {error}"));
        let surface = Surface::new(&context, Rc::clone(&window))
            .unwrap_or_else(|error| panic!("cannot draw in the window: {error}"));
        let mut presenter = Self {
            window,
            surface,
            width: 0,
            height: 0,
            stats: StatsHud::new(),
            stats_pixels: Vec::new(),
        };
        presenter.resize_surface(width, height);
        presenter
    }

    pub fn resize_surface(&mut self, width: u32, height: u32) {
        let width = width.max(1);
        let height = height.max(1);
        if self.width == width && self.height == height {
            return;
        }
        self.surface
            .resize(NonZeroU32::new(width).unwrap(), NonZeroU32::new(height).unwrap())
            .unwrap_or_else(|error| panic!("cannot resize the window buffer: {error}"));
        self.width = width;
        self.height = height;
    }

    pub fn set_stats_visible(&mut self, visible: bool) {
        self.stats.set_visible(visible);
        self.sync_stats_pixels();
    }

    fn sync_stats_pixels(&mut self) {
        if let Some(pixels) = self.stats.take_pixels() {
            self.stats_pixels = pixels;
        }
    }

    /// Shows `pixels`, the whole `pw` by `ph` scanout, of which the dirty rect
    /// changed. Scanouts are never IOSurfaces on Linux.
    pub fn present_iosurface_or_rect(
        &mut self,
        iosurface_id: Option<u32>,
        pixels: &[u32],
        pw: u32,
        ph: u32,
        dirty_x: u32,
        dirty_y: u32,
        dirty_w: u32,
        dirty_h: u32,
        mode: PresentMode,
    ) -> bool {
        assert!(iosurface_id.is_none(), "IOSurface scanouts exist only on macOS");
        let started = (mode == PresentMode::NewFrame && self.stats.is_visible()).then(Instant::now);

        let presented = self.present_rect(pixels, pw, ph, (dirty_x, dirty_y, dirty_w, dirty_h), mode);

        if let Some(started) = started {
            self.stats.record_frame(presented, started.elapsed());
        }
        self.stats.refresh();
        self.sync_stats_pixels();

        presented
    }

    fn present_rect(
        &mut self,
        pixels: &[u32],
        width: u32,
        height: u32,
        dirty: (u32, u32, u32, u32),
        mode: PresentMode,
    ) -> bool {
        if width == 0 || height == 0 || pixels.len() < (width * height) as usize {
            return false;
        }
        self.resize_surface(width, height);

        let mut buffer = self
            .surface
            .buffer_mut()
            .unwrap_or_else(|error| panic!("cannot draw in the window: {error}"));
        for (destination, source) in buffer.iter_mut().zip(pixels) {
            *destination = source & RGB;
        }

        let stats_visible = self.stats.is_visible() && !self.stats_pixels.is_empty();
        if stats_visible {
            let x0 = STATS_MARGIN.min((width as usize).saturating_sub(STATS_WIDTH as usize));
            let y0 = STATS_MARGIN.min((height as usize).saturating_sub(STATS_HEIGHT as usize));
            let stats_w = (STATS_WIDTH as usize).min(width as usize - x0);
            let stats_h = (STATS_HEIGHT as usize).min(height as usize - y0);
            for row in 0..stats_h {
                let source = &self.stats_pixels[row * STATS_WIDTH as usize..][..stats_w];
                let start = (y0 + row) * width as usize + x0;
                for (destination, source) in buffer[start..start + stats_w].iter_mut().zip(source) {
                    *destination = source & RGB;
                }
            }
        }

        let (x, y, w, h) = dirty;
        let damage = if mode == PresentMode::Redraw || stats_visible || w == 0 || h == 0 {
            Rect {
                x: 0,
                y: 0,
                width: NonZeroU32::new(width).unwrap(),
                height: NonZeroU32::new(height).unwrap(),
            }
        } else {
            Rect {
                x,
                y,
                width: NonZeroU32::new(w).unwrap(),
                height: NonZeroU32::new(h).unwrap(),
            }
        };
        buffer
            .present_with_damage(&[damage])
            .unwrap_or_else(|error| panic!("cannot present the window: {error}"));
        true
    }
}

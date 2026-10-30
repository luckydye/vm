use super::{DisplayBuffer, DisplayNotifier, Rect};
#[cfg(target_os = "macos")]
use super::{angle::AngleCopy, iosurface::ScopedIOSurface};
use crate::virtio::gpu::{PresentFrame, PresentSource, Presentation};
use std::sync::Mutex;

pub struct ScanoutPublisher<'a> {
    display: &'a Mutex<DisplayBuffer>,
    display_notifier: DisplayNotifier,
    #[cfg(target_os = "macos")]
    angle_copy: AngleCopy,
    #[cfg(target_os = "macos")]
    iosurface: Option<ScopedIOSurface>,
}

impl<'a> ScanoutPublisher<'a> {
    pub fn new(display: &'a Mutex<DisplayBuffer>, display_notifier: DisplayNotifier) -> Self {
        Self {
            display,
            display_notifier,
            #[cfg(target_os = "macos")]
            angle_copy: AngleCopy::new(),
            #[cfg(target_os = "macos")]
            iosurface: None,
        }
    }

    pub fn present(&mut self, presentation: Presentation<'_>) -> bool {
        match presentation {
            Presentation::Configure { width, height } => {
                self.configure(width, height);
                true
            }
            Presentation::Frame(frame) => self.present_frame(frame),
        }
    }

    fn configure(&mut self, width: u32, height: u32) {
        self.display.lock().unwrap().resize(width as usize, height as usize);
        self.display_notifier.notify();
    }

    fn present_frame(&mut self, frame: PresentFrame<'_>) -> bool {
        let source = Rect {
            x: frame.source_rect.x as usize,
            y: frame.source_rect.y as usize,
            width: frame.source_rect.width as usize,
            height: frame.source_rect.height as usize,
        };
        let damage = Rect {
            x: frame.damage.x as usize,
            y: frame.damage.y as usize,
            width: frame.damage.width as usize,
            height: frame.damage.height as usize,
        };

        let published = match frame.source {
            PresentSource::Pixels { data, stride, .. } => {
                self.display
                    .lock()
                    .unwrap()
                    .publish_pixels(data, stride as usize, source, damage)
            }
            #[cfg(target_os = "linux")]
            PresentSource::NativeTexture(texture) => match texture {},
            #[cfg(target_os = "macos")]
            PresentSource::NativeTexture(texture) => {
                let recreate = self.iosurface.as_ref().is_none_or(|surface| {
                    surface.width() != frame.source_rect.width || surface.height() != frame.source_rect.height
                });

                if recreate {
                    self.iosurface = ScopedIOSurface::new_bgra(frame.source_rect.width, frame.source_rect.height);
                }

                let Some(surface) = self.iosurface.as_ref() else {
                    return false;
                };

                if !self.angle_copy.copy_native_texture_to_iosurface(
                    texture,
                    surface.as_ptr(),
                    (
                        frame.source_rect.x,
                        frame.source_rect.y,
                        frame.source_rect.width,
                        frame.source_rect.height,
                    ),
                ) {
                    eprintln!("virtio-gpu: ANGLE producer copy failed; falling back to readback");
                    return false;
                }

                self.display
                    .lock()
                    .unwrap()
                    .publish_iosurface(surface.id(), source, damage)
            }
        };

        if published {
            self.display_notifier.notify();
        }

        true
    }
}

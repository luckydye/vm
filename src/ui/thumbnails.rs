//! The overview's small screen previews, taken from each running machine now
//! and then. They are stills, not streams, and reading a screen is no
//! activity, so they do not keep idle machines from suspending.

use crate::{
    node::{self, Target},
    protocol::Request,
};
use gpui::{AppContext, AsyncApp, Context, RenderImage, Task, WeakEntity};
use image::{Frame, RgbaImage};
use std::{
    collections::{BTreeMap, HashMap},
    io::Read,
    sync::Arc,
    time::Duration,
};

/// How long a preview is shown before the machine is asked for a new one.
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

/// How wide a preview is scaled down to, enough for a card on a high
/// resolution screen.
const PREVIEW_WIDTH: u32 = 640;

pub struct Thumbnails {
    /// Each machine's latest screen, by the name the vm CLI takes.
    pub frames: HashMap<String, Arc<RenderImage>>,
    /// The machine each preview task watches.
    watches: BTreeMap<String, Task<()>>,
}

impl Thumbnails {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self { frames: HashMap::new(), watches: BTreeMap::new() }
    }

    /// Stops refreshing, keeping the latest previews for when they show again.
    pub fn pause(&mut self) {
        self.watches.clear();
    }

    /// Keeps a preview of each of `running`, and of `fading`, whose cards are
    /// still leaving, and forgets the rest.
    pub fn follow(&mut self, running: &[String], fading: &[String], cx: &mut Context<Self>) {
        self.frames.retain(|name, frame| {
            let shown = running.contains(name) || fading.contains(name);
            if !shown {
                cx.drop_image(frame.clone(), None);
            }
            shown
        });
        self.watches.retain(|name, _| running.contains(name));
        for name in running {
            if self.watches.contains_key(name) {
                continue;
            }
            let watched = name.clone();
            let watch = cx.spawn(async move |this, cx| watch(watched, this, cx).await);
            self.watches.insert(name.clone(), watch);
        }
        cx.notify();
    }
}

/// Asks `name` for its screen until the overview stops following it.
async fn watch(name: String, this: WeakEntity<Thumbnails>, cx: &mut AsyncApp) {
    loop {
        let asked = name.clone();
        let preview = cx.background_spawn(async move { screenshot(&asked) }).await;
        let alive = this.update(cx, |this, cx| {
            // The machine stopped while its screenshot was on the way.
            if !this.watches.contains_key(&name) {
                return;
            }
            // A machine that is still booting has no screen yet, so the next ask retries.
            if let Ok(frame) = preview {
                if let Some(old) = this.frames.insert(name.clone(), frame) {
                    cx.drop_image(old, None);
                }
                cx.notify();
            }
        });
        if alive.is_err() {
            return;
        }
        cx.background_executor().timer(REFRESH_INTERVAL).await;
    }
}

/// A machine's screen, scaled down into the BGRA bytes gpui images carry.
fn screenshot(name: &str) -> Result<Arc<RenderImage>, String> {
    let target = Target::resolve(name)?;
    let mut control = node::request(&target, &Request::Screenshot)?;
    let length = control.reply.png_len.ok_or_else(|| format!("{name} sent a screenshot without a length"))?;
    let mut png = vec![0; length];
    control.reader.read_exact(&mut png).map_err(|error| format!("{name}: {error}"))?;

    let screen = image::load_from_memory(&png).map_err(|error| format!("{name}: {error}"))?;
    let scaled = screen.thumbnail(PREVIEW_WIDTH, PREVIEW_WIDTH).to_rgba8();
    let (width, height) = scaled.dimensions();
    let mut pixels = scaled.into_raw();
    for pixel in pixels.chunks_exact_mut(4) {
        pixel.swap(0, 2);
    }
    let buffer = RgbaImage::from_raw(width, height, pixels).expect("a scaled screen holds width times height pixels");
    Ok(Arc::new(RenderImage::new([Frame::new(buffer)])))
}

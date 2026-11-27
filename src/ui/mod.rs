//! `vm ui`: a window with the machines of this computer and of every node,
//! their screens, and the actions of the CLI.

mod appearance;
mod display;
mod layout;
mod machines;
mod ports;
mod shell;
mod shell_view;
mod theme;
mod thumbnails;
mod upload;
mod view;

use gpui::{App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, actions, point, prelude::*, px, size};
use gpui_component::Root;
use libghostty_vt::kitty::graphics::{RustPngDecoder, set_png_decoder};

actions!(vm_ui, [ToggleSidebar, Quit]);

pub fn run() {
    // SAFETY: no terminal exists yet, so no other thread is inside libghostty.
    unsafe { set_png_decoder(Some(Box::new(RustPngDecoder::new()))) }.expect("failed to install the kitty PNG decoder");
    gpui_platform::application().with_assets(gpui_kit_assets::Assets).run(|cx: &mut App| {
        #[cfg(target_os = "macos")]
        set_application_icon();
        gpui_component::init(cx);
        theme::apply(cx.window_appearance(), cx);
        // Ctrl keys belong to the machine while its screen or a terminal has
        // focus, so off macOS the window's shortcuts wait outside those panes.
        let ([sidebar, quit], context) = match cfg!(target_os = "macos") {
            true => (["cmd-b", "cmd-q"], None),
            false => (["ctrl-b", "ctrl-q"], Some("!Pane")),
        };
        cx.bind_keys([
            KeyBinding::new(sidebar, ToggleSidebar, context),
            KeyBinding::new(quit, Quit, context),
            // gpui-component's Root moves focus on Tab; in a pane it belongs to the machine.
            KeyBinding::new("tab", gpui::NoAction, Some("Pane")),
            KeyBinding::new("shift-tab", gpui::NoAction, Some("Pane")),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1100.), px(720.)), cx))),
            titlebar: Some(TitlebarOptions {
                title: Some("vm".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(14.), px(16.))),
            }),
            window_min_size: Some(size(px(720.), px(480.))),
            ..WindowOptions::default()
        };
        cx.open_window(options, |window, cx| {
            let workspace = cx.new(|cx| view::Workspace::new(window, cx));
            cx.new(|cx| Root::new(workspace, window, cx))
        })
        .expect("failed to open the window");
        cx.activate(true);
    });
}

/// The Dock and Cmd-Tab icon of `vm ui`, which no app bundle supplies.
#[cfg(target_os = "macos")]
fn set_application_icon() {
    use objc2::{AllocAnyThread, MainThreadMarker, rc::autoreleasepool};
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::NSData;

    const ICON: &[u8] = include_bytes!("../../assets/icon.png");
    let mtm = MainThreadMarker::new().expect("the ui runs on the main thread");
    autoreleasepool(|_| {
        let data = NSData::with_bytes(ICON);
        let icon = NSImage::initWithData(NSImage::alloc(), &data).expect("the app icon is a valid PNG");
        unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&icon)) };
    });
}

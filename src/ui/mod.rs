//! `vm ui`: a window with the machines of this computer and of every node,
//! their screens, and the actions of the CLI.

mod display;
mod machines;
mod shell;
mod shell_view;
mod theme;
mod view;

use gpui::{App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, actions, point, prelude::*, px, size};
use gpui_component::Root;

actions!(vm_ui, [FocusSearch, ToggleSidebar, Quit]);

pub fn run() {
    gpui_platform::application().with_assets(gpui_kit_assets::Assets).run(|cx: &mut App| {
        gpui_component::init(cx);
        theme::apply(cx);
        // Ctrl keys belong to the machine while its screen or a terminal has
        // focus, so off macOS the window's shortcuts wait outside those panes.
        let ([search, sidebar, quit], context) = match cfg!(target_os = "macos") {
            true => (["cmd-k", "cmd-b", "cmd-q"], None),
            false => (["ctrl-k", "ctrl-b", "ctrl-q"], Some("!Pane")),
        };
        cx.bind_keys([
            KeyBinding::new(search, FocusSearch, context),
            KeyBinding::new(sidebar, ToggleSidebar, context),
            KeyBinding::new(quit, Quit, context),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                cx.quit();
            }
        })
        .detach();

        let options = WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(1280.), px(820.)), cx))),
            titlebar: Some(TitlebarOptions {
                title: Some("vm".into()),
                appears_transparent: true,
                traffic_light_position: Some(point(px(26.), px(26.))),
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

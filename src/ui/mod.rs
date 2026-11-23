//! `vm ui`: a window with the machines of this computer and of every node,
//! their screens, and the actions of the CLI.

mod display;
mod layout;
mod machines;
mod ports;
mod shell;
mod shell_view;
mod theme;
mod view;

use gpui::{App, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, actions, point, prelude::*, px, size};
use gpui_component::Root;

actions!(vm_ui, [ToggleSidebar, Quit]);

pub fn run() {
    gpui_platform::application().with_assets(gpui_kit_assets::Assets).run(|cx: &mut App| {
        gpui_component::init(cx);
        theme::apply(cx);
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

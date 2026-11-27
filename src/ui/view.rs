//! The window of `vm ui`: a floating sidebar with the machines, and a main
//! pane with the overview or the selected machine's screen and terminals,
//! tiled side by side and stacked. Views render the state of [`Machines`],
//! [`Display`], [`Shell`], [`Ports`] and the [`layout`] and forward what the
//! user does.

use super::{
    ToggleSidebar,
    appearance,
    display::{self, Display},
    layout::{self, Axis, Node, Pane, Step},
    machines::{self, Machine, Machines, NewMachine, Notice, PRESETS, Preset, Tone},
    ports::Ports,
    shell::{Shell, ShellEvent},
    shell_view,
    theme::{self, *},
    thumbnails::Thumbnails,
    upload,
};
use crate::{
    keymap,
    protocol::InputEvent,
    virtio::input::keys::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT},
};
use gpui::{
    Animation, AnimationExt, AnyElement, Bounds, Context, Entity, ExternalPaths, FocusHandle, Focusable, Hsla,
    KeyDownEvent, KeyUpEvent, Keystroke, Modifiers, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, ObjectFit, PathPromptOptions, Pixels, ScrollDelta, ScrollEvent, SharedString, Subscription, Window,
    canvas, div, ease_out_quint, img, prelude::*, px, rgb, rgba, size,
};
use gpui_component::{
    Icon, Sizable, ThemeMode, WindowExt,
    button::{Button, ButtonRounded, ButtonVariant},
    input::{Input, InputEvent as TextEvent, InputState},
    menu::{DropdownMenu, PopupMenuItem},
    tooltip::Tooltip,
};
use gpui_kit_assets::IconName;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap, HashSet},
    path::PathBuf,
    rc::Rc,
    time::{Duration, Instant},
};

/// How long a machine card takes to fade in or out.
const CARD_FADE: Duration = Duration::from_millis(220);

/// What a key press sends the guest: a character through its US layout, or a
/// named key that keeps the modifiers held.
#[derive(Clone, Copy)]
enum ScreenKey {
    /// `shift` is the Shift the US layout needs for the character, and
    /// `composed` says the host's Alt produced the character, as with @ on a
    /// German layout, so Alt must not reach the guest around the key.
    Char { code: u16, shift: bool, composed: bool },
    Named(u16),
}

/// The key `keystroke` types. Other host layouts are translated through the
/// guest's US layout by the character they type, so a German Shift+7 sends
/// the key for "/", not for the host's physical Shift+7.
fn screen_key(keystroke: &Keystroke) -> Option<ScreenKey> {
    let character = keystroke
        .key_char
        .as_deref()
        .filter(|text| text.chars().count() == 1)
        .and_then(|text| text.chars().next());
    if !keystroke.modifiers.control && !keystroke.modifiers.platform {
        if let Some(character) = character {
            if !character.is_control() && !character.is_whitespace() {
                if let Some((code, shift)) = keymap::char_key(character) {
                    // Alt that only composes the character, as AltGr does,
                    // must stay off the key; Alt as a modifier is a shortcut.
                    // Shift is already in `key` for symbols, but not letters.
                    let composed = keystroke.modifiers.alt
                        && match keystroke.key_char.as_deref() {
                            Some(character) if keystroke.modifiers.shift => {
                                character != keystroke.key.to_ascii_uppercase().as_str()
                            }
                            Some(character) => character != keystroke.key.as_str(),
                            None => false,
                        };
                    if !keystroke.modifiers.alt || composed {
                        return Some(ScreenKey::Char { code, shift, composed });
                    }
                }
            }
        }
    }
    keymap::named_key(&keystroke.key).map(ScreenKey::Named)
}

/// The windows of a running machine, kept while another machine is shown.
struct Page {
    /// `None` once every window is closed.
    layout: Option<Node>,
    shells: BTreeMap<u32, (Entity<Shell>, FocusHandle)>,
    next_shell: u32,
    _subscriptions: Vec<Subscription>,
}

/// A divider being dragged.
struct Resizing {
    path: Vec<Step>,
    axis: Axis,
}

/// A machine that left the overview, still on screen for its card to fade out.
struct Departing {
    machine: Machine,
    /// Unique per departure, so a card that comes back starts a new animation.
    animation: u64,
    until: Instant,
}

pub struct Workspace {
    machines: Entity<Machines>,
    /// The overview's screen previews of the running machines.
    thumbnails: Entity<Thumbnails>,
    /// The machines shown last, to tell which appeared or left.
    shown: Vec<Machine>,
    /// When each machine appeared, so its card fades in once per appearance.
    appearances: HashMap<String, u64>,
    /// Machines whose cards are fading out after they left.
    departing: Vec<Departing>,
    /// The last animation number, which makes every fade unique.
    animations: u64,
    /// The machine shown in the main pane.
    selected: Option<String>,
    /// The selected machine's screen, while it runs.
    display: Option<Entity<Display>>,
    /// The selected machine's addresses and ports, while it runs.
    ports: Entity<Ports>,
    pages: BTreeMap<String, Page>,
    resizing: Option<Resizing>,
    /// Where each split of the shown page was laid out, by its path, for
    /// turning a dragged divider's position into a ratio.
    splits: Rc<RefCell<HashMap<Vec<Step>, Bounds<Pixels>>>>,
    sidebar_open: bool,
    /// The host's appearance, whose palette colors the window.
    palette: Palette,
    /// Whether that appearance is dark, for the guests' desktops. Shared with
    /// the sends in flight, so a change while they run reaches them too.
    dark: Rc<Cell<bool>>,
    /// The running machines whose desktops were already given the appearance.
    themed: HashSet<String>,
    new_name: Entity<InputState>,
    new_preset: Preset,
    new_persist: bool,
    /// `None` for this host.
    new_node: Option<String>,
    /// The project folder the new machine mounts, which keeps it on this host.
    new_folder: Option<PathBuf>,
    screen_focus: FocusHandle,
    /// The modifiers held over the screen, to send their presses and releases.
    modifiers: Modifiers,
    /// Where the last frame was drawn, for mapping the mouse to guest pixels.
    drawn: Rc<Cell<Bounds<Pixels>>>,
    _subscriptions: Vec<Subscription>,
}

impl Workspace {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let machines = cx.new(Machines::new);
        let thumbnails = cx.new(Thumbnails::new);
        let ports = cx.new(Ports::new);
        let new_name = cx.new(|cx| InputState::new(window, cx).placeholder("Name, or leave empty for a random one"));
        let appearance = {
            let this = cx.entity().downgrade();
            window.observe_window_appearance(move |window, cx| {
                this.update(cx, |this, cx| this.follow_appearance(window, cx)).ok();
            })
        };
        let subscriptions = vec![
            appearance,
            cx.observe_in(&machines, window, |this, _, window, cx| this.follow_machines(window, cx)),
            cx.observe_in(&thumbnails, window, |_, _, _, cx| cx.notify()),
            cx.observe_in(&ports, window, |_, _, _, cx| cx.notify()),
            cx.subscribe_in(&new_name, window, |this, _, event: &TextEvent, window, cx| {
                match event {
                    TextEvent::PressEnter { .. } => this.create(window, cx),
                    // The create button tells whether the name is taken, so it starts that machine.
                    _ => cx.notify(),
                }
            }),
        ];
        Self {
            machines,
            thumbnails,
            shown: Vec::new(),
            appearances: HashMap::new(),
            departing: Vec::new(),
            animations: 0,
            selected: None,
            display: None,
            ports,
            pages: BTreeMap::new(),
            resizing: None,
            splits: Rc::new(RefCell::new(HashMap::new())),
            sidebar_open: true,
            palette: Palette::of(window.appearance()),
            dark: Rc::new(Cell::new(ThemeMode::from(window.appearance()).is_dark())),
            themed: HashSet::new(),
            new_name,
            new_preset: PRESETS[1],
            new_persist: false,
            new_node: None,
            new_folder: None,
            screen_focus: cx.focus_handle(),
            modifiers: Modifiers::default(),
            drawn: Rc::new(Cell::new(Bounds::default())),
            _subscriptions: subscriptions,
        }
    }

    fn select(&mut self, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = name;
        self.display = None;
        self.follow_machines(window, cx);
    }

    fn page(&mut self) -> &mut Page {
        let name = self.selected.as_ref().expect("windows open in the selected machine");
        self.pages.get_mut(name).expect("a running machine has a page")
    }

    /// Opens a terminal beside `beside` along `axis`, or beside everything.
    fn open_shell(&mut self, beside: Option<(Pane, Axis)>, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.selected.clone().expect("a terminal opens in the selected machine");
        let machine = name.clone();
        let palette = self.palette;
        let shell = cx.new(move |cx| Shell::new(machine, (80, 24), palette, cx));
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        let page = self.page();
        let id = page.next_shell;
        page.next_shell += 1;
        let subscription = cx.subscribe_in(&shell, window, move |this, _, _: &ShellEvent, window, cx| {
            this.close_in(name.clone(), Pane::Shell(id), window, cx);
        });
        page.shells.insert(id, (shell, focus));
        page._subscriptions.push(subscription);
        let pane = Pane::Shell(id);
        page.layout = Some(match (page.layout.take(), beside) {
            (Some(layout), Some((beside, axis))) => layout.split(beside, pane, axis),
            (layout, _) => layout::added(layout, pane),
        });
        cx.notify();
    }

    fn open_screen(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.page();
        page.layout = Some(layout::added(page.layout.take(), Pane::Screen));
        self.follow_machines(window, cx);
    }

    fn close(&mut self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        let name = self.selected.clone().expect("panes belong to the selected machine");
        self.close_in(name, pane, window, cx);
    }

    /// Closes `pane` in machine `name`'s page, should it still have one.
    fn close_in(&mut self, name: String, pane: Pane, window: &mut Window, cx: &mut Context<Self>) {
        let Some(page) = self.pages.get_mut(&name) else {
            return;
        };
        page.layout = page.layout.take().and_then(|layout| layout.without(pane));
        if let Pane::Shell(id) = pane {
            page.shells.remove(&id);
        }
        self.follow_machines(window, cx);
    }

    /// Keeps a page for each running machine, and the selected machine's
    /// screen open while its page shows it.
    fn follow_machines(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (loaded, found, running, selected, local_running, current) = {
            let machines = self.machines.read(cx);
            let is_running = |name: &str| machines.find(name).is_some_and(|machine| machine.vm.running);
            let running: HashSet<String> = machines
                .machines
                .iter()
                .filter(|machine| machine.vm.running)
                .map(|machine| machine.name.clone())
                .collect();
            let local_running: Vec<String> = machines
                .machines
                .iter()
                .filter(|machine| machine.node.is_none() && machine.vm.running)
                .map(|machine| machine.short_name().to_owned())
                .collect();
            let found = self.selected.as_deref().is_some_and(|name| machines.find(name).is_some());
            let selected = self.selected.clone().filter(|name| is_running(name));
            (machines.loaded, found, running, selected, local_running, machines.machines.clone())
        };
        // A card fades in when its machine appears, and out when it leaves.
        if loaded {
            self.track_appearances(current, cx);
        }
        // Forwards end with their machine, whether it stopped or is gone.
        if loaded {
            // Off the UI thread, as stopping a forward waits for its listener to close.
            cx.background_spawn(async move {
                crate::serve::forward::retain(&local_running.iter().map(String::as_str).collect::<Vec<_>>());
            })
            .detach();
        }
        // Terminals end with their machine.
        self.pages.retain(|name, _| running.contains(name));
        if loaded && self.selected.is_some() && !found {
            self.selected = None;
        }
        // Previews refresh only while the overview shows them.
        match self.selected.is_none() {
            true => {
                let previewed = running.iter().cloned().collect::<Vec<_>>();
                let fading: Vec<String> =
                    self.departing.iter().map(|departing| departing.machine.name.clone()).collect();
                self.thumbnails.update(cx, |thumbnails, cx| thumbnails.follow(&previewed, &fading, cx));
            }
            false => self.thumbnails.update(cx, |thumbnails, _| thumbnails.pause()),
        }
        // The guest is asked for its ports only while the sidebar shows them,
        // so hiding it does not keep the machine from suspending when idle.
        let ports_machine = match self.sidebar_open {
            true => selected.clone(),
            false => None,
        };
        if let Some(name) = &selected {
            self.pages.entry(name.clone()).or_insert_with(|| Page {
                layout: Some(Node::Leaf(Pane::Screen)),
                shells: BTreeMap::new(),
                next_shell: 1,
                _subscriptions: Vec::new(),
            });
        }
        let shows_screen = selected.as_ref().is_some_and(|name| {
            self.pages[name].layout.as_ref().is_some_and(|layout| layout.panes().contains(&Pane::Screen))
        });
        match (shows_screen, &self.display) {
            (true, None) => {
                let name = selected.expect("a shown screen has a machine");
                self.display = Some(cx.new(|cx| Display::new(name, cx)));
                self.modifiers = Modifiers::default();
                self.screen_focus.focus(window, cx);
            }
            (false, Some(_)) => self.display = None,
            _ => {}
        }
        self.ports.update(cx, |ports, cx| ports.follow(ports_machine, cx));
        self.show_appearances(cx);
        cx.notify();
    }

    fn next_animation(&mut self) -> u64 {
        self.animations += 1;
        self.animations
    }

    /// Notes which machines appeared and left since the listing before, so a
    /// card fades in once and a card that left can fade out before it goes.
    fn track_appearances(&mut self, current: Vec<Machine>, cx: &mut Context<Self>) {
        let now = Instant::now();
        for machine in &current {
            if self.shown.iter().any(|old| old.name == machine.name) {
                continue;
            }
            let animation = self.next_animation();
            self.appearances.insert(machine.name.clone(), animation);
            // A machine that came back while its card faded starts afresh.
            self.departing.retain(|departing| departing.machine.name != machine.name);
        }
        let leaving: Vec<Machine> = self
            .shown
            .iter()
            .filter(|old| !current.iter().any(|machine| machine.name == old.name))
            .filter(|old| !self.departing.iter().any(|departing| departing.machine.name == old.name))
            .cloned()
            .collect();
        let mut departed = false;
        for machine in leaving {
            let animation = self.next_animation();
            self.departing.push(Departing { machine, animation, until: now + CARD_FADE });
            departed = true;
        }
        if departed {
            cx.spawn(async move |this, cx| {
                cx.background_executor().timer(CARD_FADE + Duration::from_millis(32)).await;
                this.update(cx, |this, cx| {
                    let before = this.departing.len();
                    this.departing.retain(|departing| Instant::now() < departing.until);
                    if this.departing.len() != before {
                        cx.notify();
                    }
                })
                .ok();
            })
            .detach();
        }
        self.departing.retain(|departing| now < departing.until);
        self.shown = current;
    }

    /// Moves the dragged divider to `position`.
    fn resize(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(resizing) = &self.resizing else {
            return;
        };
        let bounds = self.splits.borrow()[&resizing.path];
        let ratio = match resizing.axis {
            Axis::Row => (position.x - bounds.origin.x) / bounds.size.width,
            Axis::Column => (position.y - bounds.origin.y) / bounds.size.height,
        };
        let path = resizing.path.clone();
        self.page().layout.as_mut().expect("a dragged divider has a layout").resized(&path, ratio);
        cx.notify();
    }

    fn create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let folder = self.new_folder.take();
        let new = NewMachine {
            name: self.new_name.read(cx).value().trim().to_owned(),
            node: if folder.is_some() { None } else { self.new_node.clone() },
            folder,
            preset: self.new_preset,
            persist: self.new_persist,
        };
        self.new_name.update(cx, |input, cx| input.set_value("", window, cx));
        self.machines.update(cx, |machines, cx| machines.create(new, cx));
    }

    /// Asks for a project folder for the new machine. A project's machine is
    /// kept after stop, so it comes back with the folder.
    fn choose_folder(&mut self, cx: &mut Context<Self>) {
        let chosen = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Open".into()),
        });
        cx.spawn(async move |this, cx| {
            let chosen = chosen.await.expect("the folder prompt answers");
            this.update(cx, |this, cx| match chosen {
                Ok(Some(mut paths)) => {
                    this.new_folder = Some(paths.pop().expect("a chosen folder"));
                    this.new_persist = true;
                    cx.notify();
                }
                Ok(None) => {}
                Err(error) => this.machines.update(cx, |machines, cx| machines.fail(format!("Choosing a folder failed: {error}"), cx)),
            })
            .ok();
        })
        .detach();
    }

    /// Asks before deleting machine `name`, since its disk goes with it.
    fn confirm_delete(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let machines = self.machines.clone();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let (machines, name) = (machines.clone(), name.clone());
            alert
                .confirm()
                .title(SharedString::from(format!("Delete {name}?")))
                .description("The machine stops and its disk is deleted. This cannot be undone.")
                .ok_text("Delete")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, _, cx| {
                    machines.update(cx, |machines, cx| machines.delete(&name, cx));
                    true
                })
        });
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        self.follow_machines(window, cx);
    }

    /// Repaints the window, its terminals and the guests' desktops in the
    /// host's new appearance.
    fn follow_appearance(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let palette = Palette::of(window.appearance());
        self.palette = palette;
        self.dark.set(ThemeMode::from(window.appearance()).is_dark());
        for page in self.pages.values() {
            for (shell, _) in page.shells.values() {
                shell.update(cx, |shell, cx| shell.set_palette(palette, cx));
            }
        }
        self.themed.clear();
        self.show_appearances(cx);
        theme::apply(window.appearance(), cx);
        cx.notify();
    }

    /// Sends the host's appearance to each machine's desktop that does not
    /// show it yet, such as one that just started.
    fn show_appearances(&mut self, cx: &mut Context<Self>) {
        let dark = self.dark.clone();
        let running: Vec<String> = {
            let machines = self.machines.read(cx);
            machines
                .machines
                .iter()
                .filter(|machine| machine.vm.suspended == Some(false))
                .map(|machine| machine.name.clone())
                .collect()
        };
        self.themed.retain(|name| running.contains(name));
        for name in running {
            if !self.themed.insert(name.clone()) {
                continue;
            }
            let dark = dark.clone();
            cx.spawn(async move |_, cx| appearance::show(name, dark, cx).await).detach();
        }
    }

    fn send(&self, event: InputEvent, cx: &mut Context<Self>) {
        if let Some(display) = &self.display {
            display.read(cx).send(event);
        }
    }

    /// Sends a key press or release. A character puts Shift and Alt into the
    /// state its US key needs, and back again, since a host layout may need
    /// Shift where the guest's does not.
    fn press_key(&self, key: ScreenKey, down: bool, cx: &mut Context<Self>) {
        let (code, shift, composed) = match key {
            ScreenKey::Named(code) => return self.send(InputEvent::Key { code, down }, cx),
            ScreenKey::Char { code, shift, composed } => (code, shift, composed),
        };
        if !down {
            return self.send(InputEvent::Key { code, down: false }, cx);
        }

        let held = self.modifiers;
        let alt = composed && held.alt;
        let release_shift = held.shift && !shift;
        let press_shift = !held.shift && shift;
        let mut events = Vec::with_capacity(5);
        if alt {
            events.push(InputEvent::Key { code: KEY_LEFTALT, down: false });
        }
        if release_shift {
            events.push(InputEvent::Key { code: KEY_LEFTSHIFT, down: false });
        }
        if press_shift {
            events.push(InputEvent::Key { code: KEY_LEFTSHIFT, down: true });
        }
        events.push(InputEvent::Key { code, down: true });
        if press_shift {
            events.push(InputEvent::Key { code: KEY_LEFTSHIFT, down: false });
        }
        if release_shift {
            events.push(InputEvent::Key { code: KEY_LEFTSHIFT, down: true });
        }
        if alt {
            events.push(InputEvent::Key { code: KEY_LEFTALT, down: true });
        }
        for event in events {
            self.send(event, cx);
        }
    }

    fn mouse_position(&self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) -> Option<InputEvent> {
        let display = self.display.as_ref()?.read(cx);
        (display.screen.width > 0).then(|| display::guest_position(position, self.drawn.get(), display.screen))
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        div()
            .id("workspace")
            .key_context("Workspace")
            .on_action(cx.listener(Self::toggle_sidebar))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| this.resize(event.position, cx)))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.resizing.take().is_some() {
                        cx.notify();
                    }
                }),
            )
            .when(self.resizing.is_some(), |workspace| {
                workspace.cursor(match self.resizing.as_ref().map(|resizing| resizing.axis) {
                    Some(Axis::Row) => gpui::CursorStyle::ResizeLeftRight,
                    _ => gpui::CursorStyle::ResizeUpDown,
                })
            })
            .size_full()
            .flex()
            .bg(rgb(palette.window))
            .text_color(rgb(palette.text))
            .text_size(TEXT_SIZE)
            .p(GAP)
            .gap(GAP)
            .when(self.sidebar_open, |workspace| workspace.child(self.render_sidebar(cx)))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(self.render_toolbar(cx))
                    .child(match self.selected.clone() {
                        Some(name) => self.render_machine(&name, window, cx),
                        None => self.render_overview(cx),
                    }),
            )
            .children(self.machines.read(cx).notice.as_ref().map(|notice| render_notice(notice, &self.machines, palette)))
    }
}

// The sidebar.
impl Workspace {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let machines = self.machines.read(cx);
        let loaded = machines.loaded;
        let shown = machines.machines.clone();
        let running: Vec<AnyElement> = shown
            .iter()
            .filter(|machine| machine.vm.running)
            .map(|machine| self.render_machine_row(machine, cx))
            .collect();
        let stopped: Vec<AnyElement> = shown
            .iter()
            .filter(|machine| !machine.vm.running)
            .map(|machine| self.render_machine_row(machine, cx))
            .collect();
        // The selected machine's size, and the ports of it while it runs, at
        // the foot of the sidebar.
        let shown_machine = self
            .selected
            .as_deref()
            .and_then(|name| shown.iter().find(|machine| machine.name == name))
            .cloned();
        let ports = shown_machine.as_ref().is_some_and(|machine| machine.vm.running);

        div()
            .w(SIDEBAR_WIDTH)
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(palette.panel))
            .border_1()
            .border_color(rgb(palette.border))
            .rounded(PANEL_RADIUS)
            .px_2()
            .child(drag_area().h(TITLEBAR_HEIGHT).flex().justify_end().items_center().child(icon_button(
                "hide-sidebar",
                IconName::PanelLeft,
                cx.listener(|this, _, window, cx| this.toggle_sidebar(&ToggleSidebar, window, cx)),
                palette,
            )))
            .child(
                div()
                    .pt_3()
                    .pb_1()
                    .flex()
                    .flex_col()
                    .gap_0p5()
                    .child(nav_row(
                        "overview",
                        IconName::LayoutDashboard,
                        "Overview",
                        self.selected.is_none(),
                        cx.listener(|this, _, window, cx| this.select(None, window, cx)),
                        palette,
                    )),
            )
            .child(
                div()
                    .id("machine-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .pb_3()
                    .when(!running.is_empty(), |list| list.child(section_label("Running", palette)).children(running))
                    .when(!stopped.is_empty(), |list| list.child(section_label("Stopped", palette)).children(stopped))
                    .when(shown.is_empty() && loaded, |list| {
                        list.child(div().px_2p5().pt_3().text_color(rgb(palette.faint)).child("No machines yet"))
                    }),
            )
            .when(ports, |sidebar| sidebar.child(self.render_ports(cx)))
            .children(shown_machine.as_ref().map(|machine| self.render_facts(machine)))
    }

    /// The selected machine's size, below its ports at the foot of the sidebar.
    fn render_facts(&self, machine: &Machine) -> AnyElement {
        let palette = self.palette;
        let hardware = &machine.vm.hardware;
        let memory = match hardware.memory_mib % 1024 {
            0 => format!("{} GiB", hardware.memory_mib / 1024),
            _ => format!("{:.1} GiB", hardware.memory_mib as f64 / 1024.),
        };
        let kind = match hardware.persist {
            true => "Persistent",
            false => "Ephemeral",
        };
        let facts = [
            ("Name", machine.name.clone()),
            ("Kind", kind.to_owned()),
            ("CPU", format!("{} cores", hardware.cpus)),
            ("Memory", memory),
            ("Disk", format!("{} GiB", hardware.disk_gib)),
            ("Display", format!("{}×{}", hardware.width, hardware.height)),
        ];
        div()
            .flex_none()
            .flex()
            .flex_col()
            .gap_0p5()
            .border_t_1()
            .border_color(rgb(palette.divider))
            .pt_2()
            .pb_3()
            .children(facts.into_iter().map(|(term, value)| {
                div()
                    .px_2p5()
                    .flex()
                    .items_center()
                    .justify_between()
                    .gap_2()
                    .text_size(SMALL_TEXT_SIZE)
                    .child(div().text_color(rgb(palette.faint)).child(term))
                    .child(div().text_color(rgb(palette.muted)).truncate().child(value))
            }))
            .into_any_element()
    }

    fn render_machine_row(&self, machine: &Machine, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        let name = machine.name.clone();
        let selected = self.selected.as_deref() == Some(name.as_str());
        div()
            .id(SharedString::from(format!("machine-{name}")))
            .h(ROW_HEIGHT)
            .px_2p5()
            .flex()
            .items_center()
            .gap_2p5()
            .rounded(ROW_RADIUS)
            .cursor_pointer()
            .when(selected, |row| row.bg(rgb(palette.selected)))
            .when(!selected, |row| row.hover(|row| row.bg(rgb(palette.hover))))
            .child(status_dot(machine, palette))
            .child(div().flex_1().min_w_0().truncate().child(machine.short_name().to_owned()))
            .children(machine.node.clone().map(|node| div().text_color(rgb(palette.faint)).child(node)))
            .on_click(cx.listener(move |this, _, window, cx| this.select(Some(name.clone()), window, cx)))
            .into_any_element()
    }

    /// The strip above the main pane: drags the window, brings back the
    /// sidebar and holds the selected machine's actions.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let palette = self.palette;
        let machines = self.machines.read(cx);
        let machine = self.selected.as_deref().and_then(|name| machines.find(name)).cloned();
        let busy = machine.as_ref().is_some_and(|machine| machines.pending.contains(&machine.name));
        let screen_closed = machine.as_ref().and_then(|machine| self.pages.get(&machine.name)).is_some_and(|page| {
            !page.layout.as_ref().is_some_and(|layout| layout.panes().contains(&Pane::Screen))
        });
        drag_area()
            .h(TITLEBAR_HEIGHT)
            .mb(GAP)
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            // Room for the traffic lights when the sidebar no longer holds them.
            .when(!self.sidebar_open, |bar| {
                bar.pl(px(74.)).child(icon_button(
                    "show-sidebar",
                    IconName::PanelLeft,
                    cx.listener(|this, _, window, cx| this.toggle_sidebar(&ToggleSidebar, window, cx)),
                    palette,
                ))
            })
            .when(self.sidebar_open, |bar| bar.pl_3())
            .when_some(machine, |bar, machine| {
                let (start, stop, delete) = (machine.name.clone(), machine.name.clone(), machine.name.clone());
                let folders = machine.folders();
                bar.child(div().font_weight(gpui::FontWeight::MEDIUM).child(machine.name.clone()))
                    .child(div().text_color(rgb(palette.muted)).child(state_label(&machine)))
                    .when(!folders.is_empty(), |bar| bar.child(folder_label(folders, palette)))
                    .child(div().flex_1())
                    .when(machine.vm.running, |bar| {
                        bar.when(screen_closed, |bar| {
                            bar.child(pill("open-screen", "Screen", false, Some(IconName::Frame), palette).on_click(
                                cx.listener(|this, _, window, cx| this.open_screen(window, cx)),
                            ))
                        })
                        .child(pill("open-shell", "Terminal", false, Some(IconName::Plus), palette).on_click(
                            cx.listener(|this, _, window, cx| this.open_shell(None, window, cx)),
                        ))
                        .child(div().w(px(1.)).h(px(16.)).mx_1().bg(rgb(palette.border)))
                    })
                    .child(match machine.vm.running {
                        true => pill("stop", "Stop", busy, Some(IconName::Square), palette).on_click(cx.listener(
                            move |this, _, _, cx| this.machines.update(cx, |machines, cx| machines.stop(&stop, cx)),
                        )),
                        false => pill("start", "Start", busy, Some(IconName::Play), palette).on_click(cx.listener(
                            move |this, _, _, cx| this.machines.update(cx, |machines, cx| machines.start(&start, cx)),
                        )),
                    })
                    .child(
                        pill("delete", "Delete", busy, Some(IconName::Delete), palette)
                            .text_color(rgb(palette.danger))
                            .on_click(cx.listener(move |this, _, window, cx| this.confirm_delete(delete.clone(), window, cx))),
                    )
            })
    }
}

// The overview.
impl Workspace {
    fn render_overview(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        let listed = self.machines.read(cx).machines.clone();
        let mut entries: Vec<(&Machine, Option<u64>)> = listed.iter().map(|machine| (machine, None)).collect();
        entries.extend(self.departing.iter().map(|departing| (&departing.machine, Some(departing.animation))));
        let mut grouped: Vec<(Option<&str>, Vec<(&Machine, Option<u64>)>)> = Vec::new();
        for entry in entries {
            let node = entry.0.node.as_deref();
            match grouped.iter_mut().find(|(group, _)| *group == node) {
                Some((_, machines)) => machines.push(entry),
                None => grouped.push((node, vec![entry])),
            }
        }
        for (_, machines) in &mut grouped {
            machines.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        }
        let empty = grouped.is_empty();
        let groups: Vec<AnyElement> =
            grouped.into_iter().map(|(node, machines)| self.render_overview_group(node, &machines, cx)).collect();
        let composer = self.render_composer(cx);
        let machines = self.machines.read(cx);
        div()
            .id("overview")
            .flex_1()
            .overflow_y_scroll()
            .px(px(20.))
            .pb(px(20.))
            .child(div().w_full().max_w(px(900.)).mx_auto().flex().flex_col().gap(px(20.)).child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2p5()
                    .child(heading("Machines", palette))
                    .child(match empty {
                        true => card(palette)
                            .p_8()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_2()
                            .child(Icon::new(IconName::Frame).size(px(28.)).text_color(rgb(palette.faint)))
                            .child(div().font_weight(gpui::FontWeight::MEDIUM).child(match machines.loaded {
                                true => "No machines",
                                false => "Looking for machines…",
                            }))
                            .child(div().text_color(rgb(palette.muted)).text_center().child(
                                "Each machine is a disposable Debian desktop that starts in about a second.",
                            ))
                            .into_any_element(),
                        false => div().flex().flex_col().gap_5().children(groups).into_any_element(),
                    }),
            )
            .child(div().flex().flex_col().gap_2p5().child(heading("New machine", palette)).child(composer))
            .when(!machines.hosts.is_empty(), |overview| {
                overview.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2p5()
                        .child(heading("Hosts", palette))
                        .child(card(palette).children(machines.hosts.iter().enumerate().map(|(index, usage)| {
                            let used = usage.host.used_bytes as f64 / usage.host.memory_bytes as f64;
                            row((index, machines.hosts.len()), palette)
                                .child(
                                    Icon::new(match usage.node {
                                        Some(_) => IconName::Network,
                                        None => IconName::Cpu,
                                    })
                                    .size(ICON_SIZE)
                                    .text_color(rgb(palette.muted)),
                                )
                                .child(div().flex_1().child(usage.node.clone().unwrap_or_else(|| "This computer".to_owned())))
                                .child(div().text_size(SMALL_TEXT_SIZE).text_color(rgb(palette.muted)).child(format!(
                                    "{} CPUs · {} of {} memory",
                                    usage.host.cpus,
                                    gib(usage.host.used_bytes),
                                    gib(usage.host.memory_bytes)
                                )))
                                .child(meter(used, palette))
                        }))),
                )
            }))
            .into_any_element()
    }

    /// The machines of one host under its name, joined by a faint rail so the
    /// tree of this computer and its nodes stays visible but quiet.
    fn render_overview_group(
        &self,
        node: Option<&str>,
        machines: &[(&Machine, Option<u64>)],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let palette = self.palette;
        let icon = match node {
            Some(_) => IconName::Network,
            None => IconName::Cpu,
        };
        let cards: Vec<AnyElement> = machines
            .iter()
            .map(|(machine, leaving)| {
                let card = self.render_overview_card(machine, cx);
                match leaving {
                    Some(animation) => card
                        .with_animation(
                            SharedString::from(format!("leaving-{}-{animation}", machine.name)),
                            Animation::new(CARD_FADE).with_easing(ease_out_quint()),
                            |card, delta| card.opacity(1.0 - delta),
                        )
                        .into_any_element(),
                    None => {
                        let appearance = self.appearances.get(&machine.name).copied().unwrap_or_default();
                        card.with_animation(
                            SharedString::from(format!("entering-{}-{appearance}", machine.name)),
                            Animation::new(CARD_FADE).with_easing(ease_out_quint()),
                            |card, delta| card.opacity(delta),
                        )
                        .into_any_element()
                    }
                }
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1p5()
                    .child(Icon::new(icon).size(px(12.)).text_color(rgb(palette.faint)))
                    .child(
                        div()
                            .text_size(SMALL_TEXT_SIZE)
                            .text_color(rgb(palette.muted))
                            .child(node.unwrap_or("This computer").to_owned()),
                    ),
            )
            .child(
                div()
                    .ml(px(6.))
                    .border_l_1()
                    .border_color(rgb(palette.divider))
                    .pl(px(12.))
                    .pt(px(8.))
                    .flex()
                    .flex_wrap()
                    .gap_4()
                    .children(cards),
            )
            .into_any_element()
    }

    /// One machine of the overview: its latest screen over its name, folder and
    /// size, which opens the machine when clicked.
    fn render_overview_card(&self, machine: &Machine, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let palette = self.palette;
        let name = machine.name.clone();
        let hardware = &machine.vm.hardware;
        let folders = machine.folders();
        let preview = self.thumbnails.read(cx).frames.get(&machine.name).cloned();
        // The guest's own shape, so its screen fits the preview instead of
        // overflowing it when it is not 16:9.
        let ratio = match &preview {
            Some(frame) => {
                let size = frame.size(0);
                size.width.0 as f32 / size.height.0 as f32
            }
            None => hardware.width as f32 / hardware.height as f32,
        };
        let specs = format!(
            "{} CPUs · {} GiB · {} GiB disk{}",
            hardware.cpus,
            hardware.memory_mib / 1024,
            hardware.disk_gib,
            if hardware.persist { " · kept" } else { "" }
        );
        div()
            .id(SharedString::from(format!("overview-{name}")))
            .w(px(280.))
            .flex_none()
            .min_w(px(0.))
            .flex()
            .flex_col()
            .rounded(CARD_RADIUS)
            .overflow_hidden()
            .border_1()
            .border_color(rgb(palette.border))
            .bg(rgb(palette.card))
            .cursor_pointer()
            .hover(|card| card.border_color(rgb(palette.faint)).bg(rgb(palette.hover)))
            .child(
                div()
                    .relative()
                    .w_full()
                    .aspect_ratio(ratio)
                    .overflow_hidden()
                    .border_b_1()
                    .border_color(rgb(palette.border))
                    .bg(rgb(palette.window))
                    .child(match preview {
                        Some(frame) => img(frame).size_full().object_fit(ObjectFit::Cover).into_any_element(),
                        None => div()
                            .size_full()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(Icon::new(IconName::Frame).size(px(28.)).text_color(rgb(palette.faint)))
                            .into_any_element(),
                    })
                    .child(div().absolute().top(px(8.)).left(px(8.)).child(state_chip(machine, palette))),
            )
            .child(
                div()
                    .px_3()
                    .pt_2p5()
                    .pb_3()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(machine.short_name().to_owned()),
                    )
                    .when(!folders.is_empty(), |body| {
                        body.child(
                            div()
                                .flex()
                                .items_center()
                                .gap_1()
                                .text_size(SMALL_TEXT_SIZE)
                                .text_color(rgb(palette.muted))
                                .child(Icon::new(IconName::Folder).size(px(11.)).flex_none().text_color(rgb(palette.faint)))
                                .child(div().flex_1().min_w_0().truncate().child(folders)),
                        )
                    })
                    .child(div().text_size(SMALL_TEXT_SIZE).text_color(rgb(palette.faint)).truncate().child(specs)),
            )
            .on_click(cx.listener(move |this, _, window, cx| this.select(Some(name.clone()), window, cx)))
    }

    /// The new machine form: the name and project folder over a strip with
    /// its size and host on the left and keeping it and creating it on the right.
    fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        let machines = self.machines.read(cx);
        let creating = machines.pending.contains("");
        let folder = self.new_folder.clone();
        let typed = self.new_name.read(cx).value().trim().to_owned();
        let starts = Machines::new_name(&typed, folder.as_deref()).is_some_and(|name| machines.find(&name).is_some());
        // A folder lives on this host, so its machine does too.
        let nodes = if folder.is_some() { Vec::new() } else { machines.nodes.clone() };
        let hosts = std::iter::once(None).chain(nodes.into_iter().map(Some)).collect::<Vec<_>>();
        card(palette)
            .border_1()
            .border_color(rgb(palette.border))
            .child(
                div()
                    .bg(rgb(palette.window))
                    .pl_3()
                    .pr_2()
                    .min_h(px(56.))
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(Input::new(&self.new_name).appearance(false)))
                    .child(match &folder {
                        None => quiet_button("choose-folder", "Open folder", IconName::FolderOpen, palette)
                            .on_click(cx.listener(|this, _, _, cx| this.choose_folder(cx)))
                            .into_any_element(),
                        Some(folder) => folder_chip(
                            folder,
                            cx.listener(|this, _, _, cx| {
                                this.new_folder = None;
                                cx.notify();
                            }),
                            palette,
                        ),
                    }),
            )
            .child(
                div()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(hosts.len() > 1, |strip| strip.child(self.render_host_select(hosts, cx)))
                    .child(segmented(
                        PRESETS.iter().map(|preset| {
                            let preset = *preset;
                            let specs = format!("{} CPUs · {} GiB memory", preset.cpus, preset.memory_mib / 1024);
                            segment(
                                SharedString::from(format!("preset-{}", preset.label)),
                                preset.label,
                                self.new_preset == preset,
                                palette,
                            )
                            .tooltip(move |window, cx| Tooltip::new(specs.clone()).build(window, cx))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.new_preset = preset;
                                cx.notify();
                            }))
                        }),
                        palette,
                    ))
                    .child(div().flex_1())
                    .child(
                        choice("persist", "Keep after stop", self.new_persist, palette)
                            .flex_none()
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.new_persist = !this.new_persist;
                                cx.notify();
                            })),
                    )
                    .child(
                        div()
                            .id("create")
                            .flex_none()
                            .h(CONTROL_HEIGHT)
                            .text_size(SMALL_TEXT_SIZE)
                            .px_3()
                            .flex()
                            .items_center()
                            .gap_1p5()
                            .rounded_full()
                            .bg(rgb(palette.text))
                            .text_color(rgb(palette.window))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .when(creating, |button| button.opacity(0.5))
                            .when(!creating, |button| {
                                button.cursor_pointer().on_click(cx.listener(|this, _, window, cx| this.create(window, cx)))
                            })
                            .child(Icon::new(if starts { IconName::Play } else { IconName::Plus }).size(ICON_SIZE))
                            .child(match (creating, starts) {
                                (true, _) => "Creating…",
                                (false, true) => "Start",
                                (false, false) => "Create",
                            }),
                    ),
            )
            .into_any_element()
    }

    /// The host a new machine runs on, picked from a dropdown.
    fn render_host_select(&self, hosts: Vec<Option<String>>, cx: &mut Context<Self>) -> AnyElement {
        let workspace = cx.entity().downgrade();
        let selected = self.new_node.clone();
        Button::new("host")
            .outline()
            .small()
            .rounded(ButtonRounded::Size(CONTROL_HEIGHT))
            .h(CONTROL_HEIGHT)
            .icon(Icon::new(IconName::Cpu).size(ICON_SIZE))
            .label(host_label(&selected))
            .dropdown_caret(true)
            .dropdown_menu(move |mut menu, _, _| {
                for node in &hosts {
                    let workspace = workspace.clone();
                    let node = node.clone();
                    menu = menu.item(PopupMenuItem::new(host_label(&node)).checked(selected == node).on_click(
                        move |_, _, cx| {
                            workspace
                                .update(cx, |this, cx| {
                                    this.new_node = node.clone();
                                    cx.notify();
                                })
                                .expect("the workspace outlives its menus");
                        },
                    ));
                }
                menu
            })
            .into_any_element()
    }
}

fn host_label(node: &Option<String>) -> String {
    node.clone().unwrap_or_else(|| "Local".to_owned())
}

// The machine page.
impl Workspace {
    fn render_machine(&self, name: &str, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        let machines = self.machines.read(cx);
        let Some(machine) = machines.find(name) else {
            return div().flex_1().into_any_element();
        };
        if let Some(page) = self.pages.get(name) {
            self.splits.borrow_mut().clear();
            return match page.layout.clone() {
                Some(layout) => div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .child(self.render_node(&layout, Vec::new(), window, cx))
                    .into_any_element(),
                None => div()
                    .flex_1()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(palette.muted))
                    .child("Open the screen or a terminal from the bar above.")
                    .into_any_element(),
            };
        }
        if machine.vm.running {
            return div().flex_1().into_any_element();
        }
        {
            let start = machine.name.clone();
            let busy = machines.pending.contains(name);
            return div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .child(
                    card(palette)
                        .p_6()
                        .w(px(360.))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(div().font_weight(gpui::FontWeight::MEDIUM).child("This machine is stopped"))
                        .child(div().text_color(rgb(palette.muted)).child("Its disk is kept until you delete it."))
                        .children(machine.vm.mounts.iter().map(|mount| {
                            div()
                                .text_size(SMALL_TEXT_SIZE)
                                .flex()
                                .items_center()
                                .gap_1p5()
                                .child(Icon::new(IconName::Folder).size(ICON_SIZE).text_color(rgb(palette.muted)))
                                .child(div().truncate().child(format!(
                                    "{} at {}",
                                    machines::home_relative(&mount.host),
                                    mount.guest.replacen("/home/varmint", "~", 1)
                                )))
                        }))
                        .child(div().flex().child(
                            pill("start-stopped", "Start", busy, Some(IconName::Play), palette).on_click(cx.listener(
                                move |this, _, _, cx| this.machines.update(cx, |machines, cx| machines.start(&start, cx)),
                            )),
                        )),
                )
                .into_any_element();
        }
    }

    /// The addresses and listening ports of the selected machine at the foot
    /// of the sidebar, with a forward on this computer's loopback for each
    /// port of a local one.
    fn render_ports(&self, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        let ports = self.ports.read(cx);
        let local = ports.local();
        let pending = ports.pending.clone();
        let (network, error) = (ports.network.as_ref(), ports.error.clone());
        let address = network.map(|network| match network.addresses.as_slice() {
            [] => "No IP address yet".to_owned(),
            addresses => addresses.join(", "),
        });
        let rows: Vec<AnyElement> = network
            .map(|network| {
                network
                    .ports
                    .iter()
                    .enumerate()
                    .map(|(index, port)| {
                        let number = port.port;
                        let forwarded = port.forwarded;
                        let busy = pending.contains(&number);
                        let secondary = match forwarded {
                            Some(local_port) if local => format!("localhost:{local_port}"),
                            Some(_) => "forwarded on the node".to_owned(),
                            None if port.loopback => "localhost only".to_owned(),
                            None => match network.addresses.first() {
                                Some(address) => format!("{address}:{number}"),
                                None => "no IP address yet".to_owned(),
                            },
                        };
                        let open = match forwarded {
                            Some(local_port) if local => Some(format!("http://localhost:{local_port}")),
                            None if !port.loopback && number != 22 => {
                                network.addresses.first().map(|address| format!("http://{address}:{number}"))
                            }
                            _ => None,
                        };
                        let cell = div()
                            .id(SharedString::from(format!("port-{number}")))
                            .min_w_0()
                            .flex_1()
                            .flex()
                            .flex_col()
                            .gap_0p5()
                            .child(div().font_weight(gpui::FontWeight::MEDIUM).child(number.to_string()))
                            .child(div().text_size(px(11.)).text_color(rgb(palette.muted)).truncate().child(secondary));
                        let mut row = div()
                            .px_2p5()
                            .py_1()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(index > 0, |row| row.border_t_1().border_color(rgb(palette.divider)))
                            .child(match open {
                                Some(url) => cell
                                    .cursor_pointer()
                                    .hover(|cell| cell.bg(rgb(palette.hover)))
                                    .on_click(cx.listener(move |_, _, _, cx| cx.open_url(&url)))
                                    .into_any_element(),
                                None => cell.into_any_element(),
                            });
                        if local {
                            let (label, icon) = match forwarded {
                                Some(_) => ("Stop", IconName::Square),
                                None => ("Forward", IconName::ExternalLink),
                            };
                            row = row.child(
                                port_action(SharedString::from(format!("forward-{number}")), label, icon, palette)
                                    .when(busy, |button| button.opacity(0.5))
                                    .when(!busy, |button| {
                                        button.on_click(cx.listener(move |this, _, _, cx| {
                                            this.ports.update(cx, |ports, cx| ports.toggle(number, cx));
                                        }))
                                    }),
                            );
                        }
                        row.into_any_element()
                    })
                    .collect()
            })
            .unwrap_or_default();
        div()
            .id("sidebar-ports")
            .flex_shrink_1()
            .min_h_0()
            .max_h(px(300.))
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .border_t_1()
            .border_color(rgb(palette.divider))
            .pt_2()
            .pb_2()
            .child(div().px_2p5().pb_1().text_size(LABEL_SIZE).text_color(rgb(palette.faint)).child("Ports"))
            .children(address.map(|address| {
                div().px_2p5().text_size(px(11.)).text_color(rgb(palette.muted)).truncate().child(address)
            }))
            .child(
                div()
                    .children(rows)
                    .when(network.is_some_and(|network| network.ports.is_empty()), |list| {
                        list.child(
                            div()
                                .px_2p5()
                                .pt_1()
                                .flex()
                                .flex_col()
                                .gap_0p5()
                                .child(
                                    div()
                                        .text_size(SMALL_TEXT_SIZE)
                                        .text_color(rgb(palette.muted))
                                        .child("No listening ports"),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(rgb(palette.faint))
                                        .child("Servers started inside the machine appear here."),
                                ),
                        )
                    })
                    .when(network.is_none() && error.is_none(), |list| {
                        list.child(
                            div()
                                .px_2p5()
                                .pt_1()
                                .text_size(SMALL_TEXT_SIZE)
                                .text_color(rgb(palette.muted))
                                .child("Looking for ports…"),
                        )
                    })
                    .when(network.is_some() && !local, |list| {
                        list.child(div().px_2p5().pt_1().text_size(px(11.)).text_color(rgb(palette.faint)).child(
                            "Forwarding needs the machine on this computer; use vm ssh -L for a node.",
                        ))
                    })
                    .when_some(error, |list, error| {
                        list.child(div().px_2p5().pt_1().text_size(px(11.)).text_color(rgb(palette.faint)).child(error))
                    }),
            )
            .into_any_element()
    }

    /// The windows of `node`, a split at `path` down from the page's root.
    fn render_node(&self, node: &Node, path: Vec<Step>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let (axis, ratio, first, second) = match node {
            Node::Leaf(pane) => return self.render_pane(*pane, window, cx),
            Node::Split { axis, ratio, first, second } => (*axis, *ratio, first, second),
        };
        let step = |step| [path.clone(), vec![step]].concat();
        let first = self.render_node(first, step(Step::First), window, cx);
        let second = self.render_node(second, step(Step::Second), window, cx);
        let (splits, laid_out) = (self.splits.clone(), path.clone());
        let divider = div()
            .id(SharedString::from(format!("divider-{path:?}")))
            .flex_none()
            .map(|divider| match axis {
                Axis::Row => divider.w(GAP).h_full().cursor(gpui::CursorStyle::ResizeLeftRight),
                Axis::Column => divider.h(GAP).w_full().cursor(gpui::CursorStyle::ResizeUpDown),
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    this.resizing = Some(Resizing { path: path.clone(), axis });
                    cx.stop_propagation();
                    cx.notify();
                }),
            );
        let along = |element: gpui::Div, share: Option<f32>| match (axis, share) {
            (Axis::Row, Some(share)) => element.flex_none().w(gpui::relative(share)).h_full(),
            (Axis::Column, Some(share)) => element.flex_none().h(gpui::relative(share)).w_full(),
            (Axis::Row, None) => element.flex_1().min_w_0().h_full(),
            (Axis::Column, None) => element.flex_1().min_h_0().w_full(),
        };
        div()
            .size_full()
            .relative()
            .flex()
            .when(axis == Axis::Column, |split| split.flex_col())
            .child(
                canvas(move |bounds, _, _| { splits.borrow_mut().insert(laid_out, bounds); }, |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .child(along(div().flex(), Some(ratio)).child(first))
            .child(divider)
            .child(along(div().flex(), None).child(second))
            .into_any_element()
    }

    /// A window with a bar naming it and splitting or closing it.
    fn render_pane(&self, pane: Pane, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let palette = self.palette;
        let page = &self.pages[self.selected.as_ref().expect("panes belong to the selected machine")];
        let (title, focus, content) = match pane {
            Pane::Screen => {
                let content = match self.display.clone() {
                    Some(display) => self.render_screen(display, window, cx),
                    None => div().into_any_element(),
                };
                ("Screen".to_owned(), self.screen_focus.clone(), content)
            }
            Pane::Shell(id) => {
                let (shell, focus) = page.shells[&id].clone();
                let ended = shell.read(cx).ended.is_some();
                let title = format!("Terminal {id}{}", if ended { " · ended" } else { "" });
                let on_drop = cx.listener(move |this, paths: &ExternalPaths, _, cx| {
                    this.drop_files(pane, paths.paths().to_vec(), cx);
                });
                (title, focus.clone(), shell_view::render(shell, &focus, palette, on_drop, window, cx))
            }
        };
        let focused = focus.contains_focused(window, cx);
        let key = match pane {
            Pane::Screen => "screen".to_owned(),
            Pane::Shell(id) => format!("shell-{id}"),
        };
        let button = |action: &str, icon: IconName, on_click: Box<dyn Fn(&mut Self, &mut Window, &mut Context<Self>)>| {
            icon_button(
                SharedString::from(format!("{action}-{key}")),
                icon,
                cx.listener(move |this, _, window, cx| on_click(this, window, cx)),
                palette,
            )
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .rounded(CARD_RADIUS)
            .border_1()
            .border_color(rgb(if focused { palette.faint } else { palette.border }))
            .drag_over::<ExternalPaths>(move |style, _, _, _| style.border_color(rgb(palette.text)))
            .bg(rgb(palette.card))
            .overflow_hidden()
            .child(
                div()
                    .h(px(28.))
                    .flex_none()
                    .pl_3()
                    .pr_1()
                    .flex()
                    .items_center()
                    .gap_0p5()
                    .text_size(SMALL_TEXT_SIZE)
                    .text_color(rgb(if focused { palette.text } else { palette.muted }))
                    .child(div().flex_1().truncate().child(title))
                    .child(button(
                        "split-right",
                        IconName::PanelRight,
                        Box::new(move |this, window, cx| this.open_shell(Some((pane, Axis::Row)), window, cx)),
                    ))
                    .child(button(
                        "split-down",
                        IconName::PanelBottom,
                        Box::new(move |this, window, cx| this.open_shell(Some((pane, Axis::Column)), window, cx)),
                    ))
                    .child(button("close", IconName::Close, Box::new(move |this, window, cx| this.close(pane, window, cx)))),
            )
            .child(div().flex_1().min_h_0().flex().child(content))
            .into_any_element()
    }

    fn render_screen(&self, display: Entity<Display>, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let state = display.read(cx);
        let (frame, screen, error) = (state.frame.clone(), state.screen, state.error.clone());
        let drawn = self.drawn.clone();
        let scale = window.scale_factor();
        let fit_display = display.clone();
        div()
            .id("screen")
            .key_context("Pane")
            .track_focus(&self.screen_focus)
            .size_full()
            .bg(gpui::black())
            .overflow_hidden()
            .relative()
            .cursor(state.cursor)
            .on_drop::<ExternalPaths>(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.drop_files(Pane::Screen, paths.paths().to_vec(), cx);
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                // Ctrl+V first puts the host clipboard into the guest's, so it pastes that.
                if event.keystroke.modifiers.control && event.keystroke.key == "v" && !event.is_held {
                    if let Some(display) = &this.display {
                        display.read(cx).paste();
                    }
                }
                if let Some(key) = screen_key(&event.keystroke) {
                    this.press_key(key, true, cx);
                }
                cx.stop_propagation();
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, cx| {
                if let Some(key) = screen_key(&event.keystroke) {
                    this.press_key(key, false, cx);
                }
            }))
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, cx| {
                let (before, after) = (this.modifiers, event.modifiers);
                for (code, was, is) in [
                    (KEY_LEFTCTRL, before.control, after.control),
                    (KEY_LEFTSHIFT, before.shift, after.shift),
                    (KEY_LEFTALT, before.alt, after.alt),
                    // Cmd stays on the Mac, as Hyprland in the guest uses Alt instead.
                    (KEY_LEFTMETA, before.platform && !cfg!(target_os = "macos"), after.platform && !cfg!(target_os = "macos")),
                ] {
                    if was != is {
                        this.send(InputEvent::Key { code, down: is }, cx);
                    }
                }
                this.modifiers = after;
            }))
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _, cx| {
                if let Some(event) = this.mouse_position(event.position, cx) {
                    this.send(event, cx);
                }
            }))
            .on_scroll(cx.listener(|this, event: &ScrollEvent, _, cx| {
                let (x, y) = match event.delta {
                    ScrollDelta::Lines(lines) => (lines.x, lines.y),
                    ScrollDelta::Pixels(pixels) => (f32::from(pixels.x) / 32., f32::from(pixels.y) / 32.),
                };
                for (value, horizontal) in [(y, false), (x, true)] {
                    let value = value.round() as i32;
                    if value != 0 {
                        this.send(InputEvent::Scroll { value, horizontal }, cx);
                    }
                }
            }))
            .on_mouse_down(MouseButton::Left, cx.listener(|this, event: &MouseDownEvent, window, cx| {
                this.screen_focus.focus(window, cx);
                this.press(BTN_LEFT, true, event.position, cx);
            }))
            .on_mouse_up(MouseButton::Left, cx.listener(|this, event: &MouseUpEvent, _, cx| this.press(BTN_LEFT, false, event.position, cx)))
            .on_mouse_down(MouseButton::Right, cx.listener(|this, event: &MouseDownEvent, _, cx| this.press(BTN_RIGHT, true, event.position, cx)))
            .on_mouse_up(MouseButton::Right, cx.listener(|this, event: &MouseUpEvent, _, cx| this.press(BTN_RIGHT, false, event.position, cx)))
            .on_mouse_down(MouseButton::Middle, cx.listener(|this, event: &MouseDownEvent, _, cx| this.press(BTN_MIDDLE, true, event.position, cx)))
            .on_mouse_up(MouseButton::Middle, cx.listener(|this, event: &MouseUpEvent, _, cx| this.press(BTN_MIDDLE, false, event.position, cx)))
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let pane = size((f32::from(bounds.size.width) * scale) as u32, (f32::from(bounds.size.height) * scale) as u32);
                        fit_display.update(cx, |display, cx| display.fit_to(pane, cx));
                        let target = display::fit(bounds, screen);
                        drawn.set(target);
                        target
                    },
                    move |_, target, window, _| {
                        if let Some(frame) = frame {
                            window
                                .paint_image(target, target, Default::default(), frame, 0, false)
                                .expect("painting a screen frame");
                        }
                    },
                )
                .size_full(),
            )
            .when(state.frame.is_none() || error.is_some(), |pane| {
                pane.child(
                    div()
                        .absolute()
                        .inset_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        // The screen pane stays black in both appearances.
                        .text_color(rgb(DARK.muted))
                        .child(error.unwrap_or_else(|| "Connecting to the screen…".to_owned())),
                )
            })
            .into_any_element()
    }

    fn press(&mut self, button: u16, down: bool, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        if let Some(event) = self.mouse_position(position, cx) {
            self.send(event, cx);
        }
        self.send(InputEvent::Button { button, down }, cx);
        cx.stop_propagation();
    }

    /// Uploads files dropped on `pane` into the guest, then puts their guest
    /// paths where they were dropped.
    fn drop_files(&mut self, pane: Pane, paths: Vec<PathBuf>, cx: &mut Context<Self>) {
        let Some(machine) = self.selected.clone() else {
            return;
        };
        if paths.is_empty() {
            return;
        }
        cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async move { upload::upload(&machine, &paths) }).await;
            this.update(cx, |this, cx| match result {
                Ok(paths) => this.place_paths(pane, &paths, cx),
                Err(error) => this.machines.update(cx, |machines, cx| machines.fail(error, cx)),
            })
            .ok();
        })
        .detach();
    }

    /// Puts the guest paths of a drop into `pane`: typed into a terminal, or
    /// pasted into the program the screen's keyboard goes to.
    fn place_paths(&mut self, pane: Pane, paths: &[String], cx: &mut Context<Self>) {
        let text = paths.iter().map(|path| upload::shell_quote(path)).collect::<Vec<_>>().join(" ");
        match pane {
            Pane::Shell(id) => {
                let Some(name) = self.selected.as_ref() else {
                    return;
                };
                let Some((shell, _)) = self.pages.get(name).and_then(|page| page.shells.get(&id)) else {
                    return;
                };
                shell.update(cx, |shell, cx| shell.paste(&format!("{text} "), cx));
            }
            Pane::Screen => {
                if let Some(display) = &self.display {
                    display.read(cx).paste_text(text);
                }
            }
        }
    }
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.screen_focus.clone()
    }
}

fn render_notice(notice: &Notice, machines: &Entity<Machines>, palette: Palette) -> impl IntoElement {
    let machines = machines.clone();
    let color: Hsla = match notice.tone {
        Tone::Done => rgb(palette.text).into(),
        Tone::Error => rgb(palette.danger).into(),
    };
    div()
        .absolute()
        .bottom(px(24.))
        .right(px(24.))
        .max_w(px(420.))
        .child(
            div()
                .id("notice")
                .px_3()
                .py_2()
                .flex()
                .items_center()
                .gap_3()
                .rounded(CARD_RADIUS)
                .bg(rgb(palette.card))
                .border_1()
                .border_color(rgb(palette.border))
                .cursor_pointer()
                .text_color(color)
                .child(div().flex_1().child(notice.text.clone()))
                .child(Icon::new(IconName::Close).size(px(12.)).text_color(rgb(palette.muted)))
                .on_click(move |_, _, cx| machines.update(cx, |machines, cx| machines.dismiss_notice(cx))),
        )
}

/// Drags the window, as the titlebar would.
fn drag_area() -> gpui::Div {
    div().on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
}

fn heading(text: &'static str, palette: Palette) -> impl IntoElement {
    div().pt_1().text_size(HEADING_SIZE).font_weight(gpui::FontWeight::MEDIUM).text_color(rgb(palette.muted)).child(text)
}

fn section_label(text: &'static str, palette: Palette) -> impl IntoElement {
    div().px_2p5().pt_3().pb_1().text_size(LABEL_SIZE).text_color(rgb(palette.faint)).child(text)
}

fn card(palette: Palette) -> gpui::Div {
    div().bg(rgb(palette.card)).rounded(CARD_RADIUS).overflow_hidden().flex().flex_col()
}

/// The row at `index` of a card of `count` rows: all but the first get a
/// divider above, and the outer ones round with the card, since cards clip
/// their rows only to their rectangle.
fn row((index, count): (usize, usize), palette: Palette) -> gpui::Div {
    div()
        .px_4()
        .py_2p5()
        .flex()
        .items_center()
        .gap_3()
        .when(index > 0, |row| row.border_t_1().border_color(rgb(palette.divider)))
        .when(index == 0, |row| row.rounded_t(CARD_RADIUS))
        .when(index + 1 == count, |row| row.rounded_b(CARD_RADIUS))
}

fn nav_row(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    selected: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    palette: Palette,
) -> impl IntoElement {
    div()
        .id(id)
        .h(ROW_HEIGHT)
        .px_2p5()
        .flex()
        .items_center()
        .gap_2p5()
        .rounded(ROW_RADIUS)
        .cursor_pointer()
        .when(selected, |row| row.bg(rgb(palette.selected)))
        .when(!selected, |row| row.hover(|row| row.bg(rgb(palette.hover))))
        .child(Icon::new(icon).size(ICON_SIZE).text_color(rgb(palette.text)))
        .child(label)
        .on_click(on_click)
}

fn icon_button(
    id: impl Into<gpui::ElementId>,
    icon: IconName,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    palette: Palette,
) -> impl IntoElement {
    div()
        .id(id)
        .size(px(24.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|button| button.bg(rgb(palette.hover)))
        .child(Icon::new(icon).size(ICON_SIZE).text_color(rgb(palette.muted)))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click)
}

/// A bordered, fully rounded button.
fn pill(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    busy: bool,
    icon: Option<IconName>,
    palette: Palette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(CONTROL_HEIGHT)
        .px_3()
        .flex()
        .items_center()
        .gap_1p5()
        .rounded_full()
        .text_size(SMALL_TEXT_SIZE)
        .border_1()
        .border_color(rgb(palette.border))
        .when(busy, |pill| pill.opacity(0.5))
        .when(!busy, |pill| pill.cursor_pointer().hover(|pill| pill.bg(rgb(palette.hover))))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .children(icon.map(|icon| Icon::new(icon).size(px(12.))))
        .child(label.into())
}

/// Options side by side in one rounded track, the selected one raised.
fn segmented(segments: impl IntoIterator<Item = gpui::Stateful<gpui::Div>>, palette: Palette) -> impl IntoElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .p(px(2.))
        .gap(px(2.))
        .rounded_full()
        .border_1()
        .border_color(rgb(palette.border))
        .children(segments)
}

fn segment(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    palette: Palette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(CONTROL_HEIGHT - px(6.))
        .px_2p5()
        .flex()
        .items_center()
        .rounded_full()
        .text_size(SMALL_TEXT_SIZE)
        .cursor_pointer()
        .when(selected, |segment| segment.bg(rgb(palette.selected)))
        .when(!selected, |segment| segment.text_color(rgb(palette.muted)).hover(|segment| segment.bg(rgb(palette.hover))))
        .child(label.into())
}

/// A pill that is one of several choices.
fn choice(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    selected: bool,
    palette: Palette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(CONTROL_HEIGHT)
        .px_3()
        .flex()
        .items_center()
        .rounded_full()
        .text_size(SMALL_TEXT_SIZE)
        .border_1()
        .cursor_pointer()
        .when(selected, |pill| pill.bg(rgb(palette.selected)).border_color(rgb(palette.selected)))
        .when(!selected, |pill| {
            pill.border_color(rgb(palette.border)).text_color(rgb(palette.muted)).hover(|pill| pill.bg(rgb(palette.hover)))
        })
        .child(label.into())
}

/// The folders a machine mounts, beside its name.
fn folder_label(folders: String, palette: Palette) -> impl IntoElement {
    div()
        .min_w_0()
        .flex()
        .items_center()
        .gap_1()
        .text_size(SMALL_TEXT_SIZE)
        .text_color(rgb(palette.faint))
        .child(Icon::new(IconName::Folder).size(px(12.)))
        .child(div().truncate().child(folders))
}

/// A borderless button with an icon, for a secondary action inside a field.
fn quiet_button(
    id: impl Into<gpui::ElementId>,
    label: impl Into<SharedString>,
    icon: IconName,
    palette: Palette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .h(CONTROL_HEIGHT)
        .px_2p5()
        .flex()
        .items_center()
        .gap_1p5()
        .rounded_full()
        .text_size(SMALL_TEXT_SIZE)
        .text_color(rgb(palette.muted))
        .cursor_pointer()
        .hover(|button| button.bg(rgb(palette.hover)).text_color(rgb(palette.text)))
        .child(Icon::new(icon).size(px(12.)))
        .child(label.into())
}

/// A small bordered button of a port row, so the action reads as one without
/// waiting for the hover.
fn port_action(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    icon: IconName,
    palette: Palette,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex_none()
        .h(px(22.))
        .px_2()
        .flex()
        .items_center()
        .gap_1()
        .rounded_full()
        .border_1()
        .border_color(rgb(palette.border))
        .text_size(px(11.))
        .text_color(rgb(palette.muted))
        .cursor_pointer()
        .hover(|button| button.bg(rgb(palette.hover)).text_color(rgb(palette.text)))
        .child(Icon::new(icon).size(px(11.)))
        .child(label)
}

/// The chosen project folder of a new machine, with a button that clears it.
fn folder_chip(
    folder: &std::path::Path,
    on_clear: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
    palette: Palette,
) -> AnyElement {
    div()
        .flex_none()
        .max_w(px(280.))
        .h(CONTROL_HEIGHT)
        .pl_3()
        .pr_1()
        .flex()
        .items_center()
        .gap_1p5()
        .rounded_full()
        .text_size(SMALL_TEXT_SIZE)
        .bg(rgb(palette.selected))
        .child(Icon::new(IconName::Folder).size(px(12.)).flex_none())
        .child(div().min_w_0().truncate().child(machines::home_relative(folder)))
        .child(icon_button("clear-folder", IconName::Close, on_clear, palette))
        .into_any_element()
}

/// A machine's state over its screen, dark enough to read over any guest.
fn state_chip(machine: &Machine, palette: Palette) -> impl IntoElement {
    div()
        .flex()
        .items_center()
        .gap_1p5()
        .h(px(20.))
        .px_2()
        .rounded_full()
        .border_1()
        .border_color(rgba(0xffffff1a))
        .bg(rgba(0x0b0b0bcc))
        .text_size(LABEL_SIZE)
        .text_color(rgb(0xf5f5f4))
        .child(status_dot(machine, palette))
        .child(state_label(machine))
        .with_animation(
            SharedString::from(format!("state-{}-{}", machine.name, state_label(machine))),
            Animation::new(CARD_FADE).with_easing(ease_out_quint()),
            |chip, delta| chip.opacity(delta),
        )
}

fn status_dot(machine: &Machine, palette: Palette) -> impl IntoElement {
    let color = match machine.vm.suspended {
        Some(true) => rgb(palette.suspended),
        Some(false) => rgb(palette.running),
        None => rgb(palette.faint),
    };
    div().size(px(6.)).flex_none().rounded_full().bg(color)
}

fn state_label(machine: &Machine) -> &'static str {
    match machine.vm.suspended {
        Some(true) => "Suspended",
        Some(false) => "Running",
        None => "Stopped",
    }
}

/// How full something is, from 0 to 1.
fn meter(fraction: f64, palette: Palette) -> impl IntoElement {
    div()
        .w(px(96.))
        .h(px(6.))
        .rounded_full()
        .bg(rgb(palette.selected))
        .child(div().h_full().rounded_full().bg(rgb(palette.muted)).w(px(96. * fraction.clamp(0., 1.) as f32)))
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

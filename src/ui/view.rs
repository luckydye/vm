//! The window of `vm ui`: a floating sidebar with the machines, and a main
//! pane with the overview or the selected machine's screen and terminals,
//! tiled side by side and stacked. Views render the state of [`Machines`],
//! [`Display`], [`Shell`] and the [`layout`] and forward what the user does.

use super::{
    FocusSearch, ToggleSidebar,
    display::{self, Display},
    layout::{self, Axis, Node, Pane, Step},
    machines::{Machine, Machines, NewMachine, Notice, PRESETS, Preset, Tone},
    shell::{Shell, ShellEvent},
    shell_view,
    theme::*,
};
use crate::{
    keymap,
    protocol::InputEvent,
    virtio::input::keys::{BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, KEY_LEFTALT, KEY_LEFTCTRL, KEY_LEFTMETA, KEY_LEFTSHIFT},
};
use gpui::{
    AnyElement, Bounds, Context, Entity, FocusHandle, Focusable, Hsla, KeyDownEvent, KeyUpEvent, Modifiers,
    ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollDelta,
    ScrollEvent, SharedString, Subscription, Window, canvas, div, prelude::*, px, rgb, size,
};
use gpui_component::{
    Icon, Sizable,
    button::Button,
    input::{Input, InputEvent as TextEvent, InputState},
    menu::{DropdownMenu, PopupMenuItem},
};
use gpui_kit_assets::IconName;
use std::{
    cell::{Cell, RefCell},
    collections::{BTreeMap, HashMap},
    rc::Rc,
};

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

pub struct Workspace {
    machines: Entity<Machines>,
    /// The machine shown in the main pane.
    selected: Option<String>,
    /// The selected machine's screen, while it runs.
    display: Option<Entity<Display>>,
    pages: BTreeMap<String, Page>,
    resizing: Option<Resizing>,
    /// Where each split of the shown page was laid out, by its path, for
    /// turning a dragged divider's position into a ratio.
    splits: Rc<RefCell<HashMap<Vec<Step>, Bounds<Pixels>>>>,
    sidebar_open: bool,
    search: Entity<InputState>,
    new_name: Entity<InputState>,
    new_preset: Preset,
    new_persist: bool,
    /// `None` for this host.
    new_node: Option<String>,
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
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Quick Search"));
        let new_name = cx.new(|cx| InputState::new(window, cx).placeholder("Name, or leave empty for a random one"));
        let subscriptions = vec![
            cx.observe_in(&machines, window, |this, _, window, cx| this.follow_machines(window, cx)),
            cx.subscribe(&search, |_, _, _: &TextEvent, cx| cx.notify()),
            cx.subscribe_in(&new_name, window, |this, _, event: &TextEvent, window, cx| {
                if matches!(event, TextEvent::PressEnter { .. }) {
                    this.create(window, cx);
                }
            }),
        ];
        Self {
            machines,
            selected: None,
            display: None,
            pages: BTreeMap::new(),
            resizing: None,
            splits: Rc::new(RefCell::new(HashMap::new())),
            sidebar_open: true,
            search,
            new_name,
            new_preset: PRESETS[1],
            new_persist: false,
            new_node: None,
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
        let shell = cx.new(move |cx| Shell::new(machine, (80, 24), cx));
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
        let machines = self.machines.read(cx);
        let running = |name: &str| machines.find(name).is_some_and(|machine| machine.vm.running);
        // Terminals end with their machine.
        self.pages.retain(|name, _| running(name));
        if machines.loaded && self.selected.as_deref().is_some_and(|name| machines.find(name).is_none()) {
            self.selected = None;
        }
        let selected = self.selected.clone().filter(|name| running(name));
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
        cx.notify();
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
        let new = NewMachine {
            name: self.new_name.read(cx).value().trim().to_owned(),
            node: self.new_node.clone(),
            preset: self.new_preset,
            persist: self.new_persist,
        };
        self.new_name.update(cx, |input, cx| input.set_value("", window, cx));
        self.machines.update(cx, |machines, cx| machines.create(new, cx));
    }

    fn focus_search(&mut self, _: &FocusSearch, window: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = true;
        self.search.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn toggle_sidebar(&mut self, _: &ToggleSidebar, _: &mut Window, cx: &mut Context<Self>) {
        self.sidebar_open = !self.sidebar_open;
        cx.notify();
    }

    fn send(&self, event: InputEvent, cx: &mut Context<Self>) {
        if let Some(display) = &self.display {
            display.read(cx).send(event);
        }
    }

    fn mouse_position(&self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) -> Option<InputEvent> {
        let display = self.display.as_ref()?.read(cx);
        (display.screen.width > 0).then(|| display::guest_position(position, self.drawn.get(), display.screen))
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("workspace")
            .key_context("Workspace")
            .on_action(cx.listener(Self::focus_search))
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
            .bg(rgb(WINDOW))
            .text_color(rgb(TEXT))
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
            .children(self.machines.read(cx).notice.as_ref().map(|notice| render_notice(notice, &self.machines)))
    }
}

// The sidebar.
impl Workspace {
    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let machines = self.machines.read(cx);
        let loaded = machines.loaded;
        let query = self.search.read(cx).value().to_lowercase();
        let shown: Vec<Machine> =
            machines.machines.iter().filter(|machine| machine.name.to_lowercase().contains(&query)).cloned().collect();
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

        div()
            .w(SIDEBAR_WIDTH)
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded(PANEL_RADIUS)
            .px_2()
            .child(drag_area().h(TITLEBAR_HEIGHT).flex().justify_end().items_center().child(icon_button(
                "hide-sidebar",
                IconName::PanelLeft,
                cx.listener(|this, _, window, cx| this.toggle_sidebar(&ToggleSidebar, window, cx)),
            )))
            .child(
                div()
                    .h(CONTROL_HEIGHT)
                    .mt_2()
                    .px_2p5()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(ROW_RADIUS)
                    .border_1()
                    .border_color(rgb(BORDER))
                    .child(Icon::new(IconName::Search).size(ICON_SIZE).text_color(rgb(MUTED)))
                    .child(div().flex_1().child(Input::new(&self.search).appearance(false).small()))
                    .child(div().text_size(SMALL_TEXT_SIZE).text_color(rgb(FAINT)).child(if cfg!(target_os = "macos") { "⌘K" } else { "Ctrl K" })),
            )
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
                    )),
            )
            .child(
                div()
                    .id("machine-list")
                    .flex_1()
                    .overflow_y_scroll()
                    .pb_3()
                    .when(!running.is_empty(), |list| list.child(section_label("Running")).children(running))
                    .when(!stopped.is_empty(), |list| list.child(section_label("Stopped")).children(stopped))
                    .when(shown.is_empty() && loaded, |list| {
                        list.child(div().px_2p5().pt_3().text_color(rgb(FAINT)).child(match query.is_empty() {
                            true => "No machines yet",
                            false => "No match",
                        }))
                    }),
            )
    }

    fn render_machine_row(&self, machine: &Machine, cx: &mut Context<Self>) -> AnyElement {
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
            .when(selected, |row| row.bg(rgb(SELECTED)))
            .when(!selected, |row| row.hover(|row| row.bg(rgb(HOVER))))
            .child(status_dot(machine))
            .child(div().flex_1().min_w_0().truncate().child(machine.short_name().to_owned()))
            .children(machine.node.clone().map(|node| div().text_color(rgb(FAINT)).child(node)))
            .on_click(cx.listener(move |this, _, window, cx| this.select(Some(name.clone()), window, cx)))
            .into_any_element()
    }

    /// The strip above the main pane: drags the window, brings back the
    /// sidebar and holds the selected machine's actions.
    fn render_toolbar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let machines = self.machines.read(cx);
        let machine = self.selected.as_deref().and_then(|name| machines.find(name)).cloned();
        let busy = machine.as_ref().is_some_and(|machine| machines.pending.contains(&machine.name));
        let screen_closed = machine.as_ref().and_then(|machine| self.pages.get(&machine.name)).is_some_and(|page| {
            !page.layout.as_ref().is_some_and(|layout| layout.panes().contains(&Pane::Screen))
        });
        drag_area()
            .h(TITLEBAR_HEIGHT)
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            // Room for the traffic lights when the sidebar no longer holds them.
            .when(!self.sidebar_open, |bar| {
                bar.pl(px(80.)).child(icon_button(
                    "show-sidebar",
                    IconName::PanelLeft,
                    cx.listener(|this, _, window, cx| this.toggle_sidebar(&ToggleSidebar, window, cx)),
                ))
            })
            .when_some(machine, |bar, machine| {
                let (start, stop, delete) = (machine.name.clone(), machine.name.clone(), machine.name.clone());
                bar.pl_3()
                    .child(div().font_weight(gpui::FontWeight::MEDIUM).child(machine.name.clone()))
                    .child(div().text_color(rgb(MUTED)).child(state_label(&machine)))
                    .child(div().flex_1())
                    .when(machine.vm.running, |bar| {
                        bar.when(screen_closed, |bar| {
                            bar.child(pill("open-screen", "Screen", false, Some(IconName::Frame)).on_click(
                                cx.listener(|this, _, window, cx| this.open_screen(window, cx)),
                            ))
                        })
                        .child(pill("open-shell", "Terminal", false, Some(IconName::Plus)).on_click(
                            cx.listener(|this, _, window, cx| this.open_shell(None, window, cx)),
                        ))
                        .child(div().w(px(1.)).h(px(16.)).mx_1().bg(rgb(BORDER)))
                    })
                    .child(match machine.vm.running {
                        true => pill("stop", "Stop", busy, Some(IconName::Square)).on_click(cx.listener(move |this, _, _, cx| {
                            this.machines.update(cx, |machines, cx| machines.stop(&stop, cx))
                        })),
                        false => pill("start", "Start", busy, Some(IconName::Play)).on_click(cx.listener(move |this, _, _, cx| {
                            this.machines.update(cx, |machines, cx| machines.start(&start, cx))
                        })),
                    })
                    .child(pill("delete", "Delete", busy, Some(IconName::Delete)).text_color(rgb(DANGER)).on_click(
                        cx.listener(move |this, _, _, cx| this.machines.update(cx, |machines, cx| machines.delete(&delete, cx))),
                    ))
            })
    }
}

// The overview.
impl Workspace {
    fn render_overview(&self, cx: &mut Context<Self>) -> AnyElement {
        let listed = self.machines.read(cx).machines.clone();
        let rows: Vec<AnyElement> =
            listed.iter().enumerate().map(|(index, machine)| self.render_overview_row(machine, (index, listed.len()), cx)).collect();
        let composer = self.render_composer(cx);
        let machines = self.machines.read(cx);
        let empty = rows.is_empty();
        div()
            .id("overview")
            .flex_1()
            .overflow_y_scroll()
            .px(px(20.))
            .pb(px(20.))
            .child(div().max_w(px(880.)).mx_auto().flex().flex_col().gap(px(20.)).child(
                div()
                    .flex()
                    .flex_col()
                    .gap_2p5()
                    .child(heading("Machines"))
                    .child(match empty {
                        true => card()
                            .p_6()
                            .flex()
                            .flex_col()
                            .gap_1()
                            .child(div().font_weight(gpui::FontWeight::MEDIUM).child(match machines.loaded {
                                true => "No machines",
                                false => "Looking for machines…",
                            }))
                            .child(div().text_color(rgb(MUTED)).child(
                                "Each machine is a disposable Debian desktop that starts in about a second.",
                            ))
                            .into_any_element(),
                        false => card().children(rows).into_any_element(),
                    }),
            )
            .child(div().flex().flex_col().gap_2p5().child(heading("New machine")).child(composer))
            .when(!machines.hosts.is_empty(), |overview| {
                overview.child(
                    div()
                        .flex()
                        .flex_col()
                        .gap_2p5()
                        .child(heading("Hosts"))
                        .child(card().children(machines.hosts.iter().enumerate().map(|(index, usage)| {
                            let used = usage.host.used_bytes as f64 / usage.host.memory_bytes as f64;
                            row((index, machines.hosts.len()))
                                .child(Icon::new(IconName::Cpu).size(ICON_SIZE).text_color(rgb(MUTED)))
                                .child(div().flex_1().child(usage.node.clone().unwrap_or_else(|| "This computer".to_owned())))
                                .child(div().text_size(SMALL_TEXT_SIZE).text_color(rgb(MUTED)).child(format!(
                                    "{} CPUs · {} of {} memory",
                                    usage.host.cpus,
                                    gib(usage.host.used_bytes),
                                    gib(usage.host.memory_bytes)
                                )))
                                .child(meter(used))
                        }))),
                )
            }))
            .into_any_element()
    }

    fn render_overview_row(&self, machine: &Machine, place: (usize, usize), cx: &mut Context<Self>) -> AnyElement {
        let name = machine.name.clone();
        let hardware = &machine.vm.hardware;
        row(place)
            .id(SharedString::from(format!("overview-{name}")))
            .cursor_pointer()
            .hover(|row| row.bg(rgb(HOVER)))
            .child(status_dot(machine))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(div().font_weight(gpui::FontWeight::MEDIUM).truncate().child(name.clone()))
                    .child(div().text_size(SMALL_TEXT_SIZE).text_color(rgb(MUTED)).child(format!(
                        "{} CPUs · {} GiB memory · {} GiB disk{}",
                        hardware.cpus,
                        hardware.memory_mib / 1024,
                        hardware.disk_gib,
                        if hardware.persist { " · kept after stop" } else { "" }
                    ))),
            )
            .child(div().text_size(SMALL_TEXT_SIZE).text_color(rgb(MUTED)).child(state_label(machine)))
            .on_click(cx.listener(move |this, _, window, cx| this.select(Some(name.clone()), window, cx)))
            .into_any_element()
    }

    /// The new machine form: an inset name field over a strip of choices.
    fn render_composer(&self, cx: &mut Context<Self>) -> AnyElement {
        let machines = self.machines.read(cx);
        let creating = machines.pending.contains("");
        let nodes = machines.nodes.clone();
        let selected = self.new_node.clone();
        let host = selected.clone().unwrap_or_else(|| "Local".to_owned());
        let workspace = cx.weak_entity();
        card()
            .border_1()
            .border_color(rgb(BORDER))
            .child(div().bg(rgb(WINDOW)).px_3().py_3().min_h(px(56.)).child(Input::new(&self.new_name).appearance(false)))
            .child(
                div()
                    .px_3()
                    .py_2()
                    .flex()
                    .items_center()
                    .gap_3()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_3()
                            .overflow_hidden()
                            .child(segmented(PRESETS.iter().map(|preset| {
                                let preset = *preset;
                                segment(SharedString::from(format!("preset-{}", preset.label)), preset.label, self.new_preset == preset)
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.new_preset = preset;
                                        cx.notify();
                                    }))
                            })))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(SMALL_TEXT_SIZE)
                                    .text_color(rgb(FAINT))
                                    .child(format!("{} CPU · {} GiB", self.new_preset.cpus, self.new_preset.memory_mib / 1024)),
                            )
                            .child(choice("persist", "Keep after stop", self.new_persist).flex_none().on_click(cx.listener(
                                |this, _, _, cx| {
                                    this.new_persist = !this.new_persist;
                                    cx.notify();
                                },
                            )))
                            .when(!nodes.is_empty(), move |strip| {
                                strip.child(
                                    Button::new("node-select")
                                        .small()
                                        .outline()
                                        .dropdown_caret(true)
                                        .w(px(120.))
                                        .label(host)
                                        .dropdown_menu(move |menu, _, _| {
                                            let local =
                                                PopupMenuItem::new("Local").checked(selected.is_none()).on_click({
                                                    let workspace = workspace.clone();
                                                    move |_, _, cx| {
                                                        workspace
                                                            .update(cx, |this, cx| {
                                                                this.new_node = None;
                                                                cx.notify();
                                                            })
                                                            .ok();
                                                    }
                                                });
                                            let menu = menu.item(local);
                                            nodes.iter().cloned().fold(menu, |menu, node| {
                                                let checked = selected.as_deref() == Some(node.as_str());
                                                let workspace = workspace.clone();
                                                menu.item(PopupMenuItem::new(node.clone()).checked(checked).on_click(move |_, _, cx| {
                                                    let node = node.clone();
                                                    workspace
                                                        .update(cx, |this, cx| {
                                                            this.new_node = Some(node);
                                                            cx.notify();
                                                        })
                                                        .ok();
                                                }))
                                            })
                                        }),
                                )
                            }),
                    )
                    .child(div().w(px(1.)).h(px(16.)).flex_none().bg(rgb(BORDER)))
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
                            .bg(rgb(TEXT))
                            .text_color(rgb(WINDOW))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .when(creating, |button| button.opacity(0.5))
                            .when(!creating, |button| {
                                button.cursor_pointer().on_click(cx.listener(|this, _, window, cx| this.create(window, cx)))
                            })
                            .child(Icon::new(IconName::Plus).size(ICON_SIZE))
                            .child(if creating { "Creating…" } else { "Create" }),
                    ),
            )
            .into_any_element()
    }
}

// The machine page.
impl Workspace {
    fn render_machine(&self, name: &str, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
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
                    .text_color(rgb(MUTED))
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
                    card()
                        .p_6()
                        .w(px(360.))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .child(div().font_weight(gpui::FontWeight::MEDIUM).child("This machine is stopped"))
                        .child(div().text_color(rgb(MUTED)).child("Its disk is kept until you delete it."))
                        .child(div().flex().child(pill("start-stopped", "Start", busy, Some(IconName::Play)).on_click(
                            cx.listener(move |this, _, _, cx| this.machines.update(cx, |machines, cx| machines.start(&start, cx))),
                        ))),
                )
                .into_any_element();
        }
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
                (title, focus.clone(), shell_view::render(shell, &focus, window, cx))
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
            )
        };
        div()
            .size_full()
            .flex()
            .flex_col()
            .rounded(CARD_RADIUS)
            .border_1()
            .border_color(rgb(if focused { FAINT } else { BORDER }))
            .bg(rgb(CARD))
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
                    .text_color(rgb(if focused { TEXT } else { MUTED }))
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
            .cursor(gpui::CursorStyle::Arrow)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                // Ctrl+V first puts the host clipboard into the guest's, so it pastes that.
                if event.keystroke.modifiers.control && event.keystroke.key == "v" && !event.is_held {
                    if let Some(display) = &this.display {
                        display.read(cx).paste();
                    }
                }
                if let Some(code) = keymap::named_key(&event.keystroke.key) {
                    this.send(InputEvent::Key { code, down: true }, cx);
                }
                cx.stop_propagation();
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, cx| {
                if let Some(code) = keymap::named_key(&event.keystroke.key) {
                    this.send(InputEvent::Key { code, down: false }, cx);
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
                        .text_color(rgb(MUTED))
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
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.screen_focus.clone()
    }
}

fn render_notice(notice: &Notice, machines: &Entity<Machines>) -> impl IntoElement {
    let machines = machines.clone();
    let color: Hsla = match notice.tone {
        Tone::Done => rgb(TEXT).into(),
        Tone::Error => rgb(DANGER).into(),
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
                .items_start()
                .gap_3()
                .rounded(CARD_RADIUS)
                .bg(rgb(CARD))
                .border_1()
                .border_color(rgb(BORDER))
                .cursor_pointer()
                .text_color(color)
                .child(div().flex_1().child(notice.text.clone()))
                .child(Icon::new(IconName::Close).size(px(12.)).text_color(rgb(MUTED)))
                .on_click(move |_, _, cx| machines.update(cx, |machines, cx| machines.dismiss_notice(cx))),
        )
}

/// Drags the window, as the titlebar would.
fn drag_area() -> gpui::Div {
    div().on_mouse_down(MouseButton::Left, |_, window, _| window.start_window_move())
}

fn heading(text: &'static str) -> impl IntoElement {
    div().pt_1().text_size(HEADING_SIZE).font_weight(gpui::FontWeight::MEDIUM).text_color(rgb(MUTED)).child(text)
}

fn section_label(text: &'static str) -> impl IntoElement {
    div().px_2p5().pt_3().pb_1().text_size(LABEL_SIZE).text_color(rgb(FAINT)).child(text)
}

fn card() -> gpui::Div {
    div().bg(rgb(CARD)).rounded(CARD_RADIUS).overflow_hidden().flex().flex_col()
}

/// The row at `index` of a card of `count` rows: all but the first get a
/// divider above, and the outer ones round with the card, since cards clip
/// their rows only to their rectangle.
fn row((index, count): (usize, usize)) -> gpui::Div {
    div()
        .px_4()
        .py_2p5()
        .flex()
        .items_center()
        .gap_3()
        .when(index > 0, |row| row.border_t_1().border_color(rgb(DIVIDER)))
        .when(index == 0, |row| row.rounded_t(CARD_RADIUS))
        .when(index + 1 == count, |row| row.rounded_b(CARD_RADIUS))
}

fn nav_row(
    id: &'static str,
    icon: IconName,
    label: &'static str,
    selected: bool,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
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
        .when(selected, |row| row.bg(rgb(SELECTED)))
        .when(!selected, |row| row.hover(|row| row.bg(rgb(HOVER))))
        .child(Icon::new(icon).size(ICON_SIZE).text_color(rgb(TEXT)))
        .child(label)
        .on_click(on_click)
}

fn icon_button(
    id: impl Into<gpui::ElementId>,
    icon: IconName,
    on_click: impl Fn(&gpui::ClickEvent, &mut Window, &mut gpui::App) + 'static,
) -> impl IntoElement {
    div()
        .id(id)
        .size(px(24.))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.))
        .cursor_pointer()
        .hover(|button| button.bg(rgb(HOVER)))
        .child(Icon::new(icon).size(ICON_SIZE).text_color(rgb(MUTED)))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(on_click)
}

/// A bordered, fully rounded button.
fn pill(id: &'static str, label: &'static str, busy: bool, icon: Option<IconName>) -> gpui::Stateful<gpui::Div> {
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
        .border_color(rgb(BORDER))
        .when(busy, |pill| pill.opacity(0.5))
        .when(!busy, |pill| pill.cursor_pointer().hover(|pill| pill.bg(rgb(HOVER))))
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .children(icon.map(|icon| Icon::new(icon).size(px(12.))))
        .child(label)
}

/// Options side by side in one rounded track, the selected one raised.
fn segmented(segments: impl IntoIterator<Item = gpui::Stateful<gpui::Div>>) -> impl IntoElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .p(px(2.))
        .gap(px(2.))
        .rounded_full()
        .border_1()
        .border_color(rgb(BORDER))
        .children(segments)
}

fn segment(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, selected: bool) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(CONTROL_HEIGHT - px(6.))
        .px_2p5()
        .flex()
        .items_center()
        .rounded_full()
        .text_size(SMALL_TEXT_SIZE)
        .cursor_pointer()
        .when(selected, |segment| segment.bg(rgb(SELECTED)))
        .when(!selected, |segment| segment.text_color(rgb(MUTED)).hover(|segment| segment.bg(rgb(HOVER))))
        .child(label.into())
}

/// A pill that is one of several choices.
fn choice(id: impl Into<gpui::ElementId>, label: impl Into<SharedString>, selected: bool) -> gpui::Stateful<gpui::Div> {
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
        .when(selected, |pill| pill.bg(rgb(SELECTED)).border_color(rgb(SELECTED)))
        .when(!selected, |pill| pill.border_color(rgb(BORDER)).text_color(rgb(MUTED)).hover(|pill| pill.bg(rgb(HOVER))))
        .child(label.into())
}

fn status_dot(machine: &Machine) -> impl IntoElement {
    let color = match machine.vm.suspended {
        Some(true) => rgb(SUSPENDED),
        Some(false) => rgb(RUNNING),
        None => rgb(FAINT),
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
fn meter(fraction: f64) -> impl IntoElement {
    div()
        .w(px(96.))
        .h(px(6.))
        .rounded_full()
        .bg(rgb(SELECTED))
        .child(div().h_full().rounded_full().bg(rgb(MUTED)).w(px(96. * fraction.clamp(0., 1.) as f32)))
}

fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

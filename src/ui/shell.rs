//! A shell in a machine: a pty session in the guest, emulated with Ghostty's
//! terminal, as plain rows of styled text for the view to draw.

use crate::{
    clipboard,
    node::{self, Target},
    protocol::{self, Frame, Request, Session, kind},
};
use gpui::{AppContext, AsyncApp, Context, EventEmitter, Keystroke, Modifiers, Task, WeakEntity};
use libghostty_vt::{
    color::Rgb,
    fmt::Formatter,
    key::{self, Key},
    mode::Mode,
    mouse,
    paste,
    point::{Coordinate, Point},
    render::{CellIterator, RenderState, RowIterator},
    selection::gesture::{DragEvent, Geometry, Gesture, PressEvent, ReleaseEvent},
    style::Underline,
    terminal::{ScrollViewport, Terminal},
};
use std::{
    collections::BTreeMap,
    io::BufReader,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const SCROLLBACK_LINES: usize = 10_000;

/// How soon a second click must follow for a double or triple click.
const CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// A run of cells with the same look, starting at `column`; `len` counts bytes
/// of the line's text.
#[derive(Clone, PartialEq)]
pub struct Run {
    pub column: u16,
    pub len: usize,
    pub foreground: u32,
    pub background: Option<u32>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

/// A block element (U+2580 to U+259F) in a cell, left out of the line's text
/// so the view can fill it exactly: the font's glyphs don't meet the cell's edges.
#[derive(Clone)]
pub struct Block {
    pub column: u16,
    pub character: char,
    pub color: u32,
}

/// One row of the screen, a character per cell.
#[derive(Clone)]
pub struct Line {
    pub text: String,
    pub runs: Vec<Run>,
    pub blocks: Vec<Block>,
}

/// What the view draws.
#[derive(Clone, Default)]
pub struct Grid {
    pub lines: Vec<Line>,
    /// Column and row of the cursor while it shows.
    pub cursor: Option<(u16, u16)>,
    pub background: u32,
    pub foreground: u32,
}

/// What the writer thread sends to the guest, in order.
enum Command {
    Input(Vec<u8>),
    Resize(u16, u16),
}

/// What subscribers of a [`Shell`] hear about.
pub enum ShellEvent {
    /// The program exited, so its pane closes.
    Exited,
}

impl EventEmitter<ShellEvent> for Shell {}

/// How a session ended.
enum End {
    /// The program exited.
    Exited,
    /// The connection failed, with what to tell the user.
    Failed(String),
}

pub struct Shell {
    pub grid: Grid,
    /// Why the shell ended, once it did.
    pub ended: Option<String>,
    /// The size in cells.
    pub size: (u16, u16),
    terminal: Terminal<'static, 'static>,
    render: RenderState<'static>,
    rows: RowIterator<'static>,
    cells: CellIterator<'static>,
    keys: key::Encoder<'static>,
    key: key::Event<'static>,
    mice: mouse::Encoder<'static>,
    mouse_event: mouse::Event<'static>,
    /// Selecting with the mouse: clicks select from a cell, a word or a line.
    gesture: Gesture<'static>,
    press: PressEvent<'static>,
    drag: DragEvent<'static>,
    release: ReleaseEvent<'static>,
    started: Instant,
    writer: mpsc::Sender<Command>,
    _read: Task<()>,
}

impl Shell {
    /// Opens a login shell in machine `name` with `size` cells.
    pub fn new(name: String, size: (u16, u16), cx: &mut Context<Self>) -> Self {
        let (writer, commands) = mpsc::channel();
        let replies = writer.clone();
        let mut terminal = Terminal::new(size.0, size.1).expect("failed to create a terminal");
        terminal
            .set_scrollback_max_lines(Some(SCROLLBACK_LINES))
            .and_then(|terminal| terminal.set_terminfo_name("xterm-256color"))
            // Set in the terminal, not just painted, so programs asking for them get them too.
            // libghostty only takes the background once the foreground is set as well.
            .and_then(|terminal| terminal.set_default_fg_color(Some(Rgb { r: 0xff, g: 0xff, b: 0xff })))
            .and_then(|terminal| terminal.set_default_bg_color(Some(Rgb { r: 0x0b, g: 0x0b, b: 0x0b })))
            .and_then(|terminal| {
                // Answers to the queries of programs like vim and tmux.
                terminal.on_pty_write(move |_, data: &[u8]| {
                    let _ = replies.send(Command::Input(data.to_vec()));
                })
            })
            .expect("failed to set up the terminal");
        let mut shell = Self {
            grid: Grid::default(),
            ended: None,
            size,
            terminal,
            render: RenderState::new().expect("failed to create a render state"),
            rows: RowIterator::new().expect("failed to create a row iterator"),
            cells: CellIterator::new().expect("failed to create a cell iterator"),
            keys: key::Encoder::new().expect("failed to create a key encoder"),
            key: key::Event::new().expect("failed to create a key event"),
            mice: mouse::Encoder::new().expect("failed to create a mouse encoder"),
            mouse_event: mouse::Event::new().expect("failed to create a mouse event"),
            gesture: Gesture::new().expect("failed to create a selection gesture"),
            press: {
                let mut press = PressEvent::new().expect("failed to create a selection event");
                press.set_repeat_interval(CLICK_INTERVAL).expect("failed to set the click interval");
                press
            },
            drag: DragEvent::new().expect("failed to create a selection event"),
            release: ReleaseEvent::new().expect("failed to create a selection event"),
            started: Instant::now(),
            writer,
            _read: cx.spawn(async move |this, cx| {
                let end = read(name, size, commands, this.clone(), cx).await;
                this.update(cx, |this, cx| match end {
                    End::Exited => cx.emit(ShellEvent::Exited),
                    End::Failed(reason) => {
                        this.ended = Some(reason);
                        cx.notify();
                    }
                })
                .ok();
            }),
        };
        shell.refresh();
        shell
    }

    /// Sends a key press to the shell, encoded for the modes its programs set.
    pub fn key_down(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) {
        let Some((code, unshifted)) = key_code(&keystroke.key) else {
            return;
        };
        let modifiers = &keystroke.modifiers;
        let mods = key_mods(modifiers);
        // Shift is already in the text of printable keys, as in 'A' for a.
        let consumed = match unshifted != '\0' && modifiers.shift {
            true => key::Mods::SHIFT,
            false => key::Mods::empty(),
        };
        let text = keystroke.key_char.clone().filter(|text| !text.chars().any(char::is_control));
        self.key
            .set_action(key::Action::Press)
            .set_key(code)
            .set_mods(mods)
            .set_consumed_mods(consumed)
            .set_unshifted_codepoint(unshifted)
            .set_utf8(text);
        let mut bytes = Vec::new();
        self.keys
            .set_options_from_terminal(&self.terminal)
            .encode_to_vec(&self.key, &mut bytes)
            .expect("failed to encode a key");
        self.input(bytes, cx);
    }

    /// Pastes `text` as the shell's programs expect it, bracketed if they ask.
    pub fn paste(&mut self, text: &str, cx: &mut Context<Self>) {
        let bracketed = self.terminal.mode(Mode::BRACKETED_PASTE).expect("failed to read the paste mode");
        let mut data = text.as_bytes().to_vec();
        let mut encoded = vec![0; data.len() * 2 + 16];
        let length = paste::encode(&mut data, bracketed, &mut encoded).expect("failed to encode a paste");
        encoded.truncate(length);
        self.input(encoded, cx);
    }

    /// Starts selecting at `position`, in pixels from the grid's top left with
    /// cells of `cell` pixels; a second or third click selects a word or line.
    pub fn select_press(&mut self, position: (f32, f32), cell: (f32, f32), cx: &mut Context<Self>) {
        let grid_ref = self.terminal.grid_ref(self.cell_at(position, cell)).expect("a cell in the grid");
        let selection = self
            .press
            .set_repeat_distance(f64::from(cell.0))
            .and_then(|press| press.set_time(self.started.elapsed()))
            .and_then(|press| press.set_position(f64::from(position.0), f64::from(position.1)))
            .and_then(|press| press.apply(&mut self.gesture, &self.terminal, grid_ref))
            .expect("failed to start a selection");
        self.terminal.set_selection(selection.as_ref()).expect("failed to select");
        self.refresh();
        cx.notify();
    }

    /// Extends the selection to `position`, as a rectangle with `rectangle`.
    pub fn select_drag(&mut self, position: (f32, f32), cell: (f32, f32), rectangle: bool, cx: &mut Context<Self>) {
        if self.gesture.click_count(&self.terminal).expect("failed to read the selection") == 0 {
            return;
        }
        let grid_ref = self.terminal.grid_ref(self.cell_at(position, cell)).expect("a cell in the grid");
        let geometry = Geometry {
            columns: self.size.0.into(),
            cell_width: cell.0 as u32,
            padding_left: 0,
            screen_height: (f32::from(self.size.1) * cell.1) as u32,
        };
        let selection = self
            .drag
            .set_rectangle(rectangle)
            .and_then(|drag| drag.set_position(f64::from(position.0), f64::from(position.1)))
            .and_then(|drag| drag.apply(&mut self.gesture, &self.terminal, grid_ref, geometry))
            .expect("failed to extend the selection");
        self.terminal.set_selection(selection.as_ref()).expect("failed to select");
        self.refresh();
        cx.notify();
    }

    /// Ends selecting; the selection stays until the next press or input.
    pub fn select_release(&mut self, position: (f32, f32), cell: (f32, f32)) {
        let grid_ref = self.terminal.grid_ref(self.cell_at(position, cell)).ok();
        self.release.apply(&mut self.gesture, &self.terminal, grid_ref).expect("failed to end the selection");
    }

    /// The selected text, if any.
    pub fn selected_text(&self) -> Option<String> {
        let mut formatter = Formatter::new_selection(&self.terminal).expect("failed to read the selection")?;
        let text = formatter.format_alloc(None).expect("failed to read the selection");
        Some(String::from_utf8_lossy(&text).into_owned()).filter(|text| !text.is_empty())
    }

    /// Reports a mouse `action` of `button` at `position` to the shell's
    /// program, when it tracks the mouse; `pressed` tells whether any button
    /// is held. Returns whether it went to the program; with Shift held it
    /// never does, so the mouse can still select, as in other terminals.
    pub fn mouse(
        &mut self,
        action: mouse::Action,
        button: Option<mouse::Button>,
        pressed: bool,
        position: (f32, f32),
        cell: (f32, f32),
        modifiers: &Modifiers,
    ) -> bool {
        if modifiers.shift || !self.terminal.is_mouse_tracking().expect("failed to read the mouse mode") {
            return false;
        }
        // In whole pixels per cell, the size the terminal was given, and at
        // the middle of the cell so rounding can't tip it into the next one.
        let (width, height) = (cell.0.ceil(), cell.1.floor());
        let Coordinate { x: column, y: row } = self.cell_under(position, cell);
        self.mouse_event
            .set_action(action)
            .set_button(button)
            .set_mods(key_mods(modifiers))
            .set_position(mouse::Position {
                x: (f32::from(column) + 0.5) * width,
                y: (row as f32 + 0.5) * height,
            });
        let size = mouse::EncoderSize {
            screen_width: u32::from(self.size.0) * width as u32,
            screen_height: u32::from(self.size.1) * height as u32,
            cell_width: width as u32,
            cell_height: height as u32,
            padding_top: 0,
            padding_bottom: 0,
            padding_right: 0,
            padding_left: 0,
        };
        let mut bytes = Vec::new();
        self.mice
            .set_options_from_terminal(&self.terminal)
            .set_any_button_pressed(pressed)
            // Motion within a cell is reported once.
            .set_track_last_cell(true)
            .set_size(size)
            .and_then(|mice| mice.encode_to_vec(&self.mouse_event, &mut bytes))
            .expect("failed to encode a mouse event");
        if !bytes.is_empty() {
            // The reader tells why, should the session have ended.
            let _ = self.writer.send(Command::Input(bytes));
        }
        true
    }

    /// The viewport cell under `position`, clamped to the grid.
    fn cell_at(&self, position: (f32, f32), cell: (f32, f32)) -> Point {
        Point::Viewport(self.cell_under(position, cell))
    }

    fn cell_under(&self, (x, y): (f32, f32), (width, height): (f32, f32)) -> Coordinate {
        let column = (x / width).floor().clamp(0., f32::from(self.size.0 - 1)) as u16;
        let row = (y / height).floor().clamp(0., f32::from(self.size.1 - 1)) as u32;
        Coordinate { x: column, y: row }
    }

    /// Scrolls the view through the scrollback by `lines`, up when negative.
    pub fn scroll(&mut self, lines: isize, cx: &mut Context<Self>) {
        self.terminal.scroll_viewport(ScrollViewport::Delta(lines));
        self.refresh();
        cx.notify();
    }

    /// Takes the size of a pane of `size` cells of `cell` pixels.
    pub fn resize(&mut self, size: (u16, u16), cell: (u32, u32), cx: &mut Context<Self>) {
        if size == self.size || size.0 == 0 || size.1 == 0 {
            return;
        }
        self.size = size;
        self.terminal.resize(size.0, size.1, cell.0, cell.1).expect("failed to resize the terminal");
        let _ = self.writer.send(Command::Resize(size.0, size.1));
        self.refresh();
        cx.notify();
    }

    /// Writes input, and shows the bottom of the screen where it lands.
    fn input(&mut self, bytes: Vec<u8>, cx: &mut Context<Self>) {
        if bytes.is_empty() {
            return;
        }
        // The reader tells why, should the session have ended.
        let _ = self.writer.send(Command::Input(bytes));
        self.terminal.set_selection(None).expect("failed to clear the selection");
        self.terminal.scroll_viewport(ScrollViewport::Bottom);
        self.refresh();
        cx.notify();
    }

    fn output(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        self.terminal.vt_write(bytes);
        self.refresh();
        cx.notify();
    }

    /// Reads the terminal's screen into the grid.
    fn refresh(&mut self) {
        let snapshot = self.render.update(&self.terminal).expect("failed to read the terminal");
        let colors = snapshot.colors().expect("failed to read the terminal colors");
        let cursor = snapshot.cursor().expect("failed to read the cursor");
        let mut grid = Grid {
            lines: Vec::new(),
            cursor: cursor.viewport.filter(|_| cursor.visible).map(|viewport| (viewport.x, viewport.y)),
            background: rgb(colors.background),
            foreground: rgb(colors.foreground),
        };
        let mut rows = self.rows.update(&snapshot).expect("failed to read the rows");
        let mut grapheme = String::new();
        while let Some(row) = rows.next() {
            let mut line = Line { text: String::new(), runs: Vec::new(), blocks: Vec::new() };
            let mut cells = self.cells.update(row).expect("failed to read the cells");
            let mut column = 0;
            while let Some(cell) = cells.next() {
                grapheme.clear();
                match cell.graphemes_len().expect("failed to read a cell") {
                    0 => grapheme.push(' '),
                    _ => cell.graphemes_utf8(&mut grapheme).expect("failed to read a cell"),
                }
                let mut foreground = cell.fg_color().expect("failed to read a cell").unwrap_or(colors.foreground);
                let mut background = cell.bg_color().expect("failed to read a cell");
                let style = cell.style().expect("failed to read a cell");
                // Selected cells show in reverse video, like inverse ones.
                if style.inverse != cell.is_selected().expect("failed to read a cell") {
                    let swapped = background.unwrap_or(colors.background);
                    background = Some(foreground);
                    foreground = swapped;
                }
                let mut characters = grapheme.chars();
                if let (Some(character @ '\u{2580}'..='\u{259F}'), None) = (characters.next(), characters.next()) {
                    line.blocks.push(Block { column, character, color: rgb(foreground) });
                    grapheme.clear();
                    grapheme.push(' ');
                }
                let run = Run {
                    column,
                    len: grapheme.len(),
                    foreground: rgb(foreground),
                    background: background.map(rgb),
                    bold: style.bold,
                    italic: style.italic,
                    underline: style.underline != Underline::None,
                };
                column += 1;
                // Only ASCII is sure to be in the monospace font at its cell
                // width; anything else may come from a fallback font of another
                // width, so it gets a run of its own, placed at its column.
                let ascii = grapheme.is_ascii() && line.text[line.text.len() - line.runs.last().map_or(0, |last| last.len)..].is_ascii();
                line.text.push_str(&grapheme);
                match line.runs.last_mut() {
                    Some(last) if ascii && Run { column: last.column, len: last.len, ..run.clone() } == *last => {
                        last.len += run.len
                    }
                    _ => line.runs.push(run),
                }
            }
            grid.lines.push(line);
        }
        self.grid = grid;
    }
}

fn key_mods(modifiers: &Modifiers) -> key::Mods {
    let mut mods = key::Mods::empty();
    mods.set(key::Mods::SHIFT, modifiers.shift);
    mods.set(key::Mods::CTRL, modifiers.control);
    mods.set(key::Mods::ALT, modifiers.alt);
    mods.set(key::Mods::SUPER, modifiers.platform);
    mods
}

fn rgb(color: Rgb) -> u32 {
    u32::from(color.r) << 16 | u32::from(color.g) << 8 | u32::from(color.b)
}

/// Opens the session and feeds its output into the terminal until it ends,
/// and returns how it did.
async fn read(
    name: String,
    size: (u16, u16),
    commands: mpsc::Receiver<Command>,
    this: WeakEntity<Shell>,
    cx: &mut AsyncApp,
) -> End {
    let session = Session::Pty {
        argv: Vec::new(),
        cols: size.0,
        rows: size.1,
        terminal: BTreeMap::from([
            ("TERM".to_owned(), "xterm-256color".to_owned()),
            ("COLORTERM".to_owned(), "truecolor".to_owned()),
            // Programs like Claude Code enable Shift+Enter in terminals they know.
            ("TERM_PROGRAM".to_owned(), "ghostty".to_owned()),
        ]),
    };
    let opened = cx
        .background_spawn(async move {
            let target = Target::resolve(&name)?;
            node::request(&target, &Request::Open { session })
        })
        .await;
    let control = match opened {
        Ok(control) => control,
        Err(error) => return End::Failed(error),
    };
    let mut writer = control.writer;
    thread::spawn(move || {
        for command in commands {
            let frame = match command {
                Command::Input(bytes) => Frame::new(0, kind::DATA, bytes),
                Command::Resize(cols, rows) => {
                    Frame::new(0, kind::RESIZE, [cols.to_le_bytes(), rows.to_le_bytes()].concat())
                }
            };
            if protocol::write_frame(&mut writer, &frame).is_err() {
                return;
            }
        }
    });
    let mut reader: BufReader<node::Wire> = control.reader;
    loop {
        let (frame, returned) = cx
            .background_spawn(async move {
                let frame = protocol::read_frame(&mut reader);
                (frame, reader)
            })
            .await;
        reader = returned;
        let frame = match frame {
            Ok(Some(frame)) => frame,
            Ok(None) => return End::Failed("The connection to the machine closed.".to_owned()),
            Err(error) => return End::Failed(format!("The connection to the machine failed: {error}")),
        };
        match frame.kind {
            kind::DATA | kind::STDERR => {
                let shown = this.update(cx, |this, cx| this.output(&frame.payload, cx));
                if shown.is_err() {
                    return End::Failed("The terminal closed.".to_owned());
                }
            }
            kind::EXIT => return End::Exited,
            // Text copied in the guest goes to the host clipboard of interactive sessions.
            kind::CLIPBOARD_TEXT => clipboard::copy_text(&frame.payload),
            other => return End::Failed(format!("unexpected frame kind {other} from the machine")),
        }
    }
}

/// The key gpui names `name`, and the character it types without modifiers.
fn key_code(name: &str) -> Option<(Key, char)> {
    Some(match name {
        "enter" => (Key::Enter, '\0'),
        "tab" => (Key::Tab, '\0'),
        "backspace" => (Key::Backspace, '\0'),
        "escape" => (Key::Escape, '\0'),
        "space" => (Key::Space, ' '),
        "delete" => (Key::Delete, '\0'),
        "insert" => (Key::Insert, '\0'),
        "home" => (Key::Home, '\0'),
        "end" => (Key::End, '\0'),
        "pageup" => (Key::PageUp, '\0'),
        "pagedown" => (Key::PageDown, '\0'),
        "up" => (Key::ArrowUp, '\0'),
        "down" => (Key::ArrowDown, '\0'),
        "left" => (Key::ArrowLeft, '\0'),
        "right" => (Key::ArrowRight, '\0'),
        "f1" => (Key::F1, '\0'),
        "f2" => (Key::F2, '\0'),
        "f3" => (Key::F3, '\0'),
        "f4" => (Key::F4, '\0'),
        "f5" => (Key::F5, '\0'),
        "f6" => (Key::F6, '\0'),
        "f7" => (Key::F7, '\0'),
        "f8" => (Key::F8, '\0'),
        "f9" => (Key::F9, '\0'),
        "f10" => (Key::F10, '\0'),
        "f11" => (Key::F11, '\0'),
        "f12" => (Key::F12, '\0'),
        _ => {
            let mut chars = name.chars();
            let (Some(character), None) = (chars.next(), chars.next()) else {
                return None;
            };
            let code = match character.to_ascii_lowercase() {
                letter @ 'a'..='z' => Key::try_from(Key::A as i32 + (letter as i32 - 'a' as i32)).ok()?,
                digit @ '0'..='9' => Key::try_from(Key::Digit0 as i32 + (digit as i32 - '0' as i32)).ok()?,
                '`' => Key::Backquote,
                '\\' => Key::Backslash,
                '[' => Key::BracketLeft,
                ']' => Key::BracketRight,
                ',' => Key::Comma,
                '=' => Key::Equal,
                '-' => Key::Minus,
                '.' => Key::Period,
                '\'' => Key::Quote,
                ';' => Key::Semicolon,
                '/' => Key::Slash,
                _ => Key::Unidentified,
            };
            (code, character.to_ascii_lowercase())
        }
    })
}

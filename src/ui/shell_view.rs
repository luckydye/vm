//! The pane of a [`Shell`]: its rows painted on a cell grid in the monospace
//! font, with keys, pastes, scrolling and mouse selection going to the shell.

use super::{shell::Shell, theme::*};
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Entity, ExternalPaths, FocusHandle, Font, FontStyle, FontWeight,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollDelta, ScrollEvent,
    SharedString, TextAlign, TextRun, UnderlineStyle, Window, canvas, div, fill, font, point, prelude::*, px, rgb,
    size,
};
use gpui_component::ActiveTheme;
use libghostty_vt::mouse;
use std::{cell::Cell, rc::Rc};

const FONT_SIZE: Pixels = px(13.);
/// Room between the pane's edge and the grid.
const PADDING: Pixels = px(12.);

/// The size of one cell in the monospace font.
fn cell_size(font: &Font, window: &Window) -> (Pixels, Pixels) {
    let text_system = window.text_system();
    let font_id = text_system.resolve_font(font);
    let width = text_system
        .advance(font_id, FONT_SIZE, 'm')
        .expect("the monospace font has an m")
        .width;
    // The glyphs' own height, not extra leading: block and box characters
    // fill it edge to edge, so rows must meet without gaps. Font metrics give
    // the descent below the baseline as a negative number.
    let descent = text_system.descent(font_id, FONT_SIZE);
    assert!(descent <= px(0.), "the font's descent is not below the baseline");
    let height = (text_system.ascent(font_id, FONT_SIZE) - descent).ceil();
    (width, height)
}

pub fn render(
    shell: Entity<Shell>,
    focus: &FocusHandle,
    palette: Palette,
    on_drop: impl Fn(&ExternalPaths, &mut Window, &mut App) + 'static,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let state = shell.read(cx);
    let family = cx.theme().mono_font_family.clone();
    let mono = font(family);
    let (cell_width, line_height) = cell_size(&mono, window);
    let background = state.grid.background;
    let ended = state.ended.clone();
    let (keys, scrolls, resizes) = (shell.clone(), shell.clone(), shell.clone());
    let (presses, drags, releases) = (shell.clone(), shell.clone(), shell.clone());
    let grid = state.grid.clone();
    let cell = (f32::from(cell_width), f32::from(line_height));
    // Where the grid starts, set when it is laid out, for mapping the mouse to cells.
    let origin = Rc::new(Cell::new(Point::default()));
    let (laid_out, pressed, dragged, released) = (origin.clone(), origin.clone(), origin.clone(), origin.clone());
    let on_grid = move |origin: &Rc<Cell<Point<Pixels>>>, position: Point<Pixels>| {
        let offset = position - origin.get();
        (f32::from(offset.x), f32::from(offset.y))
    };
    div()
        .id("shell")
        .key_context("Pane")
        .track_focus(focus)
        .size_full()
        .bg(rgb(background))
        .overflow_hidden()
        .relative()
        .cursor(gpui::CursorStyle::IBeam)
        .on_drop::<ExternalPaths>(on_drop)
        .on_key_down(move |event: &KeyDownEvent, _, cx| {
            let keystroke = &event.keystroke;
            let modifiers = &keystroke.modifiers;
            // Cmd+C and Cmd+V on macOS, Ctrl+Shift+C and Ctrl+Shift+V elsewhere, as terminals do.
            let shortcut = if cfg!(target_os = "macos") {
                modifiers.platform
            } else {
                modifiers.control && modifiers.shift
            };
            if shortcut && keystroke.key == "c" {
                if let Some(text) = keys.read(cx).selected_text() {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                }
            } else if shortcut && keystroke.key == "v" {
                if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
                    keys.update(cx, |shell, cx| shell.paste(&text, cx));
                }
            } else if cfg!(target_os = "macos") && modifiers.platform {
                // Cmd shortcuts stay with the window.
                return;
            } else {
                keys.update(cx, |shell, cx| shell.key_down(keystroke, cx));
            }
            cx.stop_propagation();
        })
        .on_scroll({
            let wheeled = origin.clone();
            move |event: &ScrollEvent, _, cx| {
                let lines = match event.delta {
                    ScrollDelta::Lines(lines) => lines.y,
                    ScrollDelta::Pixels(pixels) => pixels.y / line_height,
                };
                let lines = -lines.round() as isize;
                if lines == 0 {
                    return;
                }
                // Programs tracking the mouse get the wheel as presses of buttons four and five.
                let position = on_grid(&wheeled, event.position);
                let button = if lines < 0 { mouse::Button::Four } else { mouse::Button::Five };
                scrolls.update(cx, |shell, cx| {
                    let reported = (0..lines.unsigned_abs()).all(|_| {
                        shell.mouse(mouse::Action::Press, Some(button), false, position, cell, &event.modifiers)
                    });
                    if !reported {
                        shell.scroll(lines, cx);
                    }
                });
            }
        })
        .on_any_mouse_down({
            let focus = focus.clone();
            move |event: &MouseDownEvent, window, cx| {
                focus.focus(window, cx);
                let position = on_grid(&pressed, event.position);
                presses.update(cx, |shell, cx| {
                    let button = mouse_button(event.button);
                    let reported = button.is_some()
                        && shell.mouse(mouse::Action::Press, button, true, position, cell, &event.modifiers);
                    if !reported && event.button == MouseButton::Left {
                        shell.select_press(position, cell, cx);
                    }
                });
            }
        })
        .on_mouse_move(move |event: &MouseMoveEvent, _, cx| {
            let position = on_grid(&dragged, event.position);
            let button = event.pressed_button.and_then(mouse_button);
            let pressed = event.pressed_button.is_some();
            drags.update(cx, |shell, cx| {
                let reported = shell.mouse(mouse::Action::Motion, button, pressed, position, cell, &event.modifiers);
                if !reported && event.pressed_button == Some(MouseButton::Left) {
                    // Alt selects a rectangle, as in Ghostty.
                    shell.select_drag(position, cell, event.modifiers.alt, cx);
                }
            });
        })
        .capture_any_mouse_up(move |event: &MouseUpEvent, _, cx| {
            let position = on_grid(&released, event.position);
            releases.update(cx, |shell, _| {
                let button = mouse_button(event.button);
                let reported = button.is_some()
                    && shell.mouse(mouse::Action::Release, button, false, position, cell, &event.modifiers);
                if !reported && event.button == MouseButton::Left {
                    shell.select_release(position, cell);
                }
            });
        })
        .child(
            canvas(
                move |bounds, _, cx| {
                    let columns = ((bounds.size.width - PADDING * 2.) / cell_width).floor().max(1.) as u16;
                    let rows = ((bounds.size.height - PADDING * 2.) / line_height).floor().max(1.) as u16;
                    let cell = (f32::from(cell_width).ceil() as u32, f32::from(line_height) as u32);
                    resizes.update(cx, |shell, cx| shell.resize((columns, rows), cell, cx));
                },
                move |bounds, _, window, cx| {
                    // On whole device pixels, so rows' backgrounds meet without seams.
                    let scale = window.scale_factor();
                    let origin = (bounds.origin + point(PADDING, PADDING))
                        .map(|value| px((f32::from(value) * scale).round() / scale));
                    laid_out.set(origin);
                    paint(&grid, origin, &mono, (cell_width, line_height), window, cx);
                },
            )
            .size_full(),
        )
        .children(ended.map(|ended| {
            div()
                .absolute()
                .bottom(px(16.))
                .left(px(16.))
                .px_3()
                .py_2()
                .rounded(ROW_RADIUS)
                .bg(rgb(palette.card))
                .border_1()
                .border_color(rgb(palette.border))
                .text_color(rgb(palette.muted))
                .child(SharedString::from(ended))
        }))
        .into_any_element()
}

/// Paints the rows of `grid` from `origin`, one cell of `cell` per character.
fn paint(
    grid: &super::shell::Grid,
    origin: gpui::Point<Pixels>,
    mono: &Font,
    (cell_width, line_height): (Pixels, Pixels),
    window: &mut Window,
    cx: &mut App,
) {
    let top = |index: usize| point(origin.x, origin.y + line_height * index as f32);
    // Edges snap to device pixels so neighbouring cells meet without seams.
    let scale = window.scale_factor();
    let snap = |value: Pixels| px((f32::from(value) * scale).round() / scale);
    // Each run is shaped on its own and placed at its column, so a glyph
    // from a fallback font of another width can't shift the rest of the row.
    let mut shaped = Vec::new();
    for (row, line) in grid.lines.iter().enumerate() {
        let mut start = 0;
        for run in &line.runs {
            let text = &line.text[start..start + run.len];
            start += run.len;
            let at = top(row) + point(cell_width * f32::from(run.column), px(0.));
            // Backgrounds, such as the selection's, all go below the glyphs, which may reach into the next row.
            if let Some(background) = run.background {
                // ASCII runs hold a cell per byte, the others a single cell.
                let cells = if text.is_ascii() { run.len } else { 1 };
                let end = at + point(cell_width * cells as f32, line_height);
                let corners = Bounds::from_corners(at.map(snap), end.map(snap));
                window.paint_quad(fill(corners, rgb(background)));
            }
            if text.trim().is_empty() && !run.underline {
                continue;
            }
            let font = Font {
                weight: if run.bold { FontWeight::BOLD } else { FontWeight::NORMAL },
                style: if run.italic { FontStyle::Italic } else { FontStyle::Normal },
                ..mono.clone()
            };
            let style = TextRun {
                len: run.len,
                font,
                color: rgb(run.foreground).into(),
                background_color: None,
                underline: run.underline.then(|| UnderlineStyle {
                    thickness: px(1.),
                    color: Some(rgb(run.foreground).into()),
                    wavy: false,
                }),
                strikethrough: None,
            };
            let line = window
                .text_system()
                .shape_line(SharedString::from(text.to_owned()), FONT_SIZE, &[style], Some(cell_width));
            shaped.push((at, line));
        }
    }
    for (row, line) in grid.lines.iter().enumerate() {
        for block in &line.blocks {
            let (rectangles, alpha) = block_shape(block.character);
            let mut color: gpui::Hsla = rgb(block.color).into();
            color.a = alpha;
            let cell = top(row) + point(cell_width * f32::from(block.column), px(0.));
            for [left, top, right, bottom] in rectangles {
                let corner = |x: f32, y: f32| point(snap(cell.x + cell_width * x), snap(cell.y + line_height * y));
                window.paint_quad(fill(Bounds::from_corners(corner(left, top), corner(right, bottom)), color));
            }
        }
    }
    for (at, line) in &shaped {
        line.paint(*at, line_height, TextAlign::Left, None, window, cx)
            .expect("painting a terminal run");
    }
    if let Some((column, row)) = grid.cursor {
        let at = point(
            origin.x + cell_width * f32::from(column),
            origin.y + line_height * f32::from(row),
        );
        let mut cursor: gpui::Hsla = rgb(grid.foreground).into();
        cursor.a = 0.6;
        window.paint_quad(fill(Bounds::new(at, size(cell_width, line_height)), cursor));
    }
}

/// The button a program tracking the mouse knows `button` as, if any.
fn mouse_button(button: MouseButton) -> Option<mouse::Button> {
    match button {
        MouseButton::Left => Some(mouse::Button::Left),
        MouseButton::Right => Some(mouse::Button::Right),
        MouseButton::Middle => Some(mouse::Button::Middle),
        MouseButton::Navigate(_) => None,
    }
}

/// The filled parts of a block element as `[left, top, right, bottom]`
/// fractions of its cell, and the opacity they are filled with.
fn block_shape(character: char) -> (Vec<[f32; 4]>, f32) {
    const UPPER_LEFT: [f32; 4] = [0., 0., 0.5, 0.5];
    const UPPER_RIGHT: [f32; 4] = [0.5, 0., 1., 0.5];
    const LOWER_LEFT: [f32; 4] = [0., 0.5, 0.5, 1.];
    const LOWER_RIGHT: [f32; 4] = [0.5, 0.5, 1., 1.];
    let full = [0., 0., 1., 1.];
    match character {
        '▀' => (vec![[0., 0., 1., 0.5]], 1.),
        // Lower one eighth up to the full block.
        '▁'..='█' => {
            let eighths = (character as u32 - 0x2580) as f32;
            (vec![[0., 1. - eighths / 8., 1., 1.]], 1.)
        }
        // Left seven eighths down to one eighth.
        '▉'..='▏' => {
            let eighths = (0x2590 - character as u32) as f32;
            (vec![[0., 0., eighths / 8., 1.]], 1.)
        }
        '▐' => (vec![[0.5, 0., 1., 1.]], 1.),
        '░' => (vec![full], 0.25),
        '▒' => (vec![full], 0.5),
        '▓' => (vec![full], 0.75),
        '▔' => (vec![[0., 0., 1., 0.125]], 1.),
        '▕' => (vec![[0.875, 0., 1., 1.]], 1.),
        '▖' => (vec![LOWER_LEFT], 1.),
        '▗' => (vec![LOWER_RIGHT], 1.),
        '▘' => (vec![UPPER_LEFT], 1.),
        '▙' => (vec![UPPER_LEFT, LOWER_LEFT, LOWER_RIGHT], 1.),
        '▚' => (vec![UPPER_LEFT, LOWER_RIGHT], 1.),
        '▛' => (vec![UPPER_LEFT, UPPER_RIGHT, LOWER_LEFT], 1.),
        '▜' => (vec![UPPER_LEFT, UPPER_RIGHT, LOWER_RIGHT], 1.),
        '▝' => (vec![UPPER_RIGHT], 1.),
        '▞' => (vec![UPPER_RIGHT, LOWER_LEFT], 1.),
        '▟' => (vec![UPPER_RIGHT, LOWER_LEFT, LOWER_RIGHT], 1.),
        _ => unreachable!("{character:?} is not a block element"),
    }
}

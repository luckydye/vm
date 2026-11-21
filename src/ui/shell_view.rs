//! The pane of a [`Shell`]: its rows painted on a cell grid in the monospace
//! font, with keys, pastes, scrolling and mouse selection going to the shell.

use super::{shell::Shell, theme::*};
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Entity, FocusHandle, Font, FontStyle, FontWeight, KeyDownEvent,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollDelta, ScrollEvent, SharedString,
    TextAlign, TextRun, UnderlineStyle, Window, canvas, div, fill, font, point, prelude::*, px, rgb, size,
};
use gpui_component::ActiveTheme;
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

pub fn render(shell: Entity<Shell>, focus: &FocusHandle, window: &mut Window, cx: &mut App) -> AnyElement {
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
        .on_scroll(move |event: &ScrollEvent, _, cx| {
            let lines = match event.delta {
                ScrollDelta::Lines(lines) => lines.y,
                ScrollDelta::Pixels(pixels) => pixels.y / line_height,
            };
            let lines = -lines.round() as isize;
            if lines != 0 {
                scrolls.update(cx, |shell, cx| shell.scroll(lines, cx));
            }
        })
        .on_mouse_down(MouseButton::Left, {
            let focus = focus.clone();
            move |event: &MouseDownEvent, window, cx| {
                focus.focus(window, cx);
                let position = on_grid(&pressed, event.position);
                presses.update(cx, |shell, cx| shell.select_press(position, cell, cx));
            }
        })
        .on_mouse_move(move |event: &MouseMoveEvent, _, cx| {
            if event.pressed_button == Some(MouseButton::Left) {
                let position = on_grid(&dragged, event.position);
                // Alt selects a rectangle, as in Ghostty.
                let rectangle = event.modifiers.alt;
                drags.update(cx, |shell, cx| shell.select_drag(position, cell, rectangle, cx));
            }
        })
        .on_mouse_up(MouseButton::Left, move |event: &MouseUpEvent, _, cx| {
            let position = on_grid(&released, event.position);
            releases.update(cx, |shell, _| shell.select_release(position, cell));
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
                    let origin = bounds.origin + point(PADDING, PADDING);
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
                .bg(rgb(CARD))
                .border_1()
                .border_color(rgb(BORDER))
                .text_color(rgb(MUTED))
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
    let shaped: Vec<_> = grid
        .lines
        .iter()
        .map(|line| {
            let runs: Vec<TextRun> = line
                .runs
                .iter()
                .map(|run| TextRun {
                    len: run.len,
                    font: Font {
                        weight: if run.bold { FontWeight::BOLD } else { FontWeight::NORMAL },
                        style: if run.italic {
                            FontStyle::Italic
                        } else {
                            FontStyle::Normal
                        },
                        ..mono.clone()
                    },
                    color: rgb(run.foreground).into(),
                    background_color: run.background.map(|background| rgb(background).into()),
                    underline: run.underline.then(|| UnderlineStyle {
                        thickness: px(1.),
                        color: Some(rgb(run.foreground).into()),
                        wavy: false,
                    }),
                    strikethrough: None,
                })
                .collect();
            window
                .text_system()
                .shape_line(line.text.clone().into(), FONT_SIZE, &runs, Some(cell_width))
        })
        .collect();
    // Backgrounds, such as the selection's, all go below the glyphs, which may reach into the next row.
    let top = |index: usize| point(origin.x, origin.y + line_height * index as f32);
    for (index, line) in shaped.iter().enumerate() {
        line.paint_background(top(index), line_height, TextAlign::Left, None, window, cx)
            .expect("painting a terminal row's background");
    }
    for (index, line) in shaped.iter().enumerate() {
        line.paint(top(index), line_height, TextAlign::Left, None, window, cx)
            .expect("painting a terminal row");
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

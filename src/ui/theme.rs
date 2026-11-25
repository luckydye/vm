//! The look of `vm ui`, from the tokens of Shade's gpui theme: layered
//! surfaces separated by 1px borders instead of shadows, 13px text. Views take
//! every color, radius and spacing from here, in the light or dark palette of
//! the host's appearance.

use gpui::{App, Pixels, WindowAppearance, px, rgb};
use gpui_component::{Theme, ThemeMode};

/// The colors of one appearance.
#[derive(Clone, Copy)]
pub struct Palette {
    /// Behind everything: the main pane and the gaps around the sidebar.
    pub window: u32,
    /// The sidebar panel floating above the window.
    pub panel: u32,
    /// Cards in the main pane.
    pub card: u32,
    /// A hovered row.
    pub hover: u32,
    /// The selected row.
    pub selected: u32,
    pub border: u32,
    /// The faint line between rows of a card.
    pub divider: u32,
    pub text: u32,
    pub muted: u32,
    pub faint: u32,
    pub running: u32,
    pub suspended: u32,
    pub danger: u32,
    /// The terminal's text and background.
    pub terminal_text: u32,
    pub terminal: u32,
    /// The terminal's 16 base ANSI colors, where Ghostty's built-in ones don't
    /// suit the appearance; `None` keeps Ghostty's.
    pub ansi: Option<[u32; 16]>,
}

pub const DARK: Palette = Palette {
    window: 0x111111,
    panel: 0x1b1b1b,
    card: 0x1e1e21,
    hover: 0x262626,
    selected: 0x2f2f32,
    border: 0x29292b,
    divider: 0x252527,
    text: 0xf5f5f4,
    muted: 0xa3a3a3,
    faint: 0x6b6b6b,
    running: 0x34d399,
    suspended: 0xfbbf24,
    danger: 0xf87171,
    terminal_text: 0xffffff,
    terminal: 0x0b0b0b,
    ansi: None,
};

pub const LIGHT: Palette = Palette {
    window: 0xf2f2f3,
    panel: 0xfbfbfb,
    card: 0xffffff,
    hover: 0xe9e9ea,
    selected: 0xe2e2e5,
    border: 0xdedee1,
    divider: 0xe8e8ea,
    text: 0x1b1b1e,
    muted: 0x6b6b70,
    faint: 0x9a9aa0,
    running: 0x16a34a,
    suspended: 0xb45309,
    danger: 0xdc2626,
    terminal_text: 0x24292f,
    terminal: 0xffffff,
    ansi: Some(LIGHT_ANSI),
};

/// GitHub Light's ANSI colors, whose dark text and saturated accents stay
/// readable on the light terminal.
const LIGHT_ANSI: [u32; 16] = [
    0x24292f, 0xcf222e, 0x116329, 0x4d2d00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781, 0x57606a, 0xa40e26,
    0x1a7f37, 0x633c01, 0x218bff, 0xa475f9, 0x3192aa, 0x8c959f,
];

impl Palette {
    /// The palette of a window appearance.
    pub fn of(appearance: WindowAppearance) -> Self {
        match ThemeMode::from(appearance) {
            ThemeMode::Dark => DARK,
            ThemeMode::Light => LIGHT,
        }
    }
}

pub const PANEL_RADIUS: Pixels = px(12.);
pub const CARD_RADIUS: Pixels = px(8.);
pub const ROW_RADIUS: Pixels = px(6.);

pub const SIDEBAR_WIDTH: Pixels = px(232.);
/// Space between the window edge, the sidebar and the main pane.
pub const GAP: Pixels = px(8.);
/// Room for the traffic lights above the sidebar's content.
pub const TITLEBAR_HEIGHT: Pixels = px(30.);

pub const TEXT_SIZE: Pixels = px(13.);
/// Secondary text: hardware, states, hints, pills.
pub const SMALL_TEXT_SIZE: Pixels = px(12.);
/// Section labels.
pub const LABEL_SIZE: Pixels = px(11.);
pub const HEADING_SIZE: Pixels = px(13.);
pub const ICON_SIZE: Pixels = px(14.);
/// A row of the sidebar.
pub const ROW_HEIGHT: Pixels = px(28.);
/// Inputs, pills and buttons.
pub const CONTROL_HEIGHT: Pixels = px(26.);

/// Sets the light or dark theme of the host, and gives gpui-component's
/// controls, such as text inputs, the same surfaces.
pub fn apply(appearance: WindowAppearance, cx: &mut App) {
    let palette = Palette::of(appearance);
    Theme::change(ThemeMode::from(appearance), None, cx);
    Theme::update(cx, |theme| {
        theme.font_size = TEXT_SIZE;
        theme.radius = ROW_RADIUS;
        theme.radius_lg = CARD_RADIUS;
        theme.shadow = false;
        theme.focus_ring = false;
        theme.background = rgb(palette.window).into();
        theme.foreground = rgb(palette.text).into();
        theme.muted_foreground = rgb(palette.muted).into();
        theme.border = rgb(palette.border).into();
        theme.input = rgb(palette.border).into();
        theme.ring = rgb(palette.faint).into();
        theme.caret = rgb(palette.text).into();
        theme.popover = rgb(palette.panel).into();
    });
}

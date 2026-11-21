//! The look of `vm ui`: layered dark surfaces, separated by 1px borders
//! instead of shadows. Views take every color, radius and spacing from here.

use gpui::{App, Pixels, px, rgb};
use gpui_component::{Theme, ThemeMode};

/// Behind everything: the main pane and the gaps around the sidebar.
pub const WINDOW: u32 = 0x101010;
/// The sidebar panel floating above the window.
pub const PANEL: u32 = 0x1a1a1a;
/// Cards in the main pane.
pub const CARD: u32 = 0x202020;
/// A hovered row.
pub const HOVER: u32 = 0x262626;
/// The selected row.
pub const SELECTED: u32 = 0x303030;
pub const BORDER: u32 = 0x2a2a2a;
/// The faint line between rows of a card.
pub const DIVIDER: u32 = 0x262626;

pub const TEXT: u32 = 0xededed;
pub const MUTED: u32 = 0x8a8a8a;
pub const FAINT: u32 = 0x5c5c5c;

pub const RUNNING: u32 = 0x4ade80;
pub const SUSPENDED: u32 = 0xfbbf24;
pub const DANGER: u32 = 0xf87171;

pub const PANEL_RADIUS: Pixels = px(14.);
pub const CARD_RADIUS: Pixels = px(12.);
pub const ROW_RADIUS: Pixels = px(8.);

pub const SIDEBAR_WIDTH: Pixels = px(232.);
/// Space between the window edge, the sidebar and the main pane.
pub const GAP: Pixels = px(8.);
/// Room for the traffic lights above the sidebar's content.
pub const TITLEBAR_HEIGHT: Pixels = px(38.);

pub const TEXT_SIZE: Pixels = px(13.);
pub const SMALL_TEXT_SIZE: Pixels = px(11.);
pub const HEADING_SIZE: Pixels = px(15.);
pub const ICON_SIZE: Pixels = px(14.);
/// A row of the sidebar.
pub const ROW_HEIGHT: Pixels = px(28.);
/// Inputs, pills and buttons.
pub const CONTROL_HEIGHT: Pixels = px(26.);

/// Gives gpui-component's controls, such as text inputs, the same surfaces.
pub fn apply(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let theme = Theme::global_mut(cx);
    theme.font_size = TEXT_SIZE;
    theme.radius = ROW_RADIUS;
    theme.radius_lg = CARD_RADIUS;
    theme.shadow = false;
    theme.focus_ring = false;
    theme.background = rgb(WINDOW).into();
    theme.foreground = rgb(TEXT).into();
    theme.muted_foreground = rgb(MUTED).into();
    theme.border = rgb(BORDER).into();
    theme.input = rgb(BORDER).into();
    theme.ring = rgb(FAINT).into();
    theme.caret = rgb(TEXT).into();
    theme.popover = rgb(PANEL).into();
}

//! The look of `vm ui`, from the tokens of Shade's gpui theme: layered dark
//! surfaces separated by 1px borders instead of shadows, 13px text. Views take
//! every color, radius and spacing from here.

use gpui::{App, Pixels, px, rgb};
use gpui_component::{Theme, ThemeMode};

/// Behind everything: the main pane and the gaps around the sidebar.
pub const WINDOW: u32 = 0x111111;
/// The sidebar panel floating above the window.
pub const PANEL: u32 = 0x1b1b1b;
/// Cards in the main pane.
pub const CARD: u32 = 0x1e1e21;
/// A hovered row.
pub const HOVER: u32 = 0x262626;
/// The selected row.
pub const SELECTED: u32 = 0x2f2f32;
pub const BORDER: u32 = 0x29292b;
/// The faint line between rows of a card.
pub const DIVIDER: u32 = 0x252527;

pub const TEXT: u32 = 0xf5f5f4;
pub const MUTED: u32 = 0xa3a3a3;
pub const FAINT: u32 = 0x6b6b6b;

pub const RUNNING: u32 = 0x34d399;
pub const SUSPENDED: u32 = 0xfbbf24;
pub const DANGER: u32 = 0xf87171;

pub const PANEL_RADIUS: Pixels = px(12.);
pub const CARD_RADIUS: Pixels = px(8.);
pub const ROW_RADIUS: Pixels = px(6.);

pub const SIDEBAR_WIDTH: Pixels = px(232.);
/// Space between the window edge, the sidebar and the main pane.
pub const GAP: Pixels = px(8.);
/// Room for the traffic lights above the sidebar's content.
pub const TITLEBAR_HEIGHT: Pixels = px(38.);

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

//! Spacing, sizes and widget styles in one place. Colours come from the
//! built-in light and dark palettes, so swapping the theme restyles
//! everything.

use iced::widget::{button as btn, container, text};
use iced::{Border, Color, Theme};

pub const SPACE_XS: f32 = 4.0;
pub const SPACE_S: f32 = 8.0;
pub const SPACE_M: f32 = 12.0;
pub const SPACE_L: f32 = 20.0;
pub const SPACE_XL: f32 = 28.0;

pub const TEXT_SMALL: f32 = 13.0;
pub const TEXT_BODY: f32 = 15.0;
pub const TEXT_SECTION: f32 = 18.0;
pub const TEXT_TITLE: f32 = 24.0;
pub const TEXT_HEADLINE: f32 = 32.0;

pub const RADIUS: f32 = 8.0;

pub const NAV_WIDTH: f32 = 150.0;
pub const LABEL_WIDTH: f32 = 140.0;
pub const MUTE_WIDTH: f32 = 90.0;
pub const PERCENT_WIDTH: f32 = 48.0;
pub const STATUS_ICON: f32 = 64.0;
/// Application icons in the Mixer.
pub const APP_ICON: f32 = 24.0;
pub const APP_ICON_RADIUS: f32 = 6.0;
/// The letter on an application's fallback tile.
pub const APP_ICON_LETTER: f32 = 13.0;

#[derive(Debug, Clone, Copy)]
pub enum Button {
    Primary,
    Secondary,
    Success,
    Danger,
    /// A navigation entry; `true` when selected.
    Nav(bool),
}

/// A built-in button style with rounded corners.
pub fn button(kind: Button) -> impl Fn(&Theme, btn::Status) -> btn::Style {
    move |theme, status| {
        let base = match kind {
            Button::Primary | Button::Nav(true) => btn::primary(theme, status),
            Button::Secondary => btn::secondary(theme, status),
            Button::Success => btn::success(theme, status),
            Button::Danger => btn::danger(theme, status),
            Button::Nav(false) => btn::text(theme, status),
        };
        btn::Style {
            border: Border {
                radius: RADIUS.into(),
                ..base.border
            },
            ..base
        }
    }
}

/// A rounded, bordered surface grouping related controls.
pub fn card(theme: &Theme) -> container::Style {
    let base = container::bordered_box(theme);
    container::Style {
        border: Border {
            radius: RADIUS.into(),
            ..base.border
        },
        ..base
    }
}

/// The fallback tile of an application without an icon.
pub fn icon_tile(background: Color) -> container::Style {
    container::Style {
        background: Some(background.into()),
        border: Border {
            radius: APP_ICON_RADIUS.into(),
            ..Border::default()
        },
        ..container::Style::default()
    }
}

/// Where an application icon appears once it has been looked up.
pub fn icon_placeholder(theme: &Theme) -> container::Style {
    icon_tile(theme.extended_palette().background.weak.color)
}

/// De-emphasised text such as section labels and hints.
pub fn muted_text(theme: &Theme) -> text::Style {
    text::secondary(theme)
}
